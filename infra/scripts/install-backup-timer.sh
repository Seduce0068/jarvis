#!/usr/bin/env bash
# Install the local-only JARVIS backup service + timer (BK-2).
# Dry-run by default; --apply performs the changes. Root required for --apply.
# Idempotent and reversible: previous unit files are backed up before replace,
# and pre-existing timer state (active/enabled) is restored on rollback instead
# of being stopped unconditionally. The next elapse is NOT restored: systemd has
# no API to pin a timer's next trigger, so it is recalculated and may differ.
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

# Single source of truth for every configuration file this installer replaces.
# Each entry is "dst:bak". Both backup_configs (before install) and
# restore_configs (rollback) iterate this one array, so backup and restore can
# never drift apart when a target is added or changed. It covers all targets:
# the migration unit (I32-B) and the verify script are included alongside
# service, timer and the backup script.
restore_pairs=(
  "$service_dst:$service_dst.bak-$ts"
  "$timer_dst:$timer_dst.bak-$ts"
  "$migrations_unit_dst:$migrations_unit_dst.bak-$ts"
  "$script_dst:$script_dst.bak-$ts"
  "$verify_dst:$verify_dst.bak-$ts"
)

backup_configs() {
  local pair dst bak
  for pair in "${restore_pairs[@]}"; do
    dst="${pair%%:*}"
    bak="${pair##*:}"
    if [ -f "$dst" ]; then
      cp -a "$dst" "$bak"
    fi
  done
}

restore_configs() {
  local pair dst bak
  for pair in "${restore_pairs[@]}"; do
    dst="${pair%%:*}"
    bak="${pair##*:}"
    if [ -f "$bak" ]; then
      mv -f "$bak" "$dst" || true
    else
      # No backup: this run created the file, so remove it on rollback.
      rm -f "$dst" || true
    fi
  done
}

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

# Read-only service-manager state captured BEFORE the first write (I36-C (b)).
# Rollback uses it to restore the exact active/enabled state instead of
# unconditionally stopping a timer that was already enabled before this run.
# Every query is best-effort: an absent unit is a valid pre-install state.
pre_timer_known=0
pre_timer_active=0
pre_timer_enabled=0
pre_timer_next=""
if systemctl cat jarvis-backup.timer >/dev/null 2>&1; then
  pre_timer_known=1
fi
if systemctl is-active --quiet jarvis-backup.timer 2>/dev/null; then
  pre_timer_active=1
fi
if systemctl is-enabled --quiet jarvis-backup.timer 2>/dev/null; then
  pre_timer_enabled=1
fi
# Informational only: pre_timer_next is captured solely for the log line below.
# Rollback does NOT try to restore it. systemd has no API to pin a timer's next
# trigger, so after rollback the next elapse is recalculated and may differ from
# the value recorded here; only active/enabled are re-asserted on rollback.
pre_timer_next="$(systemctl list-timers jarvis-backup.timer --no-pager 2>/dev/null \
  | awk 'NR==2 {print $1, $2, $3}' || true)"
echo "  pre-install timer: known=$pre_timer_known active=$pre_timer_active enabled=$pre_timer_enabled${pre_timer_next:+ next=$pre_timer_next}"

# ERR trap must be disarmed before rollback runs: otherwise a failure inside
# rollback would re-enter it. Explicit `exit 1` paths call fail() instead.
rollback() {
  trap - ERR
  echo "== ROLLBACK ==" >&2
  # Restore the service-manager state observed before this run. A blanket
  # stop+disable breaks the "Idempotent and reversible" contract when the
  # timer was already active/enabled before the installer ran: only a timer
  # this run newly activated is stopped and disabled. A pre-existing state is
  # re-asserted per dimension (active and enabled are independent).
  if [ "$pre_timer_known" -eq 1 ] || [ "$pre_timer_active" -eq 1 ] || [ "$pre_timer_enabled" -eq 1 ]; then
    if [ "$pre_timer_enabled" -eq 1 ]; then
      systemctl enable jarvis-backup.timer || true
    else
      systemctl disable jarvis-backup.timer || true
    fi
    if [ "$pre_timer_active" -eq 1 ]; then
      systemctl start jarvis-backup.timer || true
    else
      systemctl stop jarvis-backup.timer || true
    fi
  else
    # No unit/state before this run: this execution introduced it, so undo the
    # enable --now before restore_configs removes the installed unit file.
    systemctl stop jarvis-backup.timer || true
    systemctl disable jarvis-backup.timer || true
  fi
  # Restore every configuration target from the same registry used for backup
  # (service, timer, migrations unit, backup script and verify script). A
  # missing backup means this run created the file, so it is removed.
  restore_configs
  systemctl daemon-reload || true
}

trap 'rollback' ERR
fail() {
  echo "ERROR: $*" >&2
  rollback
  exit 1
}

# Back up all targets up front from the single registry, then install. Doing
# this before the first write keeps rollback complete even when a later step
# (e.g. post-install verification) fails mid-way.
backup_configs
install -o root -g root -m 0755 "$script_src" "$script_dst"
for pair in "$service_src:$service_dst" "$timer_src:$timer_dst" "$migrations_unit_src:$migrations_unit_dst"; do
  src="${pair%%:*}"; dst="${pair##*:}"
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
