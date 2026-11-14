#!/usr/bin/env bash
# 稼働中のjarvis-mcp-gwとリポジトリ成果物の乖離を検査（読み取り専用）
# バイナリ/設定の一致に加え、systemd 配下で動く実サービスに tools/call して
# 「設定は一致しているが実行時に失敗する」乖離も検出する。
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
# 5) systemd unit（配備版と repo の一致）
unit_repo="$repo/infra/systemd/jarvis-mcp-gw.service"
unit_sys="/etc/systemd/system/jarvis-mcp-gw.service"
if [ -f "$unit_sys" ] && [ -f "$unit_repo" ]; then
  if cmp -s "$unit_sys" "$unit_repo"; then say "systemd unit" "OK (same)"; else say "systemd unit" "DIFF"; drift=1; fi
else
  say "systemd unit" "MISSING"; drift=1
fi
# 6) 実行時E2E: systemd 配下の実サービスに読み取り系 tools/call を行う。
#    ProtectHome の取り違えのように「設定一致でも実行不能」な乖離を検出する。
runtime_check() {
  local tmp sid code
  tmp="$(mktemp -d)" || return 1
  curl -q -s -D "$tmp/h" -o /dev/null -m 5 -X POST http://127.0.0.1:8082/mcp \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"drift-check","version":"0"}}}' || { rm -rf "$tmp"; return 1; }
  sid=$(grep -i '^mcp-session-id:' "$tmp/h" | tr -d '\r' | cut -d' ' -f2)
  [ -n "$sid" ] || { rm -rf "$tmp"; return 1; }
  curl -q -s -o /dev/null -m 5 -X POST http://127.0.0.1:8082/mcp \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $sid" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  # filesystem.list（読み取り）と git.status（外部プロセス起動）を確認
  curl -q -s -o "$tmp/r1" -m 8 -X POST http://127.0.0.1:8082/mcp \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $sid" \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"filesystem.list","arguments":{"path":"."}}}'
  curl -q -s -o "$tmp/r2" -m 8 -X POST http://127.0.0.1:8082/mcp \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $sid" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"git.status\",\"arguments\":{\"path\":\"$repo\"}}}"
  code=0
  grep -q '"entries"' "$tmp/r1" || code=1
  grep -q 'Permission denied' "$tmp/r1" && code=1
  grep -q '"stdout"' "$tmp/r2" || code=1
  grep -q 'Permission denied' "$tmp/r2" && code=1
  rm -rf "$tmp"
  return "$code"
}
if systemctl is-active --quiet jarvis-mcp-gw 2>/dev/null; then
  if runtime_check; then say "runtime tools/call" "OK (filesystem.list/git.status)"; else say "runtime tools/call" "DANGER: systemd 配下で実行失敗"; danger=1; fi
else
  say "runtime tools/call" "SKIP (service inactive)"
fi
if [ "$danger" -eq 1 ]; then
  echo "== RESULT: DANGER（実行時機能不全または危険な設定。上記を解消してからデプロイ） =="
  exit 2
elif [ "$drift" -eq 1 ]; then
  echo "== RESULT: PENDING（デプロイ差分あり） =="
  exit 1
else
  echo "== RESULT: OK =="
fi
