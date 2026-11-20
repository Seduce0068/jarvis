#!/usr/bin/env bash
# I24-E / S9: restore-local.sh の合成フィクスチャ回帰テスト（自動化可能な範囲のみ）。
#
# 目的:
#   - 実 DB / 実 Qdrant / 本番パスを一切使わずに、verify-backup.sh と
#     restore-local.sh の検証ゲート、dry-run（既定）経路、
#     --apply（隔離先 = 一時ディレクトリ）経路を通す。
#   - --apply の事後検証では、モックの呼び出し記録とバックアップセットの
#     不変性を確認し、本番リソースへ到達していないことを証明する。
#   - config allowlist 必須ゲート（Round 66）の回帰として、非 allowlist の
#     config-* を含むセットが --apply --confirm / --skip-configs の双方で
#     拒否され、DB / Qdrant / サービスが触られないことを検証する。
#   - 本番データの復元演習はユーザー承認が必要なため対象外。通常の --apply 経路
#     では --confirm を渡さない。allowlist ゲート検証のみ --apply --confirm を使う
#     （ゲートが停止処理より前に立ち、モックが全外部呼び出しを隔離するため安全）。
#   - I73-A: 状態付き systemctl モックで「active だが disabled」な backup timer
#     が停止対象に入ること／停止後 inactive を確認できなければ dispatch 前に
#     abort することを検証する（既定実行でもモックのみなので実環境に触れない）。
#   - I76-A: 未知 role / 同一 role の複数一致 / 解決先ファイル不在の set が
#     stop_services() より前の resolve_restore_input() ゲートで拒否され、
#     systemctl.log に stop が1行も出ないことを検証する（モックのみ）。
#   - I78-B: git モジュール内の resolver rc=2（absent／未解決）の警告付き
#     成功スキップを廃止し fail-closed にした回帰を、共有 resolver / log / fail
#     のスタブで pre-stop ゲート通過状態を再現して検証する（モックのみ）。
#
# 安全境界:
#   - フィクスチャは mktemp -d 配下にのみ生成し、EXIT トラップで破棄する。
#   - psql / curl / systemctl / jq / cmp は一時 bin のモックで置換する。
#   - QDRANT_URL は到達不能な 127.0.0.1:1 へ上書きする（モックが外れても
#     実 Qdrant には当たらない）。
#   - BACKUP_ROOT と TMPDIR を一時領域へ上書きし、生成物が外へ出ないようにする。
#   - /var/lib/postgresql と /etc/jarvis への読み書きは行わない。
#     configs の差分比較は cmp モックで傍受し、宛先パスは存在確認のみに留める。
#
# 終了コード契約:
#   0 : 全アサーション成功
#   1 : フィクスチャ準備失敗または 1 つ以上のアサーション失敗
#   2 : 不正なコマンドライン引数
#
# 統合（担当外・提案のみ、本ファイルでは実行しない）:
#   - ci/scripts/test-verify-backup.sh と Makefile は本タスクの担当外のため未編集。
#     親が判断するなら Makefile に test-restore-local ターゲットを追加し、
#     ci-local / safety-test から呼び出すのが自然である。
#
# 注: 現行 restore-local.sh には --dry-run オプションが無く、無指定時が dry-run
#     である。したがって本テストは「オプション無し」をもって dry-run 経路とみなす。
set -uo pipefail

usage() {
  cat <<'USAGE'
Usage: test-restore-local.sh [--dry-run | --apply] [--help]

  (既定)   合成フィクスチャで verify-backup.sh と restore-local.sh の
           dry-run（検証 + 計画、書き込みゼロ）経路をテストする。
  --dry-run 既定と同じ（明示指定用の別名）。restore-local.sh 自身には無い。
  --apply  追加で restore-local.sh --apply を一時ディレクトリに対して実行し、
           モック記録とフィクスチャ不変性で事後検証する。本番には触れない。
  --help   このヘルプを表示して終了する。
USAGE
}

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
driver="$repo/infra/scripts/restore-local.sh"
verifier="$repo/infra/scripts/verify-backup.sh"

apply=0
if [ "$#" -gt 1 ]; then
  printf 'ERROR: too many arguments\n' >&2
  usage >&2
  exit 2
fi
case "${1:-}" in
  '') ;;
  --dry-run) ;; # 既定と同じ。ドライバ側のオプションではない。
  --apply) apply=1 ;;
  -h|--help) usage; exit 0 ;;
  *) printf 'ERROR: unknown option: %s\n' "$1" >&2; exit 2 ;;
esac

for script in "$driver" "$verifier"; do
  if [ ! -f "$script" ]; then
    printf 'ERROR: required script not found: %s\n' "$script" >&2
    exit 1
  fi
done

tmp="$(mktemp -d "${TMPDIR:-/tmp}/jarvis-restore-test.XXXXXX")" || {
  printf 'ERROR: cannot create temp directory\n' >&2
  exit 1
}
trap 'rm -rf -- "$tmp"' EXIT
mkdir -p -- "$tmp/bin" "$tmp/logs" "$tmp/tmp" "$tmp/backups" || exit 1

# ---------------------------------------------------------------------------
# 外部副作用を持つコマンドのモック（すべて一時 bin に閉じ込める）
# ---------------------------------------------------------------------------
cat > "$tmp/bin/systemctl" <<'MOCK_SYSTEMCTL'
#!/usr/bin/env bash
# I73-A: 状態を持つ systemctl モック。
# - 変更系は従来どおり systemctl.log に記録する（既存アサーションを維持）。
# - 読み取り系 is-enabled / is-active は結果付きで systemctl-query.log に記録し、
#   「active だが disabled」な timer を観測可能にする。
# - 初期状態は MOCK_DISABLED_UNITS / MOCK_INACTIVE_UNITS で指定する。
#   MOCK_STOP_INEFFECTIVE_UNITS は stop が効かない（active のまま）ケース用。
# - 既定は全ユニット enabled + active（旧モックと同一の振る舞い）。
set -u
action="${1:-}"
unit="${2:-}"
state_dir="${MOCK_LOG:?}/state"
mkdir -p -- "$state_dir"

listed() { # needle list
  case " $2 " in
    *" $1 "*) return 0 ;;
    *) return 1 ;;
  esac
}

initial_flag() { # active|enabled -> 1|0
  case "$1" in
    active)
      if listed "$unit" "${MOCK_INACTIVE_UNITS:-}"; then printf '0\n'; else printf '1\n'; fi
      ;;
    enabled)
      if listed "$unit" "${MOCK_DISABLED_UNITS:-}"; then printf '0\n'; else printf '1\n'; fi
      ;;
  esac
}

flag() { # active|enabled -> 1|0
  local key="$1" file="$state_dir/${unit}.$1"
  if [ -f "$file" ]; then
    cat -- "$file"
  else
    initial_flag "$key"
  fi
}

set_flag() { # key value
  printf '%s\n' "$2" > "$state_dir/${unit}.$1"
}

case "$action" in
  is-enabled)
    v="$(flag enabled)"
    if [ "$v" = 1 ]; then result=enabled; else result=disabled; fi
    printf '%s %s -> %s\n' "$action" "$unit" "$result" >> "${MOCK_LOG:?}/systemctl-query.log"
    if [ "$v" = 1 ]; then exit 0; else exit 1; fi
    ;;
  is-active)
    v="$(flag active)"
    if [ "$v" = 1 ]; then result=active; else result=inactive; fi
    printf '%s %s -> %s\n' "$action" "$unit" "$result" >> "${MOCK_LOG:?}/systemctl-query.log"
    if [ "$v" = 1 ]; then exit 0; else exit 3; fi
    ;;
  stop)
    printf '%s %s\n' "$action" "$unit" >> "${MOCK_LOG:?}/systemctl.log"
    if ! listed "$unit" "${MOCK_STOP_INEFFECTIVE_UNITS:-}"; then
      set_flag active 0
    fi
    exit 0
    ;;
  start)
    printf '%s %s\n' "$action" "$unit" >> "${MOCK_LOG:?}/systemctl.log"
    set_flag active 1
    exit 0
    ;;
esac
# 変更系だけを記録（既存契約を維持）
printf '%s %s\n' "$action" "$unit" >> "${MOCK_LOG:?}/systemctl.log"
exit 0
MOCK_SYSTEMCTL

# I77-C: 失敗注入モードは MOCK_PSQL_MODE / MOCK_CURL_MODE / MOCK_JQ_MODE で指定する
# （既定は空 = 旧モックと同じ成功応答）。既存ケースの挙動は変えない。
cat > "$tmp/bin/psql" <<'MOCK_PSQL'
#!/usr/bin/env bash
printf 'psql %s\n' "$*" >> "${MOCK_LOG:?}/psql.log"
sql="" prev="" db=""
for arg in "$@"; do
  case "$prev" in
    -tAc) sql="$arg" ;;
    -d) db="$arg" ;;
  esac
  prev="$arg"
done
mode="${MOCK_PSQL_MODE:-ok}"
out="" rc=0
if [[ "$sql" == *"datname = 'jarvis_restore_test'"* ]]; then
  case "$mode" in
    exists_fail) rc=1 ;;
    exists_nonempty) out='1' ;;
  esac
elif [[ "$sql" == *"datname = 'jarvis_restore_backup_"* ]]; then
  # 退避DB は既定で「存在する」と答える（--confirm の後続分岐に到達させる）。
  out='1'
  case "$mode" in
    backup_verify_fail) rc=1 ;;
    backup_missing) out='' ;;
  esac
elif [ "$sql" = 'CREATE DATABASE "jarvis_restore_test"' ]; then
  [ "$mode" = create_fail ] && rc=1
elif [ "$sql" = 'DROP DATABASE "jarvis_restore_test"' ]; then
  [ "$mode" = drop_fail ] && rc=1
elif [[ "$sql" == 'CREATE DATABASE "jarvis_restore_backup_'* ]]; then
  [ "$mode" = backup_fail ] && rc=1
elif [ "$sql" = 'DROP DATABASE "jarvis"' ]; then
  [ "$mode" = prod_drop_fail ] && rc=1
