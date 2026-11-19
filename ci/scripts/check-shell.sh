#!/usr/bin/env bash
# Shell static analysis for all tracked *.sh files (P2-5).
# bash -n and shellcheck are both mandatory gates: a missing shellcheck fails
# closed instead of silently skipping static analysis on CI runners.
# shfmt formatting is reported as a warning only, not a gate: the repository
# has pre-existing formatting drift and auto-formatting would be out of scope.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo" || exit 1

# Resolve the target list once so a failing `git ls-files` or an empty target
# set fails the gate (fail-closed) instead of silently passing.
if ! script_list="$(git ls-files '*.sh')"; then
  echo "FAIL: git ls-files '*.sh' failed" >&2
  exit 1
fi
if [ -z "$script_list" ]; then
  echo "FAIL: no tracked *.sh files to check" >&2
  exit 1
fi

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
done <<< "$script_list"

if ! command -v shellcheck >/dev/null 2>&1; then
  echo "FAIL: shellcheck is not installed (required gate; install it in the CI runner / environment)" >&2
  fail=$((fail+1))
else
  mapfile -t scripts <<< "$script_list"
  if shellcheck "${scripts[@]}" >/tmp/check-shell-sc.out 2>&1; then
    echo "shellcheck: OK"
  else
    echo "FAIL: shellcheck found issues"
    head -20 /tmp/check-shell-sc.out >&2
    fail=$((fail+1))
  fi
fi

if command -v shfmt >/dev/null 2>&1; then
  count=0
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    [ -n "$(shfmt -d "$f" 2>/dev/null)" ] && count=$((count+1))
  done <<< "$script_list"
  if [ "$count" -gt 0 ]; then
    echo "shfmt: WARN ($count scripts differ; formatting not enforced)"
  else
    echo "shfmt: OK"
  fi
fi

echo "== shell check: ok=$ok fail=$fail =="
[ "$fail" -eq 0 ]
