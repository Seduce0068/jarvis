/// ```cargo
/// [dependencies]
/// anyhow = "=1.0.104"
/// clap = { version = "4.5", features = ["derive"] }
/// tracing = "=0.1.44"
/// tracing-subscriber = { version = "=0.3.23", features = ["env-filter", "fmt"] }
/// ```

/// JARVIS Deploy Script - Rust version
/// Run with: `rust-script deploy.rs <host> [build|deploy|restart|logs|status]`
/// Or compile with: `cargo build --release --bin deploy`
///
/// Use `--dry-run` to print every remote command without executing it.
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::{info, warn};

/// Services managed by this deploy script (build + restart + logs + status).
const SERVICES: [&str; 5] = [
    "jarvis-policy",
    "jarvis-mcp-gw",
    "jarvis-event",
    "jarvis-context",
    "otelcol",
];

/// Service binaries that must exist before a real (non-dry-run) deploy may
/// proceed. A missing binary is a hard error so a deploy can never report
/// success while shipping an incomplete build.
const REQUIRED_BINARIES: [&str; 4] = [
    "jarvis-policy",
    "jarvis-mcp-gw",
    "jarvis-event",
    "jarvis-context",
];

/// Log line emitted when health checks are skipped during `--dry-run`.
/// Deliberately avoids the word "healthy": no check ran, so no health is claimed.
const DRY_RUN_HEALTH_SKIP_LOG: &str = "DRY RUN: health check skipped";

/// HTTP health-check ports for every managed service: the four required
/// services plus otelcol's health_check extension (13133).
const HEALTH_PORTS: [u16; 5] = [8081, 8082, 8083, 8084, 13133];

/// stdout plus exit status of a remote command (see `Runner::ssh_output`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteOutput {
    success: bool,
    stdout: String,
}

// health-gate-begin
/// Classification of one remote `/health` probe. Kept deliberately small and
/// std-only so `ci/scripts/test-legacy-guards.sh` can compile this exact block
/// with `rustc` and exercise it against a stubbed 503 response.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HealthStatus {
    Healthy,
    NotResponding,
    Degraded(String),
    CheckFailed(String),
}

/// Fixed degraded marker from the `jarvis-core` health contract. Matching the
/// whole `status` field (rather than a service-specific reason) keeps the gate
/// correct as individual services add their own downstream reasons.
const HEALTH_DEGRADED_MARKER: &str = r#""status":"degraded""#;

/// Parse the output of `curl -s --max-time <secs> -w '\n%{http_code}' <url>`:
/// the last line is the three-digit status, everything before it is the body.
/// A non-success HTTP status, a degraded body, a malformed response and a
/// failed command are all fail-closed; only a 2xx with a non-degraded body is
/// `Healthy`.
fn parse_health_probe(command_succeeded: bool, stdout: &str) -> HealthStatus {
    if !command_succeeded {
        return HealthStatus::CheckFailed("health probe command exited non-zero".to_owned());
    }
    let Some((body, code)) = stdout.rsplit_once('\n') else {
        return HealthStatus::NotResponding;
    };
    let code = code.trim();
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return HealthStatus::NotResponding;
    }
    let body = body.trim();
    // Whitespace-tolerant match: a pretty-printed body (`"status": "degraded"`)
    // must be classified exactly like the compact contract form. Otherwise a
    // relay that re-serializes the JSON could turn degradation into fail-open.
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let degraded = compact.contains(HEALTH_DEGRADED_MARKER);
    if code.starts_with('2') {
        return if degraded {
            HealthStatus::Degraded(body.to_owned())
        } else {
            HealthStatus::Healthy
        };
    }
    if code == "503" {
        return HealthStatus::Degraded(body.to_owned());
    }
    HealthStatus::NotResponding
}

