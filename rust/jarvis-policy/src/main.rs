//! Main binary entry point

use jarvis_policy::{PolicyEngine, config};
use std::sync::Arc;
use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = config::PolicyConfig::load()?;
    let engine = Arc::new(PolicyEngine::new(config.clone()).await?);

    info!("Starting JARVIS Policy Engine on gRPC {} HTTP {}", config.grpc_addr, config.http_addr);

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(
        engine.serve_grpc(grpc_addr),
        engine.serve_http(http_addr),
    )?;

    Ok(())
}