//! JARVIS Core - Shared types and traits

// Canonical approval domain types live in `approval` (P1-7, 案A); that module is
// the single source of truth shared by policy, transports and the UI.
pub mod approval;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Canonical HTTP header / gRPC metadata key carrying the context admin token.
///
/// The context service's gRPC ingress, its HTTP gate and the event-side gRPC
/// provider must all read this one definition. Duplicating the literal in a
/// transport lets a client send a key the server never reads, which turns
/// every `BuildContext` in `context_mode = grpc` into a 401/UNAUTHENTICATED.
/// The wire name itself is stable and must not change.
pub const JARVIS_CONTEXT_TOKEN_HEADER: &str = "x-jarvis-context-token";

/// Fixed HTTP 409 body emitted by `jarvis-policy` when the emergency stop
/// (kill switch) blocks an approval decision.
///
/// The policy handler and the approval UI both classify a 409 by this exact
/// body: the `ALREADY_DECIDED` literal must still resolve to the authoritative
/// winner, while this literal must pin the UI's halt display with no winner
/// readback. Keeping one definition in `jarvis-core` prevents a policy wording
/// change from silently pushing a kill-switch 409 into the winner path in the
/// UI. The wire text is stable: changing it requires updating the contract pins
/// in both consumers.
pub const KILL_SWITCH_409_BODY: &str = "approval decisions are blocked by the kill switch";

/// Fixed HTTP 409 body emitted by `jarvis-policy` when a competing operator has
/// already decided an approval request.
///
/// This is the *only* 409 body the approval UI treats as a benign winner: a
/// trimmed exact match authorizes the status-endpoint readback. The kill-switch
/// literal and every other body (empty, unreadable, or rewritten by an older
/// peer or a relay) pin the halt display instead (I58-B). Keeping one definition
/// here lets a one-sided wording change be caught by the contract pins in both
/// consumers instead of silently turning a real emergency stop into a false
/// "already decided" display. The wire text is stable: changing it requires
/// updating the contract pins in both consumers.
pub const ALREADY_DECIDED_409_BODY: &str = "approval request already decided";

/// Fixed classification token emitted when a JSON body fails to deserialize.
///
/// The offending bytes and axum's `JsonRejection` detail are never formatted:
/// only these static tokens reach tracing (and therefore OTLP/Grafana).
#[cfg(feature = "http")]
pub const INVALID_JSON_BODY_ERROR_KIND: &str = "invalid_input";
/// Static field label used by [`BareJson`]'s rejection log.
#[cfg(feature = "http")]
pub const INVALID_JSON_BODY_FIELD: &str = "body";

/// Map a JSON rejection onto the fixed bare-status contract.
///
/// The match is on the rejection *variant* only: no rejection text, body byte
/// or serde error is read, so nothing attacker-controlled can reach the
/// response or a log record. axum's own `status()` is deliberately not used:
/// it reports 422 for `JsonDataError`, while every service here keeps the
/// historical bare 400 for wrong types and syntax errors.
#[cfg(feature = "http")]
fn bare_json_rejection_status(
    rejection: &axum::extract::rejection::JsonRejection,
) -> axum::http::StatusCode {
    use axum::extract::rejection::JsonRejection;
    match rejection {
        // The client can fix this by adding `Content-Type: application/json`.
        JsonRejection::MissingJsonContentType(_) => axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
        // `BytesRejection` is the default 2 MiB body-limit rejection: the client
        // can fix this by shrinking the payload. Keep it visible to metrics.
        JsonRejection::BytesRejection(_) => axum::http::StatusCode::PAYLOAD_TOO_LARGE,
        // Type mismatches (`JsonDataError`) and syntax errors
        // (`JsonSyntaxError`) stay on the established bare-400 contract. The
        // wildcard also covers any future `#[non_exhaustive]` variant.
        _ => axum::http::StatusCode::BAD_REQUEST,
    }
}

