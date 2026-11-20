#!/usr/bin/env bash
# S6: configs component restore for restore-local (ADR-0004, local-only).
# Sourced by restore-local.sh. The caller defines: SET_DIR, APPLY, CONFIRM,
# log(), fail(). Functions: restore_configs() plus the I72-A policy helpers
# (expected_config_mode(), install_expected_config()), shared with
# build-selfhost.sh and deploy-mcp-gw-safe.sh (I74-C: 3経路共有).
#
# Safety invariants:
#   - Only the 6 non-secret config-<basename> allowlist entries are ever read
#     or copied. bws.env / *.env / otelcol-config.yaml / token files are never
#     opened. The authoritative "before any write" rejection of non-allowlisted
#     config-* names now lives in verify_set() in ../restore-local.sh, which
#     runs before stop_services()/dispatch_restore() and even with --skip-configs.
#     The re-check below is read-only and idempotent; it reuses the very same
#     config_dest_for_base() helper as a defense-in-depth net.
#   - Config contents are NEVER printed. Only filenames and a change status
#     are logged (diff summary, filename only).
#   - APPLY=0 => plan only, zero writes (no temp dir, no production copy).
#   - Production writes require APPLY=1 AND CONFIRM=1; the previous file is
#     written to <dest>.bak-<ts> first with the same policy. No recursive tar
#     extraction is used.
#   - I72-A: every production write (the .bak and the new file) goes through
#     install_expected_config() with the reviewed owner/mode policy below, so
#     neither the caller's umask nor its identity can change the result.
#
# I72-A / I74-C: single source of truth for the reviewed owner/mode policy.
# This ONE table is shared by ALL THREE mcp_allowlist.toml write paths:
#   (1) build-selfhost.sh                       - fresh deploy of config files
#   (2) restore-local.d/configs.sh restore_configs() - .bak + restore writes
#   (3) deploy-mcp-gw-safe.sh                   - step3 placement + rollback
# Every path must resolve its mode via expected_config_mode() and write via
# install_expected_config(); no path may hardcode `install -o root -g root -m`.
# The six config-<basename> keys below are the reviewed non-secret restore
# entries (identical to config_dest_for_base() in ../restore-local.sh); all are
# root:root 0644. The namespaced systemd/<unit> key is a deploy-only artifact
# used exclusively by path (3) and must never be treated as a restore entry.
# Keying by basename prevents an unreviewed name from inheriting a default.
# mcp_gateway.env is a secret and is deliberately absent here; it stays
# root:root 0600 (build-selfhost.sh I52-B/I54-B).
#
# NOTE: build-selfhost.sh and deploy-mcp-gw-safe.sh both source this file to
# reuse the table. Keep everything at file scope free of side effects
# (definitions only).
# --- I75-B: shared restore-input resolver ------------------------------------
# Single derivation path for the input basename of every restore component.
# The classification is delegated to lib/payload-descriptors.sh (sourced by
# restore-local.sh before load_modules()), so verify_set()/verify-backup.sh and
# the four restore modules cannot disagree about a payload's role/format.
#
# resolve_restore_input <set_dir> <role> <legacy_name> [one|all]
#   stdout : "$set_dir/<basename>" (one path in `one` mode, all matches in
#            `all` mode).  A basename is derived from the manifest row whose
#            resolved role == <role>; the declared format participates through
#            resolve_descriptors()/descriptor_supported().
#   return : 0 resolved, 1 invalid / ambiguous / resolved file missing
#            (fail-closed), 2 no role match (absent; only optional components
#            may treat this as "nothing to do").
#   Backward compatibility: a legacy 3-column row has no role column, so
#   resolve_descriptors() derives the role from the canonical basename via
#   legacy_role_for(). If that still yields no match, <legacy_name> is accepted
#   ONLY when it is actually recorded in the manifest - a renamed payload in a
#   role-less manifest is intentionally not discovered.
#   P1-6 connection point: descriptor_supported() currently accepts only
#   encryption=none / signature=none. Future encryption/signature support (a
#   user decision) plugs in here and in lib/payload-descriptors.sh; this helper
#   never decrypts and never opens a payload's contents.
resolve_restore_input() {
  local set_dir="$1" role="$2" legacy_name="${3:-}" mode="${4:-one}"
  local manifest="$set_dir/manifest.txt"
  local name _bytes _sha format encryption signature row_role resolved item
  local -a matches=()
  local matched_count=0

  [ -n "$set_dir" ] && [ -d "$set_dir" ] && [ -f "$manifest" ] || return 1

  while read -r name _bytes _sha format encryption signature row_role; do
    case "$name" in ''|\#*) continue ;; esac
    # Unknown explicit roles fail closed, exactly as in verify_set().
    case "$row_role" in
      ''|postgres|qdrant|config|git|other) ;;
      *) return 1 ;;
    esac
    resolved="$(resolve_descriptors "$name" "$format" "$encryption" "$signature" "$row_role")" || return 1
    read -r format encryption signature row_role <<< "$resolved"
    # I75-B: only the none/none contract is accepted (P1-6 undecided).
    descriptor_supported "$format" "$encryption" "$signature" || return 1
    # A role claim that the real basename does not back is rejected by the
    # same shared rule the verification gate uses.
    role_basename_consistent "$name" "$row_role" || return 1
    if [ "$row_role" = "$role" ]; then
      matches+=("$name")
      matched_count=$((matched_count + 1))
    fi
  done < "$manifest"

  # Legacy 3-column fallback: derive from the existing fixed name, but only if
  # that name is recorded in this manifest (no bypass of integrity checking).
  if [ "$matched_count" -eq 0 ] && [ -n "$legacy_name" ]; then
    while read -r name _bytes _sha; do
      case "$name" in ''|\#*) continue ;; esac
      if [ "$name" = "$legacy_name" ]; then
        matches+=("$name")
        matched_count=$((matched_count + 1))
      fi
    done < "$manifest"
  fi

  [ "$matched_count" -ne 0 ] || return 2
  # Fail closed on a resolved payload that is absent on disk.
  for item in "${matches[@]}"; do
    [ -f "$set_dir/$item" ] || return 1
  done

  case "$mode" in
    one)
      [ "$matched_count" -eq 1 ] || return 1 # ambiguous: multiple matches
      printf '%s\n' "$set_dir/${matches[0]}"
      ;;
    all)
      for item in "${matches[@]}"; do
        printf '%s\n' "$set_dir/$item"
      done
      ;;
    *)
      return 1
      ;;
  esac
  return 0
}

