//! HTTP/REST + Metrics endpoint for Policy Engine

use crate::PolicyEngine;
use crate::policy::{ApprovalDecision, ApprovalRequest};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use tracing::instrument;

/// Handle for the process-wide Prometheus recorder.
static METRICS_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

/// Install the Prometheus recorder exactly once per process.
///
/// Called from `main`; tests never install the global recorder so they can
/// run in parallel with other tests in the same binary.
pub fn install_metrics() -> anyhow::Result<()> {
    let handle = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;
    if METRICS_HANDLE.set(handle).is_err() {
        tracing::warn!("metrics recorder already installed; keeping the first handle");
    }
    Ok(())
}

#[derive(Deserialize, Debug)]
struct CheckRequest {
    principal: String,
    action: String,
    resource: String,
    context: serde_json::Value,
}

#[derive(Serialize, Debug)]
struct CheckResponse {
    allowed: bool,
}

/// Wire DTO for `POST /v1/approval`.
///
/// `id` and `requested_at` are kept as optional legacy fields so existing
/// clients (e.g. the MCP gateway) keep working, but the server never trusts
/// them: fresh values are generated and echoed in the response. A present but
/// malformed value is rejected with 400 instead of being silently swapped for
/// a new UUID or epoch 0. `expires_at` is only a hint, clamped to the
/// configured approval timeout so no caller can extend the decision window.
#[derive(Deserialize, Debug)]
struct ApprovalRequestHttp {
    id: Option<String>,
    principal: String,
    action: String,
    resource: String,
    context: serde_json::Value,
    requested_at: Option<i64>,
    expires_at: Option<i64>,
}

