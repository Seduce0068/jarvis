//! gRPC service for Event Normalizer

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

pub struct GrpcEventService {
    normalizer: Arc<Normalizer>,
}

#[tonic::async_trait]
impl EventService for GrpcEventService {
    async fn emit_event(
        &self,
        request: Request<EmitEventRequest>,
    ) -> Result<Response<EmitEventResponse>, Status> {
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
    let service = GrpcEventService { normalizer };
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
}
