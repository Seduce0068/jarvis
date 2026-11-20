//! HTTP/REST for MCP Gateway

use crate::McpGateway;
use axum::{
    Json, Router,
    extract::State,
    response::IntoResponse,
    routing::{get, post},
};
// `INTERNAL_ERROR_MESSAGE` aliases the shared gateway failure text so HTTP,
// gRPC and the MCP surface cannot drift (R4-FIX b). The full error chain is
// logged server-side only; a 500 body must never carry paths or upstream text.
use jarvis_core::{
    BareJson, HEALTH_OK_BODY, INTERNAL_GATEWAY_ERROR_MESSAGE as INTERNAL_ERROR_MESSAGE,
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
        "status" => jarvis_core::status_label(response.status()),
    )
    .increment(1);
    response
}

/// I66-B: `jarvis_http_requests_total{status=...}` must keep body-size (413)
/// and content-type (415) rejections in their own fixed buckets. Before this
/// fix both collapsed into `other`, hiding size-limit spikes in Grafana.
/// I93-D: the mapping now lives in the shared `jarvis_core::status_label`; the
/// gateway previously folded 503 into `other`, hiding availability incidents.
#[cfg(test)]
mod status_label_tests {
    use axum::http::StatusCode;

    #[test]
    fn maps_413_and_415_to_distinct_fixed_labels() {
        let too_large = StatusCode::from_u16(413).expect("valid status");
        let wrong_media_type = StatusCode::from_u16(415).expect("valid status");
        let unlisted = StatusCode::from_u16(418).expect("valid status");
        assert_eq!(jarvis_core::status_label(too_large), "413");
        assert_eq!(jarvis_core::status_label(wrong_media_type), "415");
        // Bounded vocabulary: unrelated codes must still collapse to `other`.
        assert_eq!(jarvis_core::status_label(unlisted), "other");
    }

    /// I93-D: a 503 must keep its own bucket instead of being rounded to
    /// `other`, so a gateway availability incident stays visible in Grafana.
    #[test]
    fn maps_503_to_its_own_label() {
        let unavailable = StatusCode::from_u16(503).expect("valid status");
        assert_eq!(jarvis_core::status_label(unavailable), "503");
    }

    /// I97-D: the shared vocabulary must expose 410 as its own bucket so an
    /// expired approval stays observable instead of collapsing to `other`.
    #[test]
    fn maps_410_to_its_own_label() {
        let expired = StatusCode::from_u16(410).expect("valid status");
        let decided = StatusCode::from_u16(409).expect("valid status");
        assert_eq!(jarvis_core::status_label(expired), "410");
        assert_ne!(
            jarvis_core::status_label(expired),
            jarvis_core::status_label(decided)
        );
        assert_ne!(jarvis_core::status_label(expired), "other");
    }
}

/// Fixed 503 body for a gateway whose configured upstream MCP servers are not
/// registered. It mirrors `jarvis-core`'s closed health shape: the healthy body
/// is the shared `HEALTH_OK_BODY`, and a degraded answer is one complete literal
/// with `status: "degraded"` plus a fixed reason. The body is never assembled
/// from request data or upstream text, so a relay cannot turn it into a false
/// release by rewriting the wire shape.
const HEALTH_NO_UPSTREAM_BODY: &str = r#"{"status":"degraded","reason":"no_upstream"}"#;

/// Wrap one of the two fixed health bodies with the stable JSON content type
/// (same shape as `jarvis-policy::http::fixed_json_response`).
fn fixed_json_response(
    status: axum::http::StatusCode,
    body: &'static str,
) -> axum::response::Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    response
}

/// Pure status/body decision so the wire contract can be pinned in a unit test
/// without standing up a listener.
fn health_response(upstreams_ready: bool) -> axum::response::Response {
    if upstreams_ready {
        fixed_json_response(axum::http::StatusCode::OK, HEALTH_OK_BODY)
    } else {
        fixed_json_response(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            HEALTH_NO_UPSTREAM_BODY,
        )
    }
}

