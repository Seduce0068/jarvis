#!/usr/bin/env bash
# jarvis-mcp-gw の安全デプロイ: ドリフト確認 → web.fetch承認必須化 → スモーク → 配置/再起動 → 事後確認
# 既定は乾燥実行。--apply で実行（要root）。--allow-web-fetch で承認必須化を自動適用。
set -uo pipefail
apply=0; allow=0
for arg in "$@"; do
  case "$arg" in
    --apply) apply=1 ;;
    --allow-web-fetch) allow=1 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
bin="$repo/rust/target/release/jarvis-mcp-gw"
allowlist="/etc/jarvis/mcp_allowlist.toml"
repo_allowlist="$repo/infra/config/mcp_allowlist.toml"
ts="$(date +%Y%m%d-%H%M%S)"
die() { echo "ERROR: $*" >&2; exit 1; }

[ -x "$bin" ] || die "リリースバイナリがありません: cargo build --release -p jarvis-mcp-gw"
if [ "$apply" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then die "rootで実行してください（sudo）"; fi

echo "== 1. ドリフト確認 =="
bash "$repo/infra/scripts/check-deploy-drift.sh" || true

if grep -A8 'name = "web.fetch"' "$allowlist" 2>/dev/null | grep -q 'requires_approval = false'; then
  if [ "$allow" -eq 1 ]; then
    echo "== 2. web.fetch承認必須化（repo + /etc） =="
    if [ "$apply" -eq 1 ]; then
      cp -a "$allowlist" "$allowlist.bak-$ts"
      cp -a "$repo_allowlist" "$repo_allowlist.bak-$ts" 2>/dev/null || true
      python3 - "$allowlist" "$repo_allowlist" <<'PY'
import sys
for path in sys.argv[1:]:
    text = open(path).read()
    old = '''name = "web.fetch"
description = "Fetch web page content"
allowed_principals = ["jarvis", "opencode"]
max_args_size = 5242880
timeout_ms = 30000
requires_approval = false'''
    new = old.replace('requires_approval = false', 'requires_approval = true')
    if old in text:
        open(path, 'w').write(text.replace(old, new, 1))
        print('  patched:', path)
PY
    else
      echo "  DRY: would set requires_approval = true in repo and /etc"
    fi
  else
    die "web.fetchが承認なしです。--allow-web-fetch を付けるか手動で承認必須化してください"
  fi
else
  echo "== 2. web.fetch承認: 既に必須（スキップ） =="
fi

echo "== 3. スモークテスト =="
bash "$repo/infra/scripts/smoke-mcp-gw.sh" "$bin" approval || die "スモークテスト失敗（デプロイ中止）"

echo "== 4. 配置・再起動 =="
if [ "$apply" -eq 1 ]; then
  [ -f /opt/jarvis/bin/jarvis-mcp-gw ] && cp -a /opt/jarvis/bin/jarvis-mcp-gw "/opt/jarvis/bin/jarvis-mcp-gw.prev-$ts"
  install -o root -g root -m 0755 "$bin" /opt/jarvis/bin/jarvis-mcp-gw
  systemctl restart jarvis-mcp-gw
  for _ in $(seq 1 20); do curl -q -s -o /dev/null http://127.0.0.1:8082/health && break; sleep 0.5; done
  systemctl is-active --quiet jarvis-mcp-gw || die "サービスが起動していません"
  echo "  deployed: $(systemctl is-active jarvis-mcp-gw)"
else
  echo "  DRY: would install $bin -> /opt/jarvis/bin/jarvis-mcp-gw and restart"
fi

echo "== 5. 事後確認 =="
if [ "$apply" -eq 1 ]; then
  bash "$repo/infra/scripts/check-deploy-drift.sh" || true
fi
echo "== DONE: $([ "$apply" -eq 1 ] && echo APPLIED || echo DRY-RUN) =="
