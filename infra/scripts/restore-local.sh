#!/usr/bin/env bash
# Local restore driver for JARVIS (ADR-0004; docs/integration/local-restore-design.md S1-S3).
# Reads backup-local.sh output sets. This file owns: argument parsing, set
# resolution, the integrity + config-allowlist verification gate (the single
# "before any write" gate, also enforced with --skip-configs), service inventory
# + stop/restart, and dispatch to the component modules. Actual component
# restore lives in restore-local.d/{pg,qdrant,configs,git}.sh.
#
# Default is dry-run: verification + plan only, zero writes (no files changed,
# no services stopped). --apply restores into an isolated target; --apply
# --confirm is required before any module may overwrite production data.
set -euo pipefail

# ---------------------------------------------------------------------------
# Common interface globals (shared with restore-local.d/*.sh modules)
# ---------------------------------------------------------------------------
BACKUP_ROOT="${BACKUP_ROOT:-/var/lib/postgresql/jarvis-backups}"
QDRANT_COLLECTION="${QDRANT_COLLECTION:-jarvis_memories}"
DB_NAME="${DB_NAME:-jarvis}"

SET_DIR=""        # absolute path of the selected backup set
APPLY=0           # 0 = dry-run, 1 = restore into an isolated target
CONFIRM=0         # 0 = never touch production, 1 = allow production overwrite
# Isolated-DB aftercare state shared with restore-local.d/pg.sh and on_exit().
# pg.sh sets CREATED=1 after a successful CREATE DATABASE and clears it after a
# successful DROP. The EXIT trap drops any leftover so a failed exercise never
# leaves a copy of production data behind. Default is cleanup; the explicit
# --keep-isolated-db option is the only way to retain the isolated database.
ISOLATED_DB_NAME=""
ISOLATED_DB_CREATED=0
KEEP_ISOLATED_DB=0
ALLOW_LEGACY=0    # accept a set that has no COMPLETE marker
SKIP_DB=0
SKIP_QDRANT=0
SKIP_CONFIGS=0
SKIP_GIT=0

BACKUP_SET_ARG=""
LATEST=0
VERIFY_SCRIPT="${VERIFY_SCRIPT:-}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
MODULE_DIR="$SCRIPT_DIR/restore-local.d"

# Qdrant is deliberately absent from this list. Restore uses its HTTP API, so
# the process must keep listening; "stop then HTTP upload" is never produced.
# jarvis-backup.timer IS a stop target: OnCalendar=daily + Persistent=true
# (infra/systemd/jarvis-backup.timer) can fire while pg.sh drops/recreates the
# production DB (restore-local.d/pg.sh:119-144), yielding a failed pg_dump or a
# COMPLETE set that still contains the old DB. It goes through the same
# inventory -> stop_services -> on_exit restart path as every other unit; stop
# leaves the enablement bit untouched, so start in on_exit restores both active
# and enabled state. Dry-run never calls stop_services, so the timer is untouched.
SERVICE_INVENTORY=(jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context opencode code-server otelcol jarvis-backup.timer)
SERVICES_TO_STOP=()
STOPPED_SERVICES=()

log() {
  printf '[restore-local] %s\n' "$*"
}

fail() {
  printf '[restore-local] ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'USAGE'
Usage: restore-local.sh [--backup-set DIR | --latest] [--apply] [--confirm]
                        [--keep-isolated-db] [--allow-legacy] [--skip-db]
                        [--skip-qdrant] [--skip-configs] [--skip-git] [--help]

  Default: dry-run (verify the selected set and print the plan; no writes).
  --backup-set DIR  select a specific backup set directory (absolute or relative).
  --latest          select the last set by `find | sort | tail -1` under
                    BACKUP_ROOT; this is the default when neither selector is given.
  --apply           execute restore into an isolated target via the modules.
  --confirm         must be combined with --apply for production overwrite;
                    rejected on its own.
  --keep-isolated-db
                    retain the isolated PostgreSQL database after verification
                    (default: drop it, like Qdrant drops its isolated collection).
  --allow-legacy    accept a set without the COMPLETE marker.
  --skip-*          omit a component (and its manifest presence gate).
  -h, --help        show this help.

  Verification is mandatory even in dry-run; a failed gate means zero writes.
  Test-only environment overrides: BACKUP_ROOT, VERIFY_SCRIPT.
USAGE
}

