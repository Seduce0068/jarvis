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

# I75-A (a): the casbin_policy.csv check must honour JARVIS_DRIFT_POLICY_DIR.
# A stale casbin file in the overridden dir must surface as a policy DIFF;
# before the fix the hardcoded /etc/jarvis/policy path made the override a
# no-op and this drift went undetected. The casbin check reports under the
# "policy" label, so match that line specifically.
casbin_drift="$tmp/casbin_drift"
mkdir -p "$casbin_drift/policy"
cp "$repo/infra/config/policy/policies.cedar" "$casbin_drift/policy/"
cp "$repo/infra/config/policy/cedar_schema.cedarschema" "$casbin_drift/policy/"
printf 'g, stale, role:observer\n' > "$casbin_drift/policy/casbin_policy.csv"
out=$(JARVIS_DRIFT_POLICY_DIR="$casbin_drift/policy" \
      JARVIS_DRIFT_PII="$repo/infra/config/pii_patterns.toml" \
      JARVIS_DRIFT_ALLOWLIST=/nonexistent \
      bash "$repo/infra/scripts/check-deploy-drift.sh" 2>&1)
if printf '%s' "$out" | grep -qE '^policy[[:space:]]+DIFF'; then
  echo "  PASS: drift detects stale casbin via JARVIS_DRIFT_POLICY_DIR"
  ok=$((ok+1))
else
  echo "  FAIL: drift ignored JARVIS_DRIFT_POLICY_DIR for casbin"
  fail=$((fail+1))
fi

# I75-A (b): web.fetch requires_approval must be resolved from the TOML table,
# not by line distance. Build a fixture whose web.fetch entry carries padding
# comments plus an extra key so name->requires_approval exceeds the old
# grep -A8 window, and set the value to false; tomllib must still flag DANGER.
webfetch_fixture="$tmp/webfetch_comment_allowlist.toml"
if python3 - "$repo/infra/config/mcp_allowlist.toml" "$webfetch_fixture" <<'PY'
import sys

src, dst = sys.argv[1:3]
with open(src, encoding='utf-8') as fh:
    lines = fh.readlines()
start = next((i for i, l in enumerate(lines)
              if l.strip() == 'name = "web.fetch"'), None)
if start is None:
    raise SystemExit('web.fetch entry not found')
end = next((j for j in range(start + 1, len(lines))
            if lines[j].strip().startswith('[[tools]]')), len(lines))
approval = next((k for k in range(start, end)
                 if 'requires_approval' in lines[k]), None)
if approval is None:
    raise SystemExit('requires_approval not found in web.fetch entry')
lines[approval] = 'requires_approval = false\n'
padding = [
    '# I75-A regression: padding comments push the approval key\n',
    '# beyond the old grep -A8 window; tomllib must still see false.\n',
    '# padding comment 3\n',
    '# padding comment 4\n',
    'extra_padding_key = "still inside the web.fetch table"\n',
]
lines[start + 1:start + 1] = padding
with open(dst, 'w', encoding='utf-8') as fh:
    fh.writelines(lines)
PY
then
  out=$(JARVIS_DRIFT_ALLOWLIST="$webfetch_fixture" \
        bash "$repo/infra/scripts/check-deploy-drift.sh" 2>&1)
  if printf '%s' "$out" | grep -q 'web.fetch approval.*DANGER'; then
    echo "  PASS: drift detects web.fetch requires_approval=false past grep window"
    ok=$((ok+1))
  else
    echo "  FAIL: drift missed web.fetch false with padding comments"
    fail=$((fail+1))
  fi
else
  echo "  FAIL: could not prepare web.fetch padding fixture"
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
elif mode == 'empty_suffix':
    # The empty hash suffix is a FILENAME defect, not a JSON defect: a gate
    # that guards it only with a truthiness check would silently skip the file.
    # Keep the content pristine so only the filename gate can reject this copy;
    # the caller supplies a dst whose basename is exactly "test-run-.json".
    # The re-dump below still runs so this mode follows the same write path.
    pass
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
    # Keep the original contract keywords: replacing notes wholesale would
    # make SCENARIO_STEP_UNCOVERED fire first and mask the scenario/skip gate
    # this tamper mode is meant to isolate.
    notes = data['notes']
    notes = re.sub(r'\d+([ \t]*件)', str(short) + r'\1', notes)
    notes = re.sub(r'\d+([ \t]*passed)', str(short) + r'\1', notes)
    if mode == 'skipped_inflated':
        notes = re.sub(r'\d+([ \t]*ignored)', str(skipped) + r'\1', notes)
        if 'ignored' not in notes:
            notes += ' ' + str(skipped) + ' ignored。'
    data['notes'] = notes
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
              notes_count_conflict notes_passed_conflict req_removed empty_suffix; do
    # empty_suffix is a filename defect: write to a copy named test-run-.json
    # and restore the pristine JSON first so no earlier tamper's error masks
    # the HASH_FILENAME_MISSING gate this mode isolates.
    if [ "$mode" = empty_suffix ]; then
      cp "$source_json" "$evidence_copy"
      mode_copy="$evidence_tmp/evidence/test-run-.json"
      rm -f -- "$mode_copy"
    else
      mode_copy="$evidence_copy"
    fi
    if ! tamper_evidence "$source_json" "$mode_copy" "$mode"; then
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
      # An empty filename suffix (test-run-.json) must be rejected with the
      # dedicated code, never silently skipped nor reported as a mismatch.
      empty_suffix) pattern='HASH_FILENAME_MISSING' ;;
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

