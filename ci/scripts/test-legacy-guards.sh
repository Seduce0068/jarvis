#!/usr/bin/env bash
# Regression guard: the legacy OCI/destructive launchers must stay disabled.
# A future edit that re-enables delegation to backup.rs/restore.rs/deploy.rs
# would silently reintroduce data loss or an out-of-policy OCI upload.
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

ok=0; fail=0
check_guard() { # script expected-hint
  local script="$1" hint="$2" out
  out=$(bash "$repo/$script" 2>&1)
  local code=$?
  if [ "$code" -eq 2 ] && printf '%s' "$out" | grep -qi "$hint"; then
    echo "  PASS: $script remains fail-closed (exit 2, hint present)"
    ok=$((ok+1))
  else
    echo "  FAIL: $script code=$code hint='$hint' output=$(printf '%s' "$out" | head -1)"
    fail=$((fail+1))
  fi
}

check_guard infra/scripts/backup.sh 'backup-preflight'
check_guard infra/scripts/restore.sh 'backup-redesign-notes'
check_guard infra/scripts/deploy.sh 'deploy-mcp-gw-safe'

# The new local backup must stay non-destructive by default: a dry run must
# not create the destination directory at all.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if bash "$repo/infra/scripts/backup-local.sh" --backup-dir "$tmp/dest" >/dev/null 2>&1 \
   && [ ! -e "$tmp/dest" ]; then
  echo "  PASS: backup-local.sh dry run writes nothing"
  ok=$((ok+1))
else
  echo "  FAIL: backup-local.sh dry run created files or failed"
  fail=$((fail+1))
fi

# Help output must work for all new scripts (cheap smoke test for syntax).
for script in backup-local.sh verify-backup.sh install-backup-timer.sh; do
  if bash "$repo/infra/scripts/$script" --help >/dev/null 2>&1; then
    echo "  PASS: $script --help"
    ok=$((ok+1))
  else
    echo "  FAIL: $script --help"
    fail=$((fail+1))
  fi
done

# install-backup-timer.sh must roll back on every explicit failure after the
# install phase. Static guard: no bare "exit 1" in the post-install region.
install_script="$repo/infra/scripts/install-backup-timer.sh"
post_install="$(sed -n '/^install -o root/,$p' "$install_script")"
if printf '%s' "$post_install" | grep -qE '[[:space:]]exit 1'; then
  echo "  FAIL: install-backup-timer.sh has bare exit 1 after install (rollback skipped)"
  fail=$((fail+1))
else
  echo "  PASS: install-backup-timer.sh uses fail() for post-install errors"
  ok=$((ok+1))
fi

# P0-9: drift check must flag a missing deployed allowlist as DANGER, not
# silently skip all allowlist/web.fetch checks.
drift_script="$repo/infra/scripts/check-deploy-drift.sh"
out=$(JARVIS_DRIFT_ALLOWLIST=/nonexistent/allowlist.toml bash "$drift_script" 2>&1)
if printf '%s' "$out" | grep -q "MISSING (deployed)"; then
  echo "  PASS: drift flags missing deployed allowlist"
  ok=$((ok+1))
else
  echo "  FAIL: drift silently skipped missing allowlist"
  fail=$((fail+1))
fi

# P2-12: drift check must compare Cedar policy/schema and PII patterns.
tmp_drift="$(mktemp -d "$tmp/drift.XXXXXX")"
mkdir -p "$tmp_drift/policy"
cp "$repo/infra/config/policy/policies.cedar" "$tmp_drift/policy/"
cp "$repo/infra/config/policy/casbin_policy.csv" "$tmp_drift/policy/"
cp "$repo/infra/config/pii_patterns.toml" "$tmp_drift/pii_patterns.toml"
# Deliberately stale cedar schema -> must report DIFF.
printf '// stale\n' > "$tmp_drift/policy/cedar_schema.cedarschema"
out=$(JARVIS_DRIFT_POLICY_DIR="$tmp_drift/policy" \
      JARVIS_DRIFT_PII="$tmp_drift/pii_patterns.toml" \
      JARVIS_DRIFT_ALLOWLIST=/nonexistent \
      bash "$repo/infra/scripts/check-deploy-drift.sh" 2>&1)
if printf '%s' "$out" | grep -q "cedar_schema.cedarschema.*DIFF"; then
  echo "  PASS: drift detects stale cedar schema"
  ok=$((ok+1))
else
  echo "  FAIL: drift missed stale cedar schema"
  fail=$((fail+1))
fi

# P1-9: the evidence gate must reject both a stale spec_hash and a failed test
# count. Run a copy of the gate from a temporary repo layout so the real
# specs/ and evidence/ files are never modified. The temp tree is removed by
# the EXIT trap on $tmp.
evidence_tmp="$tmp/evidence_negative"
mkdir -p "$evidence_tmp/ci/scripts" "$evidence_tmp/evidence" "$evidence_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$evidence_tmp/ci/scripts/check-evidence.sh"
for spec in "$repo"/specs/test-*.md; do
  ln -s "$spec" "$evidence_tmp/specs/$(basename "$spec")"
done
# The gate also checks both indexes and their local links (arch #3), so mirror
# those files plus the linked todo.md into the temporary layout.
cp "$repo/specs/index.md" "$evidence_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$evidence_tmp/evidence/index.md"
cp "$repo/todo.md" "$evidence_tmp/todo.md"

