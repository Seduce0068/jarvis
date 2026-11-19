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
//!
//! The window is configurable via `JARVIS_MCP_GW_APPROVAL_WINDOW_SECS`
//! (default 300 seconds). It must be set to the same value as the policy
//! service's `approval_timeout_secs`; both `expires_at` and the HTTP deadline
//! are derived from this one value.

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// Env override for the human approval window, in seconds. Operators must
/// set this to the same value as the policy side's `approval_timeout_secs`.
const APPROVAL_WINDOW_ENV: &str = "JARVIS_MCP_GW_APPROVAL_WINDOW_SECS";

/// Default approval window, in seconds. Must be set to the same value as
/// `jarvis_policy::config::DEFAULT_APPROVAL_TIMEOUT_SECS` (300): the gateway's
/// `expires_at` and the policy-side expiry describe the same deadline.
const DEFAULT_APPROVAL_WINDOW_SECS: i64 = 300;

// Compile-time guard against the two defaults drifting apart.
const _: () = assert!(
    DEFAULT_APPROVAL_WINDOW_SECS as u64 == jarvis_policy::config::DEFAULT_APPROVAL_TIMEOUT_SECS,
    "gateway approval window default must match policy approval timeout default"
);

/// Upper bound for a configured window. Beyond a day an approval is
/// effectively never actionable (fail-open), and the derived HTTP deadline
/// must not overflow. Values above this are rejected like any other invalid
/// input.
const MAX_APPROVAL_WINDOW_SECS: i64 = 24 * 60 * 60;

/// Extra time added to the approval window for transport and serialization.
/// Without this, a slow-but-alive policy service could cut the request off
/// before the operator's deadline elapses.
const HTTP_TIMEOUT_SLACK_SECS: i64 = 30;

/// Connect deadline. Loopback connects are immediate; a slow connect means
/// something is wrong and should fail fast rather than consume the window.
const CONNECT_TIMEOUT_SECS: u64 = 2;

/// Parse a configured approval window, falling back to the safe default on
/// any invalid value. Never fail open: a bogus, non-positive or oversized
/// value must not extend the window.
fn parse_approval_window(raw: Option<&str>) -> i64 {
    match raw {
        None => DEFAULT_APPROVAL_WINDOW_SECS,
        Some(value) => match value.trim().parse::<i64>() {
            Ok(secs) if (1..=MAX_APPROVAL_WINDOW_SECS).contains(&secs) => secs,
            _ => {
                tracing::warn!(
                    value = %value,
                    default = DEFAULT_APPROVAL_WINDOW_SECS,
                    "invalid JARVIS_MCP_GW_APPROVAL_WINDOW_SECS; \
                     falling back to the default approval window"
                );
                DEFAULT_APPROVAL_WINDOW_SECS
            }
        },
    }
}

/// Read the approval window once, at client construction. Both `expires_at`
/// and the HTTP deadline are derived from this single value so they cannot
/// drift apart mid-request.
fn approval_window_secs() -> i64 {
    let raw = std::env::var(APPROVAL_WINDOW_ENV).ok();
    parse_approval_window(raw.as_deref())
}

/// Overall HTTP deadline: the approval window plus transport slack. Without
/// this, a wedged policy service could hang a tool call forever (a
/// denial-by-hang is not fail-closed in a useful time).
fn http_timeout_for_window(window_secs: i64) -> Duration {
    Duration::from_secs((window_secs + HTTP_TIMEOUT_SLACK_SECS) as u64)
}

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
    /// Deadline sent to the policy service as `expires_at`. Read from
    /// `JARVIS_MCP_GW_APPROVAL_WINDOW_SECS` once at construction so it always
    /// matches the HTTP deadline.
    approval_window_secs: i64,
}

