/// ```cargo
/// [dependencies]
/// anyhow = "1.0"
/// chrono = { version = "0.4", features = ["serde"] }
/// clap = { version = "4.5", features = ["derive"] }
/// reqwest = { version = "0.12", features = ["json", "blocking", "rustls-tls", "multipart"] }
/// serde = { version = "1.0", features = ["derive"] }
/// serde_json = "1.0"
/// tracing = "0.1"
/// tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
/// hostname = "0.3"
/// dirs = "5.0"
/// ```

/// JARVIS Restore Script - Rust version
/// Run with: `rust-script restore.rs <date|latest>` or compile with `cargo build --release --bin restore`
use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;
use reqwest::blocking::Client;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "jarvis-restore",
    about = "JARVIS restore from OCI Object Storage"
)]
struct Args {
    /// Backup date (YYYYMMDD-HHMMSS) or "latest"
    #[arg(default_value = "latest")]
    date: String,

    /// Rclone remote name
    #[arg(long, default_value = "oci")]
    remote: String,

    /// Rclone bucket name
    #[arg(long, default_value = "jarvis-backups")]
    bucket: String,

    /// Backup directory
    #[arg(long, default_value = "/opt/jarvis/backups")]
    backup_dir: String,

    /// Dry run (no actual changes)
    #[arg(long)]
    dry_run: bool,

    /// Confirm destructive restoration of databases, configs, and repositories
    #[arg(long)]
    confirm: bool,

    /// Skip remote operations
    #[arg(long)]
    local_only: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    if !args.dry_run && !args.confirm {
        anyhow::bail!("restore is destructive; pass --confirm, or use --dry-run first");
    }

    let hostname = hostname::get()?.to_string_lossy().to_string();
    let backup_dir = Path::new(&args.backup_dir);

    info!(
        "Starting JARVIS restore for {} (backup: {})",
        hostname, args.date
    );

    // Resolve 1Password references in rclone config
    let rclone_config_flag = if !args.local_only {
        resolve_rclone_config()?
    } else {
        String::new()
    };

    let date = if args.date == "latest" {
        find_latest_backup(&args, &hostname, &rclone_config_flag)?
    } else {
        if !valid_backup_date(&args.date) {
            anyhow::bail!("invalid backup date; expected YYYYMMDD-HHMMSS or latest");
        }
        args.date.clone()
    };
    if !valid_backup_date(&date) {
        anyhow::bail!("selected backup has an invalid date: {date}");
    }

    if args.dry_run {
        info!(
            "DRY RUN: would restore backup set {} for {}",
            date, hostname
        );
        return Ok(());
    }

    fs::create_dir_all(backup_dir)?;

    restore_configs(&args, &hostname, &date, &rclone_config_flag)?;
    restore_postgres(&args, &hostname, &date, &rclone_config_flag)?;
    restore_qdrant(&args, &hostname, &date, &rclone_config_flag)?;
    restore_repos(&args, &hostname, &date, &rclone_config_flag)?;

    if !args.dry_run {
        restart_services()?;
    }

    info!("Restore completed. Verify services with: sudo systemctl status jarvis-*");
    Ok(())
}

fn valid_backup_date(date: &str) -> bool {
    date.len() == 15
        && date.as_bytes()[8] == b'-'
        && date
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 8 || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::valid_backup_date;

    #[test]
    fn accepts_timestamp_format() {
        assert!(valid_backup_date("20261009-120000"));
    }

    #[test]
    fn rejects_invalid_length_and_characters() {
        assert!(!valid_backup_date("latest"));
        assert!(!valid_backup_date("20261009_120000"));
        assert!(!valid_backup_date("2026100x-120000"));
    }
}

