//! OpenCode server client.
//!
//! Implements the `jarvis-event` -> OpenCode hop of the event-driven flow in
//! `adr/0001-unified-architecture.md`. The authorization order follows
//! `Normalizer::process_event` and the MCP gateway's `execute_tool`:
//!
//! ```text
//! HA / Android / webhooks -> jarvis-event -> jarvis-context
//!     -> policy (auto_respond, before dispatch) -> OpenCode
//!     -> MCP gateway (policy re-check per tool call, before execution)
//! ```
//!
//! Events are delivered to one long-lived session with
//! `POST /api/session/{sessionID}/prompt` and `delivery = "queue"` so a burst
//! of events cannot interrupt an agent turn that is already running. The
//! session is created lazily (`POST /api/session`) with the configured
//! location and recreated automatically when the server has forgotten it.
//! Tool execution stays in the MCP gateway, which re-checks every tool call
//! against the policy engine before running it.

use anyhow::{Context, Result, bail};
use jarvis_core::UnifiedEvent;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::info;

/// Summarize a failed upstream HTTP call without echoing its response body.
///
/// Upstream bodies can contain tokens, prompts or other secret-bearing text.
/// Following the `safe_error_summary()` convention in `jarvis-mcp-gw`, keep
/// only structural metadata (status, coarse kind, byte length) so operators can
/// triage failures without a disclosure channel into logs.
fn upstream_error_summary(status: reqwest::StatusCode, body: &str) -> String {
    let kind = if status.is_client_error() {
        "client_error"
    } else if status.is_server_error() {
        "server_error"
    } else {
        "unexpected_status"
    };
    let status_code = status.as_u16();
    let body_bytes = body.len();
    format!("upstream=opencode, status={status_code}, kind={kind}, bytes={body_bytes}")
}

pub struct OpenCodeClient {
    http: reqwest::Client,
    base_url: String,
    location: String,
    session: Mutex<Option<String>>,
}

impl OpenCodeClient {
    pub fn new(config: &crate::config::EventConfig) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(password) = &config.opencode_password {
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {password}"))
                .context("invalid OpenCode password header")?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .context("building OpenCode HTTP client")?;

        Ok(Self {
            http,
            base_url: config.opencode_url.trim_end_matches('/').to_string(),
            location: config.opencode_location.clone(),
            session: Mutex::new(None),
        })
    }

    /// Deliver a normalized event and its PII-filtered context to the agent.
    pub async fn dispatch(&self, event: &UnifiedEvent, context: &Value) -> Result<()> {
        let prompt = format_prompt(event, context);
        let session_id = self.session_id().await?;

        let response = self
            .http
            .post(format!("{}/api/session/{session_id}/prompt", self.base_url))
            .json(&json!({ "text": prompt, "delivery": "queue" }))
            .send()
            .await
            .context("sending event prompt to OpenCode")?;

        match response.status() {
            status if status.is_success() => {
                info!(session = %session_id, event_id = %event.id, "Event dispatched to OpenCode");
                Ok(())
            }
            reqwest::StatusCode::NOT_FOUND => {
                // The session disappeared (deleted, or the server restarted);
                // recreate it once and retry.
                tracing::warn!(
                    session = %session_id,
                    "OpenCode session vanished; recreating and retrying"
                );
                *self.session.lock().await = None;
                let session_id = self.session_id().await?;
                let prompt = format_prompt(event, context);
                self.send_prompt(&session_id, &prompt).await?;
                info!(session = %session_id, event_id = %event.id, "Event dispatched to OpenCode");
                Ok(())
            }
            status => {
                let body = response.text().await.unwrap_or_default();
                bail!(
                    "OpenCode rejected the event prompt ({})",
                    upstream_error_summary(status, &body)
                );
            }
        }
    }

    async fn send_prompt(&self, session_id: &str, prompt: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/api/session/{session_id}/prompt", self.base_url))
            .json(&json!({ "text": prompt, "delivery": "queue" }))
            .send()
            .await
            .context("retrying event prompt on OpenCode")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "OpenCode rejected the event prompt ({})",
                upstream_error_summary(status, &body)
            );
        }
        Ok(())
    }

    /// Return the cached session id, creating the session on first use.
    async fn session_id(&self) -> Result<String> {
        let mut guard = self.session.lock().await;
        if let Some(id) = guard.as_ref() {
            return Ok(id.clone());
        }

        let response = self
            .http
            .post(format!("{}/api/session", self.base_url))
            .json(&json!({
                "title": "JARVIS events",
                "location": { "directory": self.location },
            }))
            .send()
            .await
            .context("creating OpenCode session")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "OpenCode session creation failed ({})",
                upstream_error_summary(status, &body)
            );
        }

        let body: Value = response
            .json()
            .await
            .context("decoding OpenCode session response")?;
        let id = body["data"]["id"]
            .as_str()
            .context("OpenCode session response is missing data.id")?
            .to_string();
        info!(session = %id, "Created OpenCode event session");
        *guard = Some(id.clone());
        Ok(id)
    }
}

