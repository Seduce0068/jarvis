//! Human approval client for the policy engine's HTTP surface.
//!
//! Approval is intentionally HTTP-only on the policy side (see
//! `jarvis-policy/src/approval.rs`): a long-running human decision must not
//! occupy a gRPC stream. The gateway therefore POSTs the approval request to
//! `POST /v1/approval` and awaits the decision on the same response, while
//! the operator approves or denies via
//! `POST /v1/approval/{id}/approve|deny` (pending requests are listed by
//! `GET /v1/approvals`).
//!
//! The request blocks until the operator decides or the policy-side approval
//! timeout elapses, so callers must treat a slow response as part of the
//! human-in-the-loop flow.

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// How long the approval request stays actionable, in seconds.
///
/// This mirrors the default `approval_timeout_secs` of `jarvis-policy`; the
/// effective deadline is the earlier of this window and the policy-side
/// timeout.
const APPROVAL_WINDOW_SECS: i64 = 300;

/// Overall HTTP deadline: the approval window plus slack for transport and
/// serialization. Without this, a wedged policy service could hang a tool
/// call forever (a denial-by-hang is not fail-closed in a useful time).
const APPROVAL_HTTP_TIMEOUT_SECS: u64 = APPROVAL_WINDOW_SECS as u64 + 30;

/// Connect deadline. Loopback connects are immediate; a slow connect means
/// something is wrong and should fail fast rather than consume the window.
const CONNECT_TIMEOUT_SECS: u64 = 2;

/// Request body for `POST /v1/approval` on `jarvis-policy`.
#[derive(Serialize, Debug)]
struct ApprovalRequestHttp<'a> {
    id: String,
    principal: &'a str,
    action: &'a str,
    resource: &'a str,
    context: &'a serde_json::Value,
    requested_at: i64,
    expires_at: i64,
}

/// Decision returned by `jarvis-policy`.
///
/// Mirrors `jarvis_policy::policy::ApprovalDecision` (serde tagged with
/// `"decision"`); `approved_at` is an RFC 3339 timestamp string.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "decision")]
pub enum ApprovalOutcome {
    Approved {
        approved_by: String,
        approved_at: String,
    },
    Denied {
        reason: String,
    },
    Pending,
    Expired,
}