/// JSON body extractor that maps `JsonRejection` onto a bare status without
/// ever exposing axum's rejection detail.
///
/// axum's stock `Json<T>` rejects type-mismatched or malformed bodies (for
/// example `limit: "abc"`, an embedding encoded as a string, or a syntax error)
/// *before* the handler runs and returns a detailed `JsonRejection` body. That
/// body bypasses each service's no-leak contract and makes HTTP asymmetric with
/// gRPC's fixed `InvalidArgument`. Wrapping `Json<T>` here discards the rejection
/// detail and selects an empty status from the rejection kind: 415 when the
/// `Content-Type` header is missing, 413 when the body exceeds axum's default
/// 2 MiB limit, and 400 for every other failure (wrong type or bad syntax).
///
/// Every rejection emits one fixed-vocabulary log record — the same
/// `error_kind = "invalid_input"` / `field = "body"` / "invalid client input"
/// record the context service already emits — but at DEBUG, not WARN: a flood
/// of malformed requests must not amplify journald/OTLP volume. The offending
/// bytes and axum's rejection text are never formatted. Valid bodies reach the
/// handler unchanged; all semantic validation stays in the owning service.
///
/// The tuple field is public so consumers can keep the existing
/// `let BareJson(Json(body)) = req;` destructuring pattern.
#[cfg(feature = "http")]
#[derive(Debug)]
pub struct BareJson<T>(pub axum::Json<T>);

#[cfg(feature = "http")]
#[axum::async_trait]
impl<S, T> axum::extract::FromRequest<S> for BareJson<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = axum::http::StatusCode;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        <axum::Json<T> as axum::extract::FromRequest<S>>::from_request(req, state)
            .await
            .map(Self)
            .map_err(|rejection| {
                // Fixed vocabulary only: never format `rejection`, the request,
                // or any body byte into this record. DEBUG (not WARN) keeps a
                // rejection flood from amplifying journald/OTLP volume.
                tracing::debug!(
                    error_kind = INVALID_JSON_BODY_ERROR_KIND,
                    field = INVALID_JSON_BODY_FIELD,
                    "invalid client input"
                );
                bare_json_rejection_status(&rejection)
            })
    }
}

/// Unified event type used across JARVIS services
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedEvent {
    pub id: Uuid,
    pub source: EventSource,
    pub event_type: String,
    pub payload: Value,
    pub timestamp: DateTime<Utc>,
    pub correlation_id: String,
}

/// Event source enumeration
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    HomeAssistant,
    Android,
    Gmail,
    Calendar,
    Web,
    Finance,
    Manual,
}

/// Memory entry for context storage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Uuid,
    pub content: String,
    pub embedding: Vec<f32>,
    pub metadata: Value,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Similarity score captured from vector search (P1-1).
    ///
    /// `None` for entries that never went through Qdrant ranking (for example
    /// the PostgreSQL-only recent-memory path). `#[serde(default)]` keeps
    /// deserialization compatible with payloads written before this additive
    /// field existed; serde ignores the new key for older readers.
    #[serde(default)]
    pub score: Option<f32>,
}

/// Policy check request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheckRequest {
    pub principal: String,
    pub action: String,
    pub resource: String,
    pub context: Value,
}

/// Policy check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheckResponse {
    pub allowed: bool,
    /// Fixed, non-sensitive decision reason (e.g. `allowed`, `kill_switch`,
    /// `rbac_denied`, `cedar_denied`, `evaluation_error`).
    ///
    /// `#[serde(default)]` keeps deserialization backward compatible with
    /// payloads written before this field existed, and serde ignores unknown
    /// keys by default so older readers can still consume new payloads.
    #[serde(default)]
    pub reason: String,
}

/// Kill switch status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchStatus {
    pub activated: bool,
    pub activated_at: Option<i64>,
    pub reason: Option<String>,
    pub activated_by: Option<String>,
}

/// MCP tool execution request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteToolRequest {
    pub tool: String,
    pub args: Value,
    pub principal: String,
}

/// MCP tool execution response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteToolResponse {
    pub result: Value,
}

impl std::fmt::Display for EventSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventSource::HomeAssistant => write!(f, "home_assistant"),
            EventSource::Android => write!(f, "android"),
            EventSource::Gmail => write!(f, "gmail"),
            EventSource::Calendar => write!(f, "calendar"),
            EventSource::Web => write!(f, "web"),
            EventSource::Finance => write!(f, "finance"),
            EventSource::Manual => write!(f, "manual"),
        }
    }
}

