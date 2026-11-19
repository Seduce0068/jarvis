//! Context building core

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
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

/// A memory is expired once `expires_at <= now` (UTC). The boundary is
/// inclusive: an entry expiring exactly at `now` is already expired. `None`
/// means the entry never expires.
fn is_expired(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    expires_at.is_some_and(|expires_at| expires_at <= now)
}

/// Keep only memories still valid at `now`, preserving the input order.
fn retain_unexpired(entries: Vec<MemoryEntry>, now: DateTime<Utc>) -> Vec<MemoryEntry> {
    entries
        .into_iter()
        .filter(|entry| !is_expired(entry.expires_at, now))
        .collect()
}

/// Classify the outcome of producing a query embedding.
///
/// `None` means the endpoint is intentionally unconfigured: vector search is
/// skipped by design and PostgreSQL-backed recent memories still work.
/// `Some(Err(..))` is a genuine `embed()` failure and is propagated with
/// context, so no caller can mistake a failed embedding for a search that
/// simply found no results.
fn query_embedding_outcome(
    embed_result: Option<anyhow::Result<Vec<f32>>>,
) -> anyhow::Result<Option<Vec<f32>>> {
    match embed_result {
        None => {
            warn!("memory vector search skipped: embedding endpoint not configured");
            Ok(None)
        }
        Some(result) => result
            .context("producing query embedding for memory vector search")
            .map(Some),
    }
}

/// Payload fields of a Qdrant scored point, keyed by payload name.
type QdrantPayload = std::collections::HashMap<String, qdrant_client::qdrant::Value>;

/// Map a raw Qdrant point id string to a `Uuid`.
///
/// Returns `None` for a missing or malformed id instead of fabricating a
/// `Uuid::nil()` (or a random v4). A fabricated id would silently leak into
/// the caller's search results as if Qdrant had returned a real memory.
fn parse_point_id(id_str: Option<&str>) -> Option<Uuid> {
    Uuid::parse_str(id_str?).ok()
}

