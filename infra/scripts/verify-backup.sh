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
# Payload descriptors: the single derivation path for mandatory roles and
# format checks. The row contract is defined in backup-local.sh.
# Legacy manifests record only "name size sha256"; those rows are accepted by
# defaulting the descriptors to the historical extension/name mapping. A
# descriptor that is explicitly present but unknown is rejected (fail-closed).
# ---------------------------------------------------------------------------
legacy_format_for() { # name -> format
  case "$1" in
    *.gz) printf 'gzip\n' ;;
    *.tar|*.bin) printf 'tar\n' ;;
    *) printf 'raw\n' ;;
  esac
}

legacy_role_for() { # name -> role
  case "$1" in
    postgres-jarvis.sql.gz) printf 'postgres\n' ;;
    qdrant-snapshot.bin) printf 'qdrant\n' ;;
    config-*) printf 'config\n' ;;
    git-jarvis-head.tar.gz) printf 'git\n' ;;
    *) printf 'other\n' ;;
  esac
}

resolve_descriptors() { # name format encryption signature role -> "fmt enc sig role"
  local name="$1" format="$2" encryption="$3" signature="$4" role="$5"
  [ -n "$format" ] || format="$(legacy_format_for "$name")"
  [ -n "$encryption" ] || encryption=none
  [ -n "$signature" ] || signature=none
  [ -n "$role" ] || role="$(legacy_role_for "$name")"
  printf '%s %s %s %s\n' "$format" "$encryption" "$signature" "$role"
}

descriptor_supported() { # format encryption signature -> 0/1
  local format="$1" encryption="$2" signature="$3"
  case "$format" in gzip|tar|raw) ;; *) return 1 ;; esac
  # P1-6: encryption/signature wiring (key injection point, decrypt timing) is
  # a user decision and is intentionally not implemented. Anything other than
  # the current none/none default is rejected here. Future support connects
  # here: "将来 P1-6 で暗号化を適用する時にここへ接続する".
  [ "$encryption" = none ] && [ "$signature" = none ]
}

check_payload_format() { # set name format encryption signature role
  local set="$1" name="$2" format="$3" encryption="$4" signature="$5" role="$6"
  local path="$set/$name" label="$name"
  if ! descriptor_supported "$format" "$encryption" "$signature"; then
    echo "  FORMAT MISMATCH: $name (unsupported descriptor format=$format encryption=$encryption signature=$signature)"
    fail=$((fail+1))
    return 0
  fi
  [ -f "$path" ] || return 0
  case "$role" in
    postgres) label="postgres dump" ;;
    qdrant) label="qdrant snapshot" ;;
  esac
  case "$format" in
    gzip)
      if gzip -t "$path" 2>/dev/null; then
        echo "  FORMAT OK: $label (gzip readable)"
        ok=$((ok+1))
        if [ "$role" = postgres ]; then
          # A truncated dump can still pass gzip -t. Check the pg_dump footer
          # written on a clean completion (no data values are read).
          if gzip -cd "$path" 2>/dev/null | tail -5 | grep -q "PostgreSQL database dump complete"; then
            echo "  COMPLETE OK: postgres dump has pg_dump footer"
            ok=$((ok+1))
          else
            echo "  COMPLETE MISMATCH: postgres dump missing footer (truncated?)"
            fail=$((fail+1))
          fi
        fi
      else
        echo "  FORMAT MISMATCH: $label gzip corrupt"
        fail=$((fail+1))
      fi
      ;;
    tar)
      if tar -tf "$path" >/dev/null 2>&1; then
        echo "  FORMAT OK: $label (tar readable)"
        ok=$((ok+1))
      else
        echo "  FORMAT MISMATCH: $label unreadable"
        fail=$((fail+1))
      fi
      ;;
    raw)
      # Presence, size and SHA-256 were checked by the manifest loop below.
      echo "  FORMAT OK: $name (raw payload present)"
      ok=$((ok+1))
      ;;
  esac
}

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
  # postgres / qdrant / config-*. Roles and formats are now derived from the
  # manifest descriptors by the single path below, not from fixed filenames.
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
    case "$role" in
      postgres) has_pg=1 ;;
      qdrant) has_qdrant=1 ;;
      config) has_config=1 ;;
    esac
    # Format-level check: prove the payload is structurally readable without
    # restoring or touching production data. Dispatch is descriptor-driven.
    check_payload_format "$set" "$name" "$format" "$encryption" "$signature" "$role"
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