impl std::str::FromStr for EventSource {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "home_assistant" => Ok(EventSource::HomeAssistant),
            "android" => Ok(EventSource::Android),
            "gmail" => Ok(EventSource::Gmail),
            "calendar" => Ok(EventSource::Calendar),
            "web" => Ok(EventSource::Web),
            "finance" => Ok(EventSource::Finance),
            "manual" => Ok(EventSource::Manual),
            _ => Err(anyhow::anyhow!("Unknown event source: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single canonical pin of the wire name. Transport crates assert against
    /// [`JARVIS_CONTEXT_TOKEN_HEADER`] instead of repeating this literal.
    #[test]
    fn context_token_header_pins_the_wire_contract() {
        assert_eq!(JARVIS_CONTEXT_TOKEN_HEADER, "x-jarvis-context-token");
    }

    /// The shared JSON rejection vocabulary must stay byte-identical to the
    /// tokens the context service's `validation::log_invalid_input("body")`
    /// emits, so switching that service onto the shared extractor is a no-op
    /// for its log-based regression tests.
    #[cfg(feature = "http")]
    #[test]
    fn bare_json_rejection_vocabulary_pins_the_wire_contract() {
        assert_eq!(INVALID_JSON_BODY_ERROR_KIND, "invalid_input");
        assert_eq!(INVALID_JSON_BODY_FIELD, "body");
    }

    /// Poll an extractor future without pulling an async runtime into
    /// jarvis-core's dev-dependencies. Every rejection path below resolves
    /// from an in-memory body in a single poll; `Waker::noop` is safe here
    /// because no timer or I/O driver is involved.
    #[cfg(feature = "http")]
    fn poll_once<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => output,
            std::task::Poll::Pending => panic!("BareJson rejection did not resolve immediately"),
        }
    }

    /// Drive the real `BareJson` extractor and return its rejection status.
    #[cfg(feature = "http")]
    fn bare_json_rejection<T>(
        body: axum::body::Body,
        content_type: Option<&str>,
    ) -> axum::http::StatusCode
    where
        T: serde::de::DeserializeOwned,
    {
        let mut builder = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/check");
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let request = builder.body(body).expect("request builds");
        let future = <BareJson<T> as axum::extract::FromRequest<()>>::from_request(request, &());
        poll_once(future)
            .err()
            .expect("the extractor rejects this body")
    }

