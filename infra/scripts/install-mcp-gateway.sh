#!/usr/bin/env bash
# jarvis-mcp-gw のリリースバイナリを /opt/jarvis/bin へ配置しサービス再起動（要root）
# 既定は乾燥実行。--apply で実行。失敗時は旧バイナリへロールバック。
set -euo pipefail
apply=0; [ "${1:-}" = "--apply" ] && apply=1
if [ "$apply" -eq 1 ] && [ "$(id -u)" -ne 0 ]; then echo "rootで実行してください" >&2; exit 1; fi
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$script_dir/../.." && pwd)"
src="$repo/rust/target/release/jarvis-mcp-gw"
dst="/opt/jarvis/bin/jarvis-mcp-gw"
ts="$(date +%Y%m%d-%H%M%S)"

echo "== 1. ビルド確認 =="
if [ ! -x "$src" ]; then
  echo "  リリースバイナリがありません。次を実行:"
  echo "  (cd $repo/rust && cargo build --release -p jarvis-mcp-gw)"
  exit 1
fi
echo "  $src ($(stat -c %s "$src")B)"

echo "== 2. 配置計画 =="
echo "  install -o root -g root -m 0755 $src $dst"
echo "  backup: $dst.prev-$ts"
if [ "$apply" -eq 1 ]; then
  [ -f "$dst" ] && cp -a "$dst" "$dst.prev-$ts"
  install -o root -g root -m 0755 "$src" "$dst"
  echo "== 3. サービス再起動 =="
  health_code=""
  if systemctl restart jarvis-mcp-gw && sleep 2 && health_code="$(curl -fsS -o /dev/null -w '%{http_code}' http://127.0.0.1:8082/health || true)" && [ "$health_code" = "200" ]; then
    echo "  再起動OK: $(systemctl is-active jarvis-mcp-gw)"
  else
    echo "  再起動失敗: ロールバックします"
    [ -f "$dst.prev-$ts" ] && install -o root -g root -m 0755 "$dst.prev-$ts" "$dst"
    systemctl restart jarvis-mcp-gw || true
    exit 1
  fi
else
  echo "乾燥実行のみ。実行するには --apply を付けてください。"
fi
