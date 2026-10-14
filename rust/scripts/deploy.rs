/// ```cargo
/// [dependencies]
/// anyhow = "1.0"
/// clap = { version = "4.5", features = ["derive"] }
/// chrono = { version = "0.4", features = ["serde"] }
/// tracing = "0.1"
/// tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
/// ```

/// JARVIS Deploy Script - Rust version
/// Run with: `rust-script deploy.rs <host> [build|deploy|restart|logs|status]`
/// Or compile with: `cargo build --release --bin deploy`
use anyhow::{Context, Result};
use chrono::Local;
use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::{error, info, warn};

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
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let rust_dir = args.rust_dir.clone().unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("rust")
            .to_string_lossy()
            .to_string()
    });

    match &args.command {
        Commands::Build => build(&rust_dir)?,
        Commands::Deploy => {
            build(&rust_dir)?;
            deploy(&args, &rust_dir)?;
        }
        Commands::Restart => restart(&args)?,
        Commands::Logs => logs(&args)?,
        Commands::Status => status(&args)?,
    }

    Ok(())
}

fn build(rust_dir: &str) -> Result<()> {
    info!("Building Rust services for aarch64...");

    // Ensure target is installed
    let status = Command::new("rustup")
        .args(["target", "add", "aarch64-unknown-linux-gnu"])
        .status()
        .context("Failed to add aarch64 target")?;

    if !status.success() {
        warn!("rustup target add returned non-zero (may already be installed)");
    }

    // Build all services
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--workspace",
            "--target",
            "aarch64-unknown-linux-gnu",
        ])
        .current_dir(rust_dir)
        .status()
        .context("Failed to run cargo build")?;

    if !status.success() {
        return Err(anyhow::anyhow!("Build failed"));
    }

    info!("Build completed");
    Ok(())
}

fn deploy(args: &Args, rust_dir: &str) -> Result<()> {
    info!("Deploying to {}...", args.host);

    // Create remote directories
    let remote_base = format!("{}@{}", args.user, args.host);
    run_ssh(&remote_base, &format!("mkdir -p {}/bin", args.remote_path))?;

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
        if !binary_path.exists() {
            warn!("Binary not found: {}", binary_path.display());
            continue;
        }
        run_scp(
            &binary_path,
            &remote_base,
            &format!("{}/bin/", args.remote_path),
        )?;
    }

    // Copy config files
    info!("Deploying config files...");
    let config_dir = Path::new(rust_dir).parent().unwrap().join("infra/config");
    run_scp(&config_dir, &remote_base, "/etc/jarvis/")?;

    // Restart services
    info!("Restarting services...");
    run_ssh(&remote_base, "sudo systemctl daemon-reload && sudo systemctl restart jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context")?;

    // Wait for health checks
    info!("Waiting for services to be healthy...");
    std::thread::sleep(std::time::Duration::from_secs(5));

    for port in [8081, 8082, 8083, 8084] {
        let url = format!("http://127.0.0.1:{}/health", port);
        let output = Command::new("ssh")
            .args([&remote_base, "curl", "-sf", &url])
            .output()?;

        if output.status.success() {
            info!("Service on port {} is healthy", port);
        } else {
            warn!("Service on port {} not responding", port);
        }
    }

    info!("Deployment completed");
    Ok(())
}

fn restart(args: &Args) -> Result<()> {
    info!("Restarting services on {}...", args.host);
    let remote_base = format!("{}@{}", args.user, args.host);
    run_ssh(
        &remote_base,
        "sudo systemctl restart jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context",
    )?;
    Ok(())
}

fn logs(args: &Args) -> Result<()> {
    let remote_base = format!("{}@{}", args.user, args.host);
    let status = Command::new("ssh")
        .args([
            &remote_base,
            "sudo",
            "journalctl",
            "-u",
            "jarvis-policy",
            "-u",
            "jarvis-mcp-gw",
            "-u",
            "jarvis-event",
            "-u",
            "jarvis-context",
            "-f",
        ])
        .status()?;

    if !status.success() {
        return Err(anyhow::anyhow!("Failed to follow logs"));
    }
    Ok(())
}

fn status(args: &Args) -> Result<()> {
    let remote_base = format!("{}@{}", args.user, args.host);
    let status = Command::new("ssh")
        .args([
            &remote_base,
            "sudo",
            "systemctl",
            "status",
            "jarvis-policy",
            "jarvis-mcp-gw",
            "jarvis-event",
            "jarvis-context",
        ])
        .status()?;

    if !status.success() {
        return Err(anyhow::anyhow!("Failed to get status"));
    }
    Ok(())
}

fn run_ssh(host: &str, command: &str) -> Result<()> {
    let status = Command::new("ssh")
        .args([host, command])
        .status()
        .context("SSH command failed")?;

    if !status.success() {
        return Err(anyhow::anyhow!("SSH command returned non-zero"));
    }
    Ok(())
}

fn run_scp(local: &Path, host: &str, remote: &str) -> Result<()> {
    let status = Command::new("scp")
        .args([
            "-r",
            local.to_str().unwrap(),
            &format!("{}:{}", host, remote),
        ])
        .status()
        .context("SCP failed")?;

    if !status.success() {
        return Err(anyhow::anyhow!("SCP returned non-zero"));
    }
    Ok(())
}
