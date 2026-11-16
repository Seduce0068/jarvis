#!/usr/bin/env bash
# Contract tests for review-run.sh using a stub runner (no network/credentials).
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
adapter="$repo/ci/scripts/review-run.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

cat >"$tmp/stub-runner" <<'STUB'
#!/usr/bin/env bash
printf 'RUNNER=%s\n' "$(basename "$0")"
printf 'ARGS=%s\n' "$*"
STUB
chmod +x "$tmp/stub-runner"

ok=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "  PASS: $1"; ok=$((ok+1)); else echo "  FAIL: $1 expected=[$2] actual=[$3]"; fail=$((fail+1)); fi; }

# 1) default backend + prompt forwarding
out=$(JARVIS_REVIEW_RUNNER=stub-runner PATH="$tmp:$PATH" bash "$adapter" "/review-flow gate-check 42")
check "default runner args" "ARGS=run /review-flow gate-check 42" "$(echo "$out" | grep '^ARGS=')"

# 2) --json flag forwarding
out=$(JARVIS_REVIEW_RUNNER=stub-runner PATH="$tmp:$PATH" bash "$adapter" "/cross-review" --json)
check "json flag" "ARGS=run /cross-review --json" "$(echo "$out" | grep '^ARGS=')"

# 3) missing runner fails closed
if JARVIS_REVIEW_RUNNER=does-not-exist bash "$adapter" "x" >/dev/null 2>&1; then
  echo "  FAIL: missing runner should fail"; fail=$((fail+1))
else
  echo "  PASS: missing runner fails"; ok=$((ok+1))
fi

# 4) unknown flag rejected
if JARVIS_REVIEW_RUNNER=stub-runner PATH="$tmp:$PATH" bash "$adapter" "x" --bogus >/dev/null 2>&1; then
  echo "  FAIL: unknown flag should be rejected"; fail=$((fail+1))
else
  echo "  PASS: unknown flag rejected"; ok=$((ok+1))
fi

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
