//! JARVIS Event Normalizer - Main library

pub mod config;
pub mod context_grpc;
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

/// Pure gate used by [`EventNormalizer::new`] to select the context backend.
/// Kept separate so the default legacy route is regression-testable without
/// constructing a PostgreSQL/Qdrant-backed `ContextBuilder`.
fn use_grpc_context(config: &config::EventConfig) -> bool {
    matches!(config.context_mode, config::ContextMode::Grpc)
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct EventNormalizer {
    config: config::EventConfig,
    normalizer: Arc<normalizer::Normalizer>,
    ha_client: Arc<sources::HomeAssistantClient>,
    android_client: Arc<sources::AndroidClient>,
    policy_client: Arc<jarvis_policy::PolicyClient>,
    opencode: Arc<opencode::OpenCodeClient>,
    /// Present only on the legacy in-process route. `grpc` mode delegates to
    /// the sidecar and must not initialize local PG/Qdrant clients.
    context_client: Option<Arc<jarvis_context::ContextBuilder>>,
}

impl EventNormalizer {
    pub async fn new(config: config::EventConfig) -> Result<Self> {
        let policy_client = Arc::new(jarvis_policy::PolicyClient::new(&config.policy_grpc_addr)?);

        let opencode = Arc::new(opencode::OpenCodeClient::new(&config)?);

        // Opt-in `grpc` mode delegates context assembly to the sidecar; the
        // default route keeps the slice-1 in-process adapter. Local PG/Qdrant
        // clients are only built on the in-process path, so the sidecar route
        // has no local database dependency.
        let context_client;
        let context_provider: Arc<dyn ports::ContextProvider>;
        if use_grpc_context(&config) {
            context_client = None;
            context_provider = Arc::new(context_grpc::GrpcContextProvider::new(
                &config.context_grpc_addr,
                config.context_grpc_token(),
            )?);
        } else {
            let client = Arc::new(
                jarvis_context::ContextBuilder::new(jarvis_context::config::ContextConfig::load()?)
                    .await?,
            );
            context_provider = Arc::new(DefaultContextProvider::new(client.clone()));
            context_client = Some(client);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// I31-B regression: with the default flag the normalizer still takes the
    /// in-process `DefaultContextProvider` route; no gRPC channel is created.
    #[test]
    fn default_flag_selects_the_in_process_context_route() {
        let config = config::EventConfig::default();
        assert_eq!(config.context_mode, config::ContextMode::InProcess);
        assert!(!use_grpc_context(&config));
    }

    #[test]
    fn grpc_context_mode_is_strictly_opt_in() {
        let mut config = config::EventConfig::default();
        assert!(!use_grpc_context(&config));
        config.context_mode = config::ContextMode::Grpc;
        assert!(use_grpc_context(&config));
    }
}
