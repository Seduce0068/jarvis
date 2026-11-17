#!/usr/bin/env bash
# Verify test specification evidence is complete, hash-matched and traceable
# to its declared spec (P1-9). Every evidence/test-run-*.json must declare a
# spec_hash matching the sha256 of its test_basis_ref.spec_path, record zero
# failed tests, and every tracked specs/test-*.md must have evidence.
# No secret values are read.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"

shopt -s nullglob
evidence=(evidence/test-run-*.json)
specs=(specs/test-*.md)
if [ "${#evidence[@]}" -eq 0 ] || [ "${#specs[@]}" -eq 0 ]; then
  echo "FAIL: no evidence or spec files found"
  exit 1
fi

result=$(python3 - "${evidence[@]}" <<'PY'
import hashlib, json, os, sys
evidence_files = sys.argv[1:]
errors = []
covered = set()
ok = 0

def digest(path):
    return "sha256:" + hashlib.sha256(open(path, "rb").read()).hexdigest()

for f in evidence_files:
    try:
        d = json.load(open(f, encoding="utf-8"))
    except Exception as e:
        errors.append(f"{f}: INVALID_JSON"); continue
    required = ["spec_hash", "timestamp", "environment", "results", "signature"]
    missing = [k for k in required if k not in d]
    if missing:
        errors.append(f"{f}: MISSING:{','.join(missing)}"); continue
    top = d["spec_hash"]
    if not isinstance(top, str) or not top.startswith("sha256:"):
        errors.append(f"{f}: BAD_HASH_FORMAT"); continue
    r = d["results"]
    if not isinstance(r, dict) or r.get("failed", 1) != 0:
        errors.append(f"{f}: FAILED_TESTS"); continue
    ref = d.get("test_basis_ref")
    if not isinstance(ref, dict) or not ref.get("spec_path"):
        errors.append(f"{f}: NO_SPEC_PATH"); continue
    spec_path = ref["spec_path"]
    if not os.path.isfile(spec_path):
        errors.append(f"{f}: SPEC_PATH_NOT_FOUND:{spec_path}"); continue
    actual = digest(spec_path)
    if actual != top:
        errors.append(f"{f}: SPEC_HASH_MISMATCH:{spec_path}"); continue
    nested = ref.get("spec_hash")
    if nested is not None and nested != top:
        errors.append(f"{f}: NESTED_HASH_MISMATCH"); continue
    covered.add(os.path.normpath(spec_path))
    ok += 1

for spec in sorted(s for s in __import__('glob').glob("specs/test-*.md")):
    if os.path.normpath(spec) not in covered:
        errors.append(f"MISSING_EVIDENCE:{spec}")

print(f"ok={ok} errors={len(errors)}")
for e in errors:
    print("ERROR:" + e)
PY
)

echo "$result" | head -1
echo "$result" | grep '^ERROR:' | head -20 || true
echo "$result" | grep -q '^ok=[0-9]* errors=0$'
