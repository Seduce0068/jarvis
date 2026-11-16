//! Loopback HTTP client for the `jarvis-policy` approval API.
//!
//! Endpoints (see `rust/jarvis-policy/src/http.rs`):
//! - `GET  /v1/approvals`             -> `{ "pending": [ApprovalRequest, ..] }`
//! - `POST /v1/approval/{id}/approve` -> `{ "decision": "Approved", .. }`
//! - `POST /v1/approval/{id}/deny`    -> `{ "decision": "Denied", .. }`
//!
//! This client is intentionally **blocking**: GPUI drives its own executor,
//! so a Tokio-based client such as reqwest has no reactor here. Callers must
//! run these methods on GPUI's background executor (see `approvals.rs`).
//! The console only ever talks to the configured loopback base URL.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A pending approval request as returned by `jarvis-policy`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ApprovalRequest {
    pub id: String,
    pub principal: String,
    pub action: String,
    pub resource: String,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub requested_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
struct PendingResponse {
    #[serde(default)]
    pending: Vec<ApprovalRequest>,
}

#[derive(Serialize)]
struct ApproveBody<'a> {
    approved_by: &'a str,
}

#[derive(Serialize)]
struct DenyBody<'a> {
    reason: &'a str,
}

/// Overall deadline for a single policy HTTP exchange.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Connect deadline (loopback connects are immediate).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Blocking client for the policy approval endpoints.
#[derive(Clone)]
pub struct PolicyClient {
    base_url: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl PolicyClient {
    /// Convenience constructor (used by tests and embedders).
    #[allow(dead_code)]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::new_with_token(base_url, None)
    }

    pub fn new_with_token(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self::with_timeouts(base_url, token, REQUEST_TIMEOUT, CONNECT_TIMEOUT)
    }

    /// Constructor with explicit deadlines (used by timeout tests).
    #[allow(dead_code)]
    fn with_timeouts(
        base_url: impl Into<String>,
        token: Option<String>,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        // Both polling and decision posts are short; a hung policy service
        // must not occupy a GPUI background thread indefinitely.
        let config = ureq::config::Config::builder()
            .timeout_global(Some(request_timeout))
            .timeout_connect(Some(connect_timeout))
            .build();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token,
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// Requests currently awaiting an operator decision.
    pub fn pending(&self) -> Result<Vec<ApprovalRequest>> {
        let url = format!("{}/v1/approvals", self.base_url);
        let mut request = self.agent.get(&url);
        if let Some(token) = &self.token {
            request = request.header("X-Jarvis-Approval-Token", token);
        }
        let mut response = request
            .call()
            .with_context(|| format!("GET {url} failed"))?;
        let body: PendingResponse = response
            .body_mut()
            .read_json()
            .context("invalid JSON from /v1/approvals")?;
        Ok(body.pending)
    }

    /// Approve the request `id` as `approved_by`.
    pub fn approve(&self, id: &str, approved_by: &str) -> Result<()> {
        self.decide(id, "approve", &ApproveBody { approved_by })
    }

    /// Deny the request `id` with `reason`.
    pub fn deny(&self, id: &str, reason: &str) -> Result<()> {
        self.decide(id, "deny", &DenyBody { reason })
    }

    fn decide<B: Serialize + ?Sized>(&self, id: &str, verb: &str, body: &B) -> Result<()> {
        let url = format!("{}/v1/approval/{id}/{verb}", self.base_url);
        let mut request = self.agent.post(&url);
        if let Some(token) = &self.token {
            request = request.header("X-Jarvis-Approval-Token", token);
        }
        request
            .send_json(body)
            .with_context(|| format!("POST {url} failed"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pending_list_and_tolerates_extra_fields() {
        let body = serde_json::json!({
            "pending": [{
                "id": "4d1a1f2e-0000-0000-0000-000000000000",
                "principal": "opencode",
                "action": "execute",
                "resource": "tool:shell.execute",
                "context": {"cmd": "ls"},
                "requested_at": "2026-11-15T08:00:00Z",
                "expires_at": "2026-11-15T08:05:00Z",
                "unknown_future_field": true
            }]
        });
        let parsed: PendingResponse = serde_json::from_value(body).expect("parses");
        assert_eq!(parsed.pending.len(), 1);
        assert_eq!(parsed.pending[0].principal, "opencode");
        assert_eq!(parsed.pending[0].resource, "tool:shell.execute");
    }

    #[test]
    fn empty_pending_list_is_accepted() {
        let parsed: PendingResponse =
            serde_json::from_value(serde_json::json!({ "pending": [] })).expect("parses");
        assert!(parsed.pending.is_empty());
    }

    #[test]
    fn base_url_trailing_slash_is_normalized() {
        let client = PolicyClient::new("http://127.0.0.1:8081/");
        assert_eq!(client.base_url, "http://127.0.0.1:8081");
    }

    #[test]
    fn hung_policy_endpoint_times_out() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test address");
        let handle = std::thread::spawn(move || {
            // Accept the connection and keep it open without answering.
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            std::thread::sleep(Duration::from_millis(600));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        });

        let client = PolicyClient::with_timeouts(
            format!("http://{addr}"),
            None,
            Duration::from_millis(150),
            Duration::from_secs(1),
        );
        let started = std::time::Instant::now();
        let result = client.pending();
        assert!(result.is_err(), "hung endpoint must error: {result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must fail within the configured deadline"
        );
        let _ = handle.join();
    }

    /// The UI must send the configured token to jarvis-policy as well.
    #[test]
    fn approval_token_is_sent_as_header() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen_for_thread = seen.clone();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let token = request
                .lines()
                .find(|line| {
                    line.to_ascii_lowercase()
                        .starts_with("x-jarvis-approval-token:")
                })
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_string());
            *seen_for_thread.lock().expect("lock") = token;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n{\"pending\":[]}")
                .expect("write");
        });

        let client =
            PolicyClient::new_with_token(format!("http://{addr}"), Some("ui-token".to_string()));
        // The request fails to parse only if the body is malformed; an empty
        // pending list is valid, so this must succeed.
        let result = client.pending();
        handle.join().expect("server thread");
        assert!(result.is_ok(), "unexpected: {result:?}");
        assert_eq!(seen.lock().expect("lock").as_deref(), Some("ui-token"));
    }
}
