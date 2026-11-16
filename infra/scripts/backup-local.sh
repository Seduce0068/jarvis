#!/usr/bin/env bash
# Local-only JARVIS backup (ADR-0004; BK-1/BK-2 redesign).
# Runs as the postgres OS user so `pg_dump -d jarvis` works via peer auth.
# No OCI, rclone, 1Password, or secret values are read. Default is dry-run.
set -euo pipefail

backup_dir="/var/lib/postgresql/jarvis-backups"
apply=0
retain_days=""
skip_db=0
skip_qdrant=0

usage() {
  cat <<'USAGE'
Usage: backup-local.sh [--backup-dir DIR] [--apply] [--retain-days N]
                       [--skip-db] [--skip-qdrant]
  Default is a non-destructive dry run. --apply performs the backup.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --backup-dir) backup_dir="$2"; shift 2 ;;
    --apply) apply=1; shift ;;
    --retain-days) retain_days="$2"; shift 2 ;;
    --skip-db) skip_db=1; shift ;;
    --skip-qdrant) skip_qdrant=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "ERROR: unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

stamp="$(date +%Y%m%d-%H%M%S)"
host="$(hostname)"
set_dir="${backup_dir}/${host}-${stamp}"

echo "JARVIS local backup: dest=${set_dir} apply=${apply}"

if [ "$apply" -ne 1 ]; then
  echo "DRY RUN: no files will be written (pass --apply to execute)"
  [ "$skip_db" -eq 0 ] && echo "DRY RUN: would dump PostgreSQL 'jarvis'"
  [ "$skip_qdrant" -eq 0 ] && echo "DRY RUN: would snapshot Qdrant 'jarvis_memories'"
  echo "DRY RUN: would copy non-secret config allowlist"
  exit 0
fi

mkdir -p "$set_dir"
chmod 0700 "$set_dir"

manifest="$set_dir/manifest.txt"
# Header line carries the creation time for precise, opt-in retention.
printf '# created_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$manifest"

record() { # name path
  local name="$1" path="$2" bytes sha
  bytes="$(wc -c < "$path" | tr -d ' ')"
  sha="$(sha256sum "$path" | cut -d' ' -f1)"
  printf '%s %s %s\n' "$name" "$bytes" "$sha" >> "$manifest"
}

if [ "$skip_db" -eq 0 ]; then
  pg_dump -d jarvis | gzip > "$set_dir/postgres-jarvis.sql.gz"
  record postgres-jarvis.sql.gz "$set_dir/postgres-jarvis.sql.gz"
  echo "PostgreSQL dump OK"
fi

if [ "$skip_qdrant" -eq 0 ]; then
  curl -fsS -X POST "http://127.0.0.1:6333/collections/jarvis_memories/snapshots" >/dev/null
  sleep 2
  snapshot_name="$(curl -fsS "http://127.0.0.1:6333/collections/jarvis_memories/snapshots" \
    | jq -r '.result[0].name // empty')"
  if [ -z "$snapshot_name" ]; then
    echo "ERROR: no Qdrant snapshot returned" >&2
    exit 1
  fi
  curl -fsS "http://127.0.0.1:6333/collections/jarvis_memories/snapshots/${snapshot_name}" \
    > "$set_dir/qdrant-snapshot.bin"
  record qdrant-snapshot.bin "$set_dir/qdrant-snapshot.bin"
  echo "Qdrant snapshot OK"
fi

# Non-secret config allowlist. Never include .env / bws.env / token files.
for source in /etc/jarvis/mcp_allowlist.toml /etc/jarvis/pii_patterns.toml \
              /etc/jarvis/otelcol-config.yaml /etc/jarvis/policy; do
  if [ -e "$source" ]; then
    name="config-$(basename "$source")"
    if [ -d "$source" ]; then
      # Hash a tar archive rather than the directory itself.
      archive="$set_dir/$name.tar"
      tar -cf "$archive" -C "$(dirname "$source")" "$(basename "$source")"
      record "$name.tar" "$archive"
    else
      cp "$source" "$set_dir/$name"
      record "$name" "$set_dir/$name"
    fi
  fi
done
echo "Config copy OK"

chmod 0700 "$set_dir"
# Completion marker: only written after every step succeeded. The verifier
# uses this to reject partially-written sets from a failed run.
printf 'complete %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$set_dir/COMPLETE"
echo "Backup complete: $set_dir"

if [ -n "$retain_days" ]; then
  echo "Retention: ${retain_days} days (opt-in, destructive)"
  cutoff_ts="$(( $(date +%s) - retain_days * 86400 ))"
  while IFS= read -r old; do
    # Prefer the manifest timestamp; fall back to the <host>-<YYYYMMDD-HHMMSS>
    # directory name used by this script and the earlier Rust implementation.
    created="$(sed -n 's/^# created_at=//p' "$old/manifest.txt" 2>/dev/null | head -1)"
    if [ -n "$created" ]; then
      epoch="$(date -u -d "$created" +%s 2>/dev/null || echo 0)"
    else
      base="$(basename "$old")"
      stamp="${base##*-}"
      epoch="$(date -d "${stamp:0:8} ${stamp:9:2}:${stamp:11:2}:${stamp:13:2}" +%s 2>/dev/null || echo 0)"
    fi
    if [ "$epoch" -gt 0 ] && [ "$epoch" -lt "$cutoff_ts" ]; then
      echo "PRUNE: $old (created epoch=$epoch)"
      rm -rf "$old"
    fi
  done < <(find "$backup_dir" -mindepth 1 -maxdepth 1 -type d | sort)
fi
