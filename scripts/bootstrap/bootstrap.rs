/// ```cargo
/// [dependencies]
/// anyhow = "1.0"
/// clap = { version = "4.5", features = ["derive"] }
/// ```
// Bootstrap JARVIS server or Termux environment.
use anyhow::{bail, Context, Result};
use clap::Parser;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SERVER_ENV: &str = r#"# Do not commit this file.
# Home Assistant MCP:
# HA_MCP_URL=https://homeassistant.example/api/mcp
# HA_TOKEN=REPLACE_ME
# Android MCP should normally be reached through VPN/tunnel.
# ANDROID_MCP_URL=http://127.0.0.1:8765/mcp
# ANDROID_MCP_TOKEN=REPLACE_ME
# OPENAI_API_KEY=
# ANTHROPIC_API_KEY=
# GEMINI_API_KEY=
# OPENROUTER_API_KEY=
"#;

const TOOL_POLICY: &str = r#"{
  "default": "deny",
  "classes": {
    "read_only": {"decision": "allow"},
    "notify": {"decision": "allow"},
    "write_low_risk": {"decision": "approval_required"},
    "external_side_effect": {"decision": "approval_required"},
    "financial": {"decision": "deny_without_explicit_session_approval"}
  }
}
"#;

const MOBILE_ENV: &str = r#"# Generated placeholder; keep Android MCP private.
ANDROID_MCP_BIND=127.0.0.1
ANDROID_MCP_PORT=8765
"#;

#[derive(Debug, Parser)]
#[command(
    name = "jarvis-bootstrap",
    about = "Bootstrap JARVIS server or Termux environment"
)]
struct Args {
    #[arg(long, value_parser = ["server", "termux"])]
    target: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    skip_opencode: bool,
    #[arg(long)]
    skip_android_mcp: bool,
    #[arg(long)]
    setup_boot: bool,
    #[arg(long, default_value = "")]
    root: PathBuf,
    #[arg(long, default_value = ".")]
    source_root: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let root = if args.root.as_os_str().is_empty() {
        home.join("jarvis")
    } else {
        args.root.clone()
    };
    let source_root = args
        .source_root
        .canonicalize()
        .context("Invalid source root")?;

    match args.target.as_str() {
        "server" => bootstrap_server(&args, &root, &source_root),
        "termux" => bootstrap_termux(&args, &root, &source_root),
        _ => bail!("target must be server or termux"),
    }
}

fn bootstrap_server(args: &Args, root: &Path, source_root: &Path) -> Result<()> {
    if env::consts::OS != "linux" {
        bail!("server target currently supports Linux only");
    }
    install_linux_packages(args)?;
    create_layout(args, root, source_root, true)?;
    install_opencode(args)?;
    println!("Server bootstrap complete. Run `opencode auth login` to connect providers.");
    println!(
        "Start the agent runtime with `opencode serve` and keep it loopback-only behind Tailscale."
    );
    Ok(())
}

fn install_linux_packages(args: &Args) -> Result<()> {
    if executable_on_path("apt-get") {
        run(args, "sudo", &["apt-get", "update"])?;
        install_packages(
            args,
            "apt-get",
            &[
                "ca-certificates",
                "curl",
                "git",
                "jq",
                "openssl",
                "openssh-client",
                "python3",
            ],
        )?;
    } else if executable_on_path("dnf") {
        install_packages(
            args,
            "dnf",
            &[
                "ca-certificates",
                "curl",
                "git",
                "jq",
                "openssl",
                "openssh-clients",
                "python3",
            ],
        )?;
    } else if executable_on_path("yum") {
        install_packages(
            args,
            "yum",
            &[
                "ca-certificates",
                "curl",
                "git",
                "jq",
                "openssl",
                "openssh-clients",
                "python3",
            ],
        )?;
    } else if executable_on_path("pacman") {
        let packages = [
            "ca-certificates",
            "curl",
            "git",
            "jq",
            "openssl",
            "openssh",
            "python",
        ];
        let mut command = vec!["pacman", "-Sy", "--needed", "--noconfirm"];
        command.extend(packages);
        run(args, "sudo", &command)?;
    } else {
        bail!("unsupported Linux package manager");
    }
    Ok(())
}

fn install_packages(args: &Args, manager: &str, packages: &[&str]) -> Result<()> {
    let mut command = vec![manager, "install", "-y"];
    command.extend_from_slice(packages);
    run(args, "sudo", &command)
}