tamper_evidence() { # src dst mode [index_dst]
  python3 - "$1" "$2" "$3" "${4:-}" <<'PY'
import json, os, re, sys

def rewrite_index_passed(index_dst, src_name, short):
    """Keep the cited entry's passed count in sync with a tampered JSON.

    Without this the index-vs-results gate would also fire and mask the
    Scenario-count regression that these tamper modes are meant to isolate.
    """
    with open(index_dst, encoding='utf-8') as fh:
        idx = fh.read()
    label_match = re.search(
        r'^\[([^\]]+)\]:\s*' + re.escape(src_name) + r'\s*$', idx, re.MULTILINE)
    if not label_match:
        raise SystemExit('index REF_DEF not found for ' + src_name)
    label = label_match.group(1)
    starts = [m.start() for m in re.finditer(r'^[ \t]*-[ \t]+\[', idx, re.MULTILINE)]
    for i, start in enumerate(starts):
        end = starts[i + 1] if i + 1 < len(starts) else len(idx)
        block = idx[start:end]
        if '][' + label + ']' not in block:
            continue
        new_block, n = re.subn(
            r'\d+([ \t]+passed)', lambda m: str(short) + m.group(1), block, count=1)
        if n != 1:
            raise SystemExit('passed count not found in index entry for ' + label)
        idx = idx[:start] + new_block + idx[end:]
        break
    else:
        raise SystemExit('index entry not found for ' + label)
    with open(index_dst, 'w', encoding='utf-8') as fh:
        fh.write(idx)

src, dst, mode = sys.argv[1:4]
index_dst = sys.argv[4] or None
with open(src, encoding='utf-8') as fh:
    data = json.load(fh)
if mode == 'hash':
    data['spec_hash'] = 'sha256:' + '0' * 64
elif mode == 'failed':
    data['results']['failed'] = 1
elif mode == 'empty_passed':
    data['results']['passed'] = 0
elif mode == 'timestamp':
    # Naive timestamp: no timezone, so it is not RFC 3339.
    data['timestamp'] = '2026-11-18 05:42:57'
elif mode == 'calendar':
    data['timestamp'] = '9999-99-99T99:99:99Z'
elif mode == 'failed_bool':
    data['results']['failed'] = False
elif mode == 'passed_bool':
    data['results']['passed'] = True
elif mode in ('notes_count_conflict', 'notes_passed_conflict'):
    # Keep the original (correct) count, then append a contradictory second
    # one. A gate using re.search would see only the first count and pass;
    # the fixed gate uses re.finditer and must reject both variants.
    conflict = int(data['results']['passed']) + 1
    suffix = 'checks' if mode == 'notes_count_conflict' else 'passed'
    notes = data.get('notes', '')
    data['notes'] = (notes + ' ' + str(conflict) + ' ' + suffix + '.') \
        if isinstance(notes, str) else (str(conflict) + ' ' + suffix + '.')
elif mode == 'ignored_no_number':
    # Replace the numeric "N ignored" justification with a phrase that still
    # contains the substring "ignored" but no count. The old gate only looked
    # for that substring and accepted this; the fixed gate requires the number
    # to equal results.skipped. Caller picks a skipped>0 source.
    notes = data.get('notes', '')
    if not isinstance(notes, str):
        raise SystemExit('notes must be str for ignored_no_number')
    new_notes, n = re.subn(r'\b\d+\s+ignored\b', 'no tests were ignored',
                           notes, count=1, flags=re.IGNORECASE)
    if n != 1:
        raise SystemExit('numeric ignored phrase not found')
    data['notes'] = new_notes
elif mode == 'bad_notes_type':
    # notes must be a str. A dict previously turned both the skipped
    # justification and the notes-consistency checks into no-ops. Only the
    # BAD_NOTES_TYPE gate can reject this (caller picks a skipped=0 source).
    data['notes'] = {'count': data['results']['passed']}
elif mode == 'bad_results_type':
    # skipped must be a strict non-negative int. A negative value trips only
    # the BAD_RESULTS_TYPE gate when the source has skipped=0 and notes with
    # no "ignored" phrase (chosen by the caller), so a disabled type gate is
    # detectable instead of being masked by a notes/index error.
    data['results']['skipped'] = -1
elif mode == 'req_removed':
    # Drop one requirement from the evidence declaration. The referenced spec
    # still defines the full set, so only the REQ_SET_MISMATCH gate can reject
    # this copy; every other check stays valid.
    reqs = data['test_basis_ref'].get('requirements')
    if not isinstance(reqs, list) or len(reqs) < 2:
        raise SystemExit('requirements array too short for tamper')
    reqs.pop()
elif mode in ('spec_outside_glob', 'spec_dotdot', 'spec_absolute'):
    # I64-A (b): name a spec outside specs/test-*.md, or escape the specs/
    # tree. requirements=[] reproduces the original forgery; only the
    # REQ_SPEC_UNRESOLVED gate can reject these copies.
    ref = data['test_basis_ref']
    if mode == 'spec_outside_glob':
        ref['spec_path'] = 'specs/test-rogue.md'
    elif mode == 'spec_dotdot':
        ref['spec_path'] = 'specs/../../etc/passwd'
    else:
        ref['spec_path'] = '/nonexistent/specs/test-absolute.md'
    ref['requirements'] = []
elif mode in ('scenario_shortfall', 'skipped_inflated'):
    # Report fewer executions than the spec declares while keeping results,
    # notes and the index entry mutually consistent. Only the Scenario gate
    # can reject these copies, so disabling it makes the tamper accepted.
    # skipped_inflated additionally hides the shortfall behind a large skipped
    # count explicitly justified with "ignored" in notes: a gate that still
    # sums skipped into executed would wrongly accept it.
    spec_rel = data['test_basis_ref']['spec_path']
    spec_abs = os.path.join(os.path.dirname(os.path.dirname(src)), spec_rel)
    with open(spec_abs, encoding='utf-8') as fh:
        spec_text = fh.read()
    scenario_total = len(re.findall(r'^[ \t]*Scenario[ \t]*:', spec_text, re.MULTILINE))
    short = max(1, scenario_total - 1)
    skipped = scenario_total + 100 if mode == 'skipped_inflated' else 0
    data['results']['passed'] = short
    data['results']['failed'] = 0
    data['results']['skipped'] = skipped
    data['notes'] = ('実測結果 ' + str(short) + ' 件。' + str(skipped) + ' ignored。'
                     if mode == 'skipped_inflated'
                     else '実測結果 ' + str(short) + ' 件。')
    if not index_dst:
        raise SystemExit(mode + ' needs index_dst')
    rewrite_index_passed(index_dst, os.path.basename(src), short)
else:
    raise SystemExit('unknown tamper mode: ' + mode)
with open(dst, 'w', encoding='utf-8') as fh:
    json.dump(data, fh)
PY
}

evidence_files=( "$repo"/evidence/test-run-*.json )
if [ "${#evidence_files[@]}" -eq 0 ]; then
  echo "  FAIL: no source evidence JSON available for negative test"
  fail=$((fail+1))