/// Render the prompt text handed to the agent.
///
/// The context object is produced by `jarvis-context`, which applies PII
/// filtering and token truncation, so the raw event payload is intentionally
/// not repeated here.
pub fn format_prompt(event: &UnifiedEvent, context: &Value) -> String {
    let context = serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string());
    format!(
        "[JARVIS event]\n\
         source: {:?}\n\
         type: {}\n\
         timestamp: {}\n\
         correlation_id: {}\n\
         event_id: {}\n\
         \n\
         Context (PII-filtered by jarvis-context):\n\
         {}",
        event.source,
        event.event_type,
        event.timestamp.to_rfc3339(),
        event.correlation_id,
        event.id,
        context,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_core::EventSource;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    fn sample_event() -> UnifiedEvent {
        UnifiedEvent {
            id: uuid::Uuid::nil(),
            source: EventSource::HomeAssistant,
            event_type: "state_changed".into(),
            payload: json!({"entity_id": "light.kitchen"}),
            timestamp: chrono::DateTime::parse_from_rfc3339("2026-10-15T12:00:00+00:00")
                .unwrap()
                .with_timezone(&chrono::Utc),
            correlation_id: "01JTESTCONTEXT".into(),
        }
    }

    #[test]
    fn prompt_carries_event_metadata_and_context() {
        let context = json!({"memories": [{"text": "kitchen light was on"}]});
        let prompt = format_prompt(&sample_event(), &context);

        assert!(prompt.contains("[JARVIS event]"));
        assert!(prompt.contains("source: HomeAssistant"));
        assert!(prompt.contains("type: state_changed"));
        assert!(prompt.contains("timestamp: 2026-10-15T12:00:00+00:00"));
        assert!(prompt.contains("correlation_id: 01JTESTCONTEXT"));
        assert!(prompt.contains("00000000-0000-0000-0000-000000000000"));
        assert!(prompt.contains("kitchen light was on"));
        // The raw payload must not bypass the PII-filtered context.
        assert!(!prompt.contains("light.kitchen"));
    }

    #[test]
    fn prompt_renders_non_object_context() {
        let prompt = format_prompt(&sample_event(), &json!("plain"));
        assert!(prompt.contains("\"plain\""));
    }

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

    #[test]
    fn upstream_error_summary_never_leaks_body_into_logs() {
        const TOKEN: &str = "sk-opencode-secret-9c3f";
        const BODY_TEXT: &str = "private meeting notes about the user";
        let body = format!("{{\"error\":\"{TOKEN}\",\"detail\":\"{BODY_TEXT}\"}}");

        let summary = upstream_error_summary(reqwest::StatusCode::UNAUTHORIZED, &body);
        assert!(summary.contains("upstream=opencode"), "{summary}");
        assert!(summary.contains("status=401"), "{summary}");
        assert!(summary.contains("kind=client_error"), "{summary}");
        let has_length = summary.contains(&format!("bytes={}", body.len()));
        assert!(has_length, "{summary}");
        assert!(!summary.contains(TOKEN), "{summary}");
        assert!(!summary.contains(BODY_TEXT), "{summary}");

        let server = upstream_error_summary(reqwest::StatusCode::BAD_GATEWAY, &body);
        assert!(server.contains("status=502"), "{server}");
        assert!(server.contains("kind=server_error"), "{server}");
        assert!(!server.contains(TOKEN), "{server}");

        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(
                "Event processing failed: {}",
                format!("OpenCode rejected the event prompt ({summary})")
            );
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(
            !logs.contains(TOKEN),
            "upstream body leaked into logs: {logs}"
        );
        assert!(!logs.contains(BODY_TEXT), "upstream body leaked: {logs}");
        assert!(
            logs.contains("status=401"),
            "summary missing from logs: {logs}"
        );
    }
}