# ---------------------------------------------------------------------------
# S1: argument parsing
# ---------------------------------------------------------------------------
while [ "$#" -gt 0 ]; do
  case "$1" in
    --backup-set)
      [ "$#" -ge 2 ] || fail "--backup-set requires a directory"
      BACKUP_SET_ARG="$2"; shift 2 ;;
    --latest) LATEST=1; shift ;;
    --apply) APPLY=1; shift ;;
    --confirm) CONFIRM=1; shift ;;
    --keep-isolated-db) KEEP_ISOLATED_DB=1; shift ;;
    --allow-legacy) ALLOW_LEGACY=1; shift ;;
    --skip-db) SKIP_DB=1; shift ;;
    --skip-qdrant) SKIP_QDRANT=1; shift ;;
    --skip-configs) SKIP_CONFIGS=1; shift ;;
    --skip-git) SKIP_GIT=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) fail "unknown option: $1" ;;
  esac
done

if [ "$CONFIRM" -eq 1 ] && [ "$APPLY" -ne 1 ]; then
  fail "--confirm requires --apply; --confirm alone is rejected"
fi
if [ -n "$BACKUP_SET_ARG" ] && [ "$LATEST" -eq 1 ]; then
  fail "--backup-set and --latest are mutually exclusive"
fi

