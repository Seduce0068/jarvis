//! Configuration for Event Normalizer

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub webhook_addr: String,
    pub ha_url: String,
    pub ha_token: String,
    pub android_mcp_url: String,
    /// OpenCode server URL that receives policy-approved events.
    pub opencode_url: String,
    /// OpenCode location (working directory) used for agent sessions.
    pub opencode_location: String,
    pub policy_grpc_addr: String,
}

impl EventConfig {
    pub fn load() -> Result<Self> {
        // Flat snake_case fields: do NOT use figment's Env split, which would
        // turn JARVIS_EVENT_GRPC_ADDR into the nested key `grpc.addr`.
        let config: EventConfig = Figment::new()
            .merge(Toml::file("config/event.toml"))
            .merge(Env::prefixed("JARVIS_EVENT_"))
            .extract()?;
        Ok(config)
    }

    pub fn ha_token(&self) -> &str {
        &self.ha_token
    }
}

impl Default for EventConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50053".into(),
            http_addr: "127.0.0.1:8083".into(),
            webhook_addr: "127.0.0.1:8085".into(),
            ha_url: "http://homeassistant.local:8123".into(),
            ha_token: "".into(),
            android_mcp_url: "http://127.0.0.1:49374".into(),
            opencode_url: "http://127.0.0.1:49374".into(),
            opencode_location: "/home/jarvis/repos".into(),
            policy_grpc_addr: "http://127.0.0.1:50051".into(),
        }
    }
}
