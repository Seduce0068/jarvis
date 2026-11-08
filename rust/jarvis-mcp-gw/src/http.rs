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
use serde_json::{Value, json};
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
        "service" => "jarvis-mcp-gw",
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

#[instrument(skip(gateway))]
async fn execute_tool(
    State(gateway): State<Arc<McpGateway>>,
    Json(req): Json<ExecuteToolRequest>,
) -> Result<Json<ExecuteToolResponse>, (axum::http::StatusCode, Json<Value>)> {
    let result = gateway
        .execute_tool(&req.tool, req.args, &req.principal)
        .await
        .map_err(http_error)?;
    Ok(Json(ExecuteToolResponse { result }))
}

/// Build a JSON error body alongside the mapped status so callers can
/// distinguish denials (403), bad arguments (400), and internal failures.
fn http_error(error: anyhow::Error) -> (axum::http::StatusCode, Json<Value>) {
    let message = error.to_string();
    (
        http_status_for(&error, &message),
        Json(json!({ "error": message })),
    )
}

fn http_status_for(error: &anyhow::Error, message: &str) -> axum::http::StatusCode {
    match error.downcast_ref::<crate::error::McpGatewayError>() {
        Some(
            crate::error::McpGatewayError::NotAllowed(_)
            | crate::error::McpGatewayError::PolicyDenied { .. }
            | crate::error::McpGatewayError::PrincipalNotAllowed { .. }
            | crate::error::McpGatewayError::ApprovalRequired { .. },
        ) => axum::http::StatusCode::FORBIDDEN,
        _ if is_client_error(message) => axum::http::StatusCode::BAD_REQUEST,
        _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Native handlers reject bad input with these message prefixes; such errors
/// are caller mistakes (400), not gateway failures (500).
fn is_client_error(message: &str) -> bool {
    const PREFIXES: [&str; 19] = [
        "missing '",
        "invalid url",
        "unsupported scheme",
        "url has no host",
        "credentials in URL",
        "unsupported content type",
        "response too large",
        "too many redirects",
        "refusing to fetch",
        "dns lookup",
        "file too large",
        "no native handler",
        "path escapes",
        "invalid git path",
        "path is a directory",
        "stat ",
        "read ",
        "read_dir ",
        "canonicalize ",
    ];
    PREFIXES.iter().any(|prefix| message.starts_with(prefix))
}

pub async fn serve(gateway: Arc<McpGateway>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(gateway);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpGatewayConfig;
    use crate::testing;
    use serde_json::{Value, json};

    const EXECUTE_ALLOWLIST: &str = r#"
principals = ["opencode", "jarvis"]

[[tools]]
name = "filesystem.read"
description = "Read a file"
allowed_principals = ["opencode"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = false

[[tools]]
name = "shell.execute"
description = "Execute a shell command"
allowed_principals = ["opencode"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = true
"#;

    async fn test_gateway(label: &str) -> Arc<McpGateway> {
        let dir = std::env::temp_dir().join(format!("jarvis-mcp-gw-http-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create temp dir");
        let allowlist_path = dir.join("mcp_allowlist.toml");
        tokio::fs::write(&allowlist_path, EXECUTE_ALLOWLIST)
            .await
            .expect("write allowlist");

        let config = McpGatewayConfig {
            allowlist_path: allowlist_path.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let policy = Arc::new(testing::shipped_policy_client(label).await);
        Arc::new(
            McpGateway::new(config, policy)
                .await
                .expect("gateway starts"),
        )
    }

    async fn start_server(gateway: Arc<McpGateway>) -> String {
        let app = router(gateway);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server runs");
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn execute_endpoint_routes_to_a_registered_server() {
        let gateway = test_gateway("routes").await;
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(testing::run_mock_mcp_server(
            server_side,
            "filesystem.read",
            false,
        ));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock server registers");

        let base = start_server(gateway).await;
        let response = reqwest::Client::new()
            .post(format!("{base}/v1/execute"))
            .json(&json!({
                "tool": "filesystem.read",
                "args": { "path": "/tmp/example" },
                "principal": "opencode",
            }))
            .send()
            .await
            .expect("execute request runs");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let body: Value = response.json().await.expect("execute response is JSON");
        assert_eq!(body["result"]["content"][0]["text"], "pong");
        assert_eq!(body["result"]["isError"], false);
    }

    #[tokio::test]
    async fn execute_endpoint_reports_policy_and_approval_denials() {
        let gateway = test_gateway("denials").await;
        let base = start_server(gateway).await;
        let client = reqwest::Client::new();

        // Unknown tools fail the policy check before the allowlist.
        let unknown = client
            .post(format!("{base}/v1/execute"))
            .json(&json!({
                "tool": "unknown.tool",
                "args": {},
                "principal": "opencode",
            }))
            .send()
            .await
            .expect("unknown request runs");
        assert_eq!(unknown.status(), reqwest::StatusCode::FORBIDDEN);

        // Approval-required tools are rejected until an approval flow exists.
        let approval = client
            .post(format!("{base}/v1/execute"))
            .json(&json!({
                "tool": "shell.execute",
                "args": {},
                "principal": "opencode",
            }))
            .send()
            .await
            .expect("approval request runs");
        assert_eq!(approval.status(), reqwest::StatusCode::FORBIDDEN);
    }
    #[test]
    fn client_errors_map_to_400_with_json_body() {
        let (status, body) = http_error(anyhow::anyhow!("missing 'url' argument"));
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("missing 'url'"));
        let (status, _) = http_error(anyhow::anyhow!("kaboom"));
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let (status, _) = http_error(anyhow::anyhow!(
            "refusing to fetch non-public address 127.0.0.1"
        ));
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }
}
