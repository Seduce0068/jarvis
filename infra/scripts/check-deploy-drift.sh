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
# I97-B: binary も内容検査と owner/mode 検査で同じ override を使う。
deployed_binary="${JARVIS_DRIFT_BINARY:-/opt/jarvis/bin/jarvis-mcp-gw}"
drift=0; danger=0
say() { printf '%-32s %s\n' "$1" "$2"; }
# I95-C: stat-based owner/mode verification. EXPECTED_CONFIG_MODE in
# restore-local.d/configs.sh promises root:root for deployed configs, but the
# content-only drift check could not detect a post-deploy chown/chmod.
# `stat` is invoked BY NAME (never an absolute path) so isolated regression
# tests can stub it through PATH; this helper only READS metadata (no chmod).
# On mismatch -- or when stat is unavailable / the file is unreadable -- fail
# closed with a FIXED code only. Never print the path, owner name, uid/gid or
# mode value (data minimization; same rule as the env/unit checks above).
check_owner_mode() { # label path expected_mode
  local label="$1" path="$2" expected="$3" observed
  # Absent files are already flagged by the corresponding content check;
  # do not double-report them here.
  [ -e "$path" ] || return 0
  if ! command -v stat >/dev/null 2>&1; then
    say "$label" "DANGER: E_BAD_OWNER_MODE (code only)"
    danger=1
    return 0
  fi
  observed="$(stat -c '%u:%g %a' -- "$path" 2>/dev/null)" || {
    say "$label" "DANGER: E_BAD_OWNER_MODE (code only)"
    danger=1
    return 0
  }
  if [ "$observed" != "0:0 $expected" ]; then
    say "$label" "DANGER: E_BAD_OWNER_MODE (code only)"
    danger=1
  fi
}
# I54-B: --env-only は mcp-gw env の形式検査のみを行い、binary/unit/policy/
# runtime E2E 等の他検査をスキップする。build-selfhost.sh が env 配置直後に
# 即時検証するためのモード（サービス未起動でも env 自体の正当性は検証できる）。
# 既定（引数なし）の挙動は不変。
env_only=0
for arg in "$@"; do
  case "$arg" in
    --env-only) env_only=1 ;;
  esac
done

# I54-B / I56-A: EnvironmentFile は同一キー「最後の割り当てが勝ち」セマンティクス。
# any-match な grep は先頭に安全な値・末尾に危険な値を置いたすり抜けを許す。
# よって BASE_DIR と egress の両方で同一キーの重複自体を DANGER とし、最後の
# 割り当て行のみを抽出して形式検査する。BASE_DIR は絶対パス・/ 単独でない・
# .. 成分なしを必須化。egress は空値を正当（genuine deny-all）として維持し、
# 非空なら Rust と同一のホストパターン規則で検証する。値は一切出力しない
# （secret を含みうるためエラーコードのみ）。
# I97-A: Rust の parse_egress_allowlist / is_valid_host_pattern
# （rust/jarvis-mcp-gw/src/config.rs:209-244）と同じ規則:
#   - ',' で分割し各 entry を trim、空 entry は無視（空値＝deny-all）
#   - 先頭の '*.' のみワイルドカードとして許可（host は空不可・253 文字以下）
#   - 各ラベルは非空・63 文字以下・ASCII 英数字/ハイフンのみ・前後ハイフン不可
# python3 不在／解析不能も fail-closed（非 0 を返す）。値は stdout に出さない。
valid_egress_allowlist() { # value -> 0 = valid (incl. deny-all), !0 = invalid
  command -v python3 >/dev/null 2>&1 || return 2
  printf '%s' "$1" | python3 -c '
import sys

raw = sys.stdin.read()

def valid(entry):
    host = entry[2:] if entry.startswith("*.") else entry
    if not host or len(host) > 253:
        return False
    for label in host.split("."):
        if not label or len(label) > 63:
            return False
        if not all(c.isascii() and (c.isalnum() or c == "-") for c in label):
            return False
        if label.startswith("-") or label.endswith("-"):
            return False
    return True

entries = [e.strip() for e in raw.split(",")]
sys.exit(0 if all(valid(e) for e in entries if e) else 1)
'
}

