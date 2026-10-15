/// ```cargo
/// [dependencies]
/// anyhow = "1.0"
/// chrono = { version = "0.4", features = ["serde"] }
/// clap = { version = "4.5", features = ["derive"] }
/// reqwest = { version = "0.12", features = ["json", "blocking", "rustls-tls"] }
/// serde = { version = "1.0", features = ["derive"] }
/// serde_json = "1.0"
/// tokio = { version = "1.40", features = ["full"] }
/// tracing = "0.1"
/// tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
/// hostname = "0.3"
/// dirs = "5.0"
/// ```

/// JARVIS Backup Script - Rust version
/// Run with: `rust-script backup.rs` or compile with `cargo build --release --bin backup`
use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "jarvis-backup",
    about = "JARVIS daily backup to OCI Object Storage"
)]
struct Args {
    /// Rclone remote name
    #[arg(long, default_value = "oci")]
    remote: String,

    /// Rclone bucket name
    #[arg(long, default_value = "jarvis-backups")]
    bucket: String,

    /// Backup directory
    #[arg(long, default_value = "/opt/jarvis/backups")]
    backup_dir: String,

    /// Dry run (no actual upload/delete)
    #[arg(long)]
    dry_run: bool,

    /// Skip remote operations
    #[arg(long)]
    local_only: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct QdrantSnapshotResponse {
    result: Vec<SnapshotInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotInfo {
    name: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let hostname = hostname::get()?.to_string_lossy().to_string();
    let date = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let backup_dir = Path::new(&args.backup_dir);

    fs::create_dir_all(backup_dir)?;

    info!("Starting JARVIS backup: {} on {}", date, hostname);

    // Resolve 1Password references in rclone config
    let rclone_config_flag = if !args.local_only {
        resolve_rclone_config()?
    } else {
        String::new()
    };

    // Run backup steps
    backup_postgres(backup_dir, &date)?;
    backup_qdrant(backup_dir, &date)?;
    backup_configs(backup_dir, &date)?;
    backup_repos(backup_dir, &date)?;

    if !args.local_only {
        upload_to_oci(backup_dir, &args, &hostname, &rclone_config_flag)?;
        cleanup_remote(&args, &hostname, &rclone_config_flag, args.dry_run)?;
    }

    cleanup_local(backup_dir)?;

    info!("Backup completed successfully: {}", date);
    Ok(())
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

fn run_rclone(args: &[&str], config_flag: &str) -> Result<()> {
    let mut cmd = Command::new("rclone");
    if !config_flag.is_empty() {
        cmd.args(config_flag.split_whitespace());
    }
    let status = cmd
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run rclone")?;
    if !status.success() {
        anyhow::bail!("rclone exited with status {status}");
    }
    Ok(())
}

fn backup_postgres(backup_dir: &Path, date: &str) -> Result<()> {
    info!("Backing up PostgreSQL...");
    let output_file = backup_dir.join(format!("pg-{}.sql.gz", date));

    let mut pg_dump = Command::new("pg_dump")
        .args(["-h", "localhost", "-U", "jarvis", "jarvis"])
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to start pg_dump")?;

    let mut gzip = Command::new("gzip")
        .stdin(pg_dump.stdout.take().context("No stdout from pg_dump")?)
        .stdout(fs::File::create(&output_file)?)
        .spawn()
        .context("Failed to start gzip")?;

    let pg_dump_status = pg_dump.wait().context("Failed waiting for pg_dump")?;
    let gzip_status = gzip.wait().context("Failed waiting for gzip")?;
    if !pg_dump_status.success() {
        anyhow::bail!("pg_dump failed with status {pg_dump_status}");
    }
    if !gzip_status.success() {
        anyhow::bail!("gzip failed with status {gzip_status}");
    }

    info!("PostgreSQL backup completed: {}", output_file.display());
    Ok(())
}

fn backup_qdrant(backup_dir: &Path, date: &str) -> Result<()> {
    info!("Backing up Qdrant...");
    let client = Client::new();

    // Create snapshot
    client
        .post("http://localhost:6333/collections/jarvis_memories/snapshots")
        .send()?
        .error_for_status()?;

    std::thread::sleep(std::time::Duration::from_secs(3));

    // List snapshots
    let response: QdrantSnapshotResponse = client
        .get("http://localhost:6333/collections/jarvis_memories/snapshots")
        .send()?
        .error_for_status()?
        .json()?;

    if let Some(snapshot) = response.result.first() {
        let snapshot_name = &snapshot.name;
        let output_file = backup_dir.join(format!("qdrant-{}.snapshot", date));

        let download = client
            .get(&format!(
                "http://localhost:6333/collections/jarvis_memories/snapshots/{}",
                snapshot_name
            ))
            .send()?
            .error_for_status()?
            .bytes()?;

        fs::write(&output_file, download)?;
        info!("Qdrant backup completed: {}", output_file.display());
    } else {
        warn!("No Qdrant snapshot found");
    }

    Ok(())
}

fn backup_configs(backup_dir: &Path, date: &str) -> Result<()> {
    info!("Backing up configuration files...");
    let output_file = backup_dir.join(format!("configs-{}.tar.gz", date));

    let status = Command::new("tar")
        .args([
            "-czf",
            output_file.to_str().unwrap(),
            "/etc/jarvis",
            "/home/jarvis/.config/opencode",
            "/home/jarvis/.config/code-server",
            "/home/jarvis/.local/share/code-server",
        ])
        .status()
        .context("Failed to run tar")?;

    if status.success() {
        info!("Config backup completed: {}", output_file.display());
    } else {
        warn!("Config backup partially failed");
    }

    Ok(())
}

fn backup_repos(backup_dir: &Path, date: &str) -> Result<()> {
    info!("Backing up git repositories...");
    let repos_dir = Path::new("/home/jarvis/repos");

    if !repos_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(repos_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.join(".git").exists() {
            let repo_name = path.file_name().unwrap().to_string_lossy();
            let output_file = backup_dir.join(format!("repo-{}-{}.bundle", repo_name, date));

            let status = Command::new("git")
                .args([
                    "-C",
                    path.to_str().unwrap(),
                    "bundle",
                    "create",
                    output_file.to_str().unwrap(),
                    "--all",
                ])
                .status()
                .context("Failed to run git bundle")?;

            if status.success() {
                info!("Repo backup completed: {}", output_file.display());
            } else {
                warn!("Failed to backup repo: {}", repo_name);
            }
        }
    }

    Ok(())
}

fn upload_to_oci(backup_dir: &Path, args: &Args, hostname: &str, config_flag: &str) -> Result<()> {
    info!("Uploading backups to OCI Object Storage...");

    let remote_path = format!("{}:{}/{}/", args.remote, args.bucket, hostname);
    run_rclone(
        &[
            "copy",
            backup_dir.to_str().unwrap(),
            &remote_path,
            "--progress",
            "--transfers",
            "4",
        ],
        config_flag,
    )?;

    info!("Upload to OCI completed");
    Ok(())
}

fn cleanup_local(backup_dir: &Path) -> Result<()> {
    info!("Cleaning up old local backups (older than 7 days)...");
    let count = fs::read_dir(backup_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let elapsed = std::time::SystemTime::now()
                        .duration_since(t)
                        .unwrap_or_default();
                    elapsed.as_secs() > 7 * 24 * 3600
                })
                .unwrap_or(false)
        })
        .inspect(|e| {
            let _ = fs::remove_file(e.path());
        })
        .count();

    info!("Cleaned up {} old local backups", count);
    Ok(())
}

fn cleanup_remote(args: &Args, hostname: &str, config_flag: &str, dry_run: bool) -> Result<()> {
    info!("Cleaning up old remote backups (older than 30 days)...");

    let remote_path = format!("{}:{}/{}/", args.remote, args.bucket, hostname);

    let mut cmd_args = vec!["delete", &remote_path, "--min-age", "30d"];
    if dry_run {
        cmd_args.push("--dry-run");
    }

    run_rclone(&cmd_args, config_flag)?;
    Ok(())
}
