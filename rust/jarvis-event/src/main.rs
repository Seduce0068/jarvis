//! Main binary entry point

use anyhow::Result;
use jarvis_event::EventNormalizer;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = jarvis_event::config::EventConfig::load()?;
    let normalizer = EventNormalizer::new(config.clone()).await?;

    info!(
        "Starting JARVIS Event Normalizer on gRPC {} HTTP {} Webhook {}",
        config.grpc_addr, config.http_addr, config.webhook_addr
    );

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(
        normalizer.serve_grpc(grpc_addr),
        normalizer.serve_http(http_addr),
        normalizer.start_webhook_server(),
    )?;

    Ok(())
}
