//! Configuration for Policy Engine

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

/// Placeholder kill-switch admin token used until a real secret is injected.
/// The opt-in strict guard rejects this value at startup.
pub const PLACEHOLDER_KILL_SWITCH_TOKEN: &str = "changeme";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub cedar_policy_dir: String,
    pub casbin_model_path: String,
    pub casbin_policy_path: String,
    pub kill_switch_admin_token: String,
    /// Optional JSON file used to persist kill-switch state across restarts.
    /// Unset keeps the switch in memory (tests, ephemeral runs).
    pub kill_switch_state_path: Option<String>,
    pub approval_timeout_secs: u64,
    /// Upper bound on simultaneously pending approval requests.
    #[serde(default = "default_approval_max_pending")]
    pub approval_max_pending: usize,
    /// Optional bearer token for the HTTP approval endpoints. When unset
    /// (the default), behavior is unchanged; set it before exposing the
    /// listener beyond loopback.
    #[serde(default)]
    pub approval_api_token: Option<String>,
    /// Optional separate token for the approve/deny routes only. When set,
    /// decisions must present this value in `X-Jarvis-Approval-Token` while
    /// request/list/status keep using `approval_api_token`. This prevents a
    /// caller holding the gateway API token from approving its own request.
    /// Unset (default) keeps the legacy single-token behavior.
    #[serde(default)]
    pub approval_admin_token: Option<String>,
    /// Opt-in P0-1 guard: when true, approval endpoints fail closed with 503
    /// while `approval_api_token` is unset. Default false keeps the legacy
    /// loopback-only, unauthenticated behavior.
    #[serde(default)]
    pub require_approval_api_token: bool,
    /// Opt-in P0-10 guard: when true, startup fails if the kill-switch admin
    /// token is still `PLACEHOLDER_KILL_SWITCH_TOKEN`. Default false keeps the
    /// legacy warn-only behavior.
    #[serde(default)]
    pub require_kill_switch_admin_token: bool,
    pub default_deny: bool,
}

fn default_approval_max_pending() -> usize {
    crate::approval::DEFAULT_MAX_PENDING
}

impl PolicyConfig {
    pub fn load() -> Result<Self> {
        let config: PolicyConfig = Figment::new()
            .merge(Toml::file("config/policy.toml"))
            // Flat snake_case fields: do NOT use figment's Env split, which
            // would turn JARVIS_POLICY_GRPC_ADDR into the nested key `grpc.addr`.
            .merge(Env::prefixed("JARVIS_POLICY_"))
            .extract()?;
        Ok(config)
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50051".into(),
            http_addr: "127.0.0.1:8081".into(),
            cedar_policy_dir: "/etc/jarvis/policy".into(),
            casbin_model_path: "/etc/jarvis/policy/casbin_model.conf".into(),
            casbin_policy_path: "/etc/jarvis/policy/casbin_policy.csv".into(),
            kill_switch_admin_token: PLACEHOLDER_KILL_SWITCH_TOKEN.into(),
            kill_switch_state_path: None,
            approval_timeout_secs: 300,
            approval_max_pending: default_approval_max_pending(),
            approval_api_token: None,
            approval_admin_token: None,
            require_approval_api_token: false,
            require_kill_switch_admin_token: false,
            default_deny: true,
        }
    }
}

impl PolicyConfig {
    pub fn kill_switch_admin_token(&self) -> &str {
        &self.kill_switch_admin_token
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::{Format, Serialized};

    /// Config files written before the pending-approval cap was introduced
    /// must still load (the MCP-1 approval flow depends on this).
    #[test]
    fn legacy_config_without_cap_uses_default() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("legacy config parses");
        assert_eq!(config.approval_max_pending, 100);
    }

    /// `default_deny` is kept for config compatibility; authorization is
    /// always deny-by-default regardless of this value.
    #[test]
    fn default_deny_is_kept_for_compatibility() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = false
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("parses");
        assert!(!config.default_deny); // parsed, but ignored by the engine
    }

    #[test]
    fn explicit_cap_is_honoured() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
