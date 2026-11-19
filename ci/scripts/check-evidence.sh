#!/usr/bin/env bash
# Verify test specification evidence is complete, hash-matched and traceable
# to its declared spec (P1-9). Every evidence/test-run-*.json must declare a
# spec_hash matching the sha256 of its test_basis_ref.spec_path, record zero
# failed tests, and every tracked specs/test-*.md must have evidence.
# Also verify specs/index.md and evidence/index.md link every artifact and
# contain no broken local links (cross-review arch #3). Reference-style link
# definitions such as "[run-ui]: test-run-*.json" are parsed as well.
# Every spec must also contain the four required test-spec sections, the
# seven-column Traceability header, the six 29119-3 information items in
# Document map, and a tailored conformance statement (test-spec skill).
# Evidence timestamps must be RFC 3339, signatures self-attested, and
# document_type (when present) one of the declared kinds. `notes` must be a str
# and every results count a strict, non-negative int; a wrong type fails closed
# instead of silently skipping a check. Every notes count must agree with
# results.passed/failed/skipped, so a multi-gate claim cannot drift away from
# the single aggregated total. The evidence requirements, the spec's REQ
# definitions and the Traceability Req column must be the same set, so adding
# or removing a requirement or citing an undefined Req fails closed. Every
# evidence declaration for a spec is checked, so a second evidence file cannot
# hide behind a first one with the correct requirements. The executed total
# (passed + failed only; skipped does not count) must cover every
# `Scenario:` declared by the referenced spec, so a near-total skip or a run
# that executes only part of a specification cannot pass. Index entry counts
# ("N passed / N failed / N skipped") are resolved through reference-style link
# definitions and must agree with the referenced JSON results, so tampering a
# headline count cannot hide a failure. Values are withheld from error output;
# no secret values are read.
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
from datetime import datetime
from urllib.parse import unquote

evidence_files = sys.argv[1:]
errors = []
covered = set()
# Keep one entry per evidence file. A dict keyed by spec would let a later
# evidence overwrite an earlier one, hiding a tampered requirements array.
evi_reqs_entries = []  # (evidence_file, spec_path, requirements)
ok = 0

# A timezone is mandatory: naive ISO strings are not RFC 3339. Fractional
# seconds and either "Z"/"z" or a numeric offset are accepted.
RFC3339 = re.compile(
    r'^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})$')
