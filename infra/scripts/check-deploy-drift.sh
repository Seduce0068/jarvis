#!/usr/bin/env bash
# 稼働中のjarvis-mcp-gwとリポジトリ成果物の乖離を検査（読み取り専用）
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
drift=0; danger=0
say() { printf '%-32s %s\n' "$1" "$2"; }
echo "== deploy drift =="
# 1) binary
if [ -x /opt/jarvis/bin/jarvis-mcp-gw ] && [ -x "$repo/rust/target/release/jarvis-mcp-gw" ]; then
  a=$(sha256sum /opt/jarvis/bin/jarvis-mcp-gw | cut -d' ' -f1)
  b=$(sha256sum "$repo/rust/target/release/jarvis-mcp-gw" | cut -d' ' -f1)
  if [ "$a" = "$b" ]; then say "binary" "OK (same)"; else say "binary" "PENDING (installed != repo)"; drift=1; fi
else
  say "binary" "MISSING"; drift=1
fi
# 2) allowlist
if [ -f /etc/jarvis/mcp_allowlist.toml ] && [ -f "$repo/infra/config/mcp_allowlist.toml" ]; then
  if cmp -s /etc/jarvis/mcp_allowlist.toml "$repo/infra/config/mcp_allowlist.toml"; then
    say "allowlist" "OK (same)"
  else
    say "allowlist" "DIFF"; drift=1
  fi
  # 3) web.fetch の承認設定（配備版）
  if grep -A8 'name = "web.fetch"' /etc/jarvis/mcp_allowlist.toml 2>/dev/null | grep -q 'requires_approval = false'; then
    say "web.fetch approval" "DANGER: false -> active on deploy"; danger=1
  else
    say "web.fetch approval" "OK (requires approval)"
  fi
fi
# 4) policy
if [ -f /etc/jarvis/policy/casbin_policy.csv ] && [ -f "$repo/infra/config/policy/casbin_policy.csv" ]; then
  if cmp -s /etc/jarvis/policy/casbin_policy.csv "$repo/infra/config/policy/casbin_policy.csv"; then
    say "policy" "OK (same)"
  else
    say "policy" "DIFF"; drift=1
  fi
fi
if [ "$danger" -eq 1 ]; then
  echo "== RESULT: DANGER（デプロイ前に /etc/jarvis/mcp_allowlist.toml の web.fetch を承認必須化） =="
  exit 2
elif [ "$drift" -eq 1 ]; then
  echo "== RESULT: PENDING（デプロイ差分あり） =="
  exit 1
else
  echo "== RESULT: OK =="
fi
