//! HTTP/REST for Context Builder

use crate::ContextBuilder;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use jarvis_core::{MemoryEntry, UnifiedEvent};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::instrument;

#[derive(Debug, Deserialize)]
struct BuildContextRequest {
    event_id: String,
    event_type: String,
    payload: serde_json::Value,
    correlation_id: String,
}

#[derive(Serialize, Debug)]
struct BuildContextResponse {
    context: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct StoreMemoryRequest {
    id: Option<String>,
    content: String,
    embedding: Vec<f32>,
    metadata: String,
    source: String,
    expires_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SearchMemoryRequest {
    query: String,
    limit: usize,
}

pub fn router(builder: Arc<ContextBuilder>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/context/build", post(build_context))
        .route("/v1/memory", post(store_memory))
        .route("/v1/memory/search", post(search_memory))
        .with_state(builder)
}

async fn health() -> &'static str {
    "OK"
}

async fn metrics() -> String {
    // Simplified metrics endpoint - in production would use proper Prometheus exporter
    "# JARVIS Context Builder metrics placeholder\n".to_string()
}

#[instrument(skip(builder))]
async fn build_context(
    State(builder): State<Arc<ContextBuilder>>,
    Json(req): Json<BuildContextRequest>,
) -> Result<Json<BuildContextResponse>, axum::http::StatusCode> {
    let event = UnifiedEvent {
        id: uuid::Uuid::parse_str(&req.event_id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
        source: jarvis_core::EventSource::Manual,
        event_type: req.event_type,
        payload: req.payload,
        timestamp: chrono::Utc::now(),
        correlation_id: req.correlation_id,
    };
    let context = builder
        .build_context(&event)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(BuildContextResponse { context }))
}

#[instrument(skip(builder))]
async fn store_memory(
    State(builder): State<Arc<ContextBuilder>>,
    Json(req): Json<StoreMemoryRequest>,
) -> Result<&'static str, axum::http::StatusCode> {
    let memory = MemoryEntry {
        id: req
            .id
            .and_then(|s| uuid::Uuid::parse_str(&s).ok())
            .unwrap_or_else(uuid::Uuid::new_v4),
        content: req.content,
        embedding: req.embedding,
        metadata: serde_json::from_str(&req.metadata).unwrap_or_default(),
        source: req.source,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        expires_at: req
            .expires_at
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
    };
    builder
        .store_memory(memory)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok("stored")
}

#[instrument(skip(builder))]
async fn search_memory(
    State(builder): State<Arc<ContextBuilder>>,
    Json(req): Json<SearchMemoryRequest>,
) -> Result<Json<Vec<MemoryEntry>>, axum::http::StatusCode> {
    let memories = builder
        .search_memory(&req.query, req.limit)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(memories))
}

pub async fn serve(builder: Arc<ContextBuilder>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(builder);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
