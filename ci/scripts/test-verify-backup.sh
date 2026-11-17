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

assert_true() { # label command args...
  local label="$1"; shift
  if "$@"; then
    echo "  PASS: $label"
    ok=$((ok+1))
  else
    echo "  FAIL: $label"
    fail=$((fail+1))
  fi
}

assert_false() { # label command args...
  local label="$1"; shift
  if "$@"; then
    echo "  FAIL: $label (unexpected success)"
    fail=$((fail+1))
  else
    echo "  PASS: $label"
    ok=$((ok+1))
  fi
}

check_includes() { # label expected_exit pattern args...
  local label="$1" expected="$2" pattern="$3"; shift 3
  local out code
  out=$(bash "$script" "$@" 2>&1); code=$?
  if [ "$code" -eq "$expected" ] && printf '%s' "$out" | grep -q -- "$pattern"; then
    echo "  PASS: $label (exit $code, output matches)"
    ok=$((ok+1))
  else
    echo "  FAIL: $label expected=$expected actual=$code pattern=$pattern"
    printf '%s\n' "$out" | head -5 >&2
    fail=$((fail+1))
  fi
}

# Synthetic sentinel only; no real secret value is ever written or read.
sentinel='DUMMY_SENTINEL_DO_NOT_USE'

# Inspect the decompressed/decomposed payload of a single archive. The grep
# deliberately reads to EOF (no -q) so pipefail cannot mask a match when the
# decompressor exits early on SIGPIPE.
archive_has_sentinel() { # archive
  case "$1" in
    *.gz)
      gzip -cd "$1" 2>/dev/null | grep -a -- "$sentinel" >/dev/null 2>&1
      ;;
    *.tar|*.bin)
      # Qdrant snapshots are tar streams despite their .bin extension.
      tar -xOf "$1" 2>/dev/null | grep -a -- "$sentinel" >/dev/null 2>&1
      ;;
    *)
      return 1
      ;;
  esac
}

sentinel_absent_in_archives() { # dir
  local archive
  while IFS= read -r archive; do
    if archive_has_sentinel "$archive"; then
      return 1
    fi
  done < <(find "$1" -type f \( -name '*.gz' -o -name '*.tar' -o -name '*.bin' \))
  return 0
}

sentinel_absent() { # dir
  if grep -R -a -q -- "$sentinel" "$1"; then
    return 1
  fi
  sentinel_absent_in_archives "$1"
}

no_secret_files() { # dir
  [ -z "$(find "$1" -type f \( -name '*.env' -o -name '*token*' \) )" ]
}

manifest_matches() { # dir name
  local dir="$1" name="$2" actual_bytes actual_sha recorded
  actual_bytes="$(wc -c < "$dir/$name" | tr -d ' ')"
  actual_sha="$(sha256sum "$dir/$name" | cut -d' ' -f1)"
  recorded="$(awk -v n="$name" '$1 == n { print $2, $3; exit }' "$dir/manifest.txt")"
  [ "$recorded" = "$actual_bytes $actual_sha" ]
}