# Evidence file names embed a per-run hash and change whenever the evidence
# set is regenerated. Resolve the copied JSON through test_basis_ref.spec_path
# instead of pinning a literal test-run-<hash>.json name, so the negative gates
# below keep exercising the real file after regeneration.
evidence_for_spec() { # evidence_dir spec_basename -> matching path on stdout
  local dir="$1" spec="$2" candidate
  for candidate in "$dir"/test-run-*.json; do
    [ -f "$candidate" ] || continue
    if python3 - "$candidate" "$spec" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as fh:
    data = json.load(fh)
path = data.get("test_basis_ref", {}).get("spec_path", "")
raise SystemExit(0 if path == "specs/" + sys.argv[2] else 1)
PY
    then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

# I65-C: in strict mode a contract keyword present in a spec but absent from
# the matching notes, or outnumbered by duplicated spec steps, must fail with
# SCENARIO_STEP_UNCOVERED. Use COPIED specs
# and evidence so the real files are untouched; the gate evaluates contract
# coverage before the hash gate, so the drifted sha256 cannot mask the
# targeted code.
step_tmp="$tmp/scenario_step_negative"
mkdir -p "$step_tmp/ci/scripts" "$step_tmp/evidence" "$step_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$step_tmp/ci/scripts/check-evidence.sh"
cp "$repo/specs/index.md" "$step_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$step_tmp/evidence/index.md"
cp "$repo/todo.md" "$step_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$step_tmp/evidence/"
cp "$repo"/specs/test-*.md "$step_tmp/specs/"
# Duplicate the existing 415 contract step in TC-C and strip 415 from the
# COPIED evidence notes, reproducing an out-of-sync (notes 未同期) run. A
# spec-only duplication is now rejected too (next case) because note matches
# are counted against the spec count. The Scenario count is unchanged here, so
# only the contract-coverage gate can reject this copy.
# I65-C tamper the copied specs/test-context.md, so select the evidence that
# attests that spec rather than a hard-coded regenerated file name.
step_evidence=$(evidence_for_spec "$step_tmp/evidence" test-context.md) || step_evidence=""
if [ -z "$step_evidence" ]; then
  echo "  FAIL: no copied evidence JSON attests specs/test-context.md"
  fail=$((fail+1))
elif python3 - "$step_tmp/specs/test-context.md" "$step_evidence" <<'PY'
import json, sys

spec_path, evidence_path = sys.argv[1:3]
with open(spec_path, encoding='utf-8') as fh:
    text = fh.read()
needle = '  Then 空本文の 415 を返す（ヘッダ付与を促す）\n'
if text.count(needle) != 1:
    raise SystemExit('415 contract step anchor not unique')
with open(spec_path, 'w', encoding='utf-8') as fh:
    fh.write(text.replace(needle, needle + needle, 1))
# Only the copied JSON is edited; the repository evidence stays pristine.
with open(evidence_path, encoding='utf-8') as fh:
    data = json.load(fh)
notes = data.get('notes', '')
if not isinstance(notes, str) or '415' not in notes:
    raise SystemExit('415 keyword not found in copied evidence notes')
data['notes'] = notes.replace('415', '')
with open(evidence_path, 'w', encoding='utf-8') as fh:
    json.dump(data, fh, ensure_ascii=False)
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

# I68-A: duplicating a contract step in the SPEC while the notes still claim
# the keyword once must fail closed. The old gate iterated Counter keys and
# discarded the count, so detected=415:2 with one note mention passed. Use a
# fresh copy so the "notes keyword removed" case above stays intact.
count_tmp="$tmp/scenario_step_count"
mkdir -p "$count_tmp/ci/scripts" "$count_tmp/evidence" "$count_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$count_tmp/ci/scripts/check-evidence.sh"
cp "$repo/specs/index.md" "$count_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$count_tmp/evidence/index.md"
cp "$repo/todo.md" "$count_tmp/todo.md"
cp "$repo"/evidence/test-run-*.json "$count_tmp/evidence/"
cp "$repo"/specs/test-*.md "$count_tmp/specs/"
# Same dynamic selection for the fresh copy used by the count case.
count_evidence=$(evidence_for_spec "$count_tmp/evidence" test-context.md) || count_evidence=""
if [ -z "$count_evidence" ]; then
  echo "  FAIL: no copied evidence JSON attests specs/test-context.md (count case)"
  fail=$((fail+1))
elif python3 - "$count_tmp/specs/test-context.md" "$count_evidence" <<'PY'
import json, sys

spec_path, evidence_path = sys.argv[1:3]
with open(spec_path, encoding='utf-8') as fh:
    text = fh.read()
needle = '  Then 空本文の 415 を返す（ヘッダ付与を促す）\n'
if text.count(needle) != 1:
    raise SystemExit('415 contract step anchor not unique')
with open(spec_path, 'w', encoding='utf-8') as fh:
    fh.write(text.replace(needle, needle + needle, 1))
# Notes are left untouched on purpose: assert the precondition that the
# copied evidence claims 415 exactly once, so two spec steps outrank it.
with open(evidence_path, encoding='utf-8') as fh:
    data = json.load(fh)
notes = data.get('notes', '')
if not isinstance(notes, str) or notes.count('415') != 1:
    raise SystemExit('expected exactly one 415 mention in copied evidence notes')
PY
then
  out=$(bash "$count_tmp/ci/scripts/check-evidence.sh" 2>&1); code=$?
  if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'SCENARIO_STEP_UNCOVERED'; then
    echo "  PASS: check-evidence.sh rejects a duplicated contract step with one note mention"
    ok=$((ok+1))
  else
    echo "  FAIL: check-evidence.sh accepted a duplicated contract step (exit $code)"
    printf '%s\n' "$out" | head -3 >&2
    fail=$((fail+1))
  fi
else
  echo "  FAIL: could not prepare contract-step count tamper"
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

# R2 bounded-placeholder regression. The old unbounded PLACEHOLDER.search()
# matched `example`/`placeholder` inside an otherwise value-like token and
# silently downgraded it to "placeholder". All probes below are synthetic;
# on failure only the exit code is reported, never the scanner output.
scan_bounded="$tmp/secret_scan_bounded"
mkdir -p "$scan_bounded"
bounded_probe_example='A1B2C3exampleXYZ'
bounded_probe_placeholder='A1B2C3placeholderXYZ'
# Key names are held in variables so this tracked script never contains a
# literal `KEY = value` line that the scanner would classify itself. The
# assignment lines are generated at runtime inside the throwaway repo only.
probe_key_password='PASSWORD'
probe_key_api='API_KEY'
probe_key_secret='SECRET'
probe_key_access='ACCESS_TOKEN'
probe_key_client='CLIENT_SECRET'
probe_key_bws='BWS_ACCESS_TOKEN'
if ! (cd "$scan_bounded" \
      && git init -q >/dev/null 2>&1 \
      && printf '%s = %s\n%s = %s\n' \
           "$probe_key_password" "$bounded_probe_example" \
           "$probe_key_api" "$bounded_probe_placeholder" > leaky.env \
      && git add -f leaky.env >/dev/null 2>&1); then
  echo "  FAIL: could not prepare bounded-placeholder git repo for secret scanner test"
  fail=$((fail+1))
else
  # Copy the scanner only AFTER git add so its own regex literals stay
  # untracked and are not scanned as tracked content.
  cp "$repo/ci/scripts/secret_scan.py" "$scan_bounded/secret_scan.py"
  out=$(cd "$scan_bounded" && python3 secret_scan.py 2>&1); code=$?
  leaked=0
  for probe in "$bounded_probe_example" "$bounded_probe_placeholder"; do
    if printf '%s' "$out" | grep -Fq -- "$probe"; then leaked=1; fi
  done
  if [ "$code" -eq 1 ] \
     && printf '%s' "$out" | grep -q 'VALUE-LIKE' \
     && [ "$leaked" -eq 0 ]; then
    echo "  PASS: secret_scan.py flags embedded placeholder words as VALUE-LIKE"
    ok=$((ok+1))
  else
    echo "  FAIL: secret_scan.py accepted an embedded placeholder word (exit=$code, output withheld)"
    fail=$((fail+1))
  fi
fi

# R2 table-driven coverage: further embedded placeholder words must all be
# VALUE-LIKE. Count the finding lines instead of echoing any probe value.
scan_bounded_table="$tmp/secret_scan_bounded_table"
mkdir -p "$scan_bounded_table"
bounded_probe_dummy='A1B2C3dummyXYZ'
bounded_probe_none='A1B2C3noneXYZ'
bounded_probe_template='A1B2C3templateXYZ'
if ! (cd "$scan_bounded_table" \
      && git init -q >/dev/null 2>&1 \
      && printf '%s = %s\n%s = %s\n%s = %s\n' \
           "$probe_key_secret" "$bounded_probe_dummy" \
           "$probe_key_access" "$bounded_probe_none" \
           "$probe_key_client" "$bounded_probe_template" > leaky.env \
      && git add -f leaky.env >/dev/null 2>&1); then
  echo "  FAIL: could not prepare table-driven bounded-placeholder git repo"
  fail=$((fail+1))
else
  cp "$repo/ci/scripts/secret_scan.py" "$scan_bounded_table/secret_scan.py"
  out=$(cd "$scan_bounded_table" && python3 secret_scan.py 2>&1); code=$?
  vl_count=$(printf '%s' "$out" | grep -c -- '-> VALUE-LIKE' || true)
  leaked=0
  for probe in "$bounded_probe_dummy" "$bounded_probe_none" "$bounded_probe_template"; do
    if printf '%s' "$out" | grep -Fq -- "$probe"; then leaked=1; fi
  done
  if [ "$code" -eq 1 ] \
     && [ "$vl_count" -eq 3 ] \
     && [ "$leaked" -eq 0 ]; then
    echo "  PASS: secret_scan.py flags all table-driven embedded placeholder words"
    ok=$((ok+1))
  else
    echo "  FAIL: secret_scan.py missed a table-driven embedded placeholder word (exit=$code, output withheld)"
    fail=$((fail+1))
  fi
fi

# R2 positive baseline: known placeholder shapes must stay classified as
# placeholder, guarding against flipping the existing baseline cases.
scan_bounded_baseline="$tmp/secret_scan_bounded_baseline"
mkdir -p "$scan_bounded_baseline"
# shellcheck disable=SC2016
# ${VAR} is scanned as a literal placeholder shape, not expanded here.
baseline_probes=('dummy_value' 'user@example.com' '<your-token>' '${VAR}' 'op://vault/item/field')
if ! (cd "$scan_bounded_baseline" \
      && git init -q >/dev/null 2>&1 \
      && printf '%s = %s\n%s = %s\n%s = %s\n%s = %s\n%s = %s\n' \
           "$probe_key_password" "${baseline_probes[0]}" \
           "$probe_key_api" "${baseline_probes[1]}" \
           "$probe_key_secret" "${baseline_probes[2]}" \
           "$probe_key_access" "${baseline_probes[3]}" \
           "$probe_key_bws" "${baseline_probes[4]}" > leaky.env \
      && git add -f leaky.env >/dev/null 2>&1); then
  echo "  FAIL: could not prepare placeholder-baseline git repo"
  fail=$((fail+1))
else
  cp "$repo/ci/scripts/secret_scan.py" "$scan_bounded_baseline/secret_scan.py"
  out=$(cd "$scan_bounded_baseline" && python3 secret_scan.py 2>&1); code=$?
  leaked=0
  for probe in "${baseline_probes[@]}"; do
    if printf '%s' "$out" | grep -Fq -- "$probe"; then leaked=1; fi
  done
  if [ "$code" -eq 0 ] \
     && ! printf '%s' "$out" | grep -q 'VALUE-LIKE' \
     && printf '%s' "$out" | grep -q 'no value-like credentials found' \
     && [ "$leaked" -eq 0 ]; then
    echo "  PASS: secret_scan.py keeps known placeholders classified as placeholder"
    ok=$((ok+1))
  else
    echo "  FAIL: secret_scan.py reclassified a known placeholder (exit=$code, output withheld)"
    fail=$((fail+1))
  fi
fi

# R2 direct assertions: import the scanner module (without running main) and
# check both sides of the boundary rule. Messages contain no probe value and
# any failing output is discarded rather than echoed.
if python3 - "$repo/ci/scripts/secret_scan.py" 2>/dev/null <<'PY'
import importlib.util
import sys

spec = importlib.util.spec_from_file_location('secret_scan_under_test', sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
assert module.classify('A1B2C3exampleXYZ') == 'VALUE-LIKE', 'embedded placeholder must be VALUE-LIKE'
assert module.classify('dummy_value') == 'placeholder', 'known placeholder must stay placeholder'
PY
then
  echo "  PASS: secret_scan.py classify() enforces the bounded placeholder rule"
  ok=$((ok+1))
else
  echo "  FAIL: secret_scan.py classify() failed direct boundary assertions (output withheld)"
  fail=$((fail+1))
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

# I79-C: an empty tracked-file set must fail closed. A repo with only
# `git init` (no `git add`) makes `git ls-files` return nothing; the scanner
# used to print "scanned 0 tracked files ... OK" and exit 0. Assert exit 1,
# the fixed stderr error, and that no OK/summary report is emitted.
scan_empty="$tmp/secret_scan_empty"
mkdir -p "$scan_empty"
if ! (cd "$scan_empty" && git init -q >/dev/null 2>&1); then
  echo "  FAIL: could not initialize empty git repo for secret scanner test"
  fail=$((fail+1))
else
  # Copy the scanner AFTER git init so it stays untracked: the tracked set
  # remains empty (its own regex literals are legitimate VALUE-LIKE shapes).
  cp "$repo/ci/scripts/secret_scan.py" "$scan_empty/secret_scan.py"
  out=$(cd "$scan_empty" && python3 secret_scan.py 2>&1); code=$?
  if [ "$code" -eq 1 ] \
     && printf '%s' "$out" | grep -qi 'no tracked files' \
     && ! printf '%s' "$out" | grep -q 'scanned 0 tracked files' \
     && ! printf '%s' "$out" | grep -q 'no value-like credentials found'; then
    echo "  PASS: secret_scan.py fails closed on an empty tracked-file set"
    ok=$((ok+1))
  else
    # Never echo raw scanner output: show only the exit code.
    echo "  FAIL: secret_scan.py accepted an empty tracked-file set (exit=$code)"
    fail=$((fail+1))
  fi
fi

# I81-B: the scanner must anchor its tracked-file listing at the repo root
# derived from its own path, not at the caller's CWD. The old CWD-relative
# `git ls-files` scanned only the subdirectory's files and printed OK even
# though a root-level credential was tracked. Run from rust/ and require both
# the root-level finding and the full two-file scan count.
scan_subdir="$tmp/secret_scan_subdir"
mkdir -p "$scan_subdir/rust"
if ! (cd "$scan_subdir" \
      && git init -q >/dev/null 2>&1 \
      && printf 'BWS_ACCESS_TOKEN = %s\n' "$dummy_value" > leaky.env \
      && printf 'fn main() {}\n' > rust/clean.rs \
      && git add -f leaky.env rust/clean.rs >/dev/null 2>&1); then
  echo "  FAIL: could not prepare subdirectory git repo for secret scanner test"
  fail=$((fail+1))
else
  # Copy the scanner AFTER git add so it stays untracked (its own regex
  # literals are legitimate VALUE-LIKE shapes).
  cp "$repo/ci/scripts/secret_scan.py" "$scan_subdir/secret_scan.py"
  out=$(cd "$scan_subdir/rust" && python3 ../secret_scan.py 2>&1); code=$?
  if [ "$code" -eq 1 ] \
     && printf '%s' "$out" | grep -q 'VALUE-LIKE' \
     && printf '%s' "$out" | grep -q 'scanned 2 tracked files' \
     && ! printf '%s' "$out" | grep -q -- "$dummy_value"; then
    echo "  PASS: secret_scan.py scans all tracked files from a subdirectory CWD"
    ok=$((ok+1))
  else
    # Never echo raw scanner output: it may contain the dummy value.
    echo "  FAIL: secret_scan.py missed a root-level credential from rust/ (exit=$code, output withheld)"
    fail=$((fail+1))
  fi
fi

# I81-B (franken CWD): invoke the scanner from an UNRELATED git repo whose own
# tracked file is harmless. The old CWD-relative listing would scan that file
# and wrongly report OK; the fixed scanner uses the repo root derived from its
# own path and must still reject the credential in the scanner's repo.
scan_franken="$tmp/secret_scan_franken"
mkdir -p "$scan_franken/cwd_repo"
if ! (cd "$scan_franken" \
      && git init -q >/dev/null 2>&1 \
      && printf 'BWS_ACCESS_TOKEN = %s\n' "$dummy_value" > leaky.env \
      && git add -f leaky.env >/dev/null 2>&1 \
      && cd cwd_repo \
      && git init -q >/dev/null 2>&1 \
      && printf 'harmless = true\n' > clean.env \
      && git add -f clean.env >/dev/null 2>&1); then
  echo "  FAIL: could not prepare franken-CWD repos for secret scanner test"
  fail=$((fail+1))
else
  cp "$repo/ci/scripts/secret_scan.py" "$scan_franken/secret_scan.py"
  out=$(cd "$scan_franken/cwd_repo" && python3 ../secret_scan.py 2>&1); code=$?
  if [ "$code" -eq 1 ] \
     && printf '%s' "$out" | grep -q 'VALUE-LIKE' \
     && printf '%s' "$out" | grep -q 'scanned 1 tracked files' \
     && ! printf '%s' "$out" | grep -q -- "$dummy_value"; then
    echo "  PASS: secret_scan.py ignores a franken CWD and scans its own repo"
    ok=$((ok+1))
  else
    echo "  FAIL: secret_scan.py reported false OK from a franken CWD (exit=$code, output withheld)"
    fail=$((fail+1))
  fi
fi

# Static guard: keep the root-anchored lookup and the fixed-vocabulary index
# cross-check in the scanner source, and make sure no bare CWD-relative
# `git ls-files` call creeps back in.
scanner_src="$repo/ci/scripts/secret_scan.py"
if grep -Fq '"git", "-C", script_dir, "rev-parse"' "$scanner_src" \
   && grep -Fq '"git", "-C", root, "ls-files"' "$scanner_src" \
   && grep -Fq 'scan set/index mismatch' "$scanner_src" \
   && ! grep -Fq '["git", "ls-files"]' "$scanner_src"; then
  echo "  PASS: secret_scan.py is root-anchored with an index-count cross-check"
  ok=$((ok+1))
else
  echo "  FAIL: secret_scan.py lost the root anchor or the index cross-check"
  fail=$((fail+1))
fi

# R2 static guard: the PLACEHOLDER decision must be boundary-aware. Accept a
# dedicated helper (e.g. is_placeholder) or an explicit bounded-regex / full-
# match / token-split form; the unbounded substring search is what allowed an
# embedded `example` to be downgraded to placeholder.
if grep -Eq 'def[[:space:]]+[A-Za-z_]*placeholder' "$scanner_src" \
   || grep -Fq '(?<!' "$scanner_src" \
   || grep -Fq '(?!' "$scanner_src" \
   || grep -Fq 'fullmatch' "$scanner_src" \
   || grep -Fq '[^A-Za-z0-9]' "$scanner_src" \
   || grep -Fq 're.escape' "$scanner_src"; then
  echo "  PASS: secret_scan.py has a boundary-aware placeholder determination"
  ok=$((ok+1))
else
  echo "  FAIL: secret_scan.py lost the boundary-aware placeholder determination"
  fail=$((fail+1))
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

# I97-A: egress must follow Rust's host-pattern rules, not the old loose
# character class. Every value below passes the old regex but must be rejected
# with the FIXED code only (the gate never prints the value).
long_label="$(python3 -c 'print("a" * 64)')"
too_long_host="$(python3 -c 'print(".".join(["a" * 63, "a" * 63, "a" * 63, "a" * 62]))')"
strict_egress_env="$tmp/i97_invalid_egress.env"
i97_invalid_values=(
  'example..com'          # empty label
  '-bad.example.com'      # leading hyphen
  'bad-.example.com'      # trailing hyphen
  'www.*.example.com'     # wildcard only allowed as a leading "*."
  '*.'                    # empty host after the wildcard prefix
  'example.com.'          # trailing dot => empty label
  'example. com'          # whitespace inside a label
  "${long_label}.com"     # label longer than 63
  "$too_long_host"        # host longer than 253
)
i97_bad=0
for value in "${i97_invalid_values[@]}"; do
  printf '%s\n' \
    'JARVIS_MCP_GW_BASE_DIR=/opt/jarvis' \
    "JARVIS_MCP_GW_EGRESS_ALLOWLIST=$value" \
    > "$strict_egress_env"
  out=$(JARVIS_DRIFT_ENV="$strict_egress_env" \
        bash "$repo/infra/scripts/check-deploy-drift.sh" --env-only 2>&1)
  code=$?
  if [ "$code" -ne 2 ] \
     || ! printf '%s' "$out" | grep -q 'E_ENV_INVALID_EGRESS_ALLOWLIST' \
     || printf '%s' "$out" | grep -qF -- "$value"; then
    i97_bad=$((i97_bad+1))
  fi
done
if [ "$i97_bad" -eq 0 ]; then
  echo "  PASS: drift --env-only rejects malformed egress patterns (code only)"
  ok=$((ok+1))
else
  echo "  FAIL: drift --env-only accepted a malformed egress pattern"
  fail=$((fail+1))
fi
# Positive control: per-entry trim, a leading "*.", a trailing comma and a
# 253-char host stay valid; deny-all (empty value) remains valid too.
max_host="$(python3 -c 'print(".".join(["a" * 63, "a" * 63, "a" * 63, "a" * 61]))')"
{
  printf '%s\n' 'JARVIS_MCP_GW_BASE_DIR=/opt/jarvis'
  printf '%s\n' "JARVIS_MCP_GW_EGRESS_ALLOWLIST= example.com , *.example.org ,${max_host}"
} > "$strict_egress_env"
out=$(JARVIS_DRIFT_ENV="$strict_egress_env" \
      bash "$repo/infra/scripts/check-deploy-drift.sh" --env-only 2>&1)
code=$?
if [ "$code" -eq 0 ] && printf '%s' "$out" | grep -q 'mcp-gw env.*OK'; then
  echo "  PASS: drift --env-only accepts Rust-valid egress patterns"
  ok=$((ok+1))
else
  echo "  FAIL: drift --env-only rejected a Rust-valid egress pattern (exit=$code)"
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

# I74-C: mcp_allowlist.toml is written by THREE paths. They must all resolve
# their owner/mode policy from the ONE table in configs.sh. These are static
# guards only: nothing here installs a file or changes a permission.
perm_configs="$repo/infra/scripts/restore-local.d/configs.sh"
perm_build="$repo/infra/scripts/build-selfhost.sh"
perm_deploy="$repo/infra/scripts/deploy-mcp-gw-safe.sh"

if grep -Fq 'declare -gA EXPECTED_CONFIG_MODE' "$perm_configs" \
   && grep -Fq 'expected_config_mode()' "$perm_configs" \
   && grep -Fq 'install_expected_config()' "$perm_configs" \
   && grep -Fq '[mcp_allowlist.toml]' "$perm_configs" \
   && grep -Fq 'ALL THREE mcp_allowlist.toml write paths' "$perm_configs"; then
  echo "  PASS: shared EXPECTED_CONFIG_MODE table documents all 3 paths"
  ok=$((ok+1))
else
  echo "  FAIL: shared owner/mode table missing from configs.sh"
  fail=$((fail+1))
fi

# Path 1 (build): sources the shared module and uses it for the allowlist.
# The ${...} fragments are matched literally in the sourced scripts.
# shellcheck disable=SC2016
if grep -Fq 'source "${SELFHOST_SCRIPT_DIR}/restore-local.d/configs.sh"' "$perm_build" \
   && grep -Fq 'expected_config_mode "${top_config}"' "$perm_build" \
   && grep -Fq 'install_expected_config "${top_mode}"' "$perm_build"; then
  echo "  PASS: build path resolves allowlist mode from the shared table"
  ok=$((ok+1))
else
  echo "  FAIL: build path bypassed the shared owner/mode table"
  fail=$((fail+1))
fi

# Path 2 (restore): the .bak and the restored file use the shared helper.
# shellcheck disable=SC2016
if grep -Fq 'install_expected_config "$mode" "$dest" "${dest}.bak-${ts}"' "$perm_configs" \
   && grep -Fq 'install_expected_config "$mode" "$tmp_dir/$name" "$dest"' "$perm_configs"; then
  echo "  PASS: restore path uses shared helper for backup and restore writes"
  ok=$((ok+1))
else
  echo "  FAIL: restore path lost shared-helper writes"
  fail=$((fail+1))
fi

# Path 3 (deploy): must source the same module, resolve both modes from the
# table, route placement AND every rollback restore through the helper, and
# retain no hardcoded root:root 0644 allowlist/unit install.
# shellcheck disable=SC2016
if grep -Fq 'source "$repo/infra/scripts/restore-local.d/configs.sh"' "$perm_deploy" \
   && grep -Fq 'expected_config_mode mcp_allowlist.toml' "$perm_deploy" \
   && grep -Fq 'expected_config_mode systemd/jarvis-mcp-gw.service' "$perm_deploy" \
   && grep -Fq 'install_expected_config "$allowlist_mode" "$repo_allowlist" "$allowlist"' "$perm_deploy" \
   && grep -Fq 'install_expected_config "$allowlist_mode" "$allowlist.bak-step3-$ts" "$allowlist"' "$perm_deploy" \
   && grep -Fq 'install_expected_config "$allowlist_mode" "$allowlist.bak-step2-$ts" "$allowlist"' "$perm_deploy" \
   && grep -Fq 'install_expected_config "$unit_mode" "$unit.bak-step3-$ts" "$unit"' "$perm_deploy" \
   && ! grep -Fq 'install -o root -g root -m 0644 "$repo_allowlist" "$allowlist"' "$perm_deploy" \
   && ! grep -Fq 'install -o root -g root -m 0644 "$allowlist.bak-' "$perm_deploy" \
   && ! grep -Fq 'install -o root -g root -m 0644 "$unit.bak-' "$perm_deploy" \
   && ! grep -Fq 'install -o root -g root -m 0644 "$repo_unit" "$unit"' "$perm_deploy"; then
  echo "  PASS: deploy path shares the table for placement and rollback restores"
  ok=$((ok+1))
else
  echo "  FAIL: deploy path still hardcodes allowlist/unit permissions"
  fail=$((fail+1))
fi

# I74-C (b): fail-closed when neither root nor sudo is available. The legacy
# mode-only fallback is allowed ONLY by the explicit isolated-test opt-in.
# Owner/mode arguments are observed with shell-function stubs inside a
# subshell: no real install/chown/sudo runs and no permission is changed.
perm_probe_dir="$tmp/config_perm_probe"
mkdir -p "$perm_probe_dir/empty_bin"
perm_probe() { # stub_uid with_sudo test_mode -> PROBE_RC / PROBE_ARGS / PROBE_ERR
  local stub_uid="$1" with_sudo="$2" test_mode="$3"
  local capture="$perm_probe_dir/capture.$stub_uid.$with_sudo.$test_mode"
  local errfile="$perm_probe_dir/stderr.$stub_uid.$with_sudo.$test_mode"
  : > "$capture"
  (
    # Intentionally empty PATH: this stub reproduces a host without sudo.
    # shellcheck disable=SC2123,SC2030
    PATH="$perm_probe_dir/empty_bin"
    export PATH
    # shellcheck disable=SC2329
    id() { printf '%s\n' "$stub_uid"; }
    # shellcheck disable=SC2329
    install() { printf 'install %s\n' "$*" >> "$capture"; return 0; }
    if [ "$with_sudo" = 1 ]; then
      # shellcheck disable=SC2329
      sudo() { printf 'sudo %s\n' "$*" >> "$capture"; return 0; }
    else
      unset -f sudo 2>/dev/null || true
    fi
    if [ "$test_mode" = 1 ]; then
      JARVIS_RESTORE_TEST_MODE=1
      export JARVIS_RESTORE_TEST_MODE
    else
      unset JARVIS_RESTORE_TEST_MODE
    fi
    # shellcheck disable=SC1090
    source "$perm_configs"
    install_expected_config 0644 /fake/src /fake/dst
  ) 2>"$errfile"
  PROBE_RC=$?
  PROBE_ARGS="$(< "$capture")"
  PROBE_ERR="$(< "$errfile")"
}

# Explicit owner (-o root -g root) AND mode (-m 0644) must be passed on both
# production paths; this is the "owner も検証" regression for finding (b).
perm_probe 0 0 0
if [ "$PROBE_RC" -eq 0 ] \
   && printf '%s' "$PROBE_ARGS" | grep -Fq 'install -o root -g root -m 0644 -- /fake/src /fake/dst'; then
  echo "  PASS: root path passes explicit root:root owner and 0644 mode"
  ok=$((ok+1))
else
  echo "  FAIL: root path lost explicit owner/mode (rc=$PROBE_RC)"
  fail=$((fail+1))
fi

perm_probe 1000 1 0
if [ "$PROBE_RC" -eq 0 ] \
   && printf '%s' "$PROBE_ARGS" | grep -Fq 'sudo install -o root -g root -m 0644 -- /fake/src /fake/dst'; then
  echo "  PASS: sudo path passes explicit root:root owner and 0644 mode"
  ok=$((ok+1))
else
  echo "  FAIL: sudo path lost explicit owner/mode (rc=$PROBE_RC)"
  fail=$((fail+1))
fi

perm_probe 1000 0 0
if [ "$PROBE_RC" -ne 0 ] \
   && printf '%s' "$PROBE_ERR" | grep -Fq 'JARVIS_RESTORE_TEST_MODE'; then
  echo "  PASS: no root/sudo fails closed without the explicit opt-in"
  ok=$((ok+1))
else
  echo "  FAIL: no root/sudo silently degraded to mode-only install (rc=$PROBE_RC)"
  fail=$((fail+1))
fi

perm_probe 1000 0 1
if [ "$PROBE_RC" -eq 0 ] \
   && printf '%s' "$PROBE_ARGS" | grep -Fq 'install -m 0644 -- /fake/src /fake/dst' \
   && ! printf '%s' "$PROBE_ARGS" | grep -Fq 'root -g root'; then
  echo "  PASS: isolated-test opt-in permits the documented mode-only fallback"
  ok=$((ok+1))
else
  echo "  FAIL: test opt-in did not permit the legacy fallback (rc=$PROBE_RC)"
  fail=$((fail+1))
fi

# I95-C: owner/mode tamper regression for check-deploy-drift.sh. The old drift
# check compared file CONTENT only, so a post-deploy chown/chmod was invisible
# even though EXPECTED_CONFIG_MODE promises root:root. A mock `stat` in a temp
# dir reports tampered metadata for the env fixture only; every drift path is
# redirected into the same temp tree. No real deployed permission is changed,
# and the throwaway tree is removed by the existing EXIT trap on $tmp.
owner_tmp="$tmp/owner_mode_tamper"
mkdir -p "$owner_tmp/bin" "$owner_tmp/policy"
cp "$repo/infra/config/mcp_allowlist.toml" "$owner_tmp/mcp_allowlist.toml"
cp "$repo/infra/config/policy/casbin_policy.csv" "$owner_tmp/policy/"
cp "$repo/infra/config/policy/policies.cedar" "$owner_tmp/policy/"
cp "$repo/infra/config/policy/cedar_schema.cedarschema" "$owner_tmp/policy/"
cp "$repo/infra/config/pii_patterns.toml" "$owner_tmp/pii_patterns.toml"
cp "$repo/infra/systemd/jarvis-mcp-gw.service" "$owner_tmp/jarvis-mcp-gw.service"
# Minimal env that satisfies check_mcp_env (single absolute BASE_DIR, deny-all).
printf '%s\n' \
  'JARVIS_MCP_GW_BASE_DIR=/opt/jarvis' \
  'JARVIS_MCP_GW_EGRESS_ALLOWLIST=' \
  > "$owner_tmp/jarvis-mcp-gw.env"
# Mock stat echoes fixed metadata by file class. OWNER_TAMPER=1 flips only the
# env to a non-root owner + 0666; the control run reports the expected values.
cat > "$owner_tmp/bin/stat" <<'MOCK'
#!/usr/bin/env bash
for last in "$@"; do :; done
case "$last" in
  *.env)
    if [ "${OWNER_TAMPER:-0}" = 1 ]; then
      printf '%s\n' '1000:1000 666'
    else
      printf '%s\n' '0:0 600'
    fi
    ;;
  *bin/jarvis-mcp-gw) printf '%s\n' '0:0 755' ;;
  *) printf '%s\n' '0:0 644' ;;