check_mcp_env() {
  local base_count base_last base_val
  local egress_count egress_last egress_val
  local env_ok=1
  if [ ! -r "$deployed_env" ]; then
    say "mcp-gw env" "DANGER: E_ENV_UNREADABLE (code only)"
    danger=1
    return 0
  fi
  # --- BASE_DIR（既存ロジックを維持） ---
  base_count="$(grep -Ec '^[[:space:]]*JARVIS_MCP_GW_BASE_DIR=' "$deployed_env" 2>/dev/null || true)"
  base_last="$(grep -E '^[[:space:]]*JARVIS_MCP_GW_BASE_DIR=' "$deployed_env" 2>/dev/null | tail -n1 || true)"
  if [ "${base_count:-0}" -eq 0 ]; then
    say "mcp-gw env" "DANGER: E_ENV_MISSING_BASE_DIR (code only)"
    danger=1; env_ok=0
  elif [ "${base_count:-0}" -gt 1 ]; then
    say "mcp-gw env" "DANGER: E_ENV_DUP_BASE_DIR (code only)"
    danger=1; env_ok=0
  else
    # 最後の割り当て行のみを検査する（行全体・値は出力しない）。
    base_val="${base_last#*=}"
    base_val="${base_val#"${base_val%%[![:space:]]*}"}"
    base_val="${base_val%"${base_val##*[![:space:]]}"}"
    case "$base_val" in
      \"*\") base_val="${base_val#\"}"; base_val="${base_val%\"}" ;;
      \'*\') base_val="${base_val#\'}"; base_val="${base_val%\'}" ;;
    esac
    if [[ "$base_val" != /* ]]; then
      say "mcp-gw env" "DANGER: E_ENV_INVALID_BASE_DIR (code only; not absolute)"
      danger=1; env_ok=0
    elif [ "$base_val" = "/" ]; then
      say "mcp-gw env" "DANGER: E_ENV_INVALID_BASE_DIR (code only; root)"
      danger=1; env_ok=0
    elif [[ "/${base_val}/" == */../* ]]; then
      say "mcp-gw env" "DANGER: E_ENV_INVALID_BASE_DIR (code only; dotdot)"
      danger=1; env_ok=0
    fi
  fi
  # --- I56-A: egress にも BASE_DIR と同じ重複拒否 + 最終行のみ検査 ---
  egress_count="$(grep -Ec '^[[:space:]]*JARVIS_MCP_GW_EGRESS_ALLOWLIST=' "$deployed_env" 2>/dev/null || true)"
  egress_last="$(grep -E '^[[:space:]]*JARVIS_MCP_GW_EGRESS_ALLOWLIST=' "$deployed_env" 2>/dev/null | tail -n1 || true)"
  if [ "${egress_count:-0}" -eq 0 ]; then
    say "mcp-gw env" "DANGER: E_ENV_MISSING_EGRESS_ALLOWLIST (code only)"
    danger=1; env_ok=0
  elif [ "${egress_count:-0}" -gt 1 ]; then
    # 先頭に空値・末尾に example.com を置く細工はここで拒否する。
    say "mcp-gw env" "DANGER: E_ENV_DUP_EGRESS_ALLOWLIST (code only)"
    danger=1; env_ok=0
  else
    egress_val="${egress_last#*=}"
    egress_val="${egress_val#"${egress_val%%[![:space:]]*}"}"
    egress_val="${egress_val%"${egress_val##*[![:space:]]}"}"
    case "$egress_val" in
      \"*\") egress_val="${egress_val#\"}"; egress_val="${egress_val%\"}" ;;
      \'*\') egress_val="${egress_val#\'}"; egress_val="${egress_val%\'}" ;;
    esac
    # 空値は正当（deny-all）。非空なら Rust と同一規則で検査し、
    # 不正時は値を出さず固定コードのみを出す。
    if [ -n "$egress_val" ] && ! valid_egress_allowlist "$egress_val"; then
      say "mcp-gw env" "DANGER: E_ENV_INVALID_EGRESS_ALLOWLIST (code only; host pattern)"
      danger=1; env_ok=0
    fi
  fi
  if [ "$env_ok" -eq 1 ]; then
    say "mcp-gw env" "OK (last BASE_DIR absolute, non-root, no dotdot; egress single assignment, empty=deny-all or valid host list)"
  fi
}

# I75-A (b): strictly resolve web.fetch's requires_approval from the [[tools]]
# array with tomllib, instead of a line-distance-dependent `grep -A8`. Returns
# 0 ONLY when exactly one web.fetch entry exists, the key is present, and its
# value is boolean true. Any parse error, missing/duplicate entry, missing key,
# non-bool value, absent python3 or absent tomllib returns non-zero so the
# caller fails closed (DANGER). No values are printed (fail-closed, code only).
webfetch_approval_ok() { # allowlist_path -> 0 = approval required, !0 = DANGER
  command -v python3 >/dev/null 2>&1 || return 2
  python3 - "$1" <<'PY'
import sys

try:
    import tomllib  # stdlib since Python 3.11
except ImportError:
    sys.exit(1)  # no strict TOML parser available -> unverifiable

try:
    with open(sys.argv[1], 'rb') as fh:
        doc = tomllib.load(fh)
except Exception:
    sys.exit(1)

tools = doc.get('tools')
if not isinstance(tools, list):
    sys.exit(1)
matches = [t for t in tools if isinstance(t, dict) and t.get('name') == 'web.fetch']
# Exactly one entry, an explicit key, and a strict boolean true are required.
if len(matches) != 1 or 'requires_approval' not in matches[0]:
    sys.exit(1)
sys.exit(0 if matches[0]['requires_approval'] is True else 1)
PY
}

echo "== deploy drift =="
if [ "$env_only" -eq 1 ]; then
  check_mcp_env
  if [ "$danger" -eq 1 ]; then
    echo "== RESULT: DANGER (env-only) =="
    exit 2
  fi
  echo "== RESULT: OK (env-only) =="
  exit 0
fi
# 1) binary（I97-B: 内容検査も JARVIS_DRIFT_BINARY を適用）
if [ -x "$deployed_binary" ] && [ -x "$repo/rust/target/release/jarvis-mcp-gw" ]; then
  a=$(sha256sum "$deployed_binary" | cut -d' ' -f1)
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
  # 3) web.fetch の承認設定（配備版）。I75-A (b): grep -A8 の行距離依存を廃止し、
  # tomllib で [[tools]] の name == "web.fetch" の requires_approval を厳密取得。
  # キー欠落・false・非 bool・エントリ欠落/重複・python3/tomllib 不在はすべて DANGER。
  if webfetch_approval_ok "$deployed_allowlist"; then
    say "web.fetch approval" "OK (requires approval)"
  else
    say "web.fetch approval" "DANGER: requires_approval false/missing/invalid -> active on deploy"; danger=1
  fi
fi
# 4) policy. A file present on only one side must no longer be silently
# skipped (I7-C): a missing deployed file is a DANGER (same framework as the
# allowlist check) and a missing repo source is flagged too.
if [ ! -f "$deployed_policy_dir/casbin_policy.csv" ]; then
  say "policy" "MISSING (deployed)"
  danger=1
elif [ ! -f "$repo/infra/config/policy/casbin_policy.csv" ]; then
  say "policy" "MISSING (repo)"
  danger=1
else
  if cmp -s "$deployed_policy_dir/casbin_policy.csv" "$repo/infra/config/policy/casbin_policy.csv"; then
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
# 5d) I95-C / I97-B: owner/mode 照合。パスは上記の内容検査と同じ
# JARVIS_DRIFT_* オーバーライド（binary は JARVIS_DRIFT_BINARY、Round 71
# 契約の JARVIS_DRIFT_POLICY_DIR を含む）をそのまま使用し、ラベルは固定語彙のみ。
# unit/allowlist/policy(pii 含む)=root:root 0644、mcp-gw env=root:root 0600、
# binary=root:root 0755 を要求する。
check_owner_mode "binary"       "$deployed_binary"                            755
check_owner_mode "allowlist"    "$deployed_allowlist"                         644
check_owner_mode "policy"       "$deployed_policy_dir/casbin_policy.csv"      644
check_owner_mode "cedar_policy" "$deployed_policy_dir/policies.cedar"         644
check_owner_mode "cedar_schema" "$deployed_policy_dir/cedar_schema.cedarschema" 644
check_owner_mode "pii_patterns" "$deployed_pii"                               644
check_owner_mode "systemd unit" "$unit_sys"                                   644
check_owner_mode "mcp-gw env"   "$deployed_env"                               600
# 5c) I50-A / I52-B / I54-B: host override env の存在と必須キーを fail-closed で
# 検証する。これが欠けると unit のリテラル既定値（開発用 BASE_DIR と
# egress=example.com）が実効になり、filesystem.* 全失敗＋「deny-all のつもりが
# example.com 許可」の fail-open が起きる。I54-B の最終割り当て検査は上記
# check_mcp_env に集約し、通常経路と --env-only 経路の両方から呼ぶ。
check_mcp_env
# 6) 実行時E2E: systemd 配下の実サービスに読み取り系 tools/call を行う。
#    ProtectHome の取り違えのように「設定一致でも実行不能」な乖離を検出する。
runtime_check() {
  local mcp_url="${JARVIS_MCP_GW_URL:-http://127.0.0.1:8082}"
  local tmp sid code git_payload
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
  # I97-C: "$repo" を JSON 文字列へ直接埋め込まない。日本語や引用符を含む
  # パスで JSON が壊れ誤 DANGER になるため、json.dumps で安全に構築する。
  git_payload="$(python3 -c 'import json,sys; print(json.dumps({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"git.status","arguments":{"path":sys.argv[1]}}}))' "$repo")" || { rm -rf "$tmp"; return 1; }
  curl -q -s -o "$tmp/r2" -m 8 -X POST "${mcp_url}/mcp" \
    -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
    -H "mcp-session-id: $sid" \
    -d "$git_payload"
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
