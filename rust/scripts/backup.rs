/// ```cargo
/// [dependencies]
/// anyhow = "=1.0.104"
/// chrono = { version = "=0.4.45", features = ["serde"] }
/// clap = { version = "4.5", features = ["derive"] }
/// reqwest = { version = "=0.12.28", features = ["json", "blocking", "rustls-tls"] }
/// serde = { version = "=1.0.229", features = ["derive"] }
/// serde_json = "=1.0.151"
/// tokio = { version = "=1.53.2", features = ["full"] }
/// tracing = "=0.1.44"
/// tracing-subscriber = { version = "=0.3.23", features = ["env-filter", "fmt"] }
/// hostname = "0.3"
/// dirs = "=5.0.1"
/// ```

/// JARVIS Backup Script - Rust version（レガシー・非推奨）
///
/// OCI Object Storage / rclone / `op inject` を前提とした旧方式のバックアップツールです。
/// ADR-0004（OCI 不使用）に従いレガシーとして保持しています。
/// 現行のバックアップには `infra/scripts/backup-local.sh` を使用してください。
///
/// 以下は参照用に残す既存コードであり、挙動・テストは変更しないでください。
/// Run with: `rust-script backup.rs` or compile with `cargo build --release --bin backup`
use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use tracing::{info, warn};

/// Default HTTP timeout (seconds) for Qdrant snapshot requests.
const QDRANT_HTTP_TIMEOUT_SECS: u64 = 30;
/// Optional environment variable to override the Qdrant HTTP timeout (seconds).
const QDRANT_TIMEOUT_ENV: &str = "JARVIS_QDRANT_TIMEOUT_SECS";

#[derive(Parser, Debug)]
#[command(
    name = "jarvis-backup",
    about = "JARVIS backup (legacy/deprecated: OCI Object Storage / rclone / op inject) - use infra/scripts/backup-local.sh",
    long_about = "レガシー・非推奨の JARVIS バックアップツール。\n\nOCI Object Storage / rclone / `op inject` を前提とした旧方式です（ADR-0004: OCI 不使用）。\n現行のバックアップには `infra/scripts/backup-local.sh` を使用してください。\nこのスクリプトは参照用に保持しており、挙動・テストは変更されません。"
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

    cleanup_local(backup_dir, args.dry_run)?;

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