esac
MOCK
# Only the throwaway mock is made executable; no deployed file is touched.
chmod 0755 -- "$owner_tmp/bin/stat"

drift_owner_run() { # tamper_flag -> drift output (stdout+stderr); status passes through
  local tamper="$1"
  # PATH is intentionally scoped to this command invocation.
  # shellcheck disable=SC2031
  PATH="$owner_tmp/bin:$PATH" \
  OWNER_TAMPER="$tamper" \
  JARVIS_DRIFT_ALLOWLIST="$owner_tmp/mcp_allowlist.toml" \
  JARVIS_DRIFT_POLICY_DIR="$owner_tmp/policy" \
  JARVIS_DRIFT_PII="$owner_tmp/pii_patterns.toml" \
  JARVIS_DRIFT_UNIT="$owner_tmp/jarvis-mcp-gw.service" \
  JARVIS_DRIFT_ENV="$owner_tmp/jarvis-mcp-gw.env" \
    bash "$repo/infra/scripts/check-deploy-drift.sh" 2>&1
}

control_out="$(drift_owner_run 0)"
if printf '%s' "$control_out" | grep -q 'E_BAD_OWNER_MODE'; then
  echo "  FAIL: drift reported E_BAD_OWNER_MODE for a correct root:root owner/mode"
  fail=$((fail+1))
