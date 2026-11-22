//! gRPC service for Event Normalizer

use crate::config::{EVENT_TOKEN_HEADER, ingress_admin_token};
use crate::normalizer::Normalizer;
use jarvis_core::{EventSource, UnifiedEvent};
use std::sync::Arc;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/proto/jarvis.event.v1.rs"));
}

use proto::event_service_server::{EventService, EventServiceServer};
use proto::{EmitEventRequest, EmitEventResponse};

/// Non-blocking submission for the gRPC `EmitEvent` RPC.
///
/// `Normalizer::emit` logs and drops on a full bounded channel, which would
/// leave the caller with `success: true` despite a lost event. Submitting via
/// the cloned sender from `event_sender()` surfaces rejection as
/// `RESOURCE_EXHAUSTED` (full) or `UNAVAILABLE` (closed), leaving the other
/// `emit` call sites unchanged. Only event metadata is logged — never the
/// payload.
fn submit_event(sender: &mpsc::Sender<UnifiedEvent>, event: UnifiedEvent) -> Result<(), Status> {
    match sender.try_send(event) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(event)) => {
            tracing::warn!(
                event_type = %event.event_type,
                source = %event.source,
                "gRPC EmitEvent: event channel full; rejecting with RESOURCE_EXHAUSTED"
            );
            Err(Status::resource_exhausted("event channel full"))
        }
        Err(mpsc::error::TrySendError::Closed(event)) => {
            tracing::warn!(
                event_type = %event.event_type,
                source = %event.source,
                "gRPC EmitEvent: event channel closed; rejecting with UNAVAILABLE"
            );
            Err(Status::unavailable("event channel closed"))
        }
    }
}

