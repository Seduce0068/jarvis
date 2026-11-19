//! HTTP/REST for Event Normalizer

use crate::config::{EVENT_TOKEN_HEADER, ingress_admin_token};
use crate::normalizer::Normalizer;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use jarvis_core::{BareJson, EventSource, UnifiedEvent};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tracing::instrument;

/// Handle for the process-wide Prometheus recorder.
static METRICS_HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();

/// Install the Prometheus recorder exactly once per process.
pub fn install_metrics() -> anyhow::Result<()> {
    let handle = metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder()?;
    if METRICS_HANDLE.set(handle).is_err() {
        tracing::warn!("metrics recorder already installed; keeping the first handle");
    }
    Ok(())
}

#[derive(Deserialize, Debug)]
struct EmitEventRequest {
    source: String,
    event_type: String,
    payload: serde_json::Value,
    correlation_id: Option<String>,
}

#[derive(Serialize, Debug)]
struct EmitEventResponse {
    success: bool,
    event_id: String,
}

/// Map a `/v1/events` source string to a `UnifiedEvent` source. Unknown values
/// fall back to `Manual`; this is the exact mapping used by the HTTP handler.
fn normalize_event_source(raw: &str) -> EventSource {
    match raw {
        "home_assistant" => EventSource::HomeAssistant,
        "android" => EventSource::Android,
        "gmail" => EventSource::Gmail,
        "calendar" => EventSource::Calendar,
        "web" => EventSource::Web,
        "finance" => EventSource::Finance,
        _ => EventSource::Manual,
    }
}

/// Pure request -> event conversion. Generated values are injected by the
/// caller so source normalization and correlation handling stay testable.
fn emit_event_from_request(
    req: EmitEventRequest,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    generate_correlation_id: impl FnOnce() -> String,
) -> UnifiedEvent {
    UnifiedEvent {
        id,
        source: normalize_event_source(&req.source),
        event_type: req.event_type,
        payload: req.payload,
        timestamp,
        correlation_id: req.correlation_id.unwrap_or_else(generate_correlation_id),
    }
}

/// Build the HTTP response from the emitted event (always successful).
fn emit_response(event: &UnifiedEvent) -> EmitEventResponse {
    EmitEventResponse {
        success: true,
        event_id: event.id.to_string(),
    }
}