else
  echo "  PASS: drift accepts correct root:root owner/mode"
  ok=$((ok+1))
fi

tamper_out="$(drift_owner_run 1)"; code=$?
# Fixed vocabulary only: the DANGER lines must contain neither the temp path,
# nor the mocked owner (1000) nor the mocked mode (666).
if [ "$code" -eq 2 ] \
   && printf '%s' "$tamper_out" | grep -q 'mcp-gw env.*E_BAD_OWNER_MODE (code only)' \
   && ! printf '%s' "$tamper_out" | grep -F 'E_BAD_OWNER_MODE' \
        | grep -Eq '1000|666|owner_mode_tamper|jarvis-mcp-gw[.]env'; then
  echo "  PASS: drift flags tampered owner/mode with a fixed code only (exit 2)"
  ok=$((ok+1))
else
  echo "  FAIL: drift missed owner/mode tamper or leaked a value (exit=$code)"
  fail=$((fail+1))
fi

# I101-D: casbin_model.conf is a backup/restore target whose mode is promised by
# configs.sh (root:root 0644), but the drift check used to skip it in BOTH the
# content (cmp) pair and the owner/mode pass. Exercise two tamper modes against
# a throwaway tree only: (1) a missing deployed copy must report
# "MISSING (deployed)", and (2) a wrong MODE must report E_BAD_OWNER_MODE with a
# fixed code (no path/owner/mode leak). The mock `stat` only READS metadata; no
# real deployed file or permission is touched. $tmp is removed by the EXIT trap.
model_src="$repo/infra/config/policy/casbin_model.conf"
if [ -f "$model_src" ]; then
  model_tmp="$tmp/casbin_model_tamper"
  mkdir -p "$model_tmp/bin" "$model_tmp/policy"
  cp "$repo/infra/config/mcp_allowlist.toml" "$model_tmp/mcp_allowlist.toml"
  cp "$repo/infra/config/policy/casbin_policy.csv" \
     "$repo/infra/config/policy/policies.cedar" \
     "$repo/infra/config/policy/cedar_schema.cedarschema" \
     "$model_src" "$model_tmp/policy/"
  cp "$repo/infra/config/pii_patterns.toml" "$model_tmp/pii_patterns.toml"
  cp "$repo/infra/systemd/jarvis-mcp-gw.service" "$model_tmp/jarvis-mcp-gw.service"
  # Minimal env that satisfies check_mcp_env (single absolute BASE_DIR, deny-all).
  printf '%s\n' \
    'JARVIS_MCP_GW_BASE_DIR=/opt/jarvis' \
    'JARVIS_MCP_GW_EGRESS_ALLOWLIST=' \
    > "$model_tmp/jarvis-mcp-gw.env"
  # Mock stat echoes fixed metadata by file class. MODEL_MODE_TAMPER=1 flips
  # ONLY casbin_model.conf to 0666; the control run reports 0644.
  cat > "$model_tmp/bin/stat" <<'MOCK'
#!/usr/bin/env bash
for last in "$@"; do :; done
case "$last" in
  *casbin_model.conf)
    if [ "${MODEL_MODE_TAMPER:-0}" = 1 ]; then
      printf '%s\n' '0:0 666'
    else
      printf '%s\n' '0:0 644'
    fi
    ;;
  *.env) printf '%s\n' '0:0 600' ;;
  *bin/jarvis-mcp-gw) printf '%s\n' '0:0 755' ;;
  *) printf '%s\n' '0:0 644' ;;
esac
MOCK
  # Only the throwaway mock is made executable; no deployed file is touched.
  chmod 0755 -- "$model_tmp/bin/stat"

  drift_model_run() { # tamper_flag -> drift output (stdout+stderr); status passes through
    local tamper="$1"
    # PATH is intentionally scoped to this command invocation.
    # shellcheck disable=SC2031
    PATH="$model_tmp/bin:$PATH" \
    MODEL_MODE_TAMPER="$tamper" \
    JARVIS_DRIFT_ALLOWLIST="$model_tmp/mcp_allowlist.toml" \
    JARVIS_DRIFT_POLICY_DIR="$model_tmp/policy" \
    JARVIS_DRIFT_PII="$model_tmp/pii_patterns.toml" \
    JARVIS_DRIFT_UNIT="$model_tmp/jarvis-mcp-gw.service" \
    JARVIS_DRIFT_ENV="$model_tmp/jarvis-mcp-gw.env" \
      bash "$repo/infra/scripts/check-deploy-drift.sh" 2>&1
  }

  # Static guard: both checks must name the same deployed path, and the owner
  # mode must match configs.sh (0644). Literal shell fragments are matched.
  model_drift_src="$repo/infra/scripts/check-deploy-drift.sh"
  # shellcheck disable=SC2016
  if grep -Fq '"$deployed_policy_dir/casbin_model.conf|$repo/infra/config/policy/casbin_model.conf"' "$model_drift_src" \
     && grep -Eq '^check_owner_mode[[:space:]]+"casbin_model"[[:space:]]+"\$deployed_policy_dir/casbin_model[.]conf"[[:space:]]+644' "$model_drift_src"; then
    echo "  PASS: drift covers casbin_model.conf in cmp pair and owner/mode check"
    ok=$((ok+1))
  else
    echo "  FAIL: drift still skips casbin_model.conf content or owner/mode"
    fail=$((fail+1))
  fi

  # Control: the pristine fixture must not be reported as missing or tampered.
  model_control="$(drift_model_run 0)"
  if printf '%s' "$model_control" | grep -q 'casbin_model[.]conf.*MISSING' \
     || printf '%s' "$model_control" | grep -q 'casbin_model.*E_BAD_OWNER_MODE'; then
    echo "  FAIL: drift flagged a pristine casbin_model.conf"
    fail=$((fail+1))
  else
    echo "  PASS: drift accepts a pristine casbin_model.conf"
    ok=$((ok+1))
  fi

  # Tamper 1: remove the deployed copy -> the cmp pair must fail closed.
  mv -- "$model_tmp/policy/casbin_model.conf" "$model_tmp/policy/casbin_model.conf.removed"
  model_missing="$(drift_model_run 0)"; code=$?
  mv -- "$model_tmp/policy/casbin_model.conf.removed" "$model_tmp/policy/casbin_model.conf"
  if [ "$code" -eq 2 ] \
     && printf '%s' "$model_missing" | grep -q 'casbin_model[.]conf[[:space:]]\+MISSING (deployed)'; then
    echo "  PASS: drift flags a missing deployed casbin_model.conf (exit 2)"
    ok=$((ok+1))
  else
    echo "  FAIL: drift missed a missing deployed casbin_model.conf (exit=$code)"
    fail=$((fail+1))
  fi

  # Tamper 2: wrong MODE -> owner/mode check must fail with a fixed code only.
  model_mode="$(drift_model_run 1)"; code=$?
  if [ "$code" -eq 2 ] \
     && printf '%s' "$model_mode" | grep -q 'casbin_model.*E_BAD_OWNER_MODE (code only)' \
     && ! printf '%s' "$model_mode" | grep -F 'E_BAD_OWNER_MODE' \
          | grep -Eq 'casbin_model[.]conf|0:0|666|casbin_model_tamper'; then
    echo "  PASS: drift flags a wrong casbin_model.conf mode with a fixed code only (exit 2)"
    ok=$((ok+1))
  else
    echo "  FAIL: drift missed a wrong casbin_model.conf mode or leaked a value (exit=$code)"
    fail=$((fail+1))
  fi
else
  echo "  FAIL: repo casbin_model.conf missing; cannot run I101-D tamper regressions"
  fail=$((fail+1))
fi

# I95-E (b)/(c): generator contract regressions. A stub `cargo` on PATH feeds
# deterministic --list / --list --ignored counts and a controllable real-run
# status, so NO real cargo/suite runs and no repository evidence is written.
gen_script="$repo/ci/scripts/generate-evidence.sh"
gen_tmp="$tmp/generator_contract"
mkdir -p "$gen_tmp/repo/ci/scripts" "$gen_tmp/repo/rust" "$gen_tmp/repo/ui" \
         "$gen_tmp/repo/evidence" "$gen_tmp/bin"
cp "$gen_script" "$gen_tmp/repo/ci/scripts/generate-evidence.sh"
: > "$gen_tmp/cargo.log"
cat > "$gen_tmp/bin/cargo" <<'STUB'
#!/usr/bin/env bash
# Deterministic cargo stub: never invokes a real toolchain.
mode=run
for arg in "$@"; do
  case "$arg" in
    --list) mode=list ;;
    --ignored) mode=ignored ;;
  esac
done
printf '%s\n' "$mode" >> "$CARGO_STUB_LOG"
case "$mode" in
  list)    printf '12 tests, 0 benchmarks\n' ;;
  ignored) printf '2 tests, 0 benchmarks\n' ;;
  run)
    if [ "${CARGO_STUB_FAIL:-0}" = 1 ]; then
      printf 'stub run failed\n' >&2
      exit 101
    fi
    printf 'stub run ok\n'
    ;;
esac
exit 0
STUB
chmod +x "$gen_tmp/bin/cargo"
export CARGO_STUB_LOG="$gen_tmp/cargo.log"

