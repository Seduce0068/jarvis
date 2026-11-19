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
# that executes only part of a specification cannot pass. A positive skipped
# count needs an explicit "N ignored" in notes whose number equals
# results.skipped; the bare substring "ignored" is not sufficient. Index entry
# counts ("N passed / N failed / N skipped") are resolved through both
# reference-style definitions and inline links, must state at least passed and
# failed, and must agree with the referenced JSON results. Every count match and
# every reference link in an entry is checked, so a second contradictory count
# or a second link to a different JSON cannot hide a failure. Contract steps
# added to an EXISTING Scenario (e.g. the Round 61 BareJson 415/413/400/no-leak
# steps) do not change the Scenario count, so an unchanged executed total could
# still be reported as sufficient. Pinned contract keywords are therefore
# counted per spec in strict mode: EVERY keyword found in a spec's contract
# steps must appear in the matching evidence notes, otherwise
# SCENARIO_STEP_UNCOVERED fails closed. No baseline/grandfather exception
# remains. No keyword value is emitted; no secret values are read.
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
from collections import Counter
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

# I65-C contract-step coverage (strict). Round 61 appended the BareJson
# 415/413/400/no-leak contract group to four EXISTING Scenarios, so the
# executed >= scenario_count gate could not notice a spec-only edit. A
# "contract step" is a gherkin step line (or the I61-A marker comment inside a
# gherkin block) that carries one of these pinned keywords. The four evidence
# notes have since been synchronised with a real test run, so EVERY detected
# keyword must be present in the matching notes; a missing or duplicated
# occurrence fails closed with SCENARIO_STEP_UNCOVERED. No baseline exception.
CONTRACT_KEYWORDS = {
    "BareJson": r'\bBareJson\b',
    "415": r'\b415\b',
    "413": r'\b413\b',
    "400": r'\b400\b',
    "no-leak": r'rejection\s*文と入力本文バイトを[^\n]*出さない|no-?leak|非漏洩',
    "invalid_input": r'error_kind\s*=\s*invalid_input',
    "field=body": r'field\s*=\s*body',
}
# Notes are prose: accept the same keyword or a close human spelling. This only
# decides whether a step is covered; the value itself is never emitted.
CONTRACT_NOTE_PATTERNS = {
    "BareJson": r'\bBareJson\b',
    "415": r'\b415\b',
    "413": r'\b413\b',
    "400": r'\b400\b',
    "no-leak": r'no-?leak|非漏洩|漏洩しな|rejection\s*文',
    "invalid_input": r'invalid[_-]?input',
    "field=body": r'field\s*[=:]\s*body',
}

def contract_keywords(spec_text):
    """Count pinned contract keywords in steps after a contract marker.

    Scoping to the marker and to its following steps keeps ordinary gherkin
    steps that merely mention a status code (e.g. "Then 400 ...") from being
    mistaken for contract steps. A new contract group in a new gherkin block
    is still detected because the block carries its own marker.
    """
    found = []
    marker = re.compile(
        r'I61-A|BareJson|error_kind\s*=\s*invalid_input|'
        r'field\s*=\s*body|rejection\s*文と入力本文バイト', re.IGNORECASE)
    step = re.compile(r'^[ \t]*(?:Given|When|Then|And|But)\b')
    blocks = []
    current = None
    for line in spec_text.splitlines():
        stripped = line.strip()
        if stripped.startswith('```'):
            if stripped.lower().startswith('```gherkin'):
                current = []
            elif current is not None:
                blocks.append(current)
                current = None
            continue
        if current is not None:
            current.append(line)
    if current is not None:
        blocks.append(current)
    for block in blocks:
        start = next((i for i, line in enumerate(block)
                      if marker.search(line)), None)
        if start is None:
            continue
        for line in block[start:]:
            if not (step.match(line) or marker.search(line)):
                continue
            for name, pattern in CONTRACT_KEYWORDS.items():
                found.extend([name] * len(re.findall(pattern, line, re.IGNORECASE)))
    return Counter(found)

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
    # accepted only when the notes state the SAME number as "N ignored"
    # (e.g. "1 ignored" for Docker-only #[ignore] tests). A bare substring such
    # as "no tests were ignored" no longer justifies a skipped count: it was
    # the old gate's fail-open hole. finditer checks every occurrence, so a
    # second, contradictory count cannot hide behind a matching one. bool
    # True == 1 is excluded above by BAD_RESULTS_TYPE. Only the error code is
    # emitted; values are never printed.
    skipped = r.get("skipped", 0)
    if skipped is True or (type(skipped) is int and skipped > 0):
        if not isinstance(notes, str):
            errors.append(f"{f}: SKIPPED_TESTS"); continue
        justified = any(int(m.group(1)) == skipped for m in
                        re.finditer(r'(\d+)\s*ignored\b', notes, re.IGNORECASE))
        if not justified:
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
    # Read the spec before the hash gate: appending a contract step to a spec
    # drifts its sha256, and SPEC_HASH_MISMATCH would otherwise mask the
    # SCENARIO_STEP_UNCOVERED regression the tamper test isolates.
    try:
        with open(spec_path, encoding="utf-8") as sf:
            spec_text = sf.read()
    except OSError:
        errors.append(f"{f}: SPEC_UNREADABLE"); continue
    # I65-C: a contract step appended to an existing Scenario keeps the
    # Scenario count unchanged, so the executed total alone cannot detect a
    # spec-only edit. In strict mode EVERY detected keyword needs a matching
    # note; the Round 61 baseline/grandfather list has been removed. A Counter
    # (not a set) is used so a duplicated step also counts as uncovered. No
    # keyword is emitted.
    detected_steps = contract_keywords(spec_text)
    uncovered_steps = detected_steps
    if uncovered_steps:
        missing_steps = [name for name in uncovered_steps
                         if not re.search(CONTRACT_NOTE_PATTERNS[name], notes,
                                          re.IGNORECASE)]
        if missing_steps:
            errors.append(f"{f}: SCENARIO_STEP_UNCOVERED")
            continue
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
    scenario_count = len(re.findall(r'^[ \t]*Scenario[ \t]*:', spec_text, re.MULTILINE))
    executed = sum(value for value in (r.get(k, 0) for k in ("passed", "failed"))
                   if type(value) is int)
    if scenario_count <= 0 or executed < scenario_count:
        errors.append(f"{f}: SCENARIO_COUNT_MISMATCH"); continue
    covered.add(os.path.normpath(spec_path))
    ok += 1

