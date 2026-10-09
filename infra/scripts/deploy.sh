#!/bin/bash
# JARVIS Deploy Script
# Builds and deploys Rust services to the JARVIS host

set -euo pipefail

HOST="${1:-jarvis-host}"
REMOTE_USER="jarvis"
REMOTE_PATH="/opt/jarvis"
LOCAL_RUST_DIR="$(dirname "$0")/../rust"

log() {
    echo "[$(date -Iseconds)] $*"
}

# Build locally (cross-compile for Arm64)
build() {
    log "Building Rust services for aarch64..."
    cd "$LOCAL_RUST_DIR"
    
    # Ensure target is installed
    rustup target add aarch64-unknown-linux-gnu
    
    # Build all services
    cargo build --release --workspace --target aarch64-unknown-linux-gnu
    
    log "Build completed"
}

# Deploy binaries via SSH
deploy() {
    log "Deploying to $HOST..."
    
    # Create remote directories
    ssh "$REMOTE_USER@$HOST" "mkdir -p $REMOTE_PATH/bin"
    
    # Copy binaries
    for service in jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context; do
        log "Deploying $service..."
        scp "target/aarch64-unknown-linux-gnu/release/$service" "$REMOTE_USER@$HOST:$REMOTE_PATH/bin/"
    done
    
    # Copy config files
    log "Deploying config files..."
    scp -r "$(dirname "$0")/config/"* "$REMOTE_USER@$HOST:/etc/jarvis/"
    
    # Restart services
    log "Restarting services..."
    ssh "$REMOTE_USER@$HOST" "sudo systemctl daemon-reload && sudo systemctl restart jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context"
    
    # Wait for health checks
    log "Waiting for services to be healthy..."
    sleep 5
    
    for port in 8081 8082 8083 8084; do
        if ssh "$REMOTE_USER@$HOST" "curl -sf http://127.0.0.1:$port/health" > /dev/null; then
            log "Service on port $port is healthy"
        else
            log "WARNING: Service on port $port not responding"
        fi
    done
    
    log "Deployment completed"
}

# Main
main() {
    case "${2:-deploy}" in
        build)
            build
            ;;
        deploy)
            build
            deploy
            ;;
        restart)
            log "Restarting services on $HOST..."
            ssh "$REMOTE_USER@$HOST" "sudo systemctl restart jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context"
            ;;
        logs)
            ssh "$REMOTE_USER@$HOST" "sudo journalctl -u jarvis-policy -u jarvis-mcp-gw -u jarvis-event -u jarvis-context -f"
            ;;
        status)
            ssh "$REMOTE_USER@$HOST" "sudo systemctl status jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context"
            ;;
        *)
            echo "Usage: $0 <host> [build|deploy|restart|logs|status]"
            exit 1
            ;;
    esac
}

main "$@"