# ---------------------------------------------------------------------------
# S2: set resolution and verification adapter
# ---------------------------------------------------------------------------
resolve_set() {
  local candidate
  if [ -n "$BACKUP_SET_ARG" ]; then
    case "$BACKUP_SET_ARG" in
      /*) candidate="$BACKUP_SET_ARG" ;;
      *) candidate="$PWD/$BACKUP_SET_ARG" ;;
    esac
    if [ -d "$candidate" ]; then
      candidate="$(cd -- "$candidate" && pwd -P)"
    fi
    SET_DIR="$candidate"
    log "set selection: --backup-set $SET_DIR"
  else
    # Identical selection rule to verify-backup.sh:26-29.
    SET_DIR="$(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort | tail -1 || true)"
    [ -n "$SET_DIR" ] || fail "no backup sets found under $BACKUP_ROOT"
    log "set selection: --latest $SET_DIR"
  fi
}

# ---------------------------------------------------------------------------
# Payload descriptors: the single derivation path for mandatory roles and
# format checks (same contract as verify-backup.sh; defined in backup-local.sh).
# Legacy 3-column manifests are still accepted by defaulting descriptors to
# the historical extension/name mapping. An explicit but unknown descriptor is
# rejected (fail-closed), so future encryption cannot silently skip a check.
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

DESCRIPTOR_FAILURES=0

check_payload_format() { # name format encryption signature role
  local name="$1" format="$2" encryption="$3" signature="$4" role="$5"
  local path="$SET_DIR/$name" label="$name"
  if ! descriptor_supported "$format" "$encryption" "$signature"; then
    log "FORMAT MISMATCH: $name (unsupported descriptor format=$format encryption=$encryption signature=$signature)"
    DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
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
        log "FORMAT OK: $label (gzip readable)"
        if [ "$role" = postgres ]; then
          # grep -F without -q avoids SIGPIPE under pipefail; no values read.
          if gzip -cd "$path" 2>/dev/null | tail -5 \
             | grep -F "PostgreSQL database dump complete" >/dev/null 2>&1; then
            log "COMPLETE OK: postgres dump has pg_dump footer"
          else
            log "FORMAT MISMATCH: postgres dump missing pg_dump footer"
            DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
          fi
        fi
      else
        log "FORMAT MISMATCH: $label gzip corrupt"
        DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
      fi
      ;;
    tar)
      if tar -tf "$path" >/dev/null 2>&1; then
        log "FORMAT OK: $label (tar readable)"
      else
        log "FORMAT MISMATCH: $label unreadable"
        DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
      fi
      ;;
    raw)
      # Presence, size and SHA-256 were checked by the manifest loop above.
      log "FORMAT OK: $name (raw payload present)"
      ;;
  esac
}

# Single source of truth for the 6 reviewed, non-secret config-<basename>
# entries. Given a basename (config-* with the prefix stripped), print its
# reviewed destination and return 0; return 1 for any other name (including
# path traversal or secret-bearing names). verify_set() uses this as the
# authoritative "before any write" allowlist gate; restore_configs() reuses
# the same helper for destination resolution, so the two can never diverge.
config_dest_for_base() {
  local base="$1"
  case "$base" in
    mcp_allowlist.toml)       printf '%s\n' /etc/jarvis/mcp_allowlist.toml ;;
    pii_patterns.toml)        printf '%s\n' /etc/jarvis/pii_patterns.toml ;;
    casbin_model.conf)        printf '%s\n' /etc/jarvis/policy/casbin_model.conf ;;
    casbin_policy.csv)        printf '%s\n' /etc/jarvis/policy/casbin_policy.csv ;;
    cedar_schema.cedarschema) printf '%s\n' /etc/jarvis/policy/cedar_schema.cedarschema ;;
    policies.cedar)           printf '%s\n' /etc/jarvis/policy/policies.cedar ;;
    *) return 1 ;;
  esac
}

verify_set() {
  local manifest="$SET_DIR/manifest.txt"
  local name expected_bytes expected_sha format encryption signature role
  local path actual_bytes actual_sha base resolved
  local failures=0 has_pg=0 has_qdrant=0 has_config=0 has_git=0 verifier=""

  [ -d "$SET_DIR" ] || { log "FAIL: backup set directory not found: $SET_DIR"; return 1; }
  [ -f "$manifest" ] || { log "FAIL: manifest.txt missing: $manifest"; return 1; }

  if [ -f "$SET_DIR/COMPLETE" ]; then
    log "COMPLETE marker: present"
  elif [ "$ALLOW_LEGACY" -eq 1 ]; then
    log "WARN: COMPLETE marker absent (accepted via --allow-legacy)"
  else
    log "FAIL: COMPLETE marker missing; pass --allow-legacy to accept a legacy set"
    failures=$((failures + 1))
  fi

  # Replicate verify-backup.sh's integrity pass (size + SHA-256 for every
  # recorded file) and derive role/format from the manifest descriptors in the
  # same loop. This is the single path that replaces fixed-name presence and
  # format checks; unknown descriptors are rejected by check_payload_format.
  DESCRIPTOR_FAILURES=0
  while read -r name expected_bytes expected_sha format encryption signature role; do
    case "$name" in ''|\#*) continue ;; esac
    path="$SET_DIR/$name"
    if [ ! -f "$path" ]; then
      log "MISSING: $name"
      failures=$((failures + 1))
      continue
    fi
    actual_bytes="$(wc -c < "$path" | tr -d ' ')"
    actual_sha="$(sha256sum "$path" | cut -d' ' -f1)"
    if [ "$actual_bytes" = "$expected_bytes" ] && [ "$actual_sha" = "$expected_sha" ]; then
      log "OK: $name ($actual_bytes bytes)"
    else
      log "MISMATCH: $name (size ${actual_bytes}/${expected_bytes} sha ${actual_sha:0:12}/${expected_sha:0:12})"
      failures=$((failures + 1))
    fi

    # An explicit role must be in the vocabulary; an empty role marks a legacy
    # row and is derived from the name below. Unknown values fail closed.
    case "$role" in
      ''|postgres|qdrant|config|git|other) ;;
      *)
        log "FORMAT MISMATCH: $name (unsupported role descriptor: $role)"
        DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
        continue
        ;;
    esac
    resolved="$(resolve_descriptors "$name" "$format" "$encryption" "$signature" "$role")"
    read -r format encryption signature role <<< "$resolved"
    case "$role" in
      postgres) has_pg=1 ;;
      qdrant) has_qdrant=1 ;;
      config) has_config=1 ;;
      git) has_git=1 ;;
    esac
    # Format checks (read-only), dispatched from the descriptor vocabulary.
    check_payload_format "$name" "$format" "$encryption" "$signature" "$role"
  done < "$manifest"
  failures=$((failures + DESCRIPTOR_FAILURES))

  # config-* allowlist: the authoritative "before any write" gate. It runs
  # unconditionally (also with --skip-configs), because a set carrying a
  # non-allowlisted config name can still drive pg/qdrant production writes
  # under --apply --confirm. Rejecting here precedes stop_services(),
  # load_modules() and dispatch_restore(), so no DB/Qdrant/config is touched.
  while read -r name _; do
    case "$name" in
      ''|\#*) continue ;;
      config-*) ;;
      *) continue ;;
    esac
    base="${name#config-}"
    if ! config_dest_for_base "$base" >/dev/null; then
      log "FAIL: non-allowlisted config entry in manifest: $name"
      failures=$((failures + 1))
    fi
  done < "$manifest"

  # Stricter than verify-backup.sh: at least one main data component, and every
  # selected primary component must be recorded (no silent skip). Presence is
  # derived from the descriptor roles collected above, not from fixed names.
  if [ "$has_pg" -eq 0 ] && [ "$has_qdrant" -eq 0 ]; then
    log "FAIL: no main data component recorded in manifest"
    failures=$((failures + 1))
  fi
  if [ "$SKIP_DB" -eq 0 ] && [ "$has_pg" -eq 0 ]; then
    log "FAIL: postgres-jarvis.sql.gz not in manifest; use --skip-db to omit it"
    failures=$((failures + 1))
  fi
  if [ "$SKIP_QDRANT" -eq 0 ] && [ "$has_qdrant" -eq 0 ]; then
    log "FAIL: qdrant-snapshot.bin not in manifest; use --skip-qdrant to omit it"
    failures=$((failures + 1))
  fi
  # configs and git are legitimately optional in the backup format; report an
  # explicit warning instead of silently skipping when they are absent.
  if [ "$SKIP_CONFIGS" -eq 0 ] && [ "$has_config" -eq 0 ]; then
    log "WARN: no config-* entry in manifest; nothing to restore for configs"
  fi
  if [ "$SKIP_GIT" -eq 0 ] && [ "$has_git" -eq 0 ]; then
    log "WARN: git-jarvis-head.tar.gz not in manifest; nothing to restore for git"
  fi

  # Optional external verifier (verify-backup.sh interface, invoked via bash so
  # the executable bit is irrelevant). Its rejection is also fail-closed.
  if [ -n "$VERIFY_SCRIPT" ]; then
    verifier="$VERIFY_SCRIPT"
  elif [ -f "$SCRIPT_DIR/verify-backup.sh" ]; then
    verifier="$SCRIPT_DIR/verify-backup.sh"
  elif command -v verify-backup.sh >/dev/null 2>&1; then
    verifier="$(command -v verify-backup.sh)"
  fi
  if [ -n "$verifier" ]; then
    log "external verifier: $verifier"
    if ! bash "$verifier" "$SET_DIR"; then
      log "FAIL: external verifier rejected the set"
      failures=$((failures + 1))
    fi
  fi

  if [ "$failures" -ne 0 ]; then
    log "verification FAILED ($failures failure(s)); no writes performed"
    return 1
  fi
  log "verification OK"
  return 0
}

# ---------------------------------------------------------------------------
# S3: service inventory and stop/restart (Qdrant is never stopped)
# ---------------------------------------------------------------------------
inventory_services() {
  local unit
  SERVICES_TO_STOP=()
  if ! command -v systemctl >/dev/null 2>&1; then
    log "WARN: systemctl unavailable; service inventory is empty"
    return 0
  fi
  for unit in "${SERVICE_INVENTORY[@]}"; do
    case "$unit" in
      qdrant|qdrant.service) continue ;; # invariant: HTTP API needs Qdrant up
    esac
    if systemctl is-enabled "$unit" >/dev/null 2>&1 \
       && systemctl is-active "$unit" >/dev/null 2>&1; then
      SERVICES_TO_STOP+=("$unit")
    fi
  done
}

stop_services() {
  local unit
  for unit in "${SERVICES_TO_STOP[@]}"; do
    log "stopping service: $unit"
    if systemctl stop "$unit"; then
      STOPPED_SERVICES+=("$unit")
    else
      log "ERROR: failed to stop service: $unit"
      return 1
    fi
  done
}

# Failure-path insurance for the isolated PostgreSQL database. It only acts when
# pg.sh created the DB but did not drop it (e.g. fail()/set -e aborted between
# CREATE and DROP). It never overrides an explicit --keep-isolated-db choice and
# never masks the existing service-restart behaviour of on_exit().
cleanup_isolated_db() {
  if [ "$ISOLATED_DB_CREATED" -ne 1 ] || [ "$KEEP_ISOLATED_DB" -eq 1 ]; then
    return 0
  fi
  [ -n "$ISOLATED_DB_NAME" ] || return 0
  if ! command -v psql >/dev/null 2>&1; then
    printf '[restore-local] WARN: isolated DB %s left behind (psql unavailable)\n' \
      "$ISOLATED_DB_NAME" >&2
    return 0
  fi
  log "cleaning up leftover isolated DB: $ISOLATED_DB_NAME"
  if psql -v ON_ERROR_STOP=1 -d postgres -tAc \
      "DROP DATABASE IF EXISTS \"$ISOLATED_DB_NAME\"" >/dev/null 2>&1; then
    ISOLATED_DB_CREATED=0
    log "leftover isolated DB dropped: $ISOLATED_DB_NAME"
  else
    printf '[restore-local] ERROR: failed to drop leftover isolated DB: %s\n' \
      "$ISOLATED_DB_NAME" >&2
  fi
  return 0
}

on_exit() {
  trap - EXIT
  # STOPPED_SERVICES can include jarvis-backup.timer (see SERVICE_INVENTORY).
  # The earlier `systemctl stop` did not change its enablement bit, and the
  # `systemctl start` below restores its active state, so this same loop
  # recovers the original active/enabled state without a timer-specific branch.
  if [ "${#STOPPED_SERVICES[@]}" -gt 0 ]; then
    log "restarting stopped services in reverse order"
    local i unit
    for (( i=${#STOPPED_SERVICES[@]}-1; i>=0; i-- )); do
      unit="${STOPPED_SERVICES[$i]}"
      if systemctl start "$unit"; then
        log "restarted: $unit"
      else
        printf '[restore-local] restart FAILED: %s\n' "$unit" >&2
      fi
    done
  fi

  # Aftercare on both success (normally a no-op: pg.sh already dropped it) and
  # failure paths. Service restart above keeps its original order and priority.
  cleanup_isolated_db
  return 0
}

# ---------------------------------------------------------------------------
# Module loading / dispatch (existence check only; modules own restore logic)
# ---------------------------------------------------------------------------
load_modules() {
  local mod modfile
  for mod in pg qdrant configs git; do
    modfile="$MODULE_DIR/$mod.sh"
    if [ -f "$modfile" ]; then
      log "loading module: $modfile"
      # shellcheck disable=SC1090
      source "$modfile" || fail "failed to source module: $modfile"
    else
      log "WARN: module not found ($modfile); $mod restore unavailable"
    fi
  done
}

dispatch_restore() {
  if [ "$SKIP_DB" -eq 0 ]; then
    if declare -F restore_pg >/dev/null 2>&1; then
      log "dispatch: restore_pg"
      restore_pg
    else
      log "WARN: restore_pg() missing; skipping db restore"
    fi
  fi
  if [ "$SKIP_QDRANT" -eq 0 ]; then
    if declare -F restore_qdrant >/dev/null 2>&1; then
      log "dispatch: restore_qdrant"
      restore_qdrant
    else
      log "WARN: restore_qdrant() missing; skipping qdrant restore"
    fi
  fi
  if [ "$SKIP_CONFIGS" -eq 0 ]; then
    if declare -F restore_configs >/dev/null 2>&1; then
      log "dispatch: restore_configs"
      restore_configs
    else
      log "WARN: restore_configs() missing; skipping configs restore"
    fi
  fi
  if [ "$SKIP_GIT" -eq 0 ]; then
    if declare -F restore_git >/dev/null 2>&1; then
      log "dispatch: restore_git"
      restore_git
    else
      log "WARN: restore_git() missing; skipping git restore"
    fi
  fi
}

# ---------------------------------------------------------------------------
# Main flow
# ---------------------------------------------------------------------------
main() {
  resolve_set

  log "JARVIS local restore: set=$SET_DIR apply=$APPLY confirm=$CONFIRM"

  # Mandatory verification gate, even in dry-run. On failure nothing is written.
  if ! verify_set; then
    fail "verification failed; refusing to write anything"
  fi

  # Read-only inventory; used for the plan and for the actual stop in apply mode.
  inventory_services

  log "PLAN: db=$([ "$SKIP_DB" -eq 0 ] && echo restore || echo skip) \
qdrant=$([ "$SKIP_QDRANT" -eq 0 ] && echo restore || echo skip) \
configs=$([ "$SKIP_CONFIGS" -eq 0 ] && echo restore || echo skip) \
git=$([ "$SKIP_GIT" -eq 0 ] && echo restore || echo skip)"
  log "PLAN: services to stop: ${SERVICES_TO_STOP[*]:-<none>}"
  log "PLAN: production overwrite: $([ "$CONFIRM" -eq 1 ] && echo enabled || echo disabled)"
  log "PLAN: isolated DB aftercare: $([ "$KEEP_ISOLATED_DB" -eq 1 ] && echo keep || echo drop)"
  log "PLAN: qdrant.service is NOT a stop target (HTTP API restore requires it active)"

  if [ "$APPLY" -ne 1 ]; then
    log "DRY RUN: verification and plan only; no files written, no services stopped"
    return 0
  fi

  # Restart on success or failure; reverse order of the recorded stops.
  trap on_exit EXIT
  load_modules

  if ! stop_services; then
    fail "service stop failed; aborting restore"
  fi

  dispatch_restore
  log "restore sequence finished"
}

main "$@"
