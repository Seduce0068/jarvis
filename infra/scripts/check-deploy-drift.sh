#!/usr/bin/env bash
# 稼働中のjarvis-mcp-gwとリポジトリ成果物の乖離を検査（読み取り専用）
# バイナリ/設定の一致に加え、systemd 配下で動く実サービスに tools/call して
# 「設定は一致しているが実行時に失敗する」乖離も検出する。
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Overridable for regression tests; production uses the fixed paths.
deployed_allowlist="${JARVIS_DRIFT_ALLOWLIST:-/etc/jarvis/mcp_allowlist.toml}"
deployed_policy_dir="${JARVIS_DRIFT_POLICY_DIR:-/etc/jarvis/policy}"
deployed_pii="${JARVIS_DRIFT_PII:-/etc/jarvis/pii_patterns.toml}"
# I50-A: unit が EnvironmentFile=- で読む host override env。
deployed_env="${JARVIS_DRIFT_ENV:-/etc/jarvis/jarvis-mcp-gw.env}"
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
# 2) allowlist. A missing deployed allowlist is a DANGER, not a skipped check:
# otherwise `web.fetch` approval and drift would silently pass (A-H2/P0-9).
if [ ! -f "$deployed_allowlist" ]; then
  say "allowlist" "MISSING (deployed)"
  danger=1
elif [ ! -f "$repo/infra/config/mcp_allowlist.toml" ]; then
  say "allowlist" "MISSING (repo)"
  danger=1
else
  if cmp -s "$deployed_allowlist" "$repo/infra/config/mcp_allowlist.toml"; then
    say "allowlist" "OK (same)"
  else
    say "allowlist" "DIFF"; drift=1
  fi
  # 3) web.fetch の承認設定（配備版）
  if grep -A8 'name = "web.fetch"' "$deployed_allowlist" 2>/dev/null | grep -q 'requires_approval = false'; then
    say "web.fetch approval" "DANGER: false -> active on deploy"; danger=1
  else
    say "web.fetch approval" "OK (requires approval)"
  fi
fi
# 4) policy. A file present on only one side must no longer be silently
# skipped (I7-C): a missing deployed file is a DANGER (same framework as the
# allowlist check) and a missing repo source is flagged too.
if [ ! -f /etc/jarvis/policy/casbin_policy.csv ]; then
  say "policy" "MISSING (deployed)"
  danger=1
elif [ ! -f "$repo/infra/config/policy/casbin_policy.csv" ]; then
  say "policy" "MISSING (repo)"
  danger=1
else
  if cmp -s /etc/jarvis/policy/casbin_policy.csv "$repo/infra/config/policy/casbin_policy.csv"; then
    say "policy" "OK (same)"
  else
    say "policy" "DIFF"; drift=1
  fi
fi
# 4b) Cedar policy / schema / PII patterns (P2-12). Missing files are no longer
# silently skipped (I7-C): a missing deployed file is a DANGER and a missing
# repo source is flagged too, using the same framework as the allowlist check.
for pair in \
  "$deployed_policy_dir/policies.cedar|$repo/infra/config/policy/policies.cedar" \
  "$deployed_policy_dir/cedar_schema.cedarschema|$repo/infra/config/policy/cedar_schema.cedarschema" \
  "$deployed_pii|$repo/infra/config/pii_patterns.toml"; do
  deployed="${pair%%|*}"; source="${pair##*|}"
  label="$(basename "$deployed")"
  if [ ! -f "$deployed" ]; then
    say "$label" "MISSING (deployed)"
    danger=1
  elif [ ! -f "$source" ]; then
    say "$label" "MISSING (repo)"
    danger=1
  else
    if cmp -s "$deployed" "$source"; then say "$label" "OK (same)"; else say "$label" "DIFF"; drift=1; fi
  fi