/// Observable upstream-registration state for this process.
///
/// A legacy/native-only deployment configures no upstreams and stays healthy.
/// When upstreams are configured, startup registers them before the listener
/// starts (`main.rs`); a registration that never happened is therefore a real
/// degraded state, not a missing capability we can silently ignore. The
/// registration capture is observable from this transport through
/// `Gateway::upstream_input_schema`: upstream-owned tools are the only ones
/// with a captured upstream schema, and native tools keep the hardcoded one.
/// Per-process liveness pruning lives in `Gateway::execute`; this probe stays
/// read-only and never runs a tool on a health scrape.
async fn upstreams_registered(gateway: &McpGateway) -> bool {
    if gateway.config.upstreams.is_empty() {
        return true;
    }
    let principal = gateway.config.mcp_principal.as_str();
    for tool in gateway.tools_for_principal(principal) {
        if gateway
            .gateway
            .upstream_input_schema(&tool.name)
            .await
            .is_some()
        {
            return true;
        }
    }
    false
}

/// 200 `{"status":"ok"}` while the gateway is usable, 503 with a fixed
/// degraded body when a configured upstream is not registered. No new endpoint
/// is added; existing pollers keep the public, token-exempt `/health` route.
async fn health(State(gateway): State<Arc<McpGateway>>) -> axum::response::Response {
    health_response(upstreams_registered(&gateway).await)
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
    req: BareJson<ExecuteToolRequest>,
) -> Result<Json<ExecuteToolResponse>, (axum::http::StatusCode, Json<Value>)> {
    // A type-mismatched body is collapsed to an empty 400 by `BareJson` before
    // this line; no `JsonRejection` detail can reach the response.
    let BareJson(Json(req)) = req;
    let result = gateway
        .execute_tool(&req.tool, req.args, &req.principal)
        .await
        .map_err(http_error)?;
    Ok(Json(ExecuteToolResponse { result }))
}

