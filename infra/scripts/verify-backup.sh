#!/usr/bin/env bash
# Verify a local backup set's integrity against its SHA-256 manifest.
# Read-only: never modifies or deletes backup data. Runs as any user that can
# read the destination (typically postgres via sudo -u postgres).
set -uo pipefail

usage() {
  cat <<'USAGE'
Usage: verify-backup.sh [backup-set-dir | --latest] [--backup-dir DIR]
  Default: verify every complete set under /var/lib/postgresql/jarvis-backups.
  Prints per-file OK/MISMATCH/MISSING and exits 1 if any set is corrupt.
USAGE
}

target=""
backup_dir="/var/lib/postgresql/jarvis-backups"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --latest) target="__latest__"; shift ;;
    --backup-dir) backup_dir="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) target="$1"; shift ;;
  esac
done

if [ "$target" = "__latest__" ]; then
  target="$(find "$backup_dir" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort | tail -1)"
  [ -n "$target" ] || { echo "ERROR: no backup sets found" >&2; exit 1; }
fi

if [ -n "$target" ]; then
  sets=("$target")
else
  sets=()
  while IFS= read -r line; do sets+=("$line"); done < <(find "$backup_dir" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort)
  if [ "${#sets[@]}" -eq 0 ]; then echo "ERROR: no backup sets found" >&2; exit 1; fi
fi

fail=0; ok=0
for set in "${sets[@]}"; do
  manifest="$set/manifest.txt"
  echo "== $(basename "$set") =="
  if [ ! -f "$manifest" ]; then
    echo "  FAIL: manifest.txt missing (incomplete backup)"
    fail=$((fail+1))
    continue
  fi
  # A partially failed run leaves a set whose recorded files are all valid.
  # Require the marker written only after every step succeeded. Sets created
  # before the marker existed are identified by a manifest header lacking the
  # created_at field; those are legacy candidates, not a blanket pass.
  created_at="$(sed -n 's/^# created_at=//p' "$manifest" | head -1)"
  if [ -f "$set/COMPLETE" ]; then
    echo "  COMPLETE marker: present"
    ok=$((ok+1))
  elif [ -z "$created_at" ]; then
    echo "  COMPLETE marker: absent (legacy set)"
  else
    echo "  FAIL: COMPLETE marker missing (partial run)"
    fail=$((fail+1))
  fi

  # An empty / header-only manifest records nothing verifiable. Reject it so a
  # vacuous manifest cannot escape integrity checks.
  manifest_entries="$(grep -c -v -e '^#' -e '^[[:space:]]*$' "$manifest" || true)"
  if [ "$manifest_entries" -eq 0 ]; then
    echo "  FAIL: manifest has no file entries"
    fail=$((fail+1))
  fi

  # Mandatory payloads must exist on disk for EVERY set with a manifest,
  # including legacy sets. This check used to be gated behind the COMPLETE
  # marker, so a manifest-only directory (no created_at, no payloads) was
  # reported as an accepted legacy set and exited 0. Requiring the payloads
  # unconditionally limits the legacy exemption to sets that actually carry
  # postgres / qdrant / config-*; empty manifests and zero payloads now fail.
  for required in postgres-jarvis.sql.gz qdrant-snapshot.bin; do
    if [ ! -f "$set/$required" ]; then
      echo "  REQUIRED MISSING: $required"
      fail=$((fail+1))
    fi
  done
  config_found=0
  for config in "$set"/config-*; do
    [ -f "$config" ] || continue
    config_found=1
    break
  done
  if [ "$config_found" -eq 0 ]; then
    echo "  REQUIRED MISSING: config-* (at least one config file)"
    fail=$((fail+1))
  fi

  while read -r name expected_bytes expected_sha; do
    case "$name" in ''|\#*) continue ;; esac
    path="$set/$name"
    if [ ! -f "$path" ]; then
      echo "  MISSING: $name"
      fail=$((fail+1))
      continue
    fi
    actual_bytes="$(wc -c < "$path" | tr -d ' ')"
    actual_sha="$(sha256sum "$path" | cut -d' ' -f1)"
    if [ "$actual_bytes" = "$expected_bytes" ] && [ "$actual_sha" = "$expected_sha" ]; then
      echo "  OK: $name ($actual_bytes bytes)"
      ok=$((ok+1))
    else
      echo "  MISMATCH: $name (size $actual_bytes/$expected_bytes sha ${actual_sha:0:12}/${expected_sha:0:12})"
      fail=$((fail+1))
    fi
  done < "$manifest"

  # Format-level checks: prove the payloads are structurally readable
  # (does NOT restore or touch production data).
  if [ -f "$set/postgres-jarvis.sql.gz" ]; then
    if gzip -t "$set/postgres-jarvis.sql.gz" 2>/dev/null; then
      echo "  FORMAT OK: postgres dump (gzip readable)"
      ok=$((ok+1))
      # A truncated dump can still pass gzip -t. Check the SQL footer marker
      # that pg_dump writes on a clean completion (no data values are read).
      if gzip -cd "$set/postgres-jarvis.sql.gz" 2>/dev/null | tail -5 | grep -q "PostgreSQL database dump complete"; then
        echo "  COMPLETE OK: postgres dump has pg_dump footer"
        ok=$((ok+1))
      else
        echo "  COMPLETE MISMATCH: postgres dump missing footer (truncated?)"
        fail=$((fail+1))
      fi
    else
      echo "  FORMAT MISMATCH: postgres dump gzip corrupt"
      fail=$((fail+1))
    fi
  fi
  if [ -f "$set/qdrant-snapshot.bin" ]; then
    if tar -tf "$set/qdrant-snapshot.bin" >/dev/null 2>&1; then
      echo "  FORMAT OK: qdrant snapshot (tar readable)"
      ok=$((ok+1))
    else
      echo "  FORMAT MISMATCH: qdrant snapshot unreadable"
      fail=$((fail+1))
    fi
  fi
  for archive in "$set"/config-*.tar; do
    [ -f "$archive" ] || continue
    if tar -tf "$archive" >/dev/null 2>&1; then
      echo "  FORMAT OK: $(basename "$archive") (tar readable)"
      ok=$((ok+1))
    else
      echo "  FORMAT MISMATCH: $(basename "$archive") unreadable"
      fail=$((fail+1))
    fi
  done
done

echo "== result: ok=$ok fail=$fail =="
[ "$fail" -eq 0 ]