done
# 5) systemd unit（配備版と repo の一致）
unit_repo="$repo/infra/systemd/jarvis-mcp-gw.service"
unit_sys="${JARVIS_DRIFT_UNIT:-/etc/systemd/system/jarvis-mcp-gw.service}"
if [ -f "$unit_sys" ] && [ -f "$unit_repo" ]; then
  if cmp -s "$unit_sys" "$unit_repo"; then say "systemd unit" "OK (same)"; else say "systemd unit" "DIFF"; drift=1; fi
else
  say "systemd unit" "MISSING"; drift=1
fi
# 5b) Fail-closed guard: the DEPLOYED unit must have no leftover "${" in any
# Environment= line. systemd.exec(5) does not expand variables in Environment=,
# so such a value is a broken literal (invalid base dir / deny-all egress).
# Emit an ERROR CODE ONLY — never echo the matched line or its value (secrets).
# A missing/unreadable unit also fails closed (cannot verify => DANGER).
if [ ! -r "$unit_sys" ]; then
  say "systemd env expansion" "DANGER: E_UNIT_UNREADABLE (code only)"
  danger=1
elif grep -Eq '^[[:space:]]*Environment=.*[$][{]' "$unit_sys"; then
  say "systemd env expansion" "DANGER: E_UNEXPANDED_ENV (code only)"
  danger=1
else
  say "systemd env expansion" "OK (no unexpanded variable in Environment=)"
fi
# 5c) I50-A: host override env の存在と必須キーを fail-closed で検証する。
# これが欠けると unit のリテラル既定値（開発用 BASE_DIR と egress=example.com）
# が実効になり、filesystem.* 全失敗＋「deny-all のつもりが example.com 許可」
# の fail-open が起きる。値は一切出力せず、エラーコードのみを出す（secret を含む
# 可能性があるため）。コメント行（先頭 #）はキー存在とみなさない。
# I52-B: BASE_DIR は filesystem.* / git.* のパスエスケープ境界であり、非空かつ
# 絶対パスでなければならない。空値・相対パスは境界が消えるためキー存在チェック
# とは別に拒否する（実在チェックは root が読めないディレクトリを誤判定しうる
# ため、ここでは先頭 / の形式までを検査する）。
# egress の空値は正当（genuine deny-all）なのでキーの有無のみを検査する。
# 値は一切出力せず、エラーコードのみを出す。
if [ ! -r "$deployed_env" ]; then
  say "mcp-gw env" "DANGER: E_ENV_UNREADABLE (code only)"
  danger=1
elif ! grep -Eq '^[[:space:]]*JARVIS_MCP_GW_BASE_DIR=' "$deployed_env"; then
  say "mcp-gw env" "DANGER: E_ENV_MISSING_BASE_DIR (code only)"
  danger=1
elif ! grep -Eq "^[[:space:]]*JARVIS_MCP_GW_BASE_DIR=[[:space:]]*[\"']?/" "$deployed_env"; then
  say "mcp-gw env" "DANGER: E_ENV_INVALID_BASE_DIR (code only; empty or relative)"
  danger=1
elif ! grep -Eq '^[[:space:]]*JARVIS_MCP_GW_EGRESS_ALLOWLIST=' "$deployed_env"; then
  say "mcp-gw env" "DANGER: E_ENV_MISSING_EGRESS_ALLOWLIST (code only)"
  danger=1
else
  say "mcp-gw env" "OK (present, required keys; BASE_DIR absolute)"
