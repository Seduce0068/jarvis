//! gRPC service for Context Builder

use crate::ContextBuilder;
use jarvis_core::{MemoryEntry, UnifiedEvent};
use tonic::{Request, Response, Status};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/proto/jarvis.context.v1.rs"));
}

use proto::context_service_server::{ContextService, ContextServiceServer};
use proto::{
    BuildContextRequest, BuildContextResponse, Empty, MemoryEntry as ProtoMemoryEntry,
    SearchMemoryRequest, SearchMemoryResponse, StoreMemoryRequest,
};

pub struct GrpcContextService {
    builder: ContextBuilder,
}

#[tonic::async_trait]
impl ContextService for GrpcContextService {
    async fn build_context(
        &self,
        request: Request<BuildContextRequest>,
    ) -> Result<Response<BuildContextResponse>, Status> {
        let req = request.into_inner();
        // Convert proto event to UnifiedEvent (simplified)
        let event = UnifiedEvent {
            id: uuid::Uuid::parse_str(&req.event_id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            source: jarvis_core::EventSource::Manual,
            event_type: req.event_type,
            payload: serde_json::from_str(&req.payload).unwrap_or_default(),
            timestamp: chrono::Utc::now(),
            correlation_id: req.correlation_id,
        };
        let context = self
            .builder
            .build_context(&event)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(BuildContextResponse {
            context: context.to_string(),
        }))
    }

    async fn store_memory(
        &self,
        request: Request<StoreMemoryRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        let memory = MemoryEntry {
            id: uuid::Uuid::parse_str(&req.id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            content: req.content,
            embedding: req.embedding,
            metadata: serde_json::from_str(&req.metadata).unwrap_or_default(),
            source: req.source,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            expires_at: if req.expires_at > 0 {
                chrono::DateTime::from_timestamp(req.expires_at, 0)
            } else {
                None
            },
        };
        self.builder
            .store_memory(memory)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }

    async fn search_memory(
        &self,
        request: Request<SearchMemoryRequest>,
    ) -> Result<Response<SearchMemoryResponse>, Status> {
        let req = request.into_inner();
        let memories = self
            .builder
            .search_memory(&req.query, req.limit as usize)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(SearchMemoryResponse {
            memories: memories
                .into_iter()
                .map(|m| ProtoMemoryEntry {
                    id: m.id.to_string(),
                    content: m.content,
                    embedding: m.embedding,
                    metadata: m.metadata.to_string(),
                    source: m.source,
                    created_at: m.created_at.timestamp(),
                    updated_at: m.updated_at.timestamp(),
                    expires_at: m.expires_at.map(|t| t.timestamp()).unwrap_or(0),
                })
                .collect(),
        }))
    }
}

pub async fn serve(builder: ContextBuilder, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let service = GrpcContextService { builder };
    tonic::transport::Server::builder()
        .add_service(ContextServiceServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}