else
  source_json="${evidence_files[0]}"
  evidence_copy="$evidence_tmp/evidence/$(basename "$source_json")"
  gate="$evidence_tmp/ci/scripts/check-evidence.sh"

  # Copy every evidence file so the gate's bidirectional spec-coverage check
  # passes in the temporary layout; only the selected copy is tampered below.
  cp "$repo"/evidence/test-run-*.json "$evidence_tmp/evidence/"
  if bash "$gate" >/dev/null 2>&1; then
    echo "  PASS: untampered evidence copy accepted"
    ok=$((ok+1))
  else
    echo "  FAIL: untampered evidence copy rejected"
    fail=$((fail+1))
  fi

  # notes_* modes add a second, conflicting count to otherwise valid notes:
  # they verify that the gate validates every count match, not only the first.
  for mode in hash failed empty_passed timestamp calendar failed_bool passed_bool \
              notes_count_conflict notes_passed_conflict req_removed; do
    if ! tamper_evidence "$source_json" "$evidence_copy" "$mode"; then
      echo "  FAIL: could not prepare tampered evidence ($mode)"
      fail=$((fail+1))
      continue
    fi
    out=$(bash "$gate" 2>&1); code=$?
    case "$mode" in
      # Changing spec_hash without renaming the file is rejected either by
      # the filename-prefix check (first) or the spec-content hash check.
      hash) pattern='HASH_FILENAME_MISMATCH\|SPEC_HASH_MISMATCH' ;;
      failed) pattern='FAILED_TESTS' ;;
      empty_passed) pattern='NO_PASSING_TESTS' ;;
      timestamp) pattern='BAD_TIMESTAMP' ;;
      calendar) pattern='BAD_TIMESTAMP' ;;
      failed_bool) pattern='FAILED_TESTS' ;;
      passed_bool) pattern='NO_PASSING_TESTS' ;;
      notes_count_conflict) pattern='NOTES_COUNT_MISMATCH' ;;
      notes_passed_conflict) pattern='NOTES_PASSED_MISMATCH' ;;
      req_removed) pattern='REQ_SET_MISMATCH' ;;
    esac
    if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q -- "$pattern"; then
      echo "  PASS: check-evidence.sh rejects tampered $mode"
      ok=$((ok+1))
    else
      echo "  FAIL: check-evidence.sh accepted tampered $mode (exit $code)"
      printf '%s\n' "$out" | head -3 >&2
      fail=$((fail+1))
    fi
  done
fi

# arch #3: removing a link to an existing spec from specs/index.md must be
# detected. Use a separate temporary tree so the earlier checks stay intact.
index_tmp="$tmp/index_negative"
mkdir -p "$index_tmp/ci/scripts" "$index_tmp/evidence" "$index_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$index_tmp/ci/scripts/check-evidence.sh"
cp "$repo/evidence/index.md" "$index_tmp/evidence/index.md"
cp "$repo/todo.md" "$index_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$index_tmp/evidence/"
spec_files=( "$repo"/specs/test-*.md )
for spec in "${spec_files[@]}"; do
  ln -s "$spec" "$index_tmp/specs/$(basename "$spec")"
done
# Drop one inline link line while leaving the spec file itself in place.
python3 - "$repo/specs/index.md" "$index_tmp/specs/index.md" "${spec_files[0]}" <<'PY'
import os, re, sys

src, dst, target_spec = sys.argv[1:4]
name = os.path.basename(target_spec)
with open(src, encoding='utf-8') as fh:
    text = fh.read()
text, removed = re.subn(
    r'^- \[[^\]]*\]\(' + re.escape(name) + r'\)\s*$',
    '', text, count=1, flags=re.MULTILINE)
if removed != 1:
    raise SystemExit('index link for ' + name + ' not found')
with open(dst, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
out=$(bash "$index_tmp/ci/scripts/check-evidence.sh" 2>&1); code=$?
if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'SPEC_NOT_IN_INDEX'; then
  echo "  PASS: check-evidence.sh rejects a spec unlinked from specs/index.md"
  ok=$((ok+1))
else
  echo "  FAIL: check-evidence.sh accepted an unlinked spec (exit $code)"
  printf '%s\n' "$out" | head -3 >&2
  fail=$((fail+1))
fi

# R4-A: index headline counts, type fail-closed gates and Scenario coverage.
# Build one more temporary tree so the real specs/ and evidence/ are untouched.
gate_tmp="$tmp/gate_negative"
mkdir -p "$gate_tmp/ci/scripts" "$gate_tmp/evidence" "$gate_tmp/specs"
gate="$gate_tmp/ci/scripts/check-evidence.sh"
cp "$repo/ci/scripts/check-evidence.sh" "$gate"
cp "$repo/specs/index.md" "$gate_tmp/specs/index.md"
cp "$repo/todo.md" "$gate_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$gate_tmp/evidence/"
for spec in "${spec_files[@]}"; do
  ln -s "$spec" "$gate_tmp/specs/$(basename "$spec")"
done

# Pick a source with skipped=0 and no "ignored" phrase in notes: this keeps the
# type-gate tamper modes isolated from the skipped-justification, notes-count
# and index-count gates, so a no-op'd type gate is actually detectable.
type_source=$(python3 - "${evidence_files[@]}" <<'PY'
import json, sys
for path in sys.argv[1:]:
    d = json.load(open(path, encoding='utf-8'))
    if d['results']['skipped'] == 0 and 'ignored' not in str(d.get('notes', '')).lower():
        print(path)
        break
PY
)
if [ -z "$type_source" ]; then
  echo "  FAIL: no suitable source evidence for type-gate tamper"
  fail=$((fail+1))
else
  type_copy="$gate_tmp/evidence/$(basename "$type_source")"
  # Each mode starts from a pristine JSON and index; scenario_shortfall also
  # rewrites the index count, so restore both before every iteration.
  for mode in bad_notes_type bad_results_type scenario_shortfall skipped_inflated; do
    cp "$repo/evidence/index.md" "$gate_tmp/evidence/index.md"
    cp "$type_source" "$type_copy"
    if ! tamper_evidence "$type_source" "$type_copy" "$mode" \
                         "$gate_tmp/evidence/index.md"; then
      echo "  FAIL: could not prepare tampered evidence ($mode)"
      fail=$((fail+1))
      continue
    fi
    out=$(bash "$gate" 2>&1); code=$?
    case "$mode" in
      bad_notes_type) pattern='BAD_NOTES_TYPE' ;;
      bad_results_type) pattern='BAD_RESULTS_TYPE' ;;
      scenario_shortfall) pattern='SCENARIO_COUNT_MISMATCH' ;;
      skipped_inflated) pattern='SCENARIO_COUNT_MISMATCH' ;;
    esac
    if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q -- "$pattern"; then
      echo "  PASS: check-evidence.sh rejects tampered $mode"
      ok=$((ok+1))
    else
      echo "  FAIL: check-evidence.sh accepted tampered $mode (exit $code)"
      printf '%s\n' "$out" | head -3 >&2
      fail=$((fail+1))
    fi
  done

  # Index passed count rewritten to 0 while the JSON still records the real
  # total: the index-vs-results gate must reject with INDEX_RESULT_MISMATCH.
  cp "$repo/evidence/index.md" "$gate_tmp/evidence/index.md"
  cp "$type_source" "$type_copy"
  if python3 - "$repo/evidence/index.md" "$gate_tmp/evidence/index.md" <<'PY'
import re, sys

src, dst = sys.argv[1:3]
with open(src, encoding='utf-8') as fh:
    text = fh.read()
text, changed = re.subn(r'\d+([ \t]+passed)', r'0\1', text, count=1)
if changed != 1:
    raise SystemExit('no passed count found in evidence index')
