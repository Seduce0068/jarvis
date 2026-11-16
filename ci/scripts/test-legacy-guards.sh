#!/usr/bin/env bash
# Regression guard: the legacy OCI/destructive launchers must stay disabled.
# A future edit that re-enables delegation to backup.rs/restore.rs/deploy.rs
# would silently reintroduce data loss or an out-of-policy OCI upload.
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

ok=0; fail=0
check_guard() { # script expected-hint
  local script="$1" hint="$2" out
  out=$(bash "$repo/$script" 2>&1)
  local code=$?
  if [ "$code" -eq 2 ] && printf '%s' "$out" | grep -qi "$hint"; then
    echo "  PASS: $script remains fail-closed (exit 2, hint present)"
    ok=$((ok+1))
  else
    echo "  FAIL: $script code=$code hint='$hint' output=$(printf '%s' "$out" | head -1)"
    fail=$((fail+1))
  fi
}

check_guard infra/scripts/backup.sh 'backup-preflight'
check_guard infra/scripts/restore.sh 'backup-redesign-notes'
check_guard infra/scripts/deploy.sh 'deploy-mcp-gw-safe'

# The new local backup must stay non-destructive by default: a dry run must
# not create the destination directory at all.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if bash "$repo/infra/scripts/backup-local.sh" --backup-dir "$tmp/dest" >/dev/null 2>&1 \
   && [ ! -e "$tmp/dest" ]; then
  echo "  PASS: backup-local.sh dry run writes nothing"
  ok=$((ok+1))
else
  echo "  FAIL: backup-local.sh dry run created files or failed"
  fail=$((fail+1))
fi

# Help output must work for all new scripts (cheap smoke test for syntax).
for script in backup-local.sh verify-backup.sh install-backup-timer.sh; do
  if bash "$repo/infra/scripts/$script" --help >/dev/null 2>&1; then
    echo "  PASS: $script --help"
    ok=$((ok+1))
  else
    echo "  FAIL: $script --help"
    fail=$((fail+1))
  fi
done

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