/// Server-authoritative request metadata echoed on every response so a client
/// can track a request whose id/timestamps it no longer supplies.
#[derive(Serialize, Debug)]
struct ApprovalRequestMeta {
    id: uuid::Uuid,
    requested_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Merge the generated metadata into the decision object at the top level.
///
/// Existing consumers read `decision["decision"]` / `approved_by` / `reason`,
/// so the tagged-enum shape is preserved and only new keys are added.
fn approval_response(decision: ApprovalDecision, meta: &ApprovalRequestMeta) -> serde_json::Value {
    let mut response = serde_json::to_value(decision).unwrap_or_default();
    if let Some(object) = response.as_object_mut() {
        object.insert("id".into(), serde_json::json!(meta.id));
        object.insert("requested_at".into(), serde_json::json!(meta.requested_at));
        object.insert("expires_at".into(), serde_json::json!(meta.expires_at));
    }
    response
}

#[derive(Deserialize, Debug)]
struct ApproveRequestHttp {
    #[serde(default = "default_approver")]
    approved_by: String,
}

fn default_approver() -> String {
    "jarvis_user".to_string()
}

#[derive(Deserialize, Debug)]
struct DenyRequestHttp {
    #[serde(default = "default_deny_reason")]
    reason: String,
}

fn default_deny_reason() -> String {
    "denied by user".to_string()
}

// SECURITY NOTE: the approval endpoints are unauthenticated by default for
// backward compatibility with the loopback-only deployment. Set
// `JARVIS_POLICY_APPROVAL_API_TOKEN` to require the X-Jarvis-Approval-Token header
// before exposing the listener beyond loopback. The MCP gateway and UI read
// the same value from their own configuration and send it on every call.
// P0-1 staged rollout: set `JARVIS_POLICY_REQUIRE_APPROVAL_API_TOKEN` to fail
// closed (503) when no token is configured, instead of silently staying open.
// I7-A: when `JARVIS_POLICY_APPROVAL_ADMIN_TOKEN` is set, approve/deny require
// that separate admin token. A caller holding only the API token can then no
// longer submit a request and approve it themselves. Unset keeps the legacy
// single-token contract.

/// Which token guards a given approval route.
#[derive(Clone, Copy)]
enum ApprovalScope {
    /// request/list/status: gateway-facing `approval_api_token`.
    Api,
    /// approve/deny: `approval_admin_token` when configured, otherwise the
    /// legacy `approval_api_token` fallback.
    Admin,
}

pub fn router(engine: Arc<PolicyEngine>) -> Router {
    // Approval routes are split by authority so health/metrics/check and the
    // kill-switch endpoints keep their existing authentication scope. The two
    // sub-routers are merged only after their layers are applied, otherwise
    // the second layer would also wrap the first group's routes.
    let api_approvals = Router::new()
        .route("/", post(request_approval))
        .route("/list", get(list_approvals))
        .route("/:id", get(approval_status))
        .layer(axum::middleware::from_fn_with_state(
            (engine.clone(), ApprovalScope::Api),
            require_approval_token,
        ));
    let admin_approvals = Router::new()
        .route("/:id/approve", post(approve_approval))
        .route("/:id/deny", post(deny_approval))
        .layer(axum::middleware::from_fn_with_state(
            (engine.clone(), ApprovalScope::Admin),
            require_approval_token,
        ));
    let approvals = api_approvals.merge(admin_approvals);

    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/check", post(check))
        .nest("/v1/approval", approvals)
        .route(
            "/v1/approvals",
            get(list_approvals).layer(axum::middleware::from_fn_with_state(
                (engine.clone(), ApprovalScope::Api),
                require_approval_token,
            )),
        )
        .route("/v1/kill-switch", get(kill_switch_status))
        .route("/v1/kill-switch/activate", post(activate_kill_switch))
        .route("/v1/kill-switch/deactivate", post(deactivate_kill_switch))
        .with_state(engine)
        .layer(axum::middleware::from_fn(track_metrics))
}

/// Scope-aware token gate for approval endpoints.
///
/// `Api` routes use `approval_api_token`; `Admin` routes use the separate
/// `approval_admin_token` when configured and fall back to `approval_api_token`
/// when it is not (legacy compatibility). When the effective token is unset
/// the endpoint keeps its historical loopback-only behavior, unless the opt-in
/// `require_approval_api_token` guard is enabled: then it fails closed with
/// 503. A configured token must be sent in the `X-Jarvis-Approval-Token`
/// header. Health/metrics/check are not covered by this middleware.
async fn require_approval_token(
    State((engine, scope)): State<(Arc<PolicyEngine>, ApprovalScope)>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let expected = match scope {
        ApprovalScope::Api => engine.approval_api_token(),
        ApprovalScope::Admin => engine
            .approval_admin_token()
            .or_else(|| engine.approval_api_token()),
    };
    let Some(expected) = expected else {
        if engine.require_approval_api_token() {
            tracing::warn!(
                "Approval endpoints are disabled: approval token is required but not configured"
            );
            return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
        }
        return Ok(next.run(request).await);
    };
    let provided = request
        .headers()
        .get("x-jarvis-approval-token")
        .and_then(|v| v.to_str().ok());
    // A missing (or non-UTF-8) header is never a valid token, even when the
    // configured value happens to be empty.
    if !provided.is_some_and(|p| constant_time_eq(p, expected)) {
        return Err(axum::http::StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

/// Compare two tokens without an early exit on the first mismatch.
///
/// The loop always covers the longer input and pads the shorter one, so
/// neither a content mismatch nor a length mismatch short-circuits. No
/// additional crate is needed and no `unsafe` code is involved.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for (x, y) in a
        .iter()
        .map(Some)
        .chain(std::iter::repeat(None))
        .zip(b.iter().map(Some).chain(std::iter::repeat(None)))
        .take(max)
    {
        diff |= (x.copied().unwrap_or(0) ^ y.copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// Count every HTTP request so `/metrics` exposes more than an empty page
/// even when the `process` feature of the exporter is disabled.
async fn track_metrics(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    metrics::counter!(
        "jarvis_http_requests_total",
        "service" => "jarvis-policy",
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
        503 => "503",
        _ => "other",
    }
}

/// Fixed, non-sensitive health states. The body deliberately uses a tiny
/// closed vocabulary: no file paths, no tokens, and never the kill-switch
/// reason string. Generic probes can poll `/health` without receiving
/// operational details even while the service is degraded.
#[derive(Serialize, Debug)]
struct HealthResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

const HEALTH_STATUS_OK: &str = "ok";
const HEALTH_STATUS_DEGRADED: &str = "degraded";
const HEALTH_REASON_KILL_SWITCH: &str = "kill_switch";
const HEALTH_REASON_NO_POLICY: &str = "no_policy";

/// Lightweight check that at least one configured policy source is still
/// present. `PolicyEngine` loads Cedar rules from `cedar_policy_dir` and Casbin
/// rules from `casbin_policy_path` at startup, so a file removed later is
/// exactly the "policy file missing" degradation a probe must detect.
///
/// This is a metadata-only lookup: no DB/Qdrant connection and no new
/// dependency. An empty Cedar directory is acceptable while the Casbin policy
/// file exists, because authorization stays deny-by-default in that setup
/// (the same setup used by the test engines).
fn policy_available(engine: &PolicyEngine) -> bool {
    if std::path::Path::new(&engine.config.casbin_policy_path).is_file() {
        return true;
    }
    std::fs::read_dir(&engine.config.cedar_policy_dir)
        .ok()
        .is_some_and(|entries| {
            entries.flatten().any(|entry| {
                let path = entry.path();
                path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("cedar")
            })
        })
}

/// 200 `{"status":"ok"}` while the service is usable, otherwise 503 with a
/// fixed reason. The kill switch takes precedence so an operator always sees
/// the emergency-stop state first. The normal 200 contract is unchanged for
/// existing `/health` pollers.
async fn health(
    State(engine): State<Arc<PolicyEngine>>,
) -> (axum::http::StatusCode, Json<HealthResponse>) {
    let degraded_reason = if engine.kill_switch_status().activated {
        Some(HEALTH_REASON_KILL_SWITCH)
    } else if !policy_available(&engine) {
        Some(HEALTH_REASON_NO_POLICY)
    } else {
        None
    };

    match degraded_reason {
        Some(reason) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthResponse {
                status: HEALTH_STATUS_DEGRADED,
                reason: Some(reason),
            }),
        ),
        None => (
            axum::http::StatusCode::OK,
            Json(HealthResponse {
                status: HEALTH_STATUS_OK,
                reason: None,
            }),
        ),
    }
}

async fn metrics() -> String {
    match METRICS_HANDLE.get() {
        Some(handle) => handle.render(),
        None => "# metrics recorder not installed\n".to_string(),
    }
}

#[instrument(skip(engine, req))]
async fn check(
    State(engine): State<Arc<PolicyEngine>>,
    Json(req): Json<CheckRequest>,
) -> Json<CheckResponse> {
    // I14-B: same policy-side normalization as the gRPC handler. Cedar rejects
    // JSON `null`, so passing the wire context through verbatim made /v1/check
    // silently deny requests the gRPC client used to sanitize before sending.
    let context = crate::client::without_nulls(&req.context);
    let allowed = engine
        .is_allowed(&req.principal, &req.action, &req.resource, &context)
        .await
        .unwrap_or(false);
    Json(CheckResponse { allowed })
}

/// HTTP 専用の承認受付。
///
/// `PolicyEngine::request_approval` と同じ kill-switch 判定を行った上で、
/// キュー枯渇を `ApprovalError::QueueFull` として返す。通常のポリシー拒否は
/// 従来どおり `ApprovalDecision::Denied` なので 200 の decision のままになる。
/// 公開シグネチャを変えずに区別を追加するため、このヘルパーは HTTP モジュール内
/// に置き、`request_approval` 自体は gRPC/既存クライアント互換のまま維持する。
impl PolicyEngine {
    /// Optional separate token for approve/deny. Kept next to the HTTP-only
    /// helpers so the rest of the engine is untouched.
    fn approval_admin_token(&self) -> Option<&str> {
        self.config.approval_admin_token.as_deref()
    }

    async fn request_approval_http(
        &self,
        request: ApprovalRequest,
    ) -> Result<ApprovalDecision, crate::approval::ApprovalError> {
        // PolicyEngine::request_approval の kill-switch ガードと同じ判定。
        if self.kill_switch.is_activated() {
            return Ok(ApprovalDecision::Denied {
                reason: "Kill switch activated".into(),
            });
        }
        self.approvals.try_request(request).await
    }
}

/// Accept an approval request without trusting client-supplied identity or
/// timestamps (I9-A).
///
/// Invalid values are rejected with 400; valid legacy values are ignored in
/// favour of server-generated ones, which are returned in the response so the
/// caller can track the request. `expires_at` is clamped to the store's
/// configured timeout, the same cap `ApprovalStore::try_request` enforces.
#[instrument(skip(engine, req))]
async fn request_approval(
    State(engine): State<Arc<PolicyEngine>>,
    Json(req): Json<ApprovalRequestHttp>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let bad_request = axum::http::StatusCode::BAD_REQUEST;

    if let Some(id) = req.id.as_deref() {
        uuid::Uuid::parse_str(id).map_err(|_| bad_request)?;
    }
    let now = chrono::Utc::now();
    if let Some(requested_at) = req.requested_at {
        chrono::DateTime::from_timestamp(requested_at, 0).ok_or(bad_request)?;
    }
    let mut expires_at = engine.approvals.max_deadline(now);
    if let Some(hint) = req.expires_at {
        let hint = chrono::DateTime::from_timestamp(hint, 0).ok_or(bad_request)?;
        if hint <= now {
            return Err(bad_request);
        }
        expires_at = std::cmp::min(hint, expires_at);
    }

    let meta = ApprovalRequestMeta {
        id: uuid::Uuid::new_v4(),
        requested_at: now,
        expires_at,
    };
    let approval_req = ApprovalRequest {
        id: meta.id,
        principal: req.principal,
        action: req.action,
        resource: req.resource,
        context: req.context,
        requested_at: meta.requested_at,
        expires_at: meta.expires_at,
    };
    let decision = engine
        .request_approval_http(approval_req)
        .await
        .map_err(approval_error_status)?;
    Ok(Json(approval_response(decision, &meta)))
}

#[instrument(skip(engine, req))]
async fn approve_approval(
    State(engine): State<Arc<PolicyEngine>>,
    Path(id): Path<String>,
    Json(req): Json<ApproveRequestHttp>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let id = uuid::Uuid::parse_str(&id).map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    let decision = engine
        .approve(id, &req.approved_by)
        .await
        .map_err(approval_error_status)?;
    Ok(Json(serde_json::to_value(decision).unwrap_or_default()))
}

#[instrument(skip(engine, req))]
async fn deny_approval(
    State(engine): State<Arc<PolicyEngine>>,
    Path(id): Path<String>,
    Json(req): Json<DenyRequestHttp>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let id = uuid::Uuid::parse_str(&id).map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    let decision = engine
        .deny(id, &req.reason)
        .await
        .map_err(approval_error_status)?;
    Ok(Json(serde_json::to_value(decision).unwrap_or_default()))
}

#[instrument(skip(engine))]
async fn approval_status(
    State(engine): State<Arc<PolicyEngine>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let id = uuid::Uuid::parse_str(&id).map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    let decision = engine
        .approval_status(id)
        .await
        .map_err(approval_error_status)?;
    Ok(Json(serde_json::to_value(decision).unwrap_or_default()))
}

#[instrument(skip(engine))]
async fn list_approvals(State(engine): State<Arc<PolicyEngine>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "pending": engine.pending_approvals().await }))
}

fn approval_error_status(error: crate::approval::ApprovalError) -> axum::http::StatusCode {
    match error {
        crate::approval::ApprovalError::NotFound(_) => axum::http::StatusCode::NOT_FOUND,
        crate::approval::ApprovalError::AlreadyDecided(_) => axum::http::StatusCode::CONFLICT,
        crate::approval::ApprovalError::KillSwitchActivated => axum::http::StatusCode::CONFLICT,
        // キュー枯渇は一時的な基盤エラー。通常の deny (200 の decision) と
        // 区別し、呼び出し側が後でリトライできるよう 503 を返す。
        crate::approval::ApprovalError::QueueFull => axum::http::StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn kill_switch_status(
    State(engine): State<Arc<PolicyEngine>>,
) -> Json<crate::kill_switch::KillSwitchStatus> {
    Json(engine.kill_switch_status())
}

#[derive(Deserialize, Debug)]
struct ActivateKillSwitchRequest {
    reason: String,
    admin_token: String,
}

async fn activate_kill_switch(
    State(engine): State<Arc<PolicyEngine>>,
    Json(req): Json<ActivateKillSwitchRequest>,
) -> Result<&'static str, axum::http::StatusCode> {
    engine
        .activate_kill_switch(req.reason, &req.admin_token)
        .map(|_| "activated")
        .map_err(|_| axum::http::StatusCode::UNAUTHORIZED)
}

#[derive(Deserialize, Debug)]
struct DeactivateKillSwitchRequest {
    admin_token: String,
}

async fn deactivate_kill_switch(
    State(engine): State<Arc<PolicyEngine>>,
    Json(req): Json<DeactivateKillSwitchRequest>,
) -> Result<&'static str, axum::http::StatusCode> {
    engine
        .deactivate_kill_switch(&req.admin_token)
        .map(|_| "deactivated")
        .map_err(|_| axum::http::StatusCode::UNAUTHORIZED)
}

pub async fn serve(engine: Arc<PolicyEngine>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    if engine.approval_admin_token().is_none() {
        tracing::warn!(
            "JARVIS_POLICY_APPROVAL_ADMIN_TOKEN is not set: approve/deny fall back to \
             JARVIS_POLICY_APPROVAL_API_TOKEN, so a caller holding that token can \
             approve its own request. Set a distinct admin token to enforce \
             separation of duties."
        );
    }
    let app = router(engine);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PolicyConfig;
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-policy-http-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    async fn test_engine_with_approval_config(
        label: &str,
        max_pending: usize,
        token: Option<String>,
        require_token: bool,
    ) -> (Arc<PolicyEngine>, PathBuf) {
        test_engine_with_full_approval_config(label, max_pending, token, None, require_token).await
    }

    async fn test_engine_with_full_approval_config(
        label: &str,
        max_pending: usize,
        token: Option<String>,
        admin_token: Option<String>,
        require_token: bool,
    ) -> (Arc<PolicyEngine>, PathBuf) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
        let dir = temp_dir(label);
        let model = dir.join("casbin_model.conf");
        let policy = dir.join("casbin_policy.csv");
        std::fs::copy(source.join("casbin_model.conf"), &model).expect("copy casbin model");
        std::fs::copy(source.join("casbin_policy.csv"), &policy).expect("copy casbin policy");

        let config = PolicyConfig {
            cedar_policy_dir: dir.join("cedar").to_string_lossy().into_owned(),
            casbin_model_path: model.to_string_lossy().into_owned(),
            casbin_policy_path: policy.to_string_lossy().into_owned(),
            kill_switch_admin_token: "test-admin-token".into(),
            approval_timeout_secs: 5,
            approval_max_pending: max_pending,
            approval_api_token: token,
            approval_admin_token: admin_token,
            require_approval_api_token: require_token,
            ..Default::default()
        };
        let engine = Arc::new(
            PolicyEngine::new(config)
                .await
                .expect("policy engine starts"),
        );
        (engine, dir)
    }

