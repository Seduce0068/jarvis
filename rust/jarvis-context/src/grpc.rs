//! gRPC service for Context Builder

use crate::ContextBuilder;
use crate::config::validation::{self, InvalidInput};
use crate::config::{CONTEXT_TOKEN_HEADER, token_matches};
use crate::error::ContextError;
use chrono::{DateTime, Utc};
use jarvis_core::{EventSource, JARVIS_CONTEXT_TOKEN_HEADER, MemoryEntry, UnifiedEvent};
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/proto/jarvis.context.v1.rs"));
}

use proto::context_service_server::{ContextService, ContextServiceServer};
use proto::{
    BuildContextRequest, BuildContextResponse, MemoryEntry as ProtoMemoryEntry,
    SearchMemoryRequest, SearchMemoryResponse, StoreMemoryRequest, StoreMemoryResponse,
};

/// Minimal backend surface used by the gRPC handlers.
///
/// Mirrors the HTTP side's `MemoryBackend`: the handlers can be driven to a
/// real failure with an injected error instead of only being scanned from
/// source. Production wiring uses the real [`ContextBuilder`].
#[async_trait::async_trait]
pub(crate) trait GrpcBackend: Send + Sync {
    async fn build(&self, event: &UnifiedEvent) -> anyhow::Result<serde_json::Value>;
    async fn store(&self, memory: MemoryEntry) -> anyhow::Result<()>;
    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<MemoryEntry>>;
}

#[async_trait::async_trait]
impl GrpcBackend for ContextBuilder {
    async fn build(&self, event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
        ContextBuilder::build_context(self, event).await
    }

    async fn store(&self, memory: MemoryEntry) -> anyhow::Result<()> {
        ContextBuilder::store_memory(self, memory).await
    }

    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        ContextBuilder::search_memory(self, query, limit).await
    }
}

pub struct GrpcContextService {
    backend: Arc<dyn GrpcBackend>,
    /// Effective value of `ContextConfig::admin_token`, shared with the HTTP
    /// gate. `None` keeps the legacy loopback path. Never logged.
    admin_token: Option<String>,
    /// Configured Qdrant vector size, used by the shared ingress validator to
    /// reject a wrong-sized or empty embedding before it reaches the store.
    embedding_dim: usize,
}

impl GrpcContextService {
    fn new(builder: ContextBuilder) -> Self {
        // `ContextBuilder.config` is visible here because `grpc` is a child
        // module of the crate root where that field is declared (same access
        // the HTTP router uses). Reading the config value keeps both transports
        // on one secret instead of duplicating the credential.
        //
        // The legacy config-side header name is retained in `config.rs` (outside
        // this change's scope). Keep it in lockstep with the canonical constant
        // so the ingress key cannot drift from the HTTP gate and the client.
        debug_assert_eq!(
            CONTEXT_TOKEN_HEADER,
            jarvis_core::JARVIS_CONTEXT_TOKEN_HEADER
        );
        let admin_token = builder.config.admin_token().map(str::to_owned);
        let embedding_dim = builder.config.embedding_dim;
        Self {
            backend: Arc::new(builder),
            admin_token,
            embedding_dim,
        }
    }

    /// Inject a backend so the three handlers can be invoked directly in
    /// tests without opening a PostgreSQL/Qdrant-backed `ContextBuilder`.
    #[cfg(test)]
    pub(crate) fn with_backend(backend: Arc<dyn GrpcBackend>, admin_token: Option<String>) -> Self {
        Self {
            backend,
            admin_token,
            // Test fixtures use a three-element vector; production `new()`
            // reads the real configured dimension from `ContextConfig`.
            embedding_dim: 3,
        }
    }
}