declare -gA EXPECTED_CONFIG_MODE=(
  [mcp_allowlist.toml]='0644'
  [pii_patterns.toml]='0644'
  [casbin_model.conf]='0644'
  [casbin_policy.csv]='0644'
  [cedar_schema.cedarschema]='0644'
  [policies.cedar]='0644'
  # Deploy-only (path 3), NOT a restore config-<basename> entry: the mcp-gw
  # unit installed by deploy-mcp-gw-safe.sh is also root:root 0644.
  [systemd/jarvis-mcp-gw.service]='0644'
)

# I72-A: read-only accessor so callers never hardcode a mode or reach into the
# table directly. Empty output means "no reviewed policy" (fail closed).
expected_config_mode() {
  printf '%s\n' "${EXPECTED_CONFIG_MODE["$1"]:-}"
}

# I72-A / I74-C (b): the only writer for reviewed configs. Explicit -o/-g/-m
# makes the result independent of umask and caller: root writes directly, an
# unprivileged caller goes through sudo, so ownership is root:root in BOTH
# production cases. If neither is available, FAIL CLOSED -- a mode-only install
# would silently leave the file owned by the caller and break the root:root
# invariant. The legacy mode-only fallback is permitted ONLY with the explicit
# isolated-test opt-in JARVIS_RESTORE_TEST_MODE=1.
install_expected_config() {
  local mode="$1" src="$2" dst="$3"
  if [ "$(id -u)" -eq 0 ]; then
    install -o root -g root -m "$mode" -- "$src" "$dst"
    return $?
  fi
  if command -v sudo >/dev/null 2>&1; then
    sudo install -o root -g root -m "$mode" -- "$src" "$dst"
    return $?
  fi
  if [ "${JARVIS_RESTORE_TEST_MODE:-0}" != "1" ]; then
    printf '%s\n' \
      "install_expected_config: cannot enforce root:root for ${dst} (no root, no sudo); refusing. Set JARVIS_RESTORE_TEST_MODE=1 only in isolated tests." >&2
    return 1
  fi
  # Isolated regression environments only: keep the legacy mode-only fallback
  # and state plainly that ownership is NOT enforced on this path.
  printf '%s\n' \
    "install_expected_config: TEST MODE fallback for ${dst} (owner left to caller; root:root NOT enforced)" >&2
  install -m "$mode" -- "$src" "$dst"
  return $?
}

