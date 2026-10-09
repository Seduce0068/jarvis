//! Configuration for MCP Gateway

use std::net::SocketAddr;
use anyhow::Result;
use figment::{Figment, providers::{Toml, Env}};
use serde::{Deserialize, Serialize};
use secrecy::SecretString;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpGatewayConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub allowlist_path: String,
    pub sandbox_type: SandboxType,
    pub sandbox_config: serde_json::Value,
    pub policy_grpc_addr: String,
}

impl McpGatewayConfig {
    pub fn load() -> Result<Self> {
        let config: McpGatewayConfig = Figment::new()
            .merge(Toml::file("config/mcp_gateway.toml"))
            .merge(Env::prefixed("JARVIS_MCP_GW_").split("_"))
            .extract()?;
        Ok(config)
    }
}

impl Default for McpGatewayConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50052".into(),
            http_addr: "127.0.0.1:8082".into(),
            allowlist_path: "/etc/jarvis/mcp_allowlist.toml".into(),
            sandbox_type: SandboxType::None,
            sandbox_config: serde_json::json!({}),
            policy_grpc_addr: "http://127.0.0.1:50051".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxType {
    None,
    Firejail,
    Bwrap,
    Docker,
    Gvisor,
}