/// Authorize a gRPC call before any handler work runs.
///
/// A configured token must be supplied in the
/// [`JARVIS_CONTEXT_TOKEN_HEADER`] metadata; when none is configured the legacy
/// loopback path stays open. Neither the expected nor the presented value is
/// ever logged.
fn authorize<T>(request: &Request<T>, expected: Option<&str>) -> Result<(), Status> {
    let provided = request
        .metadata()
        .get(JARVIS_CONTEXT_TOKEN_HEADER)
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

/// The only message a gRPC client ever receives for an internal failure.
///
/// Mirrors the HTTP transport's bare-500 contract (`http::log_internal_error`):
/// the caller learns that the operation failed, never why.
pub(crate) const INTERNAL_ERROR_MESSAGE: &str = "internal context error";

/// Reduce an internal error to a fixed-vocabulary category safe for logging.
///
/// `ContextError`'s `Display` can embed database URLs and SQL, Qdrant endpoints
/// and responses, PII payloads, and config paths. Only the variant token is
/// logged; the error text is neither echoed to the client nor written to any
/// log record.
pub(crate) fn error_kind(err: &anyhow::Error) -> &'static str {
    match err.downcast_ref::<ContextError>() {
        Some(ContextError::Database(_)) => "database",
        Some(ContextError::Qdrant(_)) => "qdrant",
        Some(ContextError::Pii(_)) => "pii",
        Some(ContextError::Config(_)) => "config",
        Some(ContextError::Grpc(_)) => "grpc",
        Some(ContextError::Serialization(_)) => "serialization",
        None => "other",
    }
}

/// Preserve an internal failure in the server log using fixed vocabulary only,
/// then return a bare `Status::internal` with no error detail attached.
///
/// Input errors keep their own classification at the call sites (e.g.
/// `InvalidArgument` for malformed JSON); this helper is only for failures
/// originating inside the store or context assembly.
fn internal_status<T>(result: anyhow::Result<T>, op: &'static str) -> Result<T, Status> {
    result.map_err(|err| {
        // Defense-in-depth: the core store re-checks required fields and may
        // surface an `InvalidInput`. It is a client error, so keep the same
        // `InvalidArgument` contract as ingress validation instead of hiding it
        // behind the bare internal failure.
        if let Some(invalid) = err.downcast_ref::<InvalidInput>() {
            return reject_invalid_input(invalid.field);
        }
        tracing::error!(
            operation = op,
            error_kind = error_kind(&err),
            "internal context error"
        );
        Status::internal(INTERNAL_ERROR_MESSAGE)
    })
}

/// Fixed client-facing message for input rejected at the gRPC boundary (I53-C).
///
/// Mirrors [`INTERNAL_ERROR_MESSAGE`]: the caller learns the request was
/// malformed, but never receives the offending bytes or a parser error string.
const INVALID_ARGUMENT_MESSAGE: &str = "invalid client input";

/// Reject malformed client input with `InvalidArgument` (I53-C).
///
/// A malformed `event_id` / `payload` / `metadata` used to become a fresh UUID /
/// `{}` via `new_v4()` / `unwrap_or_default()`, so the caller received `OK` while
/// the service used something other than what was sent. Input failures are now
/// fail-closed at the boundary.
///
/// Only the static `field` label chosen at the call site is logged — together
/// with the fixed `invalid_input` category — so the offending value cannot reach
/// either the status message or any log record (Round-49 no-leak policy).
fn reject_invalid_input(field: &'static str) -> Status {
    // Shared with the HTTP ingress so both transports emit the identical fixed
    // vocabulary for the same rejected input; no offending value is ever read.
    validation::log_invalid_input(field);
    Status::invalid_argument(INVALID_ARGUMENT_MESSAGE)
}

/// Parse a client-supplied event id, rejecting malformed values instead of
/// silently minting a new UUID (I53-C). Kept as a pure helper so the boundary is
/// testable without a live PostgreSQL/Qdrant-backed builder.
fn parse_event_id(value: &str) -> Result<uuid::Uuid, Status> {
    uuid::Uuid::parse_str(value).map_err(|_| reject_invalid_input("event_id"))
}