with open(dst, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
  then
    out=$(bash "$gate" 2>&1); code=$?
    if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'INDEX_RESULT_MISMATCH'; then
      echo "  PASS: check-evidence.sh rejects tampered index passed count"
      ok=$((ok+1))
    else
      echo "  FAIL: check-evidence.sh accepted tampered index passed count (exit $code)"
      printf '%s\n' "$out" | head -3 >&2
      fail=$((fail+1))
    fi
  else
    echo "  FAIL: could not prepare tampered index passed count"
    fail=$((fail+1))
  fi
fi

# I60-C: a positive skipped count needs a numeric "N ignored" in notes, and
# evidence index entries must resolve inline links and state passed/failed.
# Use a fresh temp tree so the earlier tamper fixtures stay pristine.
i60_tmp="$tmp/i60_negative"
mkdir -p "$i60_tmp/ci/scripts" "$i60_tmp/evidence" "$i60_tmp/specs"
i60_gate="$i60_tmp/ci/scripts/check-evidence.sh"
cp "$repo/ci/scripts/check-evidence.sh" "$i60_gate"
cp "$repo/specs/index.md" "$i60_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$i60_tmp/evidence/index.md"
cp "$repo/todo.md" "$i60_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$i60_tmp/evidence/"
for spec in "${spec_files[@]}"; do
  ln -s "$spec" "$i60_tmp/specs/$(basename "$spec")"
done

# The mcp-gw record is the existing evidence with skipped>0 and a numeric
# ignored claim; it must still pass unchanged before any tampering.
skipped_source=$(python3 - "${evidence_files[@]}" <<'PY'
import json, re, sys
for path in sys.argv[1:]:
    d = json.load(open(path, encoding='utf-8'))
    notes = d.get('notes', '')
    if (type(d['results'].get('skipped')) is int
            and d['results']['skipped'] > 0
            and isinstance(notes, str)
            and re.search(r'\d+\s+ignored', notes, re.IGNORECASE)):
        print(path)
        break
PY
)
if [ -z "$skipped_source" ]; then
  echo "  FAIL: no evidence with numeric ignored claim for I60-C"
  fail=$((fail+1))
else
  skipped_copy="$i60_tmp/evidence/$(basename "$skipped_source")"
  if bash "$i60_gate" >/dev/null 2>&1; then
    echo "  PASS: skipped evidence with numeric ignored claim accepted"
    ok=$((ok+1))
  else
    echo "  FAIL: skipped evidence with numeric ignored claim rejected"
    fail=$((fail+1))
  fi
  if ! tamper_evidence "$skipped_source" "$skipped_copy" ignored_no_number; then
    echo "  FAIL: could not prepare ignored_no_number tamper"
    fail=$((fail+1))
  else
    out=$(bash "$i60_gate" 2>&1); code=$?
    if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'SKIPPED_TESTS'; then
      echo "  PASS: check-evidence.sh rejects notes with no numeric ignored count"
      ok=$((ok+1))
    else
      echo "  FAIL: check-evidence.sh accepted notes with no numeric ignored count (exit $code)"
      printf '%s\n' "$out" | head -3 >&2
      fail=$((fail+1))
    fi
  fi
fi

# Convert one evidence entry from "[text][run-mcp]" to an inline link. The
# untouched conversion must stay accepted; a tampered count or a missing
# count line must be rejected (INDEX_RESULT_MISMATCH / INDEX_RESULT_MISSING).
for mode in inline_ok inline_tamper inline_no_counts; do
  cp "$repo/evidence/index.md" "$i60_tmp/evidence/index.md"
  cp "$repo"/evidence/test-run-*.json "$i60_tmp/evidence/"
  if ! python3 - "$repo/evidence/index.md" "$i60_tmp/evidence/index.md" "$mode" <<'PY'
import re, sys

src, dst, mode = sys.argv[1:4]
with open(src, encoding='utf-8') as fh:
    text = fh.read()
label = 'run-mcp'
def_m = re.search(r'^\[' + re.escape(label) + r'\]:[ \t]*([^\s]+)[ \t]*$',
                  text, re.MULTILINE)
if not def_m:
    raise SystemExit('run-mcp REF_DEF not found')
target = def_m.group(1)
starts = [m.start() for m in re.finditer(r'^[ \t]*-[ \t]+\[', text, re.MULTILINE)]
for i, start in enumerate(starts):
    end = starts[i + 1] if i + 1 < len(starts) else len(text)
    block = text[start:end]
    if '][' + label + ']' not in block:
        continue
    new = block.replace('][' + label + ']', '](' + target + ')', 1)
    if mode == 'inline_tamper':
        new, n = re.subn(r'\d+([ \t]+passed)', r'0\1', new, count=1)
        if n != 1:
            raise SystemExit('passed count not found in inline entry')
    elif mode == 'inline_no_counts':
        new, n = re.subn(r'^[ \t]*\d+[ \t]+passed[^\n]*\n?', '', new,
                         count=1, flags=re.MULTILINE)
        if n != 1:
            raise SystemExit('counts line not found in inline entry')
    text = text[:start] + new + text[end:]
    break
else:
    raise SystemExit('index entry for run-mcp not found')
with open(dst, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
  then
    echo "  FAIL: could not prepare index variant $mode"
    fail=$((fail+1))
    continue
  fi
  out=$(bash "$i60_gate" 2>&1); code=$?
  if [ "$mode" = inline_ok ]; then
    if printf '%s' "$out" | grep -q '^ok=[0-9]* errors=0$'; then
      echo "  PASS: inline-link index entry accepted when counts match"
      ok=$((ok+1))
    else
      echo "  FAIL: inline-link index entry rejected (exit $code)"
      printf '%s\n' "$out" | head -3 >&2
      fail=$((fail+1))
    fi
    continue
  fi
  case "$mode" in
    inline_tamper) pattern='INDEX_RESULT_MISMATCH' ;;
    inline_no_counts) pattern='INDEX_RESULT_MISSING' ;;
  esac
  if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q -- "$pattern"; then
    echo "  PASS: check-evidence.sh rejects $mode index entry"
    ok=$((ok+1))
  else
    echo "  FAIL: check-evidence.sh accepted $mode index entry (exit $code)"
    printf '%s\n' "$out" | head -3 >&2
    fail=$((fail+1))
  fi
done

# I62-A: every count match and every reference link in an evidence index entry
# is validated. The old gate used re.search for counts and REF_USAGE.search for
# links, so a second contradictory count or a second link to a different JSON
# was invisible. Both must now fail closed.
i62_tmp="$tmp/i62_negative"
mkdir -p "$i62_tmp/ci/scripts" "$i62_tmp/evidence" "$i62_tmp/specs"
i62_gate="$i62_tmp/ci/scripts/check-evidence.sh"
cp "$repo/ci/scripts/check-evidence.sh" "$i62_gate"
cp "$repo/specs/index.md" "$i62_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$i62_tmp/evidence/index.md"
cp "$repo/todo.md" "$i62_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$i62_tmp/evidence/"
for spec in "${spec_files[@]}"; do
  ln -s "$spec" "$i62_tmp/specs/$(basename "$spec")"
