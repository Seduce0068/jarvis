//! HTTP/REST for Context Builder

use crate::ContextBuilder;
use axum::{
    Json, Router,
    extract::State,
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

#[derive(Debug, Deserialize)]
struct SearchMemoryRequest {
    query: String,
    limit: usize,
}

pub fn router(builder: Arc<ContextBuilder>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/context/build", post(build_context))
        .route("/v1/memory", post(store_memory))
        .route("/v1/memory/search", post(search_memory))
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
    use super::{StoreMemoryRequest, log_internal_error, prepare_store_memory};
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
}