elif [ "$sql" = 'CREATE DATABASE "jarvis"' ]; then
  [ "$mode" = prod_create_fail ] && rc=1
elif [[ "$sql" == *information_schema.tables* ]]; then
  # 既定は隔離/本番どちらでも 2 件を返す。モードは対象 DB だけを上書きする。
  out='2'
  case "$mode" in
    count_fail)      [ "$db" = jarvis_restore_test ] && rc=1 ;;
    count_bad)       [ "$db" = jarvis_restore_test ] && out='not-a-number' ;;
    count_zero)      [ "$db" = jarvis_restore_test ] && out='0' ;;
    prod_count_fail) [ "$db" = jarvis ] && rc=1 ;;
    prod_count_bad)  [ "$db" = jarvis ] && out='not-a-number' ;;
    prod_count_zero) [ "$db" = jarvis ] && out='0' ;;
  esac
elif [ -z "$sql" ] && [ "$db" = jarvis_restore_test ]; then
  [ "$mode" = restore_fail ] && rc=1
elif [ -z "$sql" ] && [ "$db" = jarvis ]; then
  [ "$mode" = prod_restore_fail ] && rc=1
fi
# ダンプはパイプで渡される。pipefail 下で gzip が SIGPIPE にならないよう
# 入力を最後まで読み捨てる（内容は保持も出力もしない）。
cat >/dev/null
[ -n "$out" ] && printf '%s\n' "$out"
exit "$rc"
MOCK_PSQL

cat > "$tmp/bin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
printf 'curl %s\n' "$*" >> "${MOCK_LOG:?}/curl.log"
all="$*"
want_body=1
for arg in "$@"; do
  if [ "$arg" = "/dev/null" ]; then
    want_body=0
  fi
done
# スコープ（isolated / prod）と要求種別（upload / probe / delete）を判定する。
scope='prod'
case "$all" in *jarvis_memories_restore_test*) scope='isolated' ;; esac
kind='probe'
case "$all" in
  *"/snapshots/upload"*) kind='upload' ;;
  *"-X DELETE"*)         kind='delete' ;;
esac
key="${scope}_${kind}"
mode="${MOCK_CURL_MODE:-ok}"
rc=0
code='200'
body='{"result":{"points_count":3}}'
if [ "$mode" = "${key}_non2xx" ]; then
  code='500'
elif [ "$mode" = "${key}_rc" ]; then
  rc=7
  code='000'
elif [ "$mode" = "${key}_zero" ]; then
  body='{"result":{"points_count":0}}'
elif [ "$mode" = "${key}_bad" ]; then
  body='{"result":{"points_count":"x"}}'
fi
if [ "$want_body" -eq 1 ]; then
  printf '%s\n%s\n' "$body" "$code"
else
  printf '%s\n' "$code"
fi
exit "$rc"
MOCK_CURL

cat > "$tmp/bin/jq" <<'MOCK_JQ'
#!/usr/bin/env bash
printf 'jq %s\n' "$*" >> "${MOCK_LOG:?}/jq.log"
input="$(cat)"
# 呼び出し順: 1=隔離コレクション / 2=本番コレクション（--confirm 時のみ）。
state="${MOCK_LOG:?}/jq.calls"
n=0
[ -f "$state" ] && n="$(cat "$state")"
n=$((n + 1))
printf '%s\n' "$n" > "$state"
mode="${MOCK_JQ_MODE:-ok}"
case "$mode" in
  fail)       [ "$n" -eq 1 ] && exit 4 ;;
  empty)      [ "$n" -eq 1 ] && exit 0 ;;
  zero)       if [ "$n" -eq 1 ]; then printf '0\n'; exit 0; fi ;;
  bad)        if [ "$n" -eq 1 ]; then printf 'x\n'; exit 0; fi ;;
  prod_fail)  [ "$n" -eq 2 ] && exit 4 ;;
  prod_empty) [ "$n" -eq 2 ] && exit 0 ;;
  prod_zero)  if [ "$n" -eq 2 ]; then printf '0\n'; exit 0; fi ;;
  prod_bad)   if [ "$n" -eq 2 ]; then printf 'x\n'; exit 0; fi ;;
esac
# 既定は curl モックが返したボディを反映（points_count の値のみ）。
case "$input" in
  *'"points_count":0'*)   printf '0\n' ;;
  *'"points_count":"'*)  printf 'x\n' ;;
  *)                      printf '3\n' ;;
esac
exit 0
MOCK_JQ

cat > "$tmp/bin/cmp" <<'MOCK_CMP'
#!/usr/bin/env bash
printf 'cmp %s\n' "$*" >> "${MOCK_LOG:?}/cmp.log"
# operand は一切開かない（本番 /etc/jarvis の内容を読まないため）。
exit 1
MOCK_CMP

chmod 0755 -- "$tmp/bin/systemctl" "$tmp/bin/psql" "$tmp/bin/curl" \
  "$tmp/bin/jq" "$tmp/bin/cmp" || exit 1

# ---------------------------------------------------------------------------
# フィクスチャ（実データは含めない。既存 test-verify-backup.sh と同じ形）
# ---------------------------------------------------------------------------
set_dir="$tmp/fixture"
mkdir -p -- "$set_dir" "$tmp/qdrant-src" "$tmp/git-src" || exit 1

{
  printf '%s\n' '-- PostgreSQL database dump' \
    'CREATE TABLE t (id integer);' \
    '-- PostgreSQL database dump complete'
} | gzip > "$set_dir/postgres-jarvis.sql.gz"

# Qdrant snapshot は拡張子 .bin でも中身は tar ストリーム。
printf '%s\n' 'synthetic-qdrant-snapshot' > "$tmp/qdrant-src/snapshot.json"
tar -cf "$set_dir/qdrant-snapshot.bin" -C "$tmp/qdrant-src" snapshot.json

# config-* は allowlist の 1 点のみ。秘密ファイルは一切作らない。
printf '%s\n' 'allow = ["tool.read"]' > "$set_dir/config-mcp_allowlist.toml"

# git は任意コンポーネント。追跡ファイル相当の 1 ファイルのみを含む。
printf '%s\n' 'synthetic tracked file' > "$tmp/git-src/README.md"
tar -czf "$set_dir/git-jarvis-head.tar.gz" -C "$tmp/git-src" README.md

{
  printf '# created_at=2026-11-19T00:00:00Z\n'
  for name in postgres-jarvis.sql.gz qdrant-snapshot.bin \
              config-mcp_allowlist.toml git-jarvis-head.tar.gz; do
    bytes="$(wc -c < "$set_dir/$name" | tr -d ' ')"
    sha="$(sha256sum "$set_dir/$name" | cut -d' ' -f1)"
    printf '%s %s %s\n' "$name" "$bytes" "$sha"
  done
} > "$set_dir/manifest.txt"
printf 'complete 2026-11-19T00:00:00Z\n' > "$set_dir/COMPLETE"

# ---------------------------------------------------------------------------
# テストヘルパー
# ---------------------------------------------------------------------------
ok=0
bad=0
pass() {
  printf '  PASS: %s\n' "$1"
  ok=$((ok + 1))
}
fail_case() {
  printf '  FAIL: %s\n' "$1" >&2
  bad=$((bad + 1))
}

run_driver() {
  # 前のケースで stop された状態を持ち越さないよう、呼び出しごとに初期化する。
  # I77-C: jq モックの呼び出し順カウンタもリセットする。
  rm -rf -- "$tmp/logs/state"
  rm -f -- "$tmp/logs/jq.calls"
  env \
    PATH="$tmp/bin:$PATH" \
    MOCK_LOG="$tmp/logs" \
    MOCK_DISABLED_UNITS="${MOCK_DISABLED_UNITS:-}" \
    MOCK_INACTIVE_UNITS="${MOCK_INACTIVE_UNITS:-}" \
    MOCK_STOP_INEFFECTIVE_UNITS="${MOCK_STOP_INEFFECTIVE_UNITS:-}" \
    MOCK_PSQL_MODE="${MOCK_PSQL_MODE:-}" \
    MOCK_CURL_MODE="${MOCK_CURL_MODE:-}" \
    MOCK_JQ_MODE="${MOCK_JQ_MODE:-}" \
    TMPDIR="$tmp/tmp" \
    BACKUP_ROOT="$tmp/backups" \
    VERIFY_SCRIPT="$verifier" \
    QDRANT_URL='http://127.0.0.1:1' \
    bash "$driver" "$@" </dev/null
}

check_exit() { # label expected command...
  local label="$1" expected="$2" out code
  shift 2
  out="$("$@" 2>&1)"
  code=$?
  if [ "$code" -eq "$expected" ]; then
    pass "$label (exit $code)"
  else
    fail_case "$label expected=$expected actual=$code"
    printf '%s\n' "$out" | head -8 >&2
  fi
}

has_fragment() { # text fragment
  [[ "$1" == *"$2"* ]]
}

assert_present() { # label regex file
  local label="$1" pattern="$2" file="$3"
  if [ -f "$file" ] && grep -E -- "$pattern" "$file" >/dev/null 2>&1; then
    pass "$label"
  else
    fail_case "$label (pattern not present: $pattern)"
  fi
}

assert_absent() { # label regex file
  local label="$1" pattern="$2" file="$3"
  if [ ! -f "$file" ]; then
    pass "$label (no log produced)"
  elif grep -E -- "$pattern" "$file" >/dev/null 2>&1; then
    fail_case "$label (forbidden pattern present: $pattern)"
    grep -E -- "$pattern" "$file" | head -5 >&2
  else
    pass "$label"
  fi
}

assert_no_log() { # label file
  local label="$1" file="$2"
  if [ -e "$file" ]; then
    fail_case "$label (unexpected log: $file)"
  else
    pass "$label"
  fi
}

manifest_matches() { # dir name
  local dir="$1" name="$2" actual_bytes actual_sha recorded
  actual_bytes="$(wc -c < "$dir/$name" | tr -d ' ')"
  actual_sha="$(sha256sum "$dir/$name" | cut -d' ' -f1)"
  recorded="$(awk -v n="$name" '$1 == n { print $2, $3; exit }' "$dir/manifest.txt")"
  [ "$recorded" = "$actual_bytes $actual_sha" ]
}

