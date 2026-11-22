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

/// Fixed caller-visible text for a gateway-side failure, shared by all three
/// MCP gateway transports (HTTP `/v1/execute`, gRPC and the MCP `/mcp` surface).
///
/// Internal causes (absolute sandbox paths, tokens, egress hosts, upstream
/// text, anyhow chains) are written to the server-side `tracing` log only.
/// Every transport aliases this constant instead of keeping a private literal:
/// a one-sided rewording would otherwise make two adapters disagree about the
/// same contract and could accidentally reintroduce detail into a response.
/// The wire text is stable: changing it requires updating the caller-visible
/// contract pins in each transport crate.
pub const INTERNAL_GATEWAY_ERROR_MESSAGE: &str = "internal gateway error";

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

/// Fixed HTTP 410 body emitted by `jarvis-policy` when an approval request's
/// decision window has elapsed.
///
/// Expiry is a terminal outcome with no winning operator, so the approval UI may
/// surface the canonical `Expired` decision from a 410 *only* after this exact
/// body is confirmed (trimmed exact match). An unverified body — rewritten by a
/// relay, changed by a one-sided policy wording edit, empty, or unreadable —
/// must fail closed instead of fabricating a terminal decision or falling into
/// the 409 winner readback. The policy transport and the approval UI both alias
/// this constant, and the contract pins in all three crates catch a one-sided
/// change rather than silently turning a rewritten 410 into "期限切れ". The wire
/// text is stable: changing it requires updating the contract pins in both
/// consumers.
pub const APPROVAL_EXPIRED_BODY: &str = "approval request expired";

/// Closed field vocabulary of the policy service's `GET /health` wire contract
/// (I62-C). A healthy service answers `200 {"status":"ok"}` (no `reason` key);
/// a degraded service answers `503 {"status":"degraded","reason":<reason>}`.
///
/// Since I64-B the complete raw bodies below ([`HEALTH_OK_BODY`],
/// [`HEALTH_KILL_SWITCH_BODY`], [`HEALTH_NO_POLICY_BODY`]) are the authoritative
/// wire contract: the UI matches a whole body exactly rather than interpreting
/// individual fields. These tokens remain the fixed status/reason vocabulary
/// and are pinned separately. The wire text is stable: changing it requires
/// updating the pins in both consumers.
pub const HEALTH_STATUS_OK: &str = "ok";
/// Degraded `status` value of the `503` health response. See
/// [`HEALTH_STATUS_OK`] for the shared-contract rationale.
pub const HEALTH_STATUS_DEGRADED: &str = "degraded";
/// `503` health `reason` for an active emergency stop.
pub const HEALTH_REASON_KILL_SWITCH: &str = "kill_switch";
/// `503` health `reason` for a missing policy source.
pub const HEALTH_REASON_NO_POLICY: &str = "no_policy";

/// Fixed raw body of the healthy `200` `/health` response (I64-B).
///
/// This is the complete wire body, not just a field vocabulary. The approval UI
/// classifies a probe by a *trimmed exact match* against the three body literals
/// below: a body with an extra key, a duplicate key, or any unknown shape is an
/// incomplete probe and fails closed (`Err` -> `kill_switch_active`'s
/// `unwrap_or(true)`). Keeping the raw bodies here means a relay cannot turn a
/// degraded answer into a false release by exploiting serde_json's last-wins
/// handling of duplicate keys. The emit side (`jarvis-policy`) and the UI
/// classifier both alias these constants, and independent fixtures in both
/// consumers catch a one-sided change the same way the 409/410 pins do.
pub const HEALTH_OK_BODY: &str = r#"{"status":"ok"}"#;
/// Fixed raw body of the `503` `/health` response for an active kill switch
/// (I64-B). See [`HEALTH_OK_BODY`] for the shared-contract rationale.
pub const HEALTH_KILL_SWITCH_BODY: &str = r#"{"status":"degraded","reason":"kill_switch"}"#;
/// Fixed raw body of the `503` `/health` response for a missing policy source
/// (I64-B). See [`HEALTH_OK_BODY`] for the shared-contract rationale.
pub const HEALTH_NO_POLICY_BODY: &str = r#"{"status":"degraded","reason":"no_policy"}"#;

