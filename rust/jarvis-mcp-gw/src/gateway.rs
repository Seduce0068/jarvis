//! Gateway core - orchestrates MCP tool execution

use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

#[allow(dead_code)]
pub struct Gateway {
    config: crate::config::McpGatewayConfig,
    allowlist: Arc<crate::allowlist::AllowList>,
    sandbox: Arc<crate::sandbox::Sandbox>,
    // MCP server clients registered via register_mcp_server
    // For now we use opencode CLI via sandbox for tool execution
}

impl Gateway {
    pub async fn new(
        config: crate::config::McpGatewayConfig,
        allowlist: Arc<crate::allowlist::AllowList>,
        sandbox: Arc<crate::sandbox::Sandbox>,
    ) -> Result<Self> {
        Ok(Self {
            config,
            allowlist,
            sandbox,
        })
    }

    pub async fn execute(&self, tool: &str, args: Value) -> Result<Value> {
        let tool_config = self
            .allowlist
            .get_config(tool)
            .ok_or_else(|| anyhow::anyhow!("Tool config not found: {}", tool))?;

        // Execute in sandbox
        self.sandbox.execute(tool, args, &tool_config).await
    }

    pub async fn register_mcp_server(&self, _name: &str, _command: Vec<String>) -> Result<()> {
        // TODO: Implement MCP server registration with rmcp client
        // For now, we rely on opencode's built-in MCP client via sandbox execution
        Ok(())
    }
}
