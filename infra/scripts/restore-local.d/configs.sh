#!/usr/bin/env bash
# S6: configs component restore for restore-local (ADR-0004, local-only).
# Sourced by restore-local.sh. The caller defines: SET_DIR, APPLY, CONFIRM,
# log(), fail(). This file defines exactly one function: restore_configs().
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
#     copied to <dest>.bak-<ts> first. No recursive tar extraction is used.

restore_configs() {
  local set_dir="${SET_DIR:-}"
  local apply="${APPLY:-0}"
  local confirm="${CONFIRM:-0}"
  local manifest="$set_dir/manifest.txt"
  local -a names=()
  local -a destinations=()
  local name base dest src ts tmp_dir=""
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

    if [ ! -f "$set_dir/$name" ]; then
      fail "configs: allowlisted config file missing from set: $name"
      return 1
    fi
    names+=("$name")
    destinations+=("$dest")
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
    if [ -f "$dest" ]; then
      cp -p -- "$dest" "${dest}.bak-${ts}" || {
        rm -rf -- "$tmp_dir"
        fail "configs: backup failed for $dest"
        return 1
      }
      log "configs: backup: $(basename -- "$dest").bak-${ts}"
    fi
    cp -- "$tmp_dir/$name" "$dest" || {
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