    async fn test_engine_with_cap_and_token(
        label: &str,
        max_pending: usize,
        token: Option<String>,
    ) -> (Arc<PolicyEngine>, PathBuf) {
        test_engine_with_approval_config(label, max_pending, token, false).await
    }

    async fn test_engine_with_cap(label: &str, max_pending: usize) -> (Arc<PolicyEngine>, PathBuf) {
        test_engine_with_cap_and_token(label, max_pending, None).await
    }

    async fn test_engine(label: &str) -> (Arc<PolicyEngine>, PathBuf) {
        test_engine_with_cap(label, 100).await
    }

    /// Engine whose allow decision can only come from Cedar: the Casbin policy
    /// is left empty (the engine creates it at startup), so a null-bearing
    /// context used to fail `Context::from_json_value` and deny the request.
    async fn cedar_only_engine(label: &str) -> (Arc<PolicyEngine>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("jarvis-policy-http-cedar-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        let cedar = dir.join("cedar");
        std::fs::create_dir_all(&cedar).expect("create cedar dir");
        std::fs::write(
            cedar.join("allow.cedar"),
            r#"permit (
    principal == jarvis::Principal::"jarvis",
    action == jarvis::Action::"execute",
    resource == jarvis::Resource::"tool:cedar.only"
);
"#,
        )
        .expect("write cedar policy");

        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
        let model = dir.join("casbin_model.conf");
        std::fs::copy(source.join("casbin_model.conf"), &model).expect("copy casbin model");
        let config = PolicyConfig {
            cedar_policy_dir: cedar.to_string_lossy().into_owned(),
            casbin_model_path: model.to_string_lossy().into_owned(),
            casbin_policy_path: dir.join("casbin_policy.csv").to_string_lossy().into_owned(),
            kill_switch_admin_token: "test-admin-token".into(),
            ..Default::default()
        };
        let engine = Arc::new(
            PolicyEngine::new(config)
                .await
                .expect("policy engine starts"),
        );
        (engine, dir)
    }

