//! HTTP/REST for Event Normalizer

use axum::{Router, routing::{get, post}, Json, extract::State};
use std::sync::Arc;
use crate::Normalizer;
use serde::{Deserialize, Serialize};
use tracing::instrument;

#[derive(Deserialize)]
struct EmitEventRequest {
    source: String,
    event_type: String,
    payload: serde_json::Value,
    correlation_id: Option<String>,
}

#[derive(Serialize)]
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
    let encoder = metrics_exporter_prometheus::PrometheusBuilder::new().build_encoder();
    encoder.encode_to_string()
}

#[instrument(skip(normalizer))]
async fn emit_event(
    State(normalizer): State<Arc<Normalizer>>,
    Json(req): Json<EmitEventRequest>
) -> Json<EmitEventResponse> {
    let event = crate::normalizer::UnifiedEvent {
        id: uuid::Uuid::new_v4(),
        source: match req.source.as_str() {
            "home_assistant" => crate::normalizer::EventSource::HomeAssistant,
            "android" => crate::normalizer::EventSource::Android,
            "gmail" => crate::normalizer::EventSource::Gmail,
            "calendar" => crate::normalizer::EventSource::Calendar,
            "web" => crate::normalizer::EventSource::Web,
            "finance" => crate::normalizer::EventSource::Finance,
            _ => crate::normalizer::EventSource::Manual,
        },
        event_type: req.event_type,
        payload: req.payload,
        timestamp: chrono::Utc::now(),
        correlation_id: req.correlation_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    };
    normalizer.emit(event.clone());
    Json(EmitEventResponse { success: true, event_id: event.id.to_string() })
}

pub async fn serve(normalizer: Arc<Normalizer>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(normalizer);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}