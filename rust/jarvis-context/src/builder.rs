//! Context building core

use anyhow::Result;
use serde_json::Value;
use chrono::{DateTime, Utc};
use tracing::instrument;
use std::sync::Arc;
use jarvis_core::{MemoryEntry, UnifiedEvent};
use qdrant_client::Qdrant;
use uuid::Uuid;

pub struct Builder {
    config: crate::config::ContextConfig,
    pg_pool: Arc<deadpool_postgres::Pool>,
    qdrant_client: Arc<Qdrant>,
    pii_filter: Arc<crate::pii::PiiFilter>,
    policy_client: Arc<jarvis_policy::PolicyEngine>,
}

impl Builder {
    pub async fn new(
        config: crate::config::ContextConfig,
        pg_pool: Arc<deadpool_postgres::Pool>,
        qdrant_client: Arc<Qdrant>,
        pii_filter: Arc<crate::pii::PiiFilter>,
        policy_client: Arc<jarvis_policy::PolicyEngine>,
    ) -> Result<Self> {
        // Ensure collection exists
        memory::ensure_collection(&qdrant_client, &config.collection_name).await?;

        Ok(Self {
            config,
            pg_pool,
            qdrant_client,
            pii_filter,
            policy_client,
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
            &[&memory.id, &memory.content, &memory.metadata, &memory.source, 
              &memory.created_at, &memory.updated_at, &memory.expires_at],
        ).await?;

        // Store embedding in Qdrant
        memory::upsert_vector(&self.qdrant_client, &self.config.collection_name, &memory).await?;

        Ok(())
    }

    pub async fn search_memory(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        // Generate embedding for query (placeholder - would use actual embedding model)
        let embedding = vec![0.0; 384]; // all-MiniLM-L6-v2 dimension

        // Search Qdrant
        let results = memory::search_vectors(&self.qdrant_client, &self.config.collection_name, &embedding, limit).await?;

        // Fetch full entries from PostgreSQL
        let ids: Vec<Uuid> = results.iter().map(|r| r.id).collect();
        let client = self.pg_pool.get().await?;
        let rows = client.query(
            "SELECT id, content, metadata, source, created_at, updated_at, expires_at FROM memories WHERE id = ANY($1)",
            &[&ids],
        ).await?;

        Ok(rows.into_iter().map(|row| MemoryEntry {
            id: row.get(0),
            content: row.get(1),
            embedding: vec![],
            metadata: row.get(2),
            source: row.get(3),
            created_at: row.get(4),
            updated_at: row.get(5),
            expires_at: row.get(6),
        }).collect())
    }

    async fn get_recent_memories(&self, limit: usize) -> Result<Vec<MemoryEntry>> {
        let client = self.pg_pool.get().await?;
        let rows = client.query(
            "SELECT id, content, metadata, source, created_at, updated_at, expires_at 
             FROM memories ORDER BY created_at DESC LIMIT $1",
            &[&(limit as i64)],
        ).await?;

        Ok(rows.into_iter().map(|row| MemoryEntry {
            id: row.get(0),
            content: row.get(1),
            embedding: vec![],
            metadata: row.get(2),
            source: row.get(3),
            created_at: row.get(4),
            updated_at: row.get(5),
            expires_at: row.get(6),
        }).collect())
    }

    fn truncate_context(&self, mut context: Value) -> Result<Value> {
        // Simple truncation - in production would count tokens properly
        let serialized = serde_json::to_string(&context)?;
        if serialized.len() > self.config.max_context_tokens * 4 { // rough char/token ratio
            context["memories"] = Value::Array(vec![]);
            context["recent"] = Value::Array(vec![]);
        }
        Ok(context)
    }
}

pub mod memory {
    use super::*;
    use qdrant_client::qdrant::{PointStruct, VectorParams, Distance, CreateCollection, UpsertPoints, SearchPoints};
    use qdrant_client::Payload;

    pub async fn create_pg_pool(config: &crate::config::ContextConfig) -> Result<deadpool_postgres::Pool> {
        let cfg = deadpool_postgres::Config::from_url(&config.pg_url.expose_secret());
        let pool = cfg.create_pool(Some(deadpool_postgres::Runtime::Tokio1), tokio_postgres::NoTls)?;
        
        // Run migrations
        let client = pool.get().await?;
        client.batch_execute(include_str!("../../migrations/001_initial.sql")).await?;
        
        Ok(pool)
    }

    pub async fn create_qdrant_client(config: &crate::config::ContextConfig) -> Result<Qdrant> {
        let mut client = Qdrant::from_url(&config.qdrant_url);
        if let Some(key) = &config.qdrant_api_key {
            client = client.api_key(key.expose_secret());
        }
        Ok(client.build().map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?)
    }

    pub async fn ensure_collection(client: &Qdrant, name: &str) -> Result<()> {
        let collections = client.list_collections().await.map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;
        if !collections.collections.iter().any(|c| c.name == name) {
            client.create_collection(&CreateCollection {
                collection_name: name.into(),
                vectors_config: Some(VectorParams {
                    size: 384,
                    distance: Distance::Cosine.into(),
                    ..Default::default()
                }.into()),
                ..Default::default()
            }).await.map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn upsert_vector(client: &Qdrant, collection: &str, memory: &MemoryEntry) -> Result<()> {
        let point = PointStruct::new(
            memory.id.to_string(),
            memory.embedding.clone(),
            Payload::try_from(memory.metadata.clone())?,
        );
        client.upsert_points(&UpsertPoints {
            collection_name: collection.into(),
            points: vec![point],
            ..Default::default()
        }).await.map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;
        Ok(())
    }

    pub async fn search_vectors(
        client: &Qdrant,
        collection: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let response = client.search_points(&SearchPoints {
            collection_name: collection.into(),
            vector: embedding.to_vec(),
            limit: limit as u64,
            with_payload: Some(true.into()),
            ..Default::default()
        }).await.map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;

        Ok(response.result.into_iter().map(|scored| MemoryEntry {
            id: Uuid::parse_str(&scored.id.unwrap().uuid).unwrap(),
            content: String::new(), // Will be fetched from PG
            embedding: vec![],
            metadata: scored.payload.into(),
            source: String::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            expires_at: None,
        }).collect())
    }
}