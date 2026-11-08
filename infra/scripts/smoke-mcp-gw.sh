#!/usr/bin/env bash
# 新jarvis-mcp-gwのデプロイ前スモークテスト（既存サービスとは別ポート・別設定）
# - /health, /metrics, /v1/executeのエラー分類, web.fetch承認必須化の挙動を確認
set -uo pipefail
bin="${1:-/home/koppe/ドキュメント/作業用/jarvis/jarvis/rust/target/release/jarvis-mcp-gw}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
port=18082
grpc=15052
tmp="$(mktemp -d)"
trap 'kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; rm -rf "$tmp"' EXIT
[ -x "$bin" ] || { echo "FAIL: binary not found: $bin"; exit 1; }
mkdir -p "$tmp/config"
# allowlist: web.fetch を承認必須にしたコピー
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
assert old in text
open(sys.argv[2], 'w').write(text.replace(old, new, 1))
PY
cat > "$tmp/config/mcp_gateway.toml" <<TOML
grpc_addr = "127.0.0.1:$grpc"
http_addr = "127.0.0.1:$port"
allowlist_path = "$tmp/config/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
mcp_principal = "opencode"
TOML
# 事前にポートが空いていることを確認（残留プロセスで誤判定しない）
for p in "$port" "$grpc"; do
  if ss -lntH "( sport = :$p )" 2>/dev/null | grep -q .; then
    echo "FAIL: port $p is already in use"; exit 1
  fi
done
(cd "$tmp" && exec env RUST_LOG=error OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317 "$bin") >"$tmp/gw.log" 2>&1 &
pid=$!
ok=0; fail=0
check() { # name expected actual
  if [ "$2" = "$3" ]; then echo "  PASS: $1 ($3)"; ok=$((ok+1)); else echo "  FAIL: $1 expected=$2 actual=$3"; fail=$((fail+1)); fi
}
for _ in $(seq 1 30); do curl -q -s -o /dev/null "http://127.0.0.1:$port/health" && break; sleep 0.3; done
check health 200 "$(curl -q -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/health")"
check metrics 200 "$(curl -q -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/metrics")"
# web.fetch: 承認必須 → 403
code=$(curl -q -s -o "$tmp/r1" -w '%{http_code}' -X POST "http://127.0.0.1:$port/v1/execute" \
  -H 'content-type: application/json' -d '{"tool":"web.fetch","args":{"url":"https://example.com/"},"principal":"opencode"}')
check web.fetch-approval 403 "$code"
grep -q 'error' "$tmp/r1" && echo "  PASS: error body is JSON" && ok=$((ok+1)) || { echo "  FAIL: no error body"; fail=$((fail+1)); }
# 引数エラー → 400
code=$(curl -q -s -o "$tmp/r2" -w '%{http_code}' -X POST "http://127.0.0.1:$port/v1/execute" \
  -H 'content-type: application/json' -d '{"tool":"filesystem.read","args":{},"principal":"opencode"}')
check filesystem.read-missing-path 400 "$code"
# 未知ツール → 403
code=$(curl -q -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$port/v1/execute" \
  -H 'content-type: application/json' -d '{"tool":"unknown.tool","args":{},"principal":"opencode"}')
check unknown-tool 403 "$code"
echo "== SMOKE RESULT: $([ "$fail" -eq 0 ] && echo PASS || echo FAIL) (ok=$ok fail=$fail) =="
[ "$fail" -eq 0 ]
