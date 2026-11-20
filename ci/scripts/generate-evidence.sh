#!/usr/bin/env bash
# I69-D (a): derive an evidence `results` object mechanically from
# `cargo test -- --list` instead of hand-typing results.passed.
#
# READ-ONLY by design: this script never writes evidence/*.json, specs/*.md or
# either index. It prints a results fragment (and a matching notes snippet) to
# stdout so `check-evidence.sh` and the six existing evidence files stay
# untouched. The operator applies the fragment during the documented
# regeneration runbook, keeping notes/index counts in sync.
#
# Only aggregate counts are emitted. No test name, environment value, secret or
# host-specific path is read or printed. Failures emit a value-less code only.
#
# I71-D: `failed: 0` in the fragment is a --list-derived PREDICTION, not a
# measurement: `--list` only enumerates tests and never executes them, so it
# cannot observe a failing test. The emitted lines label this explicitly, and
# `--check` now runs the real suite and fails closed unless it exits 0 before
# trusting that value. check-evidence.sh's `failed == 0` gate and the six
# existing evidence files therefore stay intact.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

usage() {
  cat >&2 <<'EOF'
Usage: ci/scripts/generate-evidence.sh <target> [--check evidence/test-run-*.json]
  target: policy | mcp | context | event | ui | core
  --check also RUNS the real suite (not --list) and requires it to pass,
          then compares the derived counts with an existing evidence JSON;
          prints only MATCH / MISMATCH (no values).
EOF
}

target="${1:-}"
case "$target" in
  policy)  workdir=rust; cargo_args=(test -p jarvis-policy --lib) ;;
  mcp)     workdir=rust; cargo_args=(test -p jarvis-mcp-gw --lib) ;;
  context) workdir=rust; cargo_args=(test -p jarvis-context --lib) ;;
  event)   workdir=rust; cargo_args=(test -p jarvis-event --lib) ;;
  core)    workdir=rust; cargo_args=(test -p jarvis-core --features http --lib) ;;
  ui)      workdir=ui;   cargo_args=(test) ;;
  *) usage; exit 2 ;;
esac

check_file=""
if [ "$#" -ge 2 ]; then
  if [ "$2" != "--check" ] || [ "$#" -ne 3 ]; then
    usage
    exit 2
  fi
  check_file="$3"
  # Constrain reads to the repository's evidence tree: no absolute path, ".."
  # escape or unrelated file may be opened, so no secret can be read here.
  case "$check_file" in
    evidence/test-run-*.json) ;;
    *) echo "FAIL: BadEvidencePath" >&2; exit 2 ;;
  esac
  if [ ! -f "$repo/$check_file" ]; then
    echo "FAIL: EvidenceFileNotFound" >&2
    exit 1
  fi
fi

tmpdir="$(mktemp -d)" || { echo "FAIL: TempDirFailed" >&2; exit 1; }
trap 'rm -rf "$tmpdir"' EXIT

# `--list` enumerates tests without running them; `--list --ignored` enumerates
# only the ignored ones. Compilation output is captured and discarded so a
# failing build yields a value-less code instead of a source/value dump.
if ! all_output="$( (cd "$repo/$workdir" && cargo "${cargo_args[@]}" -- --list) 2>"$tmpdir/all.err" )"; then
  echo "FAIL: CargoListFailed" >&2
  exit 1
fi
if ! ignored_output="$( (cd "$repo/$workdir" && cargo "${cargo_args[@]}" -- --list --ignored) 2>"$tmpdir/ignored.err" )"; then
  echo "FAIL: CargoIgnoredListFailed" >&2
  exit 1
fi

# libtest ends each binary's list with "N tests, M benchmarks". Sum across
# binaries (the `ui` target has several) and default to 0 when the shape
# changes, which is then rejected by the range check below.
sum_tests() {
  awk '/^[ \t]*[0-9]+ tests?, [0-9]+ benchmarks?/ { total += $1 } END { print total + 0 }'
}
total="$(printf '%s\n' "$all_output" | sum_tests)"
ignored="$(printf '%s\n' "$ignored_output" | sum_tests)"

case "$total" in
  ''|*[!0-9]*) echo "FAIL: BadListSummary" >&2; exit 1 ;;
esac
case "$ignored" in
  ''|*[!0-9]*) echo "FAIL: BadIgnoredSummary" >&2; exit 1 ;;
esac
if [ "$total" -le 0 ] || [ "$ignored" -gt "$total" ]; then
  echo "FAIL: BadTestCount" >&2
  exit 1
fi
passed=$((total - ignored))

# I71-D: label the predicted `failed` value so it can never be mistaken for a
# measured result when this fragment is pasted into evidence.
printf '# LABEL: failed=0 は --list 由来の予測値であり実測ではない。\n'
printf '#        --list はテストを実行しないため failing test を検出できない。\n'
printf '#        適用前に実 suite を実行し失敗 0 を確認すること（--check は実 run も照合）。\n'
printf 'results: {"passed": %d, "failed": 0, "skipped": %d}\n' "$passed" "$ignored"
printf 'notes  : %d passed / 0 failed / %d ignored (cargo --list から機械導出; failed は予測値・実測ではない)\n' "$passed" "$ignored"

if [ -n "$check_file" ]; then
  # I71-D: --list cannot detect a failing test. Fail closed unless the real
  # suite also passes; cargo's exit code is the only signal read/emitted here
  # (no test names, values or log content).
  if ! (cd "$repo/$workdir" && cargo "${cargo_args[@]}") \
      >"$tmpdir/run.out" 2>"$tmpdir/run.err"; then
    echo "FAIL: ActualRunDetectedFailure" >&2
    exit 1
  fi
  # Strict ints only (bool is an int subclass in Python). Mismatch exits 1 via
  # set -e; no field value is printed by this comparison.
  python3 - "$repo/$check_file" "$passed" "$ignored" <<'PY'
import json
import sys

path, passed, ignored = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
with open(path, encoding="utf-8") as handle:
    results = json.load(handle).get("results", {})
match = (
    type(results.get("passed")) is int
    and results.get("passed") == passed
    and type(results.get("failed")) is int
    and results.get("failed") == 0
    and type(results.get("skipped")) is int
    and results.get("skipped") == ignored
)
print("CHECK: MATCH" if match else "CHECK: MISMATCH")
sys.exit(0 if match else 1)
PY
fi