DOCUMENT_TYPES = ("test-execution-log", "test-result", "test-summary-report")

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
    # Reject value-less errors only: the offending values are never printed.
    ts = d["timestamp"]
    if not isinstance(ts, str) or not RFC3339.match(ts):
        errors.append(f"{f}: BAD_TIMESTAMP"); continue
    # The regex checks the shape only; parsing rejects impossible dates such
    # as 9999-99-99T99:99:99Z. "Z" is normalised for older Python versions.
    iso_ts = ts[:-1] + "+00:00" if ts.endswith(("Z", "z")) else ts
    try:
        datetime.fromisoformat(iso_ts)
    except ValueError:
        errors.append(f"{f}: BAD_TIMESTAMP"); continue
    if d["signature"] != "self-attested":
        errors.append(f"{f}: BAD_SIGNATURE"); continue
    doc_type = d.get("document_type")
    if doc_type is not None and doc_type not in DOCUMENT_TYPES:
        errors.append(f"{f}: BAD_DOCUMENT_TYPE"); continue
    top = d["spec_hash"]
    if not isinstance(top, str) or not top.startswith("sha256:"):
        errors.append(f"{f}: BAD_HASH_FORMAT"); continue
    # The filename suffix must match the declared hash prefix; this keeps
    # evidence files traceable even though the value itself is never printed.
    prefix = os.path.basename(f).rsplit("-", 1)[-1].removesuffix(".json")
    if prefix and prefix != top[len("sha256:"):][:12]:
        errors.append(f"{f}: HASH_FILENAME_MISMATCH"); continue
    r = d["results"]
    # bool is a subclass of int in Python: False == 0 and True == 1 would
    # otherwise bypass these checks. Require the exact int type.
    if not isinstance(r, dict) or type(r.get("failed", 1)) is not int or r.get("failed", 1) != 0:
        errors.append(f"{f}: FAILED_TESTS"); continue
    if type(r.get("passed", 0)) is not int or r.get("passed", 0) <= 0:
        errors.append(f"{f}: NO_PASSING_TESTS"); continue
    # Type fail-closed gate. `failed` and `passed` were already narrowed to
    # strict ints above, so their legacy error codes are preserved. This closes
    # the remaining gap where a str/dict/float/negative count (most importantly
    # `skipped`) silently bypassed the skipped, notes and scenario checks below.
    # bool is an int subclass, so require the exact int type. Values never printed.
    if any(type(r.get(k)) is not int or r[k] < 0
           for k in ("passed", "failed", "skipped")):
        errors.append(f"{f}: BAD_RESULTS_TYPE"); continue
    # `notes` must be a str whenever present: a dict/list previously turned the
    # skipped-justification and notes-consistency checks into no-ops. Fail closed
    # with a value-less code; an absent notes still defaults to "".
    notes = d.get("notes", "")
    if not isinstance(notes, str):
        errors.append(f"{f}: BAD_NOTES_TYPE"); continue
    # A near-total skip must not satisfy the gate. A positive skipped count is
    # accepted only when the evidence notes explicitly document it as
    # `ignored` (e.g. Docker-only #[ignore] tests): silently rewriting the
    # number to 0 would falsify the record. bool True == 1 is excluded so it
    # cannot bypass. Only the error code is emitted; values are never printed.
    skipped = r.get("skipped", 0)
    if skipped is True or (type(skipped) is int and skipped > 0):
        notes = d.get("notes", "")
        if not isinstance(notes, str) or "ignored" not in notes.lower():
            errors.append(f"{f}: SKIPPED_TESTS"); continue
    # Notes/results consistency. EVERY explicit "N passed", "N failed" or
    # "N ignored|skipped" phrase, plus EVERY headline count ("N 件", "N checks",
    # "N テスト"), must agree with results. finditer enumerates all matches: a
    # second, contradictory count must not hide behind the first one (e.g.
    # "23 checks ... 22 checks ... 23 passed"). Notes are optional; only
    # value-less error codes are emitted, so no count is ever printed.
    notes = d.get("notes", "")
    if isinstance(notes, str):
        for label, key, fallback in (
            ("passed", "passed", None),
            ("failed", "failed", None),
            ("ignored", "skipped", 0),
            ("skipped", "skipped", 0),
        ):
            expected = r.get(key, fallback)
            mismatch = False
            for m in re.finditer(r'(\d+)\s*' + label, notes, re.IGNORECASE):
                if type(expected) is not int or int(m.group(1)) != expected:
                    mismatch = True
                    break
            if mismatch:
                errors.append(f"{f}: NOTES_{label.upper()}_MISMATCH")
                break
        else:
            expected = r.get("passed")
            mismatch = False
            for m in re.finditer(r'(\d+)\s*(?:件|checks?|テスト)', notes):
                if type(expected) is not int or int(m.group(1)) != expected:
                    mismatch = True
                    break
            if mismatch:
                errors.append(f"{f}: NOTES_COUNT_MISMATCH")
    ref = d.get("test_basis_ref")
    if not isinstance(ref, dict) or not ref.get("spec_path"):
        errors.append(f"{f}: NO_SPEC_PATH"); continue
    spec_path = ref["spec_path"]
    # Remember the declared requirements of EVERY evidence. The set comparison
    # happens in the spec loop below, independent of the hash/result checks
    # above, so a tampered spec still reaches the REQ gate instead of
    # short-circuiting. A list (not a dict) keeps two evidence files that cite
    # the same spec from overwriting each other's requirements.
    evi_reqs_entries.append((f, os.path.normpath(spec_path),
                             ref.get("requirements")))
    if not os.path.isfile(spec_path):
        errors.append(f"{f}: SPEC_PATH_NOT_FOUND:{spec_path}"); continue
    actual = digest(spec_path)
    if actual != top:
        errors.append(f"{f}: SPEC_HASH_MISMATCH:{spec_path}"); continue
    nested = ref.get("spec_hash")
    if nested is not None and nested != top:
        errors.append(f"{f}: NESTED_HASH_MISMATCH"); continue
    # A spec declares the complete scenario set. One scenario may expand to
    # several test functions, so the executed total must cover at least every
    # declared `Scenario:` line. Only passed + failed count as executed:
    # skipped tests did not run, so a near-total skip (even one justified with
    # "ignored" in notes) cannot satisfy this gate. This matches the
    # SKIPPED_TESTS rationale above. A spec with no declared scenarios is
    # unverifiable and fails as well. bool values are excluded from the sum
    # so True cannot be counted as 1. Only the error code is emitted; the
    # counts themselves are never printed.
    try:
        with open(spec_path, encoding="utf-8") as sf:
            spec_text = sf.read()
    except OSError:
        errors.append(f"{f}: SPEC_UNREADABLE"); continue
    scenario_count = len(re.findall(r'^[ \t]*Scenario[ \t]*:', spec_text, re.MULTILINE))
    executed = sum(value for value in (r.get(k, 0) for k in ("passed", "failed"))
                   if type(value) is int)
    if scenario_count <= 0 or executed < scenario_count:
        errors.append(f"{f}: SCENARIO_COUNT_MISMATCH"); continue
    covered.add(os.path.normpath(spec_path))
    ok += 1

