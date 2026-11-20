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

# Mirror of check_includes for fail-closed assertions: the pattern must NOT be
# present. grep reads to EOF (no -q) so pipefail cannot invert the result.
check_excludes() { # label expected_exit pattern args...
  local label="$1" expected="$2" pattern="$3"; shift 3
  local out code
  out=$(bash "$script" "$@" 2>&1); code=$?
  if [ "$code" -eq "$expected" ] && \
     ! printf '%s\n' "$out" | grep -F -- "$pattern" >/dev/null 2>&1; then
    echo "  PASS: $label (exit $code, output does not match)"
    ok=$((ok+1))
  else
    echo "  FAIL: $label expected=$expected actual=$code forbidden_pattern=$pattern"
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

# Write a 7-column descriptor manifest:
#   name size sha256 format encryption signature role
# Each spec is "name|format|encryption|signature|role"; size and SHA-256 are
# computed from the synthetic payload only. No key or signature value appears.
record_described_manifest_entries() { # dir spec...
  local dir="$1" spec name format encryption signature role bytes sha
  shift
  printf '# created_at=2026-11-20T00:00:00Z\n'
  for spec in "$@"; do
    IFS='|' read -r name format encryption signature role <<< "$spec"
    bytes="$(wc -c < "$dir/$name" | tr -d ' ')"
    sha="$(sha256sum "$dir/$name" | cut -d' ' -f1)"
    printf '%s %s %s %s %s %s %s\n' \
      "$name" "$bytes" "$sha" "$format" "$encryption" "$signature" "$role"
  done
}

base="$tmp/backups"
mkdir -p "$base/good" "$base/corrupt" "$base/partial" "$base/legacy" \
  "$base/legacy_empty" "$base/legacy_trivial"

# GOOD: complete set with valid hashes, required payloads and marker.
printf 'content-A' > "$base/good/a.txt"
printf 'hash' > "$base/good/unused"
sha=$(sha256sum "$base/good/a.txt" | cut -d' ' -f1)
{ printf '%s\n' '-- PostgreSQL database dump' '-- PostgreSQL database dump complete'; } \
  | gzip > "$base/good/postgres-jarvis.sql.gz"
mkdir -p "$tmp/good-qdrant"
printf 'snapshot' > "$tmp/good-qdrant/snapshot.json"
tar -cf "$base/good/qdrant-snapshot.bin" -C "$tmp/good-qdrant" snapshot.json
printf 'allow = ["tool.read"]\n' > "$base/good/config-mcp_allowlist.toml"
{
  printf '# created_at=2026-11-17T00:00:00Z\n'
  record_manifest_entries "$base/good" a.txt postgres-jarvis.sql.gz \
    qdrant-snapshot.bin config-mcp_allowlist.toml
} > "$base/good/manifest.txt"
printf 'complete 2026-11-17T00:00:00Z\n' > "$base/good/COMPLETE"

# CORRUPT: same manifest but file content changed.
printf 'tampered' > "$base/corrupt/a.txt"
printf '# created_at=2026-11-17T00:00:00Z\na.txt 9 %s\n' "$sha" > "$base/corrupt/manifest.txt"
printf 'complete 2026-11-17T00:00:00Z\n' > "$base/corrupt/COMPLETE"

# PARTIAL: valid hashes but no COMPLETE marker, with created_at header.
printf 'content-A' > "$base/partial/a.txt"
printf '# created_at=2026-11-17T00:00:00Z\na.txt 9 %s\n' "$sha" > "$base/partial/manifest.txt"

# LEGACY: no created_at header and no marker, but WITH full payloads. The
# exemption now applies only because postgres/qdrant/config-* really exist.
printf 'content-A' > "$base/legacy/a.txt"
{ printf '%s\n' '-- PostgreSQL database dump' '-- PostgreSQL database dump complete'; } \
  | gzip > "$base/legacy/postgres-jarvis.sql.gz"
mkdir -p "$tmp/legacy-qdrant"
printf 'snapshot' > "$tmp/legacy-qdrant/snapshot.json"
tar -cf "$base/legacy/qdrant-snapshot.bin" -C "$tmp/legacy-qdrant" snapshot.json
printf 'allow = ["tool.read"]\n' > "$base/legacy/config-mcp_allowlist.toml"
record_manifest_entries "$base/legacy" a.txt postgres-jarvis.sql.gz \
  qdrant-snapshot.bin config-mcp_allowlist.toml | tail -n +2 > "$base/legacy/manifest.txt"

# LEGACY-EMPTY: header-less, empty manifest, no payloads at all (rejected).
: > "$base/legacy_empty/manifest.txt"

# LEGACY-TRIVIAL: header-less manifest listing only a trivial file, no
# postgres/qdrant/config payloads (rejected).
printf 'content-A' > "$base/legacy_trivial/a.txt"
printf 'a.txt 9 %s\n' "$sha" > "$base/legacy_trivial/manifest.txt"

check "good set passes" 0 "$base/good"
check "corrupt set fails" 1 "$base/corrupt"
check "partial set fails" 1 "$base/partial"
check "legacy set with full payloads accepted" 0 "$base/legacy"
check "legacy set with empty manifest rejected" 1 "$base/legacy_empty"
check "trivial-only set rejected" 1 "$base/legacy_trivial"
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
{ printf '%s\n' '-- PostgreSQL database dump' '-- PostgreSQL database dump complete'; } \
  | gzip > "$no_secrets/postgres-jarvis.sql.gz"
mkdir -p "$tmp/no-secrets-qdrant"
printf 'snapshot' > "$tmp/no-secrets-qdrant/snapshot.json"
tar -cf "$no_secrets/qdrant-snapshot.bin" -C "$tmp/no-secrets-qdrant" snapshot.json
record_manifest_entries "$no_secrets" config-mcp_allowlist.toml \
  postgres-jarvis.sql.gz qdrant-snapshot.bin > "$no_secrets/manifest.txt"
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

# TC-B6 / TC-V5 (REQ-19 / REQ-20): 7-column descriptor manifests.
# The defaults are format-by-extension, encryption=none, signature=none plus an
# explicit role -- the same effective descriptors a legacy 3-column row derives.
described="$extra/described"
mkdir -p "$described" "$tmp/described-qdrant"
printf '%s\n' '-- PostgreSQL database dump' 'CREATE TABLE t (id int);' \
  '-- PostgreSQL database dump complete' | gzip > "$described/postgres-jarvis.sql.gz"
printf 'snapshot' > "$tmp/described-qdrant/snapshot.json"
tar -cf "$described/qdrant-snapshot.bin" -C "$tmp/described-qdrant" snapshot.json
printf 'allow = ["tool.read"]\n' > "$described/config-mcp_allowlist.toml"
record_described_manifest_entries "$described" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  > "$described/manifest.txt"
printf 'complete 2026-11-20T00:00:00Z\n' > "$described/COMPLETE"

check "TC-B6: 7-column none/none set accepted (backward compatible)" 0 "$described"
check_includes "TC-B6: postgres role descriptor drives footer check" 0 \
  "FORMAT OK: postgres dump" "$described"
check_includes "TC-B6: qdrant role descriptor drives tar check" 0 \
  "FORMAT OK: qdrant snapshot" "$described"

# TC-V5: an explicit but unknown descriptor must be rejected fail-closed. The
# payload itself is byte-identical to the accepted set, so only the descriptor
# column can cause the rejection. The values below are vocabulary probes, not
# secrets and not supported algorithms.
unknown_enc="$extra/described_unknown_enc"
cp -R -- "$described" "$unknown_enc"
record_described_manifest_entries "$unknown_enc" \
  'postgres-jarvis.sql.gz|gzip|age|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  > "$unknown_enc/manifest.txt"
check "TC-V5: unknown encryption rejected" 1 "$unknown_enc"
check_includes "TC-V5: unknown encryption reported" 1 \
  "encryption=age" "$unknown_enc"
check_excludes "TC-V5: no format check for unknown encryption row" 1 \
  "FORMAT OK: postgres dump" "$unknown_enc"

unknown_sig="$extra/described_unknown_sig"
cp -R -- "$described" "$unknown_sig"
record_described_manifest_entries "$unknown_sig" \
  'postgres-jarvis.sql.gz|gzip|none|ed25519|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  > "$unknown_sig/manifest.txt"
check "TC-V5: unknown signature rejected" 1 "$unknown_sig"
check_includes "TC-V5: unknown signature reported" 1 \
  "signature=ed25519" "$unknown_sig"
check_excludes "TC-V5: no format check for unknown signature row" 1 \
  "FORMAT OK: postgres dump" "$unknown_sig"

unknown_fmt="$extra/described_unknown_fmt"
cp -R -- "$described" "$unknown_fmt"
record_described_manifest_entries "$unknown_fmt" \
  'postgres-jarvis.sql.gz|zstd|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  > "$unknown_fmt/manifest.txt"
check "TC-V5: unknown format rejected" 1 "$unknown_fmt"
check_includes "TC-V5: unknown format reported" 1 \
  "format=zstd" "$unknown_fmt"
check_excludes "TC-V5: no format check for unknown format row" 1 \
  "FORMAT OK: postgres dump" "$unknown_fmt"

# I78-A (role unification): verify-backup.sh must resolve a row's role through
# the same resolved_role_for() used by restore-local.sh's verify_set() and
# resolve_restore_input(). Unknown roles and role/basename contradictions must
# fail closed. Each fixture is byte-identical to the accepted `described` set
# except for the role column, so only the role verdict can change the result.

# Unknown role vocabulary probe (not a secret or a supported role value).
unknown_role="$extra/described_unknown_role"
cp -R -- "$described" "$unknown_role"
record_described_manifest_entries "$unknown_role" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|bogus' \
  > "$unknown_role/manifest.txt"
check "I78-A: unknown role rejected" 1 "$unknown_role"
check_includes "I78-A: unknown role reported" 1 \
  "unsupported role descriptor: bogus" "$unknown_role"
check_excludes "I78-A: no format check for unknown role row" 1 \
  "FORMAT OK: config-mcp_allowlist.toml" "$unknown_role"

# A role the real basename cannot back (I74-B) must still fail closed.
role_mismatch="$extra/described_role_mismatch"
cp -R -- "$described" "$role_mismatch"
printf 'allow = ["tool.read"]\n' > "$role_mismatch/readme.txt"
record_described_manifest_entries "$role_mismatch" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'readme.txt|raw|none|none|postgres' \
  > "$role_mismatch/manifest.txt"
check "I78-A: role contradicting basename rejected" 1 "$role_mismatch"
check_includes "I78-A: role/basename mismatch reported" 1 \
  "ROLE MISMATCH" "$role_mismatch"

# I77-A drift: the old standalone role_basename_consistent() gate accepted
# `other` on config-*/git*.tar.gz, while the shared resolver rejects it. These
# two cases pin verify-backup.sh to the shared verdict.
other_config="$extra/described_other_config"
cp -R -- "$described" "$other_config"
record_described_manifest_entries "$other_config" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|other' \
  > "$other_config/manifest.txt"
check "I78-A: other on config-* rejected" 1 "$other_config"
check_includes "I78-A: other/config drift is a role mismatch" 1 \
  "ROLE MISMATCH" "$other_config"

other_git="$extra/described_other_git"
cp -R -- "$described" "$other_git"
mkdir -p "$tmp/other-git-src"
printf '%s\n' 'HEAD' > "$tmp/other-git-src/HEAD"
tar -czf "$other_git/git-extra.tar.gz" -C "$tmp/other-git-src" HEAD
record_described_manifest_entries "$other_git" \
  'postgres-jarvis.sql.gz|gzip|none|none|postgres' \
  'qdrant-snapshot.bin|tar|none|none|qdrant' \
  'config-mcp_allowlist.toml|raw|none|none|config' \
  'git-extra.tar.gz|gzip|none|none|other' \
  > "$other_git/manifest.txt"
check "I78-A: other on git*.tar.gz rejected" 1 "$other_git"
check_includes "I78-A: other/git drift is a role mismatch" 1 \
  "ROLE MISMATCH" "$other_git"

# Positive control: a legacy 3-column git row still derives its role from the
# basename through the shared resolver, so unification adds no false rejection.
legacy_git="$extra/legacy_git"
cp -R -- "$described" "$legacy_git"
mkdir -p "$tmp/legacy-git-src"
printf '%s\n' 'HEAD' > "$tmp/legacy-git-src/HEAD"
tar -czf "$legacy_git/git-extra.tar.gz" -C "$tmp/legacy-git-src" HEAD
record_manifest_entries "$legacy_git" postgres-jarvis.sql.gz \
  qdrant-snapshot.bin config-mcp_allowlist.toml git-extra.tar.gz \
  > "$legacy_git/manifest.txt"
check "I78-A: legacy git row derives role from basename" 0 "$legacy_git"

# I83-B: a bare --backup-dir must be rejected while parsing arguments (exit 2).
# The error must not echo an argument value or bash's `unbound variable` crash
# text, and the read-only verifier must leave an existing good set intact.
backup_dir_missing_value_guard() {
  local out code
  out=$(bash "$script" --backup-dir 2>&1); code=$?
  [ "$code" -eq 2 ] || return 1
  case "$out" in
    *'ERROR: --backup-dir requires a value'*) ;;
    *) return 1 ;;
  esac
  case "$out" in
    *"$base"*|*'unbound variable'*) return 1 ;;
  esac
  [ -f "$base/good/manifest.txt" ] || return 1
  bash "$script" "$base/good" >/dev/null 2>&1 || return 1
  return 0
}
assert_true "I83-B: --backup-dir missing value guard" backup_dir_missing_value_guard

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
