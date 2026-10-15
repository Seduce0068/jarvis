/// ```cargo
/// [dependencies]
/// anyhow = "1.0"
/// ```
// Configure Git's UTF-8 and Japanese filename handling.
use anyhow::{Context, Result};
use std::process::{Command, Stdio};

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--help") {
        println!("Configure global Git UTF-8, pager, and color settings.");
        return Ok(());
    }
    for (key, value) in [
        ("i18n.commitencoding", "utf-8"),
        ("i18n.logoutputencoding", "utf-8"),
        ("core.quotepath", "false"),
        ("core.pager", "less -R"),
        ("color.ui", "auto"),
        ("color.diff", "auto"),
        ("color.status", "auto"),
        ("color.branch", "auto"),
        ("color.grep", "auto"),
        ("core.editor", "vim -c 'set fenc=utf-8'"),
    ] {
        let status = Command::new("git")
            .args(["config", "--global", key, value])
            .status()
            .with_context(|| format!("configure Git key {key}"))?;
        ensure_success(status, key)?;
    }
    show_git_config(&["--get-regexp", "^i18n\\."])?;
    show_git_config(&["--get", "core.quotepath"])?;
    show_git_config(&["--get", "core.pager"])?;
    Ok(())
}

fn show_git_config(args: &[&str]) -> Result<()> {
    println!("$ git config --global {}", args.join(" "));
    let status = Command::new("git")
        .arg("config")
        .arg("--global")
        .args(args)
        .stdout(Stdio::inherit())
        .status()
        .context("read Git configuration")?;
    ensure_success(status, "read configuration")
}

fn ensure_success(status: std::process::ExitStatus, action: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("Git configuration action failed: {action}")
    }
}
