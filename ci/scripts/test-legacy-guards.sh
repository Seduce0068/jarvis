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

tamper_evidence() { # src dst mode
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys

src, dst, mode = sys.argv[1:4]
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

  for mode in hash failed empty_passed timestamp calendar failed_bool passed_bool; do
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

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
