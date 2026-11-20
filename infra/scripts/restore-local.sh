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
# I81-A (c): dedicated mutual-exclusion lock path. The default is a stable,
# root-owned path so a non-root actor cannot delete/recreate the file (inode
# swap) to break exclusion. Overridable via LOCK_FILE for isolated test runs.
LOCK_FILE_EXPLICIT=0
LOCK_FALLBACK_TMP=0
if [ -n "${LOCK_FILE:-}" ]; then
  LOCK_FILE_EXPLICIT=1
elif [ -d /run ]; then
  LOCK_FILE="/run/jarvis/restore.lock"
else
  # /run absent (non-systemd/container): fall back to /tmp and make it explicit.
  LOCK_FILE="/tmp/jarvis-restore.lock"
  LOCK_FALLBACK_TMP=1
fi
LOCK_DIR="$(dirname -- "$LOCK_FILE")"
LOCK_FAIL_TAG='[restore-local] LOCK FAIL:'
LOCK_FD=""
LOCK_INODE=""

# Qdrant is deliberately absent from this list. Restore uses its HTTP API, so
# the process must keep listening; "stop then HTTP upload" is never produced.
# jarvis-backup.timer IS a stop target: OnCalendar=daily + Persistent=true
# (infra/systemd/jarvis-backup.timer) can fire while pg.sh drops/recreates the
# production DB (restore-local.d/pg.sh:119-144), yielding a failed pg_dump or a
# COMPLETE set that still contains the old DB. It goes through the same
# inventory -> stop_services path as every other unit; stop leaves the
# enablement bit untouched. On SUCCESS on_exit starts it in reverse order,
# restoring both active and enabled state. On FAILURE on_exit deliberately
# leaves it STOPPED and emits the fixed manual-restart warning (see on_exit):
# a restarted Persistent=true timer would elapse immediately over the dropped /
# mid-restore DB and write a COMPLETE set that --latest treats as the newest
# good backup. Dry-run never calls stop_services, so the timer is untouched.
# I73-A: a timer is selected by is-active ALONE (a disabled-but-active timer can
# still fire), and verify_stopped_services() confirms it is inactive after the
# stop before dispatch; otherwise the run aborts fail-closed before any write.
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

# ---------------------------------------------------------------------------
# I73-B: single execution user = root. The backup dir is postgres:postgres 0700
# (install-backup-timer.sh:163-165), so only root can both read the set and
# authorise `systemctl stop`. PostgreSQL is reached via the postgres OS role.
# Failure vocabulary is fixed so the runbook and tests can match it exactly.
# ---------------------------------------------------------------------------
PRECONDITION_TAG='[restore-local] PRECONDITION FAIL:'

precondition_fail() { # code detail -> fail closed before any write
  printf '%s %s: %s\n' "$PRECONDITION_TAG" "$1" "$2" >&2
  exit 1
}

# Single psql entry point. Bash resolves functions before PATH entries, so this
# also wraps every bare `psql` call in the sourced restore-local.d/*.sh modules
# (pg.sh is out of scope for this change). As root, run psql as the postgres OS
# role (peer auth); keep the caller's psql otherwise (fixture harness / explicit
# override) so the existing mock-based regression suite stays intact.
psql() {
  # The fixture harness (MOCK_LOG) injects its own psql via PATH; keep using it
  # verbatim even if the harness itself is launched as root.
  if [ "$EUID" -eq 0 ] && [ -z "${MOCK_LOG:-}" ]; then
    command sudo -u postgres psql "$@"
  else
    command psql "$@"
  fi
}
export -f psql

