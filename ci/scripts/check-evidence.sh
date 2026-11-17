#!/usr/bin/env bash
# Verify test specification evidence is complete, hash-matched and traceable
# to its declared spec (P1-9). Every evidence/test-run-*.json must declare a
# spec_hash matching the sha256 of its test_basis_ref.spec_path, record zero
# failed tests, and every tracked specs/test-*.md must have evidence.
# Also verify specs/index.md and evidence/index.md link every artifact and
# contain no broken local links (cross-review arch #3). Reference-style link
# definitions such as "[run-ui]: test-run-*.json" are parsed as well.
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
import glob, hashlib, json, os, re, sys
from urllib.parse import unquote

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

spec_files = sorted(glob.glob("specs/test-*.md"))
for spec in spec_files:
    if os.path.normpath(spec) not in covered:
        errors.append(f"MISSING_EVIDENCE:{spec}")

# Index validation (cross-review arch #3). Both inline links
# "[text](target)" and reference-style definitions "[label]: target" are
# collected; external URLs, protocol-relative paths and pure anchors are
# ignored. Every remaining target must exist relative to the index directory.
INLINE_LINK = re.compile(r'\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)')
REF_DEF = re.compile(r'^\s*\[([^\]]+)\]:\s*<?([^\s>]+)>?', re.MULTILINE)
NON_LOCAL = ('http://', 'https://', 'ftp://', 'mailto:', 'data:', '//', '#')

def index_targets(index_path):
    with open(index_path, encoding="utf-8") as fh:
        text = fh.read()
    raw = [m.group(1) for m in INLINE_LINK.finditer(text)]
    raw += [m.group(2) for m in REF_DEF.finditer(text)]
    base = os.path.dirname(index_path)
    for item in raw:
        target = unquote(item.strip()).split('#', 1)[0]
        if not target or target.startswith(NON_LOCAL):
            continue
        yield os.path.normpath(os.path.join(base, target))

for index_path, artifacts, kind in (
    ("specs/index.md", spec_files, "SPEC"),
    ("evidence/index.md", evidence_files, "EVIDENCE"),
):
    if not os.path.isfile(index_path):
        errors.append(f"INDEX_NOT_FOUND:{index_path}")
        continue
    linked = set(index_targets(index_path))
    for artifact in sorted(artifacts):
        if os.path.normpath(artifact) not in linked:
            errors.append(f"{kind}_NOT_IN_INDEX:{artifact}")
    for target in sorted(linked):
        if not os.path.exists(target):
            errors.append(f"INDEX_BROKEN_LINK:{index_path}:{target}")

print(f"ok={ok} errors={len(errors)}")
for e in errors:
    print("ERROR:" + e)
PY
)

echo "$result" | head -1
echo "$result" | grep '^ERROR:' | head -20 || true
echo "$result" | grep -q '^ok=[0-9]* errors=0$'
