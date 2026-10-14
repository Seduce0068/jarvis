#!/usr/bin/env nu
# JARVIS Restore Script - Nushell version
# Run with: `nu restore.nu <date|latest>`

def log [msg: string] {
    let now = (date now) | format date '%Y-%m-%dT%H:%M:%S%.3f%:z'
    print $"($now) $msg"
}

def run_rclone [args: list<string>, config_flag: string] {
    mut cmd = ["rclone"]
    if ($config_flag | str length > 0) {
        $cmd = ($cmd | append ($config_flag | split words))
    }
    $cmd = ($cmd | append $args)
    ^run $cmd | ignore
}

def resolve_rclone_config [] {
    let config_path = ($env.HOME | path join '.config/rclone/rclone.conf')
    if not ($config_path | path exists) {
        return ""
    }
    let output_path = "/run/rclone/rclone.conf"
    (mkdir ($output_path | path dirname))
    ^op inject -i $config_path -o $output_path | ignore
    if ($env.last_exit_code == 0) {
        return $"--config $output_path"
    }
    return ""
}

def find_latest_backup [remote: string, bucket: string, hostname: string, config_flag: string] {
    let remote_path = $"($remote):($bucket)/($hostname)/"
    let output = run_rclone ["ls", $remote_path] $config_flag
    ($output | lines
        | split column " "
        | where $it.1 =~ "pg-.*\\.sql\\.gz"
        | get 1
        | max
        | str replace "pg-" ""
        | str replace ".sql.gz" ""
    )
}

def download_backup [remote_file: string, local_file: path, remote_path: string, config_flag: string] {
    log $"Downloading ($remote_file)..."
    run_rclone ["copy", $"($remote_path)($remote_file)", ($local_file | into string)] $config_flag
}

def restore_postgres [date: string, hostname: string, remote: string, bucket: string, backup_dir: path, config_flag: string, dry_run: bool] {
    let backup_file = $"pg-($date).sql.gz"
    let remote_path = $"($remote):($bucket)/($hostname)/"
    let local_file = ($backup_dir | path join $backup_file)

    log $"Restoring PostgreSQL from ($backup_file)..."
    download_backup $backup_file $local_file $remote_path $config_flag

    if $dry_run {
        log "DRY RUN: Would restore PostgreSQL"
        return
    }

    # Stop services that use DB
    for service in ["jarvis-policy", "jarvis-mcp-gw", "jarvis-event", "jarvis-context", "opencode"] {
        ^sudo systemctl stop $service | ignore
    }

    # Drop and recreate database
    ^sudo -u postgres psql -c "DROP DATABASE IF EXISTS jarvis;" | ignore
    ^sudo -u postgres psql -c "CREATE DATABASE jarvis OWNER jarvis;" | ignore

    # Restore
    ^gunzip -c $local_file | sudo -u postgres psql jarvis | ignore

    log "PostgreSQL restored"
}

def restore_qdrant [date: string, hostname: string, remote: string, bucket: string, backup_dir: path, config_flag: string, dry_run: bool] {
    let backup_file = $"qdrant-($date).snapshot"
    let remote_path = $"($remote):($bucket)/($hostname)/"
    let local_file = ($backup_dir | path join $backup_file)

    log $"Restoring Qdrant from ($backup_file)..."
    download_backup $backup_file $local_file $remote_path $config_flag

    if $dry_run {
        log "DRY RUN: Would restore Qdrant"
        return
    }

    # Stop Qdrant
    ^sudo systemctl stop qdrant | ignore

    # Restore snapshot via Qdrant API
    let form = {snapshot: ($local_file | into string)}
    ^http post "http://localhost:6334/collections/jarvis_memories/snapshots/upload" --multipart $form | ignore

    # Start Qdrant
    ^sudo systemctl start qdrant | ignore

    log "Qdrant restored"
}

def restore_configs [date: string, hostname: string, remote: string, bucket: string, backup_dir: path, config_flag: string, dry_run: bool] {
    let backup_file = $"configs-($date).tar.gz"
    let remote_path = $"($remote):($bucket)/($hostname)/"
    let local_file = ($backup_dir | path join $backup_file)

    log $"Restoring configs from ($backup_file)..."
    download_backup $backup_file $local_file $remote_path $config_flag

    if $dry_run {
        log "DRY RUN: Would restore configs"
        return
    }

    ^sudo tar -xzf $local_file -C / | ignore
    log "Configs restored"
}

def restore_repos [hostname: string, remote: string, bucket: string, backup_dir: path, config_flag: string, dry_run: bool, date: string] {
    log "Restoring git repositories..."

    if $dry_run {
        log "DRY RUN: Would restore repos"
        return
    }

    let remote_path = $"($remote):($bucket)/($hostname)/"
    let output = run_rclone ["ls", $remote_path] $config_flag

    let repos_dir = "/home/jarvis/repos"
    (mkdir $repos_dir)

    for line in ($output | lines) {
        let bundle = ($line | split column " " | get 1)
        if ($bundle =~ "repo-.*\\.bundle") {
            let repo_name = ($bundle | str replace "repo-" "" | str replace "-($date).bundle" "")
            log $"Restoring repo: ($repo_name)"
            let local_bundle = ($backup_dir | path join $bundle)
            download_backup $bundle $local_bundle $remote_path $config_flag

            let repo_path = ($repos_dir | path join $repo_name)
            if ($repo_path | path exists) {
                let backup_name = $"($repo_name).bak.((date now).timestamp)"
                ^mv $repo_path ($repos_dir | path join $backup_name) | ignore
            }

            ^git clone $local_bundle $repo_name | ignore
        }
    }

    log "Repositories restored"
}

def restart_services [] {
    log "Restarting services..."
    ^sudo systemctl daemon-reload | ignore
    for service in ["jarvis-policy", "jarvis-mcp-gw", "jarvis-event", "jarvis-context", "opencode", "code-server", "otelcol"] {
        ^sudo systemctl start $service | ignore
    }
}

def main [
    date: string = "latest",
    remote: string = "oci",
    bucket: string = "jarvis-backups",
    backup_dir: path = "/opt/jarvis/backups",
    dry_run: bool = false,
    local_only: bool = false
] {
    let hostname = (hostname)
    let backup_dir = ($backup_dir | into string)
    (mkdir $backup_dir)

    log $"Starting JARVIS restore for ($hostname) (backup: ($date))"

    let config_flag = if not $local_only {
        resolve_rclone_config
    } else {
        ""
    }

    let restore_date = if $date == "latest" {
        find_latest_backup $remote $bucket $hostname $config_flag
    } else {
        $date
    }

    restore_configs $restore_date $hostname $remote $bucket $backup_dir $config_flag $dry_run
    restore_postgres $restore_date $hostname $remote $bucket $backup_dir $config_flag $dry_run
    restore_qdrant $restore_date $hostname $remote $bucket $backup_dir $config_flag $dry_run
    restore_repos $hostname $remote $bucket $backup_dir $config_flag $dry_run $restore_date

    if not $dry_run {
        restart_services
    }

    log "Restore completed. Verify services with: sudo systemctl status jarvis-*"
}

$env.JARVIS_RESTORE_FUNCTIONS = {
    main: main
    restore_postgres: restore_postgres
    restore_qdrant: restore_qdrant
    restore_configs: restore_configs
    restore_repos: restore_repos
    restart_services: restart_services
}