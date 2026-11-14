#!/usr/bin/env bash
# 新jarvis-mcp-gwのデプロイ前スモークテスト（既存サービスとは別ポート・別設定）
# 使い方:
#   smoke-mcp-gw.sh [binary]              # repo の allowlist をそのまま使う（承認必須ツールは非表示）
#   smoke-mcp-gw.sh [binary] active       # web.fetch を強制的に承認不要へ戻し、取得実装を確認
set -uo pipefail
bin="${1:-/home/koppe/ドキュメント/作業用/jarvis/jarvis/rust/target/release/jarvis-mcp-gw}"
mode="${2:-approval}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
port=18082
grpc=15052
tmp="$(mktemp -d)"
trap 'kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; rm -rf "$tmp"' EXIT
[ -x "$bin" ] || { echo "FAIL: binary not found: $bin"; exit 1; }
# 一時ディレクトリへ cd してから起動するため、相対パスを絶対パスへ解決する
bin="$(realpath "$bin")"
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
if old in text:
    new = old.replace('requires_approval = false', 'requires_approval = true')
    open(sys.argv[2], 'w').write(text.replace(old, new, 1))
elif old.replace('requires_approval = false', 'requires_approval = true') in text:
    open(sys.argv[2], 'w').write(text)  # 既に承認必須
else:
    raise SystemExit('web.fetch entry not found')
PY
else
  cp "$repo/infra/config/mcp_allowlist.toml" "$tmp/config/mcp_allowlist.toml"
  # active モード: web.fetch だけ承認不要へ戻して実取得を検証する
  python3 - "$tmp/config/mcp_allowlist.toml" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
old = '''name = "web.fetch"
description = "Fetch web page content"
allowed_principals = ["jarvis", "opencode"]
max_args_size = 5242880
timeout_ms = 30000
requires_approval = true'''
new = old.replace('requires_approval = true', 'requires_approval = false')
if old in text:
    open(path, 'w').write(text.replace(old, new, 1))
else:
    raise SystemExit('web.fetch entry not found')
PY
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
  if python3 - "$tmp/tools.names" <<'PY'
import sys
names = set(open(sys.argv[1]).read().split())
expected = {'filesystem.list', 'filesystem.read', 'git.diff', 'git.status'}
sys.exit(0 if names == expected else 1)
PY
  then
    echo "  PASS: mcp tools/list (期待4ツールのみ / 未実装・承認待ちは非表示)"; ok=$((ok+1))
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

# ネイティブ読み取り系の tools/call が実際に成功すること
tool_call_ok() { # file needle
  python3 - "$1" "$2" <<'PY'
import json, re, sys
path, needle = sys.argv[1], sys.argv[2]
text = open(path).read()
objs = []
for m in re.finditer(r'data:\s*(\{.*\})', text, re.S):
    try: objs.append(json.loads(m.group(1)))
    except Exception: pass
if not objs:
    try: objs = [json.loads(text)]
    except Exception: sys.exit(1)
for o in objs:
    r = o.get('result') or {}
    if r.get('isError') is True:
        sys.exit(1)
    for c in r.get('content') or []:
        if c.get('type') == 'text' and needle in c.get('text', ''):
            sys.exit(0)
sys.exit(1)
PY
}
curl -q -s -o "$tmp/c1" -X POST "http://127.0.0.1:$port/mcp" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -H "mcp-session-id: $sid" \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"filesystem.list","arguments":{"path":"."}}}'
if tool_call_ok "$tmp/c1" '"entries"'; then
  echo "  PASS: tools/call filesystem.list"; ok=$((ok+1))
else
  echo "  FAIL: tools/call filesystem.list"; fail=$((fail+1))
fi
curl -q -s -o "$tmp/c2" -X POST "http://127.0.0.1:$port/mcp" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -H "mcp-session-id: $sid" \
  -d "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"git.status\",\"arguments\":{\"path\":\"$repo\"}}}"
if tool_call_ok "$tmp/c2" '"stdout"'; then
  echo "  PASS: tools/call git.status"; ok=$((ok+1))
else
  echo "  FAIL: tools/call git.status"; fail=$((fail+1))
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
