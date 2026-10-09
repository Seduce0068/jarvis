//! Configuration for Event Normalizer

use anyhow::Result;
use figment::{Figment, providers::{Toml, Env}};
use serde::{Deserialize, Serialize};
use secrecy::SecretString;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub webhook_addr: String,
    pub ha_url: String,
    pub ha_token: SecretString,
    pub android_mcp_url: String,
    pub policy_grpc_addr: String,
    pub mcp_gw_grpc_addr: String,
    pub context_grpc_addr: String,
}

impl EventConfig {
    pub fn load() -> Result<Self> {
        let config: EventConfig = Figment::new()
            .merge(Toml::file("config/event.toml"))
            .merge(Env::prefixed("JARVIS_EVENT_").split("_"))
            .extract()?;
        Ok(config)
    }
}

impl Default for EventConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50053".into(),
            http_addr: "127.0.0.1:8083".into(),
            webhook_addr: "0.0.0.0:8080".into(),
            ha_url: "http://homeassistant.local:8123".into(),
            ha_token: SecretString::new("".into()),
            android_mcp_url: "http://127.0.0.1:49374".into(),
            policy_grpc_addr: "http://127.0.0.1:50051".into(),
            mcp_gw_grpc_addr: "http://127.0.0.1:50052".into(),
            context_grpc_addr: "http://127.0.0.1:50054".into(),
        }
    }
}