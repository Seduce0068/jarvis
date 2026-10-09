//! Allowlist management for MCP tools

use std::collections::HashSet;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AllowListConfig {
    pub tools: Vec<AllowedTool>,
    pub principals: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowedTool {
    pub name: String,
    pub description: String,
    pub allowed_principals: Vec<String>,
    pub max_args_size: usize,
    pub timeout_ms: u64,
    pub requires_approval: bool,
}

pub struct AllowList {
    tools: std::sync::RwLock<HashSet<String>>,
    tool_configs: std::sync::RwLock<std::collections::HashMap<String, AllowedTool>>,
    allowed_principals: std::sync::RwLock<HashSet<String>>,
}

impl AllowList {
    pub async fn new(config: &crate::config::McpGatewayConfig) -> Result<Self> {
        let content = fs::read_to_string(&config.allowlist_path).await.unwrap_or_default();
        let allowlist_config: AllowListConfig = toml::from_str(&content).unwrap_or_default();

        let mut tools = HashSet::new();
        let mut tool_configs = std::collections::HashMap::new();
        for tool in allowlist_config.tools {
            tools.insert(tool.name.clone());
            tool_configs.insert(tool.name.clone(), tool);
        }

        Ok(Self {
            tools: std::sync::RwLock::new(tools),
            tool_configs: std::sync::RwLock::new(tool_configs),
            allowed_principals: std::sync::RwLock::new(allowlist_config.principals),
        })
    }

    pub fn check(&self, tool: &str) -> Result<()> {
        if !self.tools.read().unwrap().contains(tool) {
            return Err(crate::error::McpGatewayError::NotAllowed(tool.into()).into());
        }
        Ok(())
    }

    pub fn get_config(&self, tool: &str) -> Option<AllowedTool> {
        self.tool_configs.read().unwrap().get(tool).cloned()
    }

    pub fn is_principal_allowed(&self, principal: &str) -> bool {
        self.allowed_principals.read().unwrap().contains(principal)
    }

    pub fn add_tool(&self, tool: AllowedTool) {
        self.tools.write().unwrap().insert(tool.name.clone());
        self.tool_configs.write().unwrap().insert(tool.name.clone(), tool);
    }

    pub fn remove_tool(&self, tool: &str) {
        self.tools.write().unwrap().remove(tool);
        self.tool_configs.write().unwrap().remove(tool);
    }
}