/// Turn health outcomes into a deploy result. Any port that is not healthy is
/// a hard error for a real deploy, not a warning. All ports are evaluated so
/// the error lists every failure. Returns a plain `String` error to stay
/// std-only and directly testable; callers lift it into `anyhow`.
fn evaluate_health_checks(results: &[(u16, HealthStatus)]) -> Result<(), String> {
    let unhealthy: Vec<String> = results
        .iter()
        .filter_map(|(port, status)| match status {
            HealthStatus::Healthy => None,
            HealthStatus::NotResponding => Some(format!("{port} (not responding)")),
            HealthStatus::Degraded(reason) => Some(format!("{port} (degraded: {reason})")),
            HealthStatus::CheckFailed(err) => Some(format!("{port} (check failed: {err})")),
        })
        .collect();

    if unhealthy.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "health check failed for port(s): {}",
            unhealthy.join(", ")
        ))
    }
}

/// Bounded retry contract, kept in sync with
/// `infra/scripts/deploy-mcp-gw-safe.sh` (`health_attempts` /
/// `health_interval`). A slow start (context migration, Qdrant checks) must
/// not be misread as a dead service after one fixed 5s sleep, but the wait
/// must never be unbounded either.
const HEALTH_PROBE_ATTEMPTS: u32 = 20;
const HEALTH_PROBE_INTERVAL_MS: u64 = 500;
/// Absolute deadline: 20 * 0.5s of backoff plus slack for slow ssh/curl.
const HEALTH_WAIT_DEADLINE_SECS: u64 = 20;
/// Per-probe curl timeout (matches `-m 2` in the shell deploy/drift paths).
/// Without it a single hung probe would defeat HEALTH_WAIT_DEADLINE_SECS: the
/// absolute deadline is only checked between probe rounds, not mid-curl.
const HEALTH_PROBE_TIMEOUT_SECS: u64 = 2;

/// Decide whether another probe round is allowed. Pure and std-only so the
/// legacy guard can compile it with rustc alongside the parser/evaluator.
fn health_retry_allowed(attempt: u32, deadline: std::time::Instant) -> bool {
    attempt + 1 < HEALTH_PROBE_ATTEMPTS && std::time::Instant::now() < deadline
}
// health-gate-end

/// Environment files copied from infra/config/ that must be readable by the
/// jarvis user (op.env is intentionally excluded: it holds the 1Password
/// service account token and stays root-owned).
const JARVIS_ENV_FILES: [&str; 5] = [
    "/etc/jarvis/policy.env",
    "/etc/jarvis/mcp_gateway.env",
    "/etc/jarvis/event.env",
    "/etc/jarvis/context.env",
    "/etc/jarvis/otel.env",
];

#[derive(Parser, Debug)]
#[command(name = "jarvis-deploy", about = "JARVIS deploy Rust services to host")]
struct Args {
    /// Target host (Tailscale hostname or IP)
    #[arg(default_value = "jarvis-host")]
    host: String,

    /// Remote user
    #[arg(long, default_value = "jarvis")]
    user: String,

    /// Remote path
    #[arg(long, default_value = "/opt/jarvis")]
    remote_path: String,

    /// Local rust directory
    #[arg(long)]
    rust_dir: Option<String>,

    /// Print the commands that would run instead of executing them
    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build all services for aarch64
    Build,
    /// Build and deploy
    Deploy,
    /// Restart services on remote host
    Restart,
    /// Follow logs on remote host
    Logs,
    /// Show service status on remote host
    Status,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let runner = Runner {
        dry_run: args.dry_run,
    };
    let rust_dir = args.rust_dir.clone().unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("rust")
            .to_string_lossy()
            .to_string()
    });
    let repo_dir = Path::new(&rust_dir)
        .parent()
        .context("locating repository root")?
        .to_path_buf();

    match &args.command {
        Commands::Build => build(&runner, &rust_dir)?,
        Commands::Deploy => {
            build(&runner, &rust_dir)?;
            deploy(&runner, &args, &rust_dir, &repo_dir)?;
        }
        Commands::Restart => restart(&runner, &args)?,
        Commands::Logs => logs(&runner, &args)?,
        Commands::Status => status(&runner, &args)?,
    }

    Ok(())
}

/// Executes commands either locally or against the remote host, with an
/// optional dry-run mode that only prints what would be executed.
struct Runner {
    dry_run: bool,
}

