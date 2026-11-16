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