gen_invoke() { # args... -> GEN_RC / GEN_OUT / GEN_ERR
  local rc=0
  : > "$CARGO_STUB_LOG"
  # PATH is intentionally scoped to the generator process only.
  # shellcheck disable=SC2031
  GEN_OUT="$(PATH="$gen_tmp/bin:$PATH" bash "$gen_tmp/repo/ci/scripts/generate-evidence.sh" "$@" 2>"$gen_tmp/stderr")" || rc=$?
  GEN_ERR="$(< "$gen_tmp/stderr")"
  GEN_RC=$rc
}

# All six target arms must carry --locked on every cargo invocation. Static
# guard on a COPY of the source; no cargo is invoked here.
if python3 - "$gen_script" <<'PY'
import re, sys

with open(sys.argv[1], encoding='utf-8') as fh:
    src = fh.read()
expect = {
    'policy':  ('rust', 'jarvis-policy'),
    'mcp':     ('rust', 'jarvis-mcp-gw'),
    'context': ('rust', 'jarvis-context'),
    'event':   ('rust', 'jarvis-event'),
    'core':    ('rust', 'jarvis-core'),
    'ui':      ('ui', None),
}
for target, (workdir, package) in expect.items():
    arm = re.search(
        r'^\s*' + target + r'\)\s+workdir=([A-Za-z]+);\s*cargo_args=\(([^)]*)\)\s*;;',
        src, re.MULTILINE)
    if arm is None:
        sys.exit('MISSING_TARGET:' + target)
    args = arm.group(2).split()
    if arm.group(1) != workdir or '--locked' not in args:
        sys.exit('NO_LOCKED:' + target)
    if package is not None:
        if '-p' not in args or args[args.index('-p') + 1] != package:
            sys.exit('WRONG_PACKAGE:' + target)
PY
then
  echo "  PASS: generator uses --locked for all six targets"
  ok=$((ok+1))
else
  echo "  FAIL: generator lost --locked on one or more targets"
  fail=$((fail+1))
fi

# Path contract: nested paths and any ".." must be rejected with exit 2 BEFORE
# cargo is invoked; a well-shaped but absent path stays exit 1.
gen_invoke context --check evidence/test-run-../../nonexistent.json
if [ "$GEN_RC" -eq 2 ] && printf '%s' "$GEN_ERR" | grep -q 'FAIL: BadEvidencePath' \
   && [ ! -s "$CARGO_STUB_LOG" ]; then
  echo "  PASS: generator rejects dotdot evidence path with exit 2"
  ok=$((ok+1))
else
  echo "  FAIL: generator accepted a dotdot evidence path (exit $GEN_RC)"
  fail=$((fail+1))
fi

gen_invoke context --check evidence/nested/test-run-20261120-de83a4552e45.json
if [ "$GEN_RC" -eq 2 ] && printf '%s' "$GEN_ERR" | grep -q 'FAIL: BadEvidencePath' \
   && [ ! -s "$CARGO_STUB_LOG" ]; then
  echo "  PASS: generator rejects nested evidence path with exit 2"
  ok=$((ok+1))
else
  echo "  FAIL: generator accepted a nested evidence path (exit $GEN_RC)"
  fail=$((fail+1))
fi

gen_invoke context --check evidence/test-run-20261120-zzzzzzzzzzzz.json
if [ "$GEN_RC" -eq 2 ] && printf '%s' "$GEN_ERR" | grep -q 'FAIL: BadEvidencePath'; then
  echo "  PASS: generator rejects non-12-hex evidence filename with exit 2"
  ok=$((ok+1))
else
  echo "  FAIL: generator accepted a malformed evidence filename (exit $GEN_RC)"
  fail=$((fail+1))
fi

gen_invoke context --check evidence/test-run-20261120-000000000000.json
if [ "$GEN_RC" -eq 1 ] && printf '%s' "$GEN_ERR" | grep -q 'FAIL: EvidenceFileNotFound'; then
  echo "  PASS: generator reports a missing well-shaped evidence file with exit 1"
  ok=$((ok+1))
else
  echo "  FAIL: generator misreported a missing evidence file (exit $GEN_RC)"
  fail=$((fail+1))
fi

# Synthetic evidence in the temp repo only: passed=10 matches the stub counts,
# skipped=2 matches the ignored list; the other file forces MISMATCH.
printf '{"results":{"passed":10,"failed":0,"skipped":2}}\n' \
  > "$gen_tmp/repo/evidence/test-run-20261120-de83a4552e45.json"
printf '{"results":{"passed":9,"failed":0,"skipped":2}}\n' \
  > "$gen_tmp/repo/evidence/test-run-20261120-000000000001.json"

# Generation mode must still emit the labelled fragment (12 - 2 = 10 passed).
gen_invoke context
if [ "$GEN_RC" -eq 0 ] \
   && printf '%s' "$GEN_OUT" | grep -Fq 'LABEL: failed=0' \
   && printf '%s' "$GEN_OUT" | grep -Fq 'results: {"passed": 10, "failed": 0, "skipped": 2}' \
   && printf '%s' "$GEN_OUT" | grep -Fq 'notes  : 10 passed / 0 failed / 2 ignored' \
   && grep -qx 'list' "$CARGO_STUB_LOG" \
   && grep -qx 'ignored' "$CARGO_STUB_LOG" \
   && ! grep -qx 'run' "$CARGO_STUB_LOG"; then
  echo "  PASS: generator emits the derived results fragment in generation mode"
  ok=$((ok+1))
else
  echo "  FAIL: generator generation-mode contract regressed (exit $GEN_RC)"
  fail=$((fail+1))
fi

# --check MATCH: real suite ran, output is MATCH ONLY (no results fragment).
gen_invoke context --check evidence/test-run-20261120-de83a4552e45.json
if [ "$GEN_RC" -eq 0 ] && [ "$GEN_OUT" = 'CHECK: MATCH' ] \
   && grep -qx 'run' "$CARGO_STUB_LOG"; then
  echo "  PASS: generator --check prints CHECK: MATCH after the real run"
  ok=$((ok+1))
else
  echo "  FAIL: generator --check MATCH branch regressed (exit $GEN_RC)"
  fail=$((fail+1))
fi

# --check MISMATCH: exit 1 with MISMATCH only, still no success fragment.
gen_invoke context --check evidence/test-run-20261120-000000000001.json
if [ "$GEN_RC" -eq 1 ] && [ "$GEN_OUT" = 'CHECK: MISMATCH' ] \
   && grep -qx 'run' "$CARGO_STUB_LOG"; then
  echo "  PASS: generator --check prints CHECK: MISMATCH and exits 1"
  ok=$((ok+1))
else
  echo "  FAIL: generator --check MISMATCH branch regressed (exit $GEN_RC)"
  fail=$((fail+1))
fi

# Real-run failure must be detected and must leave NO success fragment.
CARGO_STUB_FAIL=1
export CARGO_STUB_FAIL
gen_invoke context --check evidence/test-run-20261120-de83a4552e45.json
unset CARGO_STUB_FAIL
if [ "$GEN_RC" -eq 1 ] \
   && printf '%s' "$GEN_ERR" | grep -q 'FAIL: ActualRunDetectedFailure' \
   && [ -z "$GEN_OUT" ] \
   && grep -qx 'run' "$CARGO_STUB_LOG"; then
  echo "  PASS: generator fails closed on real-run failure without a results fragment"
  ok=$((ok+1))
else
  echo "  FAIL: generator swallowed a real-run failure (exit $GEN_RC)"
  fail=$((fail+1))
fi

# I95-E (c): every concrete evidence JSON path cited under docs/ must exist
# and use test-run-<12hex>.json; documentation placeholders (* / YYYY / <>) are
# intentionally ignored. No documentation file is modified.
if python3 - "$repo" <<'PY'
import os, re, sys

root = sys.argv[1]
docs = os.path.join(root, 'docs')
concrete = re.compile(r'(?:[A-Za-z0-9_.\-]+/)*test-run-[0-9]{8}-[0-9a-fA-F]{12}[.]json')
loose = re.compile(r'test-run-[^ \t`\[\]\(\)<>\x27\x22]*[.]json')
errors = []
refs = 0
for dirpath, _, filenames in os.walk(docs):
    for name in filenames:
        if not name.endswith('.md'):
            continue
        path = os.path.join(dirpath, name)
        with open(path, encoding='utf-8') as fh:
            text = fh.read()
        for token in loose.findall(text):
            if '*' in token or 'YYYY' in token or '<' in token:
                continue
            base = token.rsplit('/', 1)[-1]
            if '..' in token or not re.fullmatch(r'test-run-[0-9]{8}-[0-9a-f]{12}[.]json', base):
                errors.append('BAD_DOC_EVIDENCE_REF ' + os.path.relpath(path, root) + ' ' + token)
        for token in concrete.findall(text):
            refs += 1
            if token.startswith('evidence/'):
                rel = token
            elif token.startswith('../'):
                rel = os.path.normpath(os.path.join(os.path.relpath(dirpath, root), token))
            else:
                rel = 'evidence/' + token
            if not os.path.isfile(os.path.join(root, rel)):
                errors.append('MISSING_DOC_EVIDENCE_REF ' + os.path.relpath(path, root) + ' ' + token)
if refs == 0:
    errors.append('NO_DOC_EVIDENCE_REF')
for err in errors:
    print(err)
sys.exit(1 if errors else 0)
PY
then
  echo "  PASS: docs evidence JSON references resolve to existing files"
  ok=$((ok+1))
else
  echo "  FAIL: docs contain a stale or malformed evidence JSON reference"
  fail=$((fail+1))
fi

# I95-D: the spec's Risk & tier R-ID set must equal risk_items[].id in every
# evidence citing that spec. Two tamper directions -- one declared R-ID removed
# from an evidence array, one undeclared R-ID appended -- must both fail with
# RISK_SET_MISMATCH. Use copied specs and evidence in a fresh tree so the
# repository files are never modified.
risk_tmp="$tmp/risk_set_negative"
mkdir -p "$risk_tmp/ci/scripts" "$risk_tmp/evidence" "$risk_tmp/specs"
cp "$repo/ci/scripts/check-evidence.sh" "$risk_tmp/ci/scripts/check-evidence.sh"
cp "$repo/specs/index.md" "$risk_tmp/specs/index.md"
cp "$repo/evidence/index.md" "$risk_tmp/evidence/index.md"
cp "$repo/todo.md" "$risk_tmp/todo.md"
cp "$repo"/specs/test-*.md "$risk_tmp/specs/"
cp "$repo"/evidence/test-run-*.json "$risk_tmp/evidence/"

if bash "$risk_tmp/ci/scripts/check-evidence.sh" >/dev/null 2>&1; then
  echo "  PASS: I95-D pristine Risk & tier sets accepted"
  ok=$((ok+1))
else
  echo "  FAIL: I95-D pristine Risk & tier sets rejected"
  fail=$((fail+1))
fi

risk_source="$risk_tmp/evidence/$(basename "${evidence_files[0]}")"
for mode in risk_removed risk_added; do
  # Restore every evidence file first: a previous tamper must not mask the
  # direction under test.
  cp "$repo"/evidence/test-run-*.json "$risk_tmp/evidence/"
  if ! python3 - "$risk_source" "$mode" <<'PY'
import json, sys

path, mode = sys.argv[1:3]
with open(path, encoding='utf-8') as fh:
    data = json.load(fh)
items = data.get('risk_items')
if not isinstance(items, list) or len(items) < 2:
    raise SystemExit('risk_items array too short for tamper')
if mode == 'risk_removed':
    items.pop()
elif mode == 'risk_added':
    # An id the referenced spec does not declare. The item keeps a valid
    # shape, so only the risk-set gate can reject this copy.
    items.append({'id': 'R-9999', 'tier': 'T1',
                  'description': 'undeclared risk',
                  'asvs': [], 'controls': []})
else:
    raise SystemExit('unknown risk tamper mode: ' + mode)
with open(path, 'w', encoding='utf-8') as fh:
    json.dump(data, fh)
PY
  then
    echo "  FAIL: could not prepare R-ID tamper ($mode)"
    fail=$((fail+1))
    continue
  fi
  out=$(bash "$risk_tmp/ci/scripts/check-evidence.sh" 2>&1); code=$?
  if [ "$code" -ne 0 ] && printf '%s' "$out" | grep -q 'RISK_SET_MISMATCH'; then
    echo "  PASS: check-evidence.sh rejects R-ID $mode"
    ok=$((ok+1))
  else
    echo "  FAIL: check-evidence.sh accepted R-ID $mode (exit $code)"
    printf '%s\n' "$out" | head -3 >&2
    fail=$((fail+1))
  fi