impl Runner {
    /// Run a command locally.
    fn local(&self, program: &str, args: &[&str], cwd: Option<&Path>) -> Result<()> {
        if self.dry_run {
            info!("[dry-run] {} {}", program, args.join(" "));
            return Ok(());
        }
        let mut command = Command::new(program);
        command.args(args);
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        let status = command
            .status()
            .with_context(|| format!("failed to run {program}"))?;
        if !status.success() {
            bail!("{program} exited with {status}");
        }
        Ok(())
    }

    /// Run a command on the remote host, failing if it exits non-zero.
    fn ssh(&self, target: &str, command: &str) -> Result<()> {
        if self.dry_run {
            info!("[dry-run] ssh {target} {command}");
            return Ok(());
        }
        let status = Command::new("ssh")
            .args([target, command])
            .status()
            .context("ssh command failed")?;
        if !status.success() {
            bail!("ssh {target} '{command}' exited with {status}");
        }
        Ok(())
    }

    /// Run a command on the remote host and keep both its exit status and its
    /// stdout. Health checks need the body, not just a 200/Non-200 boolean:
    /// a degraded service can answer 200 with a degraded fixed body.
    fn ssh_output(&self, target: &str, command: &str) -> Result<RemoteOutput> {
        if self.dry_run {
            info!("[dry-run] ssh {target} {command}");
            return Ok(RemoteOutput {
                success: true,
                stdout: String::new(),
            });
        }
        let output = Command::new("ssh")
            .args([target, command])
            .output()
            .context("ssh command failed")?;
        Ok(RemoteOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        })
    }

    /// Copy a local path (file or directory) to the remote host.
    fn scp(&self, local: &Path, target: &str, remote_dir: &str) -> Result<()> {
        let spec = format!("{target}:{remote_dir}");
        if self.dry_run {
            info!("[dry-run] scp -r {} {spec}", local.display());
            return Ok(());
        }
        let status = Command::new("scp")
            .args(["-r", local.to_str().context("non-UTF-8 path")?, &spec])
            .status()
            .context("scp failed")?;
        if !status.success() {
            bail!("scp {} -> {spec} exited with {status}", local.display());
        }
        Ok(())
    }

    /// Merge the contents of a local directory into a remote directory using
    /// `tar` piped over ssh (with sudo on the remote side). Unlike `scp -r`,
    /// this does not nest the source directory under the target.
    fn stream_dir(&self, target: &str, local_dir: &Path, remote_dir: &str) -> Result<()> {
        let remote_command = format!("sudo tar -C {remote_dir} -xf -");
        if self.dry_run {
            info!(
                "[dry-run] tar -C {} -cf - . | ssh {target} {remote_command}",
                local_dir.display()
            );
            return Ok(());
        }
        let mut tar = Command::new("tar")
            .arg("-C")
            .arg(local_dir)
            .args(["-cf", "-", "."])
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to start tar")?;
        let tar_stdout = tar.stdout.take().context("tar stdout unavailable")?;
        let status = Command::new("ssh")
            .args([target, &remote_command])
            .stdin(Stdio::from(tar_stdout))
            .status()
            .context("failed to stream files over ssh")?;
        let tar_status = tar.wait().context("failed to wait for tar")?;
        if !status.success() || !tar_status.success() {
            bail!(
                "failed to sync {} to {target}:{remote_dir}",
                local_dir.display()
            );
        }
        Ok(())
    }

    /// Stream a single local file to a remote path (`cat` over ssh).
    fn stream_file(&self, target: &str, local: &Path, remote_path: &str) -> Result<()> {
        let remote_dir = Path::new(remote_path)
            .parent()
            .map(|dir| dir.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        let remote_command = format!("mkdir -p {remote_dir} && cat > {remote_path}");
        if self.dry_run {
            info!(
                "[dry-run] cat {} | ssh {target} ({remote_command})",
                local.display()
            );
            return Ok(());
        }
        let file = std::fs::File::open(local)
            .with_context(|| format!("failed to open {}", local.display()))?;
        let status = Command::new("ssh")
            .args([target, &remote_command])
            .stdin(Stdio::from(file))
            .status()
            .context("failed to stream file over ssh")?;
        if !status.success() {
            bail!(
                "failed to copy {} to {target}:{remote_path}",
                local.display()
            );
        }
        Ok(())
    }
}

