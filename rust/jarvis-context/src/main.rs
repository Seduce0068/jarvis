//! Main binary entry point

use anyhow::Result;
use jarvis_context::ContextBuilder;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = jarvis_context::config::ContextConfig::load()?;
    let builder = ContextBuilder::new(config.clone()).await?;

    info!(
        "Starting JARVIS Context Builder on gRPC {} HTTP {}",
        config.grpc_addr, config.http_addr
    );

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(builder.serve_grpc(grpc_addr), builder.serve_http(http_addr),)?;

    Ok(())
}
