//! Context building core

use anyhow::Result;
use chrono::Utc;
use jarvis_core::{MemoryEntry, UnifiedEvent};
use qdrant_client::Qdrant;
use serde_json::Value;
use std::sync::Arc;
use tracing::{instrument, warn};
use uuid::Uuid;

/// Reassemble PostgreSQL rows in the relevance order supplied by Qdrant.
/// `WHERE id = ANY($1)` does not preserve the input order, so without this
/// the vector-search ranking is silently discarded (P1-1).
fn reorder_by_ids(entries: Vec<MemoryEntry>, ids: &[Uuid]) -> Vec<MemoryEntry> {
    let mut by_id: std::collections::HashMap<Uuid, MemoryEntry> =
        entries.into_iter().map(|entry| (entry.id, entry)).collect();
    ids.iter().filter_map(|id| by_id.remove(id)).collect()
}

pub struct Builder {
    config: crate::config::ContextConfig,
    pg_pool: Arc<deadpool_postgres::Pool>,
    qdrant_client: Arc<Qdrant>,
    pii_filter: Arc<crate::pii::PiiFilter>,
    embedding: crate::embedding::EmbeddingClient,
}

impl Builder {
    pub async fn new(
        config: crate::config::ContextConfig,
        pg_pool: Arc<deadpool_postgres::Pool>,
        qdrant_client: Arc<Qdrant>,
        pii_filter: Arc<crate::pii::PiiFilter>,
    ) -> Result<Self> {
        // Ensure collection exists
        memory::ensure_collection(&qdrant_client, &config.collection_name).await?;

        let embedding = crate::embedding::EmbeddingClient::from_config(&config)?;
        if !embedding.is_enabled() {
            warn!(
                "Embedding endpoint not configured; memory vector search is disabled \
                 (set JARVIS_CONTEXT_EMBEDDING_URL)"
            );
        }

        Ok(Self {
            config,
            pg_pool,
            qdrant_client,
            pii_filter,
            embedding,
        })
    }

    #[instrument(skip(self, event))]
    pub async fn build_context(&self, event: &UnifiedEvent) -> Result<Value> {
        // 1. Search relevant memories
        let query = format!("{} {}", event.event_type, event.payload);
        let memories = self.search_memory(&query, 10).await?;

        // 2. Get recent conversation history
        let recent = self.get_recent_memories(20).await?;

        // 3. Build context object
        let mut context = serde_json::json!({
            "event": {
                "id": event.id,
                "source": event.source,
                "type": event.event_type,
                "payload": event.payload,
                "timestamp": event.timestamp,
                "correlation_id": event.correlation_id,
            },
            "memories": memories,
            "recent": recent,
            "timestamp": Utc::now(),
        });

        // 4. Apply PII filtering
        context = self.pii_filter.filter(context)?;

        // 5. Truncate to token limit
        context = self.truncate_context(context)?;

        Ok(context)
    }

    pub async fn store_memory(&self, memory: MemoryEntry) -> Result<()> {
        // Store in PostgreSQL
        let client = self.pg_pool.get().await?;
        client.execute(
            "INSERT INTO memories (id, content, metadata, source, created_at, updated_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO UPDATE SET content = $2, metadata = $3, updated_at = $6",
            &[
                &memory.id,
                &memory.content,
                &memory.metadata,
                &memory.source,
                &memory.created_at,
                &memory.updated_at,
                &memory.expires_at,
            ],
        ).await?;

        // Store embedding in Qdrant
        memory::upsert_vector(&self.qdrant_client, &self.config.collection_name, &memory).await?;

        Ok(())
    }

    pub async fn search_memory(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        // Generate an embedding for the query; without a configured embedding
        // endpoint the vector search is skipped (recent memories still work).
        let embedding = match self.embedding.embed(query).await {
            Ok(embedding) => embedding,
            Err(error) => {
                warn!("memory vector search skipped: {error:#}");
                return Ok(Vec::new());
            }
        };

        // Search Qdrant
        let results = memory::search_vectors(
            &self.qdrant_client,
            &self.config.collection_name,
            &embedding,
            limit,
        )
        .await?;

        // Fetch full entries from PostgreSQL, then reassemble in Qdrant's
        // relevance order (P1-1).
        let ids: Vec<Uuid> = results.iter().map(|r| r.id).collect();
        let client = self.pg_pool.get().await?;
        let rows = client.query(
            "SELECT id, content, metadata, source, created_at, updated_at, expires_at FROM memories WHERE id = ANY($1)",
            &[&ids],
        ).await?;

        let fetched: Vec<MemoryEntry> = rows
            .into_iter()
            .map(|row| MemoryEntry {
                id: row.get(0),
                content: row.get(1),
                embedding: vec![],
                metadata: row.get(2),
                source: row.get(3),
                created_at: row.get(4),
                updated_at: row.get(5),
                expires_at: row.get(6),
            })
            .collect();
        Ok(reorder_by_ids(fetched, &ids))
    }

    async fn get_recent_memories(&self, limit: usize) -> Result<Vec<MemoryEntry>> {
        let client = self.pg_pool.get().await?;
        let rows = client
            .query(
                "SELECT id, content, metadata, source, created_at, updated_at, expires_at
             FROM memories ORDER BY created_at DESC LIMIT $1",
                &[&(limit as i64)],
            )
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| MemoryEntry {
                id: row.get(0),
                content: row.get(1),
                embedding: vec![],
                metadata: row.get(2),
                source: row.get(3),
                created_at: row.get(4),
                updated_at: row.get(5),
                expires_at: row.get(6),
            })
            .collect())
    }

    fn truncate_context(&self, context: Value) -> Result<Value> {
        truncate_context_to_tokens(context, self.config.max_context_tokens)
    }
}

