//! JARVIS MCP Gateway - Main library

pub mod allowlist;
pub mod config;
pub mod error;
pub mod gateway;
pub mod grpc;
pub mod http;
pub mod sandbox;

use anyhow::Result;
use std::sync::Arc;

#[derive(Clone)]
#[allow(dead_code)]
pub struct McpGateway {
    config: config::McpGatewayConfig,
    gateway: Arc<gateway::Gateway>,
    allowlist: Arc<allowlist::AllowList>,
    sandbox: Arc<sandbox::Sandbox>,
    policy_client: Arc<jarvis_policy::PolicyEngine>,
}

impl McpGateway {
    pub async fn new(
        config: config::McpGatewayConfig,
        policy_client: Arc<jarvis_policy::PolicyEngine>,
    ) -> Result<Self> {
        let allowlist = Arc::new(allowlist::AllowList::new(&config).await?);
        let sandbox = Arc::new(sandbox::Sandbox::new(&config).await?);
        let gateway = Arc::new(
            gateway::Gateway::new(config.clone(), allowlist.clone(), sandbox.clone()).await?,
        );

        Ok(Self {
            config,
            gateway,
            allowlist,
            sandbox,
            policy_client,
        })
    }

    pub async fn serve_grpc(&self, addr: std::net::SocketAddr) -> Result<()> {
        grpc::serve(self.clone(), addr).await
    }

    pub async fn serve_http(&self, addr: std::net::SocketAddr) -> Result<()> {
        http::serve(Arc::new(self.clone()), addr).await
    }

    pub async fn execute_tool(
        &self,
        tool: &str,
        args: serde_json::Value,
        principal: &str,
    ) -> Result<serde_json::Value> {
        // Check policy
        let allowed = self
            .policy_client
            .is_allowed(principal, "execute", tool, &args)
            .await?;
        if !allowed {
            return Err(anyhow::anyhow!("Policy denied: {}", tool));
        }

        // Check allowlist
        self.allowlist.check(tool)?;

        // Execute in sandbox
        self.gateway.execute(tool, args).await
    }

    pub async fn register_mcp_server(&self, name: &str, command: Vec<String>) -> Result<()> {
        self.gateway.register_mcp_server(name, command).await
    }
}
