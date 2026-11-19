//! gRPC service for Context Builder

use crate::ContextBuilder;
use crate::config::{CONTEXT_TOKEN_HEADER, token_matches};
use chrono::{DateTime, Utc};
use jarvis_core::{EventSource, MemoryEntry, UnifiedEvent};
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
    /// Effective value of `ContextConfig::admin_token`, shared with the HTTP
    /// gate. `None` keeps the legacy loopback path. Never logged.
    admin_token: Option<String>,
}

impl GrpcContextService {
    fn new(builder: ContextBuilder) -> Self {
        // `ContextBuilder.config` is visible here because `grpc` is a child
        // module of the crate root where that field is declared (same access
        // the HTTP router uses). Reading the config value keeps both transports
        // on one secret instead of duplicating the credential.
        let admin_token = builder.config.admin_token().map(str::to_owned);
        Self {
            builder,
            admin_token,
        }
    }
}

/// Authorize a gRPC call before any handler work runs.
///
/// A configured token must be supplied in the `x-jarvis-context-token`
/// metadata; when none is configured the legacy loopback path stays open.
/// Neither the expected nor the presented value is ever logged.
fn authorize<T>(request: &Request<T>, expected: Option<&str>) -> Result<(), Status> {
    let provided = request
        .metadata()
        .get(CONTEXT_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    if token_matches(expected, provided) {
        Ok(())
    } else {
        tracing::warn!(
            "gRPC context: missing or invalid admin token; rejecting with UNAUTHENTICATED"
        );
        Err(Status::unauthenticated(
            "missing or invalid context admin token",
        ))
    }
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
        score: None,
    };
    let response = StoreMemoryResponse { id: id.to_string() };
    (memory, response)
}

/// Map a core entry to its proto view, preserving the additive score (P1-1).
///
/// Kept as a pure function so score propagation is testable without a live
/// PostgreSQL/Qdrant-backed builder.
fn to_proto_memory_entry(memory: MemoryEntry) -> ProtoMemoryEntry {
    ProtoMemoryEntry {
        id: memory.id.to_string(),
        content: memory.content,
        embedding: memory.embedding,
        metadata: memory.metadata.to_string(),
        source: memory.source,
        created_at: memory.created_at.timestamp(),
        updated_at: memory.updated_at.timestamp(),
        expires_at: memory.expires_at.map(|t| t.timestamp()).unwrap_or(0),
        score: memory.score,
    }
}

/// Reassemble the event-identifying correlation fields from the additive proto
/// fields (I31-B).
///
/// `source == ""` and `timestamp == 0` are the proto3 "unset" values: they keep
/// the legacy `Manual` / `Utc::now()` behavior, so callers built against the old
/// schema are unchanged. When set, a known source and an in-range Unix
/// timestamp are preserved. An unknown source or an out-of-range timestamp
/// degrades to the legacy value instead of failing `BuildContext`: context
/// assembly is best-effort and an optional metadata field must not break an
/// otherwise valid request.
///
/// Public so the event-side gRPC provider can exercise the encode -> wire ->
/// decode contract in a unit test without opening a network connection.
pub fn event_metadata_from_request(req: &BuildContextRequest) -> (EventSource, DateTime<Utc>) {
    let source = if req.source.is_empty() {
        EventSource::Manual
    } else {
        req.source
            .parse::<EventSource>()
            .unwrap_or(EventSource::Manual)
    };
    let timestamp = if req.timestamp == 0 {
        Utc::now()
    } else {
        DateTime::from_timestamp(req.timestamp, 0).unwrap_or_else(Utc::now)
    };
    (source, timestamp)
}

