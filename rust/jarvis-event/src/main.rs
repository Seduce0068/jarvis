//! Main binary entry point

mod config;
mod error;
mod normalizer;
mod sources;
mod grpc;
mod http;

use crate::EventNormalizer;
use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = config::EventConfig::load()?;
    let normalizer = EventNormalizer::new(config.clone()).await?;

    info!("Starting JARVIS Event Normalizer on gRPC {} HTTP {} Webhook {}", 
        config.grpc_addr, config.http_addr, config.webhook_addr);

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(
        normalizer.serve_grpc(grpc_addr),
        normalizer.serve_http(http_addr),
        normalizer.start_webhook_server(),
    )?;

    Ok(())
}