#!/usr/bin/env nu
# JARVIS Deploy Script - Nushell version
# Run with: `nu deploy.nu <host> [build|deploy|restart|logs|status]`

def log [msg: string] {
    let now = (date now) | format date '%Y-%m-%dT%H:%M:%S%.3f%:z'
    print $"($now) $msg"
}

def run_ssh [host: string, command: string] {
    ^ssh $host $command | ignore
    if ($env.last_exit_code != 0) {
        log $"SSH command failed: ($command)"
    }
}

def run_scp [local: path, host: string, remote: string] {
    let remote_target = $"($host):($remote)"
    ^scp -r $local $remote_target | ignore
    if ($env.last_exit_code != 0) {
        log $"SCP failed: ($local) -> ($remote_target)"
    }
}

def build [rust_dir: path] {
    log "Building Rust services for aarch64..."

    # Ensure target is installed
    ^rustup target add aarch64-unknown-linux-gnu | ignore

    # Build all services
    ^cargo build --release --workspace --target aarch64-unknown-linux-gnu --manifest-path ($rust_dir | path join "Cargo.toml") | ignore
    if ($env.last_exit_code != 0) {
        log "Build failed"
    }

    log "Build completed"
}

def deploy [host: string, user: string, remote_path: path, rust_dir: path] {
    let remote_base = $"($user)@($host)"
    log $"Deploying to ($host)..."

    # Create remote directories
    run_ssh $remote_base $"mkdir -p ($remote_path)/bin"

    # Copy binaries
    let target_dir = ($rust_dir | path join "target/aarch64-unknown-linux-gnu/release")
    for service in ["jarvis-policy", "jarvis-mcp-gw", "jarvis-event", "jarvis-context"] {
        log $"Deploying ($service)..."
        let binary_path = ($target_dir | path join $service)
        if not ($binary_path | path exists) {
            warn $"Binary not found: ($binary_path)"
            continue
        }
        run_scp $binary_path $remote_base $"($remote_path)/bin/"
    }

    # Copy config files
    log "Deploying config files..."
    let config_dir = ($rust_dir | path join "../infra/config")
    run_scp $config_dir $remote_base "/etc/jarvis/"

    # Restart services
    log "Restarting services..."
    run_ssh $remote_base "sudo systemctl daemon-reload && sudo systemctl restart jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context"

    # Wait for health checks
    log "Waiting for services to be healthy..."
    (sleep 5sec)

    for port in [8081, 8082, 8083, 8084] {
        let url = $"http://127.0.0.1:($port)/health"
        let output = ^ssh $remote_base "curl -sf $url" | ignore
        if ($env.last_exit_code == 0) {
            log $"Service on port ($port) is healthy"
        } else {
            warn $"Service on port ($port) not responding"
        }
    }

    log "Deployment completed"
}

def restart [host: string, user: string] {
    let remote_base = $"($user)@($host)"
    log $"Restarting services on ($host)..."
    run_ssh $remote_base "sudo systemctl restart jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context"
}

def logs [host: string, user: string] {
    let remote_base = $"($user)@($host)"
    ^ssh $remote_base "sudo journalctl -u jarvis-policy -u jarvis-mcp-gw -u jarvis-event -u jarvis-context -f" | ignore
}

def status [host: string, user: string] {
    let remote_base = $"($user)@($host)"
    ^ssh $remote_base "sudo systemctl status jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context" | ignore
}

def main [
    rust_dir: path,
    host: string = "jarvis-host",
    user: string = "jarvis",
    remote_path: path = "/opt/jarvis",
    command: string = "deploy"
] {
    match $command {
        "build" => { build $rust_dir }
        "deploy" => { build $rust_dir; deploy $host $user $remote_path $rust_dir }
        "restart" => { restart $host $user }
        "logs" => { logs $host $user }
        "status" => { status $host $user }
        _ => { log $"Unknown command: ($command). Use: build, deploy, restart, logs, status" }
    }
}

$env.JARVIS_DEPLOY_FUNCTIONS = {
    main: main
    build: build
    deploy: deploy
    restart: restart
    logs: logs
    status: status
}