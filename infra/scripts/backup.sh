#!/bin/bash
# JARVIS Backup Script
# Runs daily via systemd timer, uploads to OCI Object Storage

set -euo pipefail

BACKUP_DIR="/opt/jarvis/backups"
DATE=$(date +%Y%m%d-%H%M%S)
HOSTNAME=$(hostname)
RCLONE_REMOTE="oci"
RCLONE_BUCKET="jarvis-backups"

mkdir -p "$BACKUP_DIR"

log() {
    echo "[$(date -Iseconds)] $*"
}

# Backup PostgreSQL
backup_postgres() {
    log "Backing up PostgreSQL..."
    if pg_dump -h localhost -U jarvis jarvis | gzip > "$BACKUP_DIR/pg-$DATE.sql.gz"; then
        log "PostgreSQL backup completed: pg-$DATE.sql.gz"
    else
        log "ERROR: PostgreSQL backup failed"
        return 1
    fi
}

# Backup Qdrant
backup_qdrant() {
    log "Backing up Qdrant..."
    # Create snapshot
    if curl -s -X POST "http://localhost:6334/collections/jarvis_memories/snapshots" > /dev/null; then
        sleep 3
        SNAPSHOT_NAME=$(curl -s "http://localhost:6334/collections/jarvis_memories/snapshots" | jq -r '.result[0].name // empty')
        if [ -n "$SNAPSHOT_NAME" ]; then
            if curl -s "http://localhost:6334/collections/jarvis_memories/snapshots/$SNAPSHOT_NAME" -o "$BACKUP_DIR/qdrant-$DATE.snapshot"; then
                log "Qdrant backup completed: qdrant-$DATE.snapshot"
            else
                log "WARNING: Failed to download Qdrant snapshot"
            fi
        else
            log "WARNING: No Qdrant snapshot found"
        fi
    else
        log "WARNING: Failed to create Qdrant snapshot"
    fi
}

# Backup configuration files
backup_configs() {
    log "Backing up configuration files..."
    if tar -czf "$BACKUP_DIR/configs-$DATE.tar.gz" \
        /etc/jarvis \
        /home/jarvis/.config/opencode \
        /home/jarvis/.config/code-server \
        /home/jarvis/.local/share/code-server \
        2>/dev/null; then
        log "Config backup completed: configs-$DATE.tar.gz"
    else
        log "WARNING: Config backup partially failed"
    fi
}

# Backup git repositories as bundles
backup_repos() {
    log "Backing up git repositories..."
    cd /home/jarvis/repos
    for repo in */; do
        if [ -d "$repo/.git" ]; then
            repo_name="${repo%/}"
            if git -C "$repo" bundle create "$BACKUP_DIR/repo-$repo_name-$DATE.bundle" --all 2>/dev/null; then
                log "Repo backup completed: repo-$repo_name-$DATE.bundle"
            else
                log "WARNING: Failed to backup repo: $repo_name"
            fi
        fi
    done
}

# Upload to OCI Object Storage
upload_to_oci() {
    log "Uploading backups to OCI Object Storage..."
    if command -v rclone &> /dev/null; then
        if rclone copy "$BACKUP_DIR" "$RCLONE_REMOTE:$RCLONE_BUCKET/$HOSTNAME/" --progress --transfers 4; then
            log "Upload to OCI completed"
        else
            log "ERROR: Upload to OCI failed"
            return 1
        fi
    else
        log "WARNING: rclone not installed, skipping remote upload"
    fi
}

# Cleanup old local backups
cleanup_local() {
    log "Cleaning up old local backups (older than 7 days)..."
    find "$BACKUP_DIR" -type f -mtime +7 -delete
    log "Cleanup completed"
}

# Cleanup old remote backups (keep 30 days)
cleanup_remote() {
    log "Cleaning up old remote backups (older than 30 days)..."
    if command -v rclone &> /dev/null; then
        rclone delete "$RCLONE_REMOTE:$RCLONE_BUCKET/$HOSTNAME/" --min-age 30d --dry-run 2>/dev/null | grep -c "Would delete" || true
        # Uncomment to actually delete:
        # rclone delete "$RCLONE_REMOTE:$RCLONE_BUCKET/$HOSTNAME/" --min-age 30d
    fi
}

# Main
main() {
    log "Starting JARVIS backup: $DATE"

    backup_postgres
    backup_qdrant
    backup_configs
    backup_repos
    upload_to_oci
    cleanup_local
    cleanup_remote

    log "Backup completed successfully: $DATE"
}

main "$@"