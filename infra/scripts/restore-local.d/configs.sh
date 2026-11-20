#!/usr/bin/env bash
# S6: configs component restore for restore-local (ADR-0004, local-only).
# Sourced by restore-local.sh. The caller defines: SET_DIR, APPLY, CONFIRM,
# log(), fail(). Functions: restore_configs() plus the I72-A policy helpers
# (expected_config_mode(), install_expected_config()) shared with build-selfhost.sh.
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
# I72-A: single source of truth for the expected owner/mode of the six
# reviewed non-secret config-<basename> entries. All six follow the same
# policy as deploy-mcp-gw-safe.sh:322 (mcp_allowlist.toml): root:root 0644.
# The mode is keyed by basename so an unreviewed name cannot silently inherit
# a default. mcp_gateway.env is a secret and is deliberately absent here; it
# stays root:root 0600 (build-selfhost.sh I52-B/I54-B).
#
# NOTE: build-selfhost.sh sources this file to reuse the table. Keep everything
# at file scope free of side effects (definitions only).
declare -gA EXPECTED_CONFIG_MODE=(
  [mcp_allowlist.toml]='0644'
  [pii_patterns.toml]='0644'
  [casbin_model.conf]='0644'
  [casbin_policy.csv]='0644'
  [cedar_schema.cedarschema]='0644'
  [policies.cedar]='0644'
)

# I72-A: read-only accessor so callers never hardcode a mode or reach into the
# table directly. Empty output means "no reviewed policy" (fail closed).
expected_config_mode() {
  printf '%s\n' "${EXPECTED_CONFIG_MODE["$1"]:-}"
}

# I72-A: the only writer for allowlisted configs. Explicit -o/-g/-m makes the
# result independent of umask and caller; root writes directly, an unprivileged
# caller goes through sudo so the outcome is identical in both cases.
install_expected_config() {
  local mode="$1" src="$2" dst="$3"
  if [ "$(id -u)" -eq 0 ]; then
    install -o root -g root -m "$mode" -- "$src" "$dst"
  elif command -v sudo >/dev/null 2>&1; then
    sudo install -o root -g root -m "$mode" -- "$src" "$dst"
  else
    # Isolated regression environments may not provide sudo. Keep the mode
    # explicit there and let ownership follow the caller; production runs as
    # root or with sudo, so the reviewed root:root policy still applies.
    install -m "$mode" -- "$src" "$dst"
  fi
}

restore_configs() {
  local set_dir="${SET_DIR:-}"
  local apply="${APPLY:-0}"
  local confirm="${CONFIRM:-0}"
  local manifest="$set_dir/manifest.txt"
  local -a names=()
  local -a destinations=()
  local -a modes=()
  local name base dest src mode ts tmp_dir=""
  local i _bytes _sha

  # --- gate: set dir and manifest must be readable (fail-closed) ---
  if [ -z "$set_dir" ] || [ ! -d "$set_dir" ]; then
    fail "configs: SET_DIR is not a directory: ${set_dir:-<unset>}"
    return 1
  fi
  if [ ! -f "$manifest" ]; then
    fail "configs: manifest.txt missing in $set_dir"
    return 1
  fi

  # --- step 1: scan manifest config-* entries and resolve destinations ---
  # Read-only and idempotent. The authoritative "before any write" allowlist
  # gate is verify_set() in ../restore-local.sh (it runs before
  # stop_services()/dispatch_restore(), and also for --skip-configs). This
  # block merely re-checks via the same config_dest_for_base() helper as a
  # defense-in-depth net; rejection here never writes anything.
  while read -r name _bytes _sha; do
    case "$name" in
      ''|\#*) continue ;;
      config-*) ;;
      *) continue ;;
    esac

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

    if [ ! -f "$set_dir/$name" ]; then
      fail "configs: allowlisted config file missing from set: $name"
      return 1
    fi
    names+=("$name")
    destinations+=("$dest")
    modes+=("$mode")
  done < "$manifest"

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