fi
# 6) 実行時E2E: systemd 配下の実サービスに読み取り系 tools/call を行う。
#    ProtectHome の取り違えのように「設定一致でも実行不能」な乖離を検出する。
runtime_check() {
  local mcp_url="${JARVIS_MCP_GW_URL:-http://127.0.0.1:8082}"
  local tmp sid code
  tmp="$(mktemp -d)" || return 1
  curl -q -s -D "$tmp/h" -o /dev/null -m 5 -X POST "${mcp_url}/mcp" \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"drift-check","version":"0"}}}' || { rm -rf "$tmp"; return 1; }
  sid=$(grep -i '^mcp-session-id:' "$tmp/h" | tr -d '\r' | cut -d' ' -f2)
  [ -n "$sid" ] || { rm -rf "$tmp"; return 1; }
  curl -q -s -o /dev/null -m 5 -X POST "${mcp_url}/mcp" \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $sid" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  # filesystem.list（読み取り）・git.status（外部プロセス起動）・tools/list を確認
  curl -q -s -o "$tmp/r1" -m 8 -X POST "${mcp_url}/mcp" \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $sid" \
    -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"filesystem.list","arguments":{"path":"."}}}'
  curl -q -s -o "$tmp/r2" -m 8 -X POST "${mcp_url}/mcp" \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $sid" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"git.status\",\"arguments\":{\"path\":\"$repo\"}}}"
  curl -q -s -o "$tmp/r3" -m 8 -X POST "${mcp_url}/mcp" \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $sid" \
    -d '{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}'
  # SSE の JSON を正しくパースして判定する（生テキストの grep は
  # エスケープされた \"entries\" に一致せず誤検知するため）。
  python3 - "$tmp/r1" "$tmp/r2" "$tmp/r3" <<'PY'
import json, re, sys

def parse(path):
    text = open(path).read()
    objs = []
    for m in re.finditer(r'data:\s*(\{.*\})', text, re.S):
        try:
            objs.append(json.loads(m.group(1)))
        except Exception:
            pass
    if not objs:
        try:
            objs = [json.loads(text)]
        except Exception:
            pass
    return objs

def text_of(path):
    objs = [o for o in parse(path) if 'result' in o]
    if not objs:
        return None
    r = objs[-1].get('result') or {}
    if r.get('isError') is True:
        return None
    parts = [c.get('text', '') for c in (r.get('content') or []) if c.get('type') == 'text']
    return '\n'.join(parts)

ok = True
t1 = text_of(sys.argv[1])
if not t1 or '"entries"' not in t1:
    print('  detail: filesystem.list の応答が不正', file=sys.stderr)
    ok = False
t2 = text_of(sys.argv[2])
if not t2 or '"stdout"' not in t2:
    print('  detail: git.status の応答が不正', file=sys.stderr)
    ok = False
names = set()
for o in parse(sys.argv[3]):
    r = o.get('result') or {}
    if 'tools' in r:
        names = {t.get('name') for t in r['tools']}
expected = {'filesystem.list', 'filesystem.read', 'git.diff', 'git.status'}
# MCP-1 承認フロー実装後は requires_approval=true のツールも tools/list に
# 表示されるため、読み取り系4ツールの部分集合で判定する。
if not expected <= names:
    print(f'  detail: tools/list が期待と不一致 actual={sorted(names)}', file=sys.stderr)
    ok = False
sys.exit(0 if ok else 1)
PY
  code=$?
  rm -rf "$tmp"
  return "$code"
}
if systemctl is-active --quiet jarvis-mcp-gw 2>/dev/null; then
  if runtime_check; then say "runtime tools/call" "OK (tools/call + tools/list)"; else say "runtime tools/call" "DANGER: systemd 配下で実行失敗"; danger=1; fi
else
  # サービス停止で実行時 E2E が不能な場合も SKIP+OK とせず DANGER（fail-closed）。
  # 以前は SKIP のまま exit 0 となり deploy-mcp-gw-safe.sh の事後確認が
  # 「実行時 E2E 未実施」を合格と誤判定していた（I25-E / Round 19 B4）。
  say "runtime tools/call" "DANGER: サービス停止（実行時 E2E 不能・SKIP ではない）"
  danger=1
fi
if [ "$danger" -eq 1 ]; then
  echo "== RESULT: DANGER（実行時機能不全・サービス停止によるE2E不能・危険な設定。SKIP は OK として扱わない。上記を解消してからデプロイ） =="
  exit 2
elif [ "$drift" -eq 1 ]; then
  echo "== RESULT: PENDING（デプロイ差分あり） =="
  exit 1
else
  echo "== RESULT: OK =="
fi