# I74-A: write a 7-column descriptor manifest
#   name size sha256 format encryption signature role
# Each spec is "name|format|encryption|signature|role"; size and SHA-256 come
# from the synthetic file only. Descriptor values are vocabulary probes, never
# secrets or real key material.
write_described_manifest() { # dir spec...
  local dir="$1" spec name format encryption signature role bytes sha
  shift
  printf '# created_at=2026-11-20T00:00:00Z\n'
  for spec in "$@"; do
    IFS='|' read -r name format encryption signature role <<< "$spec"
    bytes="$(wc -c < "$dir/$name" | tr -d ' ')"
    sha="$(sha256sum "$dir/$name" | cut -d' ' -f1)"
    printf '%s %s %s %s %s %s %s\n' \
      "$name" "$bytes" "$sha" "$format" "$encryption" "$signature" "$role"
  done
}

all_payloads=(
  postgres-jarvis.sql.gz
  qdrant-snapshot.bin
  config-mcp_allowlist.toml
  git-jarvis-head.tar.gz
)

printf '== I24-E: restore-local fixture regression ==\n'
printf '   workspace: %s\n' "$tmp"

# --- 0. フィクスチャ自身が verify-backup.sh を通ること ---------------------
printf '%s\n' '-- fixture / verify-backup.sh --'
check_exit "verify-backup.sh accepts good fixture" 0 bash "$verifier" "$set_dir"
for name in "${all_payloads[@]}"; do
  if manifest_matches "$set_dir" "$name"; then
    pass "manifest size/hash matches $name"
  else
    fail_case "manifest size/hash mismatch: $name"
  fi
done

# --- 1. dry-run（既定。オプション無し）------------------------------------
printf '%s\n' '-- restore-local.sh dry-run (default, zero writes) --'
dry_code=0
dry_out="$(run_driver --backup-set "$set_dir" 2>&1)" || dry_code=$?
if [ "$dry_code" -eq 0 ]; then
  pass "dry-run exit 0"
else
  fail_case "dry-run exit=$dry_code"
  printf '%s\n' "$dry_out" | head -20 >&2
fi
for fragment in 'verification OK' 'DRY RUN' 'production overwrite: disabled' \
                'qdrant.service is NOT a stop target'; do
  if has_fragment "$dry_out" "$fragment"; then
    pass "dry-run output: $fragment"
  else
    fail_case "dry-run output missing: $fragment"
  fi
done
assert_no_log "dry-run: psql never called" "$tmp/logs/psql.log"
assert_no_log "dry-run: curl never called" "$tmp/logs/curl.log"
assert_no_log "dry-run: cmp never called" "$tmp/logs/cmp.log"
assert_no_log "dry-run: no service stop/start" "$tmp/logs/systemctl.log"
# I73-A: dry-run は timer の状態を読むだけ（stop/start は上のアサーションで禁止）。
assert_present "dry-run: timer state read only" \
  '^is-active jarvis-backup[.]timer -> active$' "$tmp/logs/systemctl-query.log"
check_exit "dry-run: backup set still verifies" 0 bash "$verifier" "$set_dir"

# --- 2. fail-closed 検証ゲート --------------------------------------------
printf '%s\n' '-- fail-closed verification gates --'
corrupt="$tmp/corrupt"
cp -R -- "$set_dir" "$corrupt"
printf 'tampered config\n' > "$corrupt/config-mcp_allowlist.toml"
corrupt_code=0
corrupt_out="$(run_driver --backup-set "$corrupt" 2>&1)" || corrupt_code=$?
if [ "$corrupt_code" -ne 0 ]; then
  pass "hash mismatch rejected (exit $corrupt_code)"
else
  fail_case "hash mismatch was accepted"
fi
if has_fragment "$corrupt_out" 'MISMATCH'; then
  pass "hash mismatch reported"
else
  fail_case "hash mismatch not reported"
fi
assert_no_log "hash mismatch: psql never called" "$tmp/logs/psql.log"
assert_no_log "hash mismatch: curl never called" "$tmp/logs/curl.log"

legacy="$tmp/legacy"
cp -R -- "$set_dir" "$legacy"
rm -f -- "$legacy/COMPLETE"
# created_at ヘッダを落とし、verify-backup.sh のレガシー扱いに合わせる。
tail -n +2 "$set_dir/manifest.txt" > "$legacy/manifest.txt.new"
mv -- "$legacy/manifest.txt.new" "$legacy/manifest.txt"
legacy_code=0
legacy_out="$(run_driver --backup-set "$legacy" 2>&1)" || legacy_code=$?
if [ "$legacy_code" -ne 0 ]; then
  pass "missing COMPLETE rejected by default (exit $legacy_code)"
else
  fail_case "missing COMPLETE was accepted by default"
fi
if has_fragment "$legacy_out" 'COMPLETE marker missing'; then
  pass "missing COMPLETE reported"
else
  fail_case "missing COMPLETE not reported"
fi
legacy2_code=0
legacy2_out="$(run_driver --backup-set "$legacy" --allow-legacy 2>&1)" || legacy2_code=$?
if [ "$legacy2_code" -eq 0 ]; then
  pass "--allow-legacy accepts legacy fixture"
else
  fail_case "--allow-legacy rejected legacy fixture (exit $legacy2_code)"
  printf '%s\n' "$legacy2_out" | head -8 >&2
fi

