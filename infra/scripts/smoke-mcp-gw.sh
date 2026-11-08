#!/usr/bin/env bash
# 新jarvis-mcp-gwのデプロイ前スモークテスト（既存サービスとは別ポート・別設定）
# 使い方:
#   smoke-mcp-gw.sh [binary]              # web.fetch承認必須化を仮定（安全策適用時）
#   smoke-mcp-gw.sh [binary] active       # 現行allowlistのまま（web.fetchが有効化される事実の確認）
set -uo pipefail
bin="${1:-/home/koppe/ドキュメント/作業用/jarvis/jarvis/rust/target/release/jarvis-mcp-gw}"
mode="${2:-approval}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
port=18082
grpc=15052
tmp="$(mktemp -d)"
trap 'kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; rm -rf "$tmp"' EXIT
[ -x "$bin" ] || { echo "FAIL: binary not found: $bin"; exit 1; }
mkdir -p "$tmp/config"

if [ "$mode" = "approval" ]; then
  # web.fetch のみ requires_approval = true にしたコピー
  python3 - "$repo/infra/config/mcp_allowlist.toml" "$tmp/config/mcp_allowlist.toml" <<'PY'
import sys
text = open(sys.argv[1]).read()
old = '''name = "web.fetch"
description = "Fetch web page content"
allowed_principals = ["jarvis", "opencode"]
max_args_size = 5242880
timeout_ms = 30000
requires_approval = false'''
new = old.replace('requires_approval = false', 'requires_approval = true')
assert old in text, 'web.fetch entry not found'
open(sys.argv[2], 'w').write(text.replace(old, new, 1))
PY
else
  cp "$repo/infra/config/mcp_allowlist.toml" "$tmp/config/mcp_allowlist.toml"
fi

cat > "$tmp/config/mcp_gateway.toml" <<TOML
grpc_addr = "127.0.0.1:$grpc"
http_addr = "127.0.0.1:$port"
allowlist_path = "$tmp/config/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
mcp_principal = "opencode"
TOML

for p in "$port" "$grpc"; do
  if ss -lntH "( sport = :$p )" 2>/dev/null | grep -q .; then
    echo "FAIL: port $p is already in use"; exit 1
  fi
done
(cd "$tmp" && exec env RUST_LOG=error OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317 "$bin") >"$tmp/gw.log" 2>&1 &
pid=$!

ok=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS: $1 ($3)"; ok=$((ok+1)); else echo "  FAIL: $1 expected=$2 actual=$3"; fail=$((fail+1)); fi; }
for _ in $(seq 1 40); do curl -q -s -o /dev/null "http://127.0.0.1:$port/health" && break; sleep 0.3; done

echo "== mode: $mode =="
check health 200 "$(curl -q -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/health")"
check metrics 200 "$(curl -q -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/metrics")"

# web.fetch: 承認必須なら403、現行allowlistなら実取得（200 + Example Domain）
code=$(curl -q -s -o "$tmp/r1" -w '%{http_code}' -X POST "http://127.0.0.1:$port/v1/execute" \
  -H 'content-type: application/json' \
  -d '{"tool":"web.fetch","args":{"url":"https://example.com/","max_bytes":65536},"principal":"opencode"}')
if [ "$mode" = "approval" ]; then
  check web.fetch-approval 403 "$code"
  if grep -q 'error' "$tmp/r1"; then echo "  PASS: error body is JSON"; ok=$((ok+1)); else echo "  FAIL: no error body"; fail=$((fail+1)); fi
else
  check web.fetch-active 200 "$code"
  if grep -qi 'example domain' "$tmp/r1"; then echo "  PASS: fetched example.com body"; ok=$((ok+1)); else echo "  FAIL: body missing Example Domain"; fail=$((fail+1)); fi
fi

# MCPエンドポイント: initialize → tools/list
curl -q -s -D "$tmp/mcp.h" -o "$tmp/mcp.b" -X POST "http://127.0.0.1:$port/mcp" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
check mcp-initialize 200 "$(head -n1 "$tmp/mcp.h" | grep -q '200' && echo 200 || echo fail)"
sid=$(grep -i '^mcp-session-id:' "$tmp/mcp.h" | tr -d '\r' | cut -d' ' -f2)
if [ -n "$sid" ]; then echo "  PASS: mcp-session-id issued"; ok=$((ok+1)); else echo "  FAIL: no session id"; fail=$((fail+1)); fi
curl -q -s -o /dev/null -X POST "http://127.0.0.1:$port/mcp" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -H "mcp-session-id: $sid" \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
curl -q -s -o "$tmp/tools.b" -X POST "http://127.0.0.1:$port/mcp" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -H "mcp-session-id: $sid" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
python3 - "$tmp/tools.b" >"$tmp/tools.names" <<'PY'
import json, re, sys
t = open(sys.argv[1]).read()
objs = []
for m in re.finditer(r'data:\s*(\{.*?\})\s*(?:\n|$)', t, re.S):
    try: objs.append(json.loads(m.group(1)))
    except Exception: pass
if not objs:
    try: objs = [json.loads(t)]
    except Exception: pass
names = []
for o in objs:
    if isinstance(o, dict) and 'result' in o:
        names = [tool.get('name') for tool in o['result'].get('tools', [])]
print('\n'.join(names))
PY
# requires_approval=true のツールは tools/list から隠される仕様
if [ "$mode" = "approval" ]; then
  if grep -qx 'filesystem.read' "$tmp/tools.names" && ! grep -qx 'web.fetch' "$tmp/tools.names"; then
    echo "  PASS: mcp tools/list (filesystem.read表示 / web.fetchは承認待ちのため非表示)"; ok=$((ok+1))
  else
    echo "  FAIL: mcp tools/list (approval mode)"; fail=$((fail+1))
  fi
else
  if grep -qx 'filesystem.read' "$tmp/tools.names" && grep -qx 'web.fetch' "$tmp/tools.names"; then
    echo "  PASS: mcp tools/list (filesystem.read, web.fetch)"; ok=$((ok+1))
  else
    echo "  FAIL: mcp tools/list missing expected tools"; fail=$((fail+1))
  fi
fi

# 引数エラー → 400（ディレクトリ読取）
code=$(curl -q -s -o "$tmp/r2" -w '%{http_code}' -X POST "http://127.0.0.1:$port/v1/execute" \
  -H 'content-type: application/json' -d '{"tool":"filesystem.read","args":{},"principal":"opencode"}')
check filesystem.read-directory 400 "$code"

# 未知ツール → 403
code=$(curl -q -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$port/v1/execute" \
  -H 'content-type: application/json' -d '{"tool":"unknown.tool","args":{},"principal":"opencode"}')
check unknown-tool 403 "$code"

echo "== SMOKE RESULT: $([ "$fail" -eq 0 ] && echo PASS || echo FAIL) (ok=$ok fail=$fail) =="
[ "$fail" -eq 0 ]
