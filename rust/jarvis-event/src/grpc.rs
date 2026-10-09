//! gRPC service for Event Normalizer

use tonic::{Request, Response, Status};
use crate::Normalizer;
use jarvis_core::{UnifiedEvent, EventSource};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/proto/jarvis.event.v1.rs"));
}

use proto::event_service_server::{EventService, EventServiceServer};
use proto::{EmitEventRequest, EmitEventResponse};

pub struct GrpcEventService {
    normalizer: Normalizer,
}

#[tonic::async_trait]
impl EventService for GrpcEventService {
    async fn emit_event(&self, request: Request<EmitEventRequest>) -> Result<Response<EmitEventResponse>, Status> {
        let req = request.into_inner();
        let event = UnifiedEvent {
            id: uuid::Uuid::parse_str(&req.id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            source: req.source.parse().unwrap_or(EventSource::Manual),
            event_type: req.event_type,
            payload: serde_json::from_str(&req.payload).unwrap_or_default(),
            timestamp: chrono::DateTime::from_timestamp(req.timestamp, 0).unwrap_or_else(chrono::Utc::now),
            correlation_id: req.correlation_id,
        };
        self.normalizer.emit(event);
        Ok(Response::new(EmitEventResponse { success: true }))
    }
}

pub async fn serve(normalizer: Normalizer, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let service = GrpcEventService { normalizer };
    tonic::transport::Server::builder()
        .add_service(EventServiceServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}