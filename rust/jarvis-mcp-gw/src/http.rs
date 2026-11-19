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

/// Header carrying the shared admin token on the HTTP listener. It is kept
/// distinct from the gRPC header (`x-jarvis-grpc-token`) but is backed by the
/// same `admin_token` configuration value.
const ADMIN_TOKEN_HEADER: &str = "x-jarvis-admin-token";

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

    // Only the data-plane endpoints are gated. `/health`, `/metrics` and the
    // approval flow (served by jarvis-policy under its own
    // `x-jarvis-approval-token` middleware) keep their existing scope.
    let protected = Router::new()
        .route("/v1/execute", post(execute_tool))
        .nest_service("/mcp", mcp_service)
        .layer(axum::middleware::from_fn_with_state(
            gateway.clone(),
            require_admin_token,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .merge(protected)
        .with_state(gateway)
        .layer(axum::middleware::from_fn(track_metrics))
}

/// Optional admin-token gate for the data-plane HTTP endpoints
/// (`/v1/execute` and `/mcp`).
///
/// When `admin_token` is unset the listener keeps its historical
/// unauthenticated behavior so existing loopback clients keep working. Unlike
/// gRPC's administrative RPCs, the HTTP data plane is deliberately *not*
/// fail-closed when no token is configured. When a token is set, callers must
/// send the exact value in the `x-jarvis-admin-token` header.
///
/// This gate is intentionally separate from the policy service's
/// approval-token middleware (`x-jarvis-approval-token`): the two protect
/// different hops and must not be confused.
async fn require_admin_token(
    State(gateway): State<Arc<McpGateway>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let Some(expected) = gateway.config.admin_token.as_deref() else {
        return Ok(next.run(request).await);
    };
    let provided = request
        .headers()
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok());
    // Constant-time-ish comparison, mirroring the gRPC and approval-token
    // gates: check the length first, then XOR-fold every byte so a mismatch
    // does not short-circuit on the first differing byte.
    let authorized = provided.is_some_and(|p| {
        p.len() == expected.len()
            && p.bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    });
    if !authorized {
        return Err(axum::http::StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
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
        Some(crate::error::McpGatewayError::ArgsTooLarge { .. }) => {
            axum::http::StatusCode::BAD_REQUEST
        }
        Some(
            crate::error::McpGatewayError::NotAllowed(_)
            | crate::error::McpGatewayError::PolicyDenied { .. }
            | crate::error::McpGatewayError::PrincipalNotAllowed { .. }
            | crate::error::McpGatewayError::ApprovalRequired { .. }
            | crate::error::McpGatewayError::ApprovalDenied { .. },
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
            // Unreachable by construction: tests must not reach a policy
            // service that happens to listen on the default address.
            policy_http_addr: "http://127.0.0.1:1".into(),
            ..Default::default()
        };
        let policy = Arc::new(testing::shipped_policy_client(label).await);
        Arc::new(
            McpGateway::new(config, policy)
                .await
                .expect("gateway starts"),
        )
    }

    /// Like [`test_gateway`], but with an explicit `admin_token` setting so the
    /// HTTP auth gate can be exercised in both configured and unset states.
    async fn test_gateway_with_admin_token(
        label: &str,
        admin_token: Option<&str>,
    ) -> Arc<McpGateway> {
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
            policy_http_addr: "http://127.0.0.1:1".into(),
            admin_token: admin_token.map(str::to_owned),
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

        // Approval-required tools fail closed while the approval service is
        // unreachable (the fixture points at a port nothing serves).
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

    /// Full human-in-the-loop path (MCP-1): the gateway raises an approval
    /// request against the real policy HTTP surface, the operator approves
    /// it, and the call then executes on the registered upstream server.
    #[tokio::test]
    async fn execute_endpoint_blocks_until_operator_approval() {
        let policy_http = testing::start_policy_http("execute-approval").await;
        let dir = std::env::temp_dir().join("jarvis-mcp-gw-http-execute-approval");
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
            policy_http_addr: policy_http.clone(),
            ..Default::default()
        };
        let policy = Arc::new(testing::shipped_policy_client("execute-approval").await);
        let gateway = Arc::new(
            McpGateway::new(config, policy)
                .await
                .expect("gateway starts"),
        );
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(testing::run_mock_mcp_server(
            server_side,
            "shell.execute",
            false,
        ));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock server registers");

        let base = start_server(gateway).await;
        let http = reqwest::Client::new();

        let call = tokio::spawn({
            let base = base.clone();
            async move {
                reqwest::Client::new()
                    .post(format!("{base}/v1/execute"))
                    .json(&json!({
                        "tool": "shell.execute",
                        "args": {},
                        "principal": "opencode",
                    }))
                    .send()
                    .await
                    .expect("execute request runs")
            }
        });

        // Operator approves the pending request raised by the gateway.
        let id = loop {
            let pending: Value = http
                .get(format!("{policy_http}/v1/approvals"))
                .send()
                .await
                .expect("list approvals")
                .json()
                .await
                .expect("pending list is JSON");
            if let Some(entry) = pending["pending"]
                .as_array()
                .and_then(|items| items.first())
            {
                break entry["id"].as_str().expect("id is a string").to_owned();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let decision: Value = http
            .post(format!("{policy_http}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "koppe" }))
            .send()
            .await
            .expect("approve request runs")
            .json()
            .await
            .expect("approve response is JSON");
        assert_eq!(decision["decision"], "Approved");

        let response = call.await.expect("execute call task");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("execute response is JSON");
        assert_eq!(body["result"]["isError"], false);
        assert_eq!(body["result"]["content"][0]["text"], "pong");
    }
    #[test]
    fn oversized_arguments_map_to_400() {
        let error = anyhow::Error::from(crate::error::McpGatewayError::ArgsTooLarge {
            tool: "shell.execute".into(),
            max: 10,
            actual: 20,
        });
        let (status, body) = http_error(error);
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("exceed"));
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

    /// When an admin token is configured, `/v1/execute` requires it in
    /// `x-jarvis-admin-token`. Missing/wrong values are rejected before any
    /// tool work, and neither the approval-token nor the gRPC-token header
    /// satisfies the gate.
    #[tokio::test]
    async fn admin_token_gate_requires_header_when_configured() {
        let gateway = test_gateway_with_admin_token("auth-required", Some("secret-admin")).await;
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
        let client = reqwest::Client::new();
        let payload = json!({
            "tool": "filesystem.read",
            "args": { "path": "/tmp/example" },
            "principal": "opencode",
        });

        let missing = client
            .post(format!("{base}/v1/execute"))
            .json(&payload)
            .send()
            .await
            .expect("missing-token request runs");
        assert_eq!(missing.status(), reqwest::StatusCode::UNAUTHORIZED);

        let wrong = client
            .post(format!("{base}/v1/execute"))
            .header(ADMIN_TOKEN_HEADER, "wrong-token")
            .json(&payload)
            .send()
            .await
            .expect("wrong-token request runs");
        assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

        // The HTTP approval-token gate is a different hop and must not work.
        let approval_header = client
            .post(format!("{base}/v1/execute"))
            .header("x-jarvis-approval-token", "secret-admin")
            .json(&payload)
            .send()
            .await
            .expect("approval-token request runs");
        assert_eq!(approval_header.status(), reqwest::StatusCode::UNAUTHORIZED);

        // The gRPC header is a different surface and must not work either.
        let grpc_header = client
            .post(format!("{base}/v1/execute"))
            .header("x-jarvis-grpc-token", "secret-admin")
            .json(&payload)
            .send()
            .await
            .expect("grpc-token request runs");
        assert_eq!(grpc_header.status(), reqwest::StatusCode::UNAUTHORIZED);

        // The correct header passes the gate and reaches the handler.
        let authorized = client
            .post(format!("{base}/v1/execute"))
            .header(ADMIN_TOKEN_HEADER, "secret-admin")
            .json(&payload)
            .send()
            .await
            .expect("authorized request runs");
        assert_eq!(authorized.status(), reqwest::StatusCode::OK);
    }

    /// With no admin token configured the HTTP data plane keeps its legacy
    /// unauthenticated behavior, so existing loopback clients stay compatible.
    #[tokio::test]
    async fn admin_token_gate_is_optional_when_unset() {
        let gateway = test_gateway_with_admin_token("auth-unset", None).await;
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
            .expect("unauthenticated legacy request runs");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    /// Health and metrics stay reachable without the token even when it is
    /// configured, preserving the approval-token middleware's narrow scope.
    #[tokio::test]
    async fn admin_token_gate_exempts_health_and_metrics() {
        let gateway = test_gateway_with_admin_token("auth-exempt", Some("secret-admin")).await;
        let base = start_server(gateway).await;
        let client = reqwest::Client::new();

        for path in ["/health", "/metrics"] {
            let response = client
                .get(format!("{base}{path}"))
                .send()
                .await
                .expect("exempt endpoint responds");
            assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
        }
    }

    /// The nested `/mcp` streamable-HTTP service is inside the gated scope:
    /// it is rejected without the token and reachable with it.
    #[tokio::test]
    async fn admin_token_gate_protects_mcp_endpoint() {
        let gateway = test_gateway_with_admin_token("auth-mcp", Some("secret-admin")).await;
        let base = start_server(gateway).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("client builds");

        let unauthorized = client
            .post(format!("{base}/mcp"))
            .json(&json!({}))
            .send()
            .await
            .expect("unauthenticated MCP request runs");
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        let authorized = client
            .post(format!("{base}/mcp"))
            .header(ADMIN_TOKEN_HEADER, "secret-admin")
            .json(&json!({}))
            .send()
            .await
            .expect("authenticated MCP request runs");
        assert_ne!(authorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
}
