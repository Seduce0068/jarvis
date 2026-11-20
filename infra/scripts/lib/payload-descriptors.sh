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
# Callers MUST override on_payload_check AFTER sourcing AND register it through
# install_payload_check_hook(); the classification itself lives only here and is
# ALSO returned as the exit status of payload_format_check().
#
# Adopted accounting contract (I79-A, fail-closed): the reporting hook is LOG
# ONLY -- it emits the caller's FORMAT OK/MISMATCH line but MUST NOT touch any
# counter. The caller owns ALL accounting and derives it solely from the return
# code of payload_format_check(): 0 = ok, 1 = verdict mismatch (unsupported
# descriptor / corrupt gzip-tar / truncated pg_dump), 2 = hook not installed.
# Any rc >= 1 is a failure. Counting only rc==2 (or trusting the hook to have
# counted rc==1) is a fail-open: a future rc==1 branch with a lost hook record
# would let corrupt data through. A lost/never-installed hook still fails closed
# via rc==2 (I77-B).

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
    # I77-A: use the SAME pattern as is_git_payload(). The old fixed-name-only
    # mapping left git-extra.tar.gz with role=other while is_git_payload still
    # counted it, so the resolver returned rc=2 (absent) and the run reported a
    # successful no-op with a stale git checkout.
    git*.tar.gz) printf 'git\n' ;;
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

# --- I77-A: the single role-resolution path ----------------------------------
# resolved_role_for <name> <declared_role>
#   stdout : the one canonical role (postgres|qdrant|config|git|other) that both
#            verify_set() and resolve_restore_input() must use for a manifest row.
#   return : 0 resolved; 2 unknown declared role; 1 explicit role contradicts
#            the basename. Both non-zero statuses are fail-closed descriptor
#            errors (the caller increments DESCRIPTOR_FAILURES / returns rc 1).
#
# An empty role marks a legacy row and is derived from the basename. A concrete
# role is honoured only when the real basename can back it (I74-A/B: pg-data.sql.gz
# is a valid postgres payload, readme.txt claiming postgres is not). An explicit
# `other` may not shadow a basename that names a real component: config-* and
# git*.tar.gz are the I77-A drift that used to make verify_set count a config/git
# payload while the resolver saw no role match (rc=2) and silently succeeded,
# leaving stale /etc/jarvis settings or a stale git checkout behind. `other` on
# an unrelated basename stays legal (legacy / non-payload files).
resolved_role_for() { # name declared_role -> role; 0 ok / 1 mismatch / 2 unknown
  local name="$1" declared="${2:-}" base_role
  case "$declared" in
    ''|postgres|qdrant|config|git|other) ;;
    *) return 2 ;;
  esac
  base_role="$(legacy_role_for "$name")"
  if [ -z "$declared" ]; then
    printf '%s\n' "$base_role"
    return 0
  fi
  if [ "$declared" = other ]; then
    [ "$base_role" = other ] || return 1
    printf 'other\n'
    return 0
  fi
  role_basename_consistent "$name" "$declared" || return 1
  printf '%s\n' "$declared"
  return 0
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

# --- I77-B: mandatory hook installation guard --------------------------------
# Adopted policy (I79-A): payload_format_check() owns the classification and
# returns it as its exit status (0 = ok, 1 = verdict mismatch, 2 = hook not
# installed). on_payload_check is *reporting only* -- it emits the caller's
# FORMAT OK/MISMATCH line and MUST NOT update counters; the caller counts from
# the return code instead (any rc >= 1 is a failure). Because a no-op hook used
# to swallow every verdict (fail-open), a caller must replace the placeholder
# below and register it with install_payload_check_hook() before the first
# format check. An unregistered hook makes payload_format_check() fail closed
# with rc=2.
PAYLOAD_CHECK_HOOK_INSTALLED=0

# on_payload_check <event> <detail>
# Placeholder no-op. Each caller overrides this after sourcing to map the shared
# verdict to its own log output (counters are owned by the caller's rc handling).
# Events:
#   FORMAT_OK | FORMAT_MISMATCH | COMPLETE_OK | COMPLETE_MISMATCH
on_payload_check() { :; }

# Captured at source time so the installer can PROVE the placeholder was really
# replaced instead of trusting a caller-set flag alone.
_PAYLOAD_CHECK_PLACEHOLDER="$(declare -f on_payload_check)"

# install_payload_check_hook -> 0/1
# Call AFTER defining the caller's on_payload_check. Refuses a missing or still
# identical placeholder definition, so a lost override cannot reintroduce the
# old fail-open path. On success flips the mandatory installation flag.
install_payload_check_hook() {
  local current
  current="$(declare -f on_payload_check 2>/dev/null)" || return 1
  [ "$current" != "$_PAYLOAD_CHECK_PLACEHOLDER" ] || return 1
  declare -F on_payload_check >/dev/null 2>&1 || return 1
  PAYLOAD_CHECK_HOOK_INSTALLED=1
  return 0
}

# assert_payload_check_hook -> 0/1
# Fail-closed guard used at the top of payload_format_check(). It checks BOTH
# the mandatory flag and that the hook is still a real (non-placeholder) override,
# so a hand-set flag or a reverted no-op cannot reopen the fail-open path.
assert_payload_check_hook() {
  local current
  [ "${PAYLOAD_CHECK_HOOK_INSTALLED:-0}" -eq 1 ] || return 1
  current="$(declare -f on_payload_check 2>/dev/null)" || return 1
  [ "$current" != "$_PAYLOAD_CHECK_PLACEHOLDER" ] || return 1
  return 0
}

# payload_format_check <path> <name> <format> <encryption> <signature> <role>
# Descriptor gate + read-only structural probe. Every outcome is delivered via
# on_payload_check (log only) AND reflected in the exit status:
#   0 = every probed aspect ok / no verdict needed
#   1 = at least one mismatch (unsupported descriptor, corrupt gzip/tar,
#       missing pg_dump footer)
#   2 = reporting hook not installed -> fail closed, no classification attempted
# Callers MUST treat any rc >= 1 as a failure and do ALL failure accounting from
# this return code; the hook never touches counters. verify-backup.sh and
# restore-local.sh therefore classify a payload identically and enforce the
# verdict from the return value even if the hook's log line is lost.
payload_format_check() {
  assert_payload_check_hook || {
    printf '%s\n' 'ERROR: payload_format_check called with no installed on_payload_check hook' >&2
    return 2
  }
  local path="$1" name="$2" format="$3" encryption="$4" signature="$5" role="$6"
  local label rc=0
  label="$(payload_label "$name" "$role")"
  if ! descriptor_supported "$format" "$encryption" "$signature"; then
    on_payload_check FORMAT_MISMATCH "$name (unsupported descriptor format=$format encryption=$encryption signature=$signature)"
    return 1
  fi
  [ -f "$path" ] || return 0
  case "$format" in
    gzip)
      if gzip -t "$path" 2>/dev/null; then
        on_payload_check FORMAT_OK "$label (gzip readable)"
      else
        on_payload_check FORMAT_MISMATCH "$label gzip corrupt"
        rc=1
      fi
      ;;
    tar)
      if tar -tf "$path" >/dev/null 2>&1; then
        on_payload_check FORMAT_OK "$label (tar readable)"
      else
        on_payload_check FORMAT_MISMATCH "$label unreadable"
        rc=1
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
      rc=1
    fi
  fi
  return "$rc"
}