check_preconditions() {
  local uid="$EUID"

  # 1) backup_dir readable + searchable. In production this rejects an
  #    unprivileged caller before verify_set() fails on a missing manifest.
  if [ ! -d "$BACKUP_ROOT" ] || [ ! -r "$BACKUP_ROOT" ] || [ ! -x "$BACKUP_ROOT" ]; then
    precondition_fail BACKUP_DIR_UNREADABLE \
      "cannot read backup_dir: $BACKUP_ROOT (run as root)"
  fi

  # 2) psql runnable as the postgres role. `type -P` ignores the wrapper above
  #    and proves an external binary exists on PATH.
  if ! type -P psql >/dev/null 2>&1; then
    precondition_fail PSQL_UNAVAILABLE 'psql not found in PATH'
  fi
  if [ "$uid" -eq 0 ] && [ -z "${MOCK_LOG:-}" ]; then
    if ! command -v sudo >/dev/null 2>&1; then
      precondition_fail PSQL_UNAVAILABLE \
        'sudo required to run psql as the postgres OS role'
    fi
    if ! sudo -n -u postgres psql -d postgres -tAc 'SELECT 1' >/dev/null 2>&1; then
      precondition_fail PSQL_UNAVAILABLE \
        'cannot connect to PostgreSQL as the postgres OS role'
    fi
  fi

  # 3) systemctl stop authority. root is authoritative; a non-root caller needs
  #    non-interactive sudo. MOCK_LOG marks the fixture harness, which supplies
  #    its own systemctl mock and must not probe a real host here.
  if ! command -v systemctl >/dev/null 2>&1; then
    precondition_fail SYSTEMCTL_STOP_DENIED 'systemctl not found in PATH'
  fi
  if [ "$uid" -ne 0 ] && [ -z "${MOCK_LOG:-}" ]; then
    if ! command -v sudo >/dev/null 2>&1 || ! sudo -n true >/dev/null 2>&1; then
      precondition_fail SYSTEMCTL_STOP_DENIED \
        'systemctl stop requires root or non-interactive sudo'
    fi
  fi

  log "preconditions OK: backup_dir readable, psql (postgres role) available, systemctl stop authorised"
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
  Test-only environment overrides: BACKUP_ROOT, VERIFY_SCRIPT, LOCK_FILE.
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
# Payload descriptors and format checks: one shared implementation, sourced by
# both this script and verify-backup.sh from lib/payload-descriptors.sh. Legacy
# 3-column rows default their descriptors; explicit unknown descriptors fail
# closed, so P1-6 encryption has one update point only.
# ---------------------------------------------------------------------------
# shellcheck source=lib/payload-descriptors.sh
# Resolved at runtime relative to the script location.
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/payload-descriptors.sh"
[ "${PAYLOAD_DESCRIPTORS_LIB:-}" = 1 ] || {
  printf '%s\n' '[restore-local] ERROR: shared payload descriptor library not loaded' >&2
  exit 1
}

# I79-A adopted accounting policy: this hook is LOG ONLY. It emits the shared
# verdict line but MUST NOT touch DESCRIPTOR_FAILURES -- all accounting is done
# by the caller from payload_format_check()'s return code below (any rc >= 1 is
# a failure). Counting rc==1 here as well would double-count it; relying on the
# hook alone is fail-open if a future lib branch forgets to emit an event.
on_payload_check() { # event detail
  local event="$1" detail="$2"
  case "$event" in
    FORMAT_OK|COMPLETE_OK|FORMAT_MISMATCH|COMPLETE_MISMATCH)
      log "${event//_/ }: $detail"
      ;;
  esac
}

# I77-B: register the reporting hook. Without it payload_format_check() fails
# closed (rc=2) instead of silently discarding a corrupt/truncated payload.
install_payload_check_hook || {
  log "FAIL: could not install payload check hook"
  exit 1
}
DESCRIPTOR_FAILURES=0

# I74-B helpers (is_*_payload, role_basename_consistent, pg_dump_footer_present)
# and the I77-A single role resolver (resolved_role_for) live in
# lib/payload-descriptors.sh; verify_set() and resolve_restore_input() both use
# resolved_role_for() so their role verdicts cannot diverge.

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

# I76-A: resolve_restore_input() lives in restore-local.d/configs.sh, which
# load_modules() only sources AFTER verify_set() (and after stop_services() in
# apply mode). Load it here so the pre-stop gate calls the very same resolver
# the four restore modules use; the two can never disagree about a payload's
# role. configs.sh is definition-only at file scope, so an early source has no
# side effects and load_modules() re-sourcing it later is a harmless
# redefinition. A missing/unloadable resolver fails closed here.
load_restore_resolver() {
  if ! declare -F resolve_restore_input >/dev/null 2>&1; then
    local resolver_file="$MODULE_DIR/configs.sh"
    if [ ! -f "$resolver_file" ]; then
      log "FAIL: shared restore input resolver unavailable ($resolver_file missing)"
      return 1
    fi
    # shellcheck disable=SC1090
    source "$resolver_file" || {
      log "FAIL: failed to source shared restore input resolver"
      return 1
    }
  fi
  if ! declare -F resolve_restore_input >/dev/null 2>&1; then
    log "FAIL: shared restore input resolver unavailable after source"
    return 1
  fi
  return 0
}

