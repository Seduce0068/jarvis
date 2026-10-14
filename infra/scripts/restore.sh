#!/bin/bash
# JARVIS Restore Script
# Restores from OCI Object Storage backups

set -euo pipefail

RCLONE_REMOTE="oci"
RCLONE_BUCKET="jarvis-backups"
HOSTNAME=$(hostname)
BACKUP_DIR="/opt/jarvis/backups"
DATE="${1:-latest}"

log() {
    echo "[$(date -Iseconds)] $*"
}

# Resolve 1Password references in rclone config
resolve_rclone_config() {
    mkdir -p /run/rclone
    if command -v op &> /dev/null; then
        op inject -i /home/jarvis/.config/rclone/rclone.conf -o /run/rclone/rclone.conf
        echo "--config /run/rclone/rclone.conf"
    else
        echo ""
    fi
}

RCLONE_CONFIG_FLAG=$(resolve_rclone_config)

# Find latest backup if not specified
find_latest() {
    local pattern="$1"
    rclone $RCLONE_CONFIG_FLAG ls "$RCLONE_REMOTE:$RCLONE_BUCKET/$HOSTNAME/" | grep "$pattern" | sort -k2 | tail -1 | awk '{print $2}'
}

# Download backup file
download_backup() {
    local remote_file="$1"
    local local_file="$2"
    log "Downloading $remote_file..."
    rclone $RCLONE_CONFIG_FLAG copy "$RCLONE_REMOTE:$RCLONE_BUCKET/$HOSTNAME/$remote_file" "$BACKUP_DIR/"
    mv "$BACKUP_DIR/$remote_file" "$local_file"
}

# Restore PostgreSQL
restore_postgres() {
    local backup_file="${1:-$(find_latest 'pg-.*\.sql\.gz')}"
    if [ -z "$backup_file" ]; then
        log "ERROR: No PostgreSQL backup found"
        return 1
    fi
    
    log "Restoring PostgreSQL from $backup_file..."
    download_backup "$backup_file" "$BACKUP_DIR/$backup_file"
    
    # Stop services that use DB
    sudo systemctl stop jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context opencode
    
    # Drop and recreate database
    sudo -u postgres psql -c "DROP DATABASE IF EXISTS jarvis;"
    sudo -u postgres psql -c "CREATE DATABASE jarvis OWNER jarvis;"
    
    # Restore
    gunzip -c "$BACKUP_DIR/$backup_file" | sudo -u postgres psql jarvis
    
    log "PostgreSQL restored"
}

# Restore Qdrant
restore_qdrant() {
    local backup_file="${1:-$(find_latest 'qdrant-.*\.snapshot')}"
    if [ -z "$backup_file" ]; then
        log "ERROR: No Qdrant backup found"
        return 1
    fi
    
    log "Restoring Qdrant from $backup_file..."
    download_backup "$backup_file" "$BACKUP_DIR/$backup_file"
    
    # Stop Qdrant
    sudo systemctl stop qdrant
    
    # Restore snapshot (Qdrant API)
    curl -X POST "http://localhost:6334/collections/jarvis_memories/snapshots/upload" \
        -F "snapshot=@$BACKUP_DIR/$backup_file"
    
    # Start Qdrant
    sudo systemctl start qdrant
    
    log "Qdrant restored"
}

# Restore configs
restore_configs() {
    local backup_file="${1:-$(find_latest 'configs-.*\.tar\.gz')}"
    if [ -z "$backup_file" ]; then
        log "ERROR: No config backup found"
        return 1
    fi
    
    log "Restoring configs from $backup_file..."
    download_backup "$backup_file" "$BACKUP_DIR/$backup_file"
    
    sudo tar -xzf "$BACKUP_DIR/$backup_file" -C /
    
    log "Configs restored"
}

# Restore git repos
restore_repos() {
    log "Restoring git repositories..."
    
    cd /home/jarvis/repos
    
    # List all repo bundles
    rclone ls "$RCLONE_REMOTE:$RCLONE_BUCKET/$HOSTNAME/" | grep 'repo-.*\.bundle' | awk '{print $2}' | while read -r bundle; do
        repo_name=$(echo "$bundle" | sed 's/repo-\(.*\)-[0-9]*.bundle/\1/')
        log "Restoring repo: $repo_name"
        
        download_backup "$bundle" "$BACKUP_DIR/$bundle"
        
        if [ -d "$repo_name" ]; then
            mv "$repo_name" "$repo_name.bak.$(date +%s)"
        fi
        
        git clone "$BACKUP_DIR/$bundle" "$repo_name"
    done
    
    log "Repositories restored"
}

# Main
main() {
    log "Starting JARVIS restore for $HOSTNAME (backup: $DATE)"
    
    mkdir -p "$BACKUP_DIR"
    
    if [ "$DATE" = "latest" ]; then
        restore_configs
        restore_postgres
        restore_qdrant
        restore_repos
    else
        # Restore specific dated backup
        restore_configs "configs-$DATE.tar.gz"
        restore_postgres "pg-$DATE.sql.gz"
        restore_qdrant "qdrant-$DATE.snapshot"
        # Repos would need specific bundle names
    fi
    
    # Restart all services
    log "Restarting services..."
    sudo systemctl daemon-reload
    sudo systemctl start jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context opencode code-server otelcol
    
    log "Restore completed. Verify services with: sudo systemctl status jarvis-*"
}

main "$@"