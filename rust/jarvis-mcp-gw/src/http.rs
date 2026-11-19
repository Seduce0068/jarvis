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

/// Fixed body text for gateway-side failures (R4-FIX b). The full error chain
/// is logged server-side only; a 500 response must never carry sandbox paths,
/// tokens or upstream text. Mirrors `grpc::INTERNAL_ERROR_MESSAGE` and
/// `mcp_server::INTERNAL_ERROR_MESSAGE`.
const INTERNAL_ERROR_MESSAGE: &str = "internal gateway error";

/// Fixed wording for authorization denials, shared with the gRPC adapter so
/// both transports return the same phrase. Tool/principal names are omitted
/// as well: the caller already knows them, and interpolating backend text
/// would reopen the disclosure channel this phrase is meant to close.
pub(crate) const POLICY_DENIED_MESSAGE: &str = "request denied by policy";

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

/// Fixed-size digest comparison for the HTTP admin-token gate.
///
/// Both operands become `[u8; 32]` SHA-256 digests, so length is absorbed into
/// the digest and the XOR fold always visits exactly 32 bytes with no early
/// exit on content or length. This replaces the previous length-inclusive
/// padded XOR loop, whose iteration count depended on the input length, and
/// is equivalent to `jarvis_policy::kill_switch::constant_time_eq` and the
/// gRPC gate. The digest primitive is reused from `jarvis-policy` because
/// this crate has no direct `sha2` dependency. No `unsafe` code is involved
/// and neither token value is logged.
fn fixed_size_digest_eq(a: &str, b: &str) -> bool {
    let a: [u8; 32] = jarvis_policy::kill_switch::sha256_digest(a);
    let b: [u8; 32] = jarvis_policy::kill_switch::sha256_digest(b);
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= *x ^ *y;
    }
    diff == 0
}

