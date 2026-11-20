#!/usr/bin/env bash
# I93-A: dependency advisory audit gate for both Cargo.lock files.
#
# The gate scans `rust/Cargo.lock` and `ui/Cargo.lock` with `cargo audit
# --no-fetch --file <lock>` from the repository root. Tuning lives in the
# repository-root `audit.toml` (cargo-audit discovers it from the CWD, which
# this script pins to the repo root), so an ignore is never implicit.
#
# Fixed fail-closed vocabulary (no values or secrets in failure output):
#   BAD_USAGE, AUDIT_TOOL_MISSING, AUDIT_CONFIG_MISSING,
#   AUDIT_LOCKFILE_MISSING, AUDIT_FETCH_FAILED, AUDIT_VULNERABILITIES_FOUND
# A non-zero `cargo audit` exit is mapped to AUDIT_VULNERABILITIES_FOUND and
# fails closed. Its normal report is left on stdout for diagnosis; the gate
# itself never prints advisory values, paths beyond the fixed lock list, or
# credentials.
#
# Advisory DB policy (adopted, recorded here and in the Runbook):
#   * The scan ALWAYS uses --no-fetch. Network access is never part of a scan,
#     so a scan cannot silently change which advisory set it used.
#   * Local/offline: if the advisory DB is missing or older than
#     DB_STALE_DAYS, print `UNVERIFIED AUDIT_DB_*` and exit 0. This is the same
#     opt-out convention as check-workflow-pins.sh: local work is not blocked
#     by an unpopulated cache, but the gap is explicit rather than a false
#     "passed" claim.
#   * CI: `extra_gates` sets DEPENDENCY_AUDIT_FETCH=1 (or passes --fetch), so
#     `cargo audit fetch` refreshes the DB first (network READ only) and the
#     subsequent --no-fetch scans are checked against fresh advisories. A
#     failed fetch is a hard failure; CI never degrades to UNVERIFIED.
#   * We do NOT allow fetch implicitly during a scan, and we do NOT downgrade
#     a real advisory finding to UNVERIFIED.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root" || exit 1

readonly CONFIG_FILE="audit.toml"
readonly -a LOCK_FILES=(rust/Cargo.lock ui/Cargo.lock)
readonly DB_STALE_DAYS=14

fail() {
  echo "FAIL $1" >&2
  exit 1
}

unverified() {
  # Explicit "not audited" marker: exit 0 is a documented local opt-out, not
  # a claim that the dependency set is vulnerability-free.
  echo "UNVERIFIED $1" >&2
  exit 0
}

# --fetch is the CI opt-in. DEPENDENCY_AUDIT_FETCH=1 is its environment form.
fetch=0
case "${DEPENDENCY_AUDIT_FETCH:-0}" in
  1|true|TRUE|yes|YES|on|ON) fetch=1 ;;
esac
if [ "$#" -gt 1 ]; then
  fail BAD_USAGE
fi
if [ "$#" -eq 1 ]; then
  case "$1" in
    --fetch) fetch=1 ;;
    *) fail BAD_USAGE ;;
  esac
fi

# cargo-audit is not a default toolchain component. Missing tool is a hard
# failure (fail-closed), not a skipped gate. CI installs a pinned version.
command -v cargo >/dev/null 2>&1 || fail AUDIT_TOOL_MISSING
cargo audit --version >/dev/null 2>&1 || fail AUDIT_TOOL_MISSING

[ -f "$CONFIG_FILE" ] || fail AUDIT_CONFIG_MISSING
for lock in "${LOCK_FILES[@]}"; do
  [ -f "$lock" ] || fail AUDIT_LOCKFILE_MISSING
done

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
db_dir="$cargo_home/advisory-db"

if [ "$fetch" -eq 1 ]; then
  # CI path: refresh the advisory DB before scanning. The fetch is a network
  # read with no repository data sent anywhere; a failure is a hard failure.
  cargo audit fetch >/dev/null 2>&1 || fail AUDIT_FETCH_FAILED
else
  # Local path: --no-fetch cannot scan without a DB. Mark it UNVERIFIED rather
  # than silently passing or silently skipping the gate.
  if [ ! -d "$db_dir" ]; then
    unverified AUDIT_DB_UNVERIFIED
  fi

  # A git-backed advisory DB older than the threshold is also unverified.
  # git is optional; when absent we cannot judge freshness and say so instead
  # of guessing. With --fetch (CI) this check is skipped because we just
  # refreshed the DB.
  if command -v git >/dev/null 2>&1; then
    if git -C "$db_dir" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
      db_epoch="$(git -C "$db_dir" log -1 --format=%ct 2>/dev/null || echo 0)"
      case "$db_epoch" in
        ''|*[!0-9]*) db_epoch=0 ;;
      esac
      now_epoch="$(date +%s)"
      max_age=$((DB_STALE_DAYS * 86400))
      if [ "$db_epoch" -eq 0 ] || [ "$((now_epoch - db_epoch))" -gt "$max_age" ]; then
        unverified AUDIT_DB_STALE
      fi
    fi
  fi
fi

# cargo-audit 0.23 does not auto-discover audit.toml from the CWD. Extract the
# documented temporary suppressions and pass them explicitly as --ignore so
# the version-controlled rationale is the single source of truth.
ignore_args=()
while IFS= read -r advisory_id; do
  [ -n "$advisory_id" ] || continue
  ignore_args+=(--ignore "$advisory_id")
done < <(sed -n 's/^[[:space:]]*"\(RUSTSEC-[0-9]\{4\}-[0-9]\{4\}\)".*/\1/p' "$CONFIG_FILE")

for lock in "${LOCK_FILES[@]}"; do
  echo "== dependency audit: $lock =="
  if ! cargo audit --no-fetch --file "$lock" "${ignore_args[@]}"; then
    fail AUDIT_VULNERABILITIES_FOUND
  fi
done

echo "check-dependency-audit: ok=${#LOCK_FILES[@]}"
