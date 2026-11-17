//! Minimal-environment helper for child processes (P0-5).
//!
//! The gateway runs under `bws run`, so its own environment may contain
//! `BWS_ACCESS_TOKEN` and other secrets. Any child process (upstream MCP
//! servers, git) must receive only the variables it needs to function.

use tokio::process::Command;

/// Variables preserved for a child process. Everything else is cleared.
const ALLOWED: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR", "USER", "LOGNAME",
];

pub fn apply_minimal_env(cmd: &mut Command) {
    cmd.env_clear();
    for key in ALLOWED {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_list_has_no_secret_names() {
        for key in ALLOWED {
            let lower = key.to_ascii_lowercase();
            assert!(
                !lower.contains("token") && !lower.contains("secret") && !lower.contains("key"),
                "secret-like variable in allowlist: {key}"
            );
        }
    }
}
