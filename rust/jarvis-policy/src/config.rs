//! Configuration for Policy Engine

use anyhow::Result;
use figment::{Figment, providers::{Toml, Env, Format}};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub cedar_policy_dir: String,
    pub casbin_model_path: String,
    pub casbin_policy_path: String,
    pub kill_switch_admin_token: String,
    pub approval_timeout_secs: u64,
    pub default_deny: bool,
}

impl PolicyConfig {
    pub fn load() -> Result<Self> {
        let config: PolicyConfig = Figment::new()
            .merge(Toml::file("config/policy.toml"))
            .merge(Env::prefixed("JARVIS_POLICY_").split("_"))
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
            casbin_model_path: "/etc/jarvis/casbin_model.conf".into(),
            casbin_policy_path: "/etc/jarvis/casbin_policy.csv".into(),
            kill_switch_admin_token: "changeme".into(),
            approval_timeout_secs: 300,
            default_deny: true,
        }
    }
}

impl PolicyConfig {
    pub fn kill_switch_admin_token(&self) -> &str {
        &self.kill_switch_admin_token
    }
}