/// Drop the memory arrays when the serialized context exceeds the configured
/// budget (roughly 4 characters per token).
fn truncate_context_to_tokens(mut context: Value, max_tokens: usize) -> Result<Value> {
    let serialized = serde_json::to_string(&context)?;
    if serialized.len() > max_tokens * 4 {
        context["memories"] = Value::Array(vec![]);
        context["recent"] = Value::Array(vec![]);
    }
    Ok(context)
}

pub mod memory {
    use super::*;
    use qdrant_client::qdrant::{
        CreateCollectionBuilder, Distance, PointStruct, SearchPointsBuilder, UpsertPointsBuilder,
        VectorParamsBuilder,
    };
    use std::collections::HashMap;

    pub async fn create_pg_pool(
        config: &crate::config::ContextConfig,
    ) -> Result<deadpool_postgres::Pool> {
        let mut cfg = deadpool_postgres::Config::new();
        cfg.url = Some(config.pg_url().into());
        let pool = cfg.create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )?;

        // Run migrations
        let client = pool.get().await?;
        client
            .batch_execute(include_str!("../../migrations/001_initial.sql"))
            .await?;

        Ok(pool)
    }

    pub async fn create_qdrant_client(config: &crate::config::ContextConfig) -> Result<Qdrant> {
        // The gRPC API is backward compatible, and the released server may be
        // older than the client; skip the strict minor-version gate.
        let mut client = Qdrant::from_url(&config.qdrant_url).skip_compatibility_check();
        if let Some(key) = config.qdrant_api_key() {
            client = client.api_key(key);
        }
        Ok(client
            .build()
            .map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?)
    }

    pub async fn ensure_collection(client: &Qdrant, name: &str) -> Result<()> {
        let collections = client
            .list_collections()
            .await
            .map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;
        if !collections.collections.iter().any(|c| c.name == name) {
            client
                .create_collection(
                    CreateCollectionBuilder::new(name)
                        .vectors_config(VectorParamsBuilder::new(384, Distance::Cosine)),
                )
                .await
                .map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn upsert_vector(
        client: &Qdrant,
        collection: &str,
        memory: &MemoryEntry,
    ) -> Result<()> {
        let mut payload_map: HashMap<String, qdrant_client::qdrant::Value> = HashMap::new();
        if let Value::Object(obj) = &memory.metadata {
            for (k, v) in obj {
                payload_map.insert(k.clone(), v.clone().into());
            }
        }
        let point = PointStruct::new(memory.id.to_string(), memory.embedding.clone(), payload_map);
        client
            .upsert_points(UpsertPointsBuilder::new(collection, vec![point]))
            .await
            .map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;
        Ok(())
    }

    pub async fn search_vectors(
        client: &Qdrant,
        collection: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let response = client
            .search_points(
                SearchPointsBuilder::new(collection, embedding.to_vec(), limit as u64)
                    .with_payload(true),
            )
            .await
            .map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;

        Ok(response
            .result
            .into_iter()
            .map(|scored| {
                let id_str = match &scored.id {
                    Some(id) => match &id.point_id_options {
                        Some(qdrant_client::qdrant::point_id::PointIdOptions::Uuid(u)) => u.clone(),
                        Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(n)) => {
                            n.to_string()
                        }
                        None => Uuid::new_v4().to_string(),
                    },
                    None => Uuid::new_v4().to_string(),
                };
                MemoryEntry {
                    id: Uuid::parse_str(&id_str).unwrap_or_default(),
                    content: String::new(), // Will be fetched from PG
                    embedding: vec![],
                    metadata: serde_json::to_value(scored.payload).unwrap_or_default(),
                    source: String::new(),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                    expires_at: None,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_within_budget_is_unchanged() {
        let context = json!({"memories": [1, 2], "recent": [3]});
        let output = truncate_context_to_tokens(context.clone(), 1000).expect("truncate");
        assert_eq!(output, context);
    }

    #[test]
    fn oversized_context_drops_memory_arrays_but_keeps_event_data() {
        let context = json!({
            "memories": [{"content": "x".repeat(100)}],
            "recent": [{"content": "y".repeat(100)}],
            "event": "state_changed",
        });
        let output = truncate_context_to_tokens(context, 1).expect("truncate");
        assert_eq!(output["memories"], json!([]));
        assert_eq!(output["recent"], json!([]));
        assert_eq!(output["event"], "state_changed");
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::reorder_by_ids;
    use jarvis_core::MemoryEntry;
    use serde_json::json;
    use uuid::Uuid;

    fn entry(id: Uuid) -> MemoryEntry {
        MemoryEntry {
            id,
            content: String::new(),
            embedding: vec![],
            metadata: json!({}),
            source: String::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            expires_at: None,
        }
    }

    #[test]
    fn reorder_preserves_qdrant_relevance_order() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let fetched = vec![entry(c), entry(a), entry(b)];
        assert_eq!(
            reorder_by_ids(fetched, &[b, a, c])
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![b, a, c]
        );
    }

    #[test]
    fn reorder_skips_missing_and_extra_rows() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let reordered = reorder_by_ids(vec![entry(a), entry(Uuid::new_v4())], &[b, a]);
        assert_eq!(reordered.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a]);
    }
}
