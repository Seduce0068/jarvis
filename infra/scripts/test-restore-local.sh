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

cat > "$tmp/bin/psql" <<'MOCK_PSQL'
#!/usr/bin/env bash
printf 'psql %s\n' "$*" >> "${MOCK_LOG:?}/psql.log"
sql=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-tAc" ]; then
    sql="$arg"
  fi
  prev="$arg"
done
case "$sql" in
  *"FROM pg_database"*) : ;;                  # 隔離 DB は存在しないと答える
  *information_schema.tables*) printf '2\n' ;;
esac
# ダンプはパイプで渡される。pipefail 下で gzip が SIGPIPE にならないよう
# 入力を最後まで読み捨てる（内容は保持も出力もしない）。
cat >/dev/null
exit 0
MOCK_PSQL

cat > "$tmp/bin/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
printf 'curl %s\n' "$*" >> "${MOCK_LOG:?}/curl.log"
want_body=1
for arg in "$@"; do
  if [ "$arg" = "/dev/null" ]; then
    want_body=0
  fi
done
if [ "$want_body" -eq 1 ]; then
  # コレクション情報取得（points_count 検証用）。値はダミー固定。
  printf '%s\n%s\n' '{"result":{"points_count":3}}' '200'
else
  printf '%s\n' '200'
fi
exit 0
MOCK_CURL

cat > "$tmp/bin/jq" <<'MOCK_JQ'
#!/usr/bin/env bash
printf 'jq %s\n' "$*" >> "${MOCK_LOG:?}/jq.log"
cat >/dev/null
printf '3\n'
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
  rm -rf -- "$tmp/logs/state"
  env \
    PATH="$tmp/bin:$PATH" \
    MOCK_LOG="$tmp/logs" \
    MOCK_DISABLED_UNITS="${MOCK_DISABLED_UNITS:-}" \
    MOCK_INACTIVE_UNITS="${MOCK_INACTIVE_UNITS:-}" \
    MOCK_STOP_INEFFECTIVE_UNITS="${MOCK_STOP_INEFFECTIVE_UNITS:-}" \
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

printf 'result: ok=%d fail=%d\n' "$ok" "$bad"
[ "$bad" -eq 0 ]