/// Non-blocking submission for `/v1/events`.
///
/// `Normalizer::emit` intentionally logs and drops on a full bounded channel;
/// an API caller would see `success: true` and never learn that its event was
/// lost. Submitting through the cloned sender from `event_sender()` lets this
/// handler surface rejection as a retryable 503 instead, while leaving the
/// webhook/bridge `emit` path unchanged. Only event metadata is logged — never
/// the payload.
fn submit_event(
    sender: &mpsc::Sender<UnifiedEvent>,
    event: UnifiedEvent,
) -> Result<(), StatusCode> {
    match sender.try_send(event) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(event)) => {
            tracing::warn!(
                event_type = %event.event_type,
                source = %event.source,
                "/v1/events: event channel full; rejecting with 503"
            );
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(mpsc::error::TrySendError::Closed(event)) => {
            tracing::warn!(
                event_type = %event.event_type,
                source = %event.source,
                "/v1/events: event channel closed; rejecting with 503"
            );
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// Middleware state for the optional `/v1/events` token gate.
///
/// Deliberately not `Debug`: the configured token must never appear in a log
/// or trace record.
#[derive(Clone)]
struct AuthState {
    token: Option<String>,
}

impl AuthState {
    fn new(token: Option<&str>) -> Self {
        Self {
            token: token
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_owned),
        }
    }
}

/// Extract and verify the presented token for an ingress request.
fn header_token_allowed<T>(auth: &AuthState, request: &axum::http::Request<T>) -> bool {
    let provided = request
        .headers()
        .get(EVENT_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    crate::config::token_matches(auth.token.as_deref(), provided)
}

/// Optional gate for the event-ingress endpoint.
///
/// When no token is configured, the listener keeps its historical
/// unauthenticated behavior so existing loopback clients keep working. When a
/// token is configured, the exact value must be sent in `x-jarvis-event-token`.
/// Neither the expected nor the supplied value is ever logged.
///
/// Authentication establishes transport ingress only. The authorization
/// principal stays the fixed `"jarvis"` value owned by
/// `Normalizer::process_event`: that policy contract is runtime-independent,
/// so a caller must never be able to steer the principal or the decision
/// resource through this header.
async fn require_admin_token(
    State(auth): State<AuthState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, StatusCode> {
    if auth.token.is_some() && !header_token_allowed(&auth, &request) {
        tracing::warn!(
            method = %request.method(),
            path = %request.uri().path(),
            "event ingress: missing or invalid admin token; rejecting with 401"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

pub fn router(normalizer: Arc<Normalizer>) -> Router {
    router_with_admin_token(normalizer, ingress_admin_token())
}

/// Router with an explicit token so tests can exercise the gate without
/// depending on process-wide configuration.
fn router_with_admin_token(normalizer: Arc<Normalizer>, token: Option<String>) -> Router {
    let protected = Router::new().route("/v1/events", post(emit_event)).layer(
        axum::middleware::from_fn_with_state(AuthState::new(token.as_deref()), require_admin_token),
    );
    // `/health` and `/metrics` stay public: probes and Prometheus scraping
    // must keep working without credentials.
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .merge(protected)
        .with_state(normalizer)
        .layer(axum::middleware::from_fn(track_metrics))
}

/// Count every HTTP request so `/metrics` exposes more than an empty page.
async fn track_metrics(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    metrics::counter!(
        "jarvis_http_requests_total",
        "service" => "jarvis-event",
        "status" => status_label(response.status()),
    )
    .increment(1);
    response
}

fn status_label(status: axum::http::StatusCode) -> &'static str {
    match status.as_u16() {
        200 => "200",
        201 => "201",
        204 => "204",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        409 => "409",
        500 => "500",
        503 => "503",
        _ => "other",
    }
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

#[instrument(skip(normalizer, req))]
async fn emit_event(
    State(normalizer): State<Arc<Normalizer>>,
    req: BareJson<EmitEventRequest>,
) -> Result<Json<EmitEventResponse>, StatusCode> {
    // `BareJson` turns a type-mismatched body into an empty 400 before the
    // handler body runs; only deserialized data reaches this line.
    let BareJson(Json(req)) = req;
    let event = emit_event_from_request(req, uuid::Uuid::new_v4(), chrono::Utc::now(), || {
        uuid::Uuid::new_v4().to_string()
    });
    submit_event(&normalizer.event_sender(), event.clone())?;
    Ok(Json(emit_response(&event)))
}

pub async fn serve(normalizer: Arc<Normalizer>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(normalizer);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixed_id() -> uuid::Uuid {
        uuid::Uuid::from_u128(7)
    }

    fn fixed_timestamp() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp")
    }

    fn request(source: &str, correlation_id: Option<&str>) -> EmitEventRequest {
        EmitEventRequest {
            source: source.into(),
            event_type: "test_event".into(),
            payload: json!({"key": "value"}),
            correlation_id: correlation_id.map(str::to_string),
        }
    }

    #[test]
    fn tc_e2_known_source_strings_are_normalized() {
        let cases = [
            ("home_assistant", EventSource::HomeAssistant),
            ("android", EventSource::Android),
            ("gmail", EventSource::Gmail),
            ("calendar", EventSource::Calendar),
            ("web", EventSource::Web),
            ("finance", EventSource::Finance),
        ];
        for (raw, expected) in cases {
            let event =
                emit_event_from_request(request(raw, None), fixed_id(), fixed_timestamp(), || {
                    "generated".to_string()
                });
            assert_eq!(event.source, expected, "source {raw}");
        }
    }

    #[test]
    fn tc_e2_unknown_source_falls_back_to_manual_with_uuid_correlation() {
        let event =
            emit_event_from_request(request("mqtt", None), fixed_id(), fixed_timestamp(), || {
                uuid::Uuid::new_v4().to_string()
            });

        assert_eq!(event.id, fixed_id());
        assert_eq!(event.source, EventSource::Manual);
        assert_eq!(event.event_type, "test_event");
        assert_eq!(event.payload, json!({"key": "value"}));
        assert_eq!(event.timestamp, fixed_timestamp());
        assert!(
            event.correlation_id.parse::<uuid::Uuid>().is_ok(),
            "generated correlation id must be a UUID string"
        );

        let response = emit_response(&event);
        assert!(response.success);
        assert_eq!(response.event_id, fixed_id().to_string());
    }

    #[test]
    fn tc_e2_supplied_correlation_id_is_preserved() {
        let event = emit_event_from_request(
            request("android", Some("corr-supplied")),
            fixed_id(),
            fixed_timestamp(),
            || panic!("generator must not run when correlation id is supplied"),
        );
        assert_eq!(event.source, EventSource::Android);
        assert_eq!(event.correlation_id, "corr-supplied");
    }

    fn test_event(seq: u128, event_type: &str) -> UnifiedEvent {
        UnifiedEvent {
            id: uuid::Uuid::from_u128(seq),
            source: EventSource::Manual,
            event_type: event_type.into(),
            payload: json!({"seq": seq}),
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp"),
            correlation_id: format!("corr-{seq}"),
        }
    }

    #[test]
    fn tc_e2_submit_event_rejects_when_channel_full() {
        // Capacity 1 + one queued event => the next try_send is genuinely Full
        // (the receiver is kept alive until after the assertion).
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(test_event(1, "probe"))
            .expect("first event accepted");

        let result = submit_event(&tx, test_event(2, "overflow"));
        assert_eq!(result, Err(StatusCode::SERVICE_UNAVAILABLE));
        drop(rx);
    }

    #[test]
    fn tc_e2_submit_event_accepts_when_channel_has_capacity() {
        let (tx, mut rx) = mpsc::channel(1);
        let event = test_event(3, "accepted");
        assert!(submit_event(&tx, event.clone()).is_ok());

        let queued = rx.try_recv().expect("event accepted");
        assert_eq!(queued.id, event.id);
        assert_eq!(queued.event_type, "accepted");
    }

    #[tokio::test]
    async fn tc_e1_unreachable_policy_denies_auto_respond() {
        // Reserve then release a loopback port so no gRPC listener can answer.
        // No real policy service is contacted and no secret is involved.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
        drop(listener);

        let client = jarvis_policy::PolicyClient::new(endpoint).expect("lazy client");
        let allowed = client
            .is_allowed(
                "jarvis",
                "auto_respond",
                "event:state_changed",
                &json!({"entity_id": "light.kitchen"}),
            )
            .await;

        // Normalizer::process_event consumes this boolean verbatim: a false
        // result skips opencode.dispatch, so an unreachable policy service is
        // fail-closed without any event-side fallback.
        assert!(!allowed, "unreachable policy must deny auto_respond");
    }

    fn request_with_token(token: Option<&str>) -> axum::http::Request<()> {
        let mut request = axum::http::Request::builder()
            .uri("/v1/events")
            .body(())
            .expect("valid request");
        if let Some(token) = token {
            request.headers_mut().insert(
                EVENT_TOKEN_HEADER,
                axum::http::HeaderValue::from_str(token).expect("valid header value"),
            );
        }
        request
    }

    #[test]
    fn tc_e2_event_token_header_gate_enforces_configured_token() {
        let auth = AuthState::new(Some("event-secret"));
        assert!(header_token_allowed(
            &auth,
            &request_with_token(Some("event-secret"))
        ));
        assert!(!header_token_allowed(&auth, &request_with_token(None)));
        assert!(!header_token_allowed(
            &auth,
            &request_with_token(Some("wrong-secret"))
        ));
    }

    #[test]
    fn tc_e2_event_token_header_gate_is_open_without_configured_token() {
        let auth = AuthState::new(None);
        assert!(header_token_allowed(&auth, &request_with_token(None)));
        assert!(header_token_allowed(
            &auth,
            &request_with_token(Some("ignored"))
        ));
    }

    #[test]
    fn tc_e2_blank_configured_token_is_treated_as_unset() {
        assert!(AuthState::new(Some("   ")).token.is_none());
        assert_eq!(
            AuthState::new(Some(" secret ")).token.as_deref(),
            Some("secret")
        );
    }

    /// I59-C: `/v1/events` used stock `Json<T>`, so a wrong JSON type leaked
    /// axum's rejection body. The shared `BareJson` must map a type mismatch
    /// to a bare 400 with no detail; I61-A additionally maps a missing
    /// Content-Type to 415 and an oversized body to 413.
    #[tokio::test]
    async fn tc_i59_c_emit_event_type_mismatch_is_a_bare_400() {
        use axum::extract::FromRequest;

        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/v1/events")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"source":"android","event_type":42,"payload":{}}"#,
            ))
            .expect("request builds");
        let state = ();
        let outcome = BareJson::<EmitEventRequest>::from_request(request, &state).await;
        assert_eq!(
            outcome.unwrap_err(),
            StatusCode::BAD_REQUEST,
            "type mismatch must collapse to an empty 400"
        );
    }
}
