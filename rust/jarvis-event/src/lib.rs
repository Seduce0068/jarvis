//! JARVIS Event Normalizer - Main library

pub mod config;
pub mod error;
pub mod grpc;
pub mod http;
pub mod normalizer;
pub mod opencode;
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

        let ha_client = Arc::new(sources::HomeAssistantClient::new(&config).await?);
        let android_client = Arc::new(sources::AndroidClient::new(&config).await?);
        let normalizer = Arc::new(
            normalizer::Normalizer::new(
                config.clone(),
                policy_client.clone(),
                opencode.clone(),
                context_client.clone(),
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

            // `HomeAssistantClient::listen_events` still accepts an unbounded
            // sender. Bridge it into the bounded channel using the same
            // non-blocking, warn-and-drop policy as `Normalizer::emit` until
            // that client can take `mpsc::Sender` directly.
            let (source_tx, mut source_rx) =
                tokio::sync::mpsc::unbounded_channel::<jarvis_core::UnifiedEvent>();
            tokio::spawn(async move {
                while let Some(event) = source_rx.recv().await {
                    normalizer::try_emit_event(&event_tx, event);
                }
            });
            tokio::spawn(async move {
                if let Err(error) = ha_client.listen_events(source_tx).await {
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