# I76-A / I77-A: pre-stop resolution of one restore input. Read-only (no service
# or payload access); every verdict is accounted through DESCRIPTOR_FAILURES so
# the existing "verification FAILED; no writes performed" path owns the
# fail-closed decision before stop_services() is ever reached.
#   verify_restore_input <label> <role> <legacy_name> <one|all> <required 0|1>
# rc 2 (no role match) is now a FAILURE for REQUIRED AND OPTIONAL components
# alike. Treating "absent" as a successful no-op is exactly how a role=other
# config-* / git* row used to leave stale production files behind while the run
# reported success. rc 1 (unknown / ambiguous / resolved file missing) also
# fails closed. `required` is kept for diagnostics only; it never downgrades a
# resolver error to a warning. The module-side checks stay as a second net.
verify_restore_input() {
  local label="$1" role="$2" legacy_name="$3" mode="$4" required="$5"
  local input_path rc=0

  # The `||` both keeps `set -e` from aborting and captures the resolver's
  # exact status: 0 resolved, 1 invalid/ambiguous/resolved-file-missing,
  # 2 no role match (absent). Testing `$?` after a bare assignment must be
  # avoided here because `set -e` would abort before the test runs.
  input_path="$(resolve_restore_input "$SET_DIR" "$role" "$legacy_name" "$mode")" || rc=$?
  if [ "$rc" -eq 0 ]; then
    log "RESOLVED: $label input = $input_path"
    return 0
  fi
  log "FAIL: cannot resolve $label restore input from manifest (rc=$rc; required=$required; unknown/ambiguous/missing/absent)"
  DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
  return 0
}

