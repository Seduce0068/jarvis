#!/usr/bin/env bash
# Payload descriptors: the single derivation path for mandatory roles and
# format checks shared by verify-backup.sh and restore-local.sh.
#
# Read-only. Nothing here restores or modifies a backup. The manifest row
# contract is defined in backup-local.sh:
#   name size sha256 [format encryption signature role]
# Legacy 3-column rows default the descriptors to the historical name/extension
# mapping; a descriptor that is explicitly present but unknown is rejected
# (fail-closed), so P1-6 encryption has exactly one update point.
#
# Sourcing this file defines the functions below and sets
# PAYLOAD_DESCRIPTORS_LIB=1 so a caller can assert the source actually happened.
# Callers override on_payload_check AFTER sourcing to map the shared verdict to
# their own logging/counters; the classification itself lives only here.

# Consumed by the sourcing scripts as a load assertion (SC2034 when checked
# standalone).
# shellcheck disable=SC2034
PAYLOAD_DESCRIPTORS_LIB=1

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

# --- I74-B: basename is the source of truth for component presence -----------
# The manifest role column is auxiliary metadata. A mandatory component is
# satisfied only by a payload whose REAL basename names that component, so
# `readme.txt <size> <sha> raw none none postgres` can never stand in for
# postgres-jarvis.sql.gz. A role claim is cross-checked against the basename;
# an inconsistent claim fails closed (unknown roles already do).
# I74-A/B reconciliation: the role column is authoritative for the payload
# kind, so accept any conventional extension (pg-data.sql.gz is valid) while
# still rejecting arbitrary files such as readme.txt that only claim a role.
is_pg_payload() { # name -> 0/1
  case "$1" in *.sql.gz) return 0 ;; *) return 1 ;; esac
}
is_qdrant_payload() { # name -> 0/1
  case "$1" in *.bin) return 0 ;; *) return 1 ;; esac
}
is_config_payload() { # name -> 0/1
  case "$1" in config-*) return 0 ;; *) return 1 ;; esac
}
is_git_payload() { # name -> 0/1
  case "$1" in git*.tar.gz) return 0 ;; *) return 1 ;; esac
}

role_basename_consistent() { # name role -> 0/1
  case "$2" in
    postgres) is_pg_payload "$1" ;;
    qdrant)   is_qdrant_payload "$1" ;;
    config)   is_config_payload "$1" ;;
    git)      is_git_payload "$1" ;;
    *)        return 0 ;; # other / legacy-empty: no basename obligation
  esac
}

pg_dump_footer_present() { # path -> 0/1
  local path="$1"
  # Probe the real bytes instead of trusting the declared format: a truncated
  # dump can be relabelled format=raw to bypass a gzip-only footer check. Only
  # the last five lines are inspected; no data values are read. grep -F without
  # -q reads to EOF so pipefail cannot mask a match on SIGPIPE.
  if gzip -cd "$path" 2>/dev/null | tail -5 \
     | grep -F "PostgreSQL database dump complete" >/dev/null 2>&1; then
    return 0
  fi
  if tail -5 "$path" 2>/dev/null \
     | grep -F "PostgreSQL database dump complete" >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

payload_label() { # name role -> human-readable label used in reports
  local name="$1" role="$2"
  case "$role" in
    postgres) printf 'postgres dump\n' ;;
    qdrant) printf 'qdrant snapshot\n' ;;
    *) printf '%s\n' "$name" ;;
  esac
}

# on_payload_check <event> <detail>
# Reporting hook. Each caller overrides this after sourcing to map the shared
# verdict to its own output and counters. The default is a no-op so sourcing
# the library alone has no side effects. Events:
#   FORMAT_OK | FORMAT_MISMATCH | COMPLETE_OK | COMPLETE_MISMATCH
on_payload_check() { :; }

# payload_format_check <path> <name> <format> <encryption> <signature> <role>
# Descriptor gate + read-only structural probe. Every outcome is delivered via
# on_payload_check, so verify-backup.sh and restore-local.sh classify a payload
# identically. Always returns 0 (accounting belongs to the caller).
payload_format_check() {
  local path="$1" name="$2" format="$3" encryption="$4" signature="$5" role="$6"
  local label
  label="$(payload_label "$name" "$role")"
  if ! descriptor_supported "$format" "$encryption" "$signature"; then
    on_payload_check FORMAT_MISMATCH "$name (unsupported descriptor format=$format encryption=$encryption signature=$signature)"
    return 0
  fi
  [ -f "$path" ] || return 0
  case "$format" in
    gzip)
      if gzip -t "$path" 2>/dev/null; then
        on_payload_check FORMAT_OK "$label (gzip readable)"
      else
        on_payload_check FORMAT_MISMATCH "$label gzip corrupt"
      fi
      ;;
    tar)
      if tar -tf "$path" >/dev/null 2>&1; then
        on_payload_check FORMAT_OK "$label (tar readable)"
      else
        on_payload_check FORMAT_MISMATCH "$label unreadable"
      fi
      ;;
    raw)
      # Presence, size and SHA-256 are checked by the caller's manifest loop.
      on_payload_check FORMAT_OK "$name (raw payload present)"
      ;;
  esac
  # I74-B: a pg_dump footer is mandatory for every postgres payload, whether
  # the role came from the manifest or from the canonical basename, and
  # INDEPENDENT of the declared format. A truncated dump that declares
  # format=raw must not skip this check.
  if [ "$role" = postgres ] || is_pg_payload "$name"; then
    if pg_dump_footer_present "$path"; then
      on_payload_check COMPLETE_OK "postgres dump has pg_dump footer"
    else
      on_payload_check COMPLETE_MISMATCH "postgres dump missing footer (truncated?)"
    fi
  fi
  return 0
}