spec_files = sorted(glob.glob("specs/test-*.md"))
# Normalised set of every spec the gate actually inspects. An evidence file
# that names a path outside this set never reaches the REQ comparison loop,
# so it must be rejected explicitly below (I64-A (b)).
spec_set = {os.path.normpath(s) for s in spec_files}
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

# I64-A (b): every evidence declaration must point at one of the globbed
# specs/test-*.md files. A path outside that set is never compared against a
# spec's REQ table above, so an evidence file with requirements=[] would pass
# every other gate. Reject it explicitly, and constrain the path to the
# repository's specs/ tree: absolute paths and ".." escapes are never valid.
for evi_file, spec_norm, _evi_reqs in evi_reqs_entries:
    if (os.path.isabs(spec_norm)
            or spec_norm == '..' or spec_norm.startswith('..' + os.sep)
            or spec_norm.split(os.sep)[0] != 'specs'
            or spec_norm not in spec_set):
        errors.append(f"REQ_SPEC_UNRESOLVED:{evi_file}:{spec_norm}")

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

# Index result claims (review R4-A, I60-C). Each evidence index entry cites a
# JSON through a reference-style link ("[text][label]" plus a "[label]:
# file.json" definition) or an inline link ("[text](file.json)") and states
# "N passed / N failed / N skipped". Resolve both forms, load that JSON and
# compare every stated field against its results. An evidence entry must state
# at least passed and failed; a missing claim fails closed with
# INDEX_RESULT_MISSING instead of being silently skipped. Entries in
# specs/index.md point at .md files and carry no counts, so they are not
# evidence entries. Values are withheld: errors are value-less.
ENTRY_START = re.compile(r'^[ \t]*-[ \t]+\[', re.MULTILINE)
REF_USAGE = re.compile(r'\]\[([^\]]+)\]')
# A result-count claim ("N passed") marks a bullet as an evidence entry even
# when its JSON link has been removed. The "関連" heading separates the
# evidence list from the related-links section, whose bullets are not entries.
COUNT_CLAIM = re.compile(r'\d+[ \t]+(?:passed|failed|skipped)\b', re.IGNORECASE)
RELATED_HEADING = re.compile(
    r'^#{1,6}[ \t]+[^\n]*(?:関連|related|参照|notes)[^\n]*$',
    re.MULTILINE | re.IGNORECASE)