#[tonic::async_trait]
impl ContextService for GrpcContextService {
    async fn build_context(
        &self,
        request: Request<BuildContextRequest>,
    ) -> Result<Response<BuildContextResponse>, Status> {
        authorize(&request, self.admin_token.as_deref())?;
        let req = request.into_inner();
        // Convert proto event to UnifiedEvent; the additive fields carry the
        // original source/timestamp, falling back to legacy values when unset.
        let (source, timestamp) = event_metadata_from_request(&req);
        let event = UnifiedEvent {
            id: uuid::Uuid::parse_str(&req.event_id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
            source,
            event_type: req.event_type,
            payload: serde_json::from_str(&req.payload).unwrap_or_default(),
            timestamp,
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
        // Auth stays first so unauthenticated callers cannot reach the store.
        authorize(&request, self.admin_token.as_deref())?;
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
        authorize(&request, self.admin_token.as_deref())?;
        let req = request.into_inner();
        let memories = self
            .builder
            .search_memory(&req.query, req.limit as usize)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(SearchMemoryResponse {
            memories: memories.into_iter().map(to_proto_memory_entry).collect(),
        }))
    }
}

pub async fn serve(builder: ContextBuilder, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let service = GrpcContextService::new(builder);
    tonic::transport::Server::builder()
        .add_service(ContextServiceServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BuildContextRequest, CONTEXT_TOKEN_HEADER, DateTime, EventSource, Request,
        StoreMemoryRequest, Utc, authorize, event_metadata_from_request, prepare_store_memory,
        to_proto_memory_entry,
    };
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// In-memory `MakeWriter` target: no new dependency and no global
    /// subscriber, so other tests can still install their own.
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

    fn request_with_token(token: Option<&str>) -> Request<()> {
        let mut request = Request::new(());
        if let Some(token) = token {
            request.metadata_mut().insert(
                CONTEXT_TOKEN_HEADER,
                token.parse().expect("valid metadata token"),
            );
        }
        request
    }

    #[test]
    fn token_metadata_key_matches_http_header_literal() {
        assert_eq!(CONTEXT_TOKEN_HEADER, "x-jarvis-context-token");
    }

    #[test]
    fn authorize_enforces_configured_token() {
        let request = request_with_token(Some("context-secret"));
        assert!(authorize(&request, Some("context-secret")).is_ok());

        for provided in [None, Some("wrong-secret")] {
            let request = request_with_token(provided);
            let status = authorize(&request, Some("context-secret"))
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

    #[test]
    fn rejected_grpc_call_never_logs_the_token() {
        const TOKEN: &str = "grpc-top-secret-42";
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();

        let status = tracing::subscriber::with_default(subscriber, || {
            let request = request_with_token(Some("wrong-token"));
            authorize(&request, Some(TOKEN)).expect_err("wrong token is rejected")
        });
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(
            !logs.contains(TOKEN),
            "the token must never be logged: {logs}"
        );
    }

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
        assert_eq!(memory.score, None, "a stored (unranked) entry has no score");
    }

    /// P1-1 over gRPC: the additive proto `score` carries the similarity for
    /// ranked entries and stays absent for non-ranked ones.
    #[test]
    fn proto_mapping_preserves_additive_score() {
        let (mut memory, _) = prepare_store_memory(store_request("ignored"));

        memory.score = Some(0.625);
        let ranked = to_proto_memory_entry(memory.clone());
        assert_eq!(ranked.score, Some(0.625));
        assert_eq!(ranked.id, memory.id.to_string());

        memory.score = None;
        let unranked = to_proto_memory_entry(memory);
        assert_eq!(unranked.score, None);
    }

    fn build_context_request(source: &str, timestamp: i64) -> BuildContextRequest {
        BuildContextRequest {
            event_id: uuid::Uuid::from_u128(7).to_string(),
            event_type: "state_changed".into(),
            payload: r#"{"entity_id":"light.kitchen"}"#.into(),
            correlation_id: "corr-7".into(),
            source: source.into(),
            timestamp,
        }
    }

    /// I31-B: a caller built against the old schema leaves both additive fields
    /// unset and must observe the exact legacy `Manual` / `now` behavior.
    #[test]
    fn build_context_without_additive_fields_keeps_legacy_metadata() {
        let req = build_context_request("", 0);
        let before = Utc::now();
        let (source, timestamp) = event_metadata_from_request(&req);
        let after = Utc::now();

        assert_eq!(source, EventSource::Manual);
        assert!(
            timestamp >= before && timestamp <= after,
            "unset timestamp must fall back to the current time"
        );
    }

    /// I31-B: a known source and an in-range Unix timestamp are preserved when
    /// the handler reassembles the `UnifiedEvent`.
    #[test]
    fn build_context_preserves_additive_source_and_timestamp() {
        let req = build_context_request("home_assistant", 1_700_000_000);
        let (source, timestamp) = event_metadata_from_request(&req);

        let expected = DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
        assert_eq!(source, EventSource::HomeAssistant);
        assert_eq!(timestamp, expected);
    }

    /// I31-B: malformed optional metadata degrades to the legacy values rather
    /// than panicking or failing the whole `BuildContext` request.
    #[test]
    fn build_context_invalid_additive_fields_degrade_without_panic() {
        let req = build_context_request("telepathy", i64::MAX);
        let before = Utc::now();
        let (source, timestamp) = event_metadata_from_request(&req);
        let after = Utc::now();

        assert_eq!(source, EventSource::Manual);
        assert!(timestamp >= before && timestamp <= after);
    }
}