spec_files = sorted(glob.glob("specs/test-*.md"))
for spec in spec_files:
    if os.path.normpath(spec) not in covered:
        errors.append(f"MISSING_EVIDENCE:{spec}")

# Required test-spec structure (test-spec skill). Headings may use any level,
# but the exact section names and the seven Traceability columns are fixed.
REQUIRED_SECTIONS = ("Test basis", "Risk & tier", "Design technique", "Document map")
TRACEABILITY_HEADER = re.compile(
    r'^\|\s*Scenario\s*\|\s*Req\s*\|\s*ASVS\s*\|\s*800-53\s*\|\s*CC\s*\|'
    r'\s*Technique\s*\|\s*Tier\s*\|', re.MULTILINE)

def defined_reqs(text):
    """REQ ids defined in the requirements table (table's first cell)."""
    return set(re.findall(r'^\|[ \t]*(REQ-\d+)[ \t]*\|', text, re.MULTILINE))


def traceability_reqs(text):
    """REQ ids cited in the Req column of the seven-column Traceability table."""
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if not TRACEABILITY_HEADER.match(line):
            continue
        reqs = set()
        for row in lines[i + 1:]:
            if not row.lstrip().startswith('|'):
                break
            cells = [c.strip() for c in row.strip().strip('|').split('|')]
            if len(cells) != 7:
                continue
            if re.fullmatch(r'REQ-\d+', cells[1]):
                reqs.add(cells[1])
        return reqs
    return set()

# Document map must declare the six 29119-3:2021 information items and state
# tailored conformance (with no full-conformance claim).
INFO_ITEMS = (
    "test design specification",
    "test case specification",
    "test procedure specification",
    "test environment requirements",
    "test result / execution log",
    "test completion report",
)

def section_body(text, name):
    match = re.search(r'^#{1,6}\s+' + re.escape(name) + r'\s*$', text, re.MULTILINE)
    if not match:
        return ""
    rest = text[match.end():]
    following = re.search(r'^#{1,6}\s+', rest, re.MULTILINE)
    return rest[:following.start()] if following else rest