/// Fixed, closed label vocabulary of the `jarvis_http_requests_total{status=...}`
/// counter emitted by all four JARVIS HTTP services.
///
/// Policy, event, mcp-gw and context all classify every response through this
/// one function, so the same status can no longer produce a different metric
/// label in different services. The set is deliberately bounded: only the
/// statuses the services actually emit get a dedicated literal, and everything
/// else collapses to `other` so an unbounded status cannot inflate the metric.
///
/// I93-D: `503` is intentionally a first-class bucket. It is a genuine
/// availability signal (kill switch, approval queue exhaustion, full event
/// channel, persistence outage) and must stay visible; folding it into `other`
/// hides exactly the incidents an operator needs to see. The canonical mapping
/// here is the one the policy and event services already carried, so the
/// vocabulary itself is unchanged — this only removes the four-way duplication
/// and lifts mcp-gw/context onto the same contract.
///
/// I97-D: `410` is likewise a first-class bucket. `jarvis-policy` emits 410 for
/// an expired approval decision window ([`APPROVAL_EXPIRED_BODY`]); it is a
/// terminal outcome distinct from the 409 winner/halt branches. Without a
/// dedicated label every expiry collapsed into `other`, making expired
/// approvals unobservable in `jarvis_http_requests_total`.
#[cfg(feature = "http")]
pub fn status_label(status: axum::http::StatusCode) -> &'static str {
    match status.as_u16() {
        200 => "200",
        201 => "201",
        204 => "204",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        409 => "409",
        410 => "410",
        413 => "413",
        415 => "415",
        500 => "500",
        503 => "503",
        _ => "other",
    }
}

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
///
/// I64-C: `BytesRejection` is not a single cause. It wraps
/// `FailedToBufferBody`, whose `LengthLimitError` is the real 2 MiB overflow
/// (axum → 413) and whose `UnknownBodyError` is a body read failure such as a
/// client disconnect or a stream error (axum → 400). Only the former is fixed
/// by shrinking the payload; mapping both to 413 mislabels transport failures
/// as client input errors and pollutes 413 metrics. Adopted policy here:
/// length-limit → 413, every other body-buffer failure → 400 (axum parity).
/// Detecting/counting oversized bodies at a dedicated body-limit middleware is
/// left to the owning service; this extractor only classifies what axum reports.
#[cfg(feature = "http")]
fn bare_json_rejection_status(
    rejection: &axum::extract::rejection::JsonRejection,
) -> axum::http::StatusCode {
    use axum::extract::rejection::{BytesRejection, FailedToBufferBody, JsonRejection};
    match rejection {
        // The client can fix this by adding `Content-Type: application/json`.
        JsonRejection::MissingJsonContentType(_) => axum::http::StatusCode::UNSUPPORTED_MEDIA_TYPE,
        // Only an actual length-limit overflow is client-fixable by shrinking
        // the payload. Discriminating via `FailedToBufferBody` keeps axum's own
        // 400 for `UnknownBodyError` (disconnect / stream read failure).
        JsonRejection::BytesRejection(bytes_rejection) => {
            let is_length_limit = matches!(
                bytes_rejection,
                BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_))
            );
            bare_json_body_buffer_status(is_length_limit)
        }
        // Type mismatches (`JsonDataError`) and syntax errors
        // (`JsonSyntaxError`) stay on the established bare-400 contract. The
        // wildcard also covers any future `#[non_exhaustive]` variant, including
        // new `BytesRejection` / `FailedToBufferBody` variants.
        _ => axum::http::StatusCode::BAD_REQUEST,
    }
}