fn qdrant_http_timeout() -> Duration {
    let secs = std::env::var(QDRANT_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(QDRANT_HTTP_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn build_qdrant_client() -> Result<Client> {
    Client::builder()
        .timeout(qdrant_http_timeout())
        .build()
        .context("Failed to build Qdrant HTTP client")
}

/// Picks the snapshot name to download from the snapshots-list response.
///
/// A backup must never finish with an empty Qdrant payload, so both an empty
/// list and a blank name are hard errors instead of warnings.
fn require_snapshot_name(response: &QdrantSnapshotResponse) -> Result<&str> {
    let Some(snapshot) = response.result.first() else {
        anyhow::bail!("No Qdrant snapshot found; aborting backup");
    };
    if snapshot.name.is_empty() {
        anyhow::bail!("Qdrant snapshot name is empty; aborting backup");
    }
    Ok(snapshot.name.as_str())
}

fn backup_qdrant(backup_dir: &Path, date: &str) -> Result<()> {
    info!("Backing up Qdrant...");
    let client = build_qdrant_client()?;

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

    let snapshot_name = require_snapshot_name(&response)?;
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

fn cleanup_local(backup_dir: &Path, dry_run: bool) -> Result<()> {
    cleanup_local_at(backup_dir, dry_run, std::time::SystemTime::now())
}

fn cleanup_local_at(backup_dir: &Path, dry_run: bool, now: std::time::SystemTime) -> Result<()> {
    info!("Cleaning up old local backups (older than 7 days)...");
    let old_files = fs::read_dir(backup_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            e.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    now.duration_since(t)
                        .map(|elapsed| elapsed.as_secs() > 7 * 24 * 3600)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        })
        .map(|e| e.path());

    let remove = |path: &Path| fs::remove_file(path);
    let removed = remove_old_backup_files(old_files, dry_run, remove);

    if dry_run {
        info!("[dry-run] Would remove {} old local backups", removed);
    } else {
        info!("Cleaned up {} old local backups", removed);
    }
    Ok(())
}

/// Deletes (or, in dry-run mode, only counts) the given old backup files.
///
/// The `remove` closure is injectable so the failure/count behavior can be
/// exercised in tests without depending on filesystem permissions.
fn remove_old_backup_files<I, F>(paths: I, dry_run: bool, mut remove: F) -> usize
where
    I: Iterator<Item = std::path::PathBuf>,
    F: FnMut(&Path) -> std::io::Result<()>,
{
    let mut count = 0usize;
    for path in paths {
        if dry_run {
            info!(
                "[dry-run] Would remove old local backup: {}",
                path.display()
            );
            count += 1;
            continue;
        }
        match remove(&path) {
            Ok(()) => count += 1,
            Err(err) => {
                warn!(
                    "Failed to remove old local backup {}: {}",
                    path.display(),
                    err
                );
            }
        }
    }
    count
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "jarvis-backup-test-{}-{}-{}",
            tag,
            std::process::id(),
            nonce
        ))
    }

    #[test]
    fn cleanup_local_dry_run_keeps_old_files() {
        let dir = unique_temp_dir("dry-run");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let old_file = dir.join("pg-20260101-000000.sql.gz");
        fs::write(&old_file, b"backup data").unwrap();

        // Inject a "now" 8 days in the future so the freshly created file counts
        // as older than 7 days without altering its mtime (portable across Rust
        // versions and filesystems).
        let future_now =
            std::time::SystemTime::now() + std::time::Duration::from_secs(8 * 24 * 3600);

        cleanup_local_at(&dir, true, future_now).unwrap();
        assert!(
            old_file.exists(),
            "dry-run must not delete {}",
            old_file.display()
        );

        cleanup_local_at(&dir, false, future_now).unwrap();
        assert!(
            !old_file.exists(),
            "non-dry-run must delete {}",
            old_file.display()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_old_backup_files_counts_only_successful_removals() {
        let paths = [
            std::path::PathBuf::from("/tmp/jarvis-backup/ok"),
            std::path::PathBuf::from("/tmp/jarvis-backup/fail"),
        ];
        let mut attempted = Vec::new();
        let removed = remove_old_backup_files(paths.into_iter(), false, |path| {
            attempted.push(path.to_path_buf());
            if path.ends_with("fail") {
                Err(std::io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(())
            }
        });
        assert_eq!(removed, 1, "failed removals not counted");
        assert_eq!(attempted.len(), 2, "all files attempted");
    }

    #[test]
    fn remove_old_backup_files_dry_run_skips_removal() {
        let paths = [std::path::PathBuf::from("/tmp/jarvis-backup-dry-run")];
        let removed = remove_old_backup_files(paths.into_iter(), true, |_| {
            panic!("dry-run must not call remove")
        });
        assert_eq!(removed, 1, "dry-run counts files");
    }

    #[test]
    fn qdrant_client_builds_with_configured_timeout() {
        assert!(
            build_qdrant_client().is_ok(),
            "Qdrant HTTP client must build with the configured timeout"
        );
    }

    fn snapshot_response(json: &str) -> QdrantSnapshotResponse {
        serde_json::from_str(json).expect("test fixture must be valid JSON")
    }

    #[test]
    fn require_snapshot_name_rejects_empty_list() {
        let response = snapshot_response(r#"{"result":[]}"#);
        assert!(
            require_snapshot_name(&response).is_err(),
            "empty snapshot list must fail the backup"
        );
    }

    #[test]
    fn require_snapshot_name_rejects_blank_name() {
        let response = snapshot_response(r#"{"result":[{"name":""}]}"#);
        assert!(
            require_snapshot_name(&response).is_err(),
            "blank snapshot name must fail the backup"
        );
    }

    #[test]
    fn require_snapshot_name_uses_first_snapshot() {
        let response = snapshot_response(r#"{"result":[{"name":"first"},{"name":"second"}]}"#);
        assert_eq!(require_snapshot_name(&response).unwrap(), "first");
    }
}