fn build(runner: &Runner, rust_dir: &str) -> Result<()> {
    info!("Building Rust services for aarch64...");

    // Ensure target is installed. A non-zero exit is a hard error (the build
    // stage's `set -e` equivalent): `rustup target add` already succeeds when
    // the target is present, so failing here is not a benign "already there"
    // case. Continuing would only fail later at `cargo build`, losing the
    // original cause. In `--dry-run` `Runner::local` returns Ok without
    // executing anything, so this stays a no-op there.
    if let Err(err) = runner.local(
        "rustup",
        &["target", "add", "aarch64-unknown-linux-gnu"],
        None,
    ) {
        bail!("failed to add aarch64 target via rustup: {err}");
    }

    // Build all services
    runner.local(
        "cargo",
        &[
            "build",
            "--release",
            "--workspace",
            "--target",
            "aarch64-unknown-linux-gnu",
        ],
        Some(Path::new(rust_dir)),
    )?;

    info!("Build completed");
    Ok(())
}

/// Return the paths of required binaries that are missing under `target_dir`.
fn missing_binaries(target_dir: &Path) -> Vec<PathBuf> {
    REQUIRED_BINARIES
        .iter()
        .map(|service| target_dir.join(service))
        .filter(|path| !path.exists())
        .collect()
}

