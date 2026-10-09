//! HTTP/REST for MCP Gateway

use axum::{Router, routing::{get, post}, Json, extract::State};
use std::sync::Arc;
use crate::McpGateway;
use serde::{Deserialize, Serialize};
use tracing::instrument;

#[derive(Debug, Deserialize)]
struct ExecuteToolRequest {
    tool: String,
    args: serde_json::Value,
    principal: String,
}

#[derive(Serialize)]
struct ExecuteToolResponse {
    result: serde_json::Value,
}

pub fn router(gateway: Arc<McpGateway>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/execute", post(execute_tool))
        .with_state(gateway)
}

async fn health() -> &'static str {
    "OK"
}

async fn metrics() -> String {
    // Simplified metrics endpoint - in production would use proper Prometheus exporter
    "# JARVIS MCP Gateway metrics placeholder\n".to_string()
}

#[instrument(skip(gateway))]
async fn execute_tool(
    State(gateway): State<Arc<McpGateway>>,
    Json(req): Json<ExecuteToolRequest>
) -> Result<Json<ExecuteToolResponse>, axum::http::StatusCode> {
    let result = gateway.execute_tool(&req.tool, req.args, &req.principal).await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ExecuteToolResponse { result }))
}

pub async fn serve(gateway: Arc<McpGateway>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(gateway);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}