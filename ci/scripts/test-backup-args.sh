#!/usr/bin/env bash
# TC-B5 / REQ-18 regression guard.
# backup-local.sh must reject a missing, non-integer or non-positive
# --retain-days during argument parsing (exit 2), before any backup set is
# created or pruned. Every invocation is a dry run against a throwaway backup
# directory with all data sources skipped: no real backup, PostgreSQL,
# Qdrant, git archive or network request is involved.
# On failure only an error code is emitted: argument values, paths and raw
# script output are deliberately withheld.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo/infra/scripts/backup-local.sh"

tmp="$(mktemp -d)" || { printf 'FAIL: TMP_UNAVAILABLE\n'; exit 1; }
trap 'rm -rf "$tmp"' EXIT

# A synthetic old set that retention would prune if it ever ran. Its name and
# contents are fixed literals; the temporary path is never printed.
backup_dir="$tmp/backups"
old_set="guard-sentinel-20000101-000000"
sentinel="$backup_dir/$old_set/manifest.txt"
mkdir -p "$backup_dir/$old_set"
printf '# created_at=2000-01-01T00:00:00Z\n' > "$sentinel"
printf 'complete 2000-01-01T00:00:00Z\n' > "$backup_dir/$old_set/COMPLETE"

ok=0; fail=0

check_case() { # case_id expected_exit exit_err_code args...
  local case_id="$1" expected="$2" exit_err="$3"; shift 3
  local code=0 sets err=''

  if bash "$script" --backup-dir "$backup_dir" --skip-db --skip-qdrant --skip-git "$@" \
       >/dev/null 2>&1; then
    code=0
  else
    code=$?
  fi
  sets="$(find "$backup_dir" -mindepth 1 -maxdepth 1 -type d | wc -l)"

  if [ "$code" -ne "$expected" ]; then
    err="$exit_err"
  elif [ ! -f "$sentinel" ] || [ ! -f "$backup_dir/$old_set/COMPLETE" ]; then
    err='EXISTING_SET_LOST'
  elif [ "$sets" -ne 1 ]; then
    err='UNEXPECTED_SET_COUNT'
  fi

  if [ -n "$err" ]; then
    # Error code only: no value, no path, no captured script output.
    printf 'FAIL: %s\n' "$err"
    fail=$((fail+1))
  else
    printf 'PASS: %s\n' "$case_id"
    ok=$((ok+1))
  fi
}

# Missing value, empty string and non-integer text must be rejected.
check_case 'TC-B5-missing' 2 EXIT_NOT_2 --retain-days
check_case 'TC-B5-empty' 2 EXIT_NOT_2 --retain-days ''
check_case 'TC-B5-noninteger' 2 EXIT_NOT_2 --retain-days abc
# Zero and negative values are not greater than zero.
check_case 'TC-B5-zero' 2 EXIT_NOT_2 --retain-days 0
check_case 'TC-B5-negative' 2 EXIT_NOT_2 --retain-days -1
# Control: a valid opt-in value must still be accepted in dry-run mode, so a
# blanket rejection of --retain-days cannot satisfy the negative cases above.
check_case 'TC-B5-valid' 0 EXIT_NOT_0 --retain-days 7

printf 'result: ok=%d fail=%d\n' "$ok" "$fail"
[ "$fail" -eq 0 ]
