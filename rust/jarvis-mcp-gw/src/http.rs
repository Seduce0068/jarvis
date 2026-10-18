//! HTTP/REST for MCP Gateway

use crate::McpGateway;
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::instrument;

#[derive(Debug, Deserialize)]
struct ExecuteToolRequest {
    tool: String,
    args: serde_json::Value,
    principal: String,
}

#[derive(Serialize, Debug)]
struct ExecuteToolResponse {
    result: serde_json::Value,
}

pub fn router(gateway: Arc<McpGateway>) -> Router {
    // OpenCode connects to this endpoint as a remote MCP server
    // (`oauth: false`); the principal is fixed server-side.
    let mcp_server = crate::mcp_server::JarvisMcpServer::new(
        gateway.clone(),
        gateway.mcp_principal().to_string(),
    );
    let mcp_service: StreamableHttpService<crate::mcp_server::JarvisMcpServer> =
        StreamableHttpService::new(
            move || Ok(mcp_server.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );

    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/execute", post(execute_tool))
        .nest_service("/mcp", mcp_service)
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
    Json(req): Json<ExecuteToolRequest>,
) -> Result<Json<ExecuteToolResponse>, axum::http::StatusCode> {
    let result = gateway
        .execute_tool(&req.tool, req.args, &req.principal)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ExecuteToolResponse { result }))
}

pub async fn serve(gateway: Arc<McpGateway>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(gateway);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
