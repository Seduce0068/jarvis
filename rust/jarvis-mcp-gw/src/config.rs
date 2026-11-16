//! Configuration for MCP Gateway

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpGatewayConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub allowlist_path: String,
    pub sandbox_type: SandboxType,
    pub sandbox_config: serde_json::Value,
    pub policy_grpc_addr: String,
    /// HTTP base URL of `jarvis-policy`, used for the human approval flow
    /// (`POST /v1/approval`). Approval stays HTTP-only by design so a
    /// long-running human decision never occupies a gRPC stream.
    /// Optional in config files: older deployments fall back to the default.
    #[serde(default = "default_policy_http_addr")]
    pub policy_http_addr: String,
    /// Optional token sent as X-Jarvis-Approval-Token to jarvis-policy.
    #[serde(default)]
    pub approval_api_token: Option<String>,
    /// Principal identity used for tool calls that arrive over the MCP
    /// server endpoint (`/mcp`). Calls still pass policy and allowlist
    /// checks; the client never chooses the principal.
    pub mcp_principal: String,
}

impl McpGatewayConfig {
    pub fn load() -> Result<Self> {
        let config: McpGatewayConfig = Figment::new()
            .merge(Toml::file("config/mcp_gateway.toml"))
            // Flat snake_case fields: do NOT use figment's Env split, which
            // would turn JARVIS_MCP_GW_GRPC_ADDR into the nested key `grpc.addr`.
            .merge(Env::prefixed("JARVIS_MCP_GW_"))
            .extract()?;
        Ok(config)
    }
}

fn default_policy_http_addr() -> String {
    "http://127.0.0.1:8081".to_string()
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
            policy_http_addr: default_policy_http_addr(),
            approval_api_token: None,
            mcp_principal: "opencode".into(),
        }
    }
}

/// Sandbox type selected for the gateway.
///
/// Retained for configuration compatibility (`JARVIS_MCP_GW_SANDBOX_TYPE` /
/// `sandbox_type` in `mcp_gateway.toml` keep parsing). Execution is
/// runtime-agnostic (ADR-0002): native handlers and upstream MCP servers
/// serve all tools, and this value no longer selects an execution backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxType {
    None,
    Firejail,
    Bwrap,
    Docker,
    Gvisor,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Config files written before the MCP-1 approval flow existed do not
    /// contain `policy_http_addr`; loading them must still work (fail-open
    /// on config parsing, not on approval decisions).
    #[test]
    fn legacy_config_without_policy_http_addr_still_loads() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("legacy config parses");
        assert_eq!(config.policy_http_addr, "http://127.0.0.1:8081");
    }

    /// Config files written before the approval token was introduced must
    /// still load (the token is optional, default None).
    #[test]
    fn legacy_config_without_approval_token_still_loads() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("legacy config parses");
        assert_eq!(config.approval_api_token, None);
    }

    #[test]
    fn explicit_approval_token_is_honoured() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
approval_api_token = "test-token"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("config parses");
        assert_eq!(config.approval_api_token.as_deref(), Some("test-token"));
    }

    #[test]
    fn explicit_policy_http_addr_is_honoured() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:18081"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("config parses");
        assert_eq!(config.policy_http_addr, "http://127.0.0.1:18081");
    }
}
