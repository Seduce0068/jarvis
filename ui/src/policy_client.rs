//! Loopback HTTP client for the `jarvis-policy` approval API.
//!
//! Endpoints (see `rust/jarvis-policy/src/http.rs`):
//! - `GET  /v1/approvals`             -> `{ "pending": [ApprovalRequest, ..] }`
//! - `POST /v1/approval/{id}/approve` -> `{ "decision": "Approved", .. }`
//! - `POST /v1/approval/{id}/deny`    -> `{ "decision": "Denied", .. }`
//!
//! The POST endpoints return a canonical tagged decision JSON. That body is
//! parsed through the core boundary on the real RUN path (never only in tests)
//! before it reaches the UI; see [`decision_from_json`] and [`PolicyClient::decide`].
//!
//! This client is intentionally **blocking**: GPUI drives its own executor,
//! so a Tokio-based client such as reqwest has no reactor here. Callers must
//! run these methods on GPUI's background executor (see `approvals.rs`).
//! The console only ever talks to the configured loopback base URL.

use anyhow::{Context as _, Result, bail};
use jarvis_core::approval::{ApprovalDecision as Decision, ApprovalRequest as CoreApprovalRequest};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A pending approval request as returned by `jarvis-policy`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ApprovalRequest {
    /// Legacy peers may omit the id entirely; `""` keeps such an entry
    /// renderable instead of failing the whole poll. Canonical conversion
    /// rejects it explicitly at the boundary (see `request_to_core`).
    #[serde(default)]
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

/// Thin wire DTO for decision JSON returned by `jarvis-policy`.
///
/// Same boundary pattern as `jarvis-mcp-gw`: the HTTP contract carries
/// `approved_at` as an RFC 3339 *string*, while the canonical core type stores
/// a `DateTime<Utc>`. The serde tag and field names below are the unchanged
/// wire contract; only the receiving type is thin.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "decision")]
enum DecisionWire {
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

/// Boundary: incoming decision JSON -> canonical core [`Decision`].
///
/// The tagged shape is the current `jarvis-policy` contract: `approve`/`deny`
/// respond with the serialized canonical `ApprovalDecision` (see
/// `rust/jarvis-policy/src/http.rs`). The legacy two-variant shape
/// (`"Approved"` / `{"Denied":{"reason":".."}}`) was removed together with the
/// core legacy type (P1-7 slice 4) and is **explicitly rejected** here: an
/// unrecognized shape or a malformed RFC 3339 `approved_at` fails closed. No
/// author/timestamp is fabricated for a partially populated decision.
///
/// This boundary is on the real RUN path: [`PolicyClient::decide`] parses the
/// POST approve/deny response body through it, and `approvals.rs` projects the
/// returned core decision onto the UI-facing decision state.
pub fn decision_from_json(value: &serde_json::Value) -> Result<Decision> {
    if let Ok(wire) = serde_json::from_value::<DecisionWire>(value.clone()) {
        return match wire {
            DecisionWire::Approved {
                approved_by,
                approved_at,
            } => {
                let approved_at = chrono::DateTime::parse_from_rfc3339(&approved_at)
                    .with_context(|| format!("invalid approved_at: {approved_at}"))?
                    .with_timezone(&chrono::Utc);
                Ok(Decision::Approved {
                    approved_by,
                    approved_at,
                })
            }
            DecisionWire::Denied { reason } => Ok(Decision::Denied { reason }),
            DecisionWire::Pending => Ok(Decision::Pending),
            DecisionWire::Expired => Ok(Decision::Expired),
        };
    }
    // The legacy two-variant core type was removed with P1-7 slice 4; an
    // unrecognized tagged decision fails explicitly (fail-closed) rather than
    // being misread as a legacy shape or defaulted into an approval.
    Err(anyhow::anyhow!("unrecognized approval decision JSON"))
}

/// Boundary: UI wire request -> canonical core [`CoreApprovalRequest`].
///
/// The UI wire DTO intentionally keeps a `String` id and nullable timestamps so
/// legacy payloads still render. Conversion constructs the canonical core type
/// directly: missing timestamps become the epoch, while a missing/malformed
/// UUID id fails explicitly instead of silently producing a canonical request
/// with a fabricated identity.
///
/// Called at decision time by `ApprovalsView::decide`: the target id is
/// resolved through this boundary before it is used in the POST URL, so a
/// tolerant-wire id can never be mistaken for a canonical one.
pub fn request_to_core(wire: &ApprovalRequest) -> Result<CoreApprovalRequest> {
    let id = uuid::Uuid::parse_str(&wire.id).context("invalid approval id at UI boundary")?;
    let epoch = chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid");
    Ok(CoreApprovalRequest {
        id,
        principal: wire.principal.clone(),
        action: wire.action.clone(),
        resource: wire.resource.clone(),
        context: wire.context.clone(),
        requested_at: wire.requested_at.unwrap_or(epoch),
        expires_at: wire.expires_at.unwrap_or(epoch),
    })
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
            // Never follow redirects: ureq strips only Authorization/Cookie
            // headers on redirect, so X-Jarvis-Approval-Token would be replayed
            // to the redirect target (possibly a different host). With 0 the
            // 3xx response is returned as-is and the token stays on loopback.
            .max_redirects(0)
            .build();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token,
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// Requests currently awaiting an operator decision.
    ///
    /// This endpoint returns only request DTOs — no decision field — so the
    /// decision JSON boundary lives on the POST approve/deny path instead;
    /// see [`PolicyClient::decide`].
    pub fn pending(&self) -> Result<Vec<ApprovalRequest>> {
        let url = format!("{}/v1/approvals", self.base_url);
        let mut request = self.agent.get(&url);
        if let Some(token) = &self.token {
            request = request.header("X-Jarvis-Approval-Token", token);
        }
        let mut response = request
            .call()
            .with_context(|| format!("GET {url} failed"))?;
        // A 3xx is returned as-is because redirects are disabled; without this
        // check its (possibly empty) body could be mistaken for an empty queue.
        let status = response.status();
        if !status.is_success() {
            bail!("GET {url} returned HTTP {}", status.as_u16());
        }
        let body: PendingResponse = response
            .body_mut()
            .read_json()
            .context("invalid JSON from /v1/approvals")?;
        Ok(body.pending)
    }

    /// Approve the request `id` as `approved_by`.
    ///
    /// Returns the canonical decision parsed from the response body.
    pub fn approve(&self, id: &str, approved_by: &str) -> Result<Decision> {
        self.decide(id, "approve", &ApproveBody { approved_by })
    }

    /// Deny the request `id` with `reason`.
    ///
    /// Returns the canonical decision parsed from the response body.
    pub fn deny(&self, id: &str, reason: &str) -> Result<Decision> {
        self.decide(id, "deny", &DenyBody { reason })
    }

    fn decide<B: Serialize + ?Sized>(&self, id: &str, verb: &str, body: &B) -> Result<Decision> {
        let url = format!("{}/v1/approval/{id}/{verb}", self.base_url);
        let mut request = self.agent.post(&url);
        if let Some(token) = &self.token {
            request = request.header("X-Jarvis-Approval-Token", token);
        }
        let mut response = request
            .send_json(body)
            .with_context(|| format!("POST {url} failed"))?;
        // send_json() succeeds for any HTTP response; a 401/500/3xx must not
        // be reported to the operator as a successful approve/deny.
        let status = response.status();
        if !status.is_success() {
            bail!("POST {url} returned HTTP {}", status.as_u16());
        }
        // A 2xx body is the canonical tagged decision. Route it through the
        // same boundary as any other decision JSON before it reaches the UI:
        // a malformed body must not be reported as a successful approval.
        let value: serde_json::Value = response
            .body_mut()
            .read_json()
            .with_context(|| format!("invalid decision JSON from POST {url}"))?;
        decision_from_json(&value)
            .with_context(|| format!("unexpected decision JSON from POST {url}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::thread::JoinHandle;

    /// Bind a loopback server, accept `requests` connections and let `handle`
    /// decide what to answer. Used by every mock policy endpoint below so the
    /// socket setup is written only once.
    fn spawn_test_server(
        mut handle: impl FnMut(&mut TcpStream, String) + Send + 'static,
        requests: usize,
    ) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test address");
        let server = std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).expect("read");
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                handle(&mut stream, request);
            }
        });
        (addr, server)
    }

    fn respond(stream: &mut TcpStream, response: &str) {
        stream.write_all(response.as_bytes()).expect("write");
    }

    fn approval_token(request: &str) -> Option<String> {
        request
            .lines()
            .find(|line| {
                line.to_ascii_lowercase()
                    .starts_with("x-jarvis-approval-token:")
            })
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_string())
    }

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
        // Keep the connection open without answering until the client's
        // 150 ms deadline elapses (the delay here is 600 ms).
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                std::thread::sleep(Duration::from_millis(600));
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            },
            1,
        );

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
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                *seen_for_thread.lock().expect("lock") = approval_token(&request);
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n{\"pending\":[]}",
                );
            },
            1,
        );

        let client =
            PolicyClient::new_with_token(format!("http://{addr}"), Some("ui-token".to_string()));
        // The request fails to parse only if the body is malformed; an empty
        // pending list is valid, so this must succeed.
        let result = client.pending();
        handle.join().expect("server thread");
        assert!(result.is_ok(), "unexpected: {result:?}");
        assert_eq!(seen.lock().expect("lock").as_deref(), Some("ui-token"));
    }

    /// A 3xx from jarvis-policy must not be followed: ureq forwards custom
    /// headers on redirect, so following it would replay the approval token
    /// to the redirect target (potentially a different host).
    #[test]
    fn approval_token_is_not_replayed_on_redirect() {
        use std::sync::mpsc;
        use std::time::Instant;

        // Redirect target. If ureq follows the 3xx, this server sees the
        // replayed X-Jarvis-Approval-Token header.
        let target = TcpListener::bind("127.0.0.1:0").expect("bind target");
        let target_addr = target.local_addr().expect("target addr");
        let (seen_tx, seen_rx) = mpsc::channel();
        let target_thread = std::thread::spawn(move || {
            target
                .set_nonblocking(true)
                .expect("set nonblocking on target");
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                match target.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..n]).to_string();
                        let leaked = approval_token(&request).is_some();
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                        let _ = seen_tx.send(Some(leaked));
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
            let _ = seen_tx.send(None);
        });

        // Policy-side endpoint answering with a redirect to the other
        // listener (a different port, hence a different target).
        let (origin_addr, origin_thread) = spawn_test_server(
            move |stream, _request| {
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/leak\r\nContent-Length: 0\r\n\r\n"
                );
                let _ = stream.write_all(response.as_bytes());
            },
            1,
        );

        let client = PolicyClient::new_with_token(
            format!("http://{origin_addr}"),
            Some("redirect-token".to_string()),
        );
        // The raw 3xx must surface as an explicit error (not an empty queue),
        // and no follow-up request may reach the redirect target.
        let result = client.pending();
        assert!(result.is_err(), "3xx must be an error: {result:?}");
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("302"),
            "error must surface the HTTP status: {message}"
        );

        origin_thread.join().expect("origin thread");
        let target_seen = seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("target probe result");
        target_thread.join().expect("target thread");
        assert_eq!(
            target_seen, None,
            "redirect must not be followed, otherwise the token is replayed"
        );
    }

    /// A 500 with a *valid* body must still be an error: otherwise a broken
    /// policy service would show up as an empty approval queue.
    #[test]
    fn non_2xx_pending_status_is_an_error() {
        let (addr, server) = spawn_test_server(
            |stream, _request| {
                respond(
                    stream,
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"pending\":[]}",
                );
            },
            1,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let result = client.pending();
        server.join().expect("server thread");
        assert!(
            result.is_err(),
            "500 must not look like an empty queue: {result:?}"
        );
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("500"),
            "error must surface the HTTP status: {message}"
        );
    }

    /// A 2xx from POST /approve must be reported as success and must carry the
    /// configured approval token. The GET path is covered separately by
    /// `approval_token_is_sent_as_header`.
    #[test]
    fn approve_2xx_is_ok_and_sends_token() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                *seen_for_thread.lock().expect("lock") = Some(request);
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 82\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\",\"approved_by\":\"koppe\",\"approved_at\":\"2026-11-15T08:00:00Z\"}",
                );
            },
            1,
        );

        let client = PolicyClient::new_with_token(
            format!("http://{addr}"),
            Some("approve-token".to_string()),
        );
        let result = client.approve("req-1", "koppe");
        handle.join().expect("server thread");
        assert!(result.is_ok(), "2xx approve must succeed: {result:?}");
        assert!(matches!(
            result.expect("canonical decision returned"),
            Decision::Approved { .. }
        ));

        let request = seen.lock().expect("lock").clone().expect("request seen");
        assert!(
            request.starts_with("POST /v1/approval/req-1/approve "),
            "approve must POST to the approve endpoint: {request}"
        );
        assert_eq!(approval_token(&request).as_deref(), Some("approve-token"));
    }

    /// A 2xx from POST /deny must be reported as success and must carry the
    /// configured approval token, mirroring the approve success path.
    #[test]
    fn deny_2xx_is_ok_and_sends_token() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                *seen_for_thread.lock().expect("lock") = Some(request);
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"decision\":\"Denied\",\"reason\":\"because\"}",
                );
            },
            1,
        );

        let client =
            PolicyClient::new_with_token(format!("http://{addr}"), Some("deny-token".to_string()));
        let result = client.deny("req-2", "because");
        handle.join().expect("server thread");
        assert!(result.is_ok(), "2xx deny must succeed: {result:?}");
        assert!(matches!(
            result.expect("canonical decision returned"),
            Decision::Denied { .. }
        ));

        let request = seen.lock().expect("lock").clone().expect("request seen");
        assert!(
            request.starts_with("POST /v1/approval/req-2/deny "),
            "deny must POST to the deny endpoint: {request}"
        );
        assert_eq!(approval_token(&request).as_deref(), Some("deny-token"));
    }

    /// send_json() returns Ok for any HTTP response, so the status must be
    /// checked before reporting an approve/deny outcome to the operator.
    #[test]
    fn non_2xx_decide_status_is_an_error() {
        let mut responses = [
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ]
        .into_iter();
        let (addr, server) = spawn_test_server(
            move |stream, _request| {
                let response = responses.next().expect("queued response");
                respond(stream, response);
            },
            2,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let approved = client.approve("req-1", "koppe");
        let denied = client.deny("req-2", "because");
        server.join().expect("server thread");
        assert!(
            approved.is_err(),
            "401 must not look successful: {approved:?}"
        );
        assert!(denied.is_err(), "500 must not look successful: {denied:?}");

        let approved_message = format!("{:#}", approved.unwrap_err());
        let denied_message = format!("{:#}", denied.unwrap_err());
        assert!(
            approved_message.contains("401"),
            "approve error must surface the status: {approved_message}"
        );
        assert!(
            denied_message.contains("500"),
            "deny error must surface the status: {denied_message}"
        );
    }

    fn timestamp(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(secs, 0).expect("valid timestamp")
    }

    /// I30-D regression: the POST approve response body crosses the canonical
    /// core boundary on the real RUN path, not only in a direct unit test.
    #[test]
    fn approve_response_body_crosses_the_core_boundary() {
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 82\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\",\"approved_by\":\"koppe\",\"approved_at\":\"2026-11-15T08:00:00Z\"}",
                );
            },
            1,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        let decision = client.approve("4d1a1f2e-0000-0000-0000-000000000000", "koppe");
        handle.join().expect("server thread");
        match decision.expect("approve response parses as canonical decision") {
            Decision::Approved {
                approved_by,
                approved_at,
            } => {
                assert_eq!(approved_by, "koppe");
                assert_eq!(
                    approved_at,
                    chrono::DateTime::parse_from_rfc3339("2026-11-15T08:00:00Z")
                        .expect("timestamp parses")
                        .with_timezone(&chrono::Utc)
                );
            }
            other => panic!("expected canonical Approved, got {other:?}"),
        }
    }

    /// I30-D regression: the same boundary is applied to deny responses.
    #[test]
    fn deny_response_body_crosses_the_core_boundary() {
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"decision\":\"Denied\",\"reason\":\"because\"}",
                );
            },
            1,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        let decision = client.deny("4d1a1f2e-0000-0000-0000-000000000000", "because");
        handle.join().expect("server thread");
        match decision.expect("deny response parses as canonical decision") {
            Decision::Denied { reason } => assert_eq!(reason, "because"),
            other => panic!("expected canonical Denied, got {other:?}"),
        }
    }

    /// I30-D regression: a malformed decision on the real path fails closed
    /// instead of being reported as a successful approval.
    #[test]
    fn malformed_decision_response_fails_on_the_real_path() {
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 77\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\",\"approved_by\":\"koppe\",\"approved_at\":\"not-a-timestamp\"}",
                );
            },
            1,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        let decision = client.approve("4d1a1f2e-0000-0000-0000-000000000000", "koppe");
        handle.join().expect("server thread");
        assert!(
            decision.is_err(),
            "a malformed 2xx decision must not be reported as success: {decision:?}"
        );
    }

    /// TC-U5: tagged decision JSON converts to the canonical core type; the
    /// wire contract (tag + field names + RFC 3339 timestamp) stays unchanged.
    #[test]
    fn tagged_decision_json_converts_to_canonical_core() {
        let approved = serde_json::json!({
            "decision": "Approved",
            "approved_by": "koppe",
            "approved_at": "2026-11-15T08:00:00Z",
        });
        assert_eq!(
            decision_from_json(&approved).expect("approved converts"),
            Decision::Approved {
                approved_by: "koppe".into(),
                approved_at: chrono::DateTime::parse_from_rfc3339("2026-11-15T08:00:00Z")
                    .expect("timestamp parses")
                    .with_timezone(&chrono::Utc),
            }
        );

        let denied = serde_json::json!({ "decision": "Denied", "reason": "not now" });
        assert_eq!(
            decision_from_json(&denied).expect("denied converts"),
            Decision::Denied {
                reason: "not now".into()
            }
        );
        assert_eq!(
            decision_from_json(&serde_json::json!({ "decision": "Pending" }))
                .expect("pending converts"),
            Decision::Pending
        );
        assert_eq!(
            decision_from_json(&serde_json::json!({ "decision": "Expired" }))
                .expect("expired converts"),
            Decision::Expired
        );
    }

    /// TC-U6: legacy two-variant decisions are rejected explicitly after the
    /// core legacy type removal (P1-7 slice 4), and a malformed RFC 3339
    /// timestamp fails closed instead of being defaulted.
    #[test]
    fn legacy_decision_json_is_rejected_at_boundary() {
        let legacy_approved = serde_json::json!("Approved");
        assert!(decision_from_json(&legacy_approved).is_err());
        let legacy_denied = serde_json::json!({ "Denied": { "reason": "legacy no" } });
        assert!(decision_from_json(&legacy_denied).is_err());

        let malformed = serde_json::json!({
            "decision": "Approved",
            "approved_by": "koppe",
            "approved_at": "not-a-timestamp",
        });
        assert!(
            decision_from_json(&malformed).is_err(),
            "malformed timestamp must fail at the boundary"
        );
    }

    /// TC-U7: a full wire request projects onto the canonical core type with
    /// every field preserved, and the core JSON shape stays RFC 3339/UUID.
    #[test]
    fn wire_request_converts_to_canonical_core_keeping_the_json_shape() {
        let wire = ApprovalRequest {
            id: "3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88".into(),
            principal: "opencode".into(),
            action: "execute".into(),
            resource: "tool:shell.execute".into(),
            context: serde_json::json!({ "cmd": "ls" }),
            requested_at: Some(timestamp(0)),
            expires_at: Some(timestamp(60)),
        };
        let core = request_to_core(&wire).expect("converts");
        assert_eq!(core.principal, "opencode");
        assert_eq!(core.action, "execute");
        assert_eq!(core.resource, "tool:shell.execute");
        assert_eq!(core.context, serde_json::json!({ "cmd": "ls" }));
        assert_eq!(core.requested_at, timestamp(0));
        assert_eq!(core.expires_at, timestamp(60));

        let json = serde_json::to_value(&core).expect("canonical request serializes");
        assert_eq!(json["id"], "3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88");
        assert_eq!(json["requested_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["expires_at"], "1970-01-01T00:01:00Z");
    }

    /// TC-U8: legacy payloads that omit `requested_at`/`expires_at` still cross
    /// the boundary (nullable timestamps become the epoch), while a missing or
    /// malformed UUID id is rejected explicitly instead of being fabricated.
    #[test]
    fn legacy_wire_request_tolerance_and_explicit_id_error() {
        let legacy_json = serde_json::json!({
            "principal": "jarvis",
            "action": "execute",
            "resource": "tool:git.commit",
        });
        let wire: ApprovalRequest = serde_json::from_value(legacy_json).expect("legacy parses");
        assert_eq!(wire.requested_at, None);
        assert_eq!(wire.expires_at, None);
        assert!(
            request_to_core(&wire).is_err(),
            "empty id must fail explicitly"
        );

        let with_bad_id = ApprovalRequest {
            id: "not-a-uuid".into(),
            requested_at: None,
            expires_at: None,
            ..wire
        };
        assert!(
            request_to_core(&with_bad_id).is_err(),
            "malformed id must fail explicitly"
        );
    }
}
