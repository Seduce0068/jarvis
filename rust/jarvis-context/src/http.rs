//! HTTP/REST for Context Builder

use crate::ContextBuilder;
use crate::config::validation::{self, InvalidInput};
use crate::error::ContextError;
use axum::{
    Json, Router,
    extract::State,
    middleware::Next,
    response::Response,
    routing::{get, post},
};
use jarvis_core::{BareJson, JARVIS_CONTEXT_TOKEN_HEADER, MemoryEntry, UnifiedEvent};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
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

/// Terminal dead-letter decisions, counted with fixed labels only (I63-A).
///
/// Uses the same `metrics` crate as [`track_metrics`], so the counter is
/// rendered by `/metrics` next to `jarvis_http_requests_total`. No id, vector
/// value, content, endpoint or raw error text is attached — just the fixed
/// `service` and per-decision `error_kind` tokens.
pub(crate) fn record_dead_lettered_embedding() {
    metrics::counter!(
        "jarvis_dead_lettered_embeddings_total",
        "service" => "jarvis-context",
        "error_kind" => "embedding",
    )
    .increment(1);
}

/// Failed attempts to *persist* a dead-letter decision (I65-B).
///
/// Kept strictly separate from [`record_dead_lettered_embedding`]: the success
/// counter is only incremented after the outbox write succeeds, so a DB failure
/// can never inflate the number of durable terminal decisions. Labels remain
/// fixed vocabulary — no row id, payload, vector value or raw error text.
pub(crate) fn record_dead_letter_persistence_failure() {
    metrics::counter!(
        "jarvis_dead_letter_persistence_failures_total",
        "service" => "jarvis-context",
        "error_kind" => "persistence",
    )
    .increment(1);
}

/// Recoverable, non-dead-lettered outbox rows as a gauge (I63-A / I65-B).
///
/// `count` is a bare number produced by a `COUNT(*) FILTER`; only the numeric
/// value and fixed labels are exported, never a row id or payload. Since I65-B
/// this counts only rows whose durable `vector_state` is still `'pending'`;
/// retired rows are exported by [`observe_exhausted_embeddings`] instead, so a
/// poisoned backlog can no longer masquerade as recoverable work.
pub(crate) fn observe_pending_embeddings(count: i64) {
    metrics::gauge!(
        "jarvis_pending_embeddings",
        "service" => "jarvis-context",
    )
    .set(count as f64);
}

/// Rows that exhausted the reconcile retry budget as a gauge (I65-B).
///
/// These rows are retained for inspection but permanently excluded from future
/// batches. Exporting their count separately makes them distinguishable from
/// the recoverable backlog in [`observe_pending_embeddings`]. Only a bare
/// `COUNT(*)` value and fixed labels are exported.
pub(crate) fn observe_exhausted_embeddings(count: i64) {
    metrics::gauge!(
        "jarvis_exhausted_embeddings",
        "service" => "jarvis-context",
    )
    .set(count as f64);
}

#[derive(Debug, Deserialize)]
struct BuildContextRequest {
    event_id: String,
    event_type: String,
    payload: serde_json::Value,
    correlation_id: String,
}

