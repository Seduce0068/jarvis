/// ```cargo
/// [dependencies]
/// anyhow = "1.0"
/// clap = { version = "4.5", features = ["derive"] }
/// tracing = "0.1"
/// tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
/// ```

/// JARVIS Deploy Script - Rust version
/// Run with: `rust-script deploy.rs <host> [build|deploy|restart|logs|status]`
/// Or compile with: `cargo build --release --bin deploy`
///
/// Use `--dry-run` to print every remote command without executing it.
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::Path;
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

    /// Run a command on the remote host and report whether it succeeded.
    fn ssh_output(&self, target: &str, command: &str) -> Result<bool> {
        if self.dry_run {
            info!("[dry-run] ssh {target} {command}");
            return Ok(true);
        }
        let output = Command::new("ssh")
            .args([target, command])
            .output()
            .context("ssh command failed")?;
        Ok(output.status.success())
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
}

fn build(runner: &Runner, rust_dir: &str) -> Result<()> {
    info!("Building Rust services for aarch64...");

    // Ensure target is installed
    if let Err(err) = runner.local(
        "rustup",
        &["target", "add", "aarch64-unknown-linux-gnu"],
        None,
    ) {
        warn!("rustup target add returned non-zero (may already be installed): {err}");
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

fn deploy(runner: &Runner, args: &Args, rust_dir: &str, repo_dir: &Path) -> Result<()> {
    info!("Deploying to {}...", args.host);

    // Create remote directories owned by the deploy user
    let remote_base = format!("{}@{}", args.user, args.host);
    runner.ssh(
        &remote_base,
        &format!("mkdir -p {0}/bin {0}/scripts", args.remote_path),
    )?;

    // Copy binaries
    let target_dir = Path::new(rust_dir).join("target/aarch64-unknown-linux-gnu/release");
    for service in [
        "jarvis-policy",
        "jarvis-mcp-gw",
        "jarvis-event",
        "jarvis-context",
    ] {
        info!("Deploying {}...", service);
        let binary_path = target_dir.join(service);
        if !binary_path.exists() && !runner.dry_run {
            warn!("Binary not found: {}", binary_path.display());
            continue;
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
            warn!("Script not found: {}", script_path.display());
            continue;
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

    // Wait for health checks
    info!("Waiting for services to be healthy...");
    if !runner.dry_run {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    for port in [8081, 8082, 8083, 8084] {
        let url = format!("http://127.0.0.1:{}/health", port);
        match runner.ssh_output(&remote_base, &format!("curl -sf {url}")) {
            Ok(true) => info!("Service on port {} is healthy", port),
            Ok(false) => warn!("Service on port {} not responding", port),
            Err(err) => warn!("Health check on port {} failed: {err}", port),
        }
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