fn create_layout(args: &Args, root: &Path, source_root: &Path, server: bool) -> Result<()> {
    let subdirs: &[&str] = if server {
        &["bin", "config", "state", "logs", "policies"]
    } else {
        &["bin", "config", "repos"]
    };
    for subdir in subdirs {
        let path = root.join(subdir);
        if args.dry_run {
            println!("DRY RUN: create directory {}", path.display());
        } else {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&path)
                .with_context(|| format!("create {}", path.display()))?;
        }
    }
    if args.dry_run {
        return Ok(());
    }

    if server {
        write_private(&root.join("config/jarvis.env"), SERVER_ENV)?;
        fs::write(root.join("policies/tool-policy.json"), TOOL_POLICY)?;
        copy_tool(
            source_root,
            "scripts/doctor/jarvis-doctor.rs",
            &root.join("bin/jarvis-doctor.rs"),
        )?;
        println!(
            "Run the diagnostic with: rust-script {}/bin/jarvis-doctor.rs",
            root.display()
        );
    } else {
        copy_tool(
            source_root,
            "scripts/utils/phone-status-json.rs",
            &root.join("bin/phone-status-json.rs"),
        )?;
        write_private(&root.join("config/mobile.env"), MOBILE_ENV)?;
        println!(
            "Run phone status with: rust-script {}/bin/phone-status-json.rs",
            root.display()
        );
    }
    Ok(())
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    std::io::Write::write_all(&mut file, contents.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn copy_tool(source_root: &Path, source: &str, destination: &Path) -> Result<()> {
    let from = source_root.join(source);
    fs::copy(&from, destination)
        .with_context(|| format!("copy {} to {}", from.display(), destination.display()))?;
    Ok(())
}

fn install_opencode(args: &Args) -> Result<()> {
    if args.skip_opencode || executable_on_path("opencode") {
        return Ok(());
    }
    if args.dry_run {
        println!("DRY RUN: download official OpenCode installer and run it");
        return Ok(());
    }
    let installer = env::temp_dir().join(format!("jarvis-opencode-install-{}", std::process::id()));
    let status = Command::new("curl")
        .args([
            "--fail",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--tlsv1.2",
            "https://opencode.ai/v2/install",
            "--output",
        ])
        .arg(&installer)
        .status()
        .context("download OpenCode installer")?;
    ensure_success(status, "curl installer")?;
    let status = Command::new("bash")
        .arg(&installer)
        .status()
        .context("execute OpenCode installer")?;
    let _ = fs::remove_file(&installer);
    ensure_success(status, "OpenCode installer")?;
    println!("OpenCode installed under ~/.local/bin; ensure that directory is on PATH.");
    Ok(())
}

fn bootstrap_termux(args: &Args, root: &Path, source_root: &Path) -> Result<()> {
    if !is_termux() {
        bail!("termux target must run inside Termux");
    }
    run(args, "pkg", &["update", "-y"])?;
    run(
        args,
        "pkg",
        &[
            "install",
            "-y",
            "git",
            "curl",
            "jq",
            "openssh",
            "python",
            "termux-api",
            "rust",
        ],
    )?;
    create_layout(args, root, source_root, false)?;
    if !args.skip_android_mcp {
        install_android_mcp(args, root)?;
    }
    report_rish(args.dry_run)?;
    println!("Termux bootstrap complete. Grant only required Termux:API permissions and keep Android MCP loopback-only.");
    Ok(())
}

fn install_android_mcp(args: &Args, root: &Path) -> Result<()> {
    let repo = root.join("repos/shizuku-mcp");
    if repo.join(".git").is_dir() {
        run(
            args,
            "git",
            &[
                "-C",
                repo.to_str().context("repo path is not UTF-8")?,
                "pull",
                "--ff-only",
            ],
        )?;
    } else {
        run(
            args,
            "git",
            &[
                "clone",
                "https://github.com/shizzgar/shizuku-mcp.git",
                repo.to_str().context("repo path is not UTF-8")?,
            ],
        )?;
    }
    if args.dry_run {
        println!("DRY RUN: run {} install.sh", repo.display());
        if args.setup_boot {
            println!("DRY RUN: run {} setup_boot.sh", repo.display());
        }
        return Ok(());
    }
    run_in_dir(&repo, "bash", &["install.sh"])?;
    if args.setup_boot {
        run_in_dir(&repo, "bash", &["setup_boot.sh"])?;
    }
    Ok(())
}

fn run_in_dir(directory: &Path, program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .current_dir(directory)
        .status()?;
    ensure_success(status, program)
}

fn run(options: &Args, program: &str, args: &[&str]) -> Result<()> {
    if options.dry_run {
        println!("DRY RUN: {program} {}", args.join(" "));
        return Ok(());
    }
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {program}"))?;
    ensure_success(status, program)
}

fn ensure_success(status: std::process::ExitStatus, command: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("{command} exited with status {status}")
    }
}

fn executable_on_path(command: &str) -> bool {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .any(|directory| directory.join(command).is_file())
}

fn is_termux() -> bool {
    env::var_os("TERMUX_VERSION").is_some()
        || env::var("PREFIX").is_ok_and(|prefix| prefix.contains("com.termux"))
        || Path::new("/data/data/com.termux/files/usr").is_dir()
}

fn report_rish(dry_run: bool) -> Result<()> {
    let candidates = [
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("bin/rish"),
        env::var_os("PREFIX")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("bin/rish"),
    ];
    if let Some(path) = candidates.iter().find(|path| path.is_file()) {
        println!("Shizuku rish found at {}", path.display());
        if !dry_run {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::metadata(path)?.permissions();
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(permissions.mode() & !0o022),
            )?;
        }
    } else {
        eprintln!("WARNING: rish not found; copy it to ~/bin/rish and chmod 700 it.");
    }
    Ok(())
}