# --- 3. --apply（明示指定時のみ。隔離先は一時ディレクトリ）----------------
if [ "$apply" -eq 1 ]; then
  printf '%s\n' '-- restore-local.sh --apply (isolated temp target) --'
  rm -f -- "$tmp/logs"/*.log
  apply_code=0
  apply_out="$(run_driver --backup-set "$set_dir" --apply 2>&1)" || apply_code=$?
  if [ "$apply_code" -eq 0 ]; then
    pass "--apply exit 0"
  else
    fail_case "--apply exit=$apply_code"
    printf '%s\n' "$apply_out" | head -20 >&2
  fi
  for fragment in 'dispatch: restore_pg' 'dispatch: restore_qdrant' \
                  'dispatch: restore_configs' 'dispatch: restore_git' \
                  'restore sequence finished' \
                  'restarting stopped services in reverse order'; do
    if has_fragment "$apply_out" "$fragment"; then
      pass "--apply output: $fragment"
    else
      fail_case "--apply output missing: $fragment"
    fi
  done

  # 事後検証: モック記録から「隔離先のみ・本番ゼロ」を確認する。
  assert_present "apply: isolated DB created" \
    'CREATE DATABASE "jarvis_restore_test"' "$tmp/logs/psql.log"
  assert_present "apply: isolated DB dropped after verification" \
    'DROP DATABASE "jarvis_restore_test"' "$tmp/logs/psql.log"
  assert_absent "apply: no production DROP DATABASE" \
    'DROP DATABASE "jarvis"' "$tmp/logs/psql.log"
  assert_absent "apply: no production DB as -d target" \
    '[-]d jarvis([[:space:]]|$)' "$tmp/logs/psql.log"
  assert_absent "apply: no production DB name in SQL" \
    "datname = 'jarvis'" "$tmp/logs/psql.log"

  assert_present "apply: isolated collection used" \
    'jarvis_memories_restore_test' "$tmp/logs/curl.log"
  assert_present "apply: priority=snapshot used" \
    'priority=snapshot' "$tmp/logs/curl.log"
  assert_absent "apply: production collection path never used" \
    '/collections/jarvis_memories([/?]|$)' "$tmp/logs/curl.log"
  assert_present "apply: points_count validated via mock jq" \
    '[.]result[.]points_count' "$tmp/logs/jq.log"

  assert_present "apply: service stop recorded" \
    '^stop jarvis-policy$' "$tmp/logs/systemctl.log"
  assert_present "apply: service restart recorded" \
    '^start jarvis-policy$' "$tmp/logs/systemctl.log"
  assert_absent "apply: qdrant service never stopped or started" \
    'qdrant' "$tmp/logs/systemctl.log"

  # configs が APPLY 経路を通ったこと出力で確認する。cmp は宛先が実在する
  # 場合だけ呼ばれるため、ログの有無に依存しない（-production read はモックが保証）。
  if has_fragment "$apply_out" 'configs: APPLY without CONFIRM'; then
    pass "apply: configs ran isolated apply without production write"
  else
    fail_case "apply: configs isolated apply path not observed"
  fi
  if has_fragment "$apply_out" "$tmp/tmp/"; then
    pass "apply: git extraction stayed under TMPDIR"
  else
    fail_case "apply: git extraction path not under TMPDIR"
  fi

  check_exit "apply: backup set still verifies" 0 bash "$verifier" "$set_dir"
  for name in "${all_payloads[@]}"; do
    if manifest_matches "$set_dir" "$name"; then
      pass "apply: manifest unchanged for $name"
    else
      fail_case "apply: manifest changed for $name"
    fi
  done
else
  printf '%s\n' '-- --apply not requested: isolated apply path skipped (default contract) --'
fi

# --- 4. non-allowlisted config ゲート（Round 66 回帰。常時実行）-------------
# verify_set() の allowlist 検査は stop_services() より前の必須ゲート。
# 非 allowlist の config-* を含むセットが --apply --confirm でも --skip-configs
# 付きでも拒否され、DB / Qdrant / サービスが一切触られないことを、
# 一時フィクスチャとモックだけで証明する（実 /etc・DB・Qdrant は使わない）。
printf '%s\n' '-- non-allowlisted config gate (Round 66 regression) --'
bad_cfg="$tmp/bad-config"
cp -R -- "$set_dir" "$bad_cfg" || exit 1
# 値はプレースホルダー。名前（bws.env）が allowlist 外であることだけが重要。
printf '%s\n' 'SYNTHETIC=not-a-real-secret' > "$bad_cfg/config-bws.env"
# サイズ / SHA を manifest に記録し、整合性チェックを通らせた上で
# allowlist ゲート単独の拒否であることを確認する。
{
  printf '# created_at=2026-11-19T00:00:00Z\n'
  for name in postgres-jarvis.sql.gz qdrant-snapshot.bin \
              config-mcp_allowlist.toml config-bws.env git-jarvis-head.tar.gz; do
    bytes="$(wc -c < "$bad_cfg/$name" | tr -d ' ')"
    sha="$(sha256sum "$bad_cfg/$name" | cut -d' ' -f1)"
    printf '%s %s %s\n' "$name" "$bytes" "$sha"
  done
} > "$bad_cfg/manifest.txt"
printf 'complete 2026-11-19T00:00:00Z\n' > "$bad_cfg/COMPLETE"

# 外部 verifier は config 名を検査しないため、このセットを受理する。
# したがって以降の拒否は restore-local.sh 自身の allowlist ゲートによるもの。
check_exit "verify-backup.sh accepts bad-config fixture (gate lives in driver)" \
  0 bash "$verifier" "$bad_cfg"

# 4a. --apply --confirm（本番上書き許可付きでもゲートが先に立つ）
rm -f -- "$tmp/logs"/*.log
bad_cfg_code=0
bad_cfg_out="$(run_driver --backup-set "$bad_cfg" --apply --confirm 2>&1)" || bad_cfg_code=$?
if [ "$bad_cfg_code" -ne 0 ]; then
  pass "bad config rejected with --apply --confirm (exit $bad_cfg_code)"
else
  fail_case "bad config accepted with --apply --confirm"
fi
if has_fragment "$bad_cfg_out" 'non-allowlisted config entry'; then
  pass "bad config: non-allowlisted config entry reported"
else
  fail_case "bad config: non-allowlisted config entry not reported"
  printf '%s\n' "$bad_cfg_out" | head -12 >&2
fi
if has_fragment "$bad_cfg_out" 'config-bws.env'; then
  pass "bad config: offending entry named"
else
  fail_case "bad config: offending entry not named"
fi
assert_no_log "bad config: psql never called" "$tmp/logs/psql.log"
assert_no_log "bad config: curl never called" "$tmp/logs/curl.log"
assert_no_log "bad config: no service stop/start" "$tmp/logs/systemctl.log"

# 4b. 同じセットを --skip-configs 付きでも呼ぶ。ゲートは依然として発火する。
rm -f -- "$tmp/logs"/*.log
bad_cfg_skip_code=0
bad_cfg_skip_out="$(run_driver --backup-set "$bad_cfg" --apply --confirm --skip-configs 2>&1)" || bad_cfg_skip_code=$?
if [ "$bad_cfg_skip_code" -ne 0 ]; then
  pass "bad config still rejected with --skip-configs (exit $bad_cfg_skip_code)"
else
  fail_case "bad config accepted with --skip-configs"
fi
if has_fragment "$bad_cfg_skip_out" 'non-allowlisted config entry'; then
  pass "--skip-configs: allowlist gate still fires"
else
  fail_case "--skip-configs: allowlist gate did not fire"
  printf '%s\n' "$bad_cfg_skip_out" | head -12 >&2
fi
assert_no_log "--skip-configs bad config: psql never called" "$tmp/logs/psql.log"
assert_no_log "--skip-configs bad config: curl never called" "$tmp/logs/curl.log"
assert_no_log "--skip-configs bad config: no service stop/start" "$tmp/logs/systemctl.log"

# --- 5. I73-A: active だが disabled な backup timer の停止 / fail-closed ------
# install-backup-timer.sh:123-133 の rollback（disable + start）が残す
# 「active だが disabled」な jarvis-backup.timer を、旧コードの
# is-enabled && is-active ゲートが停止対象から漏らしていた。状態付きモックで
# その構成を再現し、(a) 停止されて inactive が確認され dispatch に進むこと、
# (b) stop が効かず active のままなら DB 書き込み前に abort することを検証する。
# いずれも --apply 経路だが実 DB / 実 systemctl はモックのみ（安全境界は不変）。
printf '%s\n' '-- I73-A: disabled-but-active backup timer --'

# 5a. disabled でも active なら停止対象。停止後 inactive を確認し dispatch へ進む。
rm -f -- "$tmp/logs"/*.log
timer_ok_code=0
timer_ok_out="$(MOCK_DISABLED_UNITS='jarvis-backup.timer' \
  run_driver --backup-set "$set_dir" --apply 2>&1)" || timer_ok_code=$?
if [ "$timer_ok_code" -eq 0 ]; then
  pass "timer: disabled-but-active apply exit 0"
else
  fail_case "timer: disabled-but-active apply exit=$timer_ok_code"
  printf '%s\n' "$timer_ok_out" | head -20 >&2
fi
# モック状態が本当に「disabled だが active」だったことをクエリ記録で証明する。
assert_present "timer: mock reports disabled" \
  '^is-enabled jarvis-backup[.]timer -> disabled$' "$tmp/logs/systemctl-query.log"
assert_present "timer: mock reports active before stop" \
  '^is-active jarvis-backup[.]timer -> active$' "$tmp/logs/systemctl-query.log"
if has_fragment "$timer_ok_out" 'jarvis-backup.timer is disabled but active; stopping anyway'; then
  pass "timer: inventory keeps disabled-but-active timer"
else
  fail_case "timer: inventory dropped the disabled-but-active timer"
  printf '%s\n' "$timer_ok_out" | head -12 >&2
fi
assert_present "timer: stop recorded" \
  '^stop jarvis-backup[.]timer$' "$tmp/logs/systemctl.log"
assert_present "timer: inactive confirmed after stop" \
  '^is-active jarvis-backup[.]timer -> inactive$' "$tmp/logs/systemctl-query.log"
if has_fragment "$timer_ok_out" 'verified inactive after stop: jarvis-backup.timer'; then
  pass "timer: post-stop verification logged"
else
  fail_case "timer: post-stop verification log missing"
fi
assert_absent "timer: enablement bit never changed" \
  '^(enable|disable) ' "$tmp/logs/systemctl.log"
if has_fragment "$timer_ok_out" 'dispatch: restore_pg'; then
  pass "timer: dispatch proceeded after verified stop"
else
  fail_case "timer: dispatch did not proceed"
fi
assert_present "timer: DB write reached isolated target" \
  'CREATE DATABASE "jarvis_restore_test"' "$tmp/logs/psql.log"

# 5b. stop が効かず active のままなら fail-closed（dispatch / DB 書き込み前に終了）。
rm -f -- "$tmp/logs"/*.log
timer_fail_code=0
timer_fail_out="$(MOCK_DISABLED_UNITS='jarvis-backup.timer' \
  MOCK_STOP_INEFFECTIVE_UNITS='jarvis-backup.timer' \
  run_driver --backup-set "$set_dir" --apply 2>&1)" || timer_fail_code=$?
if [ "$timer_fail_code" -ne 0 ]; then
  pass "timer: stubborn active timer aborted (exit $timer_fail_code)"
else
  fail_case "timer: stubborn active timer was accepted"
fi
for fragment in 'timer still active after stop' 'aborting before dispatch'; do
  if has_fragment "$timer_fail_out" "$fragment"; then
    pass "timer: abort output: $fragment"
  else
    fail_case "timer: abort output missing: $fragment"
    printf '%s\n' "$timer_fail_out" | head -12 >&2
  fi
done
assert_present "timer: stop was attempted" \
  '^stop jarvis-backup[.]timer$' "$tmp/logs/systemctl.log"
assert_present "timer: post-stop check still saw active" \
  '^is-active jarvis-backup[.]timer -> active$' "$tmp/logs/systemctl-query.log"
assert_no_log "timer: psql never called before abort" "$tmp/logs/psql.log"
assert_no_log "timer: curl never called before abort" "$tmp/logs/curl.log"
if has_fragment "$timer_fail_out" 'dispatch: restore_pg'; then
  fail_case "timer: dispatch ran despite active timer"
else
  pass "timer: no module dispatch before abort"
fi
if has_fragment "$timer_fail_out" 'restore sequence finished'; then
  fail_case "timer: success line printed despite abort"
else
  pass "timer: no success line after abort"
fi

# --- 6. I74-A: 7列記述子のロール導出と fail-closed ゲート ---------------------
# TC-R1 / TC-R2 (specs/test-backup.md, REQ-20 / REQ-21)。
#   - 7 列 manifest (format/encryption/signature/role) の既定 none/none セット
#     が legacy 3 列と同等に通ること。
#   - 必須コンポーネント判定が固定名ではなく role 列から導出されること。
#   - 未知記述子が stop_services() より前の検証ゲートで拒否され、
#     psql / curl / systemctl モックが一切呼ばれないこと。
# 実 DB / 実 Qdrant / 実サービスは使わず、既存モックのみで検証する。
printf '%s\n' '-- I74-A: descriptor roles and fail-closed gate --'

# 6a. TC-R1: 7 列 none/none セットが legacy 3 列と同等に dry-run を通る。
described="$tmp/described"
cp -R -- "$set_dir" "$described" || exit 1
write_described_manifest "$described" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'git-jarvis-head.tar.gz|gzip|none|none|git' \
  > "$described/manifest.txt"
check_exit "TC-R1: verifier accepts described set" 0 bash "$verifier" "$described"
rm -f -- "$tmp/logs"/*.log
desc_code=0
desc_out="$(run_driver --backup-set "$described" 2>&1)" || desc_code=$?
if [ "$desc_code" -eq 0 ]; then
  pass "TC-R1: described set dry-run accepted (exit 0)"
else
  fail_case "TC-R1: described set dry-run rejected (exit $desc_code)"
  printf '%s\n' "$desc_out" | head -12 >&2
fi
if has_fragment "$desc_out" 'verification OK'; then
  pass "TC-R1: described set verification OK"
else
  fail_case "TC-R1: described set verification OK missing"
fi
assert_no_log "TC-R1: described dry-run: psql never called" "$tmp/logs/psql.log"
assert_no_log "TC-R1: described dry-run: curl never called" "$tmp/logs/curl.log"
assert_no_log "TC-R1: described dry-run: no service stop/start" "$tmp/logs/systemctl.log"

# 6b. TC-R1: ファイル名が postgres 固定名でなくても role 列で必須判定される。
# 名前に依存する旧経路なら has_pg=0 となり両 verifier が拒否するため、
# このケースが通ること自体が記述子駆動の証拠になる。
renamed="$tmp/described_renamed"
cp -R -- "$set_dir" "$renamed" || exit 1
mv -- "$renamed/postgres-jarvis.sql.gz" "$renamed/pg-data.sql.gz"
write_described_manifest "$renamed" \
  'pg-data.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'git-jarvis-head.tar.gz|gzip|none|none|git' \
  > "$renamed/manifest.txt"
check_exit "TC-R1: verifier accepts renamed payload via role" 0 \
  bash "$verifier" "$renamed"
rm -f -- "$tmp/logs"/*.log
rename_code=0
rename_out="$(run_driver --backup-set "$renamed" 2>&1)" || rename_code=$?
if [ "$rename_code" -eq 0 ]; then
  pass "TC-R1: renamed payload accepted via role (exit 0)"
else
  fail_case "TC-R1: renamed payload rejected (exit $rename_code)"
  printf '%s\n' "$rename_out" | head -12 >&2
fi
if has_fragment "$rename_out" 'verification OK'; then
  pass "TC-R1: renamed payload verification OK"
else
  fail_case "TC-R1: renamed payload verification OK missing"
fi
assert_no_log "TC-R1: renamed dry-run: psql never called" "$tmp/logs/psql.log"
assert_no_log "TC-R1: renamed dry-run: curl never called" "$tmp/logs/curl.log"
assert_no_log "TC-R1: renamed dry-run: no service stop/start" "$tmp/logs/systemctl.log"

# 6c. TC-R2: 未知 signature (ed25519) を含む set は --apply でも stop_services
# に到達する前に拒否され、DB / Qdrant / サービスが一切触られないことを証明する。
unknown="$tmp/described_unknown"
cp -R -- "$set_dir" "$unknown" || exit 1
write_described_manifest "$unknown" \
  'postgres-jarvis.sql.gz|gzip|none|ed25519|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'git-jarvis-head.tar.gz|gzip|none|none|git' \
  > "$unknown/manifest.txt"
check_exit "TC-R2: verifier rejects unknown descriptor" 1 \
  bash "$verifier" "$unknown"
rm -f -- "$tmp/logs"/*.log
unknown_code=0
unknown_out="$(run_driver --backup-set "$unknown" --apply 2>&1)" || unknown_code=$?
if [ "$unknown_code" -ne 0 ]; then
  pass "TC-R2: unknown descriptor rejected with --apply (exit $unknown_code)"
else
  fail_case "TC-R2: unknown descriptor accepted with --apply"
fi
for fragment in 'FORMAT MISMATCH' 'signature=ed25519' \
                'verification FAILED' 'no writes performed'; do
  if has_fragment "$unknown_out" "$fragment"; then
    pass "TC-R2: reject output: $fragment"
  else
    fail_case "TC-R2: reject output missing: $fragment"
    printf '%s\n' "$unknown_out" | head -12 >&2
  fi
done
assert_no_log "TC-R2: psql never called" "$tmp/logs/psql.log"
assert_no_log "TC-R2: curl never called" "$tmp/logs/curl.log"
assert_no_log "TC-R2: jq never called" "$tmp/logs/jq.log"
assert_no_log "TC-R2: cmp never called" "$tmp/logs/cmp.log"
assert_no_log "TC-R2: no service stop/start" "$tmp/logs/systemctl.log"
if has_fragment "$unknown_out" 'stopping service:'; then
  fail_case "TC-R2: stop_services ran despite unknown descriptor"
else
  pass "TC-R2: stop_services never reached"
fi
if has_fragment "$unknown_out" 'dispatch: restore_pg'; then
  fail_case "TC-R2: dispatch ran despite unknown descriptor"
else
  pass "TC-R2: no module dispatch before the gate"
fi

# --- 6d. I75-B: role=postgres ペイロードを pg-data.sql.gz へ改名しても復元可 -
# 6b は dry-run のみだった。ここでは --apply 経路で 4 モジュールが固定名ではなく
# 共有ヘルパ resolve_restore_input() が manifest の role 記述子から解決した
# パスを読むことを、モックの呼び出し記録で証明する（実 DB / Qdrant は使わない）。
printf '%s\n' '-- I75-B: renamed postgres payload restore via role descriptor --'
rm -f -- "$tmp/logs"/*.log
renamed_apply_code=0
renamed_apply_out="$(run_driver --backup-set "$renamed" --apply 2>&1)" || renamed_apply_code=$?
if [ "$renamed_apply_code" -eq 0 ]; then
  pass "I75-B: renamed payload --apply exit 0"
else
  fail_case "I75-B: renamed payload --apply exit=$renamed_apply_code"
  printf '%s\n' "$renamed_apply_out" | head -20 >&2
fi
for fragment in 'dispatch: restore_pg' 'PG: resolved input = ' \
                'pg-data.sql.gz' 'dispatch: restore_qdrant' \
                'dispatch: restore_configs' 'dispatch: restore_git' \
                'restore sequence finished'; do
  if has_fragment "$renamed_apply_out" "$fragment"; then
    pass "I75-B: renamed apply output: $fragment"
  else
    fail_case "I75-B: renamed apply output missing: $fragment"
  fi
done
if has_fragment "$renamed_apply_out" 'バックアップが見つかりません'; then
  fail_case "I75-B: module fell back to the fixed postgres filename"
else
  pass "I75-B: no fixed-filename fallback error"
fi
if has_fragment "$renamed_apply_out" 'cannot resolve a unique postgres payload'; then
  fail_case "I75-B: resolver rejected the renamed role=postgres payload"
else
  pass "I75-B: resolver accepted the renamed role=postgres payload"
fi
assert_present "I75-B: isolated DB restored from resolved payload" \
  'CREATE DATABASE "jarvis_restore_test"' "$tmp/logs/psql.log"
assert_absent "I75-B: no production DB DROP" \
  'DROP DATABASE "jarvis"' "$tmp/logs/psql.log"
check_exit "I75-B: renamed set still verifies after apply" 0 \
  bash "$verifier" "$renamed"
if manifest_matches "$renamed" "pg-data.sql.gz"; then
  pass "I75-B: manifest unchanged for pg-data.sql.gz"
else
  fail_case "I75-B: manifest changed for pg-data.sql.gz"
fi

# --- 7. I76-A: 入力解決ゲートが stop_services() より前に発火すること ---------
# Round 72 で 4 モジュールの入力は共有 resolve_restore_input() に統一されたが、
# その fail-closed は dispatch_restore() 内（stop_services() 後）にしか無い。
# 未知 role / 複数一致 / 解決先不在の set が --apply でも停止前に拒否され、
# systemctl.log に stop が出ないことを既存モックだけで証明する。
printf '%s\n' '-- I76-A: pre-stop restore-input resolution gate --'

# 7a. 未知 role (oracledb)。記述子ループが先に拒否するが、解決ゲートも同じ
#     セットを止め、サービスが一切触られないことを合わせて確認する。
unknown_role="$tmp/described_unknown_role"
cp -R -- "$set_dir" "$unknown_role" || exit 1
write_described_manifest "$unknown_role" \
  'postgres-jarvis.sql.gz|gzip|none|none|oracledb' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'git-jarvis-head.tar.gz|gzip|none|none|git' \
  > "$unknown_role/manifest.txt"
check_exit "I76-A: verifier rejects unknown role" 1 bash "$verifier" "$unknown_role"
rm -f -- "$tmp/logs"/*.log
unknown_role_code=0
unknown_role_out="$(run_driver --backup-set "$unknown_role" --apply 2>&1)" || unknown_role_code=$?
if [ "$unknown_role_code" -ne 0 ]; then
  pass "I76-A: unknown role rejected with --apply (exit $unknown_role_code)"
else
  fail_case "I76-A: unknown role accepted with --apply"
fi
for fragment in 'unsupported role descriptor: oracledb' \
                'cannot resolve postgres restore input' \
                'verification FAILED' 'no writes performed'; do
  if has_fragment "$unknown_role_out" "$fragment"; then
    pass "I76-A: unknown role output: $fragment"
  else
    fail_case "I76-A: unknown role output missing: $fragment"
    printf '%s\n' "$unknown_role_out" | head -12 >&2
  fi
done
assert_no_log "I76-A: unknown role: no service stop/start" "$tmp/logs/systemctl.log"
assert_no_log "I76-A: unknown role: no systemctl query (gate precedes inventory)" \
  "$tmp/logs/systemctl-query.log"
assert_no_log "I76-A: unknown role: psql never called" "$tmp/logs/psql.log"
assert_no_log "I76-A: unknown role: curl never called" "$tmp/logs/curl.log"
if has_fragment "$unknown_role_out" 'stopping service:'; then
  fail_case "I76-A: unknown role: stop_services ran before the gate"
else
  pass "I76-A: unknown role: stop_services never reached"
fi
if has_fragment "$unknown_role_out" 'dispatch: restore_pg'; then
  fail_case "I76-A: unknown role: dispatch ran before the gate"
else
  pass "I76-A: unknown role: no module dispatch before the gate"
fi

# 7b. 同一 role=postgres が 2 行。両ファイルは有効な pg ダンプなので従来の
#     整合性/書式ゲートと外部 verifier は通過する。one モードの resolver だけが
#     ambiguous (rc=1) を検出し、stop 前に拒否できたことを証明する关键点。
multi="$tmp/described_multi_pg"
cp -R -- "$set_dir" "$multi" || exit 1
{
  printf '%s\n' '-- PostgreSQL database dump' \
    'CREATE TABLE t2 (id integer);' \
    '-- PostgreSQL database dump complete'
} | gzip > "$multi/pg-extra.sql.gz"
write_described_manifest "$multi" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'pg-extra.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'git-jarvis-head.tar.gz|gzip|none|none|git' \
  > "$multi/manifest.txt"
check_exit "I76-A: verifier accepts two valid postgres rows" 0 bash "$verifier" "$multi"
rm -f -- "$tmp/logs"/*.log
multi_code=0
multi_out="$(run_driver --backup-set "$multi" --apply 2>&1)" || multi_code=$?
if [ "$multi_code" -ne 0 ]; then
  pass "I76-A: multiple postgres matches rejected (exit $multi_code)"
else
  fail_case "I76-A: multiple postgres matches accepted"
fi
for fragment in 'cannot resolve postgres restore input' 'rc=1' 'ambiguous' \
                'verification FAILED' 'no writes performed'; do
  if has_fragment "$multi_out" "$fragment"; then
    pass "I76-A: multiple matches output: $fragment"
  else
    fail_case "I76-A: multiple matches output missing: $fragment"
    printf '%s\n' "$multi_out" | head -12 >&2
  fi
done
assert_no_log "I76-A: multiple matches: no service stop/start" "$tmp/logs/systemctl.log"
assert_no_log "I76-A: multiple matches: no systemctl query (gate precedes inventory)" \
  "$tmp/logs/systemctl-query.log"
assert_no_log "I76-A: multiple matches: psql never called" "$tmp/logs/psql.log"
assert_no_log "I76-A: multiple matches: curl never called" "$tmp/logs/curl.log"
if has_fragment "$multi_out" 'stopping service:'; then
  fail_case "I76-A: multiple matches: stop_services ran before the gate"
else
  pass "I76-A: multiple matches: stop_services never reached"
fi
if has_fragment "$multi_out" 'dispatch: restore_pg'; then
  fail_case "I76-A: multiple matches: dispatch ran before the gate"
else
  pass "I76-A: multiple matches: no module dispatch before the gate"
fi

# 7c. 改名済み pg に role=other を付けたケース。basename だけで必須判定する
#     従来ゲートは通過するが、resolver は role 一致も legacy 名も見つけられず
#     rc=2（必須 postgres の入力解決不能）を返す。これも stop 前に拒否される。
orphan="$tmp/described_orphan_pg"
cp -R -- "$set_dir" "$orphan" || exit 1
mv -- "$orphan/postgres-jarvis.sql.gz" "$orphan/pg-orphan.sql.gz"
write_described_manifest "$orphan" \
  'pg-orphan.sql.gz|gzip|none|none|other' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'git-jarvis-head.tar.gz|gzip|none|none|git' \
  > "$orphan/manifest.txt"
check_exit "I76-A: verifier accepts orphan pg row (basename-driven presence)" 0 \
  bash "$verifier" "$orphan"
rm -f -- "$tmp/logs"/*.log
orphan_code=0
orphan_out="$(run_driver --backup-set "$orphan" --apply 2>&1)" || orphan_code=$?
if [ "$orphan_code" -ne 0 ]; then
  pass "I76-A: unresolvable required input rejected (exit $orphan_code)"
else
  fail_case "I76-A: unresolvable required input accepted"
fi
for fragment in 'cannot resolve postgres restore input' 'rc=2' \
                'verification FAILED' 'no writes performed'; do
  if has_fragment "$orphan_out" "$fragment"; then
    pass "I76-A: unresolvable input output: $fragment"
  else
    fail_case "I76-A: unresolvable input output missing: $fragment"
    printf '%s\n' "$orphan_out" | head -12 >&2
  fi
done
assert_no_log "I76-A: unresolvable input: no service stop/start" "$tmp/logs/systemctl.log"
assert_no_log "I76-A: unresolvable input: no systemctl query (gate precedes inventory)" \
  "$tmp/logs/systemctl-query.log"
assert_no_log "I76-A: unresolvable input: psql never called" "$tmp/logs/psql.log"
assert_no_log "I76-A: unresolvable input: curl never called" "$tmp/logs/curl.log"
if has_fragment "$orphan_out" 'stopping service:'; then
  fail_case "I76-A: unresolvable input: stop_services ran before the gate"
else
  pass "I76-A: unresolvable input: stop_services never reached"
fi

# --- 8. I77-C: 失敗注入モックによる fail-closed 分岐の回帰 -------------------
# 旧モックは curl / psql / jq が常に成功を返すため、次の分岐が未検証だった。
#   - qdrant.sh:63-93   HTTP 非2xx / curl_rc!=0 / points_count<=0 / 不正値
#   - qdrant.sh:113-141 本番 upload / probe の HTTP 2xx・curl_rc・points_count
#   - pg.sh:76-98       psql 失敗 / 件数不正 / 0件 / 退避前の隔離 DB 処理
#   - pg.sh:135-165     退避失敗 / 退避確認失敗 / 本番 DROP・CREATE・復元失敗
# 各ケースで fail() が非ゼロ終了することを --apply と --apply --confirm の双方で
# 確認し、本番 DB / Qdrant / サービスが触られないことをアサートする。
# Qdrant 系ケースには --skip-db を付け、confirm 時に pg.sh が先に本番 DB を
# 操作して「本番未接触」のアサートを汚さないようにする。本番段のケースでは
# --apply では分岐に到達しない（成功・本番未接触）ことも合わせて確認する。
# 実 DB / 実 Qdrant / 実 systemctl は一切使わない（既存モックと :1 のまま）。
printf '%s\n' '-- I77-C: failure-injection fail-closed branches --'

assert_no_production_contact() { # label
  local label="$1"
  assert_absent "$label: no production DB DROP" \
    'DROP DATABASE "jarvis"' "$tmp/logs/psql.log"
  assert_absent "$label: no production DB as -d target" \
    '[-]d jarvis([[:space:]]|$)' "$tmp/logs/psql.log"
  assert_absent "$label: no production DB name in SQL" \
    "datname = 'jarvis'" "$tmp/logs/psql.log"
  assert_absent "$label: no production collection path" \
    '/collections/jarvis_memories([/?]|$)' "$tmp/logs/curl.log"
  assert_absent "$label: qdrant service never touched" \
    'qdrant' "$tmp/logs/systemctl.log"
  assert_absent "$label: enablement bit never changed" \
    '^(enable|disable) ' "$tmp/logs/systemctl.log"
}

assert_service_invariants() { # label
  local label="$1"
  assert_absent "$label: qdrant service never touched" \
    'qdrant' "$tmp/logs/systemctl.log"
  assert_absent "$label: enablement bit never changed" \
    '^(enable|disable) ' "$tmp/logs/systemctl.log"
}

# inject_fail <curl_mode> <psql_mode> <jq_mode> <confirm> <label> <fragment> [extra...]
inject_fail() {
  local curl_mode="$1" psql_mode="$2" jq_mode="$3" confirm="$4"
  local label="$5" fragment="$6" code=0 out=""
  shift 6
  local -a args=(--backup-set "$set_dir" --apply) extra=("$@")
  [ "$confirm" -eq 1 ] && args+=(--confirm)
  args+=("${extra[@]}")
  rm -f -- "$tmp/logs"/*.log "$tmp/logs/jq.calls"
  MOCK_CURL_MODE="$curl_mode"
  MOCK_PSQL_MODE="$psql_mode"
  MOCK_JQ_MODE="$jq_mode"
  out="$(run_driver "${args[@]}" 2>&1)" || code=$?
  if [ "$code" -ne 0 ]; then
    pass "$label (exit $code)"
  else
    fail_case "$label: fail() did not exit non-zero"
    printf '%s\n' "$out" | head -12 >&2
  fi
  if has_fragment "$out" "$fragment"; then
    pass "$label: fail vocabulary"
  else
    fail_case "$label: fail vocabulary missing: $fragment"
    printf '%s\n' "$out" | head -12 >&2
  fi
}

# both <curl_mode> <psql_mode> <jq_mode> <label> <fragment> [extra...]
# 隔離段の失敗: --apply / --apply --confirm の双方で同じ fail() に到達する。
both() {
  local curl_mode="$1" psql_mode="$2" jq_mode="$3" label="$4" fragment="$5"
  shift 5
  local -a extra=("$@")
  inject_fail "$curl_mode" "$psql_mode" "$jq_mode" 0 \
    "$label: --apply" "$fragment" "${extra[@]}"
  assert_no_production_contact "$label: --apply"
  inject_fail "$curl_mode" "$psql_mode" "$jq_mode" 1 \
    "$label: --apply --confirm" "$fragment" "${extra[@]}"
  assert_no_production_contact "$label: --apply --confirm"
}

# prod_stage <curl_mode> <psql_mode> <jq_mode> <label> <fragment> [extra...]
# 本番段の失敗: --apply では到達せず成功（本番未接触）、
# --apply --confirm で fail() が非ゼロ終了。双方でサービス不変を確認する。
prod_stage() {
  local curl_mode="$1" psql_mode="$2" jq_mode="$3" label="$4" fragment="$5"
  shift 5
  local -a extra=("$@")
  local code=0 out=""
  rm -f -- "$tmp/logs"/*.log "$tmp/logs/jq.calls"
  MOCK_CURL_MODE="$curl_mode"
  MOCK_PSQL_MODE="$psql_mode"
  MOCK_JQ_MODE="$jq_mode"
  out="$(run_driver --backup-set "$set_dir" --apply "${extra[@]}" 2>&1)" || code=$?
  if [ "$code" -eq 0 ]; then
    pass "$label: --apply stays green (branch unreachable)"
  else
    fail_case "$label: --apply unexpectedly failed (exit $code)"
    printf '%s\n' "$out" | head -12 >&2
  fi
  assert_no_production_contact "$label: --apply"
  inject_fail "$curl_mode" "$psql_mode" "$jq_mode" 1 \
    "$label: --apply --confirm" "$fragment" "${extra[@]}"
  assert_service_invariants "$label: --apply --confirm"
}

# 8a. Qdrant 隔離段（qdrant.sh:63-93）。--skip-db で本番 DB を完全に除外。
both isolated_upload_non2xx '' '' \
  'curl: isolated upload HTTP 500' 'isolated snapshot upload failed' --skip-db
both isolated_upload_rc '' '' \
  'curl: isolated upload curl_rc=7' 'curl_rc=7' --skip-db
both isolated_probe_non2xx '' '' \
  'curl: isolated probe HTTP 500' 'isolated collection info request failed' --skip-db
both isolated_probe_rc '' '' \
  'curl: isolated probe curl_rc=7' 'curl_rc=7' --skip-db
both isolated_probe_zero '' '' \
  'curl: isolated points_count=0' 'isolated collection has no points' --skip-db
both isolated_probe_bad '' '' \
  'curl: isolated points_count non-numeric' \
  'unexpected points_count format in isolated collection' --skip-db
both isolated_delete_non2xx '' '' \
  'curl: isolated cleanup HTTP 500' 'isolated collection cleanup failed' --skip-db
both isolated_delete_rc '' '' \
  'curl: isolated cleanup curl_rc=7' 'curl_rc=7' --skip-db
both '' '' fail \
  'jq: isolated points_count read failure' \
  'could not read points_count from isolated collection' --skip-db
both '' '' bad \
  'jq: isolated points_count non-numeric' \
  'unexpected points_count format in isolated collection' --skip-db
both '' '' zero \
  'jq: isolated points_count=0' 'isolated collection has no points' --skip-db

# 8b. PostgreSQL 隔離段（pg.sh:76-112）。ここで失敗すれば本番段へ進まない。
both '' exists_fail '' \
  'psql: isolated existence check failure' '隔離DB の存在確認に失敗しました'
both '' exists_nonempty '' \
  'psql: isolated DB already exists' 'が既に存在します'
both '' create_fail '' \
  'psql: isolated CREATE failure' 'の作成に失敗しました'
both '' restore_fail '' \
  'psql: isolated restore failure' 'への復元に失敗しました'
both '' count_fail '' \
  'psql: isolated count query failure' 'テーブル件数取得に失敗しました'
both '' count_bad '' \
  'psql: isolated count non-numeric' 'テーブル件数が不正です'
both '' count_zero '' \
  'psql: isolated count 0' 'テーブル件数が 0 です'
both '' drop_fail '' \
  'psql: isolated DROP failure' 'の DROP に失敗しました'

# 8c. PostgreSQL 本番段（pg.sh:135-178）。--confirm 時のみ到達。
prod_stage '' backup_fail '' \
  'psql: production backup CREATE failure' '旧DB の退避に失敗しました'
prod_stage '' backup_verify_fail '' \
  'psql: production backup verification failure' '退避DB の確認に失敗しました'
prod_stage '' backup_missing '' \
  'psql: production backup missing' '退避DB が見つかりません'
prod_stage '' prod_drop_fail '' \
  'psql: production DROP failure' "旧DB 'jarvis' の DROP に失敗しました"
prod_stage '' prod_create_fail '' \
  'psql: production CREATE failure' "本番DB 'jarvis' の CREATE に失敗しました"
prod_stage '' prod_restore_fail '' \
  'psql: production restore failure' "本番DB 'jarvis' への復元に失敗しました"
prod_stage '' prod_count_fail '' \
  'psql: production count query failure' '本番DB のテーブル件数取得に失敗しました'
prod_stage '' prod_count_bad '' \
  'psql: production count non-numeric' '本番DB のテーブル件数が不正です'
prod_stage '' prod_count_zero '' \
  'psql: production count 0' '本番DB のテーブル件数が 0 です'

# 8d. Qdrant 本番段（qdrant.sh:113-141）。--skip-db で本番 DB を除外。
prod_stage prod_upload_non2xx '' '' \
  'curl: production upload HTTP 500' 'production snapshot upload failed' --skip-db
prod_stage prod_upload_rc '' '' \
  'curl: production upload curl_rc=7' 'curl_rc=7' --skip-db
prod_stage prod_probe_non2xx '' '' \
  'curl: production probe HTTP 500' 'production collection info request failed' --skip-db
prod_stage prod_probe_rc '' '' \
  'curl: production probe curl_rc=7' 'curl_rc=7' --skip-db
prod_stage '' '' prod_fail \
  'jq: production points_count read failure' \
  'could not read points_count from production collection' --skip-db
prod_stage '' '' prod_zero \
  'jq: production points_count=0' \
  'production collection has no points after restore' --skip-db
prod_stage '' '' prod_bad \
  'jq: production points_count non-numeric' \
  'unexpected points_count format in production collection' --skip-db

# --- 9. I77-A: role=other の config-* / git* は stop 前に拒否される ----------
# is_config_payload / is_git_payload は basename からコンポーネントを数える一方、
# 旧コードの role_basename_consistent は未知 role (other) に無条件 0 を返し、
# resolve_restore_input は row_role==config/git しか一致させなかった。結果 rc=2
# (absent) が「nothing to do 成功」扱いになり、/etc/jarvis の古い設定や古い git
# チェックアウトが残った。共有 resolved_role_for() が双方を同じ role に解決し、
# 明示 role と basename の矛盾を fail-closed にすることをここで固定する。
printf '%s\n' '-- I77-A: role=other config-*/git* rejected before stop --'

