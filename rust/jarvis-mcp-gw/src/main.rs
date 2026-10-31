//! Main binary entry point

use anyhow::Result;
use jarvis_mcp_gw::McpGateway;
use jarvis_policy::PolicyClient;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();
    jarvis_mcp_gw::http::install_metrics()?;

    let config = jarvis_mcp_gw::config::McpGatewayConfig::load()?;
    let policy_client = Arc::new(PolicyClient::new(&config.policy_grpc_addr)?);

    let gateway = McpGateway::new(config.clone(), policy_client).await?;

    info!(
        "Starting JARVIS MCP Gateway on gRPC {} HTTP {}",
        config.grpc_addr, config.http_addr
    );

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(gateway.serve_grpc(grpc_addr), gateway.serve_http(http_addr),)?;

    Ok(())
}