approval_max_pending = 7
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert_eq!(config.approval_max_pending, 7);
    }

    /// The strict startup guard is opt-in: configs written before P0-10 must
    /// keep the legacy warn-only behavior without any config change.
    #[test]
    fn strict_kill_switch_guard_defaults_to_false() {
        assert!(!PolicyConfig::default().require_kill_switch_admin_token);

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("legacy config parses");
        assert!(!config.require_kill_switch_admin_token);
    }

    #[test]
    fn strict_kill_switch_guard_can_be_enabled() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
require_kill_switch_admin_token = true
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert!(config.require_kill_switch_admin_token);
    }

    /// `PolicyConfig::load()` reads the P0-10 flag via
    /// `Env::prefixed("JARVIS_POLICY_")`: figment strips the prefix and
    /// lowercases the remainder, so
    /// `JARVIS_POLICY_REQUIRE_KILL_SWITCH_ADMIN_TOKEN` becomes
    /// `require_kill_switch_admin_token`.
    ///
    /// Edition 2024 makes `std::env::set_var` unsafe, and mutating the process
    /// environment would race with other tests. The same provider chain is
    /// exercised here with the post-conversion key supplied by a `Serialize`
    /// provider, so no OS env var is read or written by this test.
    #[test]
    fn env_flag_name_maps_to_strict_guard_field() {
        const ENV_PREFIX: &str = "JARVIS_POLICY_";
        const ENV_FLAG: &str = "JARVIS_POLICY_REQUIRE_KILL_SWITCH_ADMIN_TOKEN";

        let field = ENV_FLAG
            .strip_prefix(ENV_PREFIX)
            .expect("flag uses the policy env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "require_kill_switch_admin_token");

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        // Mirrors `load()`: the env provider stays in the chain, while the
        // Serialized value is merged last so the assertion cannot depend on
        // any value set in the test runner's environment.
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .merge(Env::prefixed(ENV_PREFIX).only(&[field.as_str()]))
            .merge(Serialized::default(field.as_str(), true))
            .extract()
            .expect("env-style key merges into the strict guard field");
        assert!(config.require_kill_switch_admin_token);
    }

    /// The P0-1 approval-token guard is opt-in: configs written before it was
    /// introduced keep the loopback-only, unauthenticated behavior.
    #[test]
    fn approval_token_guard_defaults_to_false() {
        assert!(!PolicyConfig::default().require_approval_api_token);

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("legacy config parses");
        assert!(!config.require_approval_api_token);
    }

    #[test]
    fn approval_token_guard_can_be_enabled() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
require_approval_api_token = true
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert!(config.require_approval_api_token);
    }

    /// Legacy configs have no admin token: the field defaults to `None`, so
    /// approve/deny keep using the single `approval_api_token`.
    #[test]
    fn approval_admin_token_defaults_to_none_for_legacy_config() {
        assert!(PolicyConfig::default().approval_admin_token.is_none());

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("legacy config parses");
        assert!(config.approval_admin_token.is_none());
    }

    /// `JARVIS_POLICY_APPROVAL_ADMIN_TOKEN` becomes `approval_admin_token`
    /// through the same `Env::prefixed("JARVIS_POLICY_")` chain used by
    /// `load()`. As in the other env test, no OS env var is mutated.
    #[test]
    fn approval_admin_token_env_key_maps_to_field() {
        const ENV_PREFIX: &str = "JARVIS_POLICY_";
        const ENV_KEY: &str = "JARVIS_POLICY_APPROVAL_ADMIN_TOKEN";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the policy env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "approval_admin_token");

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .merge(Env::prefixed(ENV_PREFIX).only(&[field.as_str()]))
            .merge(Serialized::default(field.as_str(), "admin-secret"))
            .extract()
            .expect("env-style key merges into the admin token field");
        assert_eq!(config.approval_admin_token.as_deref(), Some("admin-secret"));
        // The two tokens stay independent.
        assert!(config.approval_api_token.is_none());
    }

    #[test]
    fn approval_admin_token_can_be_set_in_toml() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
approval_api_token = "gateway-api-token"
approval_admin_token = "admin-secret"
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert_eq!(
            config.approval_api_token.as_deref(),
            Some("gateway-api-token")
        );
        assert_eq!(config.approval_admin_token.as_deref(), Some("admin-secret"));
    }
}
