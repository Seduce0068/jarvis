//! HTTP/REST for Context Builder

use crate::ContextBuilder;
use axum::{
    Json, Router,
    extract::{Request, State},
    middleware::Next,
    response::Response,
    routing::{get, post},
};
use jarvis_core::{MemoryEntry, UnifiedEvent};
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

/// Header carrying the shared admin token on the context HTTP API.
const ADMIN_TOKEN_HEADER: &str = "x-jarvis-context-token";

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
    request: Request,
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
    limit: usize,
}

pub fn router(builder: Arc<ContextBuilder>) -> Router {
    // `ContextBuilder.config` is visible here because `http` is a child module
    // of the crate root where that field is declared.
    let auth = AuthState::new(builder.config.admin_token());
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
        .with_state(builder)
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

/// Preserve an internal failure's reason in the server log while keeping the
/// client contract unchanged (bare 500, no error body).
///
/// `{err:#}` is anyhow's full kind/context chain. The request itself is never
/// passed here, so payloads, correlation IDs and other raw data cannot leak
/// into this log record.
fn log_internal_error<T>(
    result: anyhow::Result<T>,
    op: &'static str,
) -> Result<T, axum::http::StatusCode> {
    result.map_err(|err| {
        let chain = format!("{err:#}");
        tracing::error!(operation = op, error = %chain, "internal error");
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    })
}

#[instrument(skip(builder, req))]
async fn build_context(
    State(builder): State<Arc<ContextBuilder>>,
    Json(req): Json<BuildContextRequest>,
) -> Result<Json<BuildContextResponse>, axum::http::StatusCode> {
    let event = UnifiedEvent {
        id: uuid::Uuid::parse_str(&req.event_id).unwrap_or_else(|_| uuid::Uuid::new_v4()),
        source: jarvis_core::EventSource::Manual,
        event_type: req.event_type,
        payload: req.payload,
        timestamp: chrono::Utc::now(),
        correlation_id: req.correlation_id,
    };
    let context = log_internal_error(builder.build_context(&event).await, "build_context")?;
    Ok(Json(BuildContextResponse { context }))
}

/// Build the entry to persist together with the response describing it.
///
/// `builder::store_memory` performs `ON CONFLICT (id) DO UPDATE`, so honoring a
/// client-supplied `id` would let any caller overwrite an existing memory
/// (I7-E). Always mint a fresh server-side UUID, ignore `req.id`, and hand the
/// generated id back so the caller can reference the new record. Every other
/// field keeps its previous handling.
fn prepare_store_memory(req: StoreMemoryRequest) -> (MemoryEntry, StoreMemoryResponse) {
    // I7-E: consumed and discarded so the ignore is explicit in code, not just
    // in the docs above.
    let _ignored_client_id = req.id;
    let id = uuid::Uuid::new_v4();
    let response = StoreMemoryResponse { id: id.to_string() };
    let memory = MemoryEntry {
        id,
        content: req.content,
        embedding: req.embedding,
        metadata: serde_json::from_str(&req.metadata).unwrap_or_default(),
        source: req.source,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        expires_at: req
            .expires_at
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
    };
    (memory, response)
}

#[instrument(skip(builder))]
async fn store_memory(
    State(builder): State<Arc<ContextBuilder>>,
    Json(req): Json<StoreMemoryRequest>,
) -> Result<Json<StoreMemoryResponse>, axum::http::StatusCode> {
    let (memory, response) = prepare_store_memory(req);
    builder
        .store_memory(memory)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(response))
}

#[instrument(skip(builder))]
async fn search_memory(
    State(builder): State<Arc<ContextBuilder>>,
    Json(req): Json<SearchMemoryRequest>,
) -> Result<Json<Vec<MemoryEntry>>, axum::http::StatusCode> {
    let memories = builder
        .search_memory(&req.query, req.limit)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(memories))
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
        ADMIN_TOKEN_HEADER, AuthState, StoreMemoryRequest, constant_time_eq, log_internal_error,
        prepare_store_memory, require_admin_token,
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
    fn internal_error_is_logged_with_chain_and_mapped_to_bare_500() {
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();

        let outcome = tracing::subscriber::with_default(subscriber, || {
            let err = anyhow::anyhow!("qdrant request timed out")
                .context("producing query embedding for memory vector search");
            log_internal_error::<()>(Err(err), "build_context")
        });

        assert_eq!(
            outcome.unwrap_err(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(logs.contains("ERROR"), "{logs}");
        assert!(logs.contains("build_context"), "{logs}");
        assert!(
            logs.contains("producing query embedding for memory vector search"),
            "{logs}"
        );
        assert!(logs.contains("qdrant request timed out"), "{logs}");
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
        let (memory, response) = prepare_store_memory(store_request(Some(supplied.clone())));

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

        let (second, _) = prepare_store_memory(store_request(Some(supplied)));
        assert_ne!(
            memory.id, second.id,
            "repeated requests with the same id get distinct records"
        );
    }

    #[test]
    fn store_memory_preserves_all_other_fields() {
        let (memory, _) = prepare_store_memory(store_request(None));
        assert_eq!(memory.content, "sensitive content");
        assert_eq!(memory.embedding, vec![0.1_f32, 0.2, 0.3]);
        assert_eq!(memory.metadata, serde_json::json!({ "kind": "test" }));
        assert_eq!(memory.source, "regression-test");
        assert_eq!(
            memory.expires_at,
            chrono::DateTime::from_timestamp(1_800_000_000, 0)
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
}
