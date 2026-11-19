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

use anyhow::{Context as _, Result, bail};
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
        let response = request
            .send_json(body)
            .with_context(|| format!("POST {url} failed"))?;
        // send_json() succeeds for any HTTP response; a 401/500/3xx must not
        // be reported to the operator as a successful approve/deny.
        let status = response.status();
        if !status.is_success() {
            bail!("POST {url} returned HTTP {}", status.as_u16());
        }
        Ok(())
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
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 23\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\"}",
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
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 21\r\nConnection: close\r\n\r\n{\"decision\":\"Denied\"}",
                );
            },
            1,
        );

        let client =
            PolicyClient::new_with_token(format!("http://{addr}"), Some("deny-token".to_string()));
        let result = client.deny("req-2", "because");
        handle.join().expect("server thread");
        assert!(result.is_ok(), "2xx deny must succeed: {result:?}");

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
}