restore_configs() {
  local set_dir="${SET_DIR:-}"
  local apply="${APPLY:-0}"
  local confirm="${CONFIRM:-0}"
  local manifest="$set_dir/manifest.txt"
  local -a names=()
  local -a destinations=()
  local -a modes=()
  local -a inputs=()
  local name base dest src mode ts tmp_dir="" path
  local i input_text="" rc=0

  # --- gate: set dir and manifest must be readable (fail-closed) ---
  if [ -z "$set_dir" ] || [ ! -d "$set_dir" ]; then
    fail "configs: SET_DIR is not a directory: ${set_dir:-<unset>}"
    return 1
  fi
  if [ ! -f "$manifest" ]; then
    fail "configs: manifest.txt missing in $set_dir"
    return 1
  fi

  # --- step 1: resolve config payloads via the shared descriptor helper -----
  # I75-B: input basenames come from resolve_restore_input() (role=config), not
  # from a fixed name. Read-only and idempotent. The authoritative "before any
  # write" allowlist gate is verify_set() in ../restore-local.sh (it runs before
  # stop_services()/dispatch_restore(), and also for --skip-configs). This block
  # merely re-checks via the same config_dest_for_base() helper as a
  # defense-in-depth net; rejection here never writes anything.
  input_text="$(resolve_restore_input "$set_dir" config '' all)" || rc=$?
  if [ "$rc" -eq 2 ]; then
    log "configs: no config payload resolved from manifest; nothing to do"
    return 0
  elif [ "$rc" -ne 0 ]; then
    fail "configs: cannot resolve config payloads from manifest (unknown/ambiguous/missing)"
    return 1
  fi
  if [ -n "$input_text" ]; then
    mapfile -t inputs <<< "$input_text"
  fi

  for path in "${inputs[@]}"; do
    name="${path##*/}"
    base="${name#config-}"
    if ! dest="$(config_dest_for_base "$base")"; then
      fail "configs: manifest has non-allowlisted config entry: $name"
      return 1
    fi

    # I72-A: fail closed before any write if the reviewed mode policy is absent.
    mode="$(expected_config_mode "$base")"
    if [ -z "$mode" ]; then
      fail "configs: no expected mode policy for $base"
      return 1
    fi

    if [ ! -f "$path" ]; then
      fail "configs: allowlisted config file missing from set: $name"
      return 1
    fi
    names+=("$name")
    destinations+=("$dest")
    modes+=("$mode")
  done

  if [ "${#names[@]}" -eq 0 ]; then
    log "configs: no allowlisted config entries in manifest; nothing to do"
    return 0
  fi

  # --- plan (always emitted; no writes, no content) ---
  log "configs: plan: ${#names[@]} allowlisted file(s) from ${set_dir}"
  for name in "${names[@]}"; do
    log "configs: plan: would restore $name"
  done

  # --- step 4: APPLY=0 => plan only, zero writes ---
  if [ "$apply" -ne 1 ]; then
    if [ "$confirm" -eq 1 ]; then
      log "configs: CONFIRM ignored because APPLY=0 (no writes)"
    fi
    log "configs: dry-run (APPLY=0): plan only, zero writes"
    return 0
  fi

  # --- step 2: stage allowed files in a private temp dir (no tar) ---
  tmp_dir="$(mktemp -d)" || {
    fail "configs: cannot create temp directory"
    return 1
  }
  for name in "${names[@]}"; do
    src="$set_dir/$name"
    cp -- "$src" "$tmp_dir/$name" || {
      rm -rf -- "$tmp_dir"
      fail "configs: failed to stage $name"
      return 1
    }
  done

  # Diff summary against the live /etc/jarvis files: filename + status only.
  # cmp -s is silent, so no config content ever reaches stdout/log.
  for i in "${!names[@]}"; do
    name="${names[$i]}"
    dest="${destinations[$i]}"
    if [ ! -e "$dest" ]; then
      log "configs: diff: $name -> new"
    elif cmp -s -- "$tmp_dir/$name" "$dest"; then
      log "configs: diff: $name -> unchanged"
    else
      log "configs: diff: $name -> differs"
    fi
  done

  # --- step 3: production overwrite only with APPLY=1 and CONFIRM=1 ---
  if [ "$confirm" -ne 1 ]; then
    log "configs: APPLY without CONFIRM: staged + diffed only; /etc/jarvis untouched"
    rm -rf -- "$tmp_dir"
    return 0
  fi

  ts="$(date +%Y%m%d-%H%M%S)"
  for i in "${!names[@]}"; do
    name="${names[$i]}"
    dest="${destinations[$i]}"
    mode="${modes[$i]}"
    # I72-A: the .bak is created with the same reviewed policy as the live
    # file; a bare `cp -p` would preserve a stale/misowned mode.
    if [ -f "$dest" ]; then
      install_expected_config "$mode" "$dest" "${dest}.bak-${ts}" || {
        rm -rf -- "$tmp_dir"
        fail "configs: backup failed for $dest"
        return 1
      }
      log "configs: backup: $(basename -- "$dest").bak-${ts}"
    fi
    install_expected_config "$mode" "$tmp_dir/$name" "$dest" || {
      rm -rf -- "$tmp_dir"
      fail "configs: copy failed for $name -> $dest"
      return 1
    }
    log "configs: installed: $name"
  done

  rm -rf -- "$tmp_dir"
  log "configs: restore complete (${#names[@]} file(s))"
  return 0
}
