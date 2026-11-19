//! gRPC service for Context Builder

use crate::ContextBuilder;
use jarvis_core::{MemoryEntry, UnifiedEvent};
use tonic::{Request, Response, Status};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/proto/jarvis.context.v1.rs"));
}

use proto::context_service_server::{ContextService, ContextServiceServer};
use proto::{
    BuildContextRequest, BuildContextResponse, MemoryEntry as ProtoMemoryEntry,
    SearchMemoryRequest, SearchMemoryResponse, StoreMemoryRequest, StoreMemoryResponse,
};

pub struct GrpcContextService {
    builder: ContextBuilder,
}

/// Build the memory entry and its response without touching the store.
///
/// The caller-supplied id is deliberately discarded: honouring `req.id` would
/// let any client overwrite an existing memory by choosing its id (I8-A).
fn prepare_store_memory(req: StoreMemoryRequest) -> (MemoryEntry, StoreMemoryResponse) {
    let _ignored = &req.id;
    let id = uuid::Uuid::new_v4();
    let memory = MemoryEntry {
        id,
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
    let response = StoreMemoryResponse { id: id.to_string() };
    (memory, response)
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
    ) -> Result<Response<StoreMemoryResponse>, Status> {
        let (memory, response) = prepare_store_memory(request.into_inner());
        self.builder
            .store_memory(memory)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(response))
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

#[cfg(test)]
mod tests {
    use super::{StoreMemoryRequest, prepare_store_memory};

    fn store_request(supplied_id: &str) -> StoreMemoryRequest {
        StoreMemoryRequest {
            id: supplied_id.to_string(),
            content: "sensitive content".into(),
            embedding: vec![0.1_f32, 0.2, 0.3],
            metadata: r#"{"kind":"test"}"#.into(),
            source: "regression-test".into(),
            expires_at: 1_800_000_000,
        }
    }

    #[test]
    fn store_memory_ignores_caller_id_and_returns_generated_id() {
        let supplied = uuid::Uuid::new_v4().to_string();
        let (memory, response) = prepare_store_memory(store_request(&supplied));

        assert_ne!(
            memory.id.to_string(),
            supplied,
            "a caller-supplied id must never become the stored id (I8-A)"
        );
        assert_ne!(
            memory.id,
            uuid::Uuid::nil(),
            "the server must mint a real UUID"
        );
        assert_eq!(
            response.id,
            memory.id.to_string(),
            "the response must carry the id that was actually stored"
        );

        let (second, _) = prepare_store_memory(store_request(&supplied));
        assert_ne!(
            memory.id, second.id,
            "repeated requests with the same id get distinct records"
        );
    }

    #[test]
    fn store_memory_preserves_all_other_fields() {
        let (memory, _) = prepare_store_memory(store_request("ignored"));
        assert_eq!(memory.content, "sensitive content");
        assert_eq!(memory.embedding, vec![0.1_f32, 0.2, 0.3]);
        assert_eq!(memory.metadata, serde_json::json!({ "kind": "test" }));
        assert_eq!(memory.source, "regression-test");
        assert_eq!(
            memory.expires_at,
            chrono::DateTime::from_timestamp(1_800_000_000, 0)
        );
    }
}