record_manifest_entries() { # dir name...
  local dir="$1"; shift
  printf '# created_at=2026-11-18T00:00:00Z\n'
  local name bytes sha
  for name in "$@"; do
    bytes="$(wc -c < "$dir/$name" | tr -d ' ')"
    sha="$(sha256sum "$dir/$name" | cut -d' ' -f1)"
    printf '%s %s %s\n' "$name" "$bytes" "$sha"
  done
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

# TC-B3: model the backup allowlist with synthetic files. The staging dir holds
# a dummy secret marker that must never appear in a backup artifact. This does
# not execute backup-local.sh or touch /etc/jarvis.
extra="$tmp/extra"
mkdir -p "$extra" "$tmp/staging"
printf '%s\n' "$sentinel" > "$tmp/staging/bws.env"
printf '%s\n' "$sentinel" > "$tmp/staging/token.txt"
printf 'allow = ["tool.read"]\n' > "$tmp/staging/mcp_allowlist.toml"

no_secrets="$extra/no_secrets"
mkdir -p "$no_secrets"
cp "$tmp/staging/mcp_allowlist.toml" "$no_secrets/config-mcp_allowlist.toml"
record_manifest_entries "$no_secrets" config-mcp_allowlist.toml > "$no_secrets/manifest.txt"
printf 'complete 2026-11-18T00:00:00Z\n' > "$no_secrets/COMPLETE"

check "TC-B3: secret-free set verifies" 0 "$no_secrets"
assert_true "TC-B3: sentinel absent from artifacts" sentinel_absent "$no_secrets"
assert_true "TC-B3: no .env/token files copied" no_secret_files "$no_secrets"
assert_true "TC-B3: manifest matches copied allowlist" manifest_matches "$no_secrets" config-mcp_allowlist.toml

# TC-V4: format-level checks plus manifest size/hash integrity. A valid gzip
# dump (with the pg_dump footer) and valid tar archives must pass; corrupt
# streams and a dump missing the footer must be rejected.
format_ok="$extra/format_ok"
mkdir -p "$format_ok" "$tmp/qdrant-src" "$tmp/policy-src"
printf '%s\n' '-- PostgreSQL database dump' 'CREATE TABLE t (id int);' \
  '-- PostgreSQL database dump complete' | gzip > "$format_ok/postgres-jarvis.sql.gz"
printf '%s\n' 'snapshot payload' > "$tmp/qdrant-src/snapshot.json"
tar -cf "$format_ok/qdrant-snapshot.bin" -C "$tmp/qdrant-src" snapshot.json
printf '%s\n' 'p, agent, tool.read, allow' > "$tmp/policy-src/policy.csv"
tar -cf "$format_ok/config-policy.tar" -C "$tmp/policy-src" policy.csv
record_manifest_entries "$format_ok" postgres-jarvis.sql.gz qdrant-snapshot.bin \
  config-policy.tar > "$format_ok/manifest.txt"
printf 'complete 2026-11-18T00:00:00Z\n' > "$format_ok/COMPLETE"

check "TC-V4: valid gzip/tar set verifies" 0 "$format_ok"
check_includes "TC-V4: postgres gzip format checked" 0 "FORMAT OK: postgres dump" "$format_ok"
check_includes "TC-V4: qdrant tar format checked" 0 "FORMAT OK: qdrant snapshot" "$format_ok"
check_includes "TC-V4: config tar format checked" 0 "FORMAT OK: config-policy.tar" "$format_ok"
for name in postgres-jarvis.sql.gz qdrant-snapshot.bin config-policy.tar; do
  assert_true "TC-V4: manifest size/hash matches $name" manifest_matches "$format_ok" "$name"
done

format_bad="$extra/format_bad"
mkdir -p "$format_bad"
printf '%s\n' 'not a gzip stream' > "$format_bad/postgres-jarvis.sql.gz"
printf '%s\n' 'not a tar archive' > "$format_bad/qdrant-snapshot.bin"
record_manifest_entries "$format_bad" postgres-jarvis.sql.gz qdrant-snapshot.bin \
  > "$format_bad/manifest.txt"
printf 'complete 2026-11-18T00:00:00Z\n' > "$format_bad/COMPLETE"
check "TC-V4: corrupt gzip/tar rejected" 1 "$format_bad"

format_truncated="$extra/format_truncated"
mkdir -p "$format_truncated"
printf '%s\n' '-- PostgreSQL database dump' | gzip > "$format_truncated/postgres-jarvis.sql.gz"
record_manifest_entries "$format_truncated" postgres-jarvis.sql.gz \
  > "$format_truncated/manifest.txt"
printf 'complete 2026-11-18T00:00:00Z\n' > "$format_truncated/COMPLETE"
check "TC-V4: dump without pg_dump footer rejected" 1 "$format_truncated"

# TC-B3 (archive depth): a raw grep cannot see into compressed payloads. The
# leak fixtures below use compressed archives so the new decompression-aware
# branch is the only way to detect the sentinel.
assert_true "TC-B3: sentinel absent inside valid archives" sentinel_absent "$format_ok"

leak_gz="$extra/leak_gz"
mkdir -p "$leak_gz"
printf '%s\n' "$sentinel" | gzip > "$leak_gz/leak.sql.gz"
assert_false "TC-B3: sentinel in gzip payload detected" sentinel_absent "$leak_gz"

leak_tar="$extra/leak_tar"
mkdir -p "$leak_tar" "$tmp/leak-src"
printf '%s\n' "$sentinel" > "$tmp/leak-src/token.txt"
tar -czf "$leak_tar/leak.tar" -C "$tmp/leak-src" token.txt
assert_false "TC-B3: sentinel in compressed tar member detected" sentinel_absent "$leak_tar"

leak_bin="$extra/leak_bin"
mkdir -p "$leak_bin"
tar -czf "$leak_bin/qdrant-snapshot.bin" -C "$tmp/leak-src" token.txt
assert_false "TC-B3: sentinel in tar-in-.bin member detected" sentinel_absent "$leak_bin"

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
