//! HTTP/REST for Event Normalizer

use crate::normalizer::Normalizer;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use jarvis_core::{EventSource, UnifiedEvent};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::instrument;

#[derive(Deserialize, Debug)]
struct EmitEventRequest {
    source: String,
    event_type: String,
    payload: serde_json::Value,
    correlation_id: Option<String>,
}

#[derive(Serialize, Debug)]
struct EmitEventResponse {
    success: bool,
    event_id: String,
}

pub fn router(normalizer: Arc<Normalizer>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/events", post(emit_event))
        .with_state(normalizer)
}

async fn health() -> &'static str {
    "OK"
}

async fn metrics() -> String {
    // Simplified metrics endpoint - in production would use proper Prometheus exporter
    "# JARVIS Event Normalizer metrics placeholder\n".to_string()
}

#[instrument(skip(normalizer))]
async fn emit_event(
    State(normalizer): State<Arc<Normalizer>>,
    Json(req): Json<EmitEventRequest>,
) -> Json<EmitEventResponse> {
    let event = UnifiedEvent {
        id: uuid::Uuid::new_v4(),
        source: match req.source.as_str() {
            "home_assistant" => EventSource::HomeAssistant,
            "android" => EventSource::Android,
            "gmail" => EventSource::Gmail,
            "calendar" => EventSource::Calendar,
            "web" => EventSource::Web,
            "finance" => EventSource::Finance,
            _ => EventSource::Manual,
        },
        event_type: req.event_type,
        payload: req.payload,
        timestamp: chrono::Utc::now(),
        correlation_id: req
            .correlation_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    };
    normalizer.emit(event.clone());
    Json(EmitEventResponse {
        success: true,
        event_id: event.id.to_string(),
    })
}

pub async fn serve(normalizer: Arc<Normalizer>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(normalizer);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
