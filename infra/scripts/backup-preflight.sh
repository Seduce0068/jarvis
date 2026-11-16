#!/usr/bin/env bash
# BK-1/BK-2/BK-4/SEC-2 safety gate: read-only backup preflight.
#
# Inventories the prerequisites for a future local-only backup without
# reading, copying, or printing any secret values. This script NEVER:
#   - writes outside its own stdout,
#   - starts/stops services,
#   - opens a backup destination for writing,
#   - reads the contents of /etc/jarvis/*.env or any credentials.
#
# Usage: bash infra/scripts/backup-preflight.sh
set -uo pipefail

ok=0; warn=0; fail=0
say() { printf '%-28s %s\n' "$1" "$2"; }
pass() { say "$1" "${2:-OK}"; ok=$((ok+1)); }
warns() { say "$1" "WARN: $2"; warn=$((warn+1)); }
fails() { say "$1" "FAIL: $2"; fail=$((fail+1)); }

echo "== backup preflight (read-only) =="

# 1) Services that own the data to be backed up.
for unit in postgresql.service qdrant.service; do
  if systemctl is-active --quiet "$unit" 2>/dev/null; then
    pass "$unit"
  else
    warns "$unit" "not active (backup would need adjustment)"
  fi
done

# 2) Backup tooling availability (names only).
for tool in pg_dump pg_dumpall tar sha256sum; do
  if command -v "$tool" >/dev/null 2>&1; then
    pass "tool:$tool"
  else
    fails "tool:$tool" "not on PATH"
  fi
done
if command -v rclone >/dev/null 2>&1; then
  warns "tool:rclone" "present but OCI is out of scope (ADR-0004)"
else
  pass "tool:rclone" "absent - expected (no OCI)"
fi

# 3) Config inventory: file names and modes only. Never print contents.
config_dir="/etc/jarvis"
if [ -d "$config_dir" ]; then
  pass "config dir" 
  # Count only; list non-secret config file names.
  non_secret=$(find "$config_dir" -maxdepth 1 -type f \
    ! -name '*.env' ! -name '*token*' ! -name '*key*' -printf '%f\n' 2>/dev/null | wc -l)
  say "config non-secret files" "$non_secret (names not printed)"
  secret_like=$(find "$config_dir" -maxdepth 1 -type f \
    \( -name '*.env' -o -name '*token*' -o -name '*key*' \) -printf '%f\n' 2>/dev/null | wc -l)
  if [ "$secret_like" -gt 0 ]; then
    warns "config secret-like files" "$secret_like (excluded from inventory; encrypt if backed up)"
  fi
else
  fails "config dir" "$config_dir missing"
fi

# 4) Legacy 1Password references in repo templates (BK-4/SEC-2).
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
op_refs=$(grep -rl 'op://' "$repo/infra/config" 2>/dev/null | wc -l)
if [ "$op_refs" -gt 0 ]; then
  warns "legacy op:// templates" "$op_refs files (reference only; BK-4 decision pending)"
else
  pass "legacy op:// templates"
fi

# 5) Backup timer state (must NOT be enabled before the design is settled).
if systemctl list-unit-files jarvis-backup.timer --no-pager 2>/dev/null | grep -q jarvis-backup.timer; then
  if systemctl is-active --quiet jarvis-backup.timer 2>/dev/null; then
    fails "backup timer" "active before design settled (BK-1/BK-2 gate)"
  else
    warns "backup timer" "installed but inactive"
  fi
else
  pass "backup timer" "not installed - expected"
fi

echo "== result: ok=$ok warn=$warn fail=$fail =="
# Warnings are informational; failures block enabling a backup job.
[ "$fail" -eq 0 ]