done

if bash "$i62_gate" >/dev/null 2>&1; then
  echo "  PASS: I62-A pristine evidence index accepted"
  ok=$((ok+1))
else
  echo "  FAIL: I62-A pristine evidence index rejected"
  fail=$((fail+1))
fi

for mode in duplicate_count second_reference; do
  cp "$repo/evidence/index.md" "$i62_tmp/evidence/index.md"
  cp "$repo"/evidence/test-run-*.json "$i62_tmp/evidence/"
  if ! python3 - "$repo/evidence/index.md" "$i62_tmp/evidence/index.md" "$mode" <<'PY'
import re, sys

src, dst, mode = sys.argv[1:4]
with open(src, encoding='utf-8') as fh:
    text = fh.read()
if mode == 'duplicate_count':
    # Keep the correct count and append a contradictory second one. re.search
    # saw only the first; the fixed gate validates every match.
    text, changed = re.subn(
        r'^([ \t]*\d+[ \t]+passed[^\n]*)$', r'\1 0 passed。',
        text, count=1, flags=re.MULTILINE)
elif mode == 'second_reference':
    # Add a second reference-style link inside the first evidence entry. The
    # appended label resolves to a different evidence JSON whose passed count
    # differs; REF_USAGE.search missed it, finditer must validate it.
    text, changed = re.subn(
        r'^([ \t]*-[ \t]+\[[^\]]*\]\[run-policy\])',
        r'\1 [second][run-mcp]', text, count=1, flags=re.MULTILINE)
else:
    raise SystemExit('unknown I62-A mode: ' + mode)
if changed != 1:
    raise SystemExit(mode + ' index fixture not found')
with open(dst, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
  then
    echo "  FAIL: could not prepare I62-A index variant $mode"
    fail=$((fail+1))
    continue
  fi
  out=$(bash "$i62_gate" 2>&1); code=$?
  if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'INDEX_RESULT_MISMATCH'; then
    echo "  PASS: check-evidence.sh rejects I62-A $mode index entry"
    ok=$((ok+1))
  else
    echo "  FAIL: check-evidence.sh accepted I62-A $mode index entry (exit $code)"
    printf '%s\n' "$out" | head -3 >&2
    fail=$((fail+1))
  fi
done

# I59-A: two evidence files citing the same spec must BOTH have their
# requirements checked. The old dict keyed by spec kept only the last entry,
# so tampering the earlier (lexicographically first) copy went unnoticed. The
# duplicate sorts before every real test-run-20* file because of its 00000000
# date, and it is indexed so only the REQ gate can reject it.
req_dup_tmp="$tmp/req_dup_negative"
mkdir -p "$req_dup_tmp/ci/scripts" "$req_dup_tmp/evidence" "$req_dup_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$req_dup_tmp/ci/scripts/check-evidence.sh"
cp "$repo/specs/index.md" "$req_dup_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$req_dup_tmp/evidence/index.md"
cp "$repo/todo.md" "$req_dup_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$req_dup_tmp/evidence/"
for spec in "${spec_files[@]}"; do
  ln -s "$spec" "$req_dup_tmp/specs/$(basename "$spec")"
done
req_dup_src="${evidence_files[0]}"
req_dup_suffix="${req_dup_src##*-}"
req_dup_name="test-run-00000000-$req_dup_suffix"
req_dup_copy="$req_dup_tmp/evidence/$req_dup_name"
if ! tamper_evidence "$req_dup_src" "$req_dup_copy" req_removed; then
  echo "  FAIL: could not prepare duplicate-evidence requirements tamper"
  fail=$((fail+1))
else
  python3 - "$req_dup_tmp/evidence/index.md" "$req_dup_name" "$req_dup_src" <<'PY'
import json, os, sys

index_path, dup_name, src_name = sys.argv[1:4]
base = os.path.dirname(index_path)
with open(os.path.join(base, src_name), encoding='utf-8') as fh:
    data = json.load(fh)
res = data['results']
spec_name = os.path.basename(data['test_basis_ref']['spec_path'])
with open(index_path, encoding='utf-8') as fh:
    text = fh.read()
marker = '\n## 関連\n'
if marker not in text:
    raise SystemExit('related section marker not found')
bullet = (
    '- [duplicate evidence][run-dup]\n'
    '  — [' + spec_name + '](../specs/' + spec_name + ') の実測結果。\n'
    '  ' + str(res['passed']) + ' passed / ' + str(res['failed']) + ' failed。\n'
)
text = text.replace(marker, '\n' + bullet + marker, 1)
text = text.rstrip() + '\n[run-dup]: ' + dup_name + '\n'
with open(index_path, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
  # Heredoc exit status must be checked here.
  # shellcheck disable=SC2181
  if [ $? -ne 0 ]; then
    echo "  FAIL: could not index the duplicate evidence copy"
    fail=$((fail+1))
  else
    out=$(bash "$req_dup_tmp/ci/scripts/check-evidence.sh" 2>&1); code=$?
    if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'REQ_SET_MISMATCH'; then
      echo "  PASS: check-evidence.sh rejects tampered requirements in one of two evidence files for the same spec"
      ok=$((ok+1))
    else
      echo "  FAIL: check-evidence.sh accepted a tampered duplicate-evidence requirements set (exit $code)"
      printf '%s\n' "$out" | head -3 >&2
      fail=$((fail+1))
    fi
  fi
fi

# I64-A: index entries and evidence spec paths must fail closed. Use a fresh
# tree so every fixture below starts from pristine specs, JSON and indexes.
i64_tmp="$tmp/i64_negative"
mkdir -p "$i64_tmp/ci/scripts" "$i64_tmp/evidence" "$i64_tmp/specs"
i64_gate="$i64_tmp/ci/scripts/check-evidence.sh"
cp "$repo/ci/scripts/check-evidence.sh" "$i64_gate"
cp "$repo/specs/index.md" "$i64_tmp/specs/index.md"
cp "$repo/todo.md" "$i64_tmp/todo.md"
cp "$repo/evidence/index.md" "$i64_tmp/evidence/index.md"
cp "$repo"/evidence/test-run-*.json "$i64_tmp/evidence/"
for spec in "${spec_files[@]}"; do
  ln -s "$spec" "$i64_tmp/specs/$(basename "$spec")"
done

if bash "$i64_gate" >/dev/null 2>&1; then
  echo "  PASS: I64-A pristine evidence index accepted"
  ok=$((ok+1))
else
  echo "  FAIL: I64-A pristine evidence index rejected"
  fail=$((fail+1))
fi

# (b) A spec_path outside the globbed set, a ".." escape and an absolute path
# must each be rejected with REQ_SPEC_UNRESOLVED.
for mode in spec_outside_glob spec_dotdot spec_absolute; do
  cp "$repo/evidence/index.md" "$i64_tmp/evidence/index.md"
  cp "$repo"/evidence/test-run-*.json "$i64_tmp/evidence/"
  i64_source="${evidence_files[0]}"
  i64_copy="$i64_tmp/evidence/$(basename "$i64_source")"
  if ! tamper_evidence "$i64_source" "$i64_copy" "$mode"; then
    echo "  FAIL: could not prepare I64-A evidence tamper ($mode)"
    fail=$((fail+1))
    continue
  fi
  out=$(bash "$i64_gate" 2>&1); code=$?
  if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'REQ_SPEC_UNRESOLVED'; then
    echo "  PASS: check-evidence.sh rejects I64-A $mode"
    ok=$((ok+1))
  else
    echo "  FAIL: check-evidence.sh accepted I64-A $mode (exit $code)"
    printf '%s\n' "$out" | head -3 >&2
    fail=$((fail+1))
  fi
done

# (a)/(c)/(d) Index-side fixtures. Each mode restores a pristine index and
# JSON first, so no earlier tamper can mask the targeted error code.
for mode in unlink_and_tamper spec_link_swap unhandled_entry orphan_ref_def; do
  cp "$repo/evidence/index.md" "$i64_tmp/evidence/index.md"
  cp "$repo"/evidence/test-run-*.json "$i64_tmp/evidence/"
  if ! python3 - "$repo/evidence/index.md" "$i64_tmp/evidence/index.md" "$mode" <<'PY'
import re, sys

src, dst, mode = sys.argv[1:4]
with open(src, encoding='utf-8') as fh:
    text = fh.read()
starts = [m.start() for m in re.finditer(r'^[ \t]*-[ \t]+\[', text, re.MULTILINE)]

def entry_bounds(label):
    for i, start in enumerate(starts):
        end = starts[i + 1] if i + 1 < len(starts) else len(text)
        if '][' + label + ']' in text[start:end]:
            return start, end
    raise SystemExit('entry not found for ' + label)

if mode == 'unlink_and_tamper':
    start, end = entry_bounds('run-policy')
    block = text[start:end].replace('][run-policy]', '] (link removed)', 1)
    block, n = re.subn(r'\d+([ \t]+passed)', r'999\1', block, count=1)
    if n != 1:
        raise SystemExit('passed count not found')
    text = text[:start] + block + text[end:]
elif mode == 'spec_link_swap':
    text, n = re.subn(r'\]\(\.\./specs/test-policy-approval\.md\)',
                      '](../specs/test-ui.md)', text, count=1)
    if n != 1:
        raise SystemExit('spec link not found')
elif mode == 'unhandled_entry':
    marker = '\n## 関連\n'
    if marker not in text:
        raise SystemExit('related section marker not found')
    bullet = ('- [unhandled][run-unknown]\n'
              '  999 passed / 0 failed。\n')
    text = text.replace(marker, '\n' + bullet + marker, 1)
elif mode == 'orphan_ref_def':
    start, end = entry_bounds('run-policy')
    text = text[:start] + text[end:]
else:
    raise SystemExit('unknown I64-A index mode: ' + mode)
with open(dst, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
  then
    echo "  FAIL: could not prepare I64-A index variant $mode"
    fail=$((fail+1))
    continue
  fi
  out=$(bash "$i64_gate" 2>&1); code=$?
  case "$mode" in
    unlink_and_tamper) pattern='INDEX_UNRESOLVED_ENTRY' ;;
    spec_link_swap) pattern='INDEX_SPEC_MISMATCH' ;;
    unhandled_entry) pattern='INDEX_UNRESOLVED_ENTRY' ;;
    orphan_ref_def) pattern='INDEX_UNRESOLVED_ENTRY' ;;
  esac
  if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q -- "$pattern"; then
    echo "  PASS: check-evidence.sh rejects I64-A $mode"
    ok=$((ok+1))
  else
    echo "  FAIL: check-evidence.sh accepted I64-A $mode (exit $code)"
    printf '%s\n' "$out" | head -3 >&2
    fail=$((fail+1))
  fi