verify_set() {
  local manifest="$SET_DIR/manifest.txt"
  local name expected_bytes expected_sha format encryption signature role
  local path actual_bytes actual_sha base resolved resolved_role rr_rc format_rc
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
  # recorded file) and derive format from the manifest descriptors in the same
  # loop. Mandatory presence comes from the real basename, not the role column;
  # unknown descriptors and role/basename mismatches fail closed here.
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

    # I77-A: ONE role-resolution path for the whole script. resolved_role_for()
    # validates the vocabulary, derives an empty (legacy) role from the basename
    # and rejects an explicit role the basename cannot back - including an
    # explicit `other` on config-* / git*.tar.gz. resolve_restore_input() calls
    # the same function, so this gate and the pre-stop resolver can no longer
    # disagree about a row's role (the old drift that reported a successful
    # no-op while /etc/jarvis stayed stale).
    resolved="$(resolve_descriptors "$name" "$format" "$encryption" "$signature" "$role")"
    read -r format encryption signature role <<< "$resolved"
    if resolved_role="$(resolved_role_for "$name" "$role")"; then
      role="$resolved_role"
    else
      rr_rc=$?
      if [ "$rr_rc" -eq 2 ]; then
        log "FORMAT MISMATCH: $name (unsupported role descriptor: $role)"
      else
        log "FORMAT MISMATCH: $name (role=$role is not consistent with its basename; ROLE MISMATCH)"
      fi
      DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
      continue
    fi
    # Mandatory presence still comes from the real filename (I74-B
    # "required payload really exists" contract); the resolved role above is
    # what the resolver uses, and contradictions were rejected just now.
    if is_pg_payload "$name"; then
      has_pg=1
    fi
    if is_qdrant_payload "$name"; then
      has_qdrant=1
    fi
    if is_config_payload "$name"; then
      has_config=1
    fi
    if is_git_payload "$name"; then
      has_git=1
    fi
    # Format checks (read-only): the shared function owns the classification
    # and returns it as rc (0 ok / 1 mismatch / 2 hook missing). I79-A: the hook
    # is log-only; THIS is the single accounting point -- any rc >= 1 fails
    # closed by feeding DESCRIPTOR_FAILURES.
    format_rc=0
    payload_format_check "$path" "$name" "$format" "$encryption" "$signature" "$role" || format_rc=$?
    if [ "$format_rc" -ge 1 ]; then
      if [ "$format_rc" -eq 2 ]; then
        log "ERROR: payload format check hook not installed; refusing to verify"
      fi
      DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
    fi
  done < "$manifest"
  # I76-A: DESCRIPTOR_FAILURES is also fed by the pre-stop resolver gate below,
  # so fold it into `failures` once, after that gate has run.

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

  # I76-A: pre-stop restore-input resolution gate. This sits in the same
  # "before any write" section as the config allowlist, well ahead of
  # inventory_services()/stop_services()/dispatch_restore(). The modules only
  # resolve their input inside dispatch_restore() (AFTER services are stopped),
  # so an unknown role, multiple matches for a role, or a resolved file that is
  # absent would otherwise abort the restore with services already down.
  # Calling the shared resolver here rejects the set while the host is still
  # untouched. The module-side fail-closed checks remain as a defense-in-depth
  # double check; they are deliberately not removed.
  # SKIP_* semantics are preserved: an explicitly skipped component is not
  # resolved at all and its absence stays acceptable, just as the has_* gates
  # below stay conditional. Every NON-skipped component is resolved here; a
  # resolver rc 2 (no role match) is a fail-closed verification error even for
  # the historically optional configs / git (I77-A). The config allowlist and
  # the role-resolution loop above still run unconditionally, so a malformed
  # entry cannot slip past this gate merely because its component was skipped.
  if ! load_restore_resolver; then
    DESCRIPTOR_FAILURES=$((DESCRIPTOR_FAILURES + 1))
  else
    if [ "$SKIP_DB" -eq 0 ]; then
      verify_restore_input postgres postgres postgres-jarvis.sql.gz one 1
    else
      log "RESOLVE SKIP: postgres (--skip-db)"
    fi
    if [ "$SKIP_QDRANT" -eq 0 ]; then
      verify_restore_input qdrant qdrant qdrant-snapshot.bin one 1
    else
      log "RESOLVE SKIP: qdrant (--skip-qdrant)"
    fi
    if [ "$SKIP_CONFIGS" -eq 0 ]; then
      verify_restore_input configs config '' all 0
    else
      log "RESOLVE SKIP: configs (--skip-configs)"
    fi
    if [ "$SKIP_GIT" -eq 0 ]; then
      verify_restore_input git git git-jarvis-head.tar.gz one 0
    else
      log "RESOLVE SKIP: git (--skip-git)"
    fi
  fi
  failures=$((failures + DESCRIPTOR_FAILURES))

  # Stricter than verify-backup.sh: at least one main data component, and every
  # selected primary component must be recorded (no silent skip). Presence is
  # derived from the real payload basenames collected above, not from the
  # self-declared role column.
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
  local unit enabled
  SERVICES_TO_STOP=()
  if ! command -v systemctl >/dev/null 2>&1; then
    log "WARN: systemctl unavailable; service inventory is empty"
    return 0
  fi
  for unit in "${SERVICE_INVENTORY[@]}"; do
    case "$unit" in
      qdrant|qdrant.service) continue ;; # invariant: HTTP API needs Qdrant up
    esac
    # is-enabled is observed for every unit but gates only non-timer services.
    # install-backup-timer.sh:123-133 rollback re-asserts the pre-run active and
    # enabled dimensions independently (disable + start), which can leave
    # jarvis-backup.timer ACTIVE but DISABLED. Gating a timer on is-enabled
    # would drop it from the stop list and let a daily backup fire during the
    # production DB DROP/CREATE window (I73-A).
    enabled=0
    if systemctl is-enabled "$unit" >/dev/null 2>&1; then
      enabled=1
    fi
    if ! systemctl is-active "$unit" >/dev/null 2>&1; then
      continue
    fi
    case "$unit" in
      *.timer)
        if [ "$enabled" -eq 0 ]; then
          log "inventory: $unit is disabled but active; stopping anyway (backup window risk)"
        fi
        SERVICES_TO_STOP+=("$unit")
        ;;
      *)
        if [ "$enabled" -eq 1 ]; then
          SERVICES_TO_STOP+=("$unit")
        fi
        ;;
    esac
  done
  return 0
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