/// Reduce a caller-input failure to a fixed vocabulary of safe, static
/// phrases. Several `client_error()` messages interpolate absolute sandbox
/// paths, git stderr, egress hosts or raw URLs, so only known-static messages
/// pass through verbatim. This is the single shared helper for the HTTP, gRPC
/// and MCP surfaces; every caller must use it instead of echoing
/// `error.to_string()` into a response, so a vocabulary update cannot fork one
/// surface (I97-D).
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
        // Poll with a deadline so a missing Pending entry fails the test
        // instead of hanging the whole test binary.
        let id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
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
            }
        })
        .await
        .expect("approval must become pending within 5s");
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

    /// I97-D: the HTTP adapter's caller-visible body must come from the same
    /// helper the MCP protocol surface uses (see the contract test in
    /// `mcp_server::tests`). A vocabulary edit can therefore no longer make the
    /// HTTP/gRPC and MCP faces disagree.
    #[test]
    fn http_error_uses_the_shared_client_error_vocabulary() {
        for message in [
            "missing 'url' argument",
            "too many redirects (> 3)",
            "host 'attacker.example' is not in the egress allowlist",
            "stat /home/koppe/.secret/token: permission denied",
            "completely unknown caller text",
        ] {
            let (status, body) = http_error(client_error(message));
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
            assert_eq!(
                body.0["error"].as_str().unwrap(),
                safe_client_error_text(message),
                "HTTP and MCP faces must sanitize {message} identically"
            );
        }
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

    /// I99-C: the healthy wire body is the shared `jarvis-core` literal and a
    /// degraded answer is a 503 with one complete fixed body. Both are pinned
    /// byte for byte so a relay cannot rewrite the health contract unnoticed.
    #[tokio::test]
    async fn health_uses_core_ok_body_and_fixed_503_degraded_body() {
        let ok = health_response(true);
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(
            ok.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .expect("content type"),
            "application/json"
        );
        let bytes = axum::body::to_bytes(ok.into_body(), usize::MAX)
            .await
            .expect("ok body bytes");
        assert_eq!(bytes.as_ref(), HEALTH_OK_BODY.as_bytes());

        let degraded = health_response(false);
        assert_eq!(degraded.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(degraded.into_body(), usize::MAX)
            .await
            .expect("degraded body bytes");
        assert_eq!(bytes.as_ref(), HEALTH_NO_UPSTREAM_BODY.as_bytes());
        let value: Value = serde_json::from_slice(&bytes).expect("degraded body is JSON");
        assert_eq!(value["status"], jarvis_core::HEALTH_STATUS_DEGRADED);
        assert_eq!(value["reason"], "no_upstream");
    }

    /// I99-C: a native-only deployment has no upstream dependency and keeps the
    /// historical healthy answer.
    #[tokio::test]
    async fn native_only_gateway_has_no_upstream_degradation() {
        let gateway = test_gateway("health-native-only").await;
        assert!(upstreams_registered(&gateway).await);
    }

    /// I99-C: configuring an upstream but never registering it is a degraded
    /// state. The upstream command is deliberately nonexistent: `McpGateway::new`
    /// does not spawn upstreams, so no process or network is touched here.
    #[tokio::test]
    async fn configured_upstream_without_registration_is_degraded() {
        let dir = std::env::temp_dir().join("jarvis-mcp-gw-http-health-upstream");
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
            upstreams: vec![crate::config::UpstreamConfig {
                name: "unregistered".to_owned(),
                command: vec!["/nonexistent/mcp-upstream".to_owned()],
            }],
            ..Default::default()
        };
        let policy = Arc::new(testing::shipped_policy_client("health-upstream").await);
        let gateway = McpGateway::new(config, policy)
            .await
            .expect("gateway starts");

        assert!(!upstreams_registered(&gateway).await);
        let response = health_response(upstreams_registered(&gateway).await);
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

        std::fs::remove_dir_all(&dir).ok();
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
        // Positive control (I37-B): an EVENT proves the thread-local subscriber
        // is capturing output. An empty log would make the `!contains(PROBE)`
        // assertion below vacuously pass even if `#[instrument(skip(req))]`
        // regressed. Kept distinct from PROBE so capturing it cannot mask a
        // real leak.
        const SUBSCRIBER_PROBE: &str = "subscriber-sanity-probe-4f1a";

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
            tracing::info!(marker = SUBSCRIBER_PROBE, "subscriber sanity check");
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
                let _ = execute_tool(State(gateway), BareJson(Json(req))).await;
            });
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(
            logs.contains(SUBSCRIBER_PROBE),
            "subscriber did not capture the sanity event: {logs}"
        );
        // The non-leak assertion is the contract. Do not assert that the span
        // itself was captured: the thread-local subscriber can miss spans when
        // many tests run in parallel, which made this test flaky under
        // `cargo test --all-targets` even though the single-test run passed.
        assert!(!logs.contains(PROBE), "request body leaked: {logs}");
    }

    /// I59-C: a wrong JSON type must be rejected by the shared extractor with
    /// a bare 400, never axum's detailed `JsonRejection` body. I61-A keeps the
    /// 400 for type mismatches; a missing Content-Type is now 415 and a body
    /// over axum's 2 MiB limit is 413.
    #[tokio::test]
    async fn bare_json_rejects_type_mismatch_with_bare_400() {
        use axum::extract::FromRequest;

        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/execute")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"tool":42,"args":{},"principal":"opencode"}"#,
            ))
            .expect("request builds");
        let state = ();
        let outcome = BareJson::<ExecuteToolRequest>::from_request(request, &state).await;
        assert_eq!(outcome.unwrap_err(), axum::http::StatusCode::BAD_REQUEST);
    }

    /// Static guard (I37-B): `#[instrument]` on `/v1/execute` must keep
    /// `skip(req)` so caller payloads never become span fields. `concat!`
    /// keeps the expected text out of this file's literal source, which
    /// `include_str!` also reads, so the guard cannot self-match.
    #[test]
    fn execute_tool_instrument_attribute_skips_request() {
        const SRC: &str = include_str!("http.rs");
        let attribute = concat!("#[instrument(skip(gateway, ", "req))]");
        assert!(
            SRC.contains(attribute),
            "execute_tool must skip the request body in its span"
        );
    }
}
