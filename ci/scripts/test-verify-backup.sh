#!/usr/bin/env bash
# Regression tests for verify-backup.sh using synthetic backup sets.
# No production backup data is touched.
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo/infra/scripts/verify-backup.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

ok=0; fail=0
check() { # label expected_exit extra_args...
  local label="$1" expected="$2"; shift 2
  local out code
  out=$(bash "$script" "$@" 2>&1); code=$?
  if [ "$code" -eq "$expected" ]; then
    echo "  PASS: $label (exit $code)"
    ok=$((ok+1))
  else
    echo "  FAIL: $label expected=$expected actual=$code"
    echo "$out" | head -5 >&2
    fail=$((fail+1))
  fi
}

base="$tmp/backups"
mkdir -p "$base/good" "$base/corrupt" "$base/partial" "$base/legacy"

# GOOD: complete set with valid hashes and marker.
printf 'content-A' > "$base/good/a.txt"
printf 'hash' > "$base/good/unused"
sha=$(sha256sum "$base/good/a.txt" | cut -d' ' -f1)
printf '# created_at=2026-11-17T00:00:00Z\na.txt 9 %s\n' "$sha" > "$base/good/manifest.txt"
printf 'complete 2026-11-17T00:00:00Z\n' > "$base/good/COMPLETE"

# CORRUPT: same manifest but file content changed.
printf 'tampered' > "$base/corrupt/a.txt"
printf '# created_at=2026-11-17T00:00:00Z\na.txt 9 %s\n' "$sha" > "$base/corrupt/manifest.txt"
printf 'complete 2026-11-17T00:00:00Z\n' > "$base/corrupt/COMPLETE"

# PARTIAL: valid hashes but no COMPLETE marker, with created_at header.
printf 'content-A' > "$base/partial/a.txt"
printf '# created_at=2026-11-17T00:00:00Z\na.txt 9 %s\n' "$sha" > "$base/partial/manifest.txt"

# LEGACY: no created_at header, no marker (accepted for compatibility).
printf 'content-A' > "$base/legacy/a.txt"
printf 'a.txt 9 %s\n' "$sha" > "$base/legacy/manifest.txt"

check "good set passes" 0 "$base/good"
check "corrupt set fails" 1 "$base/corrupt"
check "partial set fails" 1 "$base/partial"
check "legacy set accepted" 0 "$base/legacy"
check "all sets: corrupt detected" 1 --backup-dir "$base"
check "latest set detects partial" 1 --latest --backup-dir "$base"

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
