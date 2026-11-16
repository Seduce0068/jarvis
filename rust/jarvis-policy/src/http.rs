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

#[derive(Deserialize, Debug)]
struct ApprovalRequestHttp {
    id: String,
    principal: String,
    action: String,
    resource: String,
    context: serde_json::Value,
    requested_at: i64,
    expires_at: i64,
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

// SECURITY NOTE (2026-11-16): the approval endpoints below are intentionally
// unauthenticated. The HTTP listener binds to 127.0.0.1 and the host is a
// single-user desktop with systemd service isolation. They are NOT safe to
// expose beyond loopback: any local process could list, approve or deny
// pending requests (and the MCP gateway's approval client relies on this).
// Adding auth requires coordinated changes to jarvis-mcp-gw, ui/, and this
// router; it is tracked in docs/integration/remaining-tasks.md.

pub fn router(engine: Arc<PolicyEngine>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/check", post(check))
        .route("/v1/approval", post(request_approval))
        .route("/v1/approvals", get(list_approvals))
        .route("/v1/approval/:id", get(approval_status))
        .route("/v1/approval/:id/approve", post(approve_approval))
        .route("/v1/approval/:id/deny", post(deny_approval))
        .route("/v1/kill-switch", get(kill_switch_status))
        .route("/v1/kill-switch/activate", post(activate_kill_switch))
        .route("/v1/kill-switch/deactivate", post(deactivate_kill_switch))
        .with_state(engine)
        .layer(axum::middleware::from_fn(track_metrics))
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

#[instrument(skip(engine))]
async fn check(
    State(engine): State<Arc<PolicyEngine>>,
    Json(req): Json<CheckRequest>,
) -> Json<CheckResponse> {
    let allowed = engine
        .is_allowed(&req.principal, &req.action, &req.resource, &req.context)
        .await
        .unwrap_or(false);
    Json(CheckResponse { allowed })
}

#[instrument(skip(engine))]
async fn request_approval(
    State(engine): State<Arc<PolicyEngine>>,
    Json(req): Json<ApprovalRequestHttp>,
) -> Json<serde_json::Value> {
    let approval_req = ApprovalRequest {
        id: uuid::Uuid::parse_str(&req.id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
        principal: req.principal,
        action: req.action,
        resource: req.resource,
        context: req.context,
        requested_at: chrono::DateTime::from_timestamp(req.requested_at, 0).unwrap_or_default(),
        expires_at: chrono::DateTime::from_timestamp(req.expires_at, 0).unwrap_or_default(),
    };
    let decision =
        engine
            .request_approval(approval_req)
            .await
            .unwrap_or(ApprovalDecision::Denied {
                reason: "error".into(),
            });
    Json(serde_json::to_value(decision).unwrap_or_default())
}

#[instrument(skip(engine))]
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

#[instrument(skip(engine))]
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

    async fn test_engine(label: &str) -> (Arc<PolicyEngine>, PathBuf) {
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
            .post(format!("{base}/v1/approval/{id}/deny"))
            .json(&json!({ "reason": "not now" }))
            .send()
            .await
            .expect("deny request runs");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let decision = pending.await.expect("request task joins");
        assert_eq!(decision["decision"], "Denied");
        assert_eq!(decision["reason"], "not now");

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }
}
