//! Event normalization core

use anyhow::Result;
use axum::{Json, Router, extract::State, routing::post};
use jarvis_core::{EventSource, UnifiedEvent};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, instrument};

pub struct Normalizer {
    config: crate::config::EventConfig,
    event_tx: mpsc::UnboundedSender<UnifiedEvent>,
}

impl Normalizer {
    pub async fn new(
        config: crate::config::EventConfig,
        policy_client: Arc<jarvis_policy::PolicyClient>,
        opencode: Arc<crate::opencode::OpenCodeClient>,
        context_client: Arc<jarvis_context::ContextBuilder>,
    ) -> Result<Self> {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let normalizer = Self {
            config: config.clone(),
            event_tx,
        };

        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let Err(e) =
                    Self::process_event(&policy_client, &context_client, &opencode, event).await
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

    /// Clone of the channel sender used to submit events for processing.
    pub fn event_sender(&self) -> mpsc::UnboundedSender<UnifiedEvent> {
        self.event_tx.clone()
    }

    /// Policy-gated hand-off of a normalized event to the OpenCode agent.
    ///
    /// The gate is `auto_respond` on resource `event:<type>` for the `jarvis`
    /// principal (role `agent` in the shipped Casbin policy, which grants
    /// nothing by default). Grant an event type with:
    ///
    /// ```text
    /// p, agent, event:<type>, auto_respond
    /// ```
    ///
    /// Forwarded events run through OpenCode; every tool call the agent makes
    /// is re-authorized by the MCP gateway before it executes.
    async fn process_event(
        policy_client: &jarvis_policy::PolicyClient,
        context_client: &jarvis_context::ContextBuilder,
        opencode: &crate::opencode::OpenCodeClient,
        event: UnifiedEvent,
    ) -> Result<()> {
        info!(
            "Processing event: {} from {:?}",
            event.event_type, event.source
        );

        // Build context from memory (PII-filtered by jarvis-context)
        let context = context_client.build_context(&event).await?;

        // Check policy for automated response
        let resource = format!("event:{}", event.event_type);
        // `is_allowed` fails closed (returns false on transport errors), so
        // there is no `?` here: an unreachable policy service must not
        // forward the event.
        let should_act = policy_client
            .is_allowed("jarvis", "auto_respond", &resource, &context)
            .await;

        if !should_act {
            tracing::debug!(
                event_type = %event.event_type,
                "auto_respond not granted; event not forwarded to the agent"
            );
            return Ok(());
        }

        // Hand the event to the agent runtime; tool execution stays behind
        // the MCP gateway and its policy enforcement.
        opencode.dispatch(&event, &context).await?;
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

/// Pure payload -> `UnifiedEvent` conversion for Home Assistant webhooks.
///
/// Non-deterministic fields (id, timestamp, correlation id) are injected by
/// the caller so the conversion itself is deterministic and testable. The
/// webhook handler passes freshly generated values, preserving its behaviour.
fn ha_webhook_event(
    payload: Value,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    correlation_id: String,
) -> UnifiedEvent {
    let event_type: String = payload["event_type"].as_str().unwrap_or("unknown").into();
    UnifiedEvent {
        id,
        source: EventSource::HomeAssistant,
        event_type,
        payload,
        timestamp,
        correlation_id,
    }
}

/// Pure payload -> `UnifiedEvent` conversion for Android webhooks.
fn android_webhook_event(
    payload: Value,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    correlation_id: String,
) -> UnifiedEvent {
    let event_type: String = payload["type"].as_str().unwrap_or("unknown").into();
    UnifiedEvent {
        id,
        source: EventSource::Android,
        event_type,
        payload,
        timestamp,
        correlation_id,
    }
}

/// Pure payload -> `UnifiedEvent` conversion for Gmail webhooks.
fn gmail_webhook_event(
    payload: Value,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    correlation_id: String,
) -> UnifiedEvent {
    UnifiedEvent {
        id,
        source: EventSource::Gmail,
        event_type: "new_email".into(),
        payload,
        timestamp,
        correlation_id,
    }
}

/// Pure payload -> `UnifiedEvent` conversion for Calendar webhooks.
fn calendar_webhook_event(
    payload: Value,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    correlation_id: String,
) -> UnifiedEvent {
    UnifiedEvent {
        id,
        source: EventSource::Calendar,
        event_type: "event_update".into(),
        payload,
        timestamp,
        correlation_id,
    }
}

#[instrument(skip(state))]
async fn ha_webhook(State(state): State<Arc<Normalizer>>, Json(payload): Json<Value>) {
    let event = ha_webhook_event(
        payload,
        uuid::Uuid::new_v4(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4().to_string(),
    );
    state.emit(event);
}

#[instrument(skip(state))]
async fn android_webhook(State(state): State<Arc<Normalizer>>, Json(payload): Json<Value>) {
    let event = android_webhook_event(
        payload,
        uuid::Uuid::new_v4(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4().to_string(),
    );
    state.emit(event);
}

#[instrument(skip(state))]
async fn gmail_webhook(State(state): State<Arc<Normalizer>>, Json(payload): Json<Value>) {
    let event = gmail_webhook_event(
        payload,
        uuid::Uuid::new_v4(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4().to_string(),
    );
    state.emit(event);
}

#[instrument(skip(state))]
async fn calendar_webhook(State(state): State<Arc<Normalizer>>, Json(payload): Json<Value>) {
    let event = calendar_webhook_event(
        payload,
        uuid::Uuid::new_v4(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4().to_string(),
    );
    state.emit(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixed_id() -> uuid::Uuid {
        uuid::Uuid::from_u128(1)
    }

    fn fixed_timestamp() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp")
    }

    fn fixed_correlation_id() -> String {
        "corr-test".to_string()
    }

    /// The generated values are supplied by the caller and must reach the
    /// event unchanged; `process_event` is client-dependent and not exercised.
    fn assert_injected_fields(event: &UnifiedEvent) {
        assert_eq!(event.id, fixed_id());
        assert_eq!(event.timestamp, fixed_timestamp());
        assert_eq!(event.correlation_id, fixed_correlation_id());
    }

    #[test]
    fn ha_webhook_event_uses_event_type_field_and_keeps_payload() {
        let payload = json!({
            "event_type": "state_changed",
            "data": {"entity_id": "light.kitchen", "new_state": "on"},
        });
        let event = ha_webhook_event(
            payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );

        assert_injected_fields(&event);
        assert_eq!(event.source, EventSource::HomeAssistant);
        assert_eq!(event.event_type, "state_changed");
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn ha_webhook_event_defaults_to_unknown_for_missing_or_non_string_event_type() {
        let missing_payload = json!({"data": {"entity_id": "light.kitchen"}});
        let missing = ha_webhook_event(
            missing_payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(missing.source, EventSource::HomeAssistant);
        assert_eq!(missing.event_type, "unknown");
        assert_eq!(missing.payload, missing_payload);

        let non_string = ha_webhook_event(
            json!({"event_type": 42}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(non_string.event_type, "unknown");
    }

    #[test]
    fn android_webhook_event_uses_type_field_and_keeps_payload() {
        let payload = json!({
            "type": "device_update",
            "device": {"id": "phone-1", "battery": 81},
        });
        let event = android_webhook_event(
            payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );

        assert_injected_fields(&event);
        assert_eq!(event.source, EventSource::Android);
        assert_eq!(event.event_type, "device_update");
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn android_webhook_event_defaults_to_unknown_for_missing_or_non_string_type() {
        let missing = android_webhook_event(
            json!({"event_type": "not_the_type_field"}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(missing.source, EventSource::Android);
        assert_eq!(missing.event_type, "unknown");

        let non_string = android_webhook_event(
            json!({"type": true}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(non_string.event_type, "unknown");
    }

    #[test]
    fn gmail_webhook_event_uses_fixed_type_and_ignores_payload_type_hints() {
        let payload = json!({
            "event_type": "ignored",
            "type": "also_ignored",
            "message": {"id": "msg-1"},
        });
        let event = gmail_webhook_event(
            payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );

        assert_injected_fields(&event);
        assert_eq!(event.source, EventSource::Gmail);
        assert_eq!(event.event_type, "new_email");
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn calendar_webhook_event_uses_fixed_type_and_ignores_payload_type_hints() {
        let payload = json!({
            "event_type": "ignored",
            "type": "also_ignored",
            "calendar": {"id": "cal-1"},
        });
        let event = calendar_webhook_event(
            payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );

        assert_injected_fields(&event);
        assert_eq!(event.source, EventSource::Calendar);
        assert_eq!(event.event_type, "event_update");
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn fixed_type_webhooks_accept_an_empty_payload() {
        let gmail = gmail_webhook_event(
            json!({}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(gmail.event_type, "new_email");
        assert_eq!(gmail.payload, json!({}));

        let calendar = calendar_webhook_event(
            json!({}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(calendar.event_type, "event_update");
        assert_eq!(calendar.payload, json!({}));
    }
}