    /// Serve the real router on an ephemeral port; tests drive it with a real
    /// HTTP client so the wire format is covered end to end.
    async fn start_server(engine: Arc<PolicyEngine>) -> String {
        let app = router(engine);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("test server runs");
        });
        format!("http://{addr}")
    }

    fn pending_request(expires_at: i64) -> Value {
        json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "principal": "jarvis",
            "action": "execute",
            "resource": "tool:filesystem.write",
            "context": {},
            "requested_at": chrono::Utc::now().timestamp(),
            "expires_at": expires_at,
        })
    }

    async fn wait_for_pending(client: &reqwest::Client, base: &str) -> String {
        for _ in 0..50 {
            let body: Value = client
                .get(format!("{base}/v1/approvals"))
                .send()
                .await
                .expect("list request runs")
                .json()
                .await
                .expect("list response is JSON");
            if let Some(id) = body["pending"]
                .as_array()
                .and_then(|pending| pending.first())
                .and_then(|request| request["id"].as_str())
            {
                return id.to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("approval request never appeared");
    }

    async fn post_approval(
        client: &reqwest::Client,
        base: &str,
        body: &Value,
    ) -> reqwest::StatusCode {
        client
            .post(format!("{base}/v1/approval"))
            .json(body)
            .send()
            .await
            .expect("approval request runs")
            .status()
    }

    /// Submit a body, approve it once it is pending, and return the requester's
    /// response body together with the temp dir to clean up.
    async fn submit_and_approve(body: Value, label: &str) -> (Value, PathBuf) {
        let (engine, dir) = test_engine(label).await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();
        let request = tokio::spawn({
            let client = client.clone();
            let url = format!("{base}/v1/approval");
            async move {
                client
                    .post(url)
                    .json(&body)
                    .send()
                    .await
                    .expect("approval request runs")
            }
        });
        let id = wait_for_pending(&client, &base).await;
        let approved = client
            .post(format!("{base}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "tester" }))
            .send()
            .await
            .expect("approve request runs");
        assert_eq!(approved.status(), reqwest::StatusCode::OK);
        let response = request.await.expect("request task joins");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("approval response is JSON");
        (body, dir)
    }

    /// POST /v1/approval blocks until a decision arrives, so the request runs
    /// concurrently with the approve/deny call, exactly like a real client.
    #[tokio::test]
    async fn approval_request_is_approved_over_http() {
        let (engine, dir) = test_engine("approve").await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();
        let expires_at = chrono::Utc::now().timestamp() + 60;

        let pending = tokio::spawn({
            let client = client.clone();
            let url = format!("{base}/v1/approval");
            async move {
                client
                    .post(url)
                    .json(&pending_request(expires_at))
                    .send()
                    .await
                    .expect("approval request runs")
                    .json::<Value>()
                    .await
                    .expect("approval response is JSON")
            }
        });

        let id = wait_for_pending(&client, &base).await;
        let response = client
            .post(format!("{base}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "tester" }))
            .send()
            .await
            .expect("approve request runs");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let decision = pending.await.expect("request task joins");
        assert_eq!(decision["decision"], "Approved");
        assert_eq!(decision["approved_by"], "tester");

        let status: Value = client
            .get(format!("{base}/v1/approval/{id}"))
            .send()
            .await
            .expect("status request runs")
            .json()
            .await
            .expect("status response is JSON");
        assert_eq!(status["decision"], "Approved");

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[tokio::test]
    async fn check_endpoint_allows_and_denies() {
        let (engine, dir) = test_engine("check").await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        let allowed: Value = client
            .post(format!("{base}/v1/check"))
            .json(&json!({
                "principal": "jarvis",
                "action": "execute",
                "resource": "tool:filesystem.read",
                "context": {},
            }))
            .send()
            .await
            .expect("check request runs")
            .json()
            .await
            .expect("check response is JSON");
        assert_eq!(allowed["allowed"], true);

        let denied: Value = client
            .post(format!("{base}/v1/check"))
            .json(&json!({
                "principal": "jarvis",
                "action": "execute",
                "resource": "tool:git.commit",
                "context": {},
            }))
            .send()
            .await
            .expect("check request runs")
            .json()
            .await
            .expect("check response is JSON");
        assert_eq!(denied["allowed"], false);

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// I14-B: over HTTP, a context containing `null` must be judged exactly
    /// like the equivalent context with nulls removed.
    #[tokio::test]
    async fn null_context_matches_sanitized_context_over_http() {
        let (engine, dir) = cedar_only_engine("null-context-http").await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();
        let sanitized = json!({
            "event": { "type": "state_changed" },
            "recent": [{ "content": "x" }],
        });
        let with_nulls = json!({
            "event": { "id": null, "type": "state_changed" },
            "recent": [null, { "content": "x" }],
        });

        for (label, context) in [("sanitized", sanitized), ("with nulls", with_nulls)] {
            let response: Value = client
                .post(format!("{base}/v1/check"))
                .json(&json!({
                    "principal": "jarvis",
                    "action": "execute",
                    "resource": "tool:cedar.only",
                    "context": context,
                }))
                .send()
                .await
                .expect("check request runs")
                .json()
                .await
                .expect("check response is JSON");
            assert_eq!(
                response["allowed"], true,
                "{label} context must be allowed by the Cedar permit"
            );
        }

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// I14-B: the same null-bearing context must yield the same decision on
    /// HTTP and gRPC. Both servers share one engine, and the gRPC side uses the
    /// raw generated client so client-side normalization cannot hide a
    /// transport difference.
    #[tokio::test]
    async fn null_context_decision_matches_between_http_and_grpc() {
        let (engine, dir) = cedar_only_engine("null-context-cross-transport").await;
        let base = start_server(engine.clone()).await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind grpc test server");
        let grpc_addr = listener.local_addr().expect("grpc test server address");
        tokio::spawn(async move {
            crate::grpc::serve_with_listener(engine, listener)
                .await
                .expect("grpc test server runs");
        });
        let channel = tonic::transport::Channel::from_shared(format!("http://{grpc_addr}"))
            .expect("grpc endpoint")
            .connect_lazy();
        use crate::grpc::proto::policy_service_client::PolicyServiceClient as GrpcClient;
        let mut grpc = GrpcClient::new(channel);

        let context = json!({
            "event": { "id": null, "type": "state_changed" },
            "recent": [null, { "content": "x" }],
        });
        let http_response: Value = reqwest::Client::new()
            .post(format!("{base}/v1/check"))
            .json(&json!({
                "principal": "jarvis",
                "action": "execute",
                "resource": "tool:cedar.only",
                "context": context.clone(),
            }))
            .send()
            .await
            .expect("http check runs")
            .json()
            .await
            .expect("http response is JSON");
        let grpc_allowed = grpc
            .check(crate::grpc::proto::CheckRequest {
                principal: "jarvis".into(),
                action: "execute".into(),
                resource: "tool:cedar.only".into(),
                context: context.to_string(),
            })
            .await
            .expect("grpc check runs")
            .into_inner()
            .allowed;

        let http_allowed = http_response["allowed"]
            .as_bool()
            .expect("http response carries a boolean");
        assert!(
            http_allowed,
            "the Cedar-only permit must allow this request"
        );
        assert_eq!(
            grpc_allowed, http_allowed,
            "HTTP and gRPC must agree on the same null-bearing context"
        );

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[tokio::test]
    async fn kill_switch_endpoint_requires_the_admin_token() {
        let (engine, dir) = test_engine("killswitch-http").await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        let rejected = client
            .post(format!("{base}/v1/kill-switch/activate"))
            .json(&json!({ "reason": "test", "admin_token": "wrong-token" }))
            .send()
            .await
            .expect("activate request runs");
        assert_eq!(rejected.status(), reqwest::StatusCode::UNAUTHORIZED);

        let activated = client
            .post(format!("{base}/v1/kill-switch/activate"))
            .json(&json!({ "reason": "test", "admin_token": "test-admin-token" }))
            .send()
            .await
            .expect("activate request runs");
        assert_eq!(activated.status(), reqwest::StatusCode::OK);

        let denied: Value = client
            .post(format!("{base}/v1/check"))
            .json(&json!({
                "principal": "jarvis",
                "action": "execute",
                "resource": "tool:filesystem.read",
                "context": {},
            }))
            .send()
            .await
            .expect("check request runs")
            .json()
            .await
            .expect("check response is JSON");
        assert_eq!(denied["allowed"], false);

        let deactivated = client
            .post(format!("{base}/v1/kill-switch/deactivate"))
            .json(&json!({ "admin_token": "test-admin-token" }))
            .send()
            .await
            .expect("deactivate request runs");
        assert_eq!(deactivated.status(), reqwest::StatusCode::OK);

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[tokio::test]
    async fn approval_request_is_denied_over_http() {
        let (engine, dir) = test_engine("deny").await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();
        let expires_at = chrono::Utc::now().timestamp() + 60;

        let pending = tokio::spawn({
            let client = client.clone();
            let url = format!("{base}/v1/approval");
            async move {
                let response = client
                    .post(url)
                    .json(&pending_request(expires_at))
                    .send()
                    .await
                    .expect("approval request runs");
                let status = response.status();
                let body = response
                    .json::<Value>()
                    .await
                    .expect("approval response is JSON");
                (status, body)
            }
        });

        let id = wait_for_pending(&client, &base).await;
        let response = client
            .post(format!("{base}/v1/approval/{id}/deny"))
            .json(&json!({ "reason": "not now" }))
            .send()
            .await
            .expect("deny request runs");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let (status, decision) = pending.await.expect("request task joins");
        // 通常のポリシー拒否はキュー枯渇 (503) と区別し、従来どおり 200。
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(decision["decision"], "Denied");
        assert_eq!(decision["reason"], "not now");

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[tokio::test]
    async fn approval_endpoint_returns_denied_when_queue_is_full() {
        let (engine, _dir) = test_engine_with_cap("http-cap", 1).await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();
        let pending = pending_request(chrono::Utc::now().timestamp() + 60);

        // The approval POST blocks until decided, so run the first request in
        // the background and wait for it to appear in the pending list.
        let first_call = tokio::spawn({
            let base = base.clone();
            let client = client.clone();
            let pending = pending.clone();
            async move {
                client
                    .post(format!("{base}/v1/approval"))
                    .json(&pending)
                    .send()
                    .await
                    .expect("first request")
                    .json::<serde_json::Value>()
                    .await
                    .expect("json")
            }
        });
        let id = loop {
            let list: serde_json::Value = client
                .get(format!("{base}/v1/approvals"))
                .send()
                .await
                .expect("list")
                .json()
                .await
                .expect("json");
            if let Some(entry) = list["pending"].as_array().and_then(|v| v.first()) {
                break entry["id"].as_str().expect("id").to_owned();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        // A distinct id avoids the duplicate-id guard; the cap must trigger.
        // 期待値を更新: キュー枯渇は基盤エラーなので、従来の 200 の Denied
        // decision ではなく 503 Service Unavailable を返す（テスト名は履歴上の
        // ものを維持）。通常のポリシー拒否は引き続き 200 の decision を返す。
        let mut second_pending = pending_request(chrono::Utc::now().timestamp() + 60);
        second_pending["id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
        let second = client
            .post(format!("{base}/v1/approval"))
            .json(&second_pending)
            .send()
            .await
            .expect("second request");
        assert_eq!(second.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);

        client
            .post(format!("{base}/v1/approval/{id}/approve"))
            .json(&serde_json::json!({ "approved_by": "koppe" }))
            .send()
            .await
            .expect("approve");
        let _ = first_call.await.expect("first call task");
    }

    #[tokio::test]
    async fn approval_token_auth_rejects_missing_and_accepts_correct_header() {
        let (engine, _dir) =
            test_engine_with_cap_and_token("http-token", 10, Some("test-token".into())).await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        // Missing token -> 401 on every approval route.
        let no_token = client
            .get(format!("{base}/v1/approvals"))
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(no_token, reqwest::StatusCode::UNAUTHORIZED);

        // Wrong token -> 401.
        let wrong = client
            .get(format!("{base}/v1/approvals"))
            .header("x-jarvis-approval-token", "wrong")
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(wrong, reqwest::StatusCode::UNAUTHORIZED);

        // Correct token -> 200.
        let ok = client
            .get(format!("{base}/v1/approvals"))
            .header("x-jarvis-approval-token", "test-token")
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(ok, reqwest::StatusCode::OK);
    }

    #[tokio::test]
    async fn health_and_check_remain_unauthenticated_when_token_is_set() {
        let (engine, _dir) =
            test_engine_with_cap_and_token("http-token-public", 10, Some("t".into())).await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        let health = client
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(health, reqwest::StatusCode::OK);

        let check = client
            .post(format!("{base}/v1/check"))
            .json(&serde_json::json!({
                "principal": "jarvis",
                "action": "execute",
                "resource": "tool:filesystem.read",
                "context": {}
            }))
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(check, reqwest::StatusCode::OK);
    }

    /// I15-B: a healthy engine keeps the historical 200 contract and returns a
    /// minimal JSON body without any reason field.
    #[tokio::test]
    async fn health_reports_ok_without_reason_when_service_is_healthy() {
        let (engine, _dir) = test_engine("http-health-ok").await;
        let base = start_server(engine).await;
        let response = reqwest::Client::new()
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("health request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("health body is JSON");
        assert_eq!(body, json!({ "status": "ok" }));
        assert!(body.get("reason").is_none());
    }

    /// I15-B: an activated kill switch is visible as 503/degraded, and the
    /// free-form reason string plus configured paths never reach the body.
    /// Deactivation must restore the normal 200 response.
    #[tokio::test]
    async fn health_reports_kill_switch_degradation_without_leaking_details() {
        let (engine, _dir) = test_engine("http-health-kill-switch").await;
        let shared = engine.clone();
        let probe_reason = "SENTINEL-KILL-REASON-DO-NOT-LEAK";
        engine
            .activate_kill_switch(probe_reason.to_string(), "test-admin-token")
            .expect("activate kill switch");

        let base = start_server(engine).await;
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("health request");
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let text = response.text().await.expect("health body text");
        assert!(
            !text.contains(probe_reason),
            "kill-switch reason must not leak"
        );
        assert!(!text.contains("casbin"), "policy paths must not leak");
        let body: Value = serde_json::from_str(&text).expect("health body is JSON");
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["reason"], "kill_switch");

        shared
            .deactivate_kill_switch("test-admin-token")
            .expect("deactivate kill switch");
        let recovered = client
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("health request");
        assert_eq!(recovered.status(), reqwest::StatusCode::OK);
    }

    /// I15-B: deleting the policy file makes the probe fail closed with a
    /// generic reason; the removed path itself is not echoed back.
    #[tokio::test]
    async fn health_reports_degraded_when_policy_file_is_missing() {
        let (engine, dir) = test_engine("http-health-no-policy").await;
        let policy_path = PathBuf::from(&engine.config.casbin_policy_path);
        std::fs::remove_file(&policy_path).expect("remove policy file");

        let base = start_server(engine).await;
        let response = reqwest::Client::new()
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("health request");
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let text = response.text().await.expect("health body text");
        assert!(
            !text.contains(&policy_path.to_string_lossy().into_owned()),
            "policy path must not leak"
        );
        let body: Value = serde_json::from_str(&text).expect("health body is JSON");
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["reason"], "no_policy");

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// P0-1 opt-in guard: with no token configured, approval endpoints fail
    /// closed with 503 while public endpoints keep working.
    #[tokio::test]
    async fn approval_endpoints_fail_closed_when_required_token_is_unset() {
        let (engine, _dir) =
            test_engine_with_approval_config("http-token-required-unset", 10, None, true).await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        for status in [
            client
                .get(format!("{base}/v1/approvals"))
                .send()
                .await
                .expect("list request")
                .status(),
            client
                .get(format!("{base}/v1/approval/{}", uuid::Uuid::new_v4()))
                .send()
                .await
                .expect("status request")
                .status(),
            client
                .post(format!("{base}/v1/approval"))
                .json(&pending_request(chrono::Utc::now().timestamp() + 60))
                .send()
                .await
                .expect("request request")
                .status(),
        ] {
            assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
        }

        // The guard only affects approval routes.
        let health = client
            .get(format!("{base}/health"))
            .send()
            .await
            .expect("health request")
            .status();
        assert_eq!(health, reqwest::StatusCode::OK);
        let check = client
            .post(format!("{base}/v1/check"))
            .json(&json!({
                "principal": "jarvis",
                "action": "execute",
                "resource": "tool:filesystem.read",
                "context": {}
            }))
            .send()
            .await
            .expect("check request")
            .status();
        assert_eq!(check, reqwest::StatusCode::OK);
    }

    /// With the guard on and a token configured, the normal token gate
    /// applies as before.
    #[tokio::test]
    async fn approval_endpoints_enforce_token_when_guard_is_enabled() {
        let (engine, _dir) = test_engine_with_approval_config(
            "http-token-required-set",
            10,
            Some("test-token".into()),
            true,
        )
        .await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        let missing = client
            .get(format!("{base}/v1/approvals"))
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(missing, reqwest::StatusCode::UNAUTHORIZED);

        let wrong = client
            .get(format!("{base}/v1/approvals"))
            .header("x-jarvis-approval-token", "wrong")
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(wrong, reqwest::StatusCode::UNAUTHORIZED);

        let correct = client
            .get(format!("{base}/v1/approvals"))
            .header("x-jarvis-approval-token", "test-token")
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(correct, reqwest::StatusCode::OK);
    }

    /// Default stays loopback-only: no token and no guard means the routes
    /// remain reachable, so existing deployments are unaffected.
    #[tokio::test]
    async fn approval_endpoints_stay_open_by_default_without_token() {
        let (engine, _dir) =
            test_engine_with_approval_config("http-token-default-open", 10, None, false).await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        let list = client
            .get(format!("{base}/v1/approvals"))
            .send()
            .await
            .expect("request")
            .status();
        assert_eq!(list, reqwest::StatusCode::OK);
    }

    /// POST a pending approval carrying the given token and keep the blocking
    /// request task alive until a decision arrives.
    fn spawn_approval_request(
        client: &reqwest::Client,
        base: &str,
        token: &str,
    ) -> tokio::task::JoinHandle<Value> {
        let client = client.clone();
        let url = format!("{base}/v1/approval");
        let token = token.to_string();
        tokio::spawn(async move {
            client
                .post(url)
                .header("x-jarvis-approval-token", token)
                .json(&pending_request(chrono::Utc::now().timestamp() + 60))
                .send()
                .await
                .expect("approval request runs")
                .json::<Value>()
                .await
                .expect("approval response is JSON")
        })
    }

    /// `/v1/approvals` is API-scoped, so the list poll must send the API token.
    async fn wait_for_pending_with_token(
        client: &reqwest::Client,
        base: &str,
        token: &str,
    ) -> String {
        for _ in 0..50 {
            let body: Value = client
                .get(format!("{base}/v1/approvals"))
                .header("x-jarvis-approval-token", token)
                .send()
                .await
                .expect("list request runs")
                .json()
                .await
                .expect("list response is JSON");
            if let Some(id) = body["pending"]
                .as_array()
                .and_then(|pending| pending.first())
                .and_then(|request| request["id"].as_str())
            {
                return id.to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("approval request never appeared");
    }

    /// I7-A separation: the gateway API token can submit and read a request,
    /// but cannot approve/deny it; that requires the distinct admin token.
    #[tokio::test]
    async fn admin_token_splits_decision_authority_from_api_token() {
        let (engine, _dir) = test_engine_with_full_approval_config(
            "http-admin-split",
            10,
            Some("gateway-api-token".into()),
            Some("admin-secret".into()),
            false,
        )
        .await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        let pending = spawn_approval_request(&client, &base, "gateway-api-token");
        let id = wait_for_pending_with_token(&client, &base, "gateway-api-token").await;

        // request/list/status stay on the API token.
        let status = client
            .get(format!("{base}/v1/approval/{id}"))
            .header("x-jarvis-approval-token", "gateway-api-token")
            .send()
            .await
            .expect("status request")
            .status();
        assert_eq!(status, reqwest::StatusCode::OK);

        // The admin token does not unlock API-scoped routes.
        let list_with_admin = client
            .get(format!("{base}/v1/approvals"))
            .header("x-jarvis-approval-token", "admin-secret")
            .send()
            .await
            .expect("list request")
            .status();
        assert_eq!(list_with_admin, reqwest::StatusCode::UNAUTHORIZED);

        // Holding only the API token is not enough to decide the request.
        for action in ["approve", "deny"] {
            let payload = if action == "approve" {
                json!({ "approved_by": "insider" })
            } else {
                json!({ "reason": "insider" })
            };
            let rejected = client
                .post(format!("{base}/v1/approval/{id}/{action}"))
                .header("x-jarvis-approval-token", "gateway-api-token")
                .json(&payload)
                .send()
                .await
                .expect("decision request")
                .status();
            assert_eq!(rejected, reqwest::StatusCode::UNAUTHORIZED);
        }

        // Only the admin token can approve.
        let approved = client
            .post(format!("{base}/v1/approval/{id}/approve"))
            .header("x-jarvis-approval-token", "admin-secret")
            .json(&json!({ "approved_by": "admin" }))
            .send()
            .await
            .expect("approve request")
            .status();
        assert_eq!(approved, reqwest::StatusCode::OK);

        let decision = pending.await.expect("request task joins");
        assert_eq!(decision["decision"], "Approved");
        assert_eq!(decision["approved_by"], "admin");
    }

    /// I7-A compatibility: with no admin token configured, the API token
    /// still authorizes approve/deny exactly as before.
    #[tokio::test]
    async fn api_token_still_approves_when_admin_token_is_unset() {
        let (engine, _dir) = test_engine_with_full_approval_config(
            "http-admin-compat",
            10,
            Some("gateway-api-token".into()),
            None,
            false,
        )
        .await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();

        let pending = spawn_approval_request(&client, &base, "gateway-api-token");
        let id = wait_for_pending_with_token(&client, &base, "gateway-api-token").await;

        let approved = client
            .post(format!("{base}/v1/approval/{id}/approve"))
            .header("x-jarvis-approval-token", "gateway-api-token")
            .json(&json!({ "approved_by": "gateway-user" }))
            .send()
            .await
            .expect("approve request")
            .status();
        assert_eq!(approved, reqwest::StatusCode::OK);

        let decision = pending.await.expect("request task joins");
        assert_eq!(decision["decision"], "Approved");
        assert_eq!(decision["approved_by"], "gateway-user");

        // The token gate is still enforced on decisions.
        let missing = client
            .post(format!("{base}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "no-token" }))
            .send()
            .await
            .expect("approve request")
            .status();
        assert_eq!(missing, reqwest::StatusCode::UNAUTHORIZED);

        // deny keeps the same legacy fallback (404 means auth passed).
        let deny_with_api = client
            .post(format!("{base}/v1/approval/{}/deny", uuid::Uuid::new_v4()))
            .header("x-jarvis-approval-token", "gateway-api-token")
            .json(&json!({ "reason": "compat" }))
            .send()
            .await
            .expect("deny request")
            .status();
        assert_eq!(deny_with_api, reqwest::StatusCode::NOT_FOUND);

        let deny_without_token = client
            .post(format!("{base}/v1/approval/{}/deny", uuid::Uuid::new_v4()))
            .json(&json!({ "reason": "compat" }))
            .send()
            .await
            .expect("deny request")
            .status();
        assert_eq!(deny_without_token, reqwest::StatusCode::UNAUTHORIZED);
    }

    /// I9-A: a caller-supplied id / requested_at are never adopted. The server
    /// generates both and echoes them in the response; the legacy decision
    /// fields stay at the top level so existing consumers keep working.
    #[tokio::test]
    async fn approval_request_uses_server_generated_id_and_requested_at() {
        let caller_id = uuid::Uuid::new_v4().to_string();
        let mut body = pending_request(chrono::Utc::now().timestamp() + 60);
        body["id"] = json!(caller_id.clone());
        body["requested_at"] = json!(0i64); // caller claims epoch 0
        let before = chrono::Utc::now();

        let (response, dir) = submit_and_approve(body, "http-server-generated").await;

        let id = response["id"].as_str().expect("response carries id");
        assert_ne!(id, caller_id, "caller id must not be adopted");
        uuid::Uuid::parse_str(id).expect("response id is a UUID");

        let requested_at = chrono::DateTime::parse_from_rfc3339(
            response["requested_at"]
                .as_str()
                .expect("response carries requested_at"),
        )
        .expect("requested_at is RFC 3339");
        assert_ne!(requested_at.timestamp(), 0, "epoch 0 must not leak through");
        assert!(requested_at.timestamp() >= before.timestamp() - 1);
        assert!(requested_at.timestamp() <= chrono::Utc::now().timestamp() + 1);

        assert_eq!(response["decision"], "Approved");
        assert_eq!(response["approved_by"], "tester");

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// I9-A: malformed identity/timestamps are rejected with 400 instead of
    /// being silently replaced with a new UUID or epoch 0.
    #[tokio::test]
    async fn approval_request_rejects_malformed_caller_fields() {
        let (engine, dir) = test_engine("http-malformed-input").await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();
        let now = chrono::Utc::now().timestamp();

        let mut bad_id = pending_request(now + 60);
        bad_id["id"] = json!("not-a-uuid");
        assert_eq!(
            post_approval(&client, &base, &bad_id).await,
            reqwest::StatusCode::BAD_REQUEST
        );

        let mut bad_requested_at = pending_request(now + 60);
        bad_requested_at["requested_at"] = json!(i64::MAX);
        assert_eq!(
            post_approval(&client, &base, &bad_requested_at).await,
            reqwest::StatusCode::BAD_REQUEST
        );

        let mut bad_expires_at = pending_request(now + 60);
        bad_expires_at["expires_at"] = json!(i64::MIN);
        assert_eq!(
            post_approval(&client, &base, &bad_expires_at).await,
            reqwest::StatusCode::BAD_REQUEST
        );

        let past_expires_at = pending_request(now - 60);
        assert_eq!(
            post_approval(&client, &base, &past_expires_at).await,
            reqwest::StatusCode::BAD_REQUEST
        );

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// I9-A: `expires_at` can never exceed the configured timeout (5s here),
    /// even for a far-future hint, and the effective deadline is returned.
    #[tokio::test]
    async fn approval_request_caps_expires_at_to_server_timeout() {
        let before = chrono::Utc::now();
        let far_future = before + chrono::Duration::seconds(86_400);
        let mut body = pending_request(far_future.timestamp());
        body["expires_at"] = json!(far_future.timestamp());

        let (response, dir) = submit_and_approve(body, "http-expires-cap").await;

        let expires_at = chrono::DateTime::parse_from_rfc3339(
            response["expires_at"]
                .as_str()
                .expect("response carries expires_at"),
        )
        .expect("expires_at is RFC 3339");
        let cap = before
            + chrono::Duration::from_std(std::time::Duration::from_secs(5))
                .expect("chrono duration");
        assert!(
            expires_at.timestamp() <= cap.timestamp() + 1,
            "deadline must be capped at the server timeout"
        );
        assert!(expires_at.timestamp() > before.timestamp());

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// I9-A: a shorter caller hint is still honoured (min with the server cap),
    /// and the stored pending request carries the same effective deadline.
    #[tokio::test]
    async fn approval_request_honours_shorter_expires_at_hint() {
        let before = chrono::Utc::now();
        let short = before + chrono::Duration::seconds(2);
        let mut body = pending_request(short.timestamp());
        body["expires_at"] = json!(short.timestamp());

        let (engine, dir) = test_engine("http-expires-short").await;
        let base = start_server(engine).await;
        let client = reqwest::Client::new();
        let request = tokio::spawn({
            let client = client.clone();
            let url = format!("{base}/v1/approval");
            async move {
                client
                    .post(url)
                    .json(&body)
                    .send()
                    .await
                    .expect("approval request runs")
                    .json::<Value>()
                    .await
                    .expect("approval response is JSON")
            }
        });
        let id = wait_for_pending(&client, &base).await;

        let pending: Value = client
            .get(format!("{base}/v1/approvals"))
            .send()
            .await
            .expect("list request")
            .json()
            .await
            .expect("list JSON");
        let stored = pending["pending"]
            .as_array()
            .expect("pending list")
            .iter()
            .find(|entry| entry["id"].as_str() == Some(id.as_str()))
            .expect("submitted request is pending");
        let stored_expires_at = chrono::DateTime::parse_from_rfc3339(
            stored["expires_at"]
                .as_str()
                .expect("pending carries expires_at"),
        )
        .expect("pending expires_at is RFC 3339");
        assert!(stored_expires_at.timestamp() <= short.timestamp() + 1);

        client
            .post(format!("{base}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "tester" }))
            .send()
            .await
            .expect("approve request");
        let response = request.await.expect("request task joins");
        let expires_at = chrono::DateTime::parse_from_rfc3339(
            response["expires_at"]
                .as_str()
                .expect("response carries expires_at"),
        )
        .expect("expires_at is RFC 3339");
        assert_eq!(expires_at.timestamp(), stored_expires_at.timestamp());

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }
}