    /// I61-A: a missing `Content-Type` is a bare 415 so the client knows to
    /// add the header, instead of an indistinguishable empty 400.
    #[cfg(feature = "http")]
    #[test]
    fn bare_json_missing_content_type_is_415() {
        let status = bare_json_rejection::<serde_json::Value>(
            axum::body::Body::from(r#"{"allowed":true}"#),
            None,
        );
        assert_eq!(status, axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    /// I61-A: axum's 2 MiB `BytesRejection` stays visible as a bare 413 so the
    /// size error is not erased from metrics. The rejection is a `StatusCode`,
    /// so no rejection text or body can cross the boundary.
    #[cfg(feature = "http")]
    #[test]
    fn bare_json_oversized_body_is_413() {
        let oversized = vec![b'x'; 2_097_152 + 1];
        let status = bare_json_rejection::<serde_json::Value>(
            axum::body::Body::from(oversized),
            Some("application/json"),
        );
        assert_eq!(status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// Wrong types and bad syntax keep the established bare-400 contract;
    /// axum's 422 for `JsonDataError` is deliberately not adopted.
    #[cfg(feature = "http")]
    #[test]
    fn bare_json_type_and_syntax_errors_stay_400() {
        for body in [r#"{"allowed":42}"#, r#"{"allowed":true,"#] {
            let status = bare_json_rejection::<PolicyCheckResponse>(
                axum::body::Body::from(body),
                Some("application/json"),
            );
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
        }
    }

    /// Single canonical pin of the approval 409 wire body. Policy and UI
    /// reference [`KILL_SWITCH_409_BODY`] instead of repeating this literal;
    /// the UI also keeps an independent external-contract fixture so a change
    /// here is observable as a failing UI contract test.
    #[test]
    fn kill_switch_409_body_pins_the_wire_contract() {
        assert_eq!(
            KILL_SWITCH_409_BODY,
            "approval decisions are blocked by the kill switch"
        );
    }

    /// Single canonical pin of the already-decided 409 wire body. Policy emits
    /// [`ALREADY_DECIDED_409_BODY`] and the UI accepts only that literal before
    /// reading the winner; the UI also keeps an independent external fixture so
    /// a change here is observable as a failing UI contract test (I58-B).
    #[test]
    fn already_decided_409_body_pins_the_wire_contract() {
        assert_eq!(ALREADY_DECIDED_409_BODY, "approval request already decided");
        assert_ne!(ALREADY_DECIDED_409_BODY, KILL_SWITCH_409_BODY);
    }

    fn all_sources() -> [EventSource; 7] {
        [
            EventSource::HomeAssistant,
            EventSource::Android,
            EventSource::Gmail,
            EventSource::Calendar,
            EventSource::Web,
            EventSource::Finance,
            EventSource::Manual,
        ]
    }

    #[test]
    fn event_source_round_trips_through_display_and_from_str() {
        for source in all_sources() {
            let rendered = source.to_string();
            let parsed = rendered
                .parse::<EventSource>()
                .unwrap_or_else(|error| panic!("{rendered} does not parse: {error}"));
            assert_eq!(parsed, source);
        }
    }

    #[test]
    fn from_str_is_case_insensitive() {
        assert_eq!(
            "HOME_ASSISTANT".parse::<EventSource>().expect("uppercase"),
            EventSource::HomeAssistant
        );
    }

    #[test]
    fn unknown_event_source_is_rejected() {
        assert!("mqtt".parse::<EventSource>().is_err());
    }

    #[test]
    fn unified_event_serializes_the_source_as_snake_case() {
        let event = UnifiedEvent {
            id: Uuid::nil(),
            source: EventSource::HomeAssistant,
            event_type: "state_changed".into(),
            payload: serde_json::json!({ "entity_id": "light.kitchen" }),
            timestamp: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
            correlation_id: "corr-1".into(),
        };

        let json = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(json["source"], "home_assistant");
        assert_eq!(json["event_type"], "state_changed");
    }

    /// The additive `reason` field must not break payloads from older peers
    /// that only know `allowed`.
    #[test]
    fn policy_check_response_accepts_legacy_payload_without_reason() {
        let legacy: PolicyCheckResponse =
            serde_json::from_str(r#"{"allowed":false}"#).expect("legacy payload parses");
        assert!(!legacy.allowed);
        assert_eq!(legacy.reason, "");
    }

    /// A new payload carries the fixed reason and stays readable by callers
    /// that ignore unknown keys (serde's default behavior).
    #[test]
    fn policy_check_response_round_trips_reason() {
        let response = PolicyCheckResponse {
            allowed: false,
            reason: "kill_switch".into(),
        };
        let encoded = serde_json::to_string(&response).expect("serializes");
        assert_eq!(encoded, r#"{"allowed":false,"reason":"kill_switch"}"#);
        let decoded: PolicyCheckResponse = serde_json::from_str(&encoded).expect("parses");
        assert!(!decoded.allowed);
        assert_eq!(decoded.reason, "kill_switch");
    }

    #[test]
    fn memory_entry_round_trips() {
        let memory = MemoryEntry {
            id: Uuid::nil(),
            content: "remember this".into(),
            embedding: vec![0.25, 0.5],
            metadata: serde_json::json!({ "kind": "note" }),
            source: "test".into(),
            created_at: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
            updated_at: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
            expires_at: None,
            score: Some(0.875),
        };

        let encoded = serde_json::to_string(&memory).expect("memory serializes");
        let decoded: MemoryEntry = serde_json::from_str(&encoded).expect("memory parses");
        assert_eq!(decoded.content, memory.content);
        assert_eq!(decoded.embedding, memory.embedding);
        assert_eq!(decoded.metadata, memory.metadata);
        assert_eq!(decoded.score, Some(0.875));
        assert_eq!(
            serde_json::to_value(&memory).expect("memory serializes to JSON")["score"],
            serde_json::json!(0.875),
            "the additive score must be exposed in serialized payloads"
        );
    }

    /// Legacy payloads written before the additive `score` field existed must
    /// still deserialize; the missing key becomes `None`, not an error.
    #[test]
    fn memory_entry_accepts_legacy_json_without_score() {
        let legacy = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "content":"legacy entry",
            "embedding":[],
            "metadata":{},
            "source":"legacy",
            "created_at":"1970-01-01T00:00:00Z",
            "updated_at":"1970-01-01T00:00:00Z",
            "expires_at":null
        }"#;

        let entry: MemoryEntry = serde_json::from_str(legacy).expect("legacy payload parses");
        assert_eq!(entry.content, "legacy entry");
        assert_eq!(entry.score, None, "a missing score must default to None");
    }
}