impl ApprovalClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self::new_with_token(endpoint, None)
    }

    pub fn new_with_token(endpoint: impl Into<String>, token: Option<String>) -> Self {
        Self::with_window(endpoint, token, approval_window_secs())
    }

    /// Build a client whose HTTP deadline and `expires_at` both come from
    /// `window_secs`. Kept separate from [`Self::with_timeouts`] so tests can
    /// drive the window without mutating the process environment.
    fn with_window(endpoint: impl Into<String>, token: Option<String>, window_secs: i64) -> Self {
        let mut client = Self::with_timeouts(
            endpoint,
            token,
            http_timeout_for_window(window_secs),
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
        );
        client.approval_window_secs = window_secs;
        client
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
            approval_window_secs: DEFAULT_APPROVAL_WINDOW_SECS,
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
            expires_at: (now + ChronoDuration::seconds(self.approval_window_secs)).timestamp(),
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
    fn approval_window_env_name_is_stable() {
        assert_eq!(APPROVAL_WINDOW_ENV, "JARVIS_MCP_GW_APPROVAL_WINDOW_SECS");
    }

    #[test]
    fn approval_window_defaults_to_300_when_unset() {
        assert_eq!(parse_approval_window(None), 300);
        assert_eq!(parse_approval_window(Some("")), 300);
    }

    #[test]
    fn approval_window_accepts_valid_override() {
        assert_eq!(parse_approval_window(Some("42")), 42);
        assert_eq!(parse_approval_window(Some(" 600 ")), 600);
        assert_eq!(
            parse_approval_window(Some("86400")),
            MAX_APPROVAL_WINDOW_SECS
        );
    }

    #[test]
    fn approval_window_invalid_values_fall_back_to_default() {
        for raw in [
            "abc",
            "0",
            "-1",
            "1.5",
            "999999999999999999999",
            "9223372036854775807",
            "86401",
        ] {
            assert_eq!(
                parse_approval_window(Some(raw)),
                DEFAULT_APPROVAL_WINDOW_SECS,
                "invalid window {raw:?} must fall back to the safe default"
            );
        }
    }

    #[test]
    fn approval_window_drives_http_deadline() {
        assert_eq!(
            http_timeout_for_window(DEFAULT_APPROVAL_WINDOW_SECS),
            Duration::from_secs(330)
        );
        assert_eq!(http_timeout_for_window(42), Duration::from_secs(72));
    }

    /// The window configured on the client must be the one sent to the policy
    /// service as `expires_at` (not a hard-coded 300).
    #[tokio::test]
    async fn approval_window_drives_expires_at() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let seen_for_handler = seen.clone();
        let app = axum::Router::new().route(
            "/v1/approval",
            axum::routing::post(move |body: axum::body::Bytes| {
                let seen = seen_for_handler.clone();
                async move {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
                        *seen.lock().expect("lock") = Some(value);
                    }
                    axum::Json(json!({ "decision": "Pending" }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let client = ApprovalClient::with_window(base, None, 42);
        client
            .request_approval("opencode", "execute", "tool:shell.execute", &json!({}))
            .await
            .expect("request succeeds");

        let body = seen.lock().expect("lock").clone().expect("body captured");
        let requested_at = body["requested_at"].as_i64().expect("requested_at");
        let expires_at = body["expires_at"].as_i64().expect("expires_at");
        assert_eq!(expires_at - requested_at, 42);
    }

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

    /// The configured token must reach jarvis-policy as the exact header.
    #[tokio::test]
    async fn token_is_sent_as_approval_header() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_handler = seen.clone();
        let app = axum::Router::new().route(
            "/v1/approval",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let value = headers
                    .get("x-jarvis-approval-token")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                *seen_for_handler.lock().expect("lock") = Some(value);
                async { axum::Json(json!({ "decision": "Pending" })) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let client = ApprovalClient::new_with_token(base, Some("secret-token".into()));
        let _ = client
            .request_approval("opencode", "execute", "tool:shell.execute", &json!({}))
            .await
            .expect("request succeeds");
        assert_eq!(
            seen.lock().expect("lock").as_deref(),
            Some("secret-token"),
            "header must carry the configured token"
        );
    }

    /// Cross-crate E2E: the real jarvis-policy router with token auth must
    /// accept a correctly configured gateway client and reject a wrong one.
    #[tokio::test]
    async fn real_policy_router_enforces_token() {
        use crate::testing::start_policy_http_with_token;

        let base =
            start_policy_http_with_token("approval-auth-e2e", Some("e2e-token".to_string())).await;

        let good = ApprovalClient::new_with_token(base.clone(), Some("e2e-token".into()));
        let http = reqwest::Client::new();
        let approver_base = base.clone();
        let approver = tokio::spawn(async move {
            loop {
                let pending: serde_json::Value = http
                    .get(format!("{approver_base}/v1/approvals"))
                    .header("X-Jarvis-Approval-Token", "e2e-token")
                    .send()
                    .await
                    .expect("list")
                    .json()
                    .await
                    .expect("json");
                if let Some(entry) = pending["pending"].as_array().and_then(|v| v.first()) {
                    let id = entry["id"].as_str().expect("id").to_owned();
                    http.post(format!("{approver_base}/v1/approval/{id}/approve"))
                        .header("X-Jarvis-Approval-Token", "e2e-token")
                        .json(&json!({ "approved_by": "koppe" }))
                        .send()
                        .await
                        .expect("approve");
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
        let outcome = good
            .request_approval("opencode", "execute", "tool:shell.execute", &json!({}))
            .await
            .expect("correct token is accepted");
        approver.await.expect("approver task");
        assert!(
            matches!(outcome, ApprovalOutcome::Approved { .. }),
            "unexpected: {outcome:?}"
        );

        let bad = ApprovalClient::new_with_token(base, Some("wrong-token".into()));
        let error = bad
            .request_approval("opencode", "execute", "tool:shell.execute", &json!({}))
            .await
            .expect_err("wrong token must be rejected by the policy router");
        assert!(error.to_string().contains("401"), "unexpected: {error}");
    }
}