/// Optional admin-token gate for the data-plane HTTP endpoints
/// (`/v1/execute` and `/mcp`).
///
/// When `admin_token` is unset the listener keeps its historical
/// unauthenticated behavior so existing loopback clients keep working. Unlike
/// gRPC's administrative RPCs, the HTTP data plane is deliberately *not*
/// fail-closed when no token is configured. When a token is set, callers must
/// send the exact value in the `x-jarvis-admin-token` header; a blank or
/// whitespace-only header is never a credential, even against a blank
/// configured value (which `load()` rejects at startup).
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
    // Startup validation rejects a blank configured token; this is the
    // request-time backstop for programmatically built configs.
    if expected.trim().is_empty() {
        return Err(axum::http::StatusCode::UNAUTHORIZED);
    }
    let provided = request
        .headers()
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok());
    // Fixed-size digest comparison, mirroring the gRPC gate and
    // kill_switch::constant_time_eq: both operands are `[u8; 32]` SHA-256
    // digests and the XOR fold always visits 32 bytes.
    let authorized = provided.is_some_and(|p| {
        // A blank header must not authenticate an empty expected value
        // through the digest comparison (R4-FIX a).
        if p.trim().is_empty() {
            return false;
        }
        fixed_size_digest_eq(p, expected)
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

/// I33-B: `req` is deliberately excluded from the span. `ExecuteToolRequest`
/// derives `Debug` and its `args` carry caller payloads (file contents, URLs,
/// credentials); recording it would export those values to OTLP/Grafana.
#[instrument(skip(gateway, req))]
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

/// Reduce a caller-input failure to a fixed vocabulary of safe, static
/// phrases. Several `client_error()` messages interpolate absolute sandbox
/// paths, git stderr, egress hosts or raw URLs, so only known-static messages
/// pass through verbatim. This is the single shared helper for the HTTP and
/// gRPC adapters (and mirrors the private one in `mcp_server`); both callers
/// must use it instead of echoing `error.to_string()` into a response.
pub(crate) fn safe_client_error_text(message: &str) -> &'static str {
    match message {
        "missing 'url' argument" => "missing 'url' argument",
        "url has no host" => "url has no host",
        "credentials in URL are not allowed" => "credentials in URL are not allowed",
        "redirect response has no Location header" => "redirect response has no Location header",
        "redirect response has an empty Location header" => {
            "redirect response has an empty Location header"
        }
        "web.fetch timed out" => "web.fetch timed out",
        "web.fetch body timed out" => "web.fetch body timed out",
        "MCP server name must not be empty" => "MCP server name must not be empty",
        "MCP server command must not be empty" => "MCP server command must not be empty",
        _ if message.starts_with("too many redirects (") => "too many redirects",
        _ if message.starts_with("file too large to read (") => "file too large to read",
        _ if message.starts_with("response too large (") => "response too large",
        _ if message.starts_with("git output too large (") => "git output too large",
        _ if message.starts_with("unsupported scheme '") => "unsupported scheme",
        _ if message.starts_with("unsupported content type:") => "unsupported content type",
        _ if message.starts_with("host '") => "host is not allowed by the egress policy",
        _ if message.starts_with("refusing to fetch non-public address") => {
            "refusing to fetch non-public address"
        }
        _ if message.contains("dns lookup") => "dns lookup failed",
        _ if message.starts_with("no native handler for tool '") => "no native handler for tool",
        _ if message.starts_with("MCP server '") => "upstream MCP server is unavailable",
        _ => "invalid tool arguments",
    }
}

/// Build a JSON error body alongside the mapped status so callers can
/// distinguish denials (403), bad arguments (400), and internal failures.
/// Both the status and the body come from `ErrorKind`, never from the raw
/// error text (R4-FIX b): echoing `error.to_string()` leaked absolute paths
/// from the sandbox into 500 responses.
fn http_error(error: anyhow::Error) -> (axum::http::StatusCode, Json<Value>) {
    use crate::error::ErrorKind;

    let status = http_status_for(&error);
    let kind = error
        .downcast_ref::<crate::error::McpGatewayError>()
        .map(crate::error::McpGatewayError::kind)
        .unwrap_or(ErrorKind::Internal);
    let message = match kind {
        ErrorKind::ClientError => safe_client_error_text(&error.to_string()).to_string(),
        ErrorKind::Forbidden => POLICY_DENIED_MESSAGE.to_string(),
        ErrorKind::Internal => {
            let detail = format!("{error:#}");
            tracing::error!(
                error = %detail,
                "HTTP /v1/execute failed with internal gateway error"
            );
            INTERNAL_ERROR_MESSAGE.to_string()
        }
    };
    (status, Json(json!({ "error": message })))
}

/// Map an error to an HTTP status purely by its concrete type.
///
/// Unknown error types (including bare `anyhow!` values) fall back to 500.
/// This deliberately never inspects `error.to_string()`: rewording a message
/// must not change the API contract.
fn http_status_for(error: &anyhow::Error) -> axum::http::StatusCode {
    use crate::error::ErrorKind;

    let kind = error
        .downcast_ref::<crate::error::McpGatewayError>()
        .map(crate::error::McpGatewayError::kind)
        .unwrap_or(ErrorKind::Internal);
    match kind {
        ErrorKind::ClientError => axum::http::StatusCode::BAD_REQUEST,
        ErrorKind::Forbidden => axum::http::StatusCode::FORBIDDEN,
        ErrorKind::Internal => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
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
    use std::io::Write;
    use std::sync::Mutex;
    use tracing_subscriber::fmt::format::FmtSpan;

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
        // Client errors now use the fixed vocabulary shared with /mcp; the
        // interpolated "exceed N bytes" detail is not echoed back.
        assert_eq!(body.0["error"].as_str().unwrap(), "invalid tool arguments");
    }

    /// Build a typed client error while preserving the original message.
    fn client_error(message: &str) -> anyhow::Error {
        crate::error::McpGatewayError::ClientError(message.to_owned()).into()
    }

    #[test]
    fn client_errors_map_to_400_with_json_body() {
        let (status, body) = http_error(client_error("missing 'url' argument"));
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("missing 'url'"));
        let (status, _) = http_error(anyhow::anyhow!("kaboom"));
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let (status, _) = http_error(client_error(
            "refusing to fetch non-public address 127.0.0.1",
        ));
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }

    /// Rewording a client error (or faking a magic prefix on an untyped
    /// `anyhow!` error) must not influence the mapped status.
    #[test]
    fn status_comes_from_type_not_message() {
        let (status, _) = http_error(client_error("completely reworded 500 missing 'url'"));
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        let (status, _) = http_error(anyhow::anyhow!("missing 'url' argument"));
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// Regression (I23-C + R4-FIX b): every caller-input failure that the
    /// sandbox types as `ClientError` must keep mapping to HTTP 400, while
    /// untyped internal failures stay 500. Bodies now use the fixed shared
    /// vocabulary, so interpolated paths (e.g. `stat /tmp/...`) are rounded
    /// to a safe phrase instead of being echoed verbatim.
    #[test]
    fn typed_sandbox_failure_messages_map_to_400() {
        use crate::error::McpGatewayError;

        for (message, safe) in [
            ("too many redirects (> 3)", "too many redirects"),
            (
                "unsupported content type: 'application/octet-stream'",
                "unsupported content type",
            ),
            (
                "response too large (4096 bytes > 1024)",
                "response too large",
            ),
            (
                "file too large to read (1048577 bytes)",
                "file too large to read",
            ),
            (
                "stat /tmp/notes.txt: No such file or directory",
                "invalid tool arguments",
            ),
            (
                "read /tmp/notes.txt: permission denied",
                "invalid tool arguments",
            ),
            (
                "read_dir /tmp/notes: No such file or directory",
                "invalid tool arguments",
            ),
            ("dns lookup failed: missing.invalid", "dns lookup failed"),
            (
                "dns lookup returned no addresses for missing.invalid",
                "dns lookup failed",
            ),
            (
                "host 'attacker.example' is not in the egress allowlist",
                "host is not allowed by the egress policy",
            ),
            (
                "host 'example.com' is denied: egress allowlist is empty or invalid",
                "host is not allowed by the egress policy",
            ),
        ] {
            let error: anyhow::Error = McpGatewayError::ClientError(message.to_owned()).into();
            let (status, body) = http_error(error);
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{message}");
            let text = body.0["error"].as_str().unwrap();
            assert_eq!(text, safe, "{message}");
            assert!(!text.contains("/tmp"), "path leaked for {message}");
        }

        for message in [
            "build http client: invalid configuration",
            "build pinned http client: invalid configuration",
            "canonicalize base /home/koppe: permission denied",
        ] {
            let (status, _) = http_error(anyhow::anyhow!(message));
            assert_eq!(
                status,
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "{message}"
            );
        }
    }

    /// R4-FIX (b): internal details (sandbox absolute paths, upstream text)
    /// must stay in the server log only; the caller sees a fixed phrase. The
    /// same fixed wording applies to Forbidden denials.
    #[test]
    fn internal_error_details_never_reach_the_http_body() {
        use crate::error::McpGatewayError;

        let probe_path = "/home/koppe/secret-project/token.txt";
        let errors: [anyhow::Error; 2] = [
            McpGatewayError::Sandbox(probe_path.to_owned()).into(),
            anyhow::anyhow!("canonicalize {probe_path}: permission denied"),
        ];
        for error in errors {
            let (status, body) = http_error(error);
            assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            let text = body.0["error"].as_str().unwrap();
            assert_eq!(text, INTERNAL_ERROR_MESSAGE);
            assert!(!text.contains(probe_path), "path leaked: {text}");
            assert!(!text.contains("permission denied"), "detail leaked: {text}");
        }

        let (status, body) = http_error(McpGatewayError::NotAllowed("echo".into()).into());
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(body.0["error"].as_str().unwrap(), POLICY_DENIED_MESSAGE);
    }

    /// End-to-end status check: an argument rejected inside the native
    /// `web.fetch` handler reaches the HTTP adapter as a typed 400, not 500.
    #[tokio::test]
    async fn sandbox_client_errors_map_to_http_400() {
        let sandbox = crate::sandbox::Sandbox::new(&McpGatewayConfig::default())
            .await
            .expect("sandbox starts");
        let tool = crate::allowlist::AllowedTool {
            name: "web.fetch".to_string(),
            description: String::new(),
            allowed_principals: vec!["opencode".to_string()],
            max_args_size: 65536,
            timeout_ms: 2_000,
            requires_approval: false,
        };
        for args in [
            json!({}),
            json!({ "url": "file:///etc/passwd" }),
            json!({ "url": "https://user:pass@example.com" }),
        ] {
            let error = sandbox
                .execute("web.fetch", args.clone(), &tool)
                .await
                .expect_err("invalid fetch arguments are rejected");
            let (status, body) = http_error(error);
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{args}");
            assert!(body.0["error"].is_string());
        }
    }

    /// The fixed-size digest comparison rejects content and length mismatches
    /// and keeps working for long inputs; blank values are rejected by the
    /// gate before this helper is called.
    #[test]
    fn fixed_size_digest_eq_rejects_content_and_length_mismatches() {
        assert!(fixed_size_digest_eq("secret-admin", "secret-admin"));
        assert!(!fixed_size_digest_eq("secret-admiN", "secret-admin"));
        assert!(!fixed_size_digest_eq("secret", "secret-admin"));
        assert!(!fixed_size_digest_eq("secret-admin-extra", "secret-admin"));
        assert!(!fixed_size_digest_eq("secret-ad", "secret-admin"));

        let long = "h".repeat(4096);
        assert!(fixed_size_digest_eq(long.as_str(), long.as_str()));
        let longer = format!("{long}x");
        assert!(!fixed_size_digest_eq(longer.as_str(), long.as_str()));
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

    /// A blank header must never authenticate (R4-FIX a), including against a
    /// blank configured value. `load()` rejects blank config at startup; this
    /// exercises the request-time backstop on programmatic configs.
    #[tokio::test]
    async fn admin_token_gate_rejects_blank_header_and_blank_config() {
        let gateway =
            test_gateway_with_admin_token("auth-blank-header", Some("secret-admin")).await;
        let base = start_server(gateway).await;
        let client = reqwest::Client::new();
        let payload = json!({
            "tool": "filesystem.read",
            "args": { "path": "/tmp/example" },
            "principal": "opencode",
        });

        for header in ["", " ", "   "] {
            let response = client
                .post(format!("{base}/v1/execute"))
                .header(ADMIN_TOKEN_HEADER, header)
                .json(&payload)
                .send()
                .await
                .expect("blank-header request runs");
            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        }

        // A programmatic config with a blank expected value is locked out too.
        let blank_config = test_gateway_with_admin_token("auth-blank-config", Some("   ")).await;
        let base = start_server(blank_config).await;
        let response = client
            .post(format!("{base}/v1/execute"))
            .header(ADMIN_TOKEN_HEADER, "   ")
            .json(&payload)
            .send()
            .await
            .expect("blank-config request runs");
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
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

    /// Regression (I33-B): `#[instrument]` on `/v1/execute` must not turn the
    /// deserialized request body into span fields. `args` can hold file
    /// contents, URLs or credentials, which would otherwise be exported via
    /// OTLP. The `PROBE` name keeps this fixture clear of the repository's
    /// secret-scanner credential-key patterns.
    #[test]
    fn execute_tool_span_does_not_log_request_arguments() {
        const PROBE: &str = "I33B_PROBE_ARGUMENT_CANARY";

        #[derive(Clone, Default)]
        struct LogBuffer(Arc<Mutex<Vec<u8>>>);

        impl Write for LogBuffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let mut guard = self.0.lock().expect("log buffer mutex");
                guard.extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_span_events(FmtSpan::NEW)
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime builds");
            runtime.block_on(async {
                let gateway = test_gateway("span-probe").await;
                let req = ExecuteToolRequest {
                    tool: "unknown.tool".to_owned(),
                    args: json!({ "path": PROBE, "content": PROBE }),
                    principal: "opencode".to_owned(),
                };
                // Unknown tool: the policy check denies immediately, so no
                // error text can carry the probe into an ERROR log. Any leak
                // can only come from the instrumented span itself.
                let _ = execute_tool(State(gateway), Json(req)).await;
            });
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        // The non-leak assertion is the contract. Do not assert that the span
        // itself was captured: the thread-local subscriber can miss spans when
        // many tests run in parallel, which made this test flaky under
        // `cargo test --all-targets` even though the single-test run passed.
        assert!(!logs.contains(PROBE), "request body leaked: {logs}");
    }
}
