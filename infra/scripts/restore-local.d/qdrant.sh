#!/usr/bin/env bash
# S5 / Qdrant fragment of restore-local (ADR-0004, local-only).
# Sourced by the caller: SET_DIR, APPLY, CONFIRM, log(), fail() are pre-defined.
# Only one function is defined here: restore_qdrant() (no args, returns 0/1).
#
# INVARIANT: this fragment never manages the Qdrant process. The REST API only
# answers while the process is listening, so "stop process -> HTTP upload" is
# forbidden (the old restore.rs bug). No service-management command is issued.
#
# No snapshot content and no payload other than the points_count validation
# result is ever written to the log.
# I75-B: the snapshot path is resolved from the manifest role/format
# descriptors by the shared resolve_restore_input() helper (defined in
# restore-local.d/configs.sh); qdrant-snapshot.bin is only the legacy 3-column
# fallback. The none/none descriptor contract is unchanged (P1-6 undecided).

restore_qdrant() {
  if [ "$#" -ne 0 ]; then
    fail "restore_qdrant: unexpected arguments (expected none)"
    return 1
  fi

  local snapshot_file
  if ! declare -F resolve_restore_input >/dev/null 2>&1; then
    fail "Qdrant: shared restore input resolver unavailable"
    return 1
  fi
  if ! snapshot_file="$(resolve_restore_input "$SET_DIR" qdrant qdrant-snapshot.bin)"; then
    fail "Qdrant: cannot resolve a unique qdrant payload from manifest (unknown/ambiguous/missing)"
    return 1
  fi
  local qdrant_url="${QDRANT_URL:-http://127.0.0.1:6333}"
  # Single source of truth shared with backup-local.sh; mirrors the
  # configurable jarvis-context `collection_name`. The default preserves the
  # existing behaviour. The legacy QDRANT_COLLECTION driver global is not used.
  local qdrant_collection="${JARVIS_QDRANT_COLLECTION:-jarvis_memories}"
  local isolated_collection="${qdrant_collection}_restore_test"
  local isolated_url="${qdrant_url}/collections/${isolated_collection}"
  local production_url="${qdrant_url}/collections/${qdrant_collection}"

  # --- APPLY=0: plan only, zero writes (no HTTP request at all) -------------
  if [ "$APPLY" -eq 0 ]; then
    log "Qdrant: APPLY=0 -> plan only; no HTTP request and no write is issued"
    log "Qdrant: input ${snapshot_file} (tar snapshot; contents not read or logged)"
    log "Qdrant: would POST /collections/${isolated_collection}/snapshots/upload?priority=snapshot and validate HTTP status"
    log "Qdrant: would verify points_count (non-empty), then DELETE /collections/${isolated_collection}"
    if [ "$CONFIRM" -eq 1 ]; then
      log "Qdrant: production apply requires APPLY=1 as well as CONFIRM=1; production stays untouched"
    fi
    log "Qdrant: production target ${qdrant_collection} (same-collection API restore for now; alias/new-collection switch undecided)"
    log "Qdrant: process stays listening throughout; this fragment performs no service management"
    return 0
  fi

  if [ ! -f "$snapshot_file" ] || [ ! -r "$snapshot_file" ]; then
    fail "Qdrant: snapshot file missing or unreadable: ${snapshot_file}"
    return 1
  fi

  # --- Step 0: remove a stale isolated collection from a previous failed run -
  # Without this, a leftover collection keeps old points and Step 2a's
  # points_count > 0 check can pass even when this upload did not take effect.
  # GET the isolated collection (body discarded, never logged): a 2xx means it
  # exists and must be deleted before upload; 404 means nothing to clean up.
  # Any other status or curl error fails closed, before production is touched.
  local stale_code="" stale_rc=0
  stale_code="$(curl -sS -o /dev/null -w '%{http_code}' \
    "${isolated_url}")" || stale_rc=$?
  if [ "$stale_rc" -ne 0 ] || \
     [[ ! "$stale_code" =~ ^2[0-9][0-9]$ && "$stale_code" != "404" ]]; then
    fail "Qdrant: isolated collection info request failed (http=${stale_code}, curl_rc=${stale_rc})"
    return 1
  fi
  if [[ "$stale_code" =~ ^2[0-9][0-9]$ ]]; then
    local stale_del_code="" stale_del_rc=0
    stale_del_code="$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE \
      "${isolated_url}")" || stale_del_rc=$?
    if [ "$stale_del_rc" -ne 0 ] || [[ ! "$stale_del_code" =~ ^2[0-9][0-9]$ ]]; then
      fail "Qdrant: isolated collection cleanup failed (http=${stale_del_code}, curl_rc=${stale_del_rc})"
      return 1
    fi
    log "Qdrant: stale isolated collection removed before upload"
  fi

  # --- Step 1: upload to the isolated collection (priority=snapshot) --------
  # curl -F supplies its own multipart boundary; do not override Content-Type.
  local upload_code="" upload_rc=0
  upload_code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
    -F "snapshot=@${snapshot_file}" \
    "${isolated_url}/snapshots/upload?priority=snapshot")" || upload_rc=$?
  if [ "$upload_rc" -ne 0 ] || [[ ! "$upload_code" =~ ^2[0-9][0-9]$ ]]; then
    fail "Qdrant: isolated snapshot upload failed (http=${upload_code}, curl_rc=${upload_rc})"
    return 1
  fi
  log "Qdrant: isolated snapshot upload accepted (http=${upload_code}, priority=snapshot)"

  # --- Step 2a: verify points_count of the isolated collection -------------
  local probe="" probe_rc=0 probe_code="" probe_body="" points_count=""
  probe="$(curl -sS -w $'\n%{http_code}' \
    "${isolated_url}")" || probe_rc=$?
  probe_code="${probe##*$'\n'}"
  probe_body="${probe%$'\n'*}"
  if [ "$probe_rc" -ne 0 ] || [[ ! "$probe_code" =~ ^2[0-9][0-9]$ ]]; then
    fail "Qdrant: isolated collection info request failed (http=${probe_code}, curl_rc=${probe_rc})"
    return 1
  fi
  points_count="$(printf '%s\n' "$probe_body" | jq -er '.result.points_count' 2>/dev/null)" || {
    fail "Qdrant: could not read points_count from isolated collection"
    return 1
  }
  case "$points_count" in
    ''|*[!0-9]*) fail "Qdrant: unexpected points_count format in isolated collection"; return 1 ;;
  esac
  if [ "$points_count" -le 0 ]; then
    fail "Qdrant: isolated collection has no points (check priority=snapshot)"
    return 1
  fi
  log "Qdrant: isolated points_count validation ok (non-empty; value not logged)"

  # --- Step 2b: remove the isolated collection ------------------------------
  local del_code="" del_rc=0
  del_code="$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE \
    "${isolated_url}")" || del_rc=$?
  if [ "$del_rc" -ne 0 ] || [[ ! "$del_code" =~ ^2[0-9][0-9]$ ]]; then
    fail "Qdrant: isolated collection cleanup failed (http=${del_code}, curl_rc=${del_rc})"
    return 1
  fi
  log "Qdrant: isolated collection deleted"

  # --- Step 3: production apply, only with --confirm -----------------------
  if [ "$CONFIRM" -eq 1 ]; then
    # The evacuation of the old collection is recorded by name only here.
    # UNDECIDED (design doc "未決定事項"): same-collection API restore vs.
    # new collection + alias switch. For now we restore into the same
    # collection; the actual evacuation target/retention is still undecided.
    log "Qdrant: --confirm -> production apply; evacuating old collection '${qdrant_collection}' (name only)"

    local prod_code="" prod_rc=0
    prod_code="$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
      -F "snapshot=@${snapshot_file}" \
      "${production_url}/snapshots/upload?priority=snapshot")" || prod_rc=$?
    if [ "$prod_rc" -ne 0 ] || [[ ! "$prod_code" =~ ^2[0-9][0-9]$ ]]; then
      fail "Qdrant: production snapshot upload failed (http=${prod_code}, curl_rc=${prod_rc})"
      return 1
    fi

    local prod_probe="" prod_probe_rc=0 prod_probe_code="" prod_probe_body="" prod_points=""
    prod_probe="$(curl -sS -w $'\n%{http_code}' \
      "${production_url}")" || prod_probe_rc=$?
    prod_probe_code="${prod_probe##*$'\n'}"
    prod_probe_body="${prod_probe%$'\n'*}"
    if [ "$prod_probe_rc" -ne 0 ] || [[ ! "$prod_probe_code" =~ ^2[0-9][0-9]$ ]]; then
      fail "Qdrant: production collection info request failed (http=${prod_probe_code}, curl_rc=${prod_probe_rc})"
      return 1
    fi
    prod_points="$(printf '%s\n' "$prod_probe_body" | jq -er '.result.points_count' 2>/dev/null)" || {
      fail "Qdrant: could not read points_count from production collection"
      return 1
    }
    case "$prod_points" in
      ''|*[!0-9]*) fail "Qdrant: unexpected points_count format in production collection"; return 1 ;;
    esac
    if [ "$prod_points" -le 0 ]; then
      fail "Qdrant: production collection has no points after restore"
      return 1
    fi
    log "Qdrant: production collection re-verified (points_count ok, value not logged)"
  else
    log "Qdrant: CONFIRM=0 -> production collection ${qdrant_collection} untouched"
  fi

  return 0
}
