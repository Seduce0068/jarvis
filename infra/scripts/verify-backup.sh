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

# ---------------------------------------------------------------------------
# Payload descriptors and format checks: one shared implementation.
# The row contract is defined in backup-local.sh; legacy 3-column rows default
# their descriptors to the historical name/extension mapping and explicit
# unknown descriptors fail closed. Both this script and restore-local.sh call
# the SAME functions from lib/payload-descriptors.sh, so their verdicts cannot
# diverge and P1-6 encryption needs one update point only.
# Deploy note: install-backup-timer.sh and `make install-verify-backup` copy this
# file alone to /opt/jarvis/scripts; they must also install the lib/ directory
# next to it, or the installed verifier fails closed with a clear error.
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=lib/payload-descriptors.sh
# Resolved at runtime relative to the script location.
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/payload-descriptors.sh"
[ "${PAYLOAD_DESCRIPTORS_LIB:-}" = 1 ] || {
  echo "ERROR: shared payload descriptor library not loaded" >&2
  exit 1
}

# Map the shared verdict to this script's output/counters. The classification
# itself lives only in the shared library.
on_payload_check() { # event detail
  local event="$1" detail="$2"
  case "$event" in
    FORMAT_OK|COMPLETE_OK)
      echo "  ${event//_/ }: $detail"
      ok=$((ok+1))
      ;;
    FORMAT_MISMATCH|COMPLETE_MISMATCH)
      echo "  ${event//_/ }: $detail"
      fail=$((fail+1))
      ;;
  esac
}

# I74-B helpers (is_*_payload, role_basename_consistent, pg_dump_footer_present)
# now live in lib/payload-descriptors.sh and are shared with restore-local.sh.

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
  # postgres / qdrant / config-*. The manifest role is auxiliary metadata:
  # presence comes from the real basename, so a self-declared role=postgres on
  # an unrelated file (or a truncated dump relabelled format=raw) can no
  # longer satisfy a mandatory component.
  has_pg=0; has_qdrant=0; has_config=0
  while read -r name expected_bytes expected_sha format encryption signature role; do
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

    # An explicit role must be in the vocabulary; an empty role marks a legacy
    # row and is derived from the name below. Unknown values fail closed.
    case "$role" in
      ''|postgres|qdrant|config|git|other) ;;
      *)
        echo "  FORMAT MISMATCH: $name (unsupported role descriptor: $role)"
        fail=$((fail+1))
        continue
        ;;
    esac
    resolved="$(resolve_descriptors "$name" "$format" "$encryption" "$signature" "$role")"
    read -r format encryption signature role <<< "$resolved"
    # I74-B: role is auxiliary. A role claim that the basename does not back is
    # a fail-closed descriptor error and never counts as a required payload.
    if ! role_basename_consistent "$name" "$role"; then
      echo "  FORMAT MISMATCH: $name (role=$role is not consistent with its basename; ROLE MISMATCH)"
      fail=$((fail+1))
      continue
    fi
    # Mandatory presence comes from the real filename, not from the manifest's
    # self-declared role (I69-B "required payload really exists" contract).
    if is_pg_payload "$name"; then
      has_pg=1
    fi
    if is_qdrant_payload "$name"; then
      has_qdrant=1
    fi
    if is_config_payload "$name"; then
      has_config=1
    fi
    # Format-level check: the shared implementation classifies the payload and
    # routes the verdict through on_payload_check (ok/fail accounting above).
    payload_format_check "$path" "$name" "$format" "$encryption" "$signature" "$role"
  done < "$manifest"

  if [ "$has_pg" -eq 0 ]; then
    echo "  REQUIRED MISSING: postgres-jarvis.sql.gz"
    fail=$((fail+1))
  fi
  if [ "$has_qdrant" -eq 0 ]; then
    echo "  REQUIRED MISSING: qdrant-snapshot.bin"
    fail=$((fail+1))
  fi
  if [ "$has_config" -eq 0 ]; then
    echo "  REQUIRED MISSING: config-* (at least one config file)"
    fail=$((fail+1))
  fi
done

echo "== result: ok=$ok fail=$fail =="
[ "$fail" -eq 0 ]
