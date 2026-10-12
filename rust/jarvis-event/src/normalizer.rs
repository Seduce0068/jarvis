//! Event normalization core

use anyhow::Result;
use axum::{Json, Router, extract::State, routing::post};
use jarvis_core::{EventSource, UnifiedEvent};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, instrument};

#[allow(dead_code)]
pub struct Normalizer {
    config: crate::config::EventConfig,
    ha_client: Arc<crate::sources::HomeAssistantClient>,
    android_client: Arc<crate::sources::AndroidClient>,
    policy_client: Arc<jarvis_policy::PolicyEngine>,
    mcp_gw_client: Arc<jarvis_mcp_gw::McpGateway>,
    context_client: Arc<jarvis_context::ContextBuilder>,
    event_tx: mpsc::UnboundedSender<UnifiedEvent>,
}

impl Normalizer {
    pub async fn new(
        config: crate::config::EventConfig,
        ha_client: Arc<crate::sources::HomeAssistantClient>,
        android_client: Arc<crate::sources::AndroidClient>,
        policy_client: Arc<jarvis_policy::PolicyEngine>,
        mcp_gw_client: Arc<jarvis_mcp_gw::McpGateway>,
        context_client: Arc<jarvis_context::ContextBuilder>,
    ) -> Result<Self> {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let normalizer = Self {
            config: config.clone(),
            ha_client,
            android_client,
            policy_client: policy_client.clone(),
            mcp_gw_client: mcp_gw_client.clone(),
            context_client: context_client.clone(),
            event_tx,
        };

        // Spawn event processor - clone Arcs for the spawned task
        let policy_client = policy_client.clone();
        let mcp_gw_client = mcp_gw_client.clone();
        let context_client = context_client.clone();

        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let Err(e) =
                    Self::process_event(&policy_client, &mcp_gw_client, &context_client, event)
                        .await
                {
                    tracing::error!("Event processing failed: {}", e);
                }
            }
        });

        Ok(normalizer)
    }

    pub fn emit(&self, event: UnifiedEvent) {
        let _ = self.event_tx.send(event);
    }

    async fn process_event(
        policy_client: &jarvis_policy::PolicyEngine,
        _mcp_gw_client: &jarvis_mcp_gw::McpGateway,
        context_client: &jarvis_context::ContextBuilder,
        event: UnifiedEvent,
    ) -> Result<()> {
        info!(
            "Processing event: {} from {:?}",
            event.event_type, event.source
        );

        // Build context from memory
        let context = context_client.build_context(&event).await?;

        // Check policy for automated response
        let should_act = policy_client
            .is_allowed("jarvis", "auto_respond", &event.event_type, &context)
            .await?;

        if should_act {
            // Execute via MCP Gateway
            // mcp_gw_client.execute_tool(...).await?;
        }

        Ok(())
    }

    pub async fn start_webhook_server(this: Arc<Self>) -> Result<()> {
        let app = Router::new()
            .route("/webhook/ha", post(ha_webhook))
            .route("/webhook/android", post(android_webhook))
            .route("/webhook/gmail", post(gmail_webhook))
            .route("/webhook/calendar", post(calendar_webhook))
            .with_state(this.clone());

        let listener = tokio::net::TcpListener::bind(&this.config.webhook_addr).await?;
        info!("Webhook server listening on {}", this.config.webhook_addr);
        axum::serve(listener, app).await?;
        Ok(())
    }
}

#[instrument(skip(state))]
async fn ha_webhook(State(state): State<Arc<Normalizer>>, Json(payload): Json<Value>) {
    let event = UnifiedEvent {
        id: uuid::Uuid::new_v4(),
        source: EventSource::HomeAssistant,
        event_type: payload["event_type"].as_str().unwrap_or("unknown").into(),
        payload,
        timestamp: chrono::Utc::now(),
        correlation_id: uuid::Uuid::new_v4().to_string(),
    };
    state.emit(event);
}

#[instrument(skip(state))]
async fn android_webhook(State(state): State<Arc<Normalizer>>, Json(payload): Json<Value>) {
    let event = UnifiedEvent {
        id: uuid::Uuid::new_v4(),
        source: EventSource::Android,
        event_type: payload["type"].as_str().unwrap_or("unknown").into(),
        payload,
        timestamp: chrono::Utc::now(),
        correlation_id: uuid::Uuid::new_v4().to_string(),
    };
    state.emit(event);
}

#[instrument(skip(state))]
async fn gmail_webhook(State(state): State<Arc<Normalizer>>, Json(payload): Json<Value>) {
    let event = UnifiedEvent {
        id: uuid::Uuid::new_v4(),
        source: EventSource::Gmail,
        event_type: "new_email".into(),
        payload,
        timestamp: chrono::Utc::now(),
        correlation_id: uuid::Uuid::new_v4().to_string(),
    };
    state.emit(event);
}

#[instrument(skip(state))]
async fn calendar_webhook(State(state): State<Arc<Normalizer>>, Json(payload): Json<Value>) {
    let event = UnifiedEvent {
        id: uuid::Uuid::new_v4(),
        source: EventSource::Calendar,
        event_type: "event_update".into(),
        payload,
        timestamp: chrono::Utc::now(),
        correlation_id: uuid::Uuid::new_v4().to_string(),
    };
    state.emit(event);
}