for spec in spec_files:
    try:
        with open(spec, encoding="utf-8") as fh:
            spec_text = fh.read()
    except OSError:
        errors.append(f"SPEC_UNREADABLE:{spec}")
        continue
    for section in REQUIRED_SECTIONS:
        heading = re.compile(r'^#{1,6}\s+' + re.escape(section) + r'\s*$', re.MULTILINE)
        if not heading.search(spec_text):
            errors.append(f"MISSING_SECTION:{spec}:{section}")
    if not TRACEABILITY_HEADER.search(spec_text):
        errors.append(f"MISSING_TRACEABILITY:{spec}")
    # The requirement set of EVERY evidence referencing this spec, the REQ ids
    # defined by the spec and the Req column of the Traceability table must all
    # be the same set. A removed or added requirement, or a Req reference that
    # is not defined by the spec, fails closed. Checking every entry (instead
    # of keeping only the last one in a dict) catches tampering in any of two
    # evidence files that cite the same spec. No REQ value is emitted -- only
    # the value-less code and the file paths involved.
    entries = [entry for entry in evi_reqs_entries if entry[1] == os.path.normpath(spec)]
    def_reqs = defined_reqs(spec_text)
    trc_reqs = traceability_reqs(spec_text)
    if not entries:
        errors.append(f"REQ_SET_MISMATCH:{spec}")
    for evi_file, _spec_path, evi_reqs in entries:
        if not (isinstance(evi_reqs, list)
                and all(isinstance(x, str) for x in evi_reqs)
                and def_reqs and trc_reqs
                and set(evi_reqs) == def_reqs == trc_reqs):
            errors.append(f"REQ_SET_MISMATCH:{spec}:{evi_file}")
    doc_map = section_body(spec_text, "Document map")
    for item in INFO_ITEMS:
        if item not in doc_map:
            errors.append(f"MISSING_INFO_ITEM:{spec}:{item}")
    if not re.search(r'\btailored\b', doc_map, re.IGNORECASE):
        errors.append(f"MISSING_CONFORMANCE:{spec}:tailored")
    if not re.search(r'full conformance\s+は主張しない', doc_map):
        errors.append(f"MISSING_CONFORMANCE:{spec}:no-full-claim")

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

# Index result claims (review R4-A). Each evidence index entry cites a JSON
# through a reference-style link ("[text][label]" plus a "[label]: file.json"
# definition) and states "N passed / N failed / N skipped". Resolve the label
# through REF_DEF, load that JSON and compare every stated field against its
# results. Only fields actually printed in the entry are compared (many entries
# omit skipped). Values are withheld: the error is value-less.
ENTRY_START = re.compile(r'^[ \t]*-[ \t]+\[', re.MULTILINE)
REF_USAGE = re.compile(r'\]\[([^\]]+)\]')

def index_result_entries(index_path):
    with open(index_path, encoding="utf-8") as fh:
        text = fh.read()
    ref_defs = {m.group(1): m.group(2) for m in REF_DEF.finditer(text)}
    starts = [m.start() for m in ENTRY_START.finditer(text)]
    base = os.path.dirname(index_path)
    for i, start in enumerate(starts):
        end = starts[i + 1] if i + 1 < len(starts) else len(text)
        block = text[start:end]
        usage = REF_USAGE.search(block)
        if not usage:
            continue
        target = ref_defs.get(usage.group(1))
        if not target:
            continue
        json_path = os.path.normpath(os.path.join(base, unquote(target.strip())))
        found = {}
        for key in ("passed", "failed", "skipped"):
            m = re.search(r'(\d+)[ \t]+' + key + r'\b', block, re.IGNORECASE)
            if m:
                found[key] = int(m.group(1))
        yield json_path, found

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
    # Every stated count must match the referenced evidence JSON. A tampered
    # index that lowers passed to 0 while the JSON still records a positive
    # count must fail closed. A missing/unreadable target is already reported
    # above as a broken link, so only well-formed JSON is compared here.
    for json_path, found in index_result_entries(index_path):
        if not found or not os.path.isfile(json_path):
            continue
        try:
            with open(json_path, encoding="utf-8") as fh:
                res = json.load(fh).get("results", {})
        except Exception:
            continue
        if not isinstance(res, dict):
            continue
        for key, claimed in found.items():
            actual = res.get(key)
            if type(actual) is not int or actual != claimed:
                errors.append(f"INDEX_RESULT_MISMATCH:{index_path}")
                break

print(f"ok={ok} errors={len(errors)}")
for e in errors:
    print("ERROR:" + e)
PY
)

echo "$result" | head -1
echo "$result" | grep '^ERROR:' | head -20 || true
echo "$result" | grep -q '^ok=[0-9]* errors=0$'
