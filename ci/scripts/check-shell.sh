#!/usr/bin/env bash
# Shell static analysis for all tracked *.sh files (P2-5).
# bash -n is mandatory; shellcheck runs when available (CI runners may lack it).
# shfmt formatting is reported as a warning only, not a gate: the repository
# has pre-existing formatting drift and auto-formatting would be out of scope.
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo" || exit 1

ok=0; fail=0
while IFS= read -r f; do
  [ -n "$f" ] || continue
  if ! bash -n "$f" 2>/tmp/check-shell.err; then
    echo "FAIL bash -n: $f"
    head -3 /tmp/check-shell.err >&2
    fail=$((fail+1))
  else
    ok=$((ok+1))
  fi
done < <(git ls-files '*.sh')

if command -v shellcheck >/dev/null 2>&1; then
  mapfile -t scripts < <(git ls-files '*.sh')
  if shellcheck "${scripts[@]}" >/tmp/check-shell-sc.out 2>&1; then
    echo "shellcheck: OK"
  else
    echo "FAIL: shellcheck found issues"
    head -20 /tmp/check-shell-sc.out >&2
    fail=$((fail+1))
  fi
else
  echo "shellcheck: SKIP (not installed)"
fi

if command -v shfmt >/dev/null 2>&1; then
  count=0
  while IFS= read -r f; do
    [ -n "$(shfmt -d "$f" 2>/dev/null)" ] && count=$((count+1))
  done < <(git ls-files '*.sh')
  if [ "$count" -gt 0 ]; then
    echo "shfmt: WARN ($count scripts differ; formatting not enforced)"
  else
    echo "shfmt: OK"
  fi
fi

echo "== shell check: ok=$ok fail=$fail =="
[ "$fail" -eq 0 ]