done

# I97-B: JARVIS_DRIFT_BINARY must drive BOTH the content and the owner/mode
# check. Static guard plus a mocked-stat run; no real file/permission is touched.
drift_src="$repo/infra/scripts/check-deploy-drift.sh"
# Literal shell fragments are matched in the drift source.
# shellcheck disable=SC2016
if grep -Fq 'sha256sum "$deployed_binary"' "$drift_src" \
   && grep -Eq '^check_owner_mode[[:space:]]+"binary"[[:space:]]+"\$deployed_binary"' "$drift_src" \
   && ! grep -Fq 'sha256sum /opt/jarvis/bin/jarvis-mcp-gw' "$drift_src" \
   && ! grep -Eq '^check_owner_mode[[:space:]]+"binary"[[:space:]]+/opt/jarvis' "$drift_src"; then
  echo "  PASS: drift binary content/owner-mode share JARVIS_DRIFT_BINARY"
  ok=$((ok+1))
else
  echo "  FAIL: drift binary checks bypassed JARVIS_DRIFT_BINARY"
  fail=$((fail+1))
fi

bin_override_dir="$tmp/binary_override"
mkdir -p "$bin_override_dir"
printf 'override-binary\n' > "$bin_override_dir/custom-gw"
chmod 0755 -- "$bin_override_dir/custom-gw"
cat > "$bin_override_dir/stat" <<'MOCK'
#!/usr/bin/env bash
for last in "$@"; do :; done
case "$last" in
  *custom-gw) printf '%s\n' '1000:1000 755' ;;
  *.env) printf '%s\n' '0:0 600' ;;
  *bin/jarvis-mcp-gw) printf '%s\n' '0:0 755' ;;
  *) printf '%s\n' '0:0 644' ;;
esac
MOCK
chmod 0755 -- "$bin_override_dir/stat"
bin_out=$(
  # PATH is intentionally scoped to this drift invocation.
  # shellcheck disable=SC2031
  PATH="$bin_override_dir:$PATH" \
  JARVIS_DRIFT_BINARY="$bin_override_dir/custom-gw" \
  JARVIS_DRIFT_ALLOWLIST="$owner_tmp/mcp_allowlist.toml" \
  JARVIS_DRIFT_POLICY_DIR="$owner_tmp/policy" \
  JARVIS_DRIFT_PII="$owner_tmp/pii_patterns.toml" \
  JARVIS_DRIFT_UNIT="$owner_tmp/jarvis-mcp-gw.service" \
  JARVIS_DRIFT_ENV="$owner_tmp/jarvis-mcp-gw.env" \
    bash "$repo/infra/scripts/check-deploy-drift.sh" 2>&1
)
bin_code=$?
if [ "$bin_code" -eq 2 ] \
   && printf '%s' "$bin_out" | grep -q 'binary.*E_BAD_OWNER_MODE' \
   && ! printf '%s' "$bin_out" | grep -F "$bin_override_dir" | grep -q 'E_BAD_OWNER_MODE'; then
  echo "  PASS: drift owner/mode uses JARVIS_DRIFT_BINARY (fixed code only)"
  ok=$((ok+1))
else
  echo "  FAIL: drift owner/mode ignored JARVIS_DRIFT_BINARY (exit=$bin_code)"
  fail=$((fail+1))
fi

# I97-C: runtime_check must JSON-escape the repo path. Run a COPY of the drift
# script from a root whose path contains Japanese text and a double quote, with
# stubbed systemctl/curl/stat. No service or deployed file is touched.
esc_tmp="$tmp/drift_json_escape"
jp_repo="$esc_tmp/日本語\"引用符/repo"
mkdir -p "$jp_repo/infra/scripts" "$jp_repo/infra/config/policy" \
         "$jp_repo/infra/systemd" "$jp_repo/rust/target/release" \
         "$esc_tmp/bin"
cp "$repo/infra/scripts/check-deploy-drift.sh" "$jp_repo/infra/scripts/"
cp "$repo/infra/config/mcp_allowlist.toml" "$jp_repo/infra/config/"
cp "$repo/infra/config/policy/casbin_policy.csv" \
   "$repo/infra/config/policy/policies.cedar" \
   "$repo/infra/config/policy/cedar_schema.cedarschema" "$jp_repo/infra/config/policy/"
cp "$repo/infra/config/pii_patterns.toml" "$jp_repo/infra/config/"
cp "$repo/infra/systemd/jarvis-mcp-gw.service" "$jp_repo/infra/systemd/"
printf 'stub-binary\n' > "$jp_repo/rust/target/release/jarvis-mcp-gw"
chmod 0755 -- "$jp_repo/rust/target/release/jarvis-mcp-gw"
printf '%s\n' \
  'JARVIS_MCP_GW_BASE_DIR=/opt/jarvis' \
  'JARVIS_MCP_GW_EGRESS_ALLOWLIST=' \
  > "$esc_tmp/jarvis-mcp-gw.env"
cat > "$esc_tmp/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
[ "${1:-}" = is-active ] && [ "${3:-}" = jarvis-mcp-gw ] && exit 0
exit 1
STUB
cat > "$esc_tmp/bin/curl" <<'STUB'
#!/usr/bin/env bash
out=/dev/null
hdr=
payload=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -D) hdr="$2"; shift 2 ;;
    -d) payload="$2"; shift 2 ;;
    -w) shift 2 ;;
    http*) url="$1"; shift ;;
    *) shift ;;
  esac
done
if [ -n "$hdr" ]; then
  printf '%s\n' 'mcp-session-id: stub-session' > "$hdr"
fi
# I101-C (b): answer the drift E2E /health probe with a healthy body so this
# guard keeps exercising the tools/call path it was written for.
if [[ "$url" == */health ]]; then
  printf '%s\n%s\n' '{"status":"ok"}' '200'
  exit 0
fi
case "$payload" in
  *filesystem.list*)
    printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"entries\":[]}"}]}}' > "$out" ;;
  *git.status*)
    printf '%s\n' "$payload" > "${CAPTURE_D:?}"
    printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{\"stdout\":\"ok\",\"stderr\":\"\",\"status\":0}"}]}}' > "$out" ;;
  *tools/list*)
    printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"tools":[{"name":"filesystem.list"},{"name":"filesystem.read"},{"name":"git.diff"},{"name":"git.status"}]}}' > "$out" ;;
esac
exit 0
STUB
chmod 0755 -- "$esc_tmp/bin/systemctl" "$esc_tmp/bin/curl"
esc_out=$(
  # PATH is intentionally scoped to this drift invocation.
  # shellcheck disable=SC2031
  PATH="$esc_tmp/bin:$owner_tmp/bin:$PATH" \
  CAPTURE_D="$esc_tmp/git-status-payload.json" \
  JARVIS_DRIFT_ALLOWLIST="$jp_repo/infra/config/mcp_allowlist.toml" \
  JARVIS_DRIFT_POLICY_DIR="$jp_repo/infra/config/policy" \
  JARVIS_DRIFT_PII="$jp_repo/infra/config/pii_patterns.toml" \
  JARVIS_DRIFT_UNIT="$jp_repo/infra/systemd/jarvis-mcp-gw.service" \
  JARVIS_DRIFT_ENV="$esc_tmp/jarvis-mcp-gw.env" \
  JARVIS_DRIFT_BINARY="$jp_repo/rust/target/release/jarvis-mcp-gw" \
    bash "$jp_repo/infra/scripts/check-deploy-drift.sh" 2>&1
)
esc_code=$?
if printf '%s' "$esc_out" | grep -q 'runtime tools/call.*OK' \
   && python3 - "$esc_tmp/git-status-payload.json" "$jp_repo" <<'PY'
import json, sys
with open(sys.argv[1], encoding='utf-8') as fh:
    payload = json.load(fh)
sys.exit(0 if payload['params']['arguments']['path'] == sys.argv[2] else 1)
PY
then
  echo "  PASS: drift runtime_check escapes a Japanese/quoted repo path"
  ok=$((ok+1))
else
  echo "  FAIL: drift runtime_check broke on a Japanese/quoted path (exit=$esc_code)"
  fail=$((fail+1))
fi

# I97-D: cargo-audit 0.23 does not auto-discover audit.toml; the gate must pass
# --ignore explicitly and audit.toml must hold concrete RUSTSEC IDs.
audit_gate="$repo/ci/scripts/check-dependency-audit.sh"
# Literal shell fragment is matched in the audit gate source.
# shellcheck disable=SC2016
if grep -Fq 'ignore_args+=(--ignore "$advisory_id")' "$audit_gate" \
   && grep -Fq 'cargo audit --no-fetch --file "$lock" "${ignore_args[@]}"' "$audit_gate" \
   && ! grep -Fiq 'TODO-RUSTSEC' "$repo/audit.toml"; then
  echo "  PASS: dependency audit passes --ignore with concrete RUSTSEC IDs"
  ok=$((ok+1))
else
  echo "  FAIL: dependency audit contract regressed"
  fail=$((fail+1))
fi

# I99-C: a deploy must fail when any /health answer is degraded, not just when
# curl sees a non-200. Exercise the REAL std-only parser/evaluator from
# deploy.rs by compiling the marker-delimited block with rustc and feeding it
# stubbed probe payloads (200 healthy, 503 degraded, 200 carrying the fixed
# degraded body). No ssh, service, or rust-script is involved. The static guard
# first pins the block and the removal of the old `curl -sf` gate.
deploy_src="$repo/rust/scripts/deploy.rs"
health_block="$(sed -n '/^\/\/ health-gate-begin$/,/^\/\/ health-gate-end$/p' "$deploy_src")"
if [ -z "$health_block" ] \
   || ! printf '%s' "$health_block" | grep -q 'HEALTH_DEGRADED_MARKER' \
   || ! printf '%s' "$health_block" | grep -q 'code == "503"' \
   || ! printf '%s' "$health_block" | grep -q 'is_whitespace' \
   || grep -q 'curl -sf' "$deploy_src"; then
  echo "  FAIL: deploy.rs lost the degraded/503 health gate"
  fail=$((fail+1))
elif ! command -v rustc >/dev/null 2>&1; then
  echo "  PASS: deploy.rs keeps the degraded/503 health gate (rustc unavailable; static only)"
  ok=$((ok+1))
else
  gate_rs="$tmp/deploy_health_gate.rs"
  {
    printf '%s\n' "$health_block"
    cat <<'RUST_TEST'
fn main() {
    let ok = parse_health_probe(true, "{\"status\":\"ok\"}\n200");
    assert_eq!(ok, HealthStatus::Healthy);

    let degraded_503 = parse_health_probe(
        true,
        "{\"status\":\"degraded\",\"reason\":\"no_upstream\"}\n503",
    );
    assert!(matches!(degraded_503, HealthStatus::Degraded(_)));

    // A relay that rewrites the status to 200 must not hide degradation.
    let degraded_200 = parse_health_probe(
        true,
        "{\"status\":\"degraded\",\"reason\":\"store_unavailable\"}\n200",
    );
    assert!(matches!(degraded_200, HealthStatus::Degraded(_)));

    // I103-C (b): a pretty-printed degraded body must fail closed too, for
    // both a real 503 and a rewritten 200. matches!() keeps the stub
    // body/reason out of assertion failure output.
    let degraded_503_pretty = parse_health_probe(
        true,
        "{\n  \"status\": \"degraded\",\n  \"reason\": \"no_upstream\"\n}\n503",
    );
    assert!(matches!(degraded_503_pretty, HealthStatus::Degraded(_)));

    let degraded_200_pretty = parse_health_probe(
        true,
        "{\n  \"status\": \"degraded\",\n  \"reason\": \"store_unavailable\"\n}\n200",
    );
    assert!(matches!(degraded_200_pretty, HealthStatus::Degraded(_)));

    let all_ok = vec![(8081u16, HealthStatus::Healthy)];
    assert!(evaluate_health_checks(&all_ok).is_ok());

    let mixed = vec![(8081u16, HealthStatus::Healthy), (8082u16, degraded_503)];
    let err = evaluate_health_checks(&mixed).expect_err("degraded must fail deploy");
    assert!(err.contains("8082") && err.contains("degraded"));
}
RUST_TEST
  } > "$gate_rs"
  if rustc --edition 2021 -A dead_code -o "$tmp/deploy_health_gate" "$gate_rs" \
      >/dev/null 2>&1 && "$tmp/deploy_health_gate" >/dev/null 2>&1; then
    echo "  PASS: deploy health gate rejects stubbed 503/degraded-200 and pretty-printed bodies"
    ok=$((ok+1))
  else
    echo "  FAIL: deploy health gate accepted a degraded stub"
    fail=$((fail+1))
  fi
