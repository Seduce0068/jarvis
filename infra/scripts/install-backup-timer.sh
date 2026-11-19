#!/usr/bin/env bash
# Install the local-only JARVIS backup service + timer (BK-2).
# Dry-run by default; --apply performs the changes. Root required for --apply.
# Idempotent and reversible: previous unit files are backed up before replace.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
apply=0
[ "${1:-}" = "--apply" ] && apply=1

service_src="$repo/infra/systemd/jarvis-backup.service"
timer_src="$repo/infra/systemd/jarvis-backup.timer"
script_src="$repo/infra/scripts/backup-local.sh"
verify_src="$repo/infra/scripts/verify-backup.sh"
# Shared migration unit (I32-B): distributed with the same root:root 0644
# framing as the backup units. It expects the deployed Rust workspace sources
# at /opt/jarvis/rust/migrations (same root as the other jarvis services), and
# is pulled in by Wants= from policy/context, so no separate enable is needed.
migrations_unit_src="$repo/infra/systemd/jarvis-migrations.service"
migrations_unit_dst="/etc/systemd/system/jarvis-migrations.service"
service_dst="/etc/systemd/system/jarvis-backup.service"
timer_dst="/etc/systemd/system/jarvis-backup.timer"
script_dst="/opt/jarvis/scripts/backup-local.sh"
verify_dst="/opt/jarvis/scripts/verify-backup.sh"
backup_dir="/var/lib/postgresql/jarvis-backups"
ts="$(date +%Y%m%d-%H%M%S)"

echo "== backup timer install (apply=$apply) =="
if [ "$apply" -ne 1 ]; then
  echo "  DRY: install $script_src -> $script_dst (root:root 0755)"
  echo "  DRY: install $verify_src -> $verify_dst (root:root 0755)"
  echo "  DRY: install $service_src -> $service_dst (root:root 0644)"
  echo "  DRY: install $timer_src   -> $timer_dst   (root:root 0644)"
  echo "  DRY: install $migrations_unit_src -> $migrations_unit_dst (root:root 0644)"
  echo "  DRY: jarvis-migrations.service runs via Wants= from policy/context (no enable)"
  echo "  DRY: mkdir -p $backup_dir (owner postgres:postgres, mode 0700)"
  echo "  DRY: systemctl daemon-reload; systemctl enable --now jarvis-backup.timer"
  echo "  DRY: verify timer active + next elapse"
  echo "  DRY: run service once and verify backup integrity (read-only)"
  echo "  DRY: service ExecStartPost verifies the latest set on every nightly run"
  exit 0
fi

[ "$(id -u)" -eq 0 ] || { echo "ERROR: --apply requires root" >&2; exit 1; }

# ERR trap must be disarmed before rollback runs: otherwise a failure inside
# rollback would re-enter it. Explicit `exit 1` paths call fail() instead.
rollback() {
  trap - ERR
  echo "== ROLLBACK ==" >&2
  # Stop and disable the timer before touching unit files: restoring the files
  # alone leaves an already enabled/started timer running. Only act when
  # systemd knows the unit; ignore any failure so restore + reload still run.
  if systemctl cat jarvis-backup.timer >/dev/null 2>&1; then
    systemctl stop jarvis-backup.timer || true
    systemctl disable jarvis-backup.timer || true
  fi
  if [ -f "$service_dst.bak-$ts" ]; then mv -f "$service_dst.bak-$ts" "$service_dst" || true; else rm -f "$service_dst" || true; fi
  if [ -f "$timer_dst.bak-$ts" ]; then mv -f "$timer_dst.bak-$ts" "$timer_dst" || true; else rm -f "$timer_dst" || true; fi
  if [ -f "$script_dst.bak-$ts" ]; then mv -f "$script_dst.bak-$ts" "$script_dst" || true; else rm -f "$script_dst" || true; fi
  systemctl daemon-reload || true
}

trap 'rollback' ERR
fail() {
  echo "ERROR: $*" >&2
  rollback
  exit 1
}

install -o root -g root -m 0755 "$script_src" "$script_dst"
for pair in "$service_src:$service_dst" "$timer_src:$timer_dst" "$migrations_unit_src:$migrations_unit_dst"; do
  src="${pair%%:*}"; dst="${pair##*:}"
  [ -f "$dst" ] && cp -a "$dst" "$dst.bak-$ts"
  install -o root -g root -m 0644 "$src" "$dst"
done
mkdir -p "$backup_dir"
chown postgres:postgres "$backup_dir"
chmod 0700 "$backup_dir"

systemctl daemon-reload
systemctl enable --now jarvis-backup.timer

sleep 2

# Post-install verification: an enabled-but-never-firing timer is not useful.
# Check the timer is active, that it has a next elapse, and that one manual
# run succeeds (non-destructive payloads only; no retention pruning).
echo "== post-install verification =="
systemctl is-active --quiet jarvis-backup.timer \
  || fail "timer not active"
echo "  timer: active"

next_elapse="$(systemctl list-timers jarvis-backup.timer --no-pager 2>/dev/null \
  | awk 'NR==2 {print $1, $2, $3}')"
if [ -z "$next_elapse" ]; then
  fail "timer has no next elapse"
fi
echo "  next elapse: $next_elapse"

# Run the service once now to prove it works. This is a real backup, but
# writes only to the dedicated backup dir and never prunes (no --retain-days).
echo "  running service once (manual trigger) ..."
if ! systemctl start jarvis-backup.service; then
  systemctl status jarvis-backup.service --no-pager -l >&2 || true
  fail "backup service failed on manual run"
fi
if ! systemctl is-active --quiet jarvis-backup.service \
   && [ "$(systemctl show -p ExecMainStatus --value jarvis-backup.service)" != "0" ]; then
  fail "backup service exited non-zero"
fi
echo "  manual run: OK"

# Verify the newly produced backup set integrity (read-only).
# jq is mandatory for manifest verification: without it we cannot claim the
# backup was verified. Fail loudly (rollback + exit) instead of silently
# skipping verification and still printing the "verified" DONE message.
command -v jq >/dev/null 2>&1 || fail "jq is required for backup integrity verification"
# jarvis-backup.service now runs this same working copy via ExecStartPost after
# every nightly backup. Fail closed here as well: an enabled timer whose daily
# verifier is missing would leave failed units while this installer still
# printed the "verified" DONE message. Same framework as the jq check above.
[ -f "$verify_src" ] || fail "verify-backup.sh working copy is required for daily verification: $verify_src"
install -o root -g root -m 0755 "$verify_src" "$verify_dst"
echo "  verifying backup integrity ..."
if ! sudo -u postgres bash "$verify_dst" --backup-dir "$backup_dir" | tail -20; then
  fail "backup integrity verification failed"
fi

trap - ERR
systemctl list-timers jarvis-backup.timer --no-pager | head -3
echo "== DONE: backup timer installed, enabled and verified =="