fn resolve_rclone_config() -> Result<String> {
    let config_path = dirs::home_dir()
        .context("No home directory")?
        .join(".config/rclone/rclone.conf");

    if !config_path.exists() {
        return Ok(String::new());
    }

    let output_path = Path::new("/run/rclone/rclone.conf");
    fs::create_dir_all(output_path.parent().unwrap())?;

    let status = Command::new("op")
        .args([
            "inject",
            "-i",
            config_path.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
        ])
        .status()
        .context("Failed to run op inject")?;

    if !status.success() {
        anyhow::bail!("op inject returned a non-zero exit status");
    }

    Ok(format!("--config {}", output_path.display()))
}

fn run_rclone(args: &[&str], config_flag: &str) -> Result<String> {
    let mut cmd = Command::new("rclone");
    if !config_flag.is_empty() {
        cmd.args(config_flag.split_whitespace());
    }
    let output = cmd
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("Failed to run rclone")?;
    if !output.status.success() {
        anyhow::bail!("rclone exited with status {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn find_latest_backup(args: &Args, hostname: &str, config_flag: &str) -> Result<String> {
    let remote_path = format!("{}:{}/{}/", args.remote, args.bucket, hostname);
    let output = run_rclone(&["ls", &remote_path], config_flag)?;

    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                Some(parts[1].to_string())
            } else {
                None
            }
        })
        .filter(|f| f.starts_with("pg-") && f.ends_with(".sql.gz"))
        .max()
        .and_then(|f| {
            f.strip_prefix("pg-")
                .and_then(|s| s.strip_suffix(".sql.gz"))
                .map(|s| s.to_string())
        })
        .context("No latest backup found")
}

fn download_backup(
    remote_file: &str,
    local_file: &Path,
    remote_path: &str,
    config_flag: &str,
) -> Result<()> {
    info!("Downloading {}...", remote_file);
    run_rclone(
        &[
            "copyto",
            &format!("{}{}", remote_path, remote_file),
            local_file.to_str().unwrap(),
        ],
        config_flag,
    )?;
    Ok(())
}

fn restore_postgres(args: &Args, hostname: &str, date: &str, config_flag: &str) -> Result<()> {
    let backup_file = format!("pg-{}.sql.gz", date);
    let remote_path = format!("{}:{}/{}/", args.remote, args.bucket, hostname);
    let local_file = Path::new(&args.backup_dir).join(&backup_file);

    info!("Restoring PostgreSQL from {}...", backup_file);
    download_backup(&backup_file, &local_file, &remote_path, config_flag)?;

    if args.dry_run {
        info!("DRY RUN: Would restore PostgreSQL");
        return Ok(());
    }

    // Stop services that use DB
    for service in [
        "jarvis-policy",
        "jarvis-mcp-gw",
        "jarvis-event",
        "jarvis-context",
        "opencode",
    ] {
        let _ = Command::new("sudo")
            .args(["systemctl", "stop", service])
            .status();
    }

    // Drop and recreate database
    Command::new("sudo")
        .args([
            "-u",
            "postgres",
            "psql",
            "-c",
            "DROP DATABASE IF EXISTS jarvis;",
        ])
        .status()
        .context("Failed to drop database")?;
    Command::new("sudo")
        .args([
            "-u",
            "postgres",
            "psql",
            "-c",
            "CREATE DATABASE jarvis OWNER jarvis;",
        ])
        .status()
        .context("Failed to create database")?;

    // Restore
    let status = Command::new("gunzip")
        .arg("-c")
        .arg(&local_file)
        .stdout(Stdio::piped())
        .spawn()?
        .stdout
        .context("No stdout from gunzip")?;

    Command::new("sudo")
        .args(["-u", "postgres", "psql", "jarvis"])
        .stdin(Stdio::from(status))
        .status()
        .context("Failed to restore PostgreSQL")?;

    info!("PostgreSQL restored");
    Ok(())
}