fi

# I101-C (c): the retry budget must be bounded (20 x 0.5s) AND deadline-based,
# not the old single fixed 5s sleep + one probe. I103-C (a): each probe also
# needs a per-probe --max-time, otherwise one hung curl can erase the absolute
# deadline between rounds. The std-only retry predicate and the timeout
# constant live inside the marker block so this guard can compile them.
if ! printf '%s' "$health_block" | grep -q 'HEALTH_PROBE_ATTEMPTS: u32 = 20' \
   || ! printf '%s' "$health_block" | grep -q 'HEALTH_PROBE_INTERVAL_MS: u64 = 500' \
   || ! printf '%s' "$health_block" | grep -q 'fn health_retry_allowed' \
   || ! printf '%s' "$health_block" | grep -q 'HEALTH_PROBE_TIMEOUT_SECS: u64 = 2' \
   || ! grep -Fq -- '--max-time {HEALTH_PROBE_TIMEOUT_SECS}' "$deploy_src" \
   || grep -q 'Duration::from_secs(5)' "$deploy_src"; then
  echo "  FAIL: deploy.rs lost the bounded retry/deadline/timeout health gate"
  fail=$((fail+1))
elif ! command -v rustc >/dev/null 2>&1; then
  echo "  PASS: deploy.rs keeps the bounded retry/timeout gate (rustc unavailable; static only)"
  ok=$((ok+1))
else
  retry_rs="$tmp/deploy_health_retry.rs"
  {
    printf '%s\n' "$health_block"
    cat <<'RUST_TEST'
fn main() {
    assert_eq!(HEALTH_PROBE_ATTEMPTS, 20);
    assert_eq!(HEALTH_PROBE_INTERVAL_MS, 500);
    assert_eq!(HEALTH_PROBE_TIMEOUT_SECS, 2);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    assert!(health_retry_allowed(0, deadline));
    assert!(!health_retry_allowed(HEALTH_PROBE_ATTEMPTS - 1, deadline));
    let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
    assert!(!health_retry_allowed(0, past));
}
RUST_TEST
  } > "$retry_rs"
  if rustc --edition 2021 -A dead_code -o "$tmp/deploy_health_retry" "$retry_rs" \
      >/dev/null 2>&1 && "$tmp/deploy_health_retry" >/dev/null 2>&1; then
    echo "  PASS: deploy.rs retry budget is bounded (20 x 0.5s), deadline-based, timeout-guarded"
    ok=$((ok+1))
  else
    echo "  FAIL: deploy.rs retry predicate rejected a valid bounded probe"
    fail=$((fail+1))
  fi
fi

# I101-C (b): the shell deploy path must read the /health BODY after its retry
# loop and roll back on a degraded body or a 503, using the same 20 x 0.5s
# budget as deploy.rs. Static guard only (no service or systemctl is touched).
post_health="$(sed -n '/^echo "== 5\./,/^echo "== 6\./p' "$deploy_script")"
if printf '%s' "$post_health" | grep -Fq 'fetch_health' \
   && printf '%s' "$post_health" | grep -Fq 'health_is_degraded' \
   && printf '%s' "$post_health" | grep -Fq '"503"' \
   && printf '%s' "$post_health" | grep -Fq 'rollback' \
   && grep -Fq 'health_attempts=20' "$deploy_script" \
   && grep -Fq 'health_interval=0.5' "$deploy_script"; then
  echo "  PASS: safe deploy rolls back on degraded/503 /health with 20x0.5s budget"
  ok=$((ok+1))
else
  echo "  FAIL: safe deploy lost the degraded/503 health gate"
  fail=$((fail+1))
fi

# I101-C (b): drift's runtime E2E must verify /health 200 with a non-degraded
# body in addition to tools/call. Static guard on the drift source.
if grep -Fq 'health_check()' "$drift_src" \
   && grep -Fq 'if health_check; then say "runtime /health"' "$drift_src" \
   && grep -Fq '"status"[[:space:]]*:[[:space:]]*"degraded"' "$drift_src"; then
  echo "  PASS: drift E2E verifies /health 200 with a non-degraded body"
  ok=$((ok+1))
else
  echo "  FAIL: drift E2E lost the /health 200 gate"
  fail=$((fail+1))
fi

# I101-C (b): exercise the REAL drift /health gate with stubbed curl payloads.
# No service is contacted: curl is a shell function inside a subshell, and the
# function under test is extracted from the production drift script.
health_fn="$(sed -n '/^health_check()/,/^}/p' "$drift_src")"
probe_health_gate() { # body code -> health_check exit status
  # Distinct names: health_check() declares its own local `body`/`code`, which
  # would shadow same-named variables via bash dynamic scoping.
  local stub_body="$1" stub_code="$2"
  (
    # shellcheck disable=SC2329
    curl() { printf '%s\n%s\n' "$stub_body" "$stub_code"; }
    # shellcheck disable=SC1090,SC2086
    eval "$health_fn"
    health_check
  )
}
if probe_health_gate '{"status":"ok"}' 200 \
   && ! probe_health_gate '{"status":"degraded","reason":"no_upstream"}' 503 \
   && ! probe_health_gate '{"status":"degraded","reason":"no_upstream"}' 200 \
   && ! probe_health_gate '{"status":"ok"}' 503; then
  echo "  PASS: drift /health gate rejects 503 and degraded-200 bodies"
  ok=$((ok+1))
else
  echo "  FAIL: drift /health gate accepted a degraded answer"
  fail=$((fail+1))
fi

# I103-B: install-mcp-gateway.sh is the third deploy path. It must use the same
# bounded health contract as deploy-mcp-gw-safe.sh / deploy.rs (20 probes at
# 0.5s, degraded-body detection, no fixed 2-second sleep), and must run the
# post-rollback drift check as a gate. Static guard only: no service is touched.
install_gw_script="$repo/infra/scripts/install-mcp-gateway.sh"
if grep -Fq 'health_attempts=20' "$install_gw_script" \
   && grep -Fq 'health_interval=0.5' "$install_gw_script" \
   && grep -Fq '"status"[[:space:]]*:[[:space:]]*"degraded"' "$install_gw_script" \
   && grep -Fq "curl -s -w '\\n%{http_code}'" "$install_gw_script" \
   && grep -Fq 'health_is_degraded' "$install_gw_script" \
   && grep -Fq 'check-deploy-drift.sh' "$install_gw_script" \
   && ! grep -Eq 'sleep[[:space:]]+2([^0-9]|$)' "$install_gw_script"; then
  echo "  PASS: install-mcp-gateway.sh uses bounded retry with degraded detection"
  ok=$((ok+1))
else
  echo "  FAIL: install-mcp-gateway.sh lost the bounded/degraded health gate"
  fail=$((fail+1))
fi

# b4-scripts / 2026-11-28: a failed sha256sum produced two empty digests and
# the drift gate reported "binary OK (same)". Exercise the real binary block
# with a failing sha256sum stub and require DANGER, not OK/PENDING.
binary_block="$(sed -n '/^# 1) binary/,/^# 2) allowlist/p' "$drift_src" | sed '$d')"
binary_hash_guard() {
  local out
  mkdir -p "$tmp/rust/target/release"
  printf '%s\n' 'stub' > "$tmp/bin"
  printf '%s\n' 'stub' > "$tmp/rust/target/release/jarvis-mcp-gw"
  chmod +x "$tmp/bin" "$tmp/rust/target/release/jarvis-mcp-gw"
  out="$(
    (
      # shellcheck disable=SC2329
      say() { printf '%s %s\n' "$1" "$2"; }
      # shellcheck disable=SC2329
      sha256sum() { return 1; }
      danger=0; drift=0
      # shellcheck disable=SC2030
      repo="$tmp"
      # shellcheck disable=SC2034
      deployed_binary="$tmp/bin"
      eval "$binary_block"
      printf 'danger=%s drift=%s\n' "$danger" "$drift"
    )
  )" || return 1
  case "$out" in
    *'binary DANGER (hash unavailable)'*) ;;
    *) return 1 ;;
  esac
  case "$out" in
    *'danger=1'*) ;;
    *) return 1 ;;
  esac
  case "$out" in
    *'binary OK (same)'*) return 1 ;;
  esac
  return 0
}

if binary_hash_guard; then
  echo "  PASS: drift binary check fails closed on unavailable hashes"
  ok=$((ok+1))
else
  echo "  FAIL: drift binary check accepted an unavailable hash"
  fail=$((fail+1))
fi

# I2-FIX: backup/install failures must enter rollback() immediately. The old
# `[ -f "$dst" ] && cp ...` also aborted under set -e when no old binary
# existed. Exercise the real apply body with stubbed cp/install/systemctl/bash
# and the real rollback body, so no service, /opt or /etc path is touched.
gw_probe_dir="$tmp/install_gw_probe"
mkdir -p "$gw_probe_dir"
gw_rollback_body="$(sed -n '/^rollback()/,/^}/p' "$install_gw_script")"
# shellcheck disable=SC2016
# $apply / $src / $dst are literal sed patterns, not shell expansions.
gw_apply_body="$(sed -n '/^if \[ "$apply" -eq 1 \]; then$/,/^  install -o root -g root -m 0755 "$src" "$dst" || rollback$/p' "$install_gw_script" | sed '1d')"

gw_probe_out=; gw_probe_code=0
gw_install_probe() { # mode: cp_fail | install_fail | ok
  gw_probe_mode="$1"
  gw_probe_out=$(
    # shellcheck disable=SC2034
    # Vars are consumed by the sourced apply body.
    (
      set -euo pipefail
      apply=1
      dst="$gw_probe_dir/dst"
      src="$gw_probe_dir/src"
      ts='probe'
      health_code=000
      # shellcheck disable=SC2329
      cp() {
        printf 'cp-called\n' >&2
        if [ "$gw_probe_mode" = cp_fail ]; then return 1; fi
        return 0
      }
      # shellcheck disable=SC2329
      install() {
        printf 'install-called\n' >&2
        if [ "$gw_probe_mode" = install_fail ]; then return 1; fi
        return 0
      }
      # shellcheck disable=SC2329
      systemctl() { printf 'inactive\n'; return 0; }
      # shellcheck disable=SC2329
      bash() { return 0; }
      # shellcheck disable=SC1090,SC2086
      eval "$gw_rollback_body"
      # shellcheck disable=SC1090,SC2086
      eval "$gw_apply_body"
      printf 'after-install-reached\n' >&2
    ) 2>&1
  )
  gw_probe_code=$?
}

# rollback() must keep its terminal exit codes: exit 1 on a normal rollback and
# exit 3 when the post-rollback drift gate reports DANGER/unknown.
if printf '%s\n' "$gw_rollback_body" | grep -Fq 'exit 3' \
   && printf '%s\n' "$gw_rollback_body" | grep -Eq '^[[:space:]]+exit 1$'; then
  echo "  PASS: install-mcp-gateway.sh rollback keeps exit 1/exit 3 terminals"
  ok=$((ok+1))
else
  echo "  FAIL: install-mcp-gateway.sh rollback lost a terminal exit code"
  fail=$((fail+1))
fi

# (a) backup cp failure -> rollback, install never attempted, no continuation.
printf 'old\n' > "$gw_probe_dir/dst"
gw_install_probe cp_fail
gw_out="$gw_probe_out"; code="$gw_probe_code"
cp_calls=$(printf '%s\n' "$gw_out" | grep -c 'cp-called' || true)
install_calls=$(printf '%s\n' "$gw_out" | grep -c 'install-called' || true)
if [ "$code" -eq 1 ] \
   && [ "$cp_calls" -eq 1 ] && [ "$install_calls" -eq 0 ] \
   && printf '%s\n' "$gw_out" | grep -Fq '== ロールバック: 旧バイナリへ戻します ==' \
   && ! printf '%s\n' "$gw_out" | grep -Fq 'after-install-reached' \
   && ! printf '%s\n' "$gw_out" | grep -Fq -- "$gw_probe_dir"; then
  echo "  PASS: install-mcp-gateway.sh rolls back when backup cp fails"
  ok=$((ok+1))
