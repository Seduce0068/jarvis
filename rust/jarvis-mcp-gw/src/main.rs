//! Main binary entry point

mod config;
mod error;
mod gateway;
mod allowlist;
mod sandbox;
mod grpc;
mod http;

use crate::McpGateway;
use std::sync::Arc;
use anyhow::Result;
use tracing::info;

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

    let gateway = Arc::new(McpGateway::new(config.clone(), policy_client).await?);

    info!("Starting JARVIS MCP Gateway on gRPC {} HTTP {}", config.grpc_addr, config.http_addr);

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(
        gateway.serve_grpc(grpc_addr),
        gateway.serve_http(http_addr),
    )?;

    Ok(())
}