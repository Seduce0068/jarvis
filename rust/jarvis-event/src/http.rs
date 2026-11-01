//! HTTP/REST for Event Normalizer

use crate::normalizer::Normalizer;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use jarvis_core::{EventSource, UnifiedEvent};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use tracing::instrument;

/// Handle for the process-wide Prometheus recorder.
static METRICS_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

/// Install the Prometheus recorder exactly once per process.
pub fn install_metrics() -> anyhow::Result<()> {
    let handle = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;
    if METRICS_HANDLE.set(handle).is_err() {
        tracing::warn!("metrics recorder already installed; keeping the first handle");
    }
    Ok(())
}

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
        .layer(axum::middleware::from_fn(track_metrics))
}

/// Count every HTTP request so `/metrics` exposes more than an empty page.
async fn track_metrics(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    metrics::counter!(
        "jarvis_http_requests_total",
        "service" => "jarvis-event",
        "status" => status_label(response.status()),
    )
    .increment(1);
    response
}

fn status_label(status: axum::http::StatusCode) -> &'static str {
    match status.as_u16() {
        200 => "200",
        201 => "201",
        204 => "204",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        409 => "409",
        500 => "500",
        _ => "other",
    }
}

async fn health() -> &'static str {
    "OK"
}

async fn metrics() -> String {
    match METRICS_HANDLE.get() {
        Some(handle) => handle.render(),
        None => "# metrics recorder not installed\n".to_string(),
    }
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