else
  echo "  FAIL: install-mcp-gateway.sh did not roll back on cp failure (exit=$code)"
  fail=$((fail+1))
fi

# (b) install failure -> rollback after a successful backup, no continuation.
printf 'old\n' > "$gw_probe_dir/dst"
gw_install_probe install_fail
gw_out="$gw_probe_out"; code="$gw_probe_code"
cp_calls=$(printf '%s\n' "$gw_out" | grep -c 'cp-called' || true)
install_calls=$(printf '%s\n' "$gw_out" | grep -c 'install-called' || true)
if [ "$code" -eq 1 ] \
   && [ "$cp_calls" -eq 1 ] && [ "$install_calls" -ge 1 ] \
   && printf '%s\n' "$gw_out" | grep -Fq '== ロールバック: 旧バイナリへ戻します ==' \
   && ! printf '%s\n' "$gw_out" | grep -Fq 'after-install-reached' \
   && ! printf '%s\n' "$gw_out" | grep -Fq -- "$gw_probe_dir"; then
  echo "  PASS: install-mcp-gateway.sh rolls back when install fails"
  ok=$((ok+1))
else
  echo "  FAIL: install-mcp-gateway.sh did not roll back on install failure (exit=$code)"
  fail=$((fail+1))
fi

# (c) no old binary: [ -f "$dst" ] is false, cp must be skipped but install
# must still run and the script must reach the restart phase.
rm -f "$gw_probe_dir/dst"
gw_install_probe ok
gw_out="$gw_probe_out"; code="$gw_probe_code"
cp_calls=$(printf '%s\n' "$gw_out" | grep -c 'cp-called' || true)
install_calls=$(printf '%s\n' "$gw_out" | grep -c 'install-called' || true)
if [ "$code" -eq 0 ] \
   && [ "$cp_calls" -eq 0 ] && [ "$install_calls" -eq 1 ] \
   && printf '%s\n' "$gw_out" | grep -Fq 'after-install-reached' \
   && ! printf '%s\n' "$gw_out" | grep -Fq 'ロールバック' \
   && ! printf '%s\n' "$gw_out" | grep -Fq -- "$gw_probe_dir"; then
  echo "  PASS: install-mcp-gateway.sh skips cp when no old binary exists"
  ok=$((ok+1))
else
  echo "  FAIL: install-mcp-gateway.sh skipped install or called cp (exit=$code)"
  fail=$((fail+1))
fi

# b4-scripts / 2026-11-28 (1): step3 の unit / allowlist 配置は戻り値を検査し、
# 失敗したらその場で rollback() へ入る（スモーク/再起動/事後 drift へ進まない）。
# 実 install / systemctl / drift には触れず、本文を抽出して stub 実行する。
# shellcheck disable=SC2016
# The \$ escapes are literal sed patterns, not shell expansions.
unit_block="$(sed -n '/^  if \[ "\$unit_changed" -eq 1 \]; then$/,/^  fi$/p' "$deploy_script")"
# shellcheck disable=SC2016
allowlist_block="$(sed -n '/^  if \[ "\$allowlist_changed" -eq 1 \]; then$/,/^  fi$/p' "$deploy_script")"
# shellcheck disable=SC2034
# Vars below are consumed by the eval'd apply body.
step3_install_probe() { # fail_target block_text -> output; status = block status
  local fail_target="$1" block="$2"
  (
    apply=1
    unit_changed=1; allowlist_changed=1
    unit_mode=0644; allowlist_mode=0644
    repo_unit=/probe/repo_unit; unit=/probe/unit
    repo_allowlist=/probe/repo_allowlist; allowlist=/probe/allowlist
    # shellcheck disable=SC2329
    install_expected_config() {
      if [ "$3" = "$fail_target" ]; then
        printf 'install-failed\n' >&2
        return 1
      fi
      printf 'install-called\n' >&2
      return 0
    }
    # shellcheck disable=SC2329
    rollback() { printf 'rollback-called\n' >&2; exit 1; }
    # shellcheck disable=SC2329
    systemctl() { printf 'systemctl-called\n' >&2; return 0; }
    # shellcheck disable=SC1090,SC2086
    eval "$block"
    printf 'after-step3-reached\n' >&2
  ) 2>&1
}

step3_out="$(step3_install_probe /probe/unit "$unit_block")"; code=$?
if [ "$code" -eq 1 ] \
   && printf '%s' "$step3_out" | grep -Fq 'install-failed' \
   && printf '%s' "$step3_out" | grep -Fq 'unit 配置失敗' \
   && printf '%s' "$step3_out" | grep -Fq 'rollback-called' \
   && ! printf '%s' "$step3_out" | grep -Fq 'systemctl-called' \
   && ! printf '%s' "$step3_out" | grep -Fq 'after-step3-reached'; then
  echo "  PASS: step3 unit install failure rolls back with no continuation"
  ok=$((ok+1))
else
  echo "  FAIL: step3 unit install failure was ignored (exit=$code)"
  fail=$((fail+1))
fi

step3_out="$(step3_install_probe /probe/allowlist "$allowlist_block")"; code=$?
if [ "$code" -eq 1 ] \
   && printf '%s' "$step3_out" | grep -Fq 'install-failed' \
   && printf '%s' "$step3_out" | grep -Fq 'allowlist 配置失敗' \
   && printf '%s' "$step3_out" | grep -Fq 'rollback-called' \
   && ! printf '%s' "$step3_out" | grep -Fq 'after-step3-reached'; then
  echo "  PASS: step3 allowlist install failure rolls back with no continuation"
  ok=$((ok+1))
else
  echo "  FAIL: step3 allowlist install failure was ignored (exit=$code)"
  fail=$((fail+1))
fi

step3_both="$unit_block"$'\n'"$allowlist_block"
step3_out="$(step3_install_probe /probe/nonexistent "$step3_both")"; code=$?
step3_install_calls=$(printf '%s\n' "$step3_out" | grep -c 'install-called' || true)
if [ "$code" -eq 0 ] \
   && [ "$step3_install_calls" -eq 2 ] \
   && printf '%s' "$step3_out" | grep -Fq 'systemctl-called' \
   && printf '%s' "$step3_out" | grep -Fq 'after-step3-reached' \
   && ! printf '%s' "$step3_out" | grep -Fq 'rollback-called'; then
  echo "  PASS: step3 successful installs continue to the next stage"
  ok=$((ok+1))
else
  echo "  FAIL: step3 successful installs did not continue (exit=$code)"
  fail=$((fail+1))
fi

# b4-scripts / 2026-11-28 (2): webfetch_approval_ok() を deploy 本文から抽出し、
# tomllib の厳密判定を実実行で確認する。fixture は一時ディレクトリのみ。
wf_dir="$tmp/webfetch_approval_fn"
mkdir -p "$wf_dir"
# shellcheck disable=SC2031
# repo is only read here; the subshell edit is isolated.
if python3 - "$repo/infra/config/mcp_allowlist.toml" "$wf_dir" <<'PY'
import os, sys

src, d = sys.argv[1:3]
with open(src, encoding='utf-8') as fh:
    lines = fh.readlines()
start = next(i for i, l in enumerate(lines) if l.strip() == 'name = "web.fetch"')
end = next((j for j in range(start + 1, len(lines))
            if lines[j].strip().startswith('[[tools]]')), len(lines))
approval = next(k for k in range(start, end) if 'requires_approval' in lines[k])

def write(name, ls):
    with open(os.path.join(d, name), 'w', encoding='utf-8') as fh:
        fh.writelines(ls)

write('wf_true.toml', lines)
false_lines = lines[:]
false_lines[approval] = 'requires_approval = false\n'
write('wf_false.toml', false_lines)
write('wf_missing_key.toml', lines[:approval] + lines[approval + 1:])
padding = ['# regression padding %02d\n' % i for i in range(10)]
write('wf_padding_false.toml',
      false_lines[:start + 1] + padding + false_lines[start + 1:])
write('wf_duplicate.toml', lines[:end] + lines[start - 1:end] + lines[end:])
write('wf_invalid.toml', lines[:start] + ['not valid = = toml\n'] + lines[start:])
PY
then
  wf_fn="$(sed -n '/^webfetch_approval_ok()/,/^}/p' "$deploy_script")"
  wf_probe() { # path -> exit code
    (
      # shellcheck disable=SC1090,SC2086
      eval "$wf_fn"
      webfetch_approval_ok "$1"
    )
  }
  wf_bad=0
  for name in wf_true wf_false wf_missing_key wf_padding_false wf_duplicate wf_invalid; do
    case "$name" in
      wf_true) want=ok ;;
      *) want=reject ;;
    esac
    wf_probe "$wf_dir/$name.toml"; wf_rc=$?
    case "$want" in
      ok) [ "$wf_rc" -eq 0 ] || wf_bad=$((wf_bad+1)) ;;
      reject) [ "$wf_rc" -ne 0 ] || wf_bad=$((wf_bad+1)) ;;
    esac
  done
  if [ "$wf_bad" -eq 0 ]; then
    echo "  PASS: deploy webfetch_approval_ok() enforces strict tomllib semantics"
    ok=$((ok+1))
  else
    echo "  FAIL: deploy webfetch_approval_ok() misclassified a fixture"
    fail=$((fail+1))
  fi
else
  echo "  FAIL: could not prepare webfetch_approval_ok() fixtures"
  fail=$((fail+1))
fi

# drift 側と同じ実装であることを静的ガードで固定し、step2 が grep -A8 に
# 戻らないことも同時に固定する（共有 lib 抽出は別タスク）。
drift_wf="$(sed -n '/^webfetch_approval_ok()/,/^}/p' "$drift_src")"
deploy_wf="$(sed -n '/^webfetch_approval_ok()/,/^}/p' "$deploy_script")"
# shellcheck disable=SC2016
step2_body="$(sed -n '/^if ! webfetch_approval_ok "\$allowlist"; then$/,/^fi$/p' "$deploy_script")"
# shellcheck disable=SC2016
# Literal grep pattern below, not a shell expansion.
if [ -n "$drift_wf" ] \
   && [ "$deploy_wf" = "$drift_wf" ] \
   && printf '%s' "$step2_body" | grep -Fq 'if ! webfetch_approval_ok "$allowlist"; then' \
   && ! printf '%s' "$step2_body" | grep -Fq 'grep -A8'; then
  echo "  PASS: deploy webfetch_approval_ok() matches drift and step2 uses it"
  ok=$((ok+1))
else
  echo "  FAIL: deploy web.fetch approval gate diverged from drift"
  fail=$((fail+1))
fi

# b4-scripts / 2026-11-28 (3): rollback の PENDING(1) 経路でも、配置先
# バイナリが実行不能なら「旧も新も無い」ため DANGER(exit 3) へ昇格する。
# 上記で抽出済みの drift_gate 本文を stub 実行し、実 drift / systemctl は触らない。
pending_bin="$tmp/pending_gw_bin"
printf 'stub\n' > "$pending_bin"
chmod 0755 -- "$pending_bin"
# shellcheck disable=SC2034
# deployed_bin is read by the eval'd drift gate.
pending_drift_gate() { # bin_path -> exit code
  local stub_bin="$1"
  (
    deployed_bin="$stub_bin"
    # shellcheck disable=SC2329
    bash() { return 1; }
    # shellcheck disable=SC1090,SC2086
    eval "$drift_gate"
    # 抽出範囲は esac までなので、昇格が起きなければ rollback 本体と同じ exit 1。
    exit 1
  ) >/dev/null 2>&1
}
pending_drift_gate "$pending_bin"; pending_present_rc=$?
pending_drift_gate "$tmp/no-such-gw-binary"; pending_absent_rc=$?
if [ "$pending_present_rc" -eq 1 ] && [ "$pending_absent_rc" -eq 3 ]; then
  echo "  PASS: rollback escalates PENDING with a missing binary to exit 3"
  ok=$((ok+1))
else
  echo "  FAIL: rollback swallowed a missing binary under PENDING"
  fail=$((fail+1))
fi

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