/// HTTP client for the policy approval workflow.
#[derive(Clone)]
pub struct ApprovalClient {
    endpoint: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl ApprovalClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self::new_with_token(endpoint, None)
    }

    pub fn new_with_token(endpoint: impl Into<String>, token: Option<String>) -> Self {
        Self::with_timeouts(
            endpoint,
            token,
            Duration::from_secs(APPROVAL_HTTP_TIMEOUT_SECS),
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
        )
    }

    /// Constructor with explicit deadlines (used by tests to keep them fast).
    fn with_timeouts(
        endpoint: impl Into<String>,
        token: Option<String>,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .connect_timeout(connect_timeout)
            .build()
            .expect("approval HTTP client must build");
        Self {
            endpoint: endpoint.into(),
            token,
            http,
        }
    }

    /// Request human approval for `action` on `resource` and wait for the
    /// decision.
    ///
    /// Transport and protocol errors are surfaced to the caller so
    /// [`crate::McpGateway::execute_tool`] can deny the call (fail-closed).
    pub async fn request_approval(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> Result<ApprovalOutcome> {
        let now = Utc::now();
        let body = ApprovalRequestHttp {
            id: Uuid::new_v4().to_string(),
            principal,
            action,
            resource,
            context,
            requested_at: now.timestamp(),
            expires_at: (now + ChronoDuration::seconds(APPROVAL_WINDOW_SECS)).timestamp(),
        };
        let mut request = self.http.post(format!("{}/v1/approval", self.endpoint));
        if let Some(token) = &self.token {
            request = request.header("X-Jarvis-Approval-Token", token);
        }
        let response = request
            .json(&body)
            .send()
            .await
            .with_context(|| format!("approval request to {} failed", self.endpoint))?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("approval endpoint {} returned {}", self.endpoint, status);
        }
        let outcome: ApprovalOutcome = response
            .json()
            .await
            .with_context(|| format!("invalid approval response from {}", self.endpoint))?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::start_policy_http;
    use serde_json::json;

    #[test]
    fn decisions_deserialize_from_policy_shape() {
        let approved: ApprovalOutcome = serde_json::from_value(json!({
            "decision": "Approved",
            "approved_by": "koppe",
            "approved_at": "2026-11-15T08:00:00Z",
        }))
        .expect("approved decision parses");
        assert_eq!(
            approved,
            ApprovalOutcome::Approved {
                approved_by: "koppe".into(),
                approved_at: "2026-11-15T08:00:00Z".into(),
            }
        );

        let denied: ApprovalOutcome = serde_json::from_value(json!({
            "decision": "Denied",
            "reason": "not now",
        }))
        .expect("denied decision parses");
        assert_eq!(
            denied,
            ApprovalOutcome::Denied {
                reason: "not now".into()
            }
        );

        let pending: ApprovalOutcome =
            serde_json::from_value(json!({ "decision": "Pending" })).expect("pending parses");
        assert_eq!(pending, ApprovalOutcome::Pending);
    }

    #[tokio::test]
    async fn approval_round_trip_is_approved_by_operator() {
        let base = start_policy_http("approved").await;
        let client = ApprovalClient::new(base.clone());
        let http = reqwest::Client::new();

        let requester = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .request_approval("opencode", "execute", "tool:shell.execute", &json!({}))
                    .await
            }
        });

        // Wait for the pending request, then approve it as the operator.
        let id = loop {
            let pending: serde_json::Value = http
                .get(format!("{base}/v1/approvals"))
                .send()
                .await
                .expect("list approvals")
                .json()
                .await
                .expect("pending list is JSON");
            let first = pending["pending"]
                .as_array()
                .and_then(|items| items.first())
                .cloned();
            if let Some(entry) = first {
                break entry["id"].as_str().expect("id is a string").to_owned();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let decision: serde_json::Value = http
            .post(format!("{base}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "koppe" }))
            .send()
            .await
            .expect("approve request")
            .json()
            .await
            .expect("approve response is JSON");
        assert_eq!(decision["decision"], "Approved");

        let outcome = requester
            .await
            .expect("requester task")
            .expect("approval outcome");
        assert!(
            matches!(outcome, ApprovalOutcome::Approved { ref approved_by, .. } if approved_by == "koppe"),
            "unexpected outcome: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn approval_round_trip_surfaces_denial() {
        let base = start_policy_http("denied").await;
        let client = ApprovalClient::new(base.clone());
        let http = reqwest::Client::new();

        let requester = tokio::spawn(async move {
            client
                .request_approval("opencode", "execute", "tool:shell.execute", &json!({}))
                .await
        });

        let id = loop {
            let pending: serde_json::Value = http
                .get(format!("{base}/v1/approvals"))
                .send()
                .await
                .expect("list approvals")
                .json()
                .await
                .expect("pending list is JSON");
            let first = pending["pending"]
                .as_array()
                .and_then(|items| items.first())
                .cloned();
            if let Some(entry) = first {
                break entry["id"].as_str().expect("id is a string").to_owned();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let decision: serde_json::Value = http
            .post(format!("{base}/v1/approval/{id}/deny"))
            .json(&json!({ "reason": "not now" }))
            .send()
            .await
            .expect("deny request")
            .json()
            .await
            .expect("deny response is JSON");
        assert_eq!(decision["decision"], "Denied");

        let outcome = requester
            .await
            .expect("requester task")
            .expect("approval outcome");
        assert_eq!(
            outcome,
            ApprovalOutcome::Denied {
                reason: "not now".into(),
            }
        );
    }

    #[tokio::test]
    async fn unreachable_policy_fails_closed_with_error() {
        // Port 1 is reserved and refuses connections; the client must surface
        // the error so the gateway can deny the call.
        let client = ApprovalClient::new("http://127.0.0.1:1");
        let outcome = client
            .request_approval("opencode", "execute", "tool:shell.execute", &json!({}))
            .await;
        assert!(
            outcome.is_err(),
            "unreachable policy must error: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn hung_policy_endpoint_times_out_and_denies() {
        // A policy service that accepts the connection but never answers must
        // not hang the tool call forever; the HTTP deadline makes it fail
        // closed within a bounded time.
        let app = axum::Router::new().route(
            "/v1/approval",
            axum::routing::post(|| async {
                std::future::pending::<()>().await;
                ""
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind hung test server");
        let base = format!("http://{}", listener.local_addr().expect("test address"));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("hung test server runs");
        });

        let client = ApprovalClient::with_timeouts(
            base,
            None,
            Duration::from_millis(200),
            Duration::from_secs(1),
        );
        let started = tokio::time::Instant::now();
        let outcome = client
            .request_approval("opencode", "execute", "tool:shell.execute", &json!({}))
            .await;
        assert!(outcome.is_err(), "hung endpoint must error: {outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must fail within the configured deadline, not hang"
        );
    }
}
