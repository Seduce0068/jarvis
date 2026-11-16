//! Configuration for Policy Engine

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

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
            kill_switch_admin_token: "changeme".into(),
            kill_switch_state_path: None,
            approval_timeout_secs: 300,
            approval_max_pending: default_approval_max_pending(),
            approval_api_token: None,
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
    use figment::providers::Format;

    /// Config files written before the pending-approval cap was introduced
    /// must still load (the MCP-1 approval flow depends on this).
    #[test]
    fn legacy_config_without_cap_uses_default() {
        let toml = r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "changeme"
approval_timeout_secs = 300
default_deny = true
"#;
        let config: PolicyConfig = Figment::from(Toml::string(toml))
            .extract()
            .expect("legacy config parses");
        assert_eq!(config.approval_max_pending, 100);
    }

    /// `default_deny` is kept for config compatibility; authorization is
    /// always deny-by-default regardless of this value.
    #[test]
    fn default_deny_is_kept_for_compatibility() {
        let toml = r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "changeme"
approval_timeout_secs = 300
default_deny = false
"#;
        let config: PolicyConfig = Figment::from(Toml::string(toml)).extract().expect("parses");
        assert!(!config.default_deny); // parsed, but ignored by the engine
    }

    #[test]
    fn explicit_cap_is_honoured() {
        let toml = r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "changeme"
approval_timeout_secs = 300
approval_max_pending = 7
default_deny = true
"#;
        let config: PolicyConfig = Figment::from(Toml::string(toml))
            .extract()
            .expect("config parses");
        assert_eq!(config.approval_max_pending, 7);
    }
}