/// Authorize a gRPC call before any handler work runs.
///
/// A configured token must be supplied in the `x-jarvis-event-token` metadata;
/// when none is configured the legacy loopback path stays open. No token value
/// is ever logged.
///
/// Authentication establishes transport ingress only. The authorization
/// principal stays the fixed `"jarvis"` value used by `Normalizer::process_event`:
/// it is a runtime-independent policy contract, so caller-controlled metadata
/// must never be allowed to select the principal or the decision resource.
fn authorize<T>(request: &Request<T>, expected: Option<&str>) -> Result<(), Status> {
    let provided = request
        .metadata()
        .get(EVENT_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    if crate::config::token_matches(expected, provided) {
        Ok(())
    } else {
        tracing::warn!(
            "gRPC EmitEvent: missing or invalid admin token; rejecting with UNAUTHENTICATED"
        );
        Err(Status::unauthenticated(
            "missing or invalid event admin token",
        ))
    }
}

pub struct GrpcEventService {
    normalizer: Arc<Normalizer>,
    admin_token: Option<String>,
}

impl GrpcEventService {
    fn new(normalizer: Arc<Normalizer>) -> Self {
        Self {
            normalizer,
            admin_token: ingress_admin_token(),
        }
    }
}

/// Validate a request and build the event to enqueue.
///
/// proto3 has no required fields: an absent string arrives as `""` and an
/// absent `int64` as `0`. The handler therefore distinguishes "unspecified"
/// from malformed instead of replacing every parse failure with a default:
///
/// - `id` must parse as a UUID (`""` from an absent field is rejected);
/// - `source` must be non-empty; unknown values fall back to
///   [`EventSource::Manual`], matching the HTTP `/v1/events` path;
/// - `payload` must contain a valid JSON document (`""` from an absent field
///   is not JSON and is rejected, matching the HTTP endpoint where the field
///   is mandatory);
/// - `timestamp == 0` means "unspecified" and is replaced with the current
///   time; any other value must be representable as a `DateTime<Utc>`.
///
/// `event_type` and `correlation_id` are free-form strings that pass through
/// unchanged. No rejected value is logged, so a malicious payload never
/// reaches telemetry.
fn event_from_request(req: &EmitEventRequest) -> Result<UnifiedEvent, Status> {
    let id = match uuid::Uuid::parse_str(&req.id) {
        Ok(id) => id,
        Err(_) => return Err(Status::invalid_argument("id must be a valid UUID")),
    };

    if req.source.trim().is_empty() {
        return Err(Status::invalid_argument("source must not be empty"));
    }
    let source = EventSource::from_str_lossy(&req.source);

    if req.payload.trim().is_empty() {
        return Err(Status::invalid_argument("payload must be a JSON document"));
    }
    let payload = match serde_json::from_str::<serde_json::Value>(&req.payload) {
        Ok(payload) => payload,
        Err(_) => return Err(Status::invalid_argument("payload must be valid JSON")),
    };

    let timestamp = if req.timestamp == 0 {
        chrono::Utc::now()
    } else {
        match chrono::DateTime::from_timestamp(req.timestamp, 0) {
            Some(timestamp) => timestamp,
            None => return Err(Status::invalid_argument("timestamp is out of range")),
        }
    };

    Ok(UnifiedEvent {
        id,
        source,
        event_type: req.event_type.clone(),
        payload,
        timestamp,
        correlation_id: req.correlation_id.clone(),
    })
}

#[tonic::async_trait]
impl EventService for GrpcEventService {
    async fn emit_event(
        &self,
        request: Request<EmitEventRequest>,
    ) -> Result<Response<EmitEventResponse>, Status> {
        authorize(&request, self.admin_token.as_deref())?;
        let req = request.into_inner();
        // Reject malformed input before it can enter the bounded queue and be
        // acknowledged with `success: true`. Auth stays first so unauthenticated
        // callers cannot probe the validation rules.
        let event = event_from_request(&req)?;
        // Capture the canonical id before `event` moves into the queue so the
        // reply carries the same value as HTTP's `emit_response`:
        // `UnifiedEvent::id.to_string()`.
        let event_id = event.id.to_string();
        submit_event(&self.normalizer.event_sender(), event)?;
        Ok(Response::new(EmitEventResponse {
            success: true,
            event_id,
        }))
    }
}

pub async fn serve(normalizer: Arc<Normalizer>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let service = GrpcEventService::new(normalizer);
    tonic::transport::Server::builder()
        .add_service(EventServiceServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn submit_event_rejects_with_resource_exhausted_when_channel_full() {
        // Capacity 1 + one queued event => the next try_send is genuinely Full
        // (the receiver is kept alive until after the assertion).
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(test_event(1, "probe"))
            .expect("first event accepted");

        let status = submit_event(&tx, test_event(2, "overflow")).expect_err("overflow rejected");
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        drop(rx);
    }

    #[test]
    fn submit_event_accepts_when_channel_has_capacity() {
        let (tx, mut rx) = mpsc::channel(1);
        let event = test_event(3, "accepted");
        assert!(submit_event(&tx, event.clone()).is_ok());

        let queued = rx.try_recv().expect("event accepted");
        assert_eq!(queued.id, event.id);
        assert_eq!(queued.event_type, "accepted");
    }

    fn request_with_token(token: Option<&str>) -> Request<()> {
        let mut request = Request::new(());
        if let Some(token) = token {
            request.metadata_mut().insert(
                EVENT_TOKEN_HEADER,
                token.parse().expect("valid metadata token"),
            );
        }
        request
    }

    #[test]
    fn authorize_enforces_configured_token() {
        let request = request_with_token(Some("event-secret"));
        assert!(authorize(&request, Some("event-secret")).is_ok());

        for provided in [None, Some("wrong-secret")] {
            let request = request_with_token(provided);
            let status = authorize(&request, Some("event-secret"))
                .expect_err("missing or invalid token must be rejected");
            assert_eq!(status.code(), tonic::Code::Unauthenticated);
        }
    }

    #[test]
    fn authorize_keeps_legacy_loopback_open_without_token() {
        let request = request_with_token(None);
        assert!(authorize(&request, None).is_ok());
        let with_token = request_with_token(Some("anything"));
        assert!(authorize(&with_token, None).is_ok());
    }

    fn valid_request() -> EmitEventRequest {
        EmitEventRequest {
            id: uuid::Uuid::from_u128(42).to_string(),
            source: "web".into(),
            event_type: "test_event".into(),
            payload: r#"{"key":"value"}"#.into(),
            timestamp: 1_700_000_000,
            correlation_id: "corr-42".into(),
        }
    }

    #[test]
    fn event_from_request_accepts_a_valid_request() {
        let event = event_from_request(&valid_request()).expect("valid request accepted");
        assert_eq!(event.id, uuid::Uuid::from_u128(42));
        assert_eq!(event.source, EventSource::Web);
        assert_eq!(event.event_type, "test_event");
        assert_eq!(event.payload, json!({"key": "value"}));
        assert_eq!(
            event.timestamp,
            chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp")
        );
        assert_eq!(event.correlation_id, "corr-42");
    }

    #[test]
    fn event_from_request_rejects_invalid_or_missing_id() {
        for id in ["", "not-a-uuid", "00000000-0000-0000-0000-00000000000g"] {
            let mut req = valid_request();
            req.id = id.into();
            let status = event_from_request(&req).expect_err("invalid id must be rejected");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn event_from_request_rejects_empty_source() {
        for source in ["", "   "] {
            let mut req = valid_request();
            req.source = source.into();
            let status = event_from_request(&req).expect_err("empty source must be rejected");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn event_from_request_accepts_unknown_source_as_manual() {
        for source in ["mqtt", "telepathy"] {
            let mut req = valid_request();
            req.source = source.into();
            let event = event_from_request(&req).expect("unknown source accepted as Manual");
            assert_eq!(event.source, EventSource::Manual, "source {source}");
        }
    }

    #[test]
    fn event_from_request_rejects_invalid_or_missing_payload() {
        for payload in ["", "   ", "{not json", "null trailing"] {
            let mut req = valid_request();
            req.payload = payload.into();
            let status = event_from_request(&req).expect_err("invalid payload must be rejected");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn event_from_request_treats_zero_timestamp_as_unspecified() {
        let mut req = valid_request();
        req.timestamp = 0;
        let before = chrono::Utc::now();
        let event = event_from_request(&req).expect("zero timestamp means unspecified");
        let after = chrono::Utc::now();
        assert!(event.timestamp >= before && event.timestamp <= after);
    }

    #[test]
    fn event_from_request_rejects_out_of_range_timestamp() {
        for timestamp in [i64::MAX, i64::MIN] {
            let mut req = valid_request();
            req.timestamp = timestamp;
            let status = event_from_request(&req).expect_err("out-of-range timestamp rejected");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn event_from_request_accepts_valid_pre_epoch_timestamp() {
        let mut req = valid_request();
        req.timestamp = -1;
        let event = event_from_request(&req).expect("valid pre-epoch timestamp accepted");
        assert_eq!(
            event.timestamp,
            chrono::DateTime::from_timestamp(-1, 0).expect("valid timestamp")
        );
    }

    /// Fail-closed ports for the full-handler regression: the queued event is
    /// consumed by the worker task, but the deny gate stops it before any
    /// network call (no OpenCode dispatch, no policy/context service).
    struct DenyAllPolicyGate;

    #[async_trait::async_trait]
    impl crate::ports::PolicyGate for DenyAllPolicyGate {
        async fn is_allowed(
            &self,
            _principal: &str,
            _action: &str,
            _resource: &str,
            _context: &serde_json::Value,
        ) -> bool {
            false
        }
    }

    struct EmptyContextProvider;

    #[async_trait::async_trait]
    impl crate::ports::ContextProvider for EmptyContextProvider {
        async fn build_context(&self, _event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
            Ok(json!({}))
        }
    }

    #[tokio::test]
    async fn emit_event_response_returns_the_request_id() {
        let config = crate::config::EventConfig::default();
        let opencode =
            Arc::new(crate::opencode::OpenCodeClient::new(&config).expect("opencode client"));
        let normalizer = Normalizer::new(
            config,
            Arc::new(DenyAllPolicyGate),
            opencode,
            Arc::new(EmptyContextProvider),
        )
        .await
        .expect("normalizer");
        // Private fields are visible from this child module; setting the token
        // explicitly keeps the test independent of the host environment.
        let service = GrpcEventService {
            normalizer: Arc::new(normalizer),
            admin_token: None,
        };

        let req = valid_request();
        let expected_id = req.id.clone();
        let response = service
            .emit_event(Request::new(req))
            .await
            .expect("emit accepted");
        let body = response.into_inner();
        assert!(body.success);
        assert_eq!(body.event_id, expected_id);
    }
}
