//! JARVIS server environment diagnostic.
use std::env;
use std::path::Path;
use std::process::{Command, Stdio};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--help") {
        println!("Diagnose JARVIS server dependencies and local config files.");
        return;
    }
    let root = env::var("JARVIS_ROOT").unwrap_or_else(|_| {
        env::var("HOME")
            .map(|home| format!("{home}/jarvis"))
            .unwrap_or_else(|_| String::from("/home/jarvis"))
    });

    println!("{:<24} status", "component");
    println!("{:-<24} {:-<16}", "", "");

    report_command_version("OpenCode", "opencode", &["--version"], "installed");
    for command in ["curl", "git", "jq", "python3"] {
        report_command(command, command, "OK");
    }
    report_command("Tailscale", "tailscale", "installed");
    report_path("jarvis.env", &format!("{root}/config/jarvis.env"));
    report_path(
        "tool-policy.json",
        &format!("{root}/policies/tool-policy.json"),
    );
}

fn report_command(label: &str, command: &str, success: &str) {
    let status = if command_exists(command) {
        success
    } else if label == "Tailscale" {
        "optional / not installed"
    } else {
        "MISSING"
    };
    println!("{label:<24} {status}");
}

fn report_command_version(label: &str, command: &str, args: &[&str], fallback: &str) {
    let status = if command_exists(command) {
        Command::new(command)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|version| !version.is_empty())
            .unwrap_or_else(|| fallback.to_owned())
    } else {
        String::from("MISSING")
    };
    println!("{label:<24} {status}");
}

fn report_path(label: &str, path: &str) {
    let status = if Path::new(path).is_file() {
        "present"
    } else {
        "MISSING"
    };
    println!("{label:<24} {status}");
}

fn command_exists(command: &str) -> bool {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .any(|directory| directory.join(command).is_file())
}