fn deploy(runner: &Runner, args: &Args, rust_dir: &str, repo_dir: &Path) -> Result<()> {
    info!("Deploying to {}...", args.host);

    let target_dir = Path::new(rust_dir).join("target/aarch64-unknown-linux-gnu/release");

    // Fail closed: a real deploy must not report success when a required
    // binary is absent. `--dry-run` never builds, so skip the check there and
    // only print the commands that would run.
    if !runner.dry_run {
        let missing = missing_binaries(&target_dir);
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("required binary(ies) missing: {list}; rebuild first");
        }
    }

    // Create remote directories owned by the deploy user
    let remote_base = format!("{}@{}", args.user, args.host);
    runner.ssh(
        &remote_base,
        &format!("mkdir -p {0}/bin {0}/scripts", args.remote_path),
    )?;

    // Copy binaries
    for service in REQUIRED_BINARIES {
        info!("Deploying {}...", service);
        let binary_path = target_dir.join(service);
        if runner.dry_run && !binary_path.exists() {
            info!("[dry-run] binary not built: {}", binary_path.display());
        }
        runner.scp(
            &binary_path,
            &remote_base,
            &format!("{}/bin/", args.remote_path),
        )?;
    }

    // Merge configuration files into /etc/jarvis (1:1 with infra/config/).
    // tar-over-ssh preserves the directory contents and uses sudo remotely.
    info!("Deploying config files...");
    let config_dir = repo_dir.join("infra/config");
    runner.stream_dir(&remote_base, &config_dir, "/etc/jarvis")?;
    let env_chown = JARVIS_ENV_FILES.join(" ");
    runner.ssh(
        &remote_base,
        &format!("sudo chown jarvis:jarvis {env_chown} && sudo chmod 600 {env_chown}"),
    )?;

    // OpenCode reads its config dir from OPENCODE_CONFIG_DIR (= ~/.config/opencode
    // in the unit), so install the managed config into the user's home as well.
    let opencode_config = config_dir.join("opencode/opencode.jsonc");
    if opencode_config.exists() || runner.dry_run {
        info!("Deploying OpenCode config...");
        runner.stream_file(
            &remote_base,
            &opencode_config,
            "/home/jarvis/.config/opencode/opencode.jsonc",
        )?;
        // Remove the legacy TOML config some hosts may still have from an
        // earlier deploy; OpenCode only reads opencode.jsonc.
        runner.ssh(
            &remote_base,
            "sudo rm -f /home/jarvis/.config/opencode/config.toml",
        )?;
    }

    // Deploy systemd units (single source: infra/systemd/)
    info!("Deploying systemd units...");
    let systemd_dir = repo_dir.join("infra/systemd");
    runner.stream_dir(&remote_base, &systemd_dir, "/etc/systemd/system")?;
    runner.ssh(&remote_base, "sudo systemctl daemon-reload")?;

    // Deploy persistent Rust scripts used by backup and restore operations.
    let scripts_dir = Path::new(rust_dir).join("scripts");
    for script in ["backup.rs", "restore.rs"] {
        let script_path = scripts_dir.join(script);
        info!("Deploying {}...", script_path.display());
        if !script_path.exists() && !runner.dry_run {
            // Fail closed like the binary check: shipping a deploy without
            // backup.rs/restore.rs would break the backup/restore path while
            // still printing "Deployment completed". Dry-run still prints the
            // would-be scp command and must not fail.
            bail!(
                "required script missing: {}; cannot deploy",
                script_path.display()
            );
        }
        runner.scp(
            &script_path,
            &remote_base,
            &format!("{}/scripts/", args.remote_path),
        )?;
    }

    // Restart services
    info!("Restarting services...");
    runner.ssh(
        &remote_base,
        &format!("sudo systemctl restart {}", SERVICES.join(" ")),
    )?;

    // Wait for health checks. In `--dry-run` nothing was deployed or
    // restarted, so do not run (or claim the result of) any health check.
    if runner.dry_run {
        info!("{DRY_RUN_HEALTH_SKIP_LOG}");
    } else {
        // I101-C (c): retry with a bounded budget AND an absolute deadline
        // instead of one fixed 5s sleep plus a single probe. Slow starts now
        // get up to HEALTH_PROBE_ATTEMPTS probes spaced by
        // HEALTH_PROBE_INTERVAL_MS; neither cap nor deadline can stretch the
        // wait forever.
        info!("Waiting for services to be healthy (up to {HEALTH_PROBE_ATTEMPTS} probes)...");
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(HEALTH_WAIT_DEADLINE_SECS);
        let mut results = Vec::with_capacity(HEALTH_PORTS.len());
        let mut attempt = 0u32;
        loop {
            results.clear();
            for port in HEALTH_PORTS {
                let url = format!("http://127.0.0.1:{port}/health");
                // Deliberately NO `-f`: curl must print the body of a 503 so a
                // degraded service cannot pass as healthy. `-w` appends the
                // status code on its own last line for an explicit check too.
                let command = format!(
                    "curl -s --max-time {HEALTH_PROBE_TIMEOUT_SECS} -w '\\n%{{http_code}}' {url}"
                );
                let outcome = match runner.ssh_output(&remote_base, &command) {
                    Ok(output) => parse_health_probe(output.success, &output.stdout),
                    Err(err) => HealthStatus::CheckFailed(err.to_string()),
                };
                match &outcome {
                    HealthStatus::Healthy => info!("Service on port {port} is healthy"),
                    HealthStatus::NotResponding => warn!("Service on port {port} not responding"),
                    HealthStatus::Degraded(reason) => {
                        warn!("Service on port {port} is degraded: {reason}")
                    }
                    HealthStatus::CheckFailed(err) => {
                        warn!("Health check on port {port} failed: {err}")
                    }
                }
                results.push((port, outcome));
            }
            // Stop as soon as every port is healthy, or when either budget is
            // spent. The final `results` are always evaluated below so every
            // still-failing port is named in the error.
            let all_healthy = evaluate_health_checks(&results).is_ok();
            if all_healthy || !health_retry_allowed(attempt, deadline) {
                break;
            }
            attempt += 1;
            std::thread::sleep(std::time::Duration::from_millis(HEALTH_PROBE_INTERVAL_MS));
        }
        // A real deploy must not report completion with an unhealthy or
        // degraded service: `?` returns early so "Deployment completed" is
        // skipped. Dry-run never reaches this function.
        evaluate_health_checks(&results).map_err(anyhow::Error::msg)?;
    }

    info!("Deployment completed");
    Ok(())
}