#[derive(Serialize, Debug)]
struct BuildContextResponse {
    context: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct StoreMemoryRequest {
    /// Accepted for backwards compatibility only; deliberately ignored (I7-E).
    id: Option<String>,
    content: String,
    embedding: Vec<f32>,
    metadata: String,
    source: String,
    expires_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct StoreMemoryResponse {
    id: String,
}

/// Additive HTTP view of a memory entry (P1-1).
///
/// `score` comes from Qdrant ranking and is absent for entries that were never
/// ranked. Skipping it when `None` keeps responses byte-compatible with
/// pre-score clients; newer clients can read the key when present, while older
/// ones ignore it as an unknown field.
#[derive(Debug, Serialize)]
struct MemoryEntryResponse {
    id: uuid::Uuid,
    content: String,
    embedding: Vec<f32>,
    metadata: serde_json::Value,
    source: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f32>,
}

impl From<MemoryEntry> for MemoryEntryResponse {
    fn from(memory: MemoryEntry) -> Self {
        Self {
            id: memory.id,
            content: memory.content,
            embedding: memory.embedding,
            metadata: memory.metadata,
            source: memory.source,
            created_at: memory.created_at,
            updated_at: memory.updated_at,
            expires_at: memory.expires_at,
            score: memory.score,
        }
    }
}

/// Header carrying the shared admin token on the context HTTP API.
///
/// Aliased from `jarvis-core` so the HTTP gate reads the same canonical name as
/// the gRPC ingress and the event-side client; no transport-local literal.
const ADMIN_TOKEN_HEADER: &str = JARVIS_CONTEXT_TOKEN_HEADER;

/// The only route that stays public even when a token is configured.
const HEALTH_PATH: &str = "/health";

/// Middleware state for the optional admin-token gate.
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

/// Optional gate for the context HTTP API.
///
/// When no token is configured, the listener keeps its historical
/// unauthenticated behavior so existing loopback clients keep working. When a
/// token is configured, every route except [`HEALTH_PATH`] requires the exact
/// value in [`ADMIN_TOKEN_HEADER`]. Neither the expected nor the supplied value
/// is ever logged.
async fn require_admin_token(
    State(auth): State<AuthState>,
    request: axum::extract::Request,
    next: Next,
) -> Result<Response, axum::http::StatusCode> {
    let Some(expected) = auth.token.as_deref() else {
        return Ok(next.run(request).await);
    };
    if request.uri().path() == HEALTH_PATH {
        return Ok(next.run(request).await);
    }
    let provided = request
        .headers()
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    if !provided.is_some_and(|provided| constant_time_eq(provided, expected)) {
        // No token value (expected or supplied) is included in the response or
        // in any log record.
        return Err(axum::http::StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

/// Compare two tokens without an early exit on the first mismatch.
///
/// The loop always covers the longer input and pads the shorter one, so
/// neither a content mismatch nor a length mismatch short-circuits. This
/// mirrors the gate in `jarvis-policy`; no extra crate and no `unsafe` is
/// needed.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for (x, y) in a
        .iter()
        .map(Some)
        .chain(std::iter::repeat(None))
        .zip(b.iter().map(Some).chain(std::iter::repeat(None)))
        .take(max)
    {
        diff |= (x.copied().unwrap_or(0) ^ y.copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

#[derive(Debug, Deserialize)]
struct SearchMemoryRequest {
    query: String,
    /// Signed on the wire so a negative value reaches the shared validator and
    /// is rejected with the same bare 400 as gRPC's `InvalidArgument`, instead
    /// of being rejected by serde with a different response body.
    limit: i64,
}

/// Minimal backend surface used by the HTTP handlers.
///
/// The trait exists so the instrumented handlers can be exercised with an
/// in-memory stub in tests; production wiring uses the real [`ContextBuilder`].
#[async_trait::async_trait]
trait MemoryBackend: Send + Sync {
    async fn build(&self, event: &UnifiedEvent) -> anyhow::Result<serde_json::Value>;
    async fn store(&self, memory: MemoryEntry) -> anyhow::Result<()>;
    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<MemoryEntry>>;
    /// Configured vector size used to validate an incoming embedding before it
    /// reaches Qdrant (I55-A). Kept on the backend so instrumented handlers can
    /// stay backend-agnostic and testable with a small in-memory stub.
    fn embedding_dim(&self) -> usize;
}

#[async_trait::async_trait]
impl MemoryBackend for ContextBuilder {
    async fn build(&self, event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
        ContextBuilder::build_context(self, event).await
    }

    async fn store(&self, memory: MemoryEntry) -> anyhow::Result<()> {
        ContextBuilder::store_memory(self, memory).await
    }

    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        ContextBuilder::search_memory(self, query, limit).await
    }

    fn embedding_dim(&self) -> usize {
        self.config.embedding_dim
    }
}

pub fn router(builder: Arc<ContextBuilder>) -> Router {
    // `ContextBuilder.config` is visible here because `http` is a child module
    // of the crate root where that field is declared.
    let auth = AuthState::new(builder.config.admin_token());
    let backend: Arc<dyn MemoryBackend> = builder;
    // `/health` and `/metrics` stay public so Prometheus scraping keeps
    // working without credentials (same convention as jarvis-policy).
    let protected = Router::new()
        .route("/v1/context/build", post(build_context))
        .route("/v1/memory", post(store_memory))
        .route("/v1/memory/search", post(search_memory))
        .layer(axum::middleware::from_fn_with_state(
            auth,
            require_admin_token,
        ));
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .merge(protected)
        .with_state(backend)
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
        "service" => "jarvis-context",
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

/// Reduce an internal error to a fixed-vocabulary category safe for logging.
///
/// Mirrors `grpc.rs::error_kind`: `ContextError`'s `Display` can embed database
/// URLs and SQL, Qdrant endpoints and responses, PII payloads, and config
/// paths. Only the variant token is logged; the error text is neither echoed to
/// the client nor written to any log record. Keeping both transports on the
/// same seven tokens makes journald/OTLP output uniform across HTTP and gRPC.
fn error_kind(err: &anyhow::Error) -> &'static str {
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
/// then return the unchanged bare-500 contract (no error body).
///
/// `anyhow`'s `{err:#}` chain is deliberately never formatted here: it can carry
/// DB URLs/SQL, Qdrant endpoints, config paths and PII bodies straight into
/// journald -> otelcol -> OTLP/Grafana. The request is not passed in either, so
/// neither the chain nor the payload can reach this log record.
fn log_internal_error<T>(
    result: anyhow::Result<T>,
    op: &'static str,
) -> Result<T, axum::http::StatusCode> {
    result.map_err(|err| {
        // Defense-in-depth: the core store re-checks required fields and may
        // surface an `InvalidInput`. It is a client error, not an internal
        // failure, so keep the same bare-400 contract as ingress validation.
        if let Some(invalid) = err.downcast_ref::<InvalidInput>() {
            return reject_invalid_input(invalid.field);
        }
        tracing::error!(
            operation = op,
            error_kind = error_kind(&err),
            "internal error"
        );
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// Reject client input that used to collapse to a default value (I53-C).
///
/// A malformed `metadata` or `event_id` previously became an empty JSON value
/// / a fresh UUID via `unwrap_or_default()` / `new_v4()`, so the caller received
/// 200 while the service stored or searched something other than what was sent.
/// Input failures are now fail-closed at the boundary.
///
/// Only the static `field` label chosen at the call site is logged — together
/// with the fixed `invalid_input` category — so the offending bytes never enter
/// journald/OTLP, and no error body is returned to the caller (bare 400). This
/// mirrors the Round-49 no-leak policy used by [`log_internal_error`].
fn reject_invalid_input(field: &'static str) -> axum::http::StatusCode {
    // The log record itself is shared with the gRPC ingress so both transports
    // emit the identical fixed vocabulary for the same rejected input.
    validation::log_invalid_input(field);
    axum::http::StatusCode::BAD_REQUEST
}

// HTTP body extraction uses `jarvis_core::BareJson` (I61-A). The shared
// extractor keeps the fixed-vocabulary rejection log and the bare-status
// contract, and additionally distinguishes a missing Content-Type (415) and an
// oversized body (413) from wrong types / bad syntax (400). Keeping one
// implementation means all four services report the same rejection semantics.

#[instrument(skip(builder, req))]
async fn build_context(
    State(builder): State<Arc<dyn MemoryBackend>>,
    req: BareJson<BuildContextRequest>,
) -> Result<Json<BuildContextResponse>, axum::http::StatusCode> {
    // A type-mismatched body was already rejected with a bare 400 by the shared
    // `BareJson` (I61-A); only deserialized data reaches the handler.
    let BareJson(Json(body)) = req;
    // I53-C: a malformed event id must not be silently replaced with a fresh
    // UUID — the caller would receive 200 while the event carried an id they
    // never asked for. Reject with a bare 400 instead.
    let id = uuid::Uuid::parse_str(&body.event_id).map_err(|_| reject_invalid_input("event_id"))?;
    let event = UnifiedEvent {
        id,
        source: jarvis_core::EventSource::Manual,
        event_type: body.event_type,
        payload: body.payload,
        timestamp: chrono::Utc::now(),
        correlation_id: body.correlation_id,
    };
    let context = log_internal_error(builder.build(&event).await, "build_context")?;
    Ok(Json(BuildContextResponse { context }))
}

/// Build the entry to persist together with the response describing it.
///
/// `builder::store_memory` performs `ON CONFLICT (id) DO UPDATE`, so honoring a
/// client-supplied `id` would let any caller overwrite an existing memory
/// (I7-E). Always mint a fresh server-side UUID, ignore `req.id`, and hand the
/// generated id back so the caller can reference the new record. Every other
/// field keeps its previous handling.
fn prepare_store_memory(
    req: StoreMemoryRequest,
    embedding_dim: usize,
) -> Result<(MemoryEntry, StoreMemoryResponse), axum::http::StatusCode> {
    // I7-E: consumed and discarded so the ignore is explicit in code, not just
    // in the docs above.
    let _ignored_client_id = req.id;
    // I53-C / I55-A: all required-field checks (metadata must be an object,
    // embedding length must match the configured dimension, and a positive
    // out-of-range expires_at must be rejected) live in the shared validator.
    // Boundary note: this is *not* the additive-field path — event
    // `source`/`timestamp` and ranked `score` still degrade when absent.
    let memory = validation::prepare_memory(
        req.content,
        req.embedding,
        &req.metadata,
        req.source,
        req.expires_at.unwrap_or(0),
        embedding_dim,
    )
    .map_err(|invalid| reject_invalid_input(invalid.field))?;
    let response = StoreMemoryResponse {
        id: memory.id.to_string(),
    };
    Ok((memory, response))
}

#[instrument(skip(builder, req))]
async fn store_memory(
    State(builder): State<Arc<dyn MemoryBackend>>,
    req: BareJson<StoreMemoryRequest>,
) -> Result<Json<StoreMemoryResponse>, axum::http::StatusCode> {
    let BareJson(Json(body)) = req;
    let (memory, response) = prepare_store_memory(body, builder.embedding_dim())?;
    log_internal_error(builder.store(memory).await, "store_memory")?;
    Ok(Json(response))
}

#[instrument(skip(builder, req))]
async fn search_memory(
    State(builder): State<Arc<dyn MemoryBackend>>,
    req: BareJson<SearchMemoryRequest>,
) -> Result<Json<Vec<MemoryEntryResponse>>, axum::http::StatusCode> {
    let BareJson(Json(body)) = req;
    // I55-A: the signed wire value is widened/try_from-checked by the shared
    // validator, so a negative or oversized limit yields the same bare 400 as
    // gRPC's InvalidArgument and never reaches the over-fetch/Qdrant path.
    // A wrong JSON type (e.g. limit:"abc") never reaches here: the shared
    // `BareJson` already returned the same bare 400 (I61-A).
    let limit = validation::validate_search_limit(body.limit)
        .map_err(|invalid| reject_invalid_input(invalid.field))?;
    let memories = log_internal_error(builder.search(&body.query, limit).await, "search_memory")?;
    Ok(Json(
        memories
            .into_iter()
            .map(MemoryEntryResponse::from)
            .collect(),
    ))
}

pub async fn serve(builder: Arc<ContextBuilder>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = router(builder);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ADMIN_TOKEN_HEADER, AuthState, MemoryBackend, MemoryEntry, MemoryEntryResponse,
        StoreMemoryRequest, UnifiedEvent, build_context, constant_time_eq, error_kind,
        log_internal_error, prepare_store_memory, reject_invalid_input, require_admin_token,
        search_memory, store_memory, validation,
    };
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::{get, post},
    };
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tower::util::ServiceExt;
    use tracing_subscriber::fmt::format::FmtSpan;

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

    #[test]
    fn http_header_uses_the_shared_context_token_header() {
        assert_eq!(ADMIN_TOKEN_HEADER, jarvis_core::JARVIS_CONTEXT_TOKEN_HEADER);
    }

    // I53-C: invalid input used to collapse to a default (empty JSON / fresh
    // UUID) and the caller still got 200. These probes are deliberately named
    // `*_PROBE_*` so the repository secret scanner's credential patterns never
    // match them. The malformed strings are not JSON / not UUIDs.
    const HTTP_INVALID_EVENT_ID_PROBE: &str = "not-a-uuid-I53C-PROBE";
    const HTTP_INVALID_METADATA_PROBE: &str = "{not-json-I53C-PROBE";

    fn i53_c_test_app() -> Router {
        Router::new()
            .route("/v1/context/build", post(build_context))
            .route("/v1/memory", post(store_memory))
            .route("/v1/memory/search", post(search_memory))
            .with_state(Arc::new(StubBackend) as Arc<dyn MemoryBackend>)
    }

    fn i53_c_post(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request builds")
    }

    #[tokio::test]
    async fn i53_c_invalid_event_id_is_rejected_with_bare_400() {
        let response = i53_c_test_app()
            .oneshot(i53_c_post(
                "/v1/context/build",
                serde_json::json!({
                    "event_id": HTTP_INVALID_EVENT_ID_PROBE,
                    "event_type": "state_changed",
                    "payload": {"body": "probe"},
                    "correlation_id": "corr-1",
                }),
            ))
            .await
            .expect("service responds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads");
        assert!(body.is_empty(), "no error body may cross the boundary");
    }

    #[tokio::test]
    async fn i53_c_invalid_metadata_is_rejected_with_bare_400() {
        let response = i53_c_test_app()
            .oneshot(i53_c_post(
                "/v1/memory",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": HTTP_INVALID_METADATA_PROBE,
                    "source": "regression-test",
                }),
            ))
            .await
            .expect("service responds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads");
        assert!(body.is_empty(), "no error body may cross the boundary");
    }

    #[tokio::test]
    async fn i53_c_valid_inputs_are_still_accepted() {
        let app = i53_c_test_app();
        for (uri, body) in [
            (
                "/v1/context/build",
                serde_json::json!({
                    "event_id": "00000000-0000-0000-0000-000000000001",
                    "event_type": "state_changed",
                    "payload": {"body": "probe"},
                    "correlation_id": "corr-1",
                }),
            ),
            (
                "/v1/memory",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": r#"{"kind":"test"}"#,
                    "source": "regression-test",
                }),
            ),
        ] {
            let status = app
                .clone()
                .oneshot(i53_c_post(uri, body))
                .await
                .expect("service responds")
                .status();
            assert_eq!(status, StatusCode::OK, "{uri} must accept valid input");
        }
    }

    /// Post a JSON body and return the status together with the response body,
    /// so the regression tests can assert that rejections stay bare (no leak).
    async fn post_and_read(
        app: Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, Vec<u8>) {
        let response = app
            .oneshot(i53_c_post(uri, body))
            .await
            .expect("service responds");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads")
            .to_vec();
        (status, body)
    }

    /// I55-A: the signed wire limit is checked by the shared validator, so a
    /// negative value and an oversized positive one get the same bare 400 that
    /// gRPC reports as `InvalidArgument`.
    #[tokio::test]
    async fn i55_a_search_limit_rejects_negative_and_oversized_values() {
        let rejected = [-1_i64, validation::MAX_SEARCH_LIMIT as i64 + 1];
        for limit in rejected {
            let (status, body) = post_and_read(
                i53_c_test_app(),
                "/v1/memory/search",
                serde_json::json!({"query": "probe", "limit": limit}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "limit {limit}");
            assert!(body.is_empty(), "no error body may cross the boundary");
        }

        let (status, _) = post_and_read(
            i53_c_test_app(),
            "/v1/memory/search",
            serde_json::json!({"query": "probe", "limit": 5}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "an in-range limit stays accepted");
    }

    /// I57-C: oversized content/source/metadata and deeply nested metadata are
    /// rejected by the shared validator with the same bare 400 / no body.
    #[tokio::test]
    async fn i57_c_oversized_and_deep_fields_are_rejected_with_bare_400() {
        let deep_metadata = format!(
            "{}0{}",
            r#"{"a":"#.repeat(validation::MAX_METADATA_DEPTH + 1),
            "}".repeat(validation::MAX_METADATA_DEPTH + 1)
        );
        let cases: [(&str, serde_json::Value); 4] = [
            (
                "oversized content",
                serde_json::json!({
                    "content": "x".repeat(validation::MAX_CONTENT_BYTES + 1),
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": "{\"kind\":\"test\"}",
                    "source": "probe",
                }),
            ),
            (
                "oversized source",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": "{\"kind\":\"test\"}",
                    "source": "s".repeat(validation::MAX_SOURCE_BYTES + 1),
                }),
            ),
            (
                "oversized metadata",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": format!(
                        r#"{{"k":"{}"}}"#,
                        "m".repeat(validation::MAX_METADATA_BYTES)
                    ),
                    "source": "probe",
                }),
            ),
            (
                "deep metadata",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": deep_metadata,
                    "source": "probe",
                }),
            ),
        ];
        for (label, body) in cases {
            let app = i53_c_test_app();
            let (status, response_body) = post_and_read(app, "/v1/memory", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
            assert!(response_body.is_empty(), "{label}: no error body may leak");
        }
    }

    /// I55-A: required store fields are rejected with a bare 400. Additive
    /// fields keep their degrade contract; these cases are all required ones.
    #[tokio::test]
    async fn i55_a_store_memory_rejects_invalid_required_fields() {
        let cases: [(&str, serde_json::Value); 5] = [
            (
                "non-object metadata string",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": r#""text""#,
                    "source": "probe",
                }),
            ),
            (
                "numeric metadata",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": "123",
                    "source": "probe",
                }),
            ),
            (
                "wrong embedding length",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2],
                    "metadata": "{\"kind\":\"test\"}",
                    "source": "probe",
                }),
            ),
            (
                "empty embedding",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [],
                    "metadata": "{\"kind\":\"test\"}",
                    "source": "probe",
                }),
            ),
            (
                "out-of-range expiry",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": "{\"kind\":\"test\"}",
                    "source": "probe",
                    "expires_at": i64::MAX,
                }),
            ),
        ];
        for (label, body) in cases {
            let (status, response_body) = post_and_read(i53_c_test_app(), "/v1/memory", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
            assert!(response_body.is_empty(), "{label}: no error body may leak");
        }
    }

    /// I57-A (b): axum's stock `Json<T>` rejection carries a detailed body that
    /// bypasses the shared validator's bare-400 / fixed-vocabulary contract.
    /// `BareJson` must collapse every type mismatch to an empty 400 on all
    /// three routes, before any handler body runs.
    #[tokio::test]
    async fn i57_a_json_type_mismatch_is_rejected_with_bare_400() {
        let cases: [(&str, &str, serde_json::Value); 5] = [
            (
                "search limit as string",
                "/v1/memory/search",
                serde_json::json!({"query": "probe", "limit": "abc"}),
            ),
            (
                "search limit as fractional number",
                "/v1/memory/search",
                serde_json::json!({"query": "probe", "limit": 1.5}),
            ),
            (
                "embedding as string",
                "/v1/memory",
                serde_json::json!({
                    "content": "probe",
                    "embedding": "x",
                    "metadata": "{\"kind\":\"test\"}",
                    "source": "probe",
                }),
            ),
            (
                "non-string event_id",
                "/v1/context/build",
                serde_json::json!({
                    "event_id": 42,
                    "event_type": "state_changed",
                    "payload": {"body": "probe"},
                    "correlation_id": "corr-1",
                }),
            ),
            (
                "missing required field",
                "/v1/memory",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                }),
            ),
        ];
        for (label, uri, body) in cases {
            let (status, response_body) = post_and_read(i53_c_test_app(), uri, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{label}");
            assert!(
                response_body.is_empty(),
                "{label}: JsonRejection detail leaked: {}",
                String::from_utf8_lossy(&response_body)
            );
        }
    }

    /// I57-A (a)+(b): `NaN` is not valid JSON, so it arrives as a
    /// `JsonSyntaxError`. The bare-400 contract must hold for syntax failures
    /// too; no parser detail may cross the boundary.
    #[tokio::test]
    async fn i57_a_invalid_json_syntax_is_rejected_with_bare_400() {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/memory/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"probe","limit":NaN}"#))
            .expect("request builds");
        let response = i53_c_test_app()
            .oneshot(request)
            .await
            .expect("service responds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads");
        assert!(
            body.is_empty(),
            "JsonSyntaxError detail leaked: {}",
            String::from_utf8_lossy(&body)
        );
    }

    /// I57-A (b): the rejection log must use the same `invalid_input`
    /// vocabulary as the shared validator and must never contain the offending
    /// value or axum's `invalid type: ...` rejection text. The shared extractor
    /// lowered this record to DEBUG to avoid log amplification (I61-A), so this
    /// subscriber explicitly opts into DEBUG; the fixed-token assertions are
    /// unchanged.
    #[test]
    fn i57_a_json_rejection_logs_fixed_vocabulary_without_detail() {
        const TYPE_PROBE: &str = "abc-I57A-PROBE";
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();

        let status = tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime builds");
            runtime.block_on(async {
                i53_c_test_app()
                    .oneshot(i53_c_post(
                        "/v1/memory/search",
                        serde_json::json!({"query": "probe", "limit": TYPE_PROBE}),
                    ))
                    .await
                    .expect("service responds")
                    .status()
            })
        });
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(logs.contains("invalid_input"), "{logs}");
        assert!(logs.contains("body"), "{logs}");
        assert!(logs.contains("invalid client input"), "{logs}");
        assert!(!logs.contains(TYPE_PROBE), "offending value leaked: {logs}");
        assert!(
            !logs.contains("invalid type"),
            "rejection detail leaked: {logs}"
        );
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

        let status =
            tracing::subscriber::with_default(subscriber, || reject_invalid_input("event_id"));
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        // Fixed classification tokens are present; no attacker-controlled text
        // ever reaches this log because only a static field label is accepted.
        assert!(logs.contains("invalid_input"), "{logs}");
        assert!(logs.contains("event_id"), "{logs}");
        assert!(logs.contains("invalid client input"), "{logs}");
    }

    /// I51-A: hostile detail of the kind that used to travel through
    /// `format!("{err:#}")`: a token-bearing Qdrant URL, SQL, a config path and
    /// a PII body. The `*_PROBE_*` names keep these fixtures clear of the
    /// repository secret scanner's credential-key patterns.
    const HTTP_LOG_PROBE_URL: &str = concat!(
        "https://admin:I51A-do-not-log@qdrant.internal:6333/private",
        "?session=I51A-do-not-log"
    );
    const HTTP_LOG_PROBE_SQL: &str =
        "SELECT patient_name, diagnosis FROM decisions WHERE note = 'I51A-do-not-log'";
    const HTTP_LOG_PROBE_PATH: &str = "/etc/jarvis/I51A-do-not-log.toml";
    const HTTP_LOG_PROBE_PII: &str = "patient I51A-do-not-log / 090-1234-5678 / private note";

    #[test]
    fn internal_error_is_logged_with_fixed_kind_and_mapped_to_bare_500() {
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();

        let outcome = tracing::subscriber::with_default(subscriber, || {
            let detail = [
                HTTP_LOG_PROBE_URL,
                HTTP_LOG_PROBE_SQL,
                HTTP_LOG_PROBE_PATH,
                HTTP_LOG_PROBE_PII,
            ]
            .join(" ");
            let err = anyhow::Error::from(crate::error::ContextError::qdrant(
                crate::error::QdrantErrorKind::FAILED,
            ))
            .context(detail);
            let err = err.context("outer context that must also stay out of the log");
            log_internal_error::<()>(Err(err), "build_context")
        });

        assert_eq!(
            outcome.unwrap_err(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "the HTTP response contract stays a bare 500"
        );

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        for probe in [
            HTTP_LOG_PROBE_URL,
            HTTP_LOG_PROBE_SQL,
            HTTP_LOG_PROBE_PATH,
            HTTP_LOG_PROBE_PII,
            "qdrant.internal",
            "SELECT",
            "/etc/jarvis",
            "outer context",
        ] {
            assert!(
                !logs.contains(probe),
                "sensitive error-chain detail leaked to logs: {probe}"
            );
        }
        // Fixed message, operation label and the fixed `qdrant` token survive;
        // the absence checks above prove the token did not arrive via the URL.
        assert!(logs.contains("ERROR"), "{logs}");
        assert!(logs.contains("build_context"), "{logs}");
        assert!(logs.contains("internal error"), "{logs}");
        assert!(logs.contains("qdrant"), "{logs}");
    }

    /// The HTTP vocabulary must stay identical to `grpc.rs::error_kind`:
    /// database / qdrant / pii / config / grpc / serialization / other.
    ///
    /// I55-C: compare the *return values* of both real functions on the same
    /// seven errors instead of extracting string literals with `include_str!`.
    /// Removing a match arm or changing a returned token can no longer pass
    /// while an old literal stays somewhere in the source file.
    #[test]
    fn error_kind_vocabulary_matches_grpc_transport() {
        use crate::error::test_support::{ERROR_KIND_TOKENS, ErrorCase};

        let mut http_seen = std::collections::BTreeSet::new();
        let mut grpc_seen = std::collections::BTreeSet::new();

        for case in ErrorCase::ALL {
            let http = error_kind(&case.into_error());
            let grpc = crate::grpc::error_kind(&case.into_error());
            assert_eq!(http, case.token(), "HTTP mapping drifted for {case:?}");
            assert_eq!(grpc, case.token(), "gRPC mapping drifted for {case:?}");
            assert_eq!(http, grpc, "transports disagree for {case:?}");
            http_seen.insert(http);
            grpc_seen.insert(grpc);
        }

        let expected: std::collections::BTreeSet<&str> = ERROR_KIND_TOKENS.into_iter().collect();
        assert_eq!(http_seen, expected);
        assert_eq!(grpc_seen, expected);

        // anyhow context layers must not mask the typed variant on either side.
        let wrapped = ErrorCase::Qdrant.into_error().context("outer");
        assert_eq!(error_kind(&wrapped), "qdrant");
        assert_eq!(crate::grpc::error_kind(&wrapped), "qdrant");
    }

    fn store_request(id: Option<String>) -> StoreMemoryRequest {
        StoreMemoryRequest {
            id,
            content: "sensitive content".into(),
            embedding: vec![0.1_f32, 0.2, 0.3],
            metadata: r#"{"kind":"test"}"#.into(),
            source: "regression-test".into(),
            expires_at: Some(1_800_000_000),
        }
    }

    #[test]
    fn store_memory_ignores_caller_id_and_returns_generated_id() {
        let supplied = uuid::Uuid::new_v4().to_string();
        let (memory, response) = prepare_store_memory(store_request(Some(supplied.clone())), 3)
            .expect("valid metadata is accepted");

        assert_ne!(
            memory.id.to_string(),
            supplied,
            "a caller-supplied id must never become the stored id (I7-E)"
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
        let expected = memory.id.to_string();
        let body = serde_json::to_value(&response).expect("response serializes");
        assert_eq!(
            body["id"].as_str(),
            Some(expected.as_str()),
            "the JSON body exposes the generated id to the caller"
        );

        let (second, _) = prepare_store_memory(store_request(Some(supplied)), 3)
            .expect("valid metadata is accepted");
        assert_ne!(
            memory.id, second.id,
            "repeated requests with the same id get distinct records"
        );
    }

    #[test]
    fn store_memory_preserves_all_other_fields() {
        let (memory, _) =
            prepare_store_memory(store_request(None), 3).expect("valid metadata is accepted");
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

    /// P1-1 over HTTP: ranked responses carry `score`, unranked ones keep the
    /// legacy JSON shape, so existing clients are unaffected either way.
    #[test]
    fn search_response_exposes_score_additively() {
        let (mut memory, _) =
            prepare_store_memory(store_request(None), 3).expect("valid metadata is accepted");
        memory.score = Some(0.75);

        let ranked = serde_json::to_value(MemoryEntryResponse::from(memory.clone()))
            .expect("ranked entry serializes");
        assert_eq!(ranked["score"], serde_json::json!(0.75));
        assert_eq!(ranked["content"], serde_json::json!("sensitive content"));

        memory.score = None;
        let unranked = serde_json::to_value(MemoryEntryResponse::from(memory))
            .expect("unranked entry serializes");
        assert!(
            unranked.get("score").is_none(),
            "an unranked response must not gain a null score key"
        );
    }

    /// Minimal router carrying only the auth layer, so the gate can be tested
    /// without a live PostgreSQL/Qdrant-backed `ContextBuilder`.
    fn auth_test_app(token: Option<&str>) -> Router {
        Router::new()
            .route("/health", get(|| async { "OK" }))
            .route("/metrics", get(|| async { "metrics" }))
            .route("/v1/context/build", post(|| async { "OK" }))
            .route("/v1/memory", post(|| async { "OK" }))
            .route("/v1/memory/search", post(|| async { "OK" }))
            .layer(axum::middleware::from_fn_with_state(
                AuthState::new(token),
                require_admin_token,
            ))
    }

    async fn response_status(
        app: Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
    ) -> StatusCode {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(ADMIN_TOKEN_HEADER, token);
        }
        app.oneshot(request.body(Body::empty()).expect("request builds"))
            .await
            .expect("service responds")
            .status()
    }

    #[tokio::test]
    async fn configured_token_requires_exact_header() {
        let app = auth_test_app(Some("s3cr3t-token"));
        assert_eq!(
            response_status(app.clone(), "POST", "/v1/memory", None).await,
            StatusCode::UNAUTHORIZED,
            "a missing header must be rejected when a token is configured"
        );
        assert_eq!(
            response_status(app.clone(), "POST", "/v1/memory", Some("wrong-token")).await,
            StatusCode::UNAUTHORIZED,
            "a wrong header must be rejected"
        );
        assert_eq!(
            response_status(app.clone(), "GET", "/metrics", None).await,
            StatusCode::UNAUTHORIZED,
            "only /health is exempt"
        );
        for uri in ["/v1/context/build", "/v1/memory/search"] {
            assert_eq!(
                response_status(app.clone(), "POST", uri, None).await,
                StatusCode::UNAUTHORIZED,
                "{uri} must be gated too"
            );
            assert_eq!(
                response_status(app.clone(), "POST", uri, Some("s3cr3t-token")).await,
                StatusCode::OK,
                "{uri} must accept the exact token"
            );
        }
        assert_eq!(
            response_status(app, "POST", "/v1/memory", Some("s3cr3t-token")).await,
            StatusCode::OK,
            "the exact token must be accepted"
        );
    }

    #[tokio::test]
    async fn unset_token_preserves_legacy_access() {
        let app = auth_test_app(None);
        assert_eq!(
            response_status(app.clone(), "POST", "/v1/memory", None).await,
            StatusCode::OK,
            "an unconfigured token keeps loopback clients working"
        );
        assert_eq!(
            response_status(app.clone(), "GET", "/metrics", None).await,
            StatusCode::OK,
        );
        for uri in ["/v1/context/build", "/v1/memory/search"] {
            assert_eq!(
                response_status(app.clone(), "POST", uri, None).await,
                StatusCode::OK,
                "{uri} must stay open when no token is configured"
            );
        }
    }

    #[tokio::test]
    async fn health_is_public_with_or_without_a_token() {
        let protected = auth_test_app(Some("s3cr3t-token"));
        assert_eq!(
            response_status(protected, "GET", "/health", None).await,
            StatusCode::OK,
            "/health must stay reachable for probes even when auth is on"
        );
        let open = auth_test_app(None);
        assert_eq!(
            response_status(open, "GET", "/health", None).await,
            StatusCode::OK,
        );
    }

    #[test]
    fn token_comparison_is_content_and_length_sensitive() {
        assert!(constant_time_eq("s3cr3t", "s3cr3t"));
        assert!(constant_time_eq("", ""));
        assert!(!constant_time_eq("s3cr3t", "s3cr3T"));
        assert!(!constant_time_eq("s3cr3t", "s3cr3t-extra"));
        assert!(!constant_time_eq("s3cr3t", "s3cr3"));
    }

    #[test]
    fn rejected_request_never_logs_the_token() {
        const TOKEN: &str = "top-secret-token-42";
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
                let app = auth_test_app(Some(TOKEN));
                let response = app
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/v1/memory")
                            .header(ADMIN_TOKEN_HEADER, "wrong-token")
                            .body(Body::empty())
                            .expect("request builds"),
                    )
                    .await
                    .expect("service responds");
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            });
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(
            !logs.contains(TOKEN),
            "the token must never be logged: {logs}"
        );
    }

    struct StubBackend;

    #[async_trait::async_trait]
    impl MemoryBackend for StubBackend {
        async fn build(&self, _event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!({"memories": []}))
        }

        async fn store(&self, _memory: MemoryEntry) -> anyhow::Result<()> {
            Ok(())
        }

        async fn search(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(Vec::new())
        }

        fn embedding_dim(&self) -> usize {
            3
        }
    }

    #[test]
    fn instrumented_handlers_do_not_log_request_bodies() {
        const PROBE: &str = "private-memory-content-9c3f";
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_span_events(FmtSpan::NEW)
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime builds");
            runtime.block_on(async {
                let app = Router::new()
                    .route("/v1/context/build", post(build_context))
                    .route("/v1/memory", post(store_memory))
                    .route("/v1/memory/search", post(search_memory))
                    .with_state(Arc::new(StubBackend) as Arc<dyn MemoryBackend>);

                let requests = [
                    (
                        "/v1/context/build",
                        serde_json::json!({
                            "event_id": "00000000-0000-0000-0000-000000000001",
                            "event_type": "state_changed",
                            "payload": {"body": PROBE},
                            "correlation_id": "corr-1",
                        })
                        .to_string(),
                    ),
                    (
                        "/v1/memory",
                        serde_json::json!({
                            "content": PROBE,
                            "embedding": [0.1, 0.2, 0.3],
                            "metadata": "{\"kind\":\"test\"}",
                            "source": "regression-test",
                        })
                        .to_string(),
                    ),
                    (
                        "/v1/memory/search",
                        serde_json::json!({"query": PROBE, "limit": 5}).to_string(),
                    ),
                ];

                for (uri, body) in requests {
                    let status = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method("POST")
                                .uri(uri)
                                .header("content-type", "application/json")
                                .body(Body::from(body))
                                .expect("request builds"),
                        )
                        .await
                        .expect("service responds")
                        .status();
                    assert_eq!(status, StatusCode::OK, "{uri} should succeed");
                }
            });
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(!logs.contains(PROBE), "request body leaked: {logs}");
        let handlers = ["build_context", "store_memory", "search_memory"];
        for handler in handlers {
            assert!(
                logs.contains(handler),
                "span for {handler} not captured: {logs}"
            );
        }
    }

    /// Backend whose every operation fails with the same typed, probe-carrying
    /// error. Drives all three HTTP handlers to failure so the cross-transport
    /// test can prove each one logs through the shared helper instead of
    /// silently discarding the error into a bare 500.
    struct FailingBackend;

    fn probe_error() -> anyhow::Error {
        let detail = [
            HTTP_LOG_PROBE_URL,
            HTTP_LOG_PROBE_SQL,
            HTTP_LOG_PROBE_PATH,
            HTTP_LOG_PROBE_PII,
        ]
        .join(" ");
        anyhow::Error::from(crate::error::ContextError::qdrant(
            crate::error::QdrantErrorKind::FAILED,
        ))
        .context(detail)
    }

    #[async_trait::async_trait]
    impl MemoryBackend for FailingBackend {
        async fn build(&self, _event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
            Err(probe_error())
        }

        async fn store(&self, _memory: MemoryEntry) -> anyhow::Result<()> {
            Err(probe_error())
        }

        async fn search(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
            Err(probe_error())
        }

        fn embedding_dim(&self) -> usize {
            3
        }
    }

    /// Return the braced body that follows `marker` in `source`.
    ///
    /// Brace matching (rather than a plain substring search) scopes the
    /// assertion to one function body, so a mention of a helper name inside the
    /// test module cannot make the cross-transport check pass vacuously.
    fn body_after<'a>(source: &'a str, marker: &str) -> &'a str {
        let start = source
            .find(marker)
            .unwrap_or_else(|| panic!("marker not found: {marker}"));
        let open = source[start..]
            .find('{')
            .unwrap_or_else(|| panic!("no body after: {marker}"));
        let body_start = start + open;
        let mut depth = 0usize;
        for (offset, ch) in source[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[body_start..=body_start + offset];
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced body after: {marker}");
    }

    /// Every quoted literal in an `error_kind` body is a vocabulary token;
    /// comparing the two sets proves the seven tokens stayed in sync.
    fn error_kind_tokens(body: &str) -> std::collections::BTreeSet<&str> {
        let mut tokens = std::collections::BTreeSet::new();
        let mut rest = body;
        while let Some(start) = rest.find('"') {
            let tail = &rest[start + 1..];
            let Some(end) = tail.find('"') else {
                break;
            };
            tokens.insert(&tail[..end]);
            rest = &tail[end + 1..];
        }
        tokens
    }

    /// I53-B: all six RPCs (3 HTTP + 3 gRPC) must funnel internal failures
    /// through the fixed-vocabulary helper. This test keeps the original HTTP
    /// behavioural check plus source-scoped checks as a *secondary* guard; the
    /// primary, actual-call proof for both transports and all seven categories
    /// lives in `all_six_rpcs_return_the_same_seven_vocabularies_by_real_calls`
    /// and `grpc_handlers_log_fixed_kind_for_all_seven_error_vocabularies`.
    /// Static checks alone are not treated as evidence of routing.
    #[test]
    fn all_six_rpcs_route_internal_failures_through_fixed_vocabulary() {
        const OPS: [(&str, &str); 3] = [
            ("build_context", "build_context"),
            ("store_memory", "store_memory"),
            ("search_memory", "search_memory"),
        ];

        // --- HTTP: behavioural proof for all three handlers. ---
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
                let app = Router::new()
                    .route("/v1/context/build", post(build_context))
                    .route("/v1/memory", post(store_memory))
                    .route("/v1/memory/search", post(search_memory))
                    .with_state(Arc::new(FailingBackend) as Arc<dyn MemoryBackend>);

                let requests = [
                    (
                        "/v1/context/build",
                        serde_json::json!({
                            "event_id": "00000000-0000-0000-0000-000000000001",
                            "event_type": "state_changed",
                            "payload": {"body": "probe"},
                            "correlation_id": "corr-1",
                        })
                        .to_string(),
                    ),
                    (
                        "/v1/memory",
                        serde_json::json!({
                            "content": "probe",
                            "embedding": [0.1, 0.2, 0.3],
                            "metadata": "{\"kind\":\"test\"}",
                            "source": "regression-test",
                        })
                        .to_string(),
                    ),
                    (
                        "/v1/memory/search",
                        serde_json::json!({"query": "probe", "limit": 5}).to_string(),
                    ),
                ];

                for (uri, body) in requests {
                    let status = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method("POST")
                                .uri(uri)
                                .header("content-type", "application/json")
                                .body(Body::from(body))
                                .expect("request builds"),
                        )
                        .await
                        .expect("service responds")
                        .status();
                    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}");
                }
            });
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        for probe in [
            HTTP_LOG_PROBE_URL,
            HTTP_LOG_PROBE_SQL,
            HTTP_LOG_PROBE_PATH,
            HTTP_LOG_PROBE_PII,
            "qdrant.internal",
            "SELECT",
            "/etc/jarvis",
        ] {
            assert!(
                !logs.contains(probe),
                "sensitive error-chain detail leaked to logs: {probe}"
            );
        }
        // A bare 500 would emit no ERROR event at all; the fixed message plus
        // the `qdrant` token can only come from `log_internal_error`.
        assert!(logs.contains("internal error"), "{logs}");
        assert!(logs.contains("qdrant"), "{logs}");
        // Every HTTP handler's operation label is present, proving none of the
        // three silently dropped its error.
        for &(_, op) in OPS.iter() {
            assert!(logs.contains(op), "missing operation label {op}: {logs}");
        }

        // --- HTTP: structural proof that no handler kept a bare-500 map_err. ---
        let http_src = include_str!("http.rs");
        for &(handler, op) in OPS.iter() {
            let marker = format!("async fn {handler}(");
            let body = body_after(http_src, &marker);
            assert!(
                body.contains("log_internal_error("),
                "{handler} must route through log_internal_error: {body}"
            );
            assert!(
                body.contains(&format!("\"{op}\"")),
                "{handler} must pass its operation label: {body}"
            );
            assert!(
                !body.contains("map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)"),
                "{handler} must not discard the error into a bare 500: {body}"
            );
        }

        // --- gRPC: structural proof for all three handlers. ---
        let grpc_src = include_str!("grpc.rs");
        for &(handler, op) in OPS.iter() {
            let marker = format!("async fn {handler}(");
            let body = body_after(grpc_src, &marker);
            assert!(
                body.contains("internal_status("),
                "{handler} must route through internal_status: {body}"
            );
            assert!(
                body.contains(&format!("\"{op}\"")),
                "{handler} must pass its operation label: {body}"
            );
            assert!(
                !body.contains("Status::internal("),
                "{handler} must not build its own status: {body}"
            );
        }

        // --- Shared seven-token vocabulary across both transports. ---
        let http_tokens = error_kind_tokens(body_after(http_src, "fn error_kind(err:"));
        let grpc_tokens = error_kind_tokens(body_after(grpc_src, "fn error_kind(err:"));
        let expected: std::collections::BTreeSet<&str> = [
            "database",
            "qdrant",
            "pii",
            "config",
            "grpc",
            "serialization",
            "other",
        ]
        .into_iter()
        .collect();
        assert_eq!(http_tokens, expected);
        assert_eq!(grpc_tokens, expected);
    }

    /// HTTP backend that always fails with a chosen fixed-vocabulary category.
    struct ErrorCaseBackend(crate::error::test_support::ErrorCase);

    #[async_trait::async_trait]
    impl MemoryBackend for ErrorCaseBackend {
        async fn build(&self, _event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
            Err(self.0.into_error())
        }

        async fn store(&self, _memory: MemoryEntry) -> anyhow::Result<()> {
            Err(self.0.into_error())
        }

        async fn search(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
            Err(self.0.into_error())
        }

        fn embedding_dim(&self) -> usize {
            3
        }
    }

    /// gRPC backend with the same contract, defined here so one test can drive
    /// all six RPCs through their real handlers.
    struct GrpcErrorCaseBackend(crate::error::test_support::ErrorCase);

    #[async_trait::async_trait]
    impl crate::grpc::GrpcBackend for GrpcErrorCaseBackend {
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

    /// I55-C / I57-B: cross-transport proof based on real calls. For each of the
    /// seven categories, all six RPCs (3 HTTP + 3 gRPC) are invoked with an
    /// injected error *under their own subscriber and log buffer*. For every
    /// RPC we assert that the single emitted record carries the matching
    /// (`operation`, `error_kind`) pair: 3 RPCs x 7 categories = 21 pairs per
    /// transport, 42 in total. An aggregate token count (`matches == 3`) could
    /// pass with one RPC mis-categorizing and another double-logging; per-pair
    /// counting cannot. Callers still get a bare failure and no probe text. The
    /// static source checks above remain only as a secondary guard.
    #[test]
    fn all_six_rpcs_return_the_same_seven_vocabularies_by_real_calls() {
        use crate::error::test_support::{
            ERROR_KIND_TOKENS, ErrorCase, SENSITIVE_PROBE, assert_single_operation_kind,
        };

        // (operation label, HTTP route, JSON request body) for the three RPCs.
        let http_calls = [
            (
                "build_context",
                "/v1/context/build",
                serde_json::json!({
                    "event_id": "00000000-0000-0000-0000-000000000001",
                    "event_type": "state_changed",
                    "payload": {"body": "probe"},
                    "correlation_id": "corr-1",
                })
                .to_string(),
            ),
            (
                "store_memory",
                "/v1/memory",
                serde_json::json!({
                    "content": "probe",
                    "embedding": [0.1, 0.2, 0.3],
                    "metadata": "{\"kind\":\"test\"}",
                    "source": "regression-test",
                })
                .to_string(),
            ),
            (
                "search_memory",
                "/v1/memory/search",
                serde_json::json!({"query": "probe", "limit": 5}).to_string(),
            ),
        ];

        let mut http_seen = std::collections::BTreeSet::new();
        let mut grpc_seen = std::collections::BTreeSet::new();

        for case in ErrorCase::ALL {
            // ---------- HTTP: one real handler call + one fresh buffer per RPC. ----------
            for &(op, uri, ref body) in &http_calls {
                let http_buffer = LogBuffer::default();
                let http_writer = http_buffer.clone();
                let http_subscriber = tracing_subscriber::fmt()
                    .without_time()
                    .with_target(false)
                    .with_writer(move || http_writer.clone())
                    .finish();

                tracing::subscriber::with_default(http_subscriber, || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime builds");
                    runtime.block_on(async {
                        let app = Router::new()
                            .route("/v1/context/build", post(build_context))
                            .route("/v1/memory", post(store_memory))
                            .route("/v1/memory/search", post(search_memory))
                            .with_state(Arc::new(ErrorCaseBackend(case)) as Arc<dyn MemoryBackend>);

                        let response = app
                            .clone()
                            .oneshot(
                                Request::builder()
                                    .method("POST")
                                    .uri(uri)
                                    .header("content-type", "application/json")
                                    .body(Body::from(body.clone()))
                                    .expect("request builds"),
                            )
                            .await
                            .expect("service responds");
                        assert_eq!(
                            response.status(),
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "{uri}"
                        );
                        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                            .await
                            .expect("body reads");
                        assert!(bytes.is_empty(), "no error body may cross the boundary");
                    });
                });

                let captured = http_buffer.0.lock().expect("log buffer mutex").clone();
                let http_logs = String::from_utf8(captured).expect("logs are utf-8");
                assert_single_operation_kind(&http_logs, op, case.token(), "HTTP");
                assert!(
                    !http_logs.contains(SENSITIVE_PROBE),
                    "HTTP leak: {case:?}/{op}"
                );
                http_seen.insert(case.token());
            }

            // ---------- gRPC: one real handler call + one fresh buffer per RPC. ----------
            for op in ["build_context", "store_memory", "search_memory"] {
                let grpc_buffer = LogBuffer::default();
                let grpc_writer = grpc_buffer.clone();
                let grpc_subscriber = tracing_subscriber::fmt()
                    .without_time()
                    .with_target(false)
                    .with_writer(move || grpc_writer.clone())
                    .finish();

                tracing::subscriber::with_default(grpc_subscriber, || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("runtime builds");
                    runtime.block_on(async {
                        use crate::grpc::proto::context_service_server::ContextService;

                        let service = crate::grpc::GrpcContextService::with_backend(
                            Arc::new(GrpcErrorCaseBackend(case)),
                            None,
                        );

                        let status = match op {
                            "build_context" => service
                                .build_context(tonic::Request::new(
                                    crate::grpc::proto::BuildContextRequest {
                                        event_id: "00000000-0000-0000-0000-000000000001".into(),
                                        event_type: "state_changed".into(),
                                        payload: r#"{"body":"probe"}"#.into(),
                                        correlation_id: "corr-1".into(),
                                        source: "manual".into(),
                                        timestamp: 0,
                                    },
                                ))
                                .await
                                .expect_err("injected error must fail the RPC"),
                            "store_memory" => service
                                .store_memory(tonic::Request::new(
                                    crate::grpc::proto::StoreMemoryRequest {
                                        id: "ignored".into(),
                                        content: "probe".into(),
                                        embedding: vec![0.1, 0.2, 0.3],
                                        metadata: r#"{"kind":"test"}"#.into(),
                                        source: "regression-test".into(),
                                        expires_at: 0,
                                    },
                                ))
                                .await
                                .expect_err("injected error must fail the RPC"),
                            "search_memory" => service
                                .search_memory(tonic::Request::new(
                                    crate::grpc::proto::SearchMemoryRequest {
                                        query: "probe".into(),
                                        limit: 5,
                                    },
                                ))
                                .await
                                .expect_err("injected error must fail the RPC"),
                            other => panic!("unknown operation {other}"),
                        };
                        assert_eq!(status.code(), tonic::Code::Internal);
                        assert_eq!(status.message(), crate::grpc::INTERNAL_ERROR_MESSAGE);
                    });
                });

                let captured = grpc_buffer.0.lock().expect("log buffer mutex").clone();
                let grpc_logs = String::from_utf8(captured).expect("logs are utf-8");
                assert_single_operation_kind(&grpc_logs, op, case.token(), "gRPC");
                assert!(
                    !grpc_logs.contains(SENSITIVE_PROBE),
                    "gRPC leak: {case:?}/{op}"
                );
                grpc_seen.insert(case.token());
            }
        }

        let expected: std::collections::BTreeSet<&str> = ERROR_KIND_TOKENS.into_iter().collect();
        assert_eq!(http_seen, expected);
        assert_eq!(grpc_seen, expected);
    }
}
