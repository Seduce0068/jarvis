//! JARVIS Event Normalizer - Main library

pub mod config;
pub mod error;
pub mod normalizer;
pub mod sources;
pub mod grpc;
pub mod http;

use std::sync::Arc;
use anyhow::Result;
use tracing::info;

#[derive(Clone)]
pub struct EventNormalizer {
    config: config::EventConfig,
    normalizer: Arc<normalizer::Normalizer>,
    ha_client: Arc<sources::HomeAssistantClient>,
    android_client: Arc<sources::AndroidClient>,
    policy_client: Arc<jarvis_policy::PolicyEngine>,
    mcp_gw_client: Arc<jarvis_mcp_gw::McpGateway>,
    context_client: Arc<jarvis_context::ContextBuilder>,
}

impl EventNormalizer {
    pub async fn new(config: config::EventConfig) -> Result<Self> {
        let policy_client = Arc::new(jarvis_policy::PolicyEngine::new(
            jarvis_policy::config::PolicyConfig::load()?
        ).await?);

        let mcp_gw_client = Arc::new(jarvis_mcp_gw::McpGateway::new(
            jarvis_mcp_gw::config::McpGatewayConfig::load()?,
            policy_client.clone(),
        ).await?);

        let context_client = Arc::new(jarvis_context::ContextBuilder::new(
            jarvis_context::config::ContextConfig::load()?
        ).await?);

        let ha_client = Arc::new(sources::HomeAssistantClient::new(&config).await?);
        let android_client = Arc::new(sources::AndroidClient::new(&config).await?);
        let normalizer = Arc::new(normalizer::Normalizer::new(
            config.clone(),
            ha_client.clone(),
            android_client.clone(),
            policy_client.clone(),
            mcp_gw_client.clone(),
            context_client.clone(),
        ).await?);

        Ok(Self {
            config,
            normalizer,
            ha_client,
            android_client,
            policy_client,
            mcp_gw_client,
            context_client,
        })
    }

    pub async fn serve_grpc(&self, addr: std::net::SocketAddr) -> Result<()> {
        grpc::serve(self.clone(), addr).await
    }

    pub async fn serve_http(&self, addr: std::net::SocketAddr) -> Result<()> {
        http::serve(self.clone(), addr).await
    }

    pub async fn start_webhook_server(&self) -> Result<()> {
        self.normalizer.start_webhook_server().await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let config = config::EventConfig::load()?;
    let normalizer = EventNormalizer::new(config.clone()).await?;

    info!("Starting JARVIS Event Normalizer");

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(
        normalizer.serve_grpc(grpc_addr),
        normalizer.serve_http(http_addr),
        normalizer.start_webhook_server(),
    )?;

    Ok(())
}