/// Status for a failed body-buffer rejection, split out so the non-length-limit
/// branch (axum's `UnknownBodyError`: client disconnect / stream read error)
/// has a direct regression without adding a streaming-body dev-dependency to
/// jarvis-core. See [`bare_json_rejection_status`] for the adopted policy.
#[cfg(feature = "http")]
fn bare_json_body_buffer_status(is_length_limit: bool) -> axum::http::StatusCode {
    if is_length_limit {
        axum::http::StatusCode::PAYLOAD_TOO_LARGE
    } else {
        axum::http::StatusCode::BAD_REQUEST
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
/// `Content-Type` header is missing, 413 only when axum reports a length-limit
/// overflow (body exceeds its default 2 MiB limit), and 400 for every other
/// failure — including `UnknownBodyError` from a client disconnect or stream
/// read error, plus wrong type or bad syntax (I64-C).
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

impl EventSource {
    /// Case-insensitive source parsing with a lossy fallback.
    ///
    /// Reuses the exact mapping of [`std::str::FromStr`] but returns
    /// [`EventSource::Manual`] for unknown values instead of an error. Both
    /// HTTP and gRPC ingress use this so the same wire value normalizes
    /// identically across transports; strict parsing via `parse()` is unchanged.
    pub fn from_str_lossy(raw: &str) -> Self {
        raw.parse::<Self>().unwrap_or(EventSource::Manual)
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

    /// Single canonical pin of the gateway's fixed internal-failure wording.
    /// HTTP, gRPC and the MCP surface alias this constant, so this pin is the
    /// one place a wording change becomes observable before any transport
    /// contract test runs.
    #[test]
    fn internal_gateway_error_message_pins_the_wire_contract() {
        assert_eq!(INTERNAL_GATEWAY_ERROR_MESSAGE, "internal gateway error");
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

    /// I61-A / I64-C: axum's 2 MiB `LengthLimitError` stays visible as a bare
    /// 413 so the size error is not erased from metrics. A body read failure is
    /// not a size error and now takes the 400 branch instead. The rejection is
    /// a `StatusCode`, so no rejection text or body can cross the boundary.
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

    /// I64-C: a failed body buffer that is NOT a length-limit overflow (axum's
    /// `UnknownBodyError`, e.g. a client disconnect or stream read error) must
    /// be a bare 400, not the misleading "shrink your body" 413. The mapping
    /// seam is asserted directly because producing an erroring stream body in
    /// this crate would require a `futures-util` dev-dependency, which is out
    /// of scope for this lib.rs-only change. The second assertion guards the
    /// other half of the seam against an accidental flip.
    #[cfg(feature = "http")]
    #[test]
    fn bare_json_unknown_body_error_is_400() {
        assert_eq!(
            bare_json_body_buffer_status(false),
            axum::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            bare_json_body_buffer_status(true),
            axum::http::StatusCode::PAYLOAD_TOO_LARGE
        );
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

    /// Single canonical pin of the expired 410 wire body. Policy emits
    /// [`APPROVAL_EXPIRED_BODY`] and the UI confirms a trimmed exact match before
    /// surfacing the terminal `Expired` decision; each consumer keeps an
    /// independent external fixture so a one-sided wording change is caught.
    #[test]
    fn approval_expired_body_pins_the_wire_contract() {
        assert_eq!(APPROVAL_EXPIRED_BODY, "approval request expired");
        assert_ne!(APPROVAL_EXPIRED_BODY, ALREADY_DECIDED_409_BODY);
        assert_ne!(APPROVAL_EXPIRED_BODY, KILL_SWITCH_409_BODY);
    }

    /// Single canonical pin of the `/health` status vocabulary. Policy emits
    /// and the UI classifies these exact literals via these constants; both
    /// consumers keep an independent contract fixture so a change here is
    /// observable as a failing test on either side (I62-C).
    #[test]
    fn health_status_vocabulary_pins_the_wire_contract() {
        assert_eq!(HEALTH_STATUS_OK, "ok");
        assert_eq!(HEALTH_STATUS_DEGRADED, "degraded");
        assert_ne!(HEALTH_STATUS_OK, HEALTH_STATUS_DEGRADED);
    }

    /// Single canonical pin of the `/health` degraded reason vocabulary. The
    /// set is closed: the UI must classify any other reason as an incomplete
    /// probe and fail closed (I62-C).
    #[test]
    fn health_reason_vocabulary_pins_the_wire_contract() {
        assert_eq!(HEALTH_REASON_KILL_SWITCH, "kill_switch");
        assert_eq!(HEALTH_REASON_NO_POLICY, "no_policy");
        assert_ne!(HEALTH_REASON_KILL_SWITCH, HEALTH_REASON_NO_POLICY);
    }

    /// Single canonical pins of the three complete `/health` wire bodies
    /// (I64-B). Policy emits these literals and the UI accepts only a trimmed
    /// exact match; both consumers keep independent fixtures, so a one-sided
    /// wording change fails a pin here or on either side instead of silently
    /// turning a rewritten 503 into a false "switch lifted" display.
    #[test]
    fn health_wire_bodies_pin_the_wire_contract() {
        assert_eq!(HEALTH_OK_BODY, r#"{"status":"ok"}"#);
        assert_eq!(
            HEALTH_KILL_SWITCH_BODY,
            r#"{"status":"degraded","reason":"kill_switch"}"#
        );
        assert_eq!(
            HEALTH_NO_POLICY_BODY,
            r#"{"status":"degraded","reason":"no_policy"}"#
        );
        // The vocabulary tokens are the atoms of the raw bodies; pin that
        // relationship too so a one-sided token edit cannot drift unnoticed.
        assert!(HEALTH_OK_BODY.contains(HEALTH_STATUS_OK));
        assert!(HEALTH_KILL_SWITCH_BODY.contains(HEALTH_STATUS_DEGRADED));
        assert!(HEALTH_KILL_SWITCH_BODY.contains(HEALTH_REASON_KILL_SWITCH));
        assert!(HEALTH_NO_POLICY_BODY.contains(HEALTH_STATUS_DEGRADED));
        assert!(HEALTH_NO_POLICY_BODY.contains(HEALTH_REASON_NO_POLICY));
        // The three definitive bodies must stay mutually distinguishable.
        assert_ne!(HEALTH_OK_BODY, HEALTH_KILL_SWITCH_BODY);
        assert_ne!(HEALTH_OK_BODY, HEALTH_NO_POLICY_BODY);
        assert_ne!(HEALTH_KILL_SWITCH_BODY, HEALTH_NO_POLICY_BODY);
    }

    /// I93-D: pin the complete status→label vocabulary that all four HTTP
    /// services share through [`status_label`]. Because policy, event, mcp-gw
    /// and context no longer keep private copies, one canonical table here is
    /// the same mapping every service emits; a one-sided edit in a consumer
    /// cannot silently drift an availability bucket.
    ///
    /// The `503` row is the regression I93-D was about: it must stay its own
    /// label instead of collapsing to `other` (the old mcp-gw/context
    /// behavior). The `410` row is the I97-D regression: an expired approval
    /// window is a terminal outcome, not the 409 winner/halt branches, so it
    /// must stay observable as its own bucket. The final loop also proves the
    /// vocabulary is unchanged and bounded — other 4xx/5xx codes such as
    /// 418/429/502/504 still fall into `other`.
    #[cfg(feature = "http")]
    #[test]
    fn status_label_vocabulary_is_identical_for_all_services() {
        let labelled = [
            (200, "200"),
            (201, "201"),
            (204, "204"),
            (400, "400"),
            (401, "401"),
            (403, "403"),
            (404, "404"),
            (409, "409"),
            (410, "410"),
            (413, "413"),
            (415, "415"),
            (500, "500"),
            (503, "503"),
        ];
        for (code, expected) in labelled {
            let status = axum::http::StatusCode::from_u16(code).expect("valid status");
            assert_eq!(status_label(status), expected, "status {code}");
        }

        for code in [418, 429, 502, 504] {
            let status = axum::http::StatusCode::from_u16(code).expect("valid status");
            assert_eq!(status_label(status), "other", "status {code}");
        }

        // The availability signal must remain distinguishable from the bucket
        // that used to swallow it.
        let service_unavailable = axum::http::StatusCode::from_u16(503).expect("valid status");
        let unlisted = axum::http::StatusCode::from_u16(502).expect("valid status");
        assert_ne!(status_label(service_unavailable), status_label(unlisted));

        // I97-D: approval expiry is terminal but distinct from the 409
        // winner/halt branches; it must not fold back into `other`.
        let expired = axum::http::StatusCode::from_u16(410).expect("valid status");
        let decided = axum::http::StatusCode::from_u16(409).expect("valid status");
        assert_eq!(status_label(expired), "410");
        assert_ne!(status_label(expired), status_label(decided));
        assert_ne!(status_label(expired), status_label(unlisted));
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
    fn from_str_lossy_maps_uppercase_known_and_unknown_sources() {
        // Same case-insensitive mapping as FromStr.
        assert_eq!(
            EventSource::from_str_lossy("HOME_ASSISTANT"),
            EventSource::HomeAssistant
        );
        // Unknown values normalize to Manual instead of returning an error.
        assert_eq!(EventSource::from_str_lossy("mqtt"), EventSource::Manual);
        // Known lowercase values still resolve normally.
        assert_eq!(EventSource::from_str_lossy("android"), EventSource::Android);
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
