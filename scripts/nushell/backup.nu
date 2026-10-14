#!/usr/bin/env nu
# JARVIS Backup Script - Nushell version
# Run with: `nu backup.nu`

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

def backup_postgres [backup_dir: path, date: string] {
    log "Backing up PostgreSQL..."
    let output_file = ($backup_dir | path join $"pg-($date).sql.gz")
    ^pg_dump -h localhost -U jarvis jarvis | gzip > $output_file
    log $"PostgreSQL backup completed: ($output_file)"
}

def backup_qdrant [backup_dir: path, date: string] {
    log "Backing up Qdrant..."
    let client = (http post "http://localhost:6334/collections/jarvis_memories/snapshots")
    (sleep 3sec)
    let response = (http get "http://localhost:6334/collections/jarvis_memories/snapshots" | from json)
    let snapshot = ($response.result | first)
    if $snapshot != null {
        let snapshot_name = $snapshot.name
        let output_file = ($backup_dir | path join $"qdrant-($date).snapshot")
        let download = (http get $"http://localhost:6334/collections/jarvis_memories/snapshots/($snapshot_name)" | save --raw $output_file)
        log $"Qdrant backup completed: ($output_file)"
    } else {
        warn "No Qdrant snapshot found"
    }
}

def backup_configs [backup_dir: path, date: string] {
    log "Backing up configuration files..."
    let output_file = ($backup_dir | path join $"configs-($date).tar.gz")
    ^tar -czf $output_file /etc/jarvis /home/jarvis/.config/opencode /home/jarvis/.config/code-server /home/jarvis/.local/share/code-server
    if ($env.last_exit_code == 0) {
        log $"Config backup completed: ($output_file)"
    } else {
        warn "Config backup partially failed"
    }
}

def backup_repos [backup_dir: path, date: string] {
    log "Backing up git repositories..."
    let repos_dir = "/home/jarvis/repos"
    if not ($repos_dir | path exists) {
        return
    }
    for entry in (ls $repos_dir) {
        let repo_path = ($repos_dir | path join $entry.name)
        if ($repo_path | path join '.git' | path exists) {
            let repo_name = $entry.name
            let output_file = ($backup_dir | path join $"repo-($repo_name)-($date).bundle")
            ^git -C $repo_path bundle create $output_file --all
            if ($env.last_exit_code == 0) {
                log $"Repo backup completed: ($output_file)"
            } else {
                warn $"Failed to backup repo: ($repo_name)"
            }
        }
    }
}

def upload_to_oci [backup_dir: path, remote: string, bucket: string, hostname: string, config_flag: string] {
    log "Uploading backups to OCI Object Storage..."
    let remote_path = $"($remote):($bucket)/($hostname)/"
    run_rclone ["copy", $backup_dir, $remote_path, "--progress", "--transfers", "4"] $config_flag
    log "Upload to OCI completed"
}

def cleanup_local [backup_dir: path] {
    log "Cleaning up old local backups (older than 7 days)..."
    let now = (date now)
    let files = (ls $backup_dir | where type == file)
    for file in $files {
        let modified = ($file.modified | to text | date from-human)
        let age = ($now - $modified).total-days
        if $age > 7 {
            (rm $file.path)
        }
    }
    log "Cleanup completed"
}

def cleanup_remote [remote: string, bucket: string, hostname: string, config_flag: string, dry_run: bool] {
    log "Cleaning up old remote backups (older than 30 days)..."
    let remote_path = $"($remote):($bucket)/($hostname)/"
    mut args = ["delete", $remote_path, "--min-age", "30d"]
    if $dry_run {
        $args = ($args | append "--dry-run")
    }
    run_rclone $args $config_flag
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
    let backup_date = (date now) | format date '%Y%m%d-%H%M%S'
    let backup_dir = ($backup_dir | into string)
    mkdir $backup_dir

    log $"Starting JARVIS backup: ($backup_date) on ($hostname)"

    let config_flag = if not $local_only {
        resolve_rclone_config
    } else {
        ""
    }

    backup_postgres $backup_dir $backup_date
    backup_qdrant $backup_dir $backup_date
    backup_configs $backup_dir $backup_date
    backup_repos $backup_dir $backup_date

    if not $local_only {
        upload_to_oci $backup_dir $remote $bucket $hostname $config_flag
        cleanup_remote $remote $bucket $hostname $config_flag $dry_run
    }

    cleanup_local $backup_dir

    log $"Backup completed successfully: ($backup_date)"
}

$env.JARVIS_BACKUP_FUNCTIONS = {
    main: main
    backup_postgres: backup_postgres
    backup_qdrant: backup_qdrant
    backup_configs: backup_configs
    backup_repos: backup_repos
    upload_to_oci: upload_to_oci
    cleanup_local: cleanup_local
    cleanup_remote: cleanup_remote
}