fn restart(runner: &Runner, args: &Args) -> Result<()> {
    info!("Restarting services on {}...", args.host);
    let remote_base = format!("{}@{}", args.user, args.host);
    runner.ssh(
        &remote_base,
        &format!("sudo systemctl restart {}", SERVICES.join(" ")),
    )?;
    Ok(())
}

fn logs(runner: &Runner, args: &Args) -> Result<()> {
    let remote_base = format!("{}@{}", args.user, args.host);
    let mut command = String::from("sudo journalctl");
    for service in SERVICES {
        command.push_str(&format!(" -u {service}"));
    }
    command.push_str(" -f");
    if runner.dry_run {
        info!("[dry-run] ssh {remote_base} {command}");
        return Ok(());
    }
    let status = Command::new("ssh")
        .args([&remote_base, &command])
        .status()?;
    if !status.success() {
        bail!("failed to follow logs");
    }
    Ok(())
}

fn status(runner: &Runner, args: &Args) -> Result<()> {
    let remote_base = format!("{}@{}", args.user, args.host);
    let mut command = String::from("sudo systemctl status");
    for service in SERVICES {
        command.push_str(&format!(" {service}"));
    }
    if runner.dry_run {
        info!("[dry-run] ssh {remote_base} {command}");
        return Ok(());
    }
    let status = Command::new("ssh")
        .args([&remote_base, &command])
        .status()?;
    if !status.success() {
        bail!("failed to get status");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("jarvis-deploy-test-{}-{n}", std::process::id()))
    }

    #[test]
    fn missing_binaries_detects_all_absent_required_binaries() {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir).expect("create temp dir");

        let missing = missing_binaries(&dir);
        let names: Vec<&str> = missing
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, REQUIRED_BINARIES);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_binaries_is_empty_when_all_required_binaries_exist() {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir).expect("create temp dir");
        for service in REQUIRED_BINARIES {
            fs::write(dir.join(service), b"stub").expect("write stub binary");
        }

        assert!(missing_binaries(&dir).is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dry_run_health_log_never_claims_healthy() {
        assert_eq!(DRY_RUN_HEALTH_SKIP_LOG, "DRY RUN: health check skipped");
        assert!(!DRY_RUN_HEALTH_SKIP_LOG.to_lowercase().contains("healthy"));
    }

    fn outcomes(healthy: bool) -> Vec<(u16, HealthStatus)> {
        HEALTH_PORTS
            .into_iter()
            .map(|port| {
                (
                    port,
                    if healthy {
                        HealthStatus::Healthy
                    } else {
                        HealthStatus::NotResponding
                    },
                )
            })
            .collect()
    }

    /// I101-C (c): the retry budget must match the shell deploy path and be
    /// bounded by BOTH an attempt cap and an absolute deadline.
    #[test]
    fn health_retry_budget_is_bounded_and_matches_shell_path() {
        assert_eq!(HEALTH_PROBE_ATTEMPTS, 20);
        assert_eq!(HEALTH_PROBE_INTERVAL_MS, 500);
        assert_eq!(HEALTH_WAIT_DEADLINE_SECS, 20);
        // I103-C (a): each probe is bounded too, so one hung curl cannot eat
        // the whole absolute deadline. Matches `-m 2` in the shell paths.
        assert_eq!(HEALTH_PROBE_TIMEOUT_SECS, 2);

        let future = std::time::Instant::now()
            + std::time::Duration::from_secs(HEALTH_WAIT_DEADLINE_SECS);
        assert!(health_retry_allowed(0, future));
        assert!(health_retry_allowed(HEALTH_PROBE_ATTEMPTS - 2, future));
        // The 20th probe is the last one allowed.
        assert!(!health_retry_allowed(HEALTH_PROBE_ATTEMPTS - 1, future));

        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        assert!(!health_retry_allowed(0, past));
    }

    #[test]
    fn health_checks_pass_when_all_ports_healthy() {
        assert!(evaluate_health_checks(&outcomes(true)).is_ok());
    }

    #[test]
    fn health_checks_fail_deploy_when_port_not_responding() {
        let mut results = outcomes(true);
        let failed_port = HEALTH_PORTS[1];
        results[1] = (failed_port, HealthStatus::NotResponding);

        let err = evaluate_health_checks(&results).expect_err("unhealthy port must fail deploy");
        assert!(err.contains(&failed_port.to_string()));
        assert!(err.contains("not responding"));
    }

    #[test]
    fn health_checks_fail_deploy_when_check_errored() {
        let mut results = outcomes(true);
        let failed_port = HEALTH_PORTS[2];
        results[2] = (
            failed_port,
            HealthStatus::CheckFailed("ssh exited with status 255".to_owned()),
        );

        let err = evaluate_health_checks(&results).expect_err("check error must fail deploy");
        assert!(err.contains(&failed_port.to_string()));
        assert!(err.contains("check failed"));
    }

    /// I99-C: a 503 from any service must fail the deploy even when a naive
    /// 200-only gate would have accepted it.
    #[test]
    fn health_checks_fail_deploy_when_service_is_degraded() {
        let mut results = outcomes(true);
        let failed_port = HEALTH_PORTS[0];
        let body = r#"{"status":"degraded","reason":"kill_switch"}"#;
        results[0] = (failed_port, HealthStatus::Degraded(body.to_owned()));

        let err = evaluate_health_checks(&results).expect_err("degraded port must fail deploy");
        assert!(err.contains(&failed_port.to_string()));
        assert!(err.contains("degraded"));
        assert!(err.contains(body));
    }

    #[test]
    fn health_checks_report_every_failure() {
        let results = outcomes(false);
        let msg = evaluate_health_checks(&results)
            .expect_err("all-unhealthy ports must fail deploy");
        for port in HEALTH_PORTS {
            assert!(
                msg.contains(&port.to_string()),
                "missing port {port} in: {msg}"
            );
        }
    }

    /// I99-C: parser-level guards for the two degraded shapes (503 status and
    /// a 200 carrying the fixed degraded body), plus the healthy and unreachable
    /// cases. Stub payloads only; no service is contacted.
    #[test]
    fn parse_health_probe_distinguishes_healthy_degraded_and_unreachable() {
        assert_eq!(
            parse_health_probe(true, "{\"status\":\"ok\"}\n200"),
            HealthStatus::Healthy
        );
        assert_eq!(
            parse_health_probe(
                true,
                "{\"status\":\"degraded\",\"reason\":\"no_upstream\"}\n503"
            ),
            HealthStatus::Degraded(
                r#"{"status":"degraded","reason":"no_upstream"}"#.to_owned()
            )
        );
        // A relay that rewrites the status to 200 must not hide degradation.
        assert_eq!(
            parse_health_probe(
                true,
                "{\"status\":\"degraded\",\"reason\":\"store_unavailable\"}\n200"
            ),
            HealthStatus::Degraded(
                r#"{"status":"degraded","reason":"store_unavailable"}"#.to_owned()
            )
        );
        assert_eq!(
            parse_health_probe(true, "\n000"),
            HealthStatus::NotResponding
        );
        assert_eq!(
            parse_health_probe(false, ""),
            HealthStatus::CheckFailed("health probe command exited non-zero".to_owned())
        );
    }

    /// I103-C (b): a pretty-printed degraded body must be classified as
    /// degraded for both a real 503 and a rewritten 200. `matches!` keeps the
    /// stub body/reason out of any assertion failure output.
    #[test]
    fn parse_health_probe_detects_pretty_printed_degraded_body() {
        let pretty_degraded_503_body =
            "{\n  \"status\": \"degraded\",\n  \"reason\": \"no_upstream\"\n}\n503";
        assert!(matches!(
            parse_health_probe(true, pretty_degraded_503_body),
            HealthStatus::Degraded(_)
        ));

        let pretty_degraded_200_body =
            "{\n  \"status\": \"degraded\",\n  \"reason\": \"store_unavailable\"\n}\n200";
        assert!(matches!(
            parse_health_probe(true, pretty_degraded_200_body),
            HealthStatus::Degraded(_)
        ));
    }
}
