//! JARVIS Event Normalizer - Main library

pub mod config;
pub mod error;
pub mod grpc;
pub mod http;
pub mod normalizer;
pub mod sources;

use anyhow::Result;
use std::sync::Arc;

#[derive(Clone)]
#[allow(dead_code)]
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
        let policy_client = Arc::new(
            jarvis_policy::PolicyEngine::new(jarvis_policy::config::PolicyConfig::load()?).await?,
        );

        let mcp_gw_client = Arc::new(
            jarvis_mcp_gw::McpGateway::new(
                jarvis_mcp_gw::config::McpGatewayConfig::load()?,
                policy_client.clone(),
            )
            .await?,
        );

        let context_client = Arc::new(
            jarvis_context::ContextBuilder::new(jarvis_context::config::ContextConfig::load()?)
                .await?,
        );

        let ha_client = Arc::new(sources::HomeAssistantClient::new(&config).await?);
        let android_client = Arc::new(sources::AndroidClient::new(&config).await?);
        let normalizer = Arc::new(
            normalizer::Normalizer::new(
                config.clone(),
                ha_client.clone(),
                android_client.clone(),
                policy_client.clone(),
                mcp_gw_client.clone(),
                context_client.clone(),
            )
            .await?,
        );

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
        grpc::serve(self.normalizer.clone(), addr).await
    }

    pub async fn serve_http(&self, addr: std::net::SocketAddr) -> Result<()> {
        http::serve(self.normalizer.clone(), addr).await
    }

    pub async fn start_webhook_server(&self) -> Result<()> {
        normalizer::Normalizer::start_webhook_server(self.normalizer.clone()).await
    }
}
