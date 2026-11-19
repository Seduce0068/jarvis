#!/usr/bin/env bash
# Local restore driver for JARVIS (ADR-0004; docs/integration/local-restore-design.md S1-S3).
# Reads backup-local.sh output sets. This file owns: argument parsing, set
# resolution, the integrity verification gate, service inventory + stop/restart,
# and dispatch to the component modules. Actual component restore lives in
# restore-local.d/{pg,qdrant,configs,git}.sh.
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
SERVICE_INVENTORY=(jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context opencode code-server otelcol)
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
                        [--allow-legacy] [--skip-db] [--skip-qdrant]
                        [--skip-configs] [--skip-git] [--help]

  Default: dry-run (verify the selected set and print the plan; no writes).
  --backup-set DIR  select a specific backup set directory (absolute or relative).
  --latest          select the last set by `find | sort | tail -1` under
                    BACKUP_ROOT; this is the default when neither selector is given.
  --apply           execute restore into an isolated target via the modules.
  --confirm         must be combined with --apply for production overwrite;
                    rejected on its own.
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

manifest_has() { # exact manifest name -> 0/1
  local wanted="$1" name
  while read -r name _; do
    case "$name" in ''|\#*) continue ;; esac
    [ "$name" = "$wanted" ] && return 0
  done < "$SET_DIR/manifest.txt"
  return 1
}

manifest_has_prefix() { # manifest name prefix -> 0/1
  local prefix="$1" name
  while read -r name _; do
    case "$name" in ''|\#*) continue ;; esac
    case "$name" in "$prefix"*) return 0 ;; esac
  done < "$SET_DIR/manifest.txt"
  return 1
}

verify_set() {
  local manifest="$SET_DIR/manifest.txt"
  local name expected_bytes expected_sha path actual_bytes actual_sha
  local failures=0 has_pg=0 has_qdrant=0 verifier=""

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

  # Replicate verify-backup.sh:62-79 (size + SHA-256 for every recorded file).
  while read -r name expected_bytes expected_sha; do
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
  done < "$manifest"

  # Format checks (read-only), matching verify-backup.sh:83-109.
  if [ -f "$SET_DIR/postgres-jarvis.sql.gz" ]; then
    if gzip -t "$SET_DIR/postgres-jarvis.sql.gz" 2>/dev/null; then
      log "FORMAT OK: postgres dump (gzip readable)"
      # grep -F without -q avoids SIGPIPE under pipefail; no values are read.
      if gzip -cd "$SET_DIR/postgres-jarvis.sql.gz" 2>/dev/null | tail -5 \
         | grep -F "PostgreSQL database dump complete" >/dev/null 2>&1; then
        log "COMPLETE OK: postgres dump has pg_dump footer"
      else
        log "FORMAT MISMATCH: postgres dump missing pg_dump footer"
        failures=$((failures + 1))
      fi
    else
      log "FORMAT MISMATCH: postgres dump gzip corrupt"
      failures=$((failures + 1))
    fi
  fi
  if [ -f "$SET_DIR/qdrant-snapshot.bin" ]; then
    if tar -tf "$SET_DIR/qdrant-snapshot.bin" >/dev/null 2>&1; then
      log "FORMAT OK: qdrant snapshot (tar readable)"
    else
      log "FORMAT MISMATCH: qdrant snapshot unreadable"
      failures=$((failures + 1))
    fi
  fi

  # Stricter than verify-backup.sh: at least one main data component, and every
  # selected primary component must be recorded (no silent skip).
  if manifest_has "postgres-jarvis.sql.gz"; then has_pg=1; fi
  if manifest_has "qdrant-snapshot.bin"; then has_qdrant=1; fi
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
  if [ "$SKIP_CONFIGS" -eq 0 ] && ! manifest_has_prefix "config-"; then
    log "WARN: no config-* entry in manifest; nothing to restore for configs"
  fi
  if [ "$SKIP_GIT" -eq 0 ] && ! manifest_has "git-jarvis-head.tar.gz"; then
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

on_exit() {
  trap - EXIT
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
