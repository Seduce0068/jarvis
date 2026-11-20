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

    /// I83-C (b): the helper must really clear the command environment. A
    /// canary set *before* the call makes a removed `env_clear()` visible:
    /// `get_envs()` lists only explicitly configured variables, so checking
    /// just the allowlist keys would stay green even without the clear.
    #[test]
    fn apply_minimal_env_clears_unlisted_variables() {
        const CANARY: &str = "I83C_ENV_CLEAR_CANARY";
        let mut cmd = Command::new("true");
        cmd.env(CANARY, "must-not-survive");
        apply_minimal_env(&mut cmd);

        let envs: Vec<String> = cmd
            .as_std()
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();

        assert!(
            !envs.iter().any(|key| key.as_str() == CANARY),
            "env_clear() was removed; canary survived: {envs:?}"
        );
        for key in &envs {
            assert!(
                ALLOWED.contains(&key.as_str()),
                "unexpected child environment variable: {key}"
            );
        }
        // Allowed variables present in the service environment must still be
        // forwarded; otherwise the clear would leave the child unusable.
        if std::env::var_os("PATH").is_some() {
            assert!(
                envs.iter().any(|key| key.as_str() == "PATH"),
                "PATH was not forwarded: {envs:?}"
            );
        }
    }

    /// Static guard (I83-C b): every `Command::new` child-process spawn in the
    /// two owning modules must be paired with a call to this helper.
    /// `include_str!` checks the sibling sources at compile time, so a new
    /// bare spawn fails the test even when no child runs at test time.
    #[test]
    fn every_child_spawn_site_uses_the_minimal_env_helper() {
        for (module, source) in [
            ("gateway.rs", include_str!("gateway.rs")),
            ("sandbox.rs", include_str!("sandbox.rs")),
        ] {
            let spawns = source.matches("Command::new(").count();
            let scrubbed = source
                .matches("crate::subprocess::apply_minimal_env(")
                .count();
            assert!(spawns > 0, "{module}: no child spawn site found");
            assert_eq!(
                spawns, scrubbed,
                "{module}: {spawns} spawn site(s) but {scrubbed} apply_minimal_env call(s)"
            );
        }
    }
}
