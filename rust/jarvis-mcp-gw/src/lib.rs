//! JARVIS MCP Gateway - Main library

pub mod config;
pub mod error;
pub mod gateway;
pub mod allowlist;
pub mod sandbox;
pub mod grpc;
pub mod http;

use std::sync::Arc;
use anyhow::Result;
use tracing::info;

#[derive(Clone)]
pub struct McpGateway {
    config: config::McpGatewayConfig,
    gateway: Arc<gateway::Gateway>,
    allowlist: Arc<allowlist::AllowList>,
    sandbox: Arc<sandbox::Sandbox>,
    policy_client: Arc<jarvis_policy::PolicyEngine>,
}

impl McpGateway {
    pub async fn new(config: config::McpGatewayConfig, policy_client: Arc<jarvis_policy::PolicyEngine>) -> Result<Self> {
        let allowlist = Arc::new(allowlist::AllowList::new(&config).await?);
        let sandbox = Arc::new(sandbox::Sandbox::new(&config).await?);
        let gateway = Arc::new(gateway::Gateway::new(config.clone(), allowlist.clone(), sandbox.clone()).await?);

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
        http::serve(self.clone(), addr).await
    }

    pub async fn execute_tool(&self, tool: &str, args: serde_json::Value, principal: &str) -> Result<serde_json::Value> {
        // Check policy
        let allowed = self.policy_client.is_allowed(principal, "execute", tool, &args)?;
        if !allowed {
            return Err(anyhow::anyhow!("Policy denied: {}", tool));
        }

        // Check allowlist
        self.allowlist.check(tool)?;

        // Execute in sandbox
        self.gateway.execute(tool, args).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = config::McpGatewayConfig::load()?;
    let policy_client = Arc::new(jarvis_policy::PolicyEngine::new(
        jarvis_policy::config::PolicyConfig::load()?
    ).await?);

    let gateway = McpGateway::new(config.clone(), policy_client).await?;

    info!("Starting JARVIS MCP Gateway");

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(
        gateway.serve_grpc(grpc_addr),
        gateway.serve_http(http_addr),
    )?;

    Ok(())
}