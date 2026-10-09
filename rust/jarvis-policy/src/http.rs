//! HTTP/REST + Metrics endpoint for Policy Engine

use axum::{Router, routing::{get, post}, Json, extract::State};
use std::sync::Arc;
use crate::PolicyEngine;
use crate::policy::{ApprovalRequest, ApprovalDecision};
use serde::{Deserialize, Serialize};
use tracing::instrument;

#[derive(Deserialize, Debug)]
struct CheckRequest {
    principal: String,
    action: String,
    resource: String,
    context: serde_json::Value,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
struct ApprovalResponseHttp {
    decision: String,
}

pub fn router(engine: Arc<PolicyEngine>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/check", post(check))
        .route("/v1/approval", post(request_approval))
        .route("/v1/kill-switch", get(kill_switch_status))
        .route("/v1/kill-switch/activate", post(activate_kill_switch))
        .route("/v1/kill-switch/deactivate", post(deactivate_kill_switch))
        .with_state(engine)
}

async fn health() -> &'static str {
    "OK"
}

async fn metrics() -> String {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    handle.render()
}

#[instrument(skip(engine))]
async fn check(State(engine): State<Arc<PolicyEngine>>, Json(req): Json<CheckRequest>) -> Json<CheckResponse> {
    let allowed = engine.is_allowed(&req.principal, &req.action, &req.resource, &req.context).await.unwrap_or(false);
    Json(CheckResponse { allowed })
}

#[instrument(skip(engine))]
async fn request_approval(State(engine): State<Arc<PolicyEngine>>, Json(req): Json<ApprovalRequestHttp>) -> Json<ApprovalResponseHttp> {
    let approval_req = ApprovalRequest {
        id: uuid::Uuid::parse_str(&req.id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
        principal: req.principal,
        action: req.action,
        resource: req.resource,
        context: req.context,
        requested_at: chrono::DateTime::from_timestamp(req.requested_at, 0).unwrap_or_default(),
        expires_at: chrono::DateTime::from_timestamp(req.expires_at, 0).unwrap_or_default(),
    };
    let decision = engine.request_approval(approval_req).await.unwrap_or(ApprovalDecision::Denied { reason: "error".into() });
    Json(ApprovalResponseHttp { decision: format!("{:?}", decision) })
}

async fn kill_switch_status(State(engine): State<Arc<PolicyEngine>>) -> Json<crate::kill_switch::KillSwitchStatus> {
    Json(engine.kill_switch_status())
}

#[derive(Deserialize)]
struct ActivateKillSwitchRequest {
    reason: String,
}

async fn activate_kill_switch(State(engine): State<Arc<PolicyEngine>>, Json(req): Json<ActivateKillSwitchRequest>) -> &'static str {
    engine.activate_kill_switch(req.reason);
    "activated"
}

#[derive(Deserialize)]
struct DeactivateKillSwitchRequest {
    admin_token: String,
}

async fn deactivate_kill_switch(State(engine): State<Arc<PolicyEngine>>, Json(req): Json<DeactivateKillSwitchRequest>) -> Result<&'static str, axum::http::StatusCode> {
    engine.deactivate_kill_switch(&req.admin_token).map(|_| "deactivated").map_err(|_| axum::http::StatusCode::UNAUTHORIZED)
}

pub async fn serve(engine: Arc<PolicyEngine>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(engine);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}