/// Parse a client-supplied payload string into JSON (I53-C).
fn parse_payload(value: &str) -> Result<serde_json::Value, Status> {
    serde_json::from_str(value).map_err(|_| reject_invalid_input("payload"))
}

/// Build the memory entry and its response without touching the store.
///
/// The caller-supplied id is deliberately discarded: honouring `req.id` would
/// let any client overwrite an existing memory by choosing its id (I8-A).
fn prepare_store_memory(
    req: StoreMemoryRequest,
    embedding_dim: usize,
) -> Result<(MemoryEntry, StoreMemoryResponse), Status> {
    let _ignored = &req.id;
    // I53-C / I55-A: every required-field check lives in the shared validator
    // (metadata must be an object, embedding length must match the configured
    // dimension, and a positive out-of-range expires_at must be rejected).
    // Boundary note: this is *not* the additive-field path — the event
    // `source`/`timestamp` and ranked `score` still degrade when absent.
    let memory = validation::prepare_memory(
        req.content,
        req.embedding,
        &req.metadata,
        req.source,
        req.expires_at,
        embedding_dim,
    )
    .map_err(|invalid| reject_invalid_input(invalid.field))?;
    let response = StoreMemoryResponse {
        id: memory.id.to_string(),
    };
    Ok((memory, response))
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
        // I53-C: event_id and payload are required inputs, not additive fields.
        // Reject malformed values with InvalidArgument instead of silently
        // minting a UUID / using an empty object.
        let id = parse_event_id(&req.event_id)?;
        let payload = parse_payload(&req.payload)?;
        // Convert proto event to UnifiedEvent; the additive fields carry the
        // original source/timestamp, falling back to legacy values when unset.
        let (source, timestamp) = event_metadata_from_request(&req);
        let event = UnifiedEvent {
            id,
            source,
            event_type: req.event_type,
            payload,
            timestamp,
            correlation_id: req.correlation_id,
        };
        let context = internal_status(self.backend.build(&event).await, "build_context")?;
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
        let (memory, response) = prepare_store_memory(request.into_inner(), self.embedding_dim)?;
        internal_status(self.backend.store(memory).await, "store_memory")?;
        Ok(Response::new(response))
    }

    async fn search_memory(
        &self,
        request: Request<SearchMemoryRequest>,
    ) -> Result<Response<SearchMemoryResponse>, Status> {
        authorize(&request, self.admin_token.as_deref())?;
        let req = request.into_inner();
        // I55-A: `as usize` used to turn limit=-1 into usize::MAX and reach
        // Qdrant through the over-fetch path. The shared validator performs the
        // `try_from` sign check and the common upper bound before any store call.
        let limit = validation::validate_search_limit(req.limit as i64)
            .map_err(|invalid| reject_invalid_input(invalid.field))?;
        let memories = internal_status(
            self.backend.search(&req.query, limit).await,
            "search_memory",
        )?;
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
        BuildContextRequest, CONTEXT_TOKEN_HEADER, ContextError, ContextService, DateTime,
        EventSource, GrpcBackend, GrpcContextService, INTERNAL_ERROR_MESSAGE,
        INVALID_ARGUMENT_MESSAGE, MemoryEntry, Request, SearchMemoryRequest, StoreMemoryRequest,
        UnifiedEvent, Utc, authorize, event_metadata_from_request, internal_status, parse_event_id,
        parse_payload, prepare_store_memory, reject_invalid_input, to_proto_memory_entry,
        validation,
    };
    use crate::error::QdrantErrorKind;
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
                jarvis_core::JARVIS_CONTEXT_TOKEN_HEADER,
                token.parse().expect("valid metadata token"),
            );
        }
        request
    }

    /// Cross-contract: the legacy config-side name must not drift from the
    /// canonical header the ingress now reads (and the HTTP gate / client use).
    #[test]
    fn token_metadata_key_agrees_with_the_shared_context_header() {
        assert_eq!(
            CONTEXT_TOKEN_HEADER,
            jarvis_core::JARVIS_CONTEXT_TOKEN_HEADER
        );
    }

    // I53-C: invalid input used to collapse to a default (empty JSON / fresh
    // UUID) and the caller still got OK. `*_PROBE_*` naming keeps these fixtures
    // clear of the repository secret scanner's credential-key patterns.
    const GRPC_INVALID_EVENT_ID_PROBE: &str = "not-a-uuid-I53C-PROBE";
    const GRPC_INVALID_JSON_PROBE: &str = "{not-json-I53C-PROBE";

    #[test]
    fn i53_c_invalid_event_id_is_invalid_argument() {
        let status = parse_event_id(GRPC_INVALID_EVENT_ID_PROBE)
            .expect_err("a malformed event id must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            status.message(),
            INVALID_ARGUMENT_MESSAGE,
            "only the fixed message may cross the gRPC boundary"
        );
        // Valid ids are still accepted.
        let valid = uuid::Uuid::new_v4().to_string();
        let expected = valid.parse::<uuid::Uuid>().expect("parse");
        assert_eq!(parse_event_id(&valid).expect("valid uuid"), expected);
    }

    #[test]
    fn i53_c_invalid_payload_is_invalid_argument() {
        let status = parse_payload(GRPC_INVALID_JSON_PROBE)
            .expect_err("a malformed payload must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), INVALID_ARGUMENT_MESSAGE);
        // Valid JSON payloads are still accepted.
        assert_eq!(
            parse_payload(r#"{"entity_id":"light.kitchen"}"#).expect("valid payload"),
            serde_json::json!({"entity_id": "light.kitchen"})
        );
    }

    #[test]
    fn i53_c_invalid_metadata_is_invalid_argument() {
        let mut req = store_request("ignored");
        req.metadata = GRPC_INVALID_JSON_PROBE.into();
        let status = prepare_store_memory(req, 3).expect_err("malformed metadata must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), INVALID_ARGUMENT_MESSAGE);
    }

    #[test]
    fn i53_c_valid_inputs_are_accepted() {
        let (memory, response) =
            prepare_store_memory(store_request("ignored"), 3).expect("valid metadata is accepted");
        assert_eq!(memory.metadata, serde_json::json!({"kind": "test"}));
        assert_eq!(response.id, memory.id.to_string());
    }

    /// I55-A: a negative gRPC `int32` limit used to become `usize::MAX` via
    /// `as`, over-fetch an unbounded candidate pool and reach Qdrant. Both it
    /// and an oversized positive value must now fail with the fixed message.
    #[test]
    fn i55_a_negative_and_oversized_limit_are_invalid_argument() {
        for raw in [-1_i32, validation::MAX_SEARCH_LIMIT as i32 + 1] {
            let status = validation::validate_search_limit(raw as i64)
                .map_err(|invalid| reject_invalid_input(invalid.field))
                .expect_err("limit must be rejected");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert_eq!(status.message(), INVALID_ARGUMENT_MESSAGE);
        }
    }

    /// I55-A: prove the real handler rejects the bad limit before the backend
    /// is reached — an injected backend error would otherwise surface as
    /// `Internal`, so the status code distinguishes the two paths.
    #[tokio::test]
    async fn i55_a_search_handler_rejects_bad_limit_before_backend_call() {
        use crate::error::test_support::ErrorCase;
        let service =
            GrpcContextService::with_backend(Arc::new(ErrorBackend(ErrorCase::Database)), None);
        for raw in [-1_i32, validation::MAX_SEARCH_LIMIT as i32 + 1] {
            let status = service
                .search_memory(Request::new(SearchMemoryRequest {
                    query: "probe".into(),
                    limit: raw,
                }))
                .await
                .expect_err("bad limit is rejected before the backend runs");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert_eq!(status.message(), INVALID_ARGUMENT_MESSAGE);
        }
    }

    /// I55-A: the required store fields are validated at the boundary. Additive
    /// fields keep their degrade contract; these are all required ones.
    #[test]
    fn i55_a_store_memory_rejects_invalid_required_fields() {
        let mut req = store_request("ignored");
        req.metadata = r#""text""#.into();
        assert_eq!(
            prepare_store_memory(req, 3)
                .expect_err("a non-object metadata must be rejected")
                .code(),
            tonic::Code::InvalidArgument
        );

        let mut req = store_request("ignored");
        req.metadata = "123".into();
        assert_eq!(
            prepare_store_memory(req, 3)
                .expect_err("a numeric metadata must be rejected")
                .code(),
            tonic::Code::InvalidArgument
        );

        let mut req = store_request("ignored");
        req.embedding = vec![0.1_f32, 0.2];
        assert_eq!(
            prepare_store_memory(req, 3)
                .expect_err("a wrong-sized embedding must be rejected")
                .code(),
            tonic::Code::InvalidArgument
        );

        let mut req = store_request("ignored");
        req.embedding.clear();
        assert_eq!(
            prepare_store_memory(req, 3)
                .expect_err("an empty embedding must be rejected")
                .code(),
            tonic::Code::InvalidArgument
        );

        let mut req = store_request("ignored");
        req.expires_at = i64::MAX;
        let status = prepare_store_memory(req, 3)
            .expect_err("an out-of-range expiry must not degrade to no expiry");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), INVALID_ARGUMENT_MESSAGE);
    }

    #[test]
    fn i53_c_invalid_input_logs_fixed_vocabulary_without_offending_value() {
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();

        let status = tracing::subscriber::with_default(subscriber, || {
            parse_event_id(GRPC_INVALID_EVENT_ID_PROBE).expect_err("malformed id is rejected")
        });
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), INVALID_ARGUMENT_MESSAGE);

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        // Fixed classification tokens are present; the offending value is not.
        assert!(logs.contains("invalid_input"), "{logs}");
        assert!(logs.contains("event_id"), "{logs}");
        assert!(logs.contains("invalid client input"), "{logs}");
        assert!(!logs.contains(GRPC_INVALID_EVENT_ID_PROBE), "{logs}");
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
        let (memory, response) =
            prepare_store_memory(store_request(&supplied), 3).expect("valid metadata is accepted");

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

        let (second, _) =
            prepare_store_memory(store_request(&supplied), 3).expect("valid metadata is accepted");
        assert_ne!(
            memory.id, second.id,
            "repeated requests with the same id get distinct records"
        );
    }

    #[test]
    fn store_memory_preserves_all_other_fields() {
        let (memory, _) =
            prepare_store_memory(store_request("ignored"), 3).expect("valid metadata is accepted");
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
        let (mut memory, _) =
            prepare_store_memory(store_request("ignored"), 3).expect("valid metadata is accepted");

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

    /// Regression: an internal failure must never echo its `Display` text —
    /// which can carry DB URLs/SQL, Qdrant endpoints/responses, PII bodies,
    /// config paths or decision bodies — into the gRPC status or the logs.
    #[test]
    fn internal_status_never_leaks_error_details() {
        const SENSITIVE: &str = "postgres://svc:pw@db.local:5432/jarvis SELECT secret FROM decisions /etc/jarvis/config.toml http://qdrant.local:6333/decision-body";
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();

        let status = tracing::subscriber::with_default(subscriber, || {
            // I51-D: the fixed-vocabulary constructor cannot carry hostile
            // text. Keep the probe as an outer `anyhow` context so the
            // regression still proves no part of the error chain — not only
            // the Qdrant variant — is echoed to the client or the logs.
            let err: anyhow::Error = ContextError::qdrant(QdrantErrorKind::FAILED).into();
            let err = err.context(SENSITIVE);
            internal_status::<()>(Err(err), "build_context").expect_err("internal failure")
        });

        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(
            status.message(),
            INTERNAL_ERROR_MESSAGE,
            "only the fixed message may cross the gRPC boundary"
        );
        assert!(!status.message().contains("postgres://"));
        assert!(!status.message().contains("SELECT"));
        assert!(!status.message().contains("/etc/jarvis"));

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        for token in [
            "postgres://",
            "SELECT",
            "/etc/jarvis",
            "http://qdrant",
            "decision-body",
        ] {
            assert!(
                !logs.contains(token),
                "sensitive detail leaked to logs: {token}"
            );
        }
        assert!(logs.contains("internal context error"));
        // Do not assert on the rendering of structured fields: their textual
        // form depends on the tracing-subscriber formatter version. The contract
        // here is that the raw error text never reaches either the status or
        // the logs; the fixed category fields are set at the call site.
    }

    /// Backend that fails every operation with a chosen fixed-vocabulary
    /// category. This is the gRPC counterpart of the HTTP side's failing
    /// backend: it lets all three handlers be invoked for real.
    struct ErrorBackend(crate::error::test_support::ErrorCase);

    #[async_trait::async_trait]
    impl GrpcBackend for ErrorBackend {
        async fn build(&self, _event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
            Err(self.0.into_error())
        }

        async fn store(&self, _memory: MemoryEntry) -> anyhow::Result<()> {
            Err(self.0.into_error())
        }

        async fn search(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
            Err(self.0.into_error())
        }
    }

    /// I55-C: behavioural proof for the gRPC side. Each of the three RPCs is
    /// really invoked with an injected error; it must return the fixed status
    /// message and emit a log record carrying the category of that error. A
    /// discarded result or a stale source literal cannot satisfy this.
    #[test]
    fn grpc_handlers_log_fixed_kind_for_all_seven_error_vocabularies() {
        use crate::error::test_support::{ErrorCase, SENSITIVE_PROBE};

        for case in ErrorCase::ALL {
            let buffer = LogBuffer::default();
            let writer = buffer.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_target(false)
                .with_writer(move || writer.clone())
                .finish();

            tracing::subscriber::with_default(subscriber, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime builds");
                runtime.block_on(async {
                    let service =
                        GrpcContextService::with_backend(Arc::new(ErrorBackend(case)), None);

                    let build = service
                        .build_context(Request::new(build_context_request(
                            "home_assistant",
                            1_700_000_000,
                        )))
                        .await
                        .expect_err("injected error must fail the RPC");
                    assert_eq!(build.code(), tonic::Code::Internal);
                    assert_eq!(build.message(), INTERNAL_ERROR_MESSAGE);

                    let store = service
                        .store_memory(Request::new(store_request("ignored")))
                        .await
                        .expect_err("injected error must fail the RPC");
                    assert_eq!(store.code(), tonic::Code::Internal);
                    assert_eq!(store.message(), INTERNAL_ERROR_MESSAGE);

                    let search = service
                        .search_memory(Request::new(SearchMemoryRequest {
                            query: "probe".into(),
                            limit: 5,
                        }))
                        .await
                        .expect_err("injected error must fail the RPC");
                    assert_eq!(search.code(), tonic::Code::Internal);
                    assert_eq!(search.message(), INTERNAL_ERROR_MESSAGE);
                });
            });

            let captured = buffer.0.lock().expect("log buffer mutex").clone();
            let logs = String::from_utf8(captured).expect("logs are utf-8");
            assert_eq!(
                logs.matches(case.token()).count(),
                3,
                "each gRPC RPC must log the injected category {case:?}: {logs}"
            );
            assert!(logs.contains("internal context error"), "{case:?}: {logs}");
            assert!(!logs.contains(SENSITIVE_PROBE), "gRPC leak: {logs}");
        }
    }
}