# I73-A fail-closed post-stop check. A disabled-but-active timer must be proven
# inactive before dispatch: if `systemctl stop` did not take effect, the daily
# backup could still elapse while pg.sh drops/recreates the production DB.
# Read-only (no enable/disable, no restart); on failure main() aborts before
# dispatch_restore() and the existing EXIT trap still owns service recovery.
verify_stopped_services() {
  local unit
  if [ "${#STOPPED_SERVICES[@]}" -eq 0 ]; then
    return 0
  fi
  for unit in "${STOPPED_SERVICES[@]}"; do
    case "$unit" in
      *.timer)
        if systemctl is-active "$unit" >/dev/null 2>&1; then
          log "ERROR: timer still active after stop: $unit"
          return 1
        fi
        log "verified inactive after stop: $unit"
        ;;
    esac
  done
  return 0
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
  # I73-C (a): capture the EXIT status before any command in this trap resets
  # $?. fail()/set -e give non-zero; a normal main() return gives 0.
  local exit_status=$?
  trap - EXIT
  local i unit
  local timer_left_stopped=0
  # Fixed vocabulary (I73-C (a)): runbook and tests match this literal line.
  local timer_warn='WARN: jarvis-backup.timer LEFT STOPPED after restore failure - MANUAL RESTART REQUIRED: sudo systemctl start jarvis-backup.timer'

  # STOPPED_SERVICES can include jarvis-backup.timer (see SERVICE_INVENTORY).
  # SUCCESS: keep the original reverse-order start verbatim, restoring both the
  # active and (untouched by stop) enabled state exactly as before.
  # FAILURE: do NOT start jarvis-backup.timer. Persistent=true would make it
  # elapse immediately over a dropped / mid-restore DB and write a COMPLETE set
  # that --latest would then treat as the newest good backup. Leaving it stopped
  # is sufficient (a stopped timer never elapses); no mask is applied so a
  # manual `systemctl start` is all that is needed once the DB is verified.
  # Other units are still restarted so a failed restore does not leave JARVIS down.
  if [ "${#STOPPED_SERVICES[@]}" -gt 0 ]; then
    log "restarting stopped services in reverse order"
    for (( i=${#STOPPED_SERVICES[@]}-1; i>=0; i-- )); do
      unit="${STOPPED_SERVICES[$i]}"
      if [ "$exit_status" -ne 0 ] && [ "$unit" = "jarvis-backup.timer" ]; then
        timer_left_stopped=1
        log "restore failure: NOT restarting jarvis-backup.timer (left stopped)"
        continue
      fi
      if systemctl start "$unit"; then
        log "restarted: $unit"
      else
        printf '[restore-local] restart FAILED: %s\n' "$unit" >&2
      fi
    done
  fi

  if [ "$timer_left_stopped" -eq 1 ]; then
    printf '[restore-local] %s\n' "$timer_warn" >&2
    printf '[restore-local] WARN: verify DB integrity before the manual restart; no backup runs until then\n' >&2
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
# I81-A (a): single-execution lock (main() entry gate).
#
# SCOPE: this flock serializes restore-local.sh instances against EACH OTHER
# only. backup-local.sh does not take this lock, so it does NOT exclude the
# backup timer. Timer exclusion is the job of stop_services() +
# verify_stopped_services(): jarvis-backup.timer is in SERVICE_INVENTORY, is
# stopped before dispatch, and is confirmed inactive before any module write.
# Acquisition failure is fail-closed with the fixed vocabulary above, before
# verification, inventory, service stop or any write.
#
# Dry-run also takes the lock: it only opens the lock file and never calls
# systemctl or stop_services(), so the "dry-run does not touch the timer"
# contract is unchanged. The lock file is deliberately NOT unlinked - removing
# it while another process holds a fd on the old inode would let a third
# process lock a new inode and break mutual exclusion.
# ---------------------------------------------------------------------------
acquire_restore_lock() {
  if ! command -v flock >/dev/null 2>&1; then
    printf '%s %s: %s\n' "$LOCK_FAIL_TAG" FLOCK_UNAVAILABLE \
      'flock not found in PATH' >&2
    return 1
  fi

  # `exec` with a failing redirection terminates a non-interactive shell, so
  # prove the directory + path are usable first and emit our own fixed-vocabulary
  # line. If the default /run path cannot be created (e.g. a non-root test run),
  # fall back to the documented /tmp path with a WARN. An explicit LOCK_FILE
  # override is never silently redirected.
  local attempt=0
  while [ "$attempt" -lt 2 ]; do
    # -m applies only to the deepest directory; parent /run is owned by systemd.
    # shellcheck disable=SC2174
    if mkdir -p -m 0755 -- "$LOCK_DIR" 2>/dev/null && : >>"$LOCK_FILE" 2>/dev/null; then
      break
    fi
    if [ "$attempt" -eq 0 ] && [ "$LOCK_FILE_EXPLICIT" -eq 0 ] && [ "$LOCK_FALLBACK_TMP" -eq 0 ]; then
      LOCK_FILE="/tmp/jarvis-restore.lock"
      LOCK_DIR="/tmp"
      LOCK_FALLBACK_TMP=1
      attempt=1
      continue
    fi
    printf '%s %s: %s\n' "$LOCK_FAIL_TAG" LOCK_FILE_UNUSABLE \
      "cannot open lock file: $LOCK_FILE" >&2
    return 1
  done

  # I81-A (c): capture the inode identity BEFORE opening the lock fd, then again
  # after. A delete + recreate between the two stats changes %d:%i; refuse to
  # run instead of locking a stale inode and silently losing mutual exclusion.
  local inode_before inode_after fd_inode
  inode_before="$(stat -c '%d:%i' -- "$LOCK_FILE" 2>/dev/null || true)"
  if [ -z "$inode_before" ]; then
    printf '%s %s: %s\n' "$LOCK_FAIL_TAG" LOCK_FILE_UNUSABLE \
      "cannot stat lock file before open: $LOCK_FILE" >&2
    return 1
  fi

  exec {LOCK_FD}<>"$LOCK_FILE"
  inode_after="$(stat -c '%d:%i' -- "$LOCK_FILE" 2>/dev/null || true)"
  fd_inode="$(stat -Lc '%d:%i' -- "/proc/self/fd/$LOCK_FD" 2>/dev/null || true)"
  if [ "$inode_after" != "$inode_before" ] ||
     { [ -n "$fd_inode" ] && [ "$fd_inode" != "$inode_before" ]; }; then
    printf '%s %s: %s\n' "$LOCK_FAIL_TAG" LOCK_INODE_CHANGED \
      "lock file inode changed during open: $LOCK_FILE" >&2
    exec {LOCK_FD}>&-
    LOCK_FD=""
    return 1
  fi
  LOCK_INODE="$inode_before"

  if ! flock -n "$LOCK_FD"; then
    printf '%s %s: %s\n' "$LOCK_FAIL_TAG" LOCK_HELD \
      "another restore holds $LOCK_FILE; refusing to run" >&2
    return 1
  fi

  # I81-A (b): leave a non-sensitive stale-lock clue (owner PID + UTC acquire
  # time) in the file itself. Best effort: diagnostics never abort a valid run.
  # The fd is closed before module dispatch in main(), so module children
  # (psql/curl) do not inherit it and cannot keep the lock after this exit.
  {
    printf 'pid=%s\n' "$$"
    printf 'acquired_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  } >&"$LOCK_FD" || true

  log "lock acquired: $LOCK_FILE (inode=$LOCK_INODE)"
  if [ "$LOCK_FALLBACK_TMP" -eq 1 ]; then
    log "WARN: /run lock path unavailable; using $LOCK_FILE (inode check active)"
  fi
  return 0
}

# ---------------------------------------------------------------------------
# Main flow
# ---------------------------------------------------------------------------
main() {
  if ! acquire_restore_lock; then
    exit 1
  fi
  check_preconditions
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
  # I73-A: the DB DROP/CREATE window must not start while a backup timer is
  # still active. Fail closed before any module dispatch / DB write.
  if ! verify_stopped_services; then
    fail "timer still active after stop; aborting before dispatch"
  fi

  # I81-A (b): bash does not set FD_CLOEXEC on dynamic ({VAR}) fds, so a
  # long-lived module child (psql/curl) would inherit the flock fd and keep the
  # lock held after this process exits -> later runs get LOCK_HELD. Close it
  # before any module code runs; the PID/timestamp written above is the stale
  # lock diagnostic clue. Timer exclusion is unaffected: stop_services() +
  # verify_stopped_services() above handle it, not this fd.
  if [ -n "${LOCK_FD:-}" ]; then
    exec {LOCK_FD}>&-
    LOCK_FD=""
  fi

  dispatch_restore
  log "restore sequence finished"
}

main "$@"