/// Convert Qdrant scored-point fields into memory entries, skipping every
/// point whose id is missing or not a valid UUID.
///
/// This is the single place a Qdrant result becomes a `MemoryEntry`, so the
/// "no invalid ids" guarantee is testable without a live Qdrant connection.
/// Skipped points are reported with a warning rather than injected as a nil
/// UUID; a nil id must never reach `reorder_by_ids` or the SQL `ANY($1)` list.
fn entries_from_raw_points(points: Vec<(Option<String>, QdrantPayload)>) -> Vec<MemoryEntry> {
    let now = Utc::now();
    points
        .into_iter()
        .filter_map(|(id_str, payload)| {
            let id = match parse_point_id(id_str.as_deref()) {
                Some(id) => id,
                None => {
                    warn!("skipping Qdrant result with missing or invalid point id: {id_str:?}");
                    return None;
                }
            };
            Some(MemoryEntry {
                id,
                content: String::new(), // Fetched from PostgreSQL afterwards.
                embedding: vec![],
                metadata: serde_json::to_value(payload).unwrap_or_default(),
                source: String::new(),
                created_at: now,
                updated_at: now,
                expires_at: None,
            })
        })
        .collect()
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
        // Ensure the collection exists with the configured vector size; a
        // model change alone must not silently leave a 384-dim collection.
        memory::ensure_collection(
            &qdrant_client,
            &config.collection_name,
            config.embedding_dim,
        )
        .await?;

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
        // One UTC snapshot keeps the expiry boundary and the context timestamp
        // consistent across both memory paths.
        let now = Utc::now();

        // 1. Search relevant memories
        let query = format!("{} {}", event.event_type, event.payload);
        let memories = self.search_memory_at(&query, 10, now).await?;

        // 2. Get recent conversation history
        let recent = self.get_recent_memories_at(20, now).await?;

        // 3. Build context object
        let context = serde_json::json!({
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
            "timestamp": now,
        });

        // 4/5. Redact PII, then truncate to the token budget.
        prepare_context(
            self.pii_filter.as_ref(),
            context,
            self.config.max_context_tokens,
        )
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
        self.search_memory_at(query, limit, Utc::now()).await
    }

    /// Same as [`Builder::search_memory`] with an injectable UTC `now`, so the
    /// expiry boundary is deterministic in tests.
    async fn search_memory_at(
        &self,
        query: &str,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<MemoryEntry>> {
        // Only an unconfigured endpoint is an intentional skip. A real
        // embed() failure is propagated, never disguised as an empty result.
        let embed_result = if self.embedding.is_enabled() {
            Some(self.embedding.embed(query).await)
        } else {
            None
        };
        let embedding = match query_embedding_outcome(embed_result)? {
            Some(embedding) => embedding,
            None => return Ok(Vec::new()),
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
        // relevance order (P1-1). Expired rows are excluded by SQL and filtered
        // again in Rust so the same UTC `<=` boundary holds on both sides.
        let ids: Vec<Uuid> = results.iter().map(|r| r.id).collect();
        let client = self.pg_pool.get().await?;
        let rows = client.query(
            "SELECT id, content, metadata, source, created_at, updated_at, expires_at FROM memories WHERE id = ANY($1) AND (expires_at IS NULL OR expires_at > $2)",
            &[&ids, &now],
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
        Ok(reorder_by_ids(retain_unexpired(fetched, now), &ids))
    }

    /// Recent memories with an injectable UTC `now`; expired entries
    /// (`expires_at <= now`) never reach the assembled context.
    async fn get_recent_memories_at(
        &self,
        limit: usize,
        now: DateTime<Utc>,
    ) -> Result<Vec<MemoryEntry>> {
        let client = self.pg_pool.get().await?;
        let rows = client
            .query(
                "SELECT id, content, metadata, source, created_at, updated_at, expires_at
             FROM memories
             WHERE expires_at IS NULL OR expires_at > $1
             ORDER BY created_at DESC LIMIT $2",
                &[&now, &(limit as i64)],
            )
            .await?;

        let entries: Vec<MemoryEntry> = rows
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
        Ok(retain_unexpired(entries, now))
    }
}

/// Pure composition of the two post-assembly steps in `build_context`:
/// redact PII first, then apply the token budget. Kept separate from DB and
/// Qdrant access so the ordering and its effect are directly testable.
fn prepare_context(
    pii_filter: &crate::pii::PiiFilter,
    context: Value,
    max_tokens: usize,
) -> Result<Value> {
    let context = pii_filter.filter(context)?;
    truncate_context_to_tokens(context, max_tokens)
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

    /// Reject a nonsensical configured dimension before touching Qdrant.
    /// `0` can never be a valid vector size and usually means an invalid
    /// `JARVIS_CONTEXT_EMBEDDING_DIM` value.
    pub(super) fn validate_embedding_dim(name: &str, embedding_dim: usize) -> Result<()> {
        if embedding_dim == 0 {
            anyhow::bail!(
                "JARVIS_CONTEXT_EMBEDDING_DIM must be greater than zero \
                 (got {embedding_dim}); cannot use it for collection '{name}'"
            );
        }
        Ok(())
    }

    /// Compare an existing collection's real vector size with the configured
    /// value. Qdrant cannot resize an existing collection, so a mismatch fails
    /// fast with an actionable message instead of surfacing later as an opaque
    /// upsert/search error. An unreadable dimension only warns: keep starting
    /// rather than blocking deployments against older servers.
    pub(super) fn check_existing_dimension(
        name: &str,
        expected: usize,
        actual: Option<u64>,
    ) -> Result<()> {
        match actual {
            Some(actual) if actual != expected as u64 => anyhow::bail!(
                "Qdrant collection '{name}' has vector dimension {actual}, but \
                 JARVIS_CONTEXT_EMBEDDING_DIM is {expected}. Set the env var to \
                 {actual} to match the collection, or recreate the collection \
                 for the new embedding model."
            ),
            Some(_) => Ok(()),
            None => {
                warn!(
                    "could not determine vector dimension of Qdrant collection '{name}'; \
                     assuming configured embedding_dim={expected}"
                );
                Ok(())
            }
        }
    }

    /// Extract the dense vector size from a collection-info response.
    /// `None` when the server omitted the config or the collection uses
    /// multiple named vectors (no single dimension to compare against).
    pub(super) fn configured_vector_size(
        info: &qdrant_client::qdrant::CollectionInfo,
    ) -> Option<u64> {
        use qdrant_client::qdrant::vectors_config::Config;
        let vectors = info
            .config
            .as_ref()?
            .params
            .as_ref()?
            .vectors_config
            .as_ref()?;
        match vectors.config.as_ref()? {
            Config::Params(params) => Some(params.size),
            Config::ParamsMap(params_map) => {
                let mut sizes = params_map.map.values().map(|params| params.size);
                let first = sizes.next()?;
                sizes.next().is_none().then_some(first)
            }
        }
    }

    pub async fn ensure_collection(
        client: &Qdrant,
        name: &str,
        embedding_dim: usize,
    ) -> Result<()> {
        validate_embedding_dim(name, embedding_dim)?;

        let collections = client
            .list_collections()
            .await
            .map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;
        if !collections.collections.iter().any(|c| c.name == name) {
            let vectors = VectorParamsBuilder::new(embedding_dim as u64, Distance::Cosine);
            client
                .create_collection(CreateCollectionBuilder::new(name).vectors_config(vectors))
                .await
                .map_err(|e| crate::error::ContextError::Qdrant(e.to_string()))?;
            return Ok(());
        }

        // Existing collection: measure its vector size and fail fast on a
        // mismatch rather than letting upsert/search fail with an opaque error.
        match client.collection_info(name).await {
            Ok(info) => {
                let actual = info.result.as_ref().and_then(configured_vector_size);
                check_existing_dimension(name, embedding_dim, actual)
            }
            Err(error) => {
                warn!("could not inspect Qdrant collection '{name}' dimension: {error}");
                Ok(())
            }
        }
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

        // A missing point id, missing `point_id_options`, or a numeric id
        // cannot be represented as a `Uuid` matching PostgreSQL's `uuid`
        // column. Map those to `None` so `entries_from_raw_points` skips the
        // point (with a warning) instead of falling back to a nil/random id.
        let points = response
            .result
            .into_iter()
            .map(|scored| {
                let id_str = match scored.id.as_ref() {
                    Some(id) => match id.point_id_options.as_ref() {
                        Some(qdrant_client::qdrant::point_id::PointIdOptions::Uuid(u)) => {
                            Some(u.to_string())
                        }
                        Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(_)) | None => {
                            None
                        }
                    },
                    None => None,
                };
                (id_str, scored.payload)
            })
            .collect();
        Ok(entries_from_raw_points(points))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

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

    fn email_filter() -> crate::pii::PiiFilter {
        let mut patterns = std::collections::HashMap::new();
        patterns.insert(
            "email".into(),
            r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}".into(),
        );
        crate::pii::PiiFilter::from_config(crate::pii::PiiConfig {
            patterns,
            redact_replacement: "[REDACTED]".into(),
            enabled: true,
        })
    }

    fn pii_context(email: &str) -> Value {
        json!({
            "event": {"type": "new_email", "payload": {"from": email}},
            "memories": [{"content": format!("contact {email}")}],
            "recent": [{"content": format!("sender {email}")}],
            "timestamp": "2026-11-18T00:00:00Z",
        })
    }

    #[test]
    fn tc_c_pii_is_redacted_across_all_context_fields() {
        let email = "alice@example.com";
        let output = prepare_context(&email_filter(), pii_context(email), 1000).expect("prepare");
        let serialized = serde_json::to_string(&output).expect("serialize");
        assert!(!serialized.contains(email));
        assert_eq!(output["event"]["payload"]["from"], "[REDACTED]");
        assert_eq!(output["memories"][0]["content"], "contact [REDACTED]");
        assert_eq!(output["recent"][0]["content"], "sender [REDACTED]");
    }

    #[test]
    fn tc_c_filter_runs_before_truncation() {
        let email = "very.long.email.address@example-domain.example";
        let context = pii_context(email);
        let before = serde_json::to_string(&context).expect("serialize").len();
        let filtered = email_filter().filter(context.clone()).expect("filter");
        let after = serde_json::to_string(&filtered).expect("serialize").len();
        assert!(after < before);

        // A budget the filtered context fits but the raw one does not:
        // only filter-then-truncate preserves the memory arrays.
        let max_tokens = after / 4 + 1;
        assert!(after <= max_tokens * 4 && max_tokens * 4 < before);

        let output = prepare_context(&email_filter(), context, max_tokens).expect("prepare");
        assert_eq!(output["event"]["payload"]["from"], "[REDACTED]");
        assert_ne!(output["memories"], json!([]));
        assert_ne!(output["recent"], json!([]));
    }

    #[test]
    fn tc_c_truncation_still_applies_after_filtering() {
        let output =
            prepare_context(&email_filter(), pii_context("alice@example.com"), 1).expect("prepare");
        assert_eq!(output["memories"], json!([]));
        assert_eq!(output["recent"], json!([]));
        assert_eq!(output["event"]["payload"]["from"], "[REDACTED]");
    }

    fn entry_with_expiry(expires_at: Option<DateTime<Utc>>) -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            content: "entry".into(),
            embedding: vec![],
            metadata: json!({}),
            source: "test".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            expires_at,
        }
    }

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-19T00:00:00Z")
            .expect("valid RFC 3339 timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn tc_e_expiry_boundary_is_inclusive() {
        let now = fixed_now();
        assert!(is_expired(Some(now), now), "expires_at == now is expired");
        assert!(is_expired(Some(now - chrono::Duration::seconds(1)), now));
        assert!(!is_expired(Some(now + chrono::Duration::seconds(1)), now));
        assert!(!is_expired(None, now), "absent expiry never expires");
    }

    #[test]
    fn tc_e_expired_memories_are_filtered_in_original_order() {
        let now = fixed_now();
        let future = entry_with_expiry(Some(now + chrono::Duration::seconds(30)));
        let boundary = entry_with_expiry(Some(now));
        let never = entry_with_expiry(None);
        let past = entry_with_expiry(Some(now - chrono::Duration::seconds(1)));
        let future2 = entry_with_expiry(Some(now + chrono::Duration::seconds(1)));

        let kept = retain_unexpired(
            vec![
                future.clone(),
                boundary,
                never.clone(),
                past,
                future2.clone(),
            ],
            now,
        );
        assert_eq!(
            kept.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![future.id, never.id, future2.id]
        );
    }

    #[test]
    fn tc_e_embed_failure_is_not_silently_reported_as_empty_success() {
        let failed = Err(anyhow::anyhow!("embedding endpoint returned HTTP 500"));
        let err = query_embedding_outcome(Some(failed))
            .expect_err("a configured endpoint that fails must produce an error");
        let message = format!("{err:#}");
        assert!(message.contains("query embedding"), "{message}");
        assert!(message.contains("HTTP 500"), "{message}");
    }

    #[test]
    fn tc_e_disabled_embedding_is_an_explicit_skip() {
        let outcome =
            query_embedding_outcome(None).expect("disabled endpoint is a documented skip");
        assert_eq!(outcome, None);
    }

    #[test]
    fn tc_e_successful_embedding_is_passed_through() {
        let vector = vec![0.1_f32, 0.2, 0.3];
        let outcome = query_embedding_outcome(Some(Ok(vector.clone()))).expect("embed succeeds");
        assert_eq!(outcome, Some(vector));
    }

    #[test]
    fn tc_f_parse_point_id_rejects_missing_and_malformed_ids() {
        let valid = Uuid::new_v4();
        let valid_str = valid.to_string();
        assert_eq!(parse_point_id(Some(valid_str.as_str())), Some(valid));
        assert_eq!(parse_point_id(Some("not-a-uuid")), None);
        assert_eq!(parse_point_id(Some("")), None);
        assert_eq!(parse_point_id(None), None);
    }

    #[test]
    fn tc_f_malformed_point_id_is_skipped_instead_of_nil_uuid() {
        let valid = Uuid::new_v4();
        let points = vec![
            (Some("definitely-not-a-uuid".to_string()), HashMap::new()),
            (None, HashMap::new()), // missing / numeric point id
            (Some(valid.to_string()), HashMap::new()),
        ];

        let entries = entries_from_raw_points(points);

        assert_eq!(entries.len(), 1, "invalid ids must not enter the results");
        assert_eq!(entries[0].id, valid);
        assert!(
            !entries.iter().any(|entry| entry.id.is_nil()),
            "no nil UUID fallback is allowed"
        );
    }

    #[test]
    fn tc_f_all_invalid_point_ids_produce_no_entries() {
        let points = vec![
            (Some("bad-id".to_string()), HashMap::new()),
            (None, HashMap::new()),
            (Some("42".to_string()), HashMap::new()),
        ];
        assert!(entries_from_raw_points(points).is_empty());
    }

    #[test]
    fn configured_embedding_dim_defaults_to_384() {
        assert_eq!(crate::config::ContextConfig::default().embedding_dim, 384);
    }

    #[test]
    fn zero_embedding_dim_is_rejected_before_qdrant_access() {
        let err = memory::validate_embedding_dim("jarvis_memories", 0)
            .expect_err("a zero dimension can never create a valid collection");
        let message = format!("{err:#}");
        assert!(
            message.contains("JARVIS_CONTEXT_EMBEDDING_DIM"),
            "{message}"
        );
        assert!(message.contains("jarvis_memories"), "{message}");
    }

    #[test]
    fn existing_collection_dimension_mismatch_is_an_actionable_error() {
        assert!(
            memory::check_existing_dimension("jarvis_memories", 768, Some(768)).is_ok(),
            "a matching dimension is accepted"
        );

        let err = memory::check_existing_dimension("jarvis_memories", 768, Some(384))
            .expect_err("a 384-dim collection cannot serve a 768-dim model");
        let message = format!("{err:#}");
        assert!(message.contains("jarvis_memories"), "{message}");
        assert!(message.contains("384"), "{message}");
        assert!(message.contains("768"), "{message}");
        assert!(
            message.contains("JARVIS_CONTEXT_EMBEDDING_DIM"),
            "{message}"
        );
    }

    #[test]
    fn unknown_existing_dimension_only_warns() {
        assert!(
            memory::check_existing_dimension("jarvis_memories", 384, None).is_ok(),
            "older servers may omit the config; warn and keep starting"
        );
    }

    #[test]
    fn configured_vector_size_reads_dense_collection_config() {
        use qdrant_client::qdrant::vectors_config::Config;
        use qdrant_client::qdrant::{
            CollectionConfig, CollectionInfo, CollectionParams, VectorParams, VectorsConfig,
        };

        let info = CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig {
                        config: Some(Config::Params(VectorParams {
                            size: 768,
                            ..Default::default()
                        })),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(memory::configured_vector_size(&info), Some(768));
        assert_eq!(
            memory::configured_vector_size(&CollectionInfo::default()),
            None
        );
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