def index_result_entries(index_path, kind):
    # specs/index.md bullets describe specs, not test executions; they carry
    # no result counts and are never evidence entries.
    if kind == "SPEC":
        return
    with open(index_path, encoding="utf-8") as fh:
        text = fh.read()
    ref_defs = {m.group(1): m.group(2) for m in REF_DEF.finditer(text)}
    starts = [m.start() for m in ENTRY_START.finditer(text)]
    base = os.path.dirname(index_path)
    related = RELATED_HEADING.search(text)
    main_end = related.start() if related else len(text)
    for i, start in enumerate(starts):
        end = starts[i + 1] if i + 1 < len(starts) else len(text)
        block = text[start:end]
        # A block may hold both forms and several reference links (the
        # continuation line links the spec .md). Resolve EVERY reference link
        # and inline link; a second link to a different JSON must be validated
        # too instead of hiding behind the first one. A reference usage whose
        # label has no REF_DEF contributes an empty candidate and therefore no
        # JSON path, which is reported as INDEX_UNRESOLVED_ENTRY below.
        candidates = [ref_defs.get(usage.group(1), "")
                      for usage in REF_USAGE.finditer(block)]
        candidates.extend(m.group(1) for m in INLINE_LINK.finditer(block))
        json_paths = []
        md_names = []
        for item in candidates:
            raw = unquote(item.strip()).split('#', 1)[0]
            if not raw or raw.startswith(NON_LOCAL):
                continue
            if raw.endswith('.md'):
                md_names.append(os.path.basename(raw))
            if not (raw.endswith('.json') or 'test-run-' in raw):
                continue
            resolved = os.path.normpath(os.path.join(base, raw))
            if resolved.endswith('.json') and resolved not in json_paths:
                json_paths.append(resolved)
        # Classify entries structurally instead of by the survival of a JSON
        # link (I64-A (a)/(d)). Bullets in the main list are evidence entries;
        # the "関連" section's index.md/todo.md links are not, but an evidence
        # marker (count claim, JSON target or test-*.md link) still qualifies
        # one wherever it appears.
        is_evidence = (start < main_end or bool(json_paths)
                       or bool(COUNT_CLAIM.search(block))
                       or any(n.startswith('test-') and n.endswith('.md')
                              for n in md_names))
        if not is_evidence:
            continue
        # Collect EVERY "N passed/failed/skipped" claim in the entry. re.search
        # kept only the first match, so a second, contradictory count was
        # invisible. All claims are checked against the JSON below.
        found = {}
        for key in ("passed", "failed", "skipped"):
            claims = [int(m.group(1)) for m in re.finditer(
                r'(\d+)[ \t]+' + key + r'\b', block, re.IGNORECASE)]
            if claims:
                found[key] = claims
        # None marks an evidence entry whose JSON target cannot be resolved;
        # the validation loop emits INDEX_UNRESOLVED_ENTRY for it.
        for json_path in json_paths or [None]:
            yield json_path, found, md_names

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
    # Every stated count must match every referenced evidence JSON. A tampered
    # index that lowers passed to 0, appends a second contradictory count, or
    # adds a second reference link to a different JSON must fail closed. Missing
    # claims are reported as INDEX_RESULT_MISSING; a broken file link is already
    # reported by the link check above.
    consumed = set()
    for json_path, found, md_names in index_result_entries(index_path, kind):
        # An evidence entry with no resolvable JSON is reported with its own
        # code (I64-A (a)); a resolvable entry still needs its count claims.
        if json_path is None:
            errors.append(f"INDEX_UNRESOLVED_ENTRY:{index_path}")
            continue
        if "passed" not in found or "failed" not in found:
            errors.append(f"INDEX_RESULT_MISSING:{index_path}")
            continue
        if not os.path.isfile(json_path):
            continue  # broken link already reported above
        consumed.add(json_path)
        try:
            with open(json_path, encoding="utf-8") as fh:
                doc = json.load(fh)
        except Exception:
            continue
        res = doc.get("results", {})
        if not isinstance(res, dict):
            continue
        for key, claims in found.items():
            actual = res.get(key)
            # Check every stated count, not just the first one. A second,
            # contradictory count must not hide behind a matching first one
            # (which is exactly what re.search did).
            if type(actual) is not int or any(c != actual for c in claims):
                errors.append(f"INDEX_RESULT_MISMATCH:{index_path}")
                break
        # The .md link inside an entry must name the same spec as the JSON's
        # test_basis_ref.spec_path, so pointing the entry at a different spec
        # cannot pass (I64-A (c)). No spec path value is emitted.
        ref = doc.get("test_basis_ref")
        spec_ref_name = ""
        if isinstance(ref, dict) and isinstance(ref.get("spec_path"), str):
            spec_ref_name = os.path.basename(ref["spec_path"])
        if spec_ref_name and (not md_names
                              or any(name != spec_ref_name for name in md_names)):
            errors.append(f"INDEX_SPEC_MISMATCH:{index_path}")
    # A REF_DEF naming an evidence JSON that no entry actually cites is a
    # leftover definition whose counts are no longer verified (I64-A (d)).
    # Checking the resolved JSON target (not just the label) keeps the
    # legitimate inline-link form accepted.
    if kind == "EVIDENCE":
        with open(index_path, encoding="utf-8") as fh:
            idx_text = fh.read()
        base = os.path.dirname(index_path)
        for m in REF_DEF.finditer(idx_text):
            raw = unquote(m.group(2).strip()).split('#', 1)[0]
            if not raw or raw.startswith(NON_LOCAL):
                continue
            if not (raw.endswith('.json') or 'test-run-' in raw):
                continue
            resolved = os.path.normpath(os.path.join(base, raw))
            if resolved.endswith('.json') and resolved not in consumed:
                errors.append(f"INDEX_UNRESOLVED_ENTRY:{index_path}")
                break

print(f"ok={ok} errors={len(errors)}")
for e in errors:
    print("ERROR:" + e)
PY
)

echo "$result" | head -1
echo "$result" | grep '^ERROR:' | head -20 || true
echo "$result" | grep -q '^ok=[0-9]* errors=0$'
