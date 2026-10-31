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
    // metrics 0.21's label parser rejects `"key" => expr` here, so record the
    // request total without dimensions.
    metrics::counter!("jarvis_http_requests_total").increment(1);
    response
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
}

async fn activate_kill_switch(
    State(engine): State<Arc<PolicyEngine>>,
    Json(req): Json<ActivateKillSwitchRequest>,
) -> &'static str {
    engine.activate_kill_switch(req.reason);
    "activated"
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
