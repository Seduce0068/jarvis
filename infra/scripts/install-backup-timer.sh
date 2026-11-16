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
service_dst="/etc/systemd/system/jarvis-backup.service"
timer_dst="/etc/systemd/system/jarvis-backup.timer"
script_dst="/opt/jarvis/scripts/backup-local.sh"
backup_dir="/var/lib/postgresql/jarvis-backups"
ts="$(date +%Y%m%d-%H%M%S)"

echo "== backup timer install (apply=$apply) =="
if [ "$apply" -ne 1 ]; then
  echo "  DRY: install $script_src -> $script_dst (root:root 0755)"
  echo "  DRY: install $service_src -> $service_dst (root:root 0644)"
  echo "  DRY: install $timer_src   -> $timer_dst   (root:root 0644)"
  echo "  DRY: mkdir -p $backup_dir (owner postgres:postgres, mode 0700)"
  echo "  DRY: systemctl daemon-reload; systemctl enable --now jarvis-backup.timer"
  echo "  DRY: verify timer active + next elapse"
  exit 0
fi

[ "$(id -u)" -eq 0 ] || { echo "ERROR: --apply requires root" >&2; exit 1; }

rollback() {
  echo "== ROLLBACK ==" >&2
  if [ -f "$service_dst.bak-$ts" ]; then mv -f "$service_dst.bak-$ts" "$service_dst"; else rm -f "$service_dst"; fi
  if [ -f "$timer_dst.bak-$ts" ]; then mv -f "$timer_dst.bak-$ts" "$timer_dst"; else rm -f "$timer_dst"; fi
  if [ -f "$script_dst.bak-$ts" ]; then mv -f "$script_dst.bak-$ts" "$script_dst"; else rm -f "$script_dst"; fi
  systemctl daemon-reload || true
}

trap 'rollback' ERR

install -o root -g root -m 0755 "$script_src" "$script_dst"
for pair in "$service_src:$service_dst" "$timer_src:$timer_dst"; do
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
systemctl is-active --quiet jarvis-backup.timer || { echo "ERROR: timer not active" >&2; exit 1; }
trap - ERR
systemctl list-timers jarvis-backup.timer --no-pager | head -3
echo "== DONE: backup timer installed and enabled =="
