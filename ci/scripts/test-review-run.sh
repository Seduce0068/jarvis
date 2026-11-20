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

# 0) true default backend: with JARVIS_REVIEW_RUNNER unset, review-run.sh:23
# resolves to `opencode`. A stub named `opencode` on PATH exercises the
# `${JARVIS_REVIEW_RUNNER:-opencode}` default branch (`run <prompt>`) without
# a real agent runtime; PATH="$tmp:$PATH" makes the stub win.
cp "$tmp/stub-runner" "$tmp/opencode"
out=$(env -u JARVIS_REVIEW_RUNNER PATH="$tmp:$PATH" bash "$adapter" "/review-flow gate-check 42")
check "default runner is opencode" "RUNNER=opencode" "$(echo "$out" | grep '^RUNNER=')"
check "default runner args" "ARGS=run /review-flow gate-check 42" "$(echo "$out" | grep '^ARGS=')"

# 1) explicit generic runner (non-dsh branch) + prompt forwarding.
# The genuine default branch is covered by test 0 above.
out=$(JARVIS_REVIEW_RUNNER=stub-runner PATH="$tmp:$PATH" bash "$adapter" "/review-flow gate-check 42")
check "explicit generic runner args" "ARGS=run /review-flow gate-check 42" "$(echo "$out" | grep '^ARGS=')"

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

# 5) dsh backend: the non-default branch of build_runner_args was untested.
# A stub named `dsh` on PATH exercises it without a real agent runtime, and
# PATH="$tmp:$PATH" guarantees the stub wins over any installed `dsh` binary.
cp "$tmp/stub-runner" "$tmp/dsh"
out=$(JARVIS_REVIEW_RUNNER=dsh PATH="$tmp:$PATH" bash "$adapter" "/review-flow gate-check 42")
check "dsh runner args" "ARGS=--profile headless /review-flow gate-check 42" "$(echo "$out" | grep '^ARGS=')"

# 6) dsh backend: --json is placed before the task and the profile is overridable.
out=$(JARVIS_REVIEW_RUNNER=dsh JARVIS_REVIEW_RUNNER_PROFILE=custom PATH="$tmp:$PATH" bash "$adapter" "/cross-review" --json)
check "dsh runner json/profile args" "ARGS=--profile custom --json /cross-review" "$(echo "$out" | grep '^ARGS=')"

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