done

# I65-C: a contract step appended to an existing Scenario must not be treated
# as covered when the evidence notes still lack its keyword. Use COPIED specs
# so the real specification is untouched; the gate evaluates contract coverage
# before the hash gate, so the drifted sha256 cannot mask the targeted code.
step_tmp="$tmp/scenario_step_negative"
mkdir -p "$step_tmp/ci/scripts" "$step_tmp/evidence" "$step_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$step_tmp/ci/scripts/check-evidence.sh"
cp "$repo/specs/index.md" "$step_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$step_tmp/evidence/index.md"
cp "$repo/todo.md" "$step_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$step_tmp/evidence/"
cp "$repo"/specs/test-*.md "$step_tmp/specs/"
# Duplicate the existing 415 contract step in TC-C. The Scenario count is
# unchanged and the notes are untouched, so only the new contract-coverage
# gate can reject this copy.
if python3 - "$step_tmp/specs/test-context.md" <<'PY'
import sys

path = sys.argv[1]
with open(path, encoding='utf-8') as fh:
    text = fh.read()
needle = '  Then 空本文の 415 を返す（ヘッダ付与を促す）\n'
if text.count(needle) != 1:
    raise SystemExit('415 contract step anchor not unique')
with open(path, 'w', encoding='utf-8') as fh:
    fh.write(text.replace(needle, needle + needle, 1))
PY
then
  out=$(bash "$step_tmp/ci/scripts/check-evidence.sh" 2>&1); code=$?
  if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'SCENARIO_STEP_UNCOVERED'; then
    echo "  PASS: check-evidence.sh rejects an uncovered contract step"
    ok=$((ok+1))
  else
    echo "  FAIL: check-evidence.sh accepted an uncovered contract step (exit $code)"
    printf '%s\n' "$out" | head -3 >&2
    fail=$((fail+1))
  fi
else
  echo "  FAIL: could not prepare contract-step tamper"
  fail=$((fail+1))
fi

