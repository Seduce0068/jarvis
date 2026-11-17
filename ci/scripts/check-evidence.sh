#!/usr/bin/env bash
# Verify test specification evidence is complete and hash-matched (P1-9).
# Each evidence/test-run-*.json must declare a spec_hash matching a tracked
# specs/test-*.md file and record zero failed tests. No secret values are read.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"

ok=0; fail=0
shopt -s nullglob
evidence=(evidence/test-run-*.json)
if [ "${#evidence[@]}" -eq 0 ]; then
  echo "FAIL: no evidence files found"
  exit 1
fi

for json in "${evidence[@]}"; do
  info=$(python3 - "$json" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1], encoding='utf-8'))
except Exception as e:
    print("INVALID_JSON"); raise SystemExit
required = ["spec_hash", "timestamp", "environment", "results", "signature"]
missing = [k for k in required if k not in d]
if missing:
    print("MISSING:" + ",".join(missing)); raise SystemExit
h = d["spec_hash"]
if not isinstance(h, str) or not h.startswith("sha256:"):
    print("BAD_HASH_FORMAT"); raise SystemExit
r = d["results"]
if not isinstance(r, dict) or r.get("failed", 1) != 0:
    print("FAILED_TESTS"); raise SystemExit
print("OK:" + h[len("sha256:"):])
PY
  )
  case "$info" in
    OK:*)
      expected="${info#OK:}"
      matched=0
      for spec in specs/test-*.md; do
        actual=$(sha256sum "$spec" | cut -d' ' -f1)
        if [ "$actual" = "$expected" ]; then matched=1; break; fi
      done
      if [ "$matched" -eq 1 ]; then
        ok=$((ok+1))
      else
        echo "FAIL: $json spec_hash does not match any tracked spec"
        fail=$((fail+1))
      fi
      ;;
    *)
      echo "FAIL: $json ($info)"
      fail=$((fail+1))
      ;;
  esac
done

echo "== evidence check: ok=$ok fail=$fail =="
[ "$fail" -eq 0 ]
