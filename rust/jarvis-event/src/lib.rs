//! JARVIS Event Normalizer - Main library

pub mod config;
pub mod error;
pub mod grpc;
pub mod http;
pub mod normalizer;
pub mod opencode;
pub mod ports;
pub mod sources;

use anyhow::Result;
use ports::{DefaultContextProvider, DefaultPolicyGate};
use std::sync::Arc;

#[derive(Clone)]
#[allow(dead_code)]
pub struct EventNormalizer {
    config: config::EventConfig,
    normalizer: Arc<normalizer::Normalizer>,
    ha_client: Arc<sources::HomeAssistantClient>,
    android_client: Arc<sources::AndroidClient>,
    policy_client: Arc<jarvis_policy::PolicyClient>,
    opencode: Arc<opencode::OpenCodeClient>,
    context_client: Arc<jarvis_context::ContextBuilder>,
}

impl EventNormalizer {
    pub async fn new(config: config::EventConfig) -> Result<Self> {
        let policy_client = Arc::new(jarvis_policy::PolicyClient::new(&config.policy_grpc_addr)?);

        let context_client = Arc::new(
            jarvis_context::ContextBuilder::new(jarvis_context::config::ContextConfig::load()?)
                .await?,
        );

        let opencode = Arc::new(opencode::OpenCodeClient::new(&config)?);

        // Default adapters preserve the existing behaviour while exposing the
        // context/policy seams to `Normalizer` as trait objects.
        let context_provider: Arc<dyn ports::ContextProvider> =
            Arc::new(DefaultContextProvider::new(context_client.clone()));
        let policy_gate: Arc<dyn ports::PolicyGate> =
            Arc::new(DefaultPolicyGate::new(policy_client.clone()));

        let ha_client = Arc::new(sources::HomeAssistantClient::new(&config).await?);
        let android_client = Arc::new(sources::AndroidClient::new(&config).await?);
        let normalizer = Arc::new(
            normalizer::Normalizer::new(
                config.clone(),
                policy_gate,
                opencode.clone(),
                context_provider,
            )
            .await?,
        );

        // Subscribe to Home Assistant state changes when a token is configured;
        // webhook ingestion keeps working regardless.
        if config.ha_token().is_empty() {
            tracing::info!("Home Assistant token not configured; WebSocket listener disabled");
        } else {
            let ha_client = ha_client.clone();
            let event_tx = normalizer.event_sender();
            tokio::spawn(async move {
                if let Err(error) = ha_client.listen_events(event_tx).await {
                    tracing::error!("Home Assistant WebSocket listener stopped: {error:#}");
                }
            });
        }

        Ok(Self {
            config,
            normalizer,
            ha_client,
            android_client,
            policy_client,
            opencode,
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
