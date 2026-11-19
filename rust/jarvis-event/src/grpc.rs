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

#[tonic::async_trait]
impl EventService for GrpcEventService {
    async fn emit_event(
        &self,
        request: Request<EmitEventRequest>,
    ) -> Result<Response<EmitEventResponse>, Status> {
        authorize(&request, self.admin_token.as_deref())?;
        let req = request.into_inner();
        let event = UnifiedEvent {
            id: uuid::Uuid::parse_str(&req.id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            source: req.source.parse().unwrap_or(EventSource::Manual),
            event_type: req.event_type,
            payload: serde_json::from_str(&req.payload).unwrap_or_default(),
            timestamp: chrono::DateTime::from_timestamp(req.timestamp, 0)
                .unwrap_or_else(chrono::Utc::now),
            correlation_id: req.correlation_id,
        };
        submit_event(&self.normalizer.event_sender(), event)?;
        Ok(Response::new(EmitEventResponse { success: true }))
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
}