# secret_scan.py must fail closed on a value-like credential and must never
# echo the value itself. Build a throwaway git repo with a synthetic token
# (no real secret); the tree is removed by the existing EXIT trap on $tmp.
scan_tmp="$tmp/secret_scan_negative"
mkdir -p "$scan_tmp"
dummy_value='AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
if ! (cd "$scan_tmp" \
      && git init -q >/dev/null 2>&1 \
      && printf 'BWS_ACCESS_TOKEN = %s\n' "$dummy_value" > leaky.env \
      && git add -f leaky.env >/dev/null 2>&1); then
  echo "  FAIL: could not prepare temporary git repo for secret scanner test"
  fail=$((fail+1))
else
  cp "$repo/ci/scripts/secret_scan.py" "$scan_tmp/secret_scan.py"
  out=$(cd "$scan_tmp" && python3 secret_scan.py 2>&1); code=$?
  if [ "$code" -eq 1 ] \
     && printf '%s' "$out" | grep -q 'VALUE-LIKE' \
     && ! printf '%s' "$out" | grep -q -- "$dummy_value"; then
    echo "  PASS: secret_scan.py rejects value-like credential without echoing the value"
    ok=$((ok+1))
  else
    # Never echo raw scanner output on failure: if the scanner regressed to
    # printing the value, this test must not echo it either. Show only the
    # exit code and a masked summary.
    echo "  FAIL: secret_scan.py exit=$code (output withheld; may contain the dummy value)"
    fail=$((fail+1))
  fi
fi

# Static-guard coverage: a look-alike of a SAFE_PATHS entry must NOT inherit
# the exemption. With prefix matching, `AGENTS.md.bak` would be downgraded to
# "review (safe path)" and the gate would pass silently (fail-open).
scan_lookalike="$tmp/secret_scan_lookalike"
mkdir -p "$scan_lookalike"
if ! (cd "$scan_lookalike" \
      && git init -q >/dev/null 2>&1 \
      && printf 'BWS_ACCESS_TOKEN = %s\n' "$dummy_value" > AGENTS.md.bak \
      && git add -f AGENTS.md.bak >/dev/null 2>&1); then
  echo "  FAIL: could not prepare look-alike safe-path repo for secret scanner test"
  fail=$((fail+1))
else
  # Copy the scanner only AFTER git add so it stays untracked and is not
  # scanned itself (its own regex literals are legitimate VALUE-LIKE shapes).
  cp "$repo/ci/scripts/secret_scan.py" "$scan_lookalike/secret_scan.py"
  out=$(cd "$scan_lookalike" && python3 secret_scan.py 2>&1); code=$?
  if [ "$code" -eq 1 ] \
     && printf '%s' "$out" | grep -q 'VALUE-LIKE' \
     && ! printf '%s' "$out" | grep -q -- "$dummy_value"; then
    echo "  PASS: secret_scan.py does not exempt SAFE_PATHS look-alikes"
    ok=$((ok+1))
  else
    echo "  FAIL: secret_scan.py exempted a SAFE_PATHS look-alike (exit=$code, output withheld)"
    fail=$((fail+1))
  fi
fi

# The gate must reject a spec that lost one required test-spec section. Reuse
# the spec list from the index-negative case above; all temp trees use $tmp.
section_tmp="$tmp/section_negative"
mkdir -p "$section_tmp/ci/scripts" "$section_tmp/evidence" "$section_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$section_tmp/ci/scripts/check-evidence.sh"
cp "$repo"/specs/test-*.md "$section_tmp/specs/"
cp "$repo/specs/index.md" "$section_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$section_tmp/evidence/index.md"
cp "$repo"/evidence/test-run-*.json "$section_tmp/evidence/"
cp "$repo/todo.md" "$section_tmp/todo.md"
# Remove only the "Risk & tier" heading line; the rest of the spec stays intact.
python3 - "$section_tmp/specs/$(basename "${spec_files[0]}")" <<'PY'
import re, sys

path = sys.argv[1]
with open(path, encoding='utf-8') as fh:
    text = fh.read()
text, removed = re.subn(r'^##[ \t]+Risk & tier[ \t]*\r?\n', '', text, count=1, flags=re.MULTILINE)
if removed != 1:
    raise SystemExit('Risk & tier heading not found')