fn restore_qdrant(args: &Args, hostname: &str, date: &str, config_flag: &str) -> Result<()> {
    let backup_file = format!("qdrant-{}.snapshot", date);
    let remote_path = format!("{}:{}/{}/", args.remote, args.bucket, hostname);
    let local_file = Path::new(&args.backup_dir).join(&backup_file);

    info!("Restoring Qdrant from {}...", backup_file);
    download_backup(&backup_file, &local_file, &remote_path, config_flag)?;

    if args.dry_run {
        info!("DRY RUN: Would restore Qdrant");
        return Ok(());
    }

    // Stop Qdrant
    Command::new("sudo")
        .args(["systemctl", "stop", "qdrant"])
        .status()
        .context("Failed to stop Qdrant")?;

    // Restore snapshot via Qdrant API
    let client = Client::new();
    let form = reqwest::blocking::multipart::Form::new().file("snapshot", &local_file)?;

    client
        .post("http://localhost:6333/collections/jarvis_memories/snapshots/upload")
        .multipart(form)
        .send()?;

    // Start Qdrant
    Command::new("sudo")
        .args(["systemctl", "start", "qdrant"])
        .status()
        .context("Failed to start Qdrant")?;

    info!("Qdrant restored");
    Ok(())
}

fn restore_configs(args: &Args, hostname: &str, date: &str, config_flag: &str) -> Result<()> {
    let backup_file = format!("configs-{}.tar.gz", date);
    let remote_path = format!("{}:{}/{}/", args.remote, args.bucket, hostname);
    let local_file = Path::new(&args.backup_dir).join(&backup_file);

    info!("Restoring configs from {}...", backup_file);
    download_backup(&backup_file, &local_file, &remote_path, config_flag)?;

    if args.dry_run {
        info!("DRY RUN: Would restore configs");
        return Ok(());
    }

    Command::new("sudo")
        .args(["tar", "-xzf", local_file.to_str().unwrap(), "-C", "/"])
        .status()
        .context("Failed to restore configs")?;

    info!("Configs restored");
    Ok(())
}

fn restore_repos(args: &Args, hostname: &str, date: &str, config_flag: &str) -> Result<()> {
    info!("Restoring git repositories...");

    if args.dry_run {
        info!("DRY RUN: Would restore repos");
        return Ok(());
    }

    let remote_path = format!("{}:{}/{}/", args.remote, args.bucket, hostname);
    let output = run_rclone(&["ls", &remote_path], config_flag)?;

    let repos_dir = Path::new("/home/jarvis/repos");
    fs::create_dir_all(repos_dir)?;

    for line in output.lines() {
        if let Some(bundle) = line.split_whitespace().nth(1) {
            if bundle.starts_with("repo-") && bundle.ends_with(&format!("-{date}.bundle")) {
                let repo_name = bundle
                    .strip_prefix("repo-")
                    .and_then(|s| s.strip_suffix(&format!("-{date}.bundle")))
                    .context("Invalid repository bundle name")?;

                info!("Restoring repo: {}", repo_name);
                let local_bundle = Path::new(&args.backup_dir).join(bundle);
                download_backup(bundle, &local_bundle, &remote_path, config_flag)?;

                let repo_path = repos_dir.join(repo_name);
                if repo_path.exists() {
                    let backup_name = format!("{}.bak.{}", repo_name, Local::now().timestamp());
                    fs::rename(&repo_path, repos_dir.join(backup_name))?;
                }

                Command::new("git")
                    .args(["clone", local_bundle.to_str().unwrap(), repo_name])
                    .current_dir(repos_dir)
                    .status()
                    .context("Failed to clone repo")?;
            }
        }
    }

    info!("Repositories restored");
    Ok(())
}

fn restart_services() -> Result<()> {
    info!("Restarting services...");
    Command::new("sudo")
        .args(["systemctl", "daemon-reload"])
        .status()?;

    for service in [
        "jarvis-policy",
        "jarvis-mcp-gw",
        "jarvis-event",
        "jarvis-context",
        "opencode",
        "code-server",
        "otelcol",
    ] {
        Command::new("sudo")
            .args(["systemctl", "start", service])
            .status()?;
    }

    Ok(())
}
