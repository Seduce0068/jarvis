#!/usr/bin/env bash
# 新jarvis-mcp-gwのデプロイ前スモークテスト（既存サービスとは別ポート・別設定）
# 使い方:
#   smoke-mcp-gw.sh [binary]              # repo の allowlist をそのまま使う（承認必須ツールも表示、呼び出し時に承認）
#   smoke-mcp-gw.sh [binary] active       # web.fetch を強制的に承認不要へ戻し、取得実装を確認
set -uo pipefail
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$script_dir/../.." && pwd)"
bin="${1:-$repo/rust/target/release/jarvis-mcp-gw}"
mode="${2:-approval}"
# Fail closed on an unknown mode. Without this guard any other value falls into
# the `else` branch below and silently skips the approval verification.
case "$mode" in
  approval|active) ;;
  *) echo "FAIL: unknown mode: $mode (expected approval|active)" >&2; exit 2 ;;
esac
port=18082
grpc=15052
tmp="$(mktemp -d)"
# Initialize before installing the trap: an early exit (missing binary,
# allowlist failure, port conflict) would otherwise hit an unbound $pid inside
# the trap under `set -u`.
pid=""
trap 'if [ -n "$pid" ]; then kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; fi; rm -rf "$tmp"' EXIT
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
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"
TOML

# b4-scripts / 2026-11-28: a missing or failing `ss` made every port look free
# and a stale process could satisfy the later health probe (fail-open).
command -v ss >/dev/null 2>&1 || { echo "FAIL: ss is required for port check" >&2; exit 2; }
for p in "$port" "$grpc"; do
  if ! port_out="$(ss -lntH "( sport = :$p )" 2>&1)"; then
    echo "FAIL: port check failed for $p" >&2; exit 2
  fi
  if printf '%s' "$port_out" | grep -q .; then
    echo "FAIL: port $p is already in use"; exit 1
  fi
done
(cd "$tmp" && exec env RUST_LOG=error OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317 JARVIS_MCP_GW_BASE_DIR="$repo" JARVIS_MCP_GW_CONFIG_PATH="$tmp/config/mcp_gateway.toml" "$bin") >"$tmp/gw.log" 2>&1 &
pid=$!

ok=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS: $1 ($3)"; ok=$((ok+1)); else echo "  FAIL: $1 expected=$2 actual=$3"; fail=$((fail+1)); fi; }
for _ in $(seq 1 40); do curl -q -s -o /dev/null "http://127.0.0.1:$port/health" && break; sleep 0.3; done

echo "== mode: $mode =="
check health 200 "$(curl -q -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/health")"
# A 200 with a degraded fixed body still means an unhealthy service (Round 95).
health_body="$(curl -q -s "http://127.0.0.1:$port/health")"
health_state="healthy"
if printf '%s' "$health_body" | grep -Eq '"status"[[:space:]]*:[[:space:]]*"degraded"'; then
  health_state="degraded"
fi
check health-body healthy "$health_state"
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
# protocolVersion はドリフト検査 infra/scripts/check-deploy-drift.sh の
# runtime_check 内 initialize と同じ値へ同期する（R4-DP3）。
# 生成コード/設定に単一の参照元が未定義のため定数で同期しているため、
# どちらかを更新するときは必ず両方を同じ値に揃えること。
curl -q -s -D "$tmp/mcp.h" -o "$tmp/mcp.b" -X POST "http://127.0.0.1:$port/mcp" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
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
python3 - "$tmp/tools.b" >"$tmp/tools.names" 2>"$tmp/tools.schema" <<'PY'
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
schemas_ok = False


def read_schema_ok(tool):
    # MCP inputSchema contract (todo.md:734-738 / mcp_server.rs native_input_schema):
    # filesystem.read の path は optional で既定は "."。required キー自体が
    # 出力されない（欠落または空配列）ことを確認する。
    # 一方 git.status / git.diff は required=["path"] を維持しているため、
    # ここでは filesystem.read に対してのみ optional を要求する。
    if tool.get('name') != 'filesystem.read':
        return False
    schema = tool.get('inputSchema') or {}
    path = (schema.get('properties') or {}).get('path')
    if not isinstance(path, dict):
        return False
    desc = path.get('description') or ''
    required = schema.get('required') or []
    return '.' in desc and 'default' in desc.lower() and not required


for o in objs:
    if isinstance(o, dict) and 'result' in o:
        tools = o['result'].get('tools', [])
        names = [tool.get('name') for tool in tools]
        schemas_ok = any(read_schema_ok(tool) for tool in tools)
print('\n'.join(names))
print('SCHEMA_OK' if schemas_ok else 'SCHEMA_MISSING', file=sys.stderr)
PY
# MCP-1 承認フロー実装後は requires_approval=true のツールも tools/list に
# 表示される（呼び出すと人間承認を待つ）。読み取り系4ツールが必ず含まれる
# ことを部分集合で検証する。
if [ "$mode" = "approval" ]; then
  if python3 - "$tmp/tools.names" <<'PY'
import sys
names = set(open(sys.argv[1]).read().split())
required = {'filesystem.list', 'filesystem.read', 'git.diff', 'git.status'}
sys.exit(0 if required <= names else 1)
PY
  then
    echo "  PASS: mcp tools/list (読み取り系4ツールを含む)"; ok=$((ok+1))
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

# MCP 契約: filesystem.read は path を optional（既定 "."）として公開する。
# required キーは削除済み（todo.md:734-738 / mcp_server.rs native_input_schema）。
# git.status / git.diff は従来どおり required=["path"] を維持しているので、
# この確認は filesystem.read の optional 契約に限定している。
if grep -q SCHEMA_OK "$tmp/tools.schema"; then
  echo "  PASS: mcp tools/list inputSchema: filesystem.read exposes optional path (defaults to .)"; ok=$((ok+1))
else
  echo "  FAIL: mcp tools/list inputSchema"; fail=$((fail+1))
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