with open(path, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
out=$(bash "$section_tmp/ci/scripts/check-evidence.sh" 2>&1); code=$?
if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'MISSING_SECTION'; then
  echo "  PASS: check-evidence.sh rejects spec with missing required section"
  ok=$((ok+1))
else
  echo "  FAIL: check-evidence.sh accepted spec with missing section (exit $code)"
  printf '%s\n' "$out" | head -3 >&2
  fail=$((fail+1))
fi

# A spec whose Document map lost one of the six information items must fail.
docmap_tmp="$tmp/docmap_negative"
mkdir -p "$docmap_tmp/ci/scripts" "$docmap_tmp/evidence" "$docmap_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$docmap_tmp/ci/scripts/check-evidence.sh"
cp "$repo"/specs/test-*.md "$docmap_tmp/specs/"
cp "$repo/specs/index.md" "$docmap_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$docmap_tmp/evidence/index.md"
cp "$repo"/evidence/test-run-*.json "$docmap_tmp/evidence/"
cp "$repo/todo.md" "$docmap_tmp/todo.md"
python3 - "$docmap_tmp/specs/$(basename "${spec_files[0]}")" <<'PY'
import re, sys

path = sys.argv[1]
with open(path, encoding='utf-8') as fh:
    text = fh.read()
text, removed = re.subn(
    r'^\|[ \t]*test case specification[ \t]*\|.*\r?\n', '', text, count=1, flags=re.MULTILINE)
if removed != 1:
    raise SystemExit('test case specification row not found')
with open(path, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
out=$(bash "$docmap_tmp/ci/scripts/check-evidence.sh" 2>&1); code=$?
if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'MISSING_INFO_ITEM'; then
  echo "  PASS: check-evidence.sh rejects spec with missing Document map info item"
  ok=$((ok+1))
else
  echo "  FAIL: check-evidence.sh accepted spec with missing info item (exit $code)"
  printf '%s\n' "$out" | head -3 >&2
  fail=$((fail+1))
fi

# I57-D: a Traceability Req that no requirement defines must fail closed.
# Use a separate temp tree with copied (not symlinked) specs so one Req cell
# can be rewritten without touching the repository's real specification.
req_tmp="$tmp/req_negative"
mkdir -p "$req_tmp/ci/scripts" "$req_tmp/evidence" "$req_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$req_tmp/ci/scripts/check-evidence.sh"
cp "$repo/specs/index.md" "$req_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$req_tmp/evidence/index.md"
cp "$repo/todo.md" "$req_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$req_tmp/evidence/"
cp "$repo"/specs/test-*.md "$req_tmp/specs/"
# Rewrite the first Traceability Req cell to an id no spec defines. The spec
# hash for that evidence also drifts; the REQ gate must still fire regardless.
python3 - "$req_tmp/specs/$(basename "${spec_files[0]}")" <<'PY'
import re, sys

path = sys.argv[1]
with open(path, encoding='utf-8') as fh:
    text = fh.read()
pattern = re.compile(r'(^\|[ \t]*TC-[^|]*\|[ \t]*)REQ-\d+([ \t]\|)', re.MULTILINE)
text, changed = pattern.subn(
    lambda m: m.group(1) + 'REQ-99999' + m.group(2), text, count=1)
if changed != 1:
    raise SystemExit('Traceability Req cell not found')
with open(path, 'w', encoding='utf-8') as fh:
    fh.write(text)
PY
out=$(bash "$req_tmp/ci/scripts/check-evidence.sh" 2>&1); code=$?
if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'REQ_SET_MISMATCH'; then
  echo "  PASS: check-evidence.sh rejects an undefined Traceability Req"
  ok=$((ok+1))
else
  echo "  FAIL: check-evidence.sh accepted an undefined Traceability Req (exit $code)"
  printf '%s\n' "$out" | head -3 >&2
  fail=$((fail+1))
fi

# I58-A: regression guards for the Round 54 shell fail-closed fixes.
# (a) An env with a leading empty egress assignment and a trailing non-empty
# one must be rejected in --env-only mode. The fixture lives in the existing
# temp tree; only the error CODE is matched and never the fixture's values.
dup_egress_env="$tmp/i58_dup_egress.env"
printf '%s\n' \
  'JARVIS_MCP_GW_BASE_DIR=/opt/jarvis' \
  'JARVIS_MCP_GW_EGRESS_ALLOWLIST=' \
  'JARVIS_MCP_GW_EGRESS_ALLOWLIST=example.com' \
  > "$dup_egress_env"
out=$(JARVIS_DRIFT_ENV="$dup_egress_env" \
      bash "$repo/infra/scripts/check-deploy-drift.sh" --env-only 2>&1)
code=$?
if [ "$code" -eq 2 ] && printf '%s' "$out" | grep -q 'E_ENV_DUP_EGRESS_ALLOWLIST'; then
  echo "  PASS: drift --env-only rejects duplicate egress assignment"
  ok=$((ok+1))
else
  echo "  FAIL: drift --env-only accepted duplicate egress (exit=$code)"
  fail=$((fail+1))
fi

# (b) A live rollback needs root and mutates /etc, /opt and systemd, so the
# non-root CI gate cannot exercise it safely. Use a structural guard on the
# rollback body instead: the missing-.bak branch must install the repo deny-all
# template as root:root 0600, and the deployed env must never be deleted
# (deleting it would fall back to the unit's permissive literal default).
deploy_script="$repo/infra/scripts/deploy-mcp-gw-safe.sh"
rollback_body="$(sed -n '/^rollback()/,/^}/p' "$deploy_script")"
if printf '%s\n' "$rollback_body" \
     | grep -Fq "install -o root -g root -m 0600 \"\$repo_env\" \"\$env_target\"" \
   && printf '%s\n' "$rollback_body" \
     | grep -Fq "if [ -f \"\$env_target.bak-\$ts\" ]" \
   && ! printf '%s\n' "$rollback_body" | grep -Fq "rm -f \"\$env_target\""; then
  echo "  PASS: rollback restores deny-all env template when .bak is missing"
  ok=$((ok+1))
else
  echo "  FAIL: rollback lost fail-closed env restore (structural guard)"
  fail=$((fail+1))
fi

# (c) Exercise the real post-rollback drift gate with a stubbed drift command:
# extract the case block from rollback() and substitute a return code for the
# drift script, so no service or file is touched. DANGER (2) and any unknown
# code must both map to exit 3 instead of being swallowed as a warning.
drift_gate="$(printf '%s\n' "$rollback_body" \
  | sed -n '/^  bash "[$]repo\/infra\/scripts\/check-deploy-drift[.]sh"/,/^  esac/p')"
gate_with_rc() { # stub_rc
  local stub_rc="$1"
  (
    # Override `bash` only inside this subshell so the extracted drift call
    # returns the stub code; no real script or service is invoked.
    # Invoked indirectly by the eval below.
    # shellcheck disable=SC2329
    bash() { return "$stub_rc"; }
    eval "$drift_gate"
  )
}
gate_with_rc 2 >/dev/null 2>&1
danger_rc=$?
gate_with_rc 99 >/dev/null 2>&1
unknown_rc=$?
if [ -n "$drift_gate" ] && [ "$danger_rc" -eq 3 ] && [ "$unknown_rc" -eq 3 ]; then
  echo "  PASS: rollback drift gate forces exit 3 on DANGER"
  ok=$((ok+1))
else
  echo "  FAIL: rollback drift gate regressed (DANGER must exit 3)"
  fail=$((fail+1))
fi

# I65-A (Round 61 / migration 005): a terminal dead-letter decision must retain
# the row for audit. Guard the production UPDATE directly: reverting to remove()
# or a DELETE would destroy the audit trail while the behavioural mock tests
# would still pass. A second guard keeps the SELECT gate and its use in `list`.
builder="$repo/rust/jarvis-context/src/builder.rs"
dl_body="$(sed -n '/^    async fn dead_letter(&self, id: Uuid, dead_lettered_at: DateTime<Utc>)/,/^    }/p' "$builder")"
# The SQL placeholder is matched literally.
# shellcheck disable=SC2016
if printf '%s\n' "$dl_body" | grep -Fq 'UPDATE pending_embeddings' \
   && printf '%s\n' "$dl_body" | grep -Fq 'dead_lettered_at = $2' \
   && printf '%s\n' "$dl_body" | grep -Fq "vector_state = 'dead_lettered'" \
   && ! printf '%s\n' "$dl_body" | grep -Fq 'DELETE FROM'; then
  echo "  PASS: PgPendingStore::dead_letter retains rows with an UPDATE"
  ok=$((ok+1))
else
  echo "  FAIL: PgPendingStore::dead_letter lost its retention UPDATE"
  fail=$((fail+1))
fi

list_sql="$(sed -n '/^const LIST_PENDING_SQL: &str = /,/ORDER BY created_at ASC LIMIT/p' "$builder")"
pg_list_body="$(sed -n '/^    async fn list(&self, limit: usize) -> Result<Vec<PendingEmbedding>>/,/^    }/p' "$builder")"
if printf '%s\n' "$list_sql" | grep -Fq 'dead_lettered_at IS NULL' \
   && printf '%s\n' "$pg_list_body" | grep -Fq 'LIST_PENDING_SQL'; then
  echo "  PASS: pending selection keeps the dead-letter exclusion gate"
  ok=$((ok+1))
else
  echo "  FAIL: pending selection lost the dead-letter exclusion gate"
  fail=$((fail+1))
fi

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
