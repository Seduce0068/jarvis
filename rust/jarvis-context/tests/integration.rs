//! PostgreSQL + Qdrant integration test.
//!
//! Requires Docker, so it is ignored by default. CI runs it explicitly with
//! `cargo test -p jarvis-context -- --ignored`.

use chrono::Utc;
use jarvis_context::ContextBuilder;
use jarvis_context::config::ContextConfig;
use jarvis_core::{EventSource, MemoryEntry, UnifiedEvent};
use serde_json::json;
use testcontainers::GenericImage;
use testcontainers::ImageExt;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use uuid::Uuid;

const POSTGRES_PORT: u16 = 5432;
const QDRANT_GRPC_PORT: u16 = 6334;
/// Must match the vector size in `builder::memory::ensure_collection`.
const EMBEDDING_DIM: usize = 384;

#[tokio::test]
#[ignore = "requires Docker"]
async fn postgres_and_qdrant_round_trip() {
    let postgres = GenericImage::new("postgres", "16-alpine")
        .with_exposed_port(POSTGRES_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_USER", "jarvis")
        .with_env_var("POSTGRES_PASSWORD", "jarvis")
        .with_env_var("POSTGRES_DB", "jarvis")
        .start()
        .await
        .expect("postgres starts");

    let qdrant = GenericImage::new("qdrant/qdrant", "v1.12.4")
        .with_exposed_port(QDRANT_GRPC_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Qdrant HTTP listening on 6333"))
        .start()
        .await
        .expect("qdrant starts");

    let pg_port = postgres
        .get_host_port_ipv4(POSTGRES_PORT)
        .await
        .expect("postgres port");
    let qdrant_port = qdrant
        .get_host_port_ipv4(QDRANT_GRPC_PORT)
        .await
        .expect("qdrant gRPC port");

    let config = ContextConfig {
        pg_url: format!("postgres://jarvis:jarvis@127.0.0.1:{pg_port}/jarvis"),
        qdrant_url: format!("http://127.0.0.1:{qdrant_port}"),
        collection_name: "jarvis_memories_integration".into(),
        embedding_url: None,
        pii_patterns_path: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../infra/config/pii_patterns.toml"
        )
        .into(),
        ..Default::default()
    };
    let builder = ContextBuilder::new(config)
        .await
        .expect("context builder starts");

    let memory = MemoryEntry {
        id: Uuid::new_v4(),
        content: "the integration test remembered this".into(),
        embedding: vec![0.1; EMBEDDING_DIM],
        metadata: json!({ "kind": "integration" }),
        source: "integration".into(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        expires_at: None,
    };
    builder
        .store_memory(memory.clone())
        .await
        .expect("memory is stored in PostgreSQL and Qdrant");

    // Without an embedding endpoint, vector search is skipped.
    let empty = builder
        .search_memory("integration", 5)
        .await
        .expect("memory search runs");
    assert!(
        empty.is_empty(),
        "vector search is disabled without an embedding endpoint: {empty:?}"
    );

    let event = UnifiedEvent {
        id: Uuid::new_v4(),
        source: EventSource::Manual,
        event_type: "integration".into(),
        payload: json!({ "content": "the integration test remembered this" }),
        timestamp: Utc::now(),
        correlation_id: "integration-test".into(),
    };
    let context = builder.build_context(&event).await.expect("context builds");
    let recent = context["recent"]
        .as_array()
        .expect("context carries the recent memory section");
    // The PII filter redacts UUIDs, so compare on the preserved content.
    assert!(
        recent
            .iter()
            .any(|entry| entry["content"] == memory.content),
        "stored memory is returned by the PostgreSQL path: {context}"
    );
}
