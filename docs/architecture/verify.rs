/// ```cargo
/// [dependencies]
/// anyhow = "1.0"
/// ```
// Verify the Markdown + MPE + Rust architecture-document workflow.
use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn main() -> Result<()> {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--help") {
        println!("Usage: rust-script verify.rs [ARCHITECTURE_DOCS_DIR]");
        return Ok(());
    }
    let directory = args
        .first()
        .cloned()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let directory = directory
        .canonicalize()
        .context("resolve architecture docs directory")?;
    let report_path = directory.join("report.md");
    let report = fs::read_to_string(&report_path).context("read report.md")?;
    let mut failed = false;

    let mut calculated_output: Option<String> = None;
    println!("[INFO] Running Rust calculation...");
    match Command::new("rust-script")
        .arg(directory.join("calc.rs"))
        .current_dir(&directory)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => {
            let calculated = String::from_utf8_lossy(&output.stdout).into_owned();
            println!("[INFO] Calculation completed.");
            for expected in [
                "| Service | OCPUs |",
                "| Resource | Limit | Used | Utilization |",
                "## Verdict",
            ] {
                if !calculated.contains(expected) {
                    eprintln!("[ERROR] Calculation output lacks `{expected}`");
                    failed = true;
                }
            }
            check_utilization(&calculated, &mut failed);
            calculated_output = Some(calculated);
        }
        Ok(output) => {
            eprintln!(
                "[ERROR] calc.rs failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            failed = true;
        }
        Err(error) => {
            eprintln!("[ERROR] Could not execute rust-script: {error}");
            failed = true;
        }
    }

    println!("[INFO] Checking report structure and formulas...");
    if !report.contains("@import \"calc.rs\"") {
        eprintln!("[ERROR] MPE import directive is missing");
        failed = true;
    }
    if !report.contains("$$") {
        eprintln!("[WARN] No display formulas found");
    }
    if !directory.join("dist").is_dir() {
        eprintln!("[WARN] dist/ directory is missing");
    }

    if let Some(calculated) = &calculated_output {
        println!("[INFO] Checking verdict consistency...");
        let calculation_exceeds = calculated.contains("Exceeds Always Free limits");
        match report_verdict(&report) {
            Some(report_exceeds) if report_exceeds == calculation_exceeds => {
                println!("[INFO] report.md Verdict marker is consistent with calc.rs.");
            }
            Some(_) => {
                eprintln!("[ERROR] report.md Verdict marker contradicts calc.rs output");
                failed = true;
            }
            None => {
                eprintln!(
                    "[ERROR] report.md needs a `> **Verdict**: ⚠️` or `> **Verdict**: ✅` marker"
                );
                failed = true;
            }
        }
    }

    if executable_on_path("markdownlint") {
        match Command::new("markdownlint")
            .arg("report.md")
            .current_dir(&directory)
            .status()
        {
            Ok(status) if status.success() => println!("[INFO] markdownlint passed."),
            Ok(_) => eprintln!("[WARN] markdownlint reported issues"),
            Err(error) => eprintln!("[WARN] could not run markdownlint: {error}"),
        }
    } else {
        eprintln!("[WARN] markdownlint not installed; skipped");
    }

    let git_status = Command::new("git")
        .args(["diff", "--quiet", "--", "report.md", "calc.rs"])
        .current_dir(&directory)
        .status();
    match git_status {
        Ok(status) if status.success() => println!("[INFO] No source changes detected."),
        Ok(_) => eprintln!("[WARN] report.md or calc.rs has uncommitted changes"),
        Err(error) => eprintln!("[WARN] Could not check git status: {error}"),
    }

    if failed {
        anyhow::bail!("architecture documentation verification failed");
    }
    println!("[INFO] All blocking checks passed.");
    Ok(())
}

fn check_utilization(output: &str, failed: &mut bool) {
    for field in output.split_whitespace() {
        let Some(number) = field.strip_suffix('%') else {
            continue;
        };
        let number =
            number.trim_matches(|character: char| !character.is_ascii_digit() && character != '.');
        if let Ok(value) = number.parse::<f64>() {
            if !(0.0..=200.0).contains(&value) {
                eprintln!("[ERROR] suspicious utilization {value}%");
                *failed = true;
            }
        }
    }
}

fn report_verdict(report: &str) -> Option<bool> {
    report.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("> **Verdict**:")?.trim_start();
        if rest.starts_with('⚠') {
            Some(true)
        } else if rest.starts_with('✅') {
            Some(false)
        } else {
            None
        }
    })
}

fn executable_on_path(name: &str) -> bool {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .any(|directory| directory.join(name).is_file())
}