# 9a. config-* に明示 role=other。外部 verifier は role 列を補助情報として受理
# するので、拒否が restore-local.sh 自身の共有 role ゲートによると分離できる。
other_cfg="$tmp/described_other_config"
cp -R -- "$set_dir" "$other_cfg" || exit 1
write_described_manifest "$other_cfg" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|other' \
  'git-jarvis-head.tar.gz|gzip|none|none|git' \
  > "$other_cfg/manifest.txt"
# I78-A unified role resolution in verify-backup.sh too: both verifier and
# driver now reject an explicit role that disagrees with its basename.
check_exit "I78-A: verifier rejects other-role config row" 1 bash "$verifier" "$other_cfg"
rm -f -- "$tmp/logs"/*.log
other_cfg_code=0
other_cfg_out="$(run_driver --backup-set "$other_cfg" --apply 2>&1)" || other_cfg_code=$?
if [ "$other_cfg_code" -ne 0 ]; then
  pass "I77-A: other-role config rejected with --apply (exit $other_cfg_code)"
else
  fail_case "I77-A: other-role config accepted with --apply"
fi
for fragment in 'role=other is not consistent with its basename' 'ROLE MISMATCH' \
                'config-mcp_allowlist.toml' 'verification FAILED' 'no writes performed'; do
  if has_fragment "$other_cfg_out" "$fragment"; then
    pass "I77-A: other-role config output: $fragment"
  else
    fail_case "I77-A: other-role config output missing: $fragment"
    printf '%s\n' "$other_cfg_out" | head -12 >&2
  fi
done
assert_no_log "I77-A: other-role config: no service stop/start" "$tmp/logs/systemctl.log"
assert_no_log "I77-A: other-role config: no systemctl query (gate precedes inventory)" \
  "$tmp/logs/systemctl-query.log"
assert_no_log "I77-A: other-role config: psql never called" "$tmp/logs/psql.log"
assert_no_log "I77-A: other-role config: curl never called" "$tmp/logs/curl.log"
if has_fragment "$other_cfg_out" 'stopping service:'; then
  fail_case "I77-A: other-role config: stop_services ran before the gate"
else
  pass "I77-A: other-role config: stop_services never reached"
fi
if has_fragment "$other_cfg_out" 'dispatch: restore_configs'; then
  fail_case "I77-A: other-role config: configs dispatch ran before the gate"
else
  pass "I77-A: other-role config: no module dispatch before the gate"
fi

# 9b. git*.tar.gz を別名 (git-extra.tar.gz) にして明示 role=other。旧コードでは
# legacy_role_for が固定名しか git を出さないため role=other のままで矛盾が
# 検出されず、resolver が rc=2 を返して「success」になっていたケース。
other_git="$tmp/described_other_git"
cp -R -- "$set_dir" "$other_git" || exit 1
mv -- "$other_git/git-jarvis-head.tar.gz" "$other_git/git-extra.tar.gz" || exit 1
write_described_manifest "$other_git" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'git-extra.tar.gz|gzip|none|none|other' \
  > "$other_git/manifest.txt"
check_exit "I78-A: verifier rejects other-role git row" 1 bash "$verifier" "$other_git"
rm -f -- "$tmp/logs"/*.log
other_git_code=0
other_git_out="$(run_driver --backup-set "$other_git" --apply 2>&1)" || other_git_code=$?
if [ "$other_git_code" -ne 0 ]; then
  pass "I77-A: other-role git rejected with --apply (exit $other_git_code)"
else
  fail_case "I77-A: other-role git accepted with --apply"
fi
for fragment in 'role=other is not consistent with its basename' 'ROLE MISMATCH' \
                'git-extra.tar.gz' 'verification FAILED' 'no writes performed'; do
  if has_fragment "$other_git_out" "$fragment"; then
    pass "I77-A: other-role git output: $fragment"
  else
    fail_case "I77-A: other-role git output missing: $fragment"
    printf '%s\n' "$other_git_out" | head -12 >&2
  fi
done
assert_no_log "I77-A: other-role git: no service stop/start" "$tmp/logs/systemctl.log"
assert_no_log "I77-A: other-role git: no systemctl query (gate precedes inventory)" \
  "$tmp/logs/systemctl-query.log"
assert_no_log "I77-A: other-role git: psql never called" "$tmp/logs/psql.log"
assert_no_log "I77-A: other-role git: curl never called" "$tmp/logs/curl.log"
if has_fragment "$other_git_out" 'stopping service:'; then
  fail_case "I77-A: other-role git: stop_services ran before the gate"
else
  pass "I77-A: other-role git: stop_services never reached"
fi

# 9c. ポジティブ対照: 3列 (role 空) の git-extra.tar.gz が共有 role 解決で git と
# して解決され、rc=2 の「nothing to do」に落ちないことを dry-run で確認する。
legacy_git="$tmp/described_legacy_git"
cp -R -- "$set_dir" "$legacy_git" || exit 1
mv -- "$legacy_git/git-jarvis-head.tar.gz" "$legacy_git/git-extra.tar.gz" || exit 1
{
  printf '# created_at=2026-11-20T00:00:00Z\n'
  for name in postgres-jarvis.sql.gz qdrant-snapshot.bin \
              config-mcp_allowlist.toml git-extra.tar.gz; do
    bytes="$(wc -c < "$legacy_git/$name" | tr -d ' ')"
    sha="$(sha256sum "$legacy_git/$name" | cut -d' ' -f1)"
    printf '%s %s %s\n' "$name" "$bytes" "$sha"
  done
} > "$legacy_git/manifest.txt"
rm -f -- "$tmp/logs"/*.log
legacy_git_code=0
legacy_git_out="$(run_driver --backup-set "$legacy_git" 2>&1)" || legacy_git_code=$?
if [ "$legacy_git_code" -eq 0 ]; then
  pass "I77-A: legacy git*.tar.gz accepted via shared role resolver (exit 0)"
else
  fail_case "I77-A: legacy git*.tar.gz rejected (exit $legacy_git_code)"
  printf '%s\n' "$legacy_git_out" | head -12 >&2
fi
if has_fragment "$legacy_git_out" 'RESOLVED: git input = ' \
   && has_fragment "$legacy_git_out" 'git-extra.tar.gz'; then
  pass "I77-A: git input resolved to the renamed git*.tar.gz"
else
  fail_case "I77-A: renamed git*.tar.gz not resolved as role=git"
fi
if has_fragment "$legacy_git_out" 'cannot resolve git restore input'; then
  fail_case "I77-A: resolver returned rc=2 for the renamed git payload"
else
  pass "I77-A: no rc=2 no-op for the renamed git payload"
fi
assert_no_log "I77-A: legacy git dry-run: psql never called" "$tmp/logs/psql.log"
assert_no_log "I77-A: legacy git dry-run: no service stop/start" "$tmp/logs/systemctl.log"

# 9d. 任意コンポーネントが欠落しただけの set も rc=2 を成功扱いしない。
# config を外した set は外部 verifier も拒否するが、ドライバ自身の
# 「cannot resolve configs restore input (rc=2)」が stop 前に出ることを確認する。
absent_cfg="$tmp/absent_config"
cp -R -- "$set_dir" "$absent_cfg" || exit 1
rm -f -- "$absent_cfg/config-mcp_allowlist.toml"
write_described_manifest "$absent_cfg" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'git-jarvis-head.tar.gz|gzip|none|none|git' \
  > "$absent_cfg/manifest.txt"
check_exit "I77-A: verifier rejects set without config" 1 bash "$verifier" "$absent_cfg"
rm -f -- "$tmp/logs"/*.log
absent_cfg_code=0
absent_cfg_out="$(run_driver --backup-set "$absent_cfg" --apply 2>&1)" || absent_cfg_code=$?
if [ "$absent_cfg_code" -ne 0 ]; then
  pass "I77-A: absent optional config rejected (exit $absent_cfg_code)"
else
  fail_case "I77-A: absent optional config accepted as a no-op"
fi
for fragment in 'cannot resolve configs restore input' 'rc=2' \
                'verification FAILED' 'no writes performed'; do
  if has_fragment "$absent_cfg_out" "$fragment"; then
    pass "I77-A: absent config output: $fragment"
  else
    fail_case "I77-A: absent config output missing: $fragment"
    printf '%s\n' "$absent_cfg_out" | head -12 >&2
  fi
done
assert_no_log "I77-A: absent config: no service stop/start" "$tmp/logs/systemctl.log"
assert_no_log "I77-A: absent config: psql never called" "$tmp/logs/psql.log"

# 9e. git が欠落した set は外部 verifier は受理するので、rc=2 拒否が
# ドライバの pre-stop ゲートによることを分離して確認する。
absent_git="$tmp/absent_git"
cp -R -- "$set_dir" "$absent_git" || exit 1
rm -f -- "$absent_git/git-jarvis-head.tar.gz"
write_described_manifest "$absent_git" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  > "$absent_git/manifest.txt"
check_exit "I77-A: verifier accepts set without git" 0 bash "$verifier" "$absent_git"
rm -f -- "$tmp/logs"/*.log
absent_git_code=0
absent_git_out="$(run_driver --backup-set "$absent_git" --apply 2>&1)" || absent_git_code=$?
if [ "$absent_git_code" -ne 0 ]; then
  pass "I77-A: absent optional git rejected (exit $absent_git_code)"
else
  fail_case "I77-A: absent optional git accepted as a no-op"
fi
for fragment in 'cannot resolve git restore input' 'rc=2' \
                'verification FAILED' 'no writes performed'; do
  if has_fragment "$absent_git_out" "$fragment"; then
    pass "I77-A: absent git output: $fragment"
  else
    fail_case "I77-A: absent git output missing: $fragment"
    printf '%s\n' "$absent_git_out" | head -12 >&2
  fi
done
assert_no_log "I77-A: absent git: no service stop/start" "$tmp/logs/systemctl.log"
assert_no_log "I77-A: absent git: psql never called" "$tmp/logs/psql.log"

# --- 10. I78-B: resolver rc=2 / rc=1 が module 内でも fail-closed -------------
# 背景: git.sh は resolver の rc=2 (absent / 未解決) を「任意コンポーネントの
# 警告付き成功スキップ」に扱っていた。pre-stop ゲート (verify_set) が先に失敗
# するため通常フローでは到達不能だが、防御的二重チェックとして module 側も
# fail-closed であることを固定する。ゲートを通り抜けた状態は、共有 resolver /
# log / fail をスタブした隔離シェルで git.sh を source して再現する。
# 実 DB / 実 Qdrant / 本番パスは一切使わない（既存モックと同じ安全境界）。
printf '%s\n' '-- I78-B: git module fail-closed on resolver rc=2 / rc=1 --'

git_mod="$repo/infra/scripts/restore-local.d/git.sh"
if [ ! -f "$git_mod" ]; then
  fail_case "I78-B: git module not found: $git_mod"
else
  # スタブ: resolver は指定 rc を返し、rc=0 のときだけ解決先パスを出力する。
  # fail() は終了せず呼び出しを記録する（git.sh の `|| true` と return 1 を
  # 観測可能にするため）。
  cat > "$tmp/git-resolver-stub.sh" <<'STUB'
resolve_restore_input() {
  local rc="${STUB_RESOLVER_RC:-0}"
  if [ "$rc" -eq 0 ]; then
    printf '%s\n' "${STUB_RESOLVED_PATH:-}"
  fi
  return "$rc"
}
log() { printf 'LOG:%s\n' "$*"; }
fail() { printf 'FAIL_CALLED:%s\n' "$*"; return 0; }
STUB

  # 隔離プローブ: git.sh だけを source し、resolver rc を外から注入する。
  # APPLY=0 なので計画経路のみ（tar -t は読み取り専用、書き込みゼロ）。
  cat > "$tmp/git-rc-probe.sh" <<'PROBE'
set -uo pipefail
git_mod="$1"
stub_file="$2"
resolved_path="$3"
stub_rc="$4"
# shellcheck disable=SC1090
source "$git_mod"
# shellcheck disable=SC1090
source "$stub_file"
STUB_RESOLVER_RC="$stub_rc"
STUB_RESOLVED_PATH="$resolved_path"
SET_DIR="$(dirname -- "$resolved_path")"
APPLY=0
CONFIRM=0
restore_git
printf 'RETURN:%s\n' "$?"
PROBE

  git_probe() { # stub_rc
    bash "$tmp/git-rc-probe.sh" "$git_mod" "$tmp/git-resolver-stub.sh" \
      "$set_dir/git-jarvis-head.tar.gz" "$1" 2>&1
  }

  # 10a. rc=2 (absent / 未解決) は fail() を呼び non-zero で戻る。
  rc2_out="$(git_probe 2)"
  if has_fragment "$rc2_out" 'FAIL_CALLED:git: cannot resolve a unique git payload'; then
    pass "I78-B: rc=2 calls fail()"
  else
    fail_case "I78-B: rc=2 did not call fail()"
    printf '%s\n' "$rc2_out" | head -8 >&2
  fi
  if has_fragment "$rc2_out" 'rc=2'; then
    pass "I78-B: rc=2 reported in fail message"
  else
    fail_case "I78-B: rc=2 not reported in fail message"
  fi
  if has_fragment "$rc2_out" 'RETURN:1'; then
    pass "I78-B: rc=2 returns non-zero"
  else
    fail_case "I78-B: rc=2 did not return non-zero"
  fi
  if has_fragment "$rc2_out" 'git payload が manifest に無いため git 復元をスキップ'; then
    fail_case "I78-B: rc=2 still warn-succeeded"
  else
    pass "I78-B: rc=2 no longer warn-succeeds"
  fi

  # 10b. rc=1 (不正 role / 複数一致 / 解決先ファイル不在) も同様に拒否。
  rc1_out="$(git_probe 1)"
  if has_fragment "$rc1_out" 'FAIL_CALLED:git: cannot resolve a unique git payload'; then
    pass "I78-B: rc=1 calls fail()"
  else
    fail_case "I78-B: rc=1 did not call fail()"
    printf '%s\n' "$rc1_out" | head -8 >&2
  fi
  if has_fragment "$rc1_out" 'rc=1'; then
    pass "I78-B: rc=1 reported in fail message"
  else
    fail_case "I78-B: rc=1 not reported in fail message"
  fi
  if has_fragment "$rc1_out" 'RETURN:1'; then
    pass "I78-B: rc=1 returns non-zero"
  else
    fail_case "I78-B: rc=1 did not return non-zero"
  fi

  # 10c. ポジティブ対照: rc=0 のみ続行し、fail() を呼ばず計画経路へ進む。
  rc0_out="$(git_probe 0)"
  if has_fragment "$rc0_out" 'RETURN:0' && ! has_fragment "$rc0_out" 'FAIL_CALLED:'; then
    pass "I78-B: rc=0 continues without fail()"
  else
    fail_case "I78-B: rc=0 did not continue"
    printf '%s\n' "$rc0_out" | head -8 >&2
  fi
  if has_fragment "$rc0_out" '計画: git-jarvis-head.tar.gz を読み取り検査'; then
    pass "I78-B: rc=0 reaches plan path"
  else
    fail_case "I78-B: rc=0 did not reach plan path"
  fi
fi

printf 'result: ok=%d fail=%d\n' "$ok" "$bad"
[ "$bad" -eq 0 ]
