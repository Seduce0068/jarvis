//! Context building core

use crate::config::validation::validate_embedding;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use jarvis_core::{MemoryEntry, UnifiedEvent};
use qdrant_client::Qdrant;
use serde_json::Value;
use std::sync::Arc;
use tracing::{error, instrument, warn};
use uuid::Uuid;

/// Fixed-vocabulary error categories for builder-layer warnings.
///
/// The full `anyhow` chain is deliberately never formatted into a log record:
/// it can carry Qdrant URLs and response fragments, PostgreSQL connection and
/// statement details, and outbox re-embedding content. Only these tokens reach
/// OTLP/Grafana. The `ContextError` mapping mirrors `grpc::error_kind` and the
/// transport policy; the two extra variants cover the injectable seams
/// (persistence outbox) and embedding failures, which are plain `anyhow::Error`
/// rather than `ContextError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorKind {
    Database,
    Qdrant,
    Pii,
    Config,
    Grpc,
    Serialization,
    Persistence,
    Embedding,
}

impl ErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Qdrant => "qdrant",
            Self::Pii => "pii",
            Self::Config => "config",
            Self::Grpc => "grpc",
            Self::Serialization => "serialization",
            Self::Persistence => "persistence",
            Self::Embedding => "embedding",
        }
    }
}

/// Reduce an internal error to a fixed category, never to its message.
///
/// `fallback` is the calling site's own dependency class for errors that are
/// not a `ContextError` (the `VectorSink`/`PendingStore` seams return
/// `anyhow::Result`, and embedding failures are raised with `bail!`). No part
/// of the error's `Display` or `Debug` is ever read here.
fn error_kind(err: &anyhow::Error, fallback: ErrorKind) -> &'static str {
    match err.downcast_ref::<crate::error::ContextError>() {
        Some(crate::error::ContextError::Database(_)) => ErrorKind::Database.as_str(),
        Some(crate::error::ContextError::Qdrant(_)) => ErrorKind::Qdrant.as_str(),
        Some(crate::error::ContextError::Pii(_)) => ErrorKind::Pii.as_str(),
        Some(crate::error::ContextError::Config(_)) => ErrorKind::Config.as_str(),
        Some(crate::error::ContextError::Grpc(_)) => ErrorKind::Grpc.as_str(),
        Some(crate::error::ContextError::Serialization(_)) => ErrorKind::Serialization.as_str(),
        None => fallback.as_str(),
    }
}

/// Reduce any Qdrant client failure to the fixed `error_kind` vocabulary.
///
/// The raw error `E` is consumed by `_` and never formatted: its `Display` can
/// embed endpoint URLs and response fragments. Only the private-token newtype
/// `crate::error::QdrantErrorKind` (here: its `FAILED` constant) is accepted by
/// the `ContextError::qdrant` constructor, so the returned error chain cannot
/// carry client-supplied text into stderr/journal/OTLP. Every Qdrant call site
/// in this module uses this single mapper.
fn discard_qdrant_error<T, E>(
    result: std::result::Result<T, E>,
) -> std::result::Result<T, crate::error::ContextError> {
    result.map_err(|_| crate::error::ContextError::qdrant(crate::error::QdrantErrorKind::FAILED))
}

/// Reassemble PostgreSQL rows in the relevance order supplied by Qdrant.
/// `WHERE id = ANY($1)` does not preserve the input order, so without this
/// the vector-search ranking is silently discarded (P1-1).
fn reorder_by_ids(entries: Vec<MemoryEntry>, ids: &[Uuid]) -> Vec<MemoryEntry> {
    let mut by_id: std::collections::HashMap<Uuid, MemoryEntry> =
        entries.into_iter().map(|entry| (entry.id, entry)).collect();
    ids.iter().filter_map(|id| by_id.remove(id)).collect()
}

/// Attach Qdrant similarity scores captured before the PostgreSQL refetch.
///
/// The refetch only returns row columns, so the score would otherwise be lost
/// (P1-1). Kept pure so both the relevance order and the score values are
/// testable without a live Qdrant server. An id without a matching score keeps
/// `None`, which stays distinguishable from a genuine zero score.
fn attach_scores(
    entries: Vec<MemoryEntry>,
    scores: &std::collections::HashMap<Uuid, Option<f32>>,
) -> Vec<MemoryEntry> {
    entries
        .into_iter()
        .map(|mut entry| {
            entry.score = scores.get(&entry.id).copied().flatten();
            entry
        })
        .collect()
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

/// How many candidates to request from Qdrant for a caller limit of `limit`.
///
/// Policy for the expired-hits problem (Round 19 B3): a memory point's Qdrant
/// payload carries only `metadata` (see [`memory::upsert_vector`]) — it does
/// **not** contain `expires_at`. A server-side `expires_at` payload filter on
/// `SearchPoints` is therefore not applied: it would either match nothing on
/// legacy points that lack the field, or silently drop valid points whose
/// `metadata` happens to carry an `expires_at` key. Instead, PostgreSQL's
/// `expires_at IS NULL OR expires_at > $now` stays the authoritative expiry
/// gate, Qdrant is asked for a wider candidate pool, and the pool is filtered
/// then trimmed back to `limit`. That way an expired hit ranked in the top-K
/// cannot steal a slot from a still-valid memory.
const EXPIRY_OVERFETCH_FACTOR: usize = 2;

/// Candidate count sent to Qdrant for a caller limit of `limit`. Pure so the
/// over-fetch policy is directly testable without a live Qdrant connection.
fn overfetch_limit(limit: usize) -> usize {
    limit.saturating_mul(EXPIRY_OVERFETCH_FACTOR)
}

/// Final top-`limit` selection after the expiry gate.
///
/// `candidates` must be in Qdrant relevance order. Expired entries are removed
/// here with the same inclusive `<=` boundary SQL uses, *before* trimming, so
/// they cannot occupy a slot that a valid memory should fill. Kept pure so the
/// regression is testable without Qdrant/PostgreSQL.
fn select_top_valid(
    candidates: Vec<MemoryEntry>,
    now: DateTime<Utc>,
    limit: usize,
) -> Vec<MemoryEntry> {
    retain_unexpired(candidates, now)
        .into_iter()
        .take(limit)
        .collect()
}

/// Classify the outcome of producing a query embedding.
///
/// `None` means the endpoint is intentionally unconfigured: vector search is
/// skipped by design and PostgreSQL-backed recent memories still work.
/// `Some(Err(..))` is a genuine `embed()` failure and is propagated with
/// context, so no caller can mistake a failed embedding for a search that
/// simply found no results.
///
/// `Some(Ok(vector))` is gated through the shared embedding validator with the
/// configured dimension. The embedding service is an upstream dependency, so a
/// non-finite component (including a finite f64 saturated by the f32 cast) can
/// still arrive here; letting it through would reach `search_vectors` and
/// Qdrant's distance computation. The returned `InvalidInput` keeps the fixed
/// 400 / `InvalidArgument` vocabulary instead of a bare internal failure.
fn query_embedding_outcome(
    embed_result: Option<anyhow::Result<Vec<f32>>>,
    embedding_dim: usize,
) -> anyhow::Result<Option<Vec<f32>>> {
    match embed_result {
        None => {
            warn!("memory vector search skipped: embedding endpoint not configured");
            Ok(None)
        }
        Some(result) => {
            let embedding = result.context("producing query embedding for memory vector search")?;
            validate_embedding(&embedding, embedding_dim)?;
            Ok(Some(embedding))
        }
    }
}

/// Gate the query vector immediately before `search_vectors`. Kept as a pure
/// helper so this boundary's non-finite rejection is testable without a live
/// Qdrant client. The `InvalidInput` keeps the fixed 400 / `InvalidArgument`
/// vocabulary at both transports.
fn validate_search_vector(embedding: &[f32], embedding_dim: usize) -> Result<()> {
    validate_embedding(embedding, embedding_dim)?;
    Ok(())
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
                score: None, // Filled from the Qdrant score after the refetch.
            })
        })
        .collect()
}

/// Maximum number of outbox rows retried in one startup pass. A larger backlog
/// is drained over successive starts instead of blocking startup indefinitely.
const RECONCILE_BATCH_LIMIT: usize = 100;

/// Upper bound on the delay between two reconcile attempts for a single row.
const MAX_RETRY_BACKOFF_MINUTES: i64 = 60 * 24; // 24 h

/// Maximum number of failed reconcile attempts for one `pending_embeddings`
/// row (I61-B). A row at this count is "exhausted": `PgPendingStore::list`
/// filters it out with `attempts < $limit`, so a permanently-failing row can
/// no longer occupy a slot of [`RECONCILE_BATCH_LIMIT`] on every startup.
const MAX_RECONCILE_ATTEMPTS: i32 = 10;

/// Selection query for one reconcile batch (I30-C / I61-B / I63-A).
///
/// Named so the three eligibility gates are stated once and can be asserted in
/// a pure test without a live PostgreSQL connection. A dead-lettered row is
/// retained in the table for auditing but can never be selected again; an
/// exhausted row is excluded from retries and exported by its own gauge
/// (I65-B) rather than being counted as recoverable pending work.
const LIST_PENDING_SQL: &str = "SELECT id, content, metadata, embedding, attempts, next_retry_at
                 FROM pending_embeddings
                 WHERE dead_lettered_at IS NULL
                   AND attempts < $2
                   AND (next_retry_at IS NULL OR next_retry_at <= NOW())
                 ORDER BY created_at ASC LIMIT $1";

/// Partition the non-dead-lettered outbox into recoverable (`pending`) and
/// retired (`exhausted`) rows for the two gauges (I65-B). One aggregate query
/// keeps both counts consistent and, like the older single COUNT, exposes only
/// numbers to the metrics layer. Named for a pure string assertion.
const OUTBOX_STATE_COUNTS_SQL: &str = "SELECT
       COUNT(*) FILTER (WHERE vector_state = 'pending'),
       COUNT(*) FILTER (WHERE vector_state = 'exhausted')
   FROM pending_embeddings
   WHERE dead_lettered_at IS NULL";

/// Idempotent backfill for rows that hit the attempt cap before the `exhausted`
/// state existed (I65-B). Otherwise those legacy rows would disappear from
/// both gauges once `pending` is filtered by `vector_state`. Best-effort: a
/// failed write is retried on the next reconcile pass.
const BACKFILL_EXHAUSTED_SQL: &str = "UPDATE pending_embeddings
       SET vector_state = 'exhausted'
       WHERE dead_lettered_at IS NULL
         AND vector_state = 'pending'
         AND attempts >= $1";

/// `true` when a vector holds at least one non-finite component — NaN, a
/// signed infinity, or a finite f64 that saturated on the f32 cast (I61-B).
///
/// Detection alone is not a death sentence (I63-B): re-embedding can repair a
/// poisoned snapshot when its dimension is stale, because [`vector_for_pending`]
/// then calls the embedder instead of replaying the stored vector. Only poison
/// at the configured dimension is unrecoverable — see
/// [`is_unrecoverable_poison`]. An empty vector is deliberately *not* poison:
/// it is the recoverable "stale dimension" case too. Pure and data-free, so no
/// vector value can ever reach a log record.
fn is_non_finite_vector(vector: &[f32]) -> bool {
    vector.iter().any(|value| !value.is_finite())
}

/// Whether a non-finite stored vector is beyond repair by re-embedding
/// (I63-B).
///
/// [`vector_for_pending`] re-embeds the content only when the stored dimension
/// differs from the configured one. When the dimensions already match it just
/// re-validates and replays the stored vector, so a non-finite value there
/// cannot be repaired and the row is dead-lettered. A stale-dimension poison is
/// *not* unrecoverable: it stays on the re-embed/retry path so a healthy
/// embedding can replace it instead of losing the memory. Pure and data-free.
fn is_unrecoverable_poison(stored: &[f32], embedding_dim: usize) -> bool {
    is_non_finite_vector(stored) && embedding_dim > 0 && stored.len() == embedding_dim
}

/// Decode a stored `pending_embeddings.embedding` JSON column (C61-R2-01).
///
/// Returns `None` only when decoding fails. The JSON value and the `serde`
/// error are consumed here and never retained, so `list` can emit exactly one
/// fixed-vocabulary warning without any untrusted payload, row id or error
/// chain reaching a log record. Keeping the decision pure makes the corrupt
/// branch directly testable without a thread-local tracing subscriber.
///
/// `None` maps to the same empty vector as before, so the re-embed/retry path
/// is unchanged; only the previously silent conversion is now observable.
fn parse_embedding_json(value: Value) -> Option<Vec<f32>> {
    serde_json::from_value::<Vec<f32>>(value).ok()
}

/// Exponential backoff after `attempts` failures: `2^attempts` minutes, capped
/// at [`MAX_RETRY_BACKOFF_MINUTES`]. The exponent is clamped before shifting so
/// even a hostile/corrupt attempt count cannot overflow. Pure, so the policy is
/// directly testable without a database or a clock.
fn retry_backoff(attempts: i32) -> chrono::Duration {
    let exponent = attempts.clamp(0, 32) as u32;
    let minutes = 1_i64
        .checked_shl(exponent)
        .unwrap_or(i64::MAX)
        .min(MAX_RETRY_BACKOFF_MINUTES);
    chrono::Duration::minutes(minutes)
}

/// State to persist after a failed reconcile attempt: the capped attempt count
/// and the next eligible instant (`now + backoff`). Pure for tests.
///
/// I61-B: `attempts` saturates at [`MAX_RECONCILE_ATTEMPTS`]. Every failure site
/// goes through this helper, so no row can climb past the cap and the `list`
/// gate (`attempts < cap`) is sufficient to retire it from every future batch.
fn next_retry_state(attempts: i32, now: DateTime<Utc>) -> (i32, DateTime<Utc>) {
    let attempts = attempts.saturating_add(1).min(MAX_RECONCILE_ATTEMPTS);
    (attempts, now + retry_backoff(attempts))
}

/// Whether an outbox row is due for a retry at `now`. `None` (never deferred)
/// is always due. This mirrors the SQL gate
/// `next_retry_at IS NULL OR next_retry_at <= NOW()` and is reused by the
/// in-memory store in tests so the head-of-line regression is testable without
/// a live PostgreSQL connection.
#[cfg(test)]
fn is_retry_due(next_retry_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    next_retry_at.is_none_or(|at| at <= now)
}

/// One queued vector upsert (P1-2).
///
/// Only the fields Qdrant needs are persisted. The full row already lives in
/// `memories`, so id/content/metadata plus the captured vector are enough to
/// rebuild its point during reconcile.
#[derive(Clone, Debug)]
struct PendingEmbedding {
    id: Uuid,
    content: String,
    metadata: Value,
    embedding: Vec<f32>,
    /// Number of failed reconcile attempts so far; drives exponential backoff.
    attempts: i32,
    /// Earliest UTC instant at which this row may be retried. `None` means the
    /// row has never been deferred and is due immediately.
    // Mirrored for the in-memory test store; production due-checks run in SQL.
    #[allow(dead_code)]
    next_retry_at: Option<DateTime<Utc>>,
    /// Mirrors `pending_embeddings.vector_state` for the in-memory store
    /// (I65-B). Only the fixed tokens `'pending'` and `'exhausted'` are written
    /// here; dead-lettered rows are removed by the seam's default method.
    // Read by the in-memory assertions; production state lives in SQL.
    #[allow(dead_code)]
    vector_state: &'static str,
}

/// Qdrant upsert seam. A trait keeps the outbox/reconcile decision testable
/// with an injected failure and without a live Qdrant server.
#[async_trait::async_trait]
trait VectorSink: Send + Sync {
    async fn upsert(&self, memory: &MemoryEntry) -> Result<()>;
}

/// `pending_embeddings` persistence seam, likewise injectable in tests.
#[async_trait::async_trait]
trait PendingStore: Send + Sync {
    async fn record(&self, memory: &MemoryEntry, embedding: &[f32]) -> Result<()>;
    async fn list(&self, limit: usize) -> Result<Vec<PendingEmbedding>>;
    async fn remove(&self, id: Uuid) -> Result<()>;
    /// Record a failed reconcile attempt: bump `attempts` and defer the row
    /// until `next_retry_at` so it stops blocking the head of the queue.
    async fn mark_failed(
        &self,
        id: Uuid,
        attempts: i32,
        next_retry_at: DateTime<Utc>,
    ) -> Result<()>;
    /// Persist a terminal dead-letter decision (I63-A).
    ///
    /// `PgPendingStore` overrides this to retain an auditable row:
    /// `dead_lettered_at`/`vector_state` are written and the `list` gate keeps
    /// the row out of every future batch without deleting the memory. The
    /// default keeps the legacy delete semantics for `MemoryOutbox`, whose
    /// outbox/reconcile regressions predate retention; test stores that mirror
    /// production override the seam so retention and the gate are asserted
    /// behaviourally (see `RetainingOutbox` and the log-safety `LeakStore`).
    async fn dead_letter(&self, id: Uuid, _at: DateTime<Utc>) -> Result<()> {
        self.remove(id).await
    }
    /// Persist the terminal `exhausted` state when a row spends its whole retry
    /// budget (I65-B). The default is a no-op so existing in-memory test stores
    /// keep their historical behaviour; `PgPendingStore` writes
    /// `vector_state = 'exhausted'` and `MemoryOutbox` retains the state so the
    /// distinction between a retired row and a recoverable backlog is testable
    /// without a live database.
    async fn mark_exhausted(&self, _id: Uuid) -> Result<()> {
        Ok(())
    }
    /// Re-aggregate `OUTBOX_STATE_COUNTS_SQL` and publish both gauges once
    /// after a batch of terminal outbox transitions (I69-D).
    ///
    /// `record`/`list` already refresh on their own, but a successful
    /// `dead_letter` / `mark_exhausted` / `remove` used to leave the gauges at
    /// their pre-transition values (an inflated recoverable backlog). Only the
    /// two bare COUNTs are ever read; no id, payload or vector value crosses
    /// this seam. The default no-op keeps the pre-existing in-memory test
    /// stores unchanged; `PgPendingStore` runs the aggregate and mock stores
    /// override it so the refresh ordering is testable without a database.
    async fn observe_state_counts(&self) {}
}

struct QdrantVectorSink {
    client: Arc<Qdrant>,
    collection: String,
}

#[async_trait::async_trait]
impl VectorSink for QdrantVectorSink {
    async fn upsert(&self, memory: &MemoryEntry) -> Result<()> {
        memory::upsert_vector(&self.client, &self.collection, memory).await
    }
}

struct PgPendingStore {
    pool: Arc<deadpool_postgres::Pool>,
}

#[async_trait::async_trait]
impl PendingStore for PgPendingStore {
    async fn record(&self, memory: &MemoryEntry, embedding: &[f32]) -> Result<()> {
        let client = self.pool.get().await?;
        let embedding_json = serde_json::to_value(embedding)
            .context("serializing embedding for the pending_embeddings outbox")?;
        client
            .execute(
                "INSERT INTO pending_embeddings (id, content, metadata, embedding, created_at)
                 VALUES ($1, $2, $3, $4, NOW())
                 ON CONFLICT (id) DO UPDATE SET
                     content = EXCLUDED.content,
                     metadata = EXCLUDED.metadata,
                     embedding = EXCLUDED.embedding,
                     created_at = NOW(),
                     -- A fresh failure restarts the backoff: this row is due now.
                     attempts = 0,
                     next_retry_at = NULL,
                     -- ...and clears any earlier terminal decision (I63-A).
                     dead_lettered_at = NULL,
                     vector_state = 'pending'",
                &[
                    &memory.id,
                    &memory.content,
                    &memory.metadata,
                    &embedding_json,
                ],
            )
            .await?;
        // Best-effort observability (I63-A / I65-B): a failed aggregate must not
        // turn a successfully queued embedding into a reported store failure.
        if let Ok(row) = client.query_one(OUTBOX_STATE_COUNTS_SQL, &[]).await {
            let pending: i64 = row.get(0);
            let exhausted: i64 = row.get(1);
            crate::http::observe_pending_embeddings(pending);
            crate::http::observe_exhausted_embeddings(exhausted);
        }
        Ok(())
    }

    async fn list(&self, limit: usize) -> Result<Vec<PendingEmbedding>> {
        let client = self.pool.get().await?;
        // I65-B: retire legacy rows that exhausted the budget before the state
        // existed, so the two gauges partition the backlog correctly. This is
        // idempotent and touches only rows the `attempts < cap` gate already
        // excludes from retries; a failed backfill is retried on the next pass.
        let _ = client
            .execute(BACKFILL_EXHAUSTED_SQL, &[&MAX_RECONCILE_ATTEMPTS])
            .await;
        // Only live, due rows still inside the attempt budget are selected. A
        // deferred row drops out of the head of the batch (I30-C); once its
        // attempts reach `MAX_RECONCILE_ATTEMPTS` it drops out entirely
        // (I61-B); a dead-lettered row is never selected again (I63-A). Poison
        // rows therefore cannot starve healthy PG-only memories.
        let rows = client
            .query(
                LIST_PENDING_SQL,
                &[&(limit as i64), &MAX_RECONCILE_ATTEMPTS],
            )
            .await?;
        // Best-effort gauges (I63-A / I65-B): the list gate only selects rows
        // with `vector_state = 'pending'` (via the attempt cap), so report the
        // retired rows separately instead of presenting them as recoverable.
        if let Ok(row) = client.query_one(OUTBOX_STATE_COUNTS_SQL, &[]).await {
            let pending: i64 = row.get(0);
            let exhausted: i64 = row.get(1);
            crate::http::observe_pending_embeddings(pending);
            crate::http::observe_exhausted_embeddings(exhausted);
        }
        rows.into_iter()
            .map(|row| {
                let embedding_json: Value = row.get(3);
                let embedding = match parse_embedding_json(embedding_json) {
                    Some(embedding) => embedding,
                    None => {
                        warn!(
                            error_kind = "embedding_json_parse_failed",
                            "pending embedding row has a corrupt embedding JSON; using empty vector"
                        );
                        Vec::new()
                    }
                };
                Ok(PendingEmbedding {
                    id: row.get(0),
                    content: row.get(1),
                    metadata: row.get(2),
                    embedding,
                    attempts: row.get(4),
                    next_retry_at: row.get(5),
                    // The selection gate only returns still-pending rows, so the
                    // mirrored state is fixed vocabulary: no row value is read.
                    vector_state: "pending",
                })
            })
            .collect()
    }

    async fn remove(&self, id: Uuid) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute("DELETE FROM pending_embeddings WHERE id = $1", &[&id])
            .await?;
        Ok(())
    }

    async fn mark_failed(
        &self,
        id: Uuid,
        attempts: i32,
        next_retry_at: DateTime<Utc>,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        client
            .execute(
                // Compare-and-set: a concurrent reconciler may have bumped
                // `attempts` already. Never overwrite it with a stale snapshot,
                // otherwise the exponential backoff loses its guarantee.
                "UPDATE pending_embeddings SET attempts = $2, next_retry_at = $3
                 WHERE id = $1 AND attempts <= $2",
                &[&id, &attempts, &next_retry_at],
            )
            .await?;
        Ok(())
    }

    async fn dead_letter(&self, id: Uuid, dead_lettered_at: DateTime<Utc>) -> Result<()> {
        let client = self.pool.get().await?;
        // Retain the row (and therefore the memory's audit trail) instead of
        // deleting it. `dead_lettered_at IS NULL` in LIST_PENDING_SQL is what
        // permanently excludes it from every future reconcile batch (I63-A).
        client
            .execute(
                "UPDATE pending_embeddings
                 SET dead_lettered_at = $2, vector_state = 'dead_lettered'
                 WHERE id = $1",
                &[&id, &dead_lettered_at],
            )
            .await?;
        Ok(())
    }

    async fn mark_exhausted(&self, id: Uuid) -> Result<()> {
        let client = self.pool.get().await?;
        // I65-B: persist the terminal state instead of leaving the row as
        // `'pending'` while it silently drops out of every retry batch. The
        // `attempts < cap` gate still excludes it; this makes the retired row
        // visible to `jarvis_exhausted_embeddings` and distinct from a removable
        // backlog. No id/payload label reaches metrics — only a bare COUNT does.
        client
            .execute(
                "UPDATE pending_embeddings
                 SET vector_state = 'exhausted'
                 WHERE id = $1 AND dead_lettered_at IS NULL",
                &[&id],
            )
            .await?;
        Ok(())
    }

    async fn observe_state_counts(&self) {
        // Best-effort by construction (I63-A / I65-B / I69-D): an aggregate
        // failure must never turn an already-durable terminal write into a
        // reported store failure. Only the two bare COUNTs reach the metrics
        // layer; no row value is ever read.
        let Ok(client) = self.pool.get().await else {
            return;
        };
        if let Ok(row) = client.query_one(OUTBOX_STATE_COUNTS_SQL, &[]).await {
            let pending: i64 = row.get(0);
            let exhausted: i64 = row.get(1);
            crate::http::observe_pending_embeddings(pending);
            crate::http::observe_exhausted_embeddings(exhausted);
        }
    }
}

/// Attempt the vector upsert after the PostgreSQL write has succeeded (P1-2).
///
/// A Qdrant failure is recorded in the outbox instead of being lost: the PG
/// row is already durable and a later startup will retry the upsert. A stale
/// outbox row for the same id is cleared after a successful upsert.
async fn store_memory_with_sinks(
    memory: &MemoryEntry,
    sink: &dyn VectorSink,
    outbox: &dyn PendingStore,
) -> Result<()> {
    match sink.upsert(memory).await {
        Ok(()) => {
            if let Err(error) = outbox.remove(memory.id).await {
                warn!(
                    id = %memory.id,
                    error_kind = error_kind(&error, ErrorKind::Persistence),
                    "vector upsert succeeded but a stale pending_embeddings row could not be removed"
                );
            }
            // I69-D: a confirmed delete changes the outbox state counts, so
            // refresh once here as well. This path is outside
            // `reconcile_pending`; without this call a stale row removed after
            // a successful upsert would keep the pending gauge inflated.
            outbox.observe_state_counts().await;
            Ok(())
        }
        Err(error) => {
            warn!(
                id = %memory.id,
                error_kind = error_kind(&error, ErrorKind::Qdrant),
                "Qdrant upsert failed; queuing memory in pending_embeddings for retry"
            );
            outbox
                .record(memory, &memory.embedding)
                .await
                .context("queuing a failed Qdrant upsert in pending_embeddings")?;
            Err(error)
        }
    }
}

/// Injectable re-embedding seam (I63-B). Production delegates to
/// [`crate::embedding::EmbeddingClient`]; tests can return a canned vector so
/// the stale-dimension recovery path is exercised without a live endpoint, DB
/// or Qdrant connection.
#[async_trait::async_trait]
trait Reembed {
    async fn reembed(&self, content: &str) -> Result<Vec<f32>>;
}

#[async_trait::async_trait]
impl Reembed for crate::embedding::EmbeddingClient {
    async fn reembed(&self, content: &str) -> Result<Vec<f32>> {
        self.embed(content)
            .await
            .context("re-embedding a pending memory during reconcile")
    }
}

/// Choose the vector used to retry a queued upsert (P1-2).
///
/// The vector captured when the upsert failed is preferred: retrying is then
/// deterministic and does not depend on the embedding endpoint being up. If
/// that vector is absent or no longer matches the configured dimension, the
/// content is re-embedded when an endpoint is configured.
///
/// Both the replayed and the re-embedded vector pass the shared embedding
/// validator. A queued vector can be non-finite (an f64 that saturated on the
/// f32 cast before it was persisted). With a stale dimension this helper
/// re-embeds the content and thereby repairs the poison; with a matching
/// dimension it only re-validates the same vector and fails, which is the sole
/// case where `reconcile_pending` dead-letters the row (I63-B). The validator
/// here stays as the helper's own fail-closed boundary.
async fn vector_for_pending(
    pending: &PendingEmbedding,
    embedder: Option<&crate::embedding::EmbeddingClient>,
    embedding_dim: usize,
) -> Result<Vec<f32>> {
    let seam = embedder
        .filter(|client| client.is_enabled())
        .map(|client| client as &dyn Reembed);
    vector_for_pending_with(pending, seam, embedding_dim).await
}

/// [`vector_for_pending`] with the embed call injected, so the recovery path is
/// testable without any live service (I63-B). `reembed` is `None` when no
/// embedding endpoint is enabled.
async fn vector_for_pending_with(
    pending: &PendingEmbedding,
    reembed: Option<&dyn Reembed>,
    embedding_dim: usize,
) -> Result<Vec<f32>> {
    if embedding_dim > 0 && pending.embedding.len() == embedding_dim {
        validate_embedding(&pending.embedding, embedding_dim)?;
        return Ok(pending.embedding.clone());
    }

    let Some(seam) = reembed else {
        anyhow::bail!(
            "pending memory {} has no stored vector of dimension {embedding_dim} \
             and no embedding endpoint is configured",
            pending.id
        );
    };

    let vector = seam.reembed(&pending.content).await?;
    if vector.len() != embedding_dim {
        anyhow::bail!(
            "re-embedded pending memory {} produced {} dimensions, expected {embedding_dim}",
            pending.id,
            vector.len()
        );
    }
    validate_embedding(&vector, embedding_dim)?;
    Ok(vector)
}

/// Persist a failed reconcile attempt so the row is deferred out of the head
/// of the queue (I30-C). `next_retry_state` caps `attempts` at
/// [`MAX_RECONCILE_ATTEMPTS`], at which point `list` excludes the row entirely
/// (I61-B) and the durable `exhausted` state is written (I65-B). Backoff
/// bookkeeping is best-effort: if it cannot be written the row stays queued and
/// will simply be retried on the next pass, which is safe (no worse than the
/// pre-fix behaviour).
async fn defer_failed(outbox: &dyn PendingStore, id: Uuid, attempts: i32) {
    let (attempts, next_retry_at) = next_retry_state(attempts, Utc::now());
    if let Err(error) = outbox.mark_failed(id, attempts, next_retry_at).await {
        warn!(
            id = %id,
            attempts = attempts,
            error_kind = error_kind(&error, ErrorKind::Persistence),
            "could not record a deferred retry for a pending memory"
        );
        return;
    }
    // I65-B: a row retired by the attempt cap must not stay `'pending'` in the
    // database. Only mark it after the attempt count itself was durably stored,
    // so a failed `mark_failed` never prematurely retires a recoverable row.
    if attempts >= MAX_RECONCILE_ATTEMPTS
        && let Err(error) = outbox.mark_exhausted(id).await
    {
        warn!(
            id = %id,
            attempts = attempts,
            error_kind = error_kind(&error, ErrorKind::Persistence),
            "could not persist the exhausted state for a pending memory"
        );
    }
}

/// Dead-letter a pending row whose vector is non-finite and unrecoverable
/// (I61-B / I63-A / I63-B).
///
/// Only poison at the configured dimension is handled here:
/// [`vector_for_pending`] does not re-embed in that case, so deferring would
/// replay the same bad vector every 24h forever and starve healthy PG-only
/// memories once [`RECONCILE_BATCH_LIMIT`] rows were poisoned. A stale-dimension
/// poison gets a re-embedding attempt first and is never dead-lettered here
/// (I63-B). The row is retired instead: the
/// production store writes `dead_lettered_at`/`vector_state` and keeps the row
/// for inspection, while the `dead_lettered_at IS NULL` selection gate makes
/// any retry impossible. Test stores mirroring production override the seam
/// so both halves are asserted behaviourally; only the legacy `MemoryOutbox`
/// still uses the delete-based default. A single decision warning uses
/// only the fixed `id`/`attempts`/`error_kind` vocabulary, and the decision is
/// counted by a fixed-label metric; no raw error, payload or vector value is
/// ever formatted or exported.
async fn dead_letter_retry(outbox: &dyn PendingStore, id: Uuid, attempts: i32) {
    dead_letter_retry_with(outbox, id, attempts, &PrometheusDeadLetterCounters).await;
}

/// Counter sink for terminal dead-letter decisions (I65-B).
///
/// The success counter is incremented only after the outbox confirms the write;
/// a failed write increments a separate failure counter instead, so a decision
/// that was never persisted is never reported as durable. Injecting the sink
/// keeps the ordering testable without installing a global Prometheus recorder.
trait DeadLetterCounters: Send + Sync {
    fn persisted(&self);
    fn persistence_failed(&self);
}

/// Production counter sink: fixed-label metrics from [`crate::http`].
struct PrometheusDeadLetterCounters;

impl DeadLetterCounters for PrometheusDeadLetterCounters {
    fn persisted(&self) {
        crate::http::record_dead_lettered_embedding();
    }

    fn persistence_failed(&self) {
        crate::http::record_dead_letter_persistence_failure();
    }
}

async fn dead_letter_retry_with(
    outbox: &dyn PendingStore,
    id: Uuid,
    attempts: i32,
    counters: &dyn DeadLetterCounters,
) {
    warn!(
        id = %id,
        attempts = attempts,
        error_kind = ErrorKind::Embedding.as_str(),
        "pending memory has a non-finite vector; dead-lettering this row"
    );
    // I65-B: durability first, accounting second. The monotonic success counter
    // must only ever count decisions that actually reached the store.
    match outbox.dead_letter(id, Utc::now()).await {
        Ok(()) => counters.persisted(),
        Err(error) => {
            warn!(
                id = %id,
                error_kind = error_kind(&error, ErrorKind::Persistence),
                "could not persist a dead-letter decision for a pending memory"
            );
            counters.persistence_failed();
        }
    }
}

/// Startup reconcile of queued vector upserts (P1-2). Never fails the whole
/// startup: a transient Qdrant/embedding failure leaves the row queued with a
/// warning so the next startup retries it. Every transient failure bumps
/// `attempts` and sets a future `next_retry_at`, so a permanently-failing row
/// is filtered out of the next batch instead of starving later rows (I30-C);
/// at [`MAX_RECONCILE_ATTEMPTS`] it is excluded from every batch (I61-B). A
/// non-finite vector is only dead-lettered when it is *unrecoverable*:
/// [`vector_for_pending`] gets the first chance to re-embed the content, which
/// repairs poison stored at a stale dimension; only poison at the configured
/// dimension — where no re-embed happens — is retired with a single warning
/// rather than retried forever (I63-B, refining I61-B). I63-A persists that
/// terminal decision on the row and excludes it from every later batch, so it
/// warns once while remaining auditable.
async fn reconcile_pending(
    outbox: &dyn PendingStore,
    sink: &dyn VectorSink,
    embedder: Option<&crate::embedding::EmbeddingClient>,
    embedding_dim: usize,
) {
    let pending = match outbox.list(RECONCILE_BATCH_LIMIT).await {
        Ok(pending) => pending,
        Err(error) => {
            warn!(
                batch_limit = RECONCILE_BATCH_LIMIT,
                error_kind = error_kind(&error, ErrorKind::Persistence),
                "could not read pending_embeddings; deferring reconcile to next startup"
            );
            return;
        }
    };

    for item in pending {
        // Copy before the partial move of `item.content` / `item.metadata`
        // below, so the failure path can still defer this row.
        let attempts = item.attempts;

        // I63-B: do NOT dead-letter a non-finite vector up front. Give
        // `vector_for_pending` the first chance to re-embed the content: that
        // is the only path that can repair poison stored at a stale dimension.
        // The terminal decision is therefore taken in the failure branch below,
        // and only for unrecoverable poison (dimension already matches, so no
        // re-embed would happen). An empty vector was never poison; it already
        // used the embedder/retry path.
        let vector = match vector_for_pending(&item, embedder, embedding_dim).await {
            Ok(vector) => vector,
            Err(error) => {
                if is_unrecoverable_poison(&item.embedding, embedding_dim) {
                    // Non-finite + matching dimension: `vector_for_pending` only
                    // replayed and re-validated the same poison, so re-embedding
                    // cannot help. I61-B: retire it instead of retrying forever;
                    // I63-A: mark it rather than delete, and the
                    // `dead_lettered_at IS NULL` gate keeps it out of later
                    // batches, so it cannot reappear and warn again.
                    dead_letter_retry(outbox, item.id, attempts).await;
                } else {
                    // Stale-dimension poison (or any transient failure) keeps the
                    // re-embed/retry path: a later pass with an embedder may
                    // still repair the row instead of losing the memory.
                    warn!(
                        id = %item.id,
                        attempts = attempts,
                        error_kind = error_kind(&error, ErrorKind::Embedding),
                        "could not produce a vector for a pending memory; deferring this row"
                    );
                    defer_failed(outbox, item.id, attempts).await;
                }
                continue;
            }
        };

        let memory = MemoryEntry {
            id: item.id,
            content: item.content,
            embedding: vector,
            metadata: item.metadata,
            source: String::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            expires_at: None,
            score: None,
        };

        // Last line of defense immediately before Qdrant: `vector_for_pending`
        // already validates, but gate again so no future caller can feed an
        // invalid vector into the upsert/distance computation. With I63-B the
        // outbox poison is already resolved above, so this is defense in depth:
        // a non-finite vector is dead-lettered (never deferred); a merely
        // wrong-shaped vector keeps the transient retry path. Only the fixed
        // `error_kind` vocabulary is logged.
        if validate_embedding(&memory.embedding, embedding_dim).is_err() {
            if is_non_finite_vector(&memory.embedding) {
                dead_letter_retry(outbox, memory.id, attempts).await;
                continue;
            }
            warn!(
                id = %memory.id,
                attempts = attempts,
                error_kind = ErrorKind::Embedding.as_str(),
                "pending memory has an invalid vector; deferring this row"
            );
            defer_failed(outbox, memory.id, attempts).await;
            continue;
        }

        match sink.upsert(&memory).await {
            Ok(()) => {
                if let Err(error) = outbox.remove(memory.id).await {
                    warn!(
                        id = %memory.id,
                        error_kind = error_kind(&error, ErrorKind::Persistence),
                        "reconciled upsert succeeded but the pending row could not be deleted"
                    );
                }
            }
            Err(error) => {
                warn!(
                    id = %memory.id,
                    attempts = attempts,
                    error_kind = error_kind(&error, ErrorKind::Qdrant),
                    "reconciled Qdrant upsert failed; deferring the row to a later pass"
                );
                defer_failed(outbox, memory.id, attempts).await;
            }
        }
    }

    // I69-D: every terminal transition in this batch (dead-letter, exhausted
    // or removed) changes the pending/exhausted counts. Re-aggregate ONCE after
    // the loop rather than issuing a COUNT per row, so the gauges describe the
    // final state of the outbox. The seam is overridden by mock stores, which
    // makes the "mutations first, single refresh last" ordering verifiable
    // without a live PostgreSQL connection.
    outbox.observe_state_counts().await;
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
        // I55-A: fail closed at the core write boundary as well. Both
        // transports already reject these inputs at ingress with a bare 400 /
        // `InvalidArgument` through the shared validator; rechecking here also
        // covers direct callers that bypass an ingress. The returned
        // `InvalidInput` is mapped back to the transport's input error by
        // `internal_status` / `log_internal_error`, never to a 500.
        crate::config::validation::validate_memory_for_store(&memory, self.config.embedding_dim)?;
        // Store in PostgreSQL
        let client = self.pg_pool.get().await?;
        client.execute(
            "INSERT INTO memories (id, content, metadata, source, created_at, updated_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO UPDATE SET content = $2, metadata = $3, source = $4,
             updated_at = $6, expires_at = $7",
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

        // Store embedding in Qdrant. If that fails, queue the memory in
        // `pending_embeddings` so the next startup can reconcile it (P1-2).
        let sink = QdrantVectorSink {
            client: self.qdrant_client.clone(),
            collection: self.config.collection_name.clone(),
        };
        let outbox = PgPendingStore {
            pool: self.pg_pool.clone(),
        };
        store_memory_with_sinks(&memory, &sink, &outbox).await
    }

    /// Retry vector upserts queued after a Qdrant failure (P1-2).
    ///
    /// Called once from [`crate::ContextBuilder::new`] at startup. Reconcile
    /// never fails startup: a failed retry warns and leaves its row for the
    /// next start.
    pub(crate) async fn reconcile_pending_embeddings(&self) {
        let sink = QdrantVectorSink {
            client: self.qdrant_client.clone(),
            collection: self.config.collection_name.clone(),
        };
        let outbox = PgPendingStore {
            pool: self.pg_pool.clone(),
        };
        reconcile_pending(
            &outbox,
            &sink,
            Some(&self.embedding),
            self.config.embedding_dim,
        )
        .await;
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
        let embedding = match query_embedding_outcome(embed_result, self.config.embedding_dim)? {
            Some(embedding) => embedding,
            None => return Ok(Vec::new()),
        };

        // Defense in depth: re-gate the query vector with the configured
        // dimension immediately before the Qdrant search. `query_embedding_outcome`
        // already validates it, but this keeps the sink boundary safe even if a
        // future caller bypasses that helper. The `InvalidInput` keeps the
        // fixed 400 / `InvalidArgument` mapping at both transports.
        validate_search_vector(&embedding, self.config.embedding_dim)?;

        // Search Qdrant with an over-fetched limit (Round 19 B3): expired hits
        // must not consume the caller's top-K slots. Keep the score alongside
        // each id: the PostgreSQL refetch below only returns row columns and
        // cannot recover it (P1-1).
        let results = memory::search_vectors(
            &self.qdrant_client,
            &self.config.collection_name,
            &embedding,
            overfetch_limit(limit),
        )
        .await?;
        let ids: Vec<Uuid> = results.iter().map(|(id, _)| *id).collect();
        let scores: std::collections::HashMap<Uuid, Option<f32>> = results.into_iter().collect();

        // Fetch full entries from PostgreSQL, then reassemble in Qdrant's
        // relevance order (P1-1). Expired rows are excluded by SQL and filtered
        // again in Rust so the same UTC `<=` boundary holds on both sides.
        // Both gates run on the over-fetched pool so an expired hit cannot
        // crowd out a valid memory before the final trim (Round 19 B3).
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
                score: None,
            })
            .collect();
        // `ids` came from the over-fetched Qdrant result, so the reordered list
        // can be longer than the caller's limit. `select_top_valid` applies the
        // expiry gate and trims back to the requested result size.
        Ok(attach_scores(
            select_top_valid(reorder_by_ids(fetched, &ids), now, limit),
            &scores,
        ))
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
                score: None,
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

/// Estimated UTF-8 bytes per token used by the coarse budget check below.
///
/// This is a heuristic for typical (mostly ASCII / English) text, not a real
/// tokenizer count. The actual bytes-per-token ratio varies with the model,
/// language, and content, so the budget check is intentionally approximate.
const ESTIMATED_UTF8_BYTES_PER_TOKEN: usize = 4;

/// The observable outcome of applying the token budget. `None` means the
/// context fit and no section was dropped. When truncation happens, the report
/// carries only the dropped section names and the approximate serialized sizes
/// — never content — so it is safe both to log and to assert on in tests
/// without capturing `tracing` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TruncationReport {
    before_bytes: usize,
    after_bytes: usize,
}

/// Drop the memory arrays when the serialized context appears to exceed the
/// configured token budget.
///
/// The budget is only *estimated*: `str::len()` counts UTF-8 **bytes**, not
/// Unicode characters, and one token is approximated as roughly
/// [`ESTIMATED_UTF8_BYTES_PER_TOKEN`] bytes for typical text. A token is
/// therefore neither a character nor a byte: this is a coarse heuristic, not
/// a tokenizer, and non-ASCII text (e.g. Japanese, where a character commonly
/// occupies 3 UTF-8 bytes) will be over-counted.
fn truncate_context_to_tokens(context: Value, max_tokens: usize) -> Result<Value> {
    let (context, report) = apply_token_budget(context, max_tokens)?;
    if let Some(report) = report {
        warn!(
            before_bytes = report.before_bytes,
            after_bytes = report.after_bytes,
            sections = "memories,recent",
            "context exceeded estimated token budget; dropped memory sections"
        );
    }
    Ok(context)
}

/// Pure core of the token-budget step. Returns the (possibly trimmed) context
/// together with a [`TruncationReport`] when the memory arrays were emptied, or
/// `None` when the context was left untouched. Kept free of logging so the
/// truncation decision is directly testable.
fn apply_token_budget(
    mut context: Value,
    max_tokens: usize,
) -> Result<(Value, Option<TruncationReport>)> {
    let before_bytes = serde_json::to_string(&context)?.len();
    let budget_bytes = max_tokens.saturating_mul(ESTIMATED_UTF8_BYTES_PER_TOKEN);
    if before_bytes <= budget_bytes {
        return Ok((context, None));
    }

    context["memories"] = Value::Array(vec![]);
    context["recent"] = Value::Array(vec![]);
    let after_bytes = serde_json::to_string(&context)?.len();
    Ok((
        context,
        Some(TruncationReport {
            before_bytes,
            after_bytes,
        }),
    ))
}

pub(crate) mod memory {
    use super::*;
    use qdrant_client::qdrant::point_id::PointIdOptions;
    use qdrant_client::qdrant::{
        CreateCollectionBuilder, Distance, PointStruct, SearchPointsBuilder, UpsertPointsBuilder,
        VectorParamsBuilder,
    };
    use std::collections::HashMap;

    pub(crate) async fn create_pg_pool(
        config: &crate::config::ContextConfig,
    ) -> Result<deadpool_postgres::Pool> {
        let mut cfg = deadpool_postgres::Config::new();
        cfg.url = Some(config.pg_url().into());
        let pool = cfg.create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )?;

        // Run migrations (P1-3).
        //
        // sqlx already has the `migrate` feature enabled in the workspace
        // dependency (`runtime-tokio-rustls`, `postgres`, `migrate`), so no new
        // dependency is introduced. A short-lived `sqlx::PgConnection` is used
        // only for migration bookkeeping; the service keeps the deadpool pool
        // for all queries. `Pool::connect` is an inherent API, so no extra
        // trait import is needed here.
        //
        // Baseline for existing databases: deployments that previously ran
        // `batch_execute(001_initial.sql)` already own the 001 tables but have
        // no `_sqlx_migrations` table. On the first start of this build sqlx
        // replays 001; every statement there is `CREATE ... IF NOT EXISTS` or
        // `INSERT ... ON CONFLICT DO NOTHING`, so the replay is a no-op whose
        // only effect is recording version 1 as the baseline. The same applies
        // to 002 (the outbox table, also `IF NOT EXISTS`). No data is lost and
        // no manual step is required.
        //
        // Optional explicit baseline for operators who prefer to prepare the
        // database before deploying (requires sqlx-cli; otherwise let the
        // first startup do it as above):
        //   sqlx migrate baseline --version 1 \
        //     --source rust/migrations \
        //     --url "$JARVIS_CONTEXT_PG_URL"
        // Never delete rows from `_sqlx_migrations`: doing so makes sqlx replay
        // every migration and can break on already-existing versioned changes.
        let migration_pool = sqlx::PgPool::connect(config.pg_url()).await?;
        sqlx::migrate!("../migrations").run(&migration_pool).await?;
        drop(migration_pool);

        Ok(pool)
    }

    pub(crate) async fn create_qdrant_client(
        config: &crate::config::ContextConfig,
    ) -> Result<Qdrant> {
        // R4-DP2: never restore `skip_compatibility_check()`. The client's
        // default `check_compatibility = true` probes the server at build time
        // and reads its version; disabling that let a server-side API/version
        // change go completely unnoticed at startup.
        //
        // `check_connection()` is what makes a failed probe fail closed. In
        // qdrant-client 1.19.0 a compatibility probe that cannot reach the
        // server (or cannot obtain its version) is otherwise only `println!`ed,
        // so the service would still start. With this flag `build()` returns
        // `Err`, which is mapped below through `discard_qdrant_error` to a
        // fixed-vocabulary `ContextError` and propagated by
        // `ContextBuilder::new()` — startup fails closed without echoing the
        // client's endpoint-bearing error text.
        //
        // CI note (out-of-scope test, intentionally unchanged): the Docker
        // integration test in `tests/integration.rs` starts
        // `qdrant/qdrant:v1.12.4` while Cargo.lock pins qdrant-client 1.19.0.
        // That pair trips the client's minor-version mismatch warning (1.19 vs
        // 1.12). qdrant-client 1.19.0 only prints on a version mismatch, so the
        // ignored test still runs; only a connection failure is an error. If a
        // future client turns that warning into a hard error, the test image
        // must be bumped in a separate change.
        let mut client = Qdrant::from_url(&config.qdrant_url).check_connection();
        if let Some(key) = config.qdrant_api_key() {
            client = client.api_key(key);
        }
        Ok(discard_qdrant_error(client.build())?)
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
    /// upsert/search error. An unreadable dimension (`actual = None`) only
    /// warns and keeps starting in this pure helper, so older single-vector
    /// servers stay compatible (`unknown_existing_dimension_only_warns`).
    /// A *confirmed* multi-vector collection is no longer a valid reason to
    /// warn-and-continue: startup rejects it earlier in
    /// [`reject_multi_vector_collection`] (I51-C). Callers must pass `None`
    /// only for a *fully parsed* multi-vector response
    /// ([`inspected_dimension_from_info`], I49-C); a *failed inspection* or an
    /// unparsed response is a different case and is fail-closed in
    /// [`check_inspected_dimension`] (I47-C).
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

    /// Apply the I47-C dimension-verification policy to a `collection_info`
    /// outcome. `inspected` is normally produced by
    /// [`inspected_dimension_from_info`]: `Ok(Some(actual))` when an exact size
    /// was parsed, `Ok(None)` **only** for a fully parsed multi-vector
    /// collection, and `Err(())` when the call failed *or* the response could
    /// not be parsed (I49-C).
    ///
    /// Adopted policy: **fail closed on inspection failure**. The only reliable
    /// way to establish that an existing collection can serve the configured
    /// embedding dimension is a successful, parseable `collection_info`
    /// response. If that call fails (transport error, Qdrant 5xx) or its shape
    /// changes so the dimension cannot be read, starting anyway would defer a
    /// dimension mismatch to every subsequent upsert/search and leave the
    /// outbox retrying forever. Startup must therefore fail with an actionable
    /// error instead.
    ///
    /// The two successful-response outcomes keep their historical behavior
    /// *in this pure helper*: a readable mismatch is a hard error, while
    /// `Ok(None)` still warns and continues so
    /// `unknown_existing_dimension_only_warns` stays valid. At the startup
    /// level a confirmed multi-vector collection is rejected earlier by
    /// [`reject_multi_vector_collection`] (I51-C); this helper only classifies
    /// the inspection outcome.
    ///
    /// The raw Qdrant error is deliberately discarded at the call site: its
    /// `Display` can embed endpoints and response fragments. Only the fixed
    /// `error_kind` vocabulary and non-sensitive metadata (collection name and
    /// configured dimension) reach the log record or the returned error.
    pub(super) fn check_inspected_dimension(
        name: &str,
        embedding_dim: usize,
        inspected: std::result::Result<Option<u64>, ()>,
    ) -> Result<()> {
        match inspected {
            Ok(actual) => check_existing_dimension(name, embedding_dim, actual),
            Err(()) => {
                let kind = ErrorKind::Qdrant.as_str();
                error!(
                    collection = %name,
                    embedding_dim = embedding_dim,
                    error_kind = kind,
                    "could not inspect Qdrant collection dimension; failing closed"
                );
                anyhow::bail!(
                    "could not inspect Qdrant collection '{name}' to verify vector \
                     dimension (error_kind={kind}); refusing to start with an unverified \
                     embedding_dim={embedding_dim}"
                )
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

    /// Parse a `collection_info` response into the three-way input consumed by
    /// [`check_inspected_dimension`].
    ///
    /// The plain `Option` returned by [`configured_vector_size`] cannot tell a
    /// *confirmed* multi-vector collection from a *shape change* in the
    /// response: both produce `None` through its chain of `?`. `Ok(None)` must
    /// only mean the former (a confirmed multi-vector collection), so this helper
    /// first verifies that every field on the path is present and that a named
    /// vector map is non-empty. Anything else is "unparsed" and is mapped to
    /// `Err(())`, the same fail-closed branch used for a failed
    /// `collection_info` call (I49-C).
    ///
    /// - `Ok(Some(size))` — a single dense vector or a single named vector was
    ///   parsed; the exact dimension is known.
    /// - `Ok(None)` — the response was fully parsed and holds 2+ named vectors
    ///   (no single dimension to compare); its own warn-and-continue behavior
    ///   in [`check_existing_dimension`] is retained for the pure helpers, but
    ///   startup now fails closed via [`reject_multi_vector_collection`]
    ///   (I51-C / I53-A) because the upsert/search paths use an unnamed vector.
    ///   A single named vector yields `Ok(Some(size))` here, but is still
    ///   rejected by that guard at startup (I53-A).
    /// - `Err(())` — a field was missing, the named-vector map was empty, or
    ///   the response could not otherwise be interpreted; fail startup.
    pub(super) fn inspected_dimension_from_info(
        info: &qdrant_client::qdrant::CollectionInfo,
    ) -> std::result::Result<Option<u64>, ()> {
        use qdrant_client::qdrant::vectors_config::Config;

        let config = info.config.as_ref().ok_or(())?;
        let params = config.params.as_ref().ok_or(())?;
        let vectors = params.vectors_config.as_ref().ok_or(())?;
        match vectors.config.as_ref().ok_or(())? {
            // A single dense vector config always yields an exact size.
            Config::Params(_) => Ok(configured_vector_size(info)),
            // An empty named-vector map carries no usable dimension; that is
            // an unparseable response, not a confirmed multi-vector collection.
            Config::ParamsMap(params_map) if params_map.map.is_empty() => Err(()),
            // Fully parsed. `configured_vector_size` returns None here only
            // when the map holds 2+ named vectors (confirmed multi-vector).
            Config::ParamsMap(_) => Ok(configured_vector_size(info)),
        }
    }

    /// Reject a collection configured with named vectors (`Config::ParamsMap`,
    /// I51-C / I53-A).
    ///
    /// Adopted policy: **fail startup (fail-closed)**. Both write and read paths
    /// in this file address a single *unnamed* vector: [`upsert_vector`] builds
    /// its point with `PointStruct::new`, and [`search_vectors`] builds its query
    /// with `SearchPointsBuilder`. Qdrant rejects unnamed-vector points/queries
    /// against a named-vector collection — including one holding only a *single*
    /// named vector — so treating it as a healthy startup would make *every*
    /// memory save/search fail afterwards while the `pending_embeddings` outbox
    /// retries forever — a mismatch that is far cheaper to detect here than at
    /// runtime.
    ///
    /// The alternative policy — read a vector name from configuration and pass
    /// named vectors to upsert/search — would require a new config field and
    /// changes outside this file, so it is deliberately **not** adopted for this
    /// file-only change. A single *unnamed* vector collection (`Config::Params`)
    /// is unaffected, and the warn-and-continue path for an unreadable dimension
    /// ([`check_existing_dimension`], `unknown_existing_dimension_only_warns`) is
    /// left untouched: this guard fires for every non-empty named-vector map
    /// (`named >= 1`). An empty map carries no usable vector config and is
    /// classified as unparsed by [`inspected_dimension_from_info`], which fails
    /// closed through [`check_inspected_dimension`].
    ///
    /// As everywhere in this module, no raw response or error text is ever
    /// formatted. Only the fixed `error_kind` vocabulary, the collection name
    /// and the parsed (non-sensitive) vector count reach the log and the error.
    pub(super) fn reject_multi_vector_collection(
        name: &str,
        info: &qdrant_client::qdrant::CollectionInfo,
    ) -> Result<()> {
        use qdrant_client::qdrant::vectors_config::Config;
        let Some(config) = info.config.as_ref() else {
            return Ok(());
        };
        let Some(params) = config.params.as_ref() else {
            return Ok(());
        };
        let Some(vectors) = params.vectors_config.as_ref() else {
            return Ok(());
        };
        let Some(inner) = vectors.config.as_ref() else {
            return Ok(());
        };
        let Config::ParamsMap(params_map) = inner else {
            return Ok(());
        };
        // I53-A: `Config::ParamsMap` is named-vector mode regardless of the
        // number of entries. Even a single named vector makes Qdrant reject the
        // unnamed points/queries built by `upsert_vector`/`search_vectors`, so
        // every non-empty map (`named >= 1`) must fail startup. An empty map has
        // no usable vector config and is classified as unparsed by
        // `inspected_dimension_from_info`, which already fails closed; leave it
        // to that path.
        if params_map.map.is_empty() {
            return Ok(());
        }
        let named = params_map.map.len();
        let kind = ErrorKind::Qdrant.as_str();
        error!(
            collection = %name,
            named_vectors = named,
            error_kind = kind,
            "Qdrant collection uses named vectors; failing closed"
        );
        anyhow::bail!(
            "Qdrant collection '{name}' has {named} named vectors (error_kind={kind}); \
             this builder addresses a single unnamed vector and cannot upsert into or \
             search a named-vector collection; refusing to start"
        )
    }

    pub(crate) async fn ensure_collection(
        client: &Qdrant,
        name: &str,
        embedding_dim: usize,
    ) -> Result<()> {
        validate_embedding_dim(name, embedding_dim)?;

        let collections = discard_qdrant_error(client.list_collections().await)?;
        if !collections.collections.iter().any(|c| c.name == name) {
            let vectors = VectorParamsBuilder::new(embedding_dim as u64, Distance::Cosine);
            discard_qdrant_error(
                client
                    .create_collection(CreateCollectionBuilder::new(name).vectors_config(vectors))
                    .await,
            )?;
            // A successful create response is not proof that the collection can
            // serve the configured dimension. Fall through and read the real
            // size back below, using the same fail-closed path as a collection
            // that already existed (I49-C).
        }

        // Shared verification for both newly created and pre-existing
        // collections. Measure the real vector size and fail fast on a mismatch
        // rather than letting upsert/search fail with an opaque error. Without
        // a successful, parseable `collection_info` response we cannot verify
        // dimension compatibility, so `Builder::new` must not report a healthy
        // startup: a failed call, a missing `result` payload and an unparsed
        // response all map to `Err(())` and fail closed (I47-C / I49-C).
        let inspected = match client.collection_info(name).await {
            Ok(response) => match response.result.as_ref() {
                Some(info) => {
                    // I51-C / I53-A: any named-vector collection (even a single
                    // named vector) cannot be served by the unnamed-vector
                    // upsert and search paths below. Fail startup here instead
                    // of warning and continuing into a healthy-looking but
                    // fully broken runtime with an endlessly retrying outbox.
                    reject_multi_vector_collection(name, info)?;
                    inspected_dimension_from_info(info)
                }
                None => Err(()),
            },
            // Drop the raw error: its Display can embed endpoints and response
            // fragments, which must never reach logs. The failure kind is
            // conveyed by the fixed `error_kind` vocabulary in the helper.
            Err(_) => Err(()),
        };
        check_inspected_dimension(name, embedding_dim, inspected)
    }

    pub(crate) async fn upsert_vector(
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
        discard_qdrant_error(
            client
                .upsert_points(UpsertPointsBuilder::new(collection, vec![point]))
                .await,
        )?;
        Ok(())
    }

    /// Extract a UUID string from a Qdrant scored point id.
    ///
    /// A missing `point_id`, missing `point_id_options` or a numeric id cannot
    /// match PostgreSQL's `uuid` column, so all three map to `None` and the
    /// point is skipped instead of becoming a fabricated nil/random id.
    fn scored_point_id(scored: &qdrant_client::qdrant::ScoredPoint) -> Option<String> {
        match scored
            .id
            .as_ref()
            .and_then(|id| id.point_id_options.as_ref())
        {
            Some(PointIdOptions::Uuid(u)) => Some(u.to_string()),
            Some(PointIdOptions::Num(_)) | None => None,
        }
    }

    /// Return Qdrant hits as `(memory id, optional similarity score)` pairs in
    /// relevance order. The score is captured here because the later PostgreSQL
    /// refetch only returns row columns and cannot reconstruct it (P1-1). An id
    /// without a usable score keeps `None` instead of a fabricated `0.0`.
    pub(crate) async fn search_vectors(
        client: &Qdrant,
        collection: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(Uuid, Option<f32>)>> {
        let response = discard_qdrant_error(
            client
                .search_points(
                    SearchPointsBuilder::new(collection, embedding.to_vec(), limit as u64)
                        .with_payload(true),
                )
                .await,
        )?;

        let scores: HashMap<Uuid, Option<f32>> = response
            .result
            .iter()
            .filter_map(|scored| {
                let id = parse_point_id(scored_point_id(scored).as_deref())?;
                Some((id, Some(scored.score)))
            })
            .collect();
        let points = response
            .result
            .into_iter()
            .map(|scored| (scored_point_id(&scored), scored.payload))
            .collect();

        // Reuse the same validation as the entry path so a skipped point never
        // contributes a foreign score to a valid id.
        Ok(entries_from_raw_points(points)
            .into_iter()
            .map(|entry| (entry.id, scores.get(&entry.id).copied().flatten()))
            .collect())
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

    #[test]
    fn truncation_report_is_none_when_context_fits_budget() {
        let context = json!({"memories": [1, 2], "recent": [3]});
        let before = serde_json::to_string(&context).expect("serialize").len();
        let (output, report) = apply_token_budget(context.clone(), 1000).expect("budget");

        assert_eq!(output, context, "a fitting context must not be modified");
        assert_eq!(report, None, "no truncation must produce no report");
        assert!(before <= 1000 * ESTIMATED_UTF8_BYTES_PER_TOKEN);
    }

    #[test]
    fn truncation_report_records_only_sections_and_approximate_sizes() {
        let secret = "alice@example.com";
        let context = json!({
            "memories": [{"content": format!("contact {secret}")}],
            "recent": [{"content": "y".repeat(100)}],
            "event": "state_changed",
        });
        let (output, report) = apply_token_budget(context, 1).expect("budget");
        let report = report.expect("an oversized context must be reported");

        assert_eq!(output["memories"], json!([]));
        assert_eq!(output["recent"], json!([]));
        assert_eq!(output["event"], "state_changed");

        let budget_bytes = ESTIMATED_UTF8_BYTES_PER_TOKEN;
        assert!(
            report.before_bytes > budget_bytes,
            "truncation triggers only above the estimated budget"
        );
        assert!(
            report.after_bytes < report.before_bytes,
            "emptying the memory arrays must shrink the serialized context"
        );

        // The report must carry no payload content (including PII).
        let report_debug = format!("{report:?}");
        assert!(!report_debug.contains(secret));
        assert!(!report_debug.contains("state_changed"));
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
            score: None,
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
        let err = query_embedding_outcome(Some(failed), 3)
            .expect_err("a configured endpoint that fails must produce an error");
        let message = format!("{err:#}");
        assert!(message.contains("query embedding"), "{message}");
        assert!(message.contains("HTTP 500"), "{message}");
    }

    #[test]
    fn tc_e_disabled_embedding_is_an_explicit_skip() {
        let outcome =
            query_embedding_outcome(None, 3).expect("disabled endpoint is a documented skip");
        assert_eq!(outcome, None);
    }

    #[test]
    fn tc_e_successful_embedding_is_passed_through() {
        let vector = vec![0.1_f32, 0.2, 0.3];
        let outcome = query_embedding_outcome(Some(Ok(vector.clone())), 3).expect("embed succeeds");
        assert_eq!(outcome, Some(vector));
    }

    /// Non-finite query embeddings (including an f64 that saturated to f32
    /// infinity) must be rejected before the vector can reach `search_vectors`
    /// and Qdrant's distance computation. Pure: no DB or Qdrant is involved.
    /// The rejection is a shared `InvalidInput`, so the fixed 400 /
    /// `InvalidArgument` vocabulary is preserved.
    #[test]
    fn tc_e_non_finite_query_embedding_is_rejected() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e100_f64 as f32] {
            let err = query_embedding_outcome(Some(Ok(vec![bad, 0.2, 0.3])), 3)
                .expect_err("a non-finite query embedding must not reach search");
            assert!(
                err.downcast_ref::<crate::config::validation::InvalidInput>()
                    .is_some(),
                "the fixed InvalidInput contract must be preserved: {err:#}"
            );
        }
    }

    /// The gate immediately before `search_vectors` rejects non-finite vectors
    /// with the configured dimension, without needing a Qdrant connection.
    #[test]
    fn search_vector_gate_rejects_non_finite_vectors() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e100_f64 as f32] {
            let err = validate_search_vector(&[bad, 0.2, 0.3], 3)
                .expect_err("a non-finite search vector must not reach Qdrant");
            assert!(
                err.downcast_ref::<crate::config::validation::InvalidInput>()
                    .is_some(),
                "the fixed InvalidInput contract must be preserved: {err:#}"
            );
        }
        assert!(validate_search_vector(&[0.1, 0.2, 0.3], 3).is_ok());
        assert!(validate_search_vector(&[0.1, 0.2], 3).is_err());
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

    // --- Round 19 B3: expired Qdrant hits must not occupy top-K slots ---
    // Pure tests only: no live Qdrant/PostgreSQL, no unsafe code.

    #[test]
    fn tc_g_overfetch_limit_widens_the_candidate_pool() {
        assert_eq!(overfetch_limit(0), 0);
        assert!(overfetch_limit(1) > 1);
        assert_eq!(overfetch_limit(2), 2 * EXPIRY_OVERFETCH_FACTOR);
        assert!(overfetch_limit(10) > 10);
    }

    #[test]
    fn tc_g_expired_top_hits_do_not_consume_the_requested_slots() {
        let now = fixed_now();
        let expired_boundary = entry_with_expiry(Some(now));
        let expired_past = entry_with_expiry(Some(now - chrono::Duration::seconds(1)));
        let valid_future = entry_with_expiry(Some(now + chrono::Duration::seconds(30)));
        let valid_never = entry_with_expiry(None);

        // Without over-fetch, the two expired hits are the whole top-2 Qdrant
        // result; after the expiry gate there is nothing left for the caller.
        let narrow = vec![expired_boundary.clone(), expired_past.clone()];
        assert!(
            select_top_valid(narrow, now, 2).is_empty(),
            "an expired top hit must never be returned"
        );

        // Over-fetch lets the same top-2 request reach valid memories ranked
        // immediately below the expired ones.
        let fetch = overfetch_limit(2);
        let ranked = vec![
            expired_boundary,
            expired_past,
            valid_future.clone(),
            valid_never.clone(),
        ];
        assert_eq!(
            ranked.len(),
            fetch,
            "the model uses the full over-fetch pool"
        );
        let selected = select_top_valid(ranked, now, 2);
        assert_eq!(
            selected.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![valid_future.id, valid_never.id],
            "valid memories must fill the slots freed by expired hits"
        );
        assert!(
            selected.iter().all(|e| !is_expired(e.expires_at, now)),
            "every returned memory must still be valid at now"
        );
    }

    #[test]
    fn tc_g_final_result_never_exceeds_the_requested_limit() {
        let now = fixed_now();
        let ranked: Vec<_> = (0..6)
            .map(|_| entry_with_expiry(Some(now + chrono::Duration::seconds(30))))
            .collect();
        let selected = select_top_valid(ranked, now, 2);
        assert_eq!(
            selected.len(),
            2,
            "over-fetch must be trimmed back to limit"
        );
    }

    #[test]
    fn configured_embedding_dim_defaults_to_384() {
        assert_eq!(crate::config::ContextConfig::default().embedding_dim, 384);
    }

    /// I55-A: the core store re-checks the same required fields as the shared
    /// ingress validator, so a direct caller cannot push a wrong-sized vector
    /// or a non-object payload into Qdrant.
    #[test]
    fn store_boundary_rejects_invalid_required_fields() {
        let mut memory = entry_with_expiry(None);
        memory.embedding = vec![0.1_f32; 3];
        assert!(crate::config::validation::validate_memory_for_store(&memory, 3).is_ok());

        memory.embedding.clear();
        assert_eq!(
            crate::config::validation::validate_memory_for_store(&memory, 3)
                .expect_err("an empty vector is never a valid embedding")
                .field,
            "embedding"
        );

        memory.embedding = vec![0.1_f32; 3];
        memory.metadata = json!("text");
        assert_eq!(
            crate::config::validation::validate_memory_for_store(&memory, 3)
                .expect_err("a non-object metadata produces an empty Qdrant payload")
                .field,
            "metadata"
        );
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
    fn failed_dimension_inspection_fails_closed() {
        // I47-C: a `collection_info` failure must not be recorded as a
        // successful compatibility check. The raw Qdrant error is discarded by
        // the seam, so only the fixed `error_kind` vocabulary and non-sensitive
        // metadata (collection + configured dimension) can reach the caller.
        let err = memory::check_inspected_dimension("jarvis_memories", 384, Err(()))
            .expect_err("an unverifiable collection must fail startup");
        let message = format!("{err:#}");
        assert!(
            message.contains("jarvis_memories"),
            "the error must name the collection: {message}"
        );
        assert!(
            message.contains("error_kind=qdrant"),
            "the error must carry the fixed vocabulary token: {message}"
        );
        assert!(
            message.contains("384"),
            "the error must report the configured dimension: {message}"
        );
        assert!(
            !message.contains("http") && !message.contains("response"),
            "no endpoint or response fragment may leak: {message}"
        );
    }

    #[test]
    fn inspected_dimension_preserves_successful_response_outcomes() {
        // A readable matching dimension is accepted.
        assert!(memory::check_inspected_dimension("jarvis_memories", 384, Ok(Some(384))).is_ok());
        // A readable mismatch is still a hard error.
        assert!(memory::check_inspected_dimension("jarvis_memories", 384, Ok(Some(768))).is_err());
        // An unreadable dimension keeps the historical warn-only behavior.
        assert!(memory::check_inspected_dimension("jarvis_memories", 384, Ok(None)).is_ok());
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

    #[test]
    fn unparsed_collection_info_response_fails_closed() {
        use qdrant_client::qdrant::vectors_config::Config;
        use qdrant_client::qdrant::{
            CollectionConfig, CollectionInfo, CollectionParams, VectorParams, VectorParamsMap,
            VectorsConfig,
        };

        // A missing `config` field (e.g. an upstream response-shape change)
        // must NOT be mistaken for a confirmed multi-vector collection.
        let no_config = memory::inspected_dimension_from_info(&CollectionInfo::default());
        assert_eq!(no_config, Err(()), "an empty response is unparsed");
        let err = memory::check_inspected_dimension("jarvis_memories", 384, no_config)
            .expect_err("an unparsed response must fail startup like a failed call");
        let message = format!("{err:#}");
        assert!(message.contains("error_kind=qdrant"), "{message}");
        assert!(message.contains("jarvis_memories"), "{message}");
        assert!(message.contains("384"), "{message}");
        assert!(
            !message.contains("http") && !message.contains("response"),
            "no endpoint or response fragment may leak: {message}"
        );

        // Every intermediate field on the parse path is required. Missing any
        // one of them is "unparsed", not the historical warn-only case.
        let no_params = CollectionInfo {
            config: Some(CollectionConfig::default()),
            ..Default::default()
        };
        assert_eq!(memory::inspected_dimension_from_info(&no_params), Err(()));

        let no_vectors_config = CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            memory::inspected_dimension_from_info(&no_vectors_config),
            Err(())
        );

        let no_inner_config = CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            memory::inspected_dimension_from_info(&no_inner_config),
            Err(())
        );

        // An empty named-vector map has no usable dimension: unparsed.
        let empty_map = CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig {
                        config: Some(Config::ParamsMap(VectorParamsMap::default())),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(memory::inspected_dimension_from_info(&empty_map), Err(()));

        // Over-correction guard: a fully parsed multi-vector collection keeps
        // the historical warn-only behavior (`Ok(None)` -> startup succeeds).
        let multi = CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig {
                        config: Some(Config::ParamsMap(VectorParamsMap {
                            map: HashMap::from([
                                (
                                    "a".to_string(),
                                    VectorParams {
                                        size: 384,
                                        ..Default::default()
                                    },
                                ),
                                (
                                    "b".to_string(),
                                    VectorParams {
                                        size: 768,
                                        ..Default::default()
                                    },
                                ),
                            ]),
                        })),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(memory::inspected_dimension_from_info(&multi), Ok(None));
        assert!(
            memory::check_inspected_dimension("jarvis_memories", 384, Ok(None)).is_ok(),
            "confirmed multi-vector collections must keep the warn-only path"
        );
    }

    #[test]
    fn newly_created_collection_dimension_is_read_back_and_verified() {
        use qdrant_client::qdrant::vectors_config::Config;
        use qdrant_client::qdrant::{
            CollectionConfig, CollectionInfo, CollectionParams, VectorParams, VectorParamsMap,
            VectorsConfig,
        };

        // `ensure_collection` no longer trusts the create response: it calls
        // `collection_info` for the new collection and verifies the real size
        // through `inspected_dimension_from_info` + `check_inspected_dimension`,
        // the same fail-closed path used for pre-existing collections.
        let info_for = |size| CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig {
                        config: Some(Config::Params(VectorParams {
                            size,
                            ..Default::default()
                        })),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Real dimension matches the configured embedding_dim: startup OK.
        let matching = memory::inspected_dimension_from_info(&info_for(384));
        assert_eq!(matching, Ok(Some(384)));
        assert!(memory::check_inspected_dimension("jarvis_memories", 384, matching).is_ok());

        // The read-back size differs from the configured embedding_dim: even
        // for a freshly created collection this must fail startup.
        let mismatched = memory::inspected_dimension_from_info(&info_for(768));
        assert_eq!(mismatched, Ok(Some(768)));
        let err = memory::check_inspected_dimension("jarvis_memories", 384, mismatched)
            .expect_err("a wrong real dimension must fail startup even after create");
        let message = format!("{err:#}");
        assert!(
            message.contains("768") && message.contains("384"),
            "{message}"
        );

        // A single named vector also yields an exact, verified size.
        let named = CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig {
                        config: Some(Config::ParamsMap(VectorParamsMap {
                            map: HashMap::from([(
                                "dense".to_string(),
                                VectorParams {
                                    size: 384,
                                    ..Default::default()
                                },
                            )]),
                        })),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(memory::inspected_dimension_from_info(&named), Ok(Some(384)));
    }

    // --- I51-C: confirmed multi-vector collections must fail startup ---

    /// Build a `collection_info` response with `count` equally sized named
    /// vectors. Pure data, no live Qdrant involved.
    fn named_vector_info(names: &[&str]) -> qdrant_client::qdrant::CollectionInfo {
        use qdrant_client::qdrant::vectors_config::Config;
        use qdrant_client::qdrant::{
            CollectionConfig, CollectionInfo, CollectionParams, VectorParams, VectorParamsMap,
            VectorsConfig,
        };
        CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig {
                        config: Some(Config::ParamsMap(VectorParamsMap {
                            map: names
                                .iter()
                                .map(|name| {
                                    (
                                        (*name).to_string(),
                                        VectorParams {
                                            size: 384,
                                            ..Default::default()
                                        },
                                    )
                                })
                                .collect(),
                        })),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn multi_vector_collection_fails_closed_at_startup() {
        let multi = named_vector_info(&["text", "image"]);

        // Still "confirmed multi-vector" at the inspection layer (I49-C)...
        assert_eq!(memory::inspected_dimension_from_info(&multi), Ok(None));

        // ... but startup must now reject it instead of warn+continue.
        let err = memory::reject_multi_vector_collection("jarvis_memories", &multi)
            .expect_err("a 2+ named-vector collection cannot be served by unnamed vectors");
        let message = format!("{err:#}");
        assert!(message.contains("jarvis_memories"), "{message}");
        assert!(message.contains("error_kind=qdrant"), "{message}");
        assert!(
            message.contains('2'),
            "the parsed vector count is reported: {message}"
        );
        assert!(
            !message.contains("http") && !message.contains("response"),
            "no endpoint or response fragment may leak: {message}"
        );
    }

    #[test]
    fn single_vector_collections_are_not_rejected_by_multi_vector_guard() {
        use qdrant_client::qdrant::vectors_config::Config;
        use qdrant_client::qdrant::{
            CollectionConfig, CollectionInfo, CollectionParams, VectorParams, VectorsConfig,
        };

        // Historical name: since I53-A a single *named* vector IS rejected (see
        // `single_named_vector_collection_fails_closed_at_startup`). What still
        // holds is that this guard leaves the non-named cases alone: a single
        // unnamed vector and an unparsed response.

        // A single unnamed vector (the normal collection created here).
        let dense = CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig {
                        config: Some(Config::Params(VectorParams {
                            size: 384,
                            ..Default::default()
                        })),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(memory::reject_multi_vector_collection("jarvis_memories", &dense).is_ok());

        // I53-A: a single named vector is still named-vector mode. Qdrant
        // rejects unnamed points/queries against it, so the guard must reject.
        let single_named = named_vector_info(&["dense"]);
        assert!(
            memory::reject_multi_vector_collection("jarvis_memories", &single_named).is_err(),
            "a single named vector must fail closed (I53-A)"
        );

        // An unparsed response must not be swallowed by this guard; it stays a
        // fail-closed error through `check_inspected_dimension` (I47-C/I49-C).
        assert!(
            memory::reject_multi_vector_collection(
                "jarvis_memories",
                &qdrant_client::qdrant::CollectionInfo::default()
            )
            .is_ok()
        );

        // The single-vector unreadable-dimension warn path is unchanged.
        assert!(
            memory::check_existing_dimension("jarvis_memories", 384, None).is_ok(),
            "single-vector unknown dimension keeps warn-and-continue"
        );
    }

    #[test]
    fn single_named_vector_collection_fails_closed_at_startup() {
        // I53-A: one named vector is enough. `Config::ParamsMap` is named-vector
        // mode even with a single entry, and the builder's unnamed upsert/search
        // paths cannot serve it, so startup must fail closed with the same fixed
        // vocabulary used for the 2+ named-vector case.
        let single = named_vector_info(&["dense"]);

        // The pure inspection layer still reports the exact size of the single
        // named vector (that classification is unchanged); it is the startup
        // guard that rejects it now.
        assert_eq!(
            memory::inspected_dimension_from_info(&single),
            Ok(Some(384))
        );

        let err = memory::reject_multi_vector_collection("jarvis_memories", &single)
            .expect_err("a single named vector cannot be served by unnamed vectors");
        let message = format!("{err:#}");
        assert!(message.contains("jarvis_memories"), "{message}");
        assert!(message.contains("error_kind=qdrant"), "{message}");
        assert!(
            message.contains("1 named vector"),
            "the parsed vector count is reported: {message}"
        );
        assert!(
            !message.contains("http") && !message.contains("response"),
            "no endpoint or response fragment may leak: {message}"
        );
        assert!(
            !message.contains("dense"),
            "named-vector identifiers must not leak: {message}"
        );
    }

    #[tokio::test]
    async fn unreachable_qdrant_fails_client_creation_fail_closed() {
        // No live Qdrant is needed: nothing listens on this loopback port, so
        // the startup compatibility probe must fail instead of being skipped
        // (R4-DP2). This guards the fail-closed behavior without Docker.
        let config = crate::config::ContextConfig {
            qdrant_url: "http://127.0.0.1:6999".into(),
            ..Default::default()
        };

        // `expect_err` would require `Qdrant: Debug`, so match explicitly.
        let err = match memory::create_qdrant_client(&config).await {
            Ok(_) => panic!("an unverifiable Qdrant server must fail startup"),
            Err(err) => err,
        };
        let message = format!("{err:#}");
        assert!(
            message.contains("Qdrant"),
            "the error must name the failing dependency: {message}"
        );
    }

    // --- P1-2 outbox / reconcile regression harness (no live services) ---

    /// Scripted Qdrant sink. `true` = successful upsert, `false` = injected
    /// failure. No network is touched, so the failure is deterministic.
    struct ScriptedSink {
        results: std::sync::Mutex<Vec<bool>>,
    }

    impl ScriptedSink {
        fn new(results: Vec<bool>) -> Self {
            Self {
                results: std::sync::Mutex::new(results),
            }
        }
    }

    #[async_trait::async_trait]
    impl VectorSink for ScriptedSink {
        async fn upsert(&self, _memory: &MemoryEntry) -> Result<()> {
            let ok = self.results.lock().expect("script lock").remove(0);
            if ok {
                Ok(())
            } else {
                anyhow::bail!("injected Qdrant upsert failure")
            }
        }
    }

    /// In-memory stand-in for the `pending_embeddings` table.
    ///
    /// `respect_backoff` selects whether `list` applies the same
    /// `next_retry_at IS NULL OR next_retry_at <= NOW()` gate as the production
    /// SQL. The pre-existing reconcile tests deliberately keep it off: they
    /// assert the retry/removal mechanics and immediately run a second pass,
    /// which would not be due under a real clock. The head-of-line regression
    /// test turns it on to exercise the actual selection policy.
    #[derive(Default)]
    struct MemoryOutbox {
        rows: std::sync::Mutex<Vec<PendingEmbedding>>,
        respect_backoff: bool,
    }

    impl MemoryOutbox {
        fn with_rows(rows: Vec<PendingEmbedding>) -> Self {
            Self {
                rows: std::sync::Mutex::new(rows),
                respect_backoff: false,
            }
        }

        fn with_backoff_filter(rows: Vec<PendingEmbedding>) -> Self {
            Self {
                rows: std::sync::Mutex::new(rows),
                respect_backoff: true,
            }
        }

        fn count(&self) -> usize {
            self.rows.lock().expect("outbox lock").len()
        }

        fn snapshot(&self) -> Vec<PendingEmbedding> {
            self.rows.lock().expect("outbox lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl PendingStore for MemoryOutbox {
        async fn record(&self, memory: &MemoryEntry, embedding: &[f32]) -> Result<()> {
            self.rows
                .lock()
                .expect("outbox lock")
                .push(PendingEmbedding {
                    id: memory.id,
                    content: memory.content.clone(),
                    metadata: memory.metadata.clone(),
                    embedding: embedding.to_vec(),
                    attempts: 0,
                    next_retry_at: None,
                    vector_state: "pending",
                });
            Ok(())
        }

        async fn list(&self, limit: usize) -> Result<Vec<PendingEmbedding>> {
            let now = Utc::now();
            // Mirror both production SQL gates: exhausted rows never enter a
            // batch (I61-B), deferred rows only when due (I30-C).
            Ok(self
                .rows
                .lock()
                .expect("outbox lock")
                .iter()
                .filter(|row| row.attempts < MAX_RECONCILE_ATTEMPTS)
                .filter(|row| !self.respect_backoff || is_retry_due(row.next_retry_at, now))
                .take(limit)
                .cloned()
                .collect())
        }

        async fn remove(&self, id: Uuid) -> Result<()> {
            self.rows
                .lock()
                .expect("outbox lock")
                .retain(|row| row.id != id);
            Ok(())
        }

        async fn mark_failed(
            &self,
            id: Uuid,
            attempts: i32,
            next_retry_at: DateTime<Utc>,
        ) -> Result<()> {
            let mut rows = self.rows.lock().expect("outbox lock");
            if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
                row.attempts = attempts;
                row.next_retry_at = Some(next_retry_at);
            }
            Ok(())
        }

        async fn mark_exhausted(&self, id: Uuid) -> Result<()> {
            let mut rows = self.rows.lock().expect("outbox lock");
            if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
                row.vector_state = "exhausted";
            }
            Ok(())
        }
    }

    /// A row plus the terminal decision a production `dead_letter` writes. The
    /// PostgreSQL table stores `dead_lettered_at`/`vector_state` as columns; the
    /// test store keeps them beside the row so retention and the selection gate
    /// are both observable without a database.
    #[derive(Clone, Debug)]
    struct RetainedRow {
        pending: PendingEmbedding,
        dead_lettered_at: Option<DateTime<Utc>>,
        vector_state: &'static str,
    }

    /// Persistence-mirroring outbox for the Round 61 / migration 005 policy:
    /// `dead_letter` keeps the row and marks it, `list` applies the same
    /// `dead_lettered_at IS NULL` exclusion as `LIST_PENDING_SQL`. This is the
    /// seam the behavioural gate tests use instead of matching SQL text.
    struct RetainingOutbox {
        rows: std::sync::Mutex<Vec<RetainedRow>>,
    }

    impl RetainingOutbox {
        fn new(rows: Vec<PendingEmbedding>) -> Self {
            Self {
                rows: std::sync::Mutex::new(
                    rows.into_iter()
                        .map(|pending| RetainedRow {
                            pending,
                            dead_lettered_at: None,
                            vector_state: "pending",
                        })
                        .collect(),
                ),
            }
        }

        fn snapshot(&self) -> Vec<RetainedRow> {
            self.rows.lock().expect("outbox lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl PendingStore for RetainingOutbox {
        async fn record(&self, _memory: &MemoryEntry, _embedding: &[f32]) -> Result<()> {
            Ok(())
        }

        async fn list(&self, limit: usize) -> Result<Vec<PendingEmbedding>> {
            let now = Utc::now();
            Ok(self
                .rows
                .lock()
                .expect("outbox lock")
                .iter()
                .filter(|row| row.dead_lettered_at.is_none())
                .filter(|row| row.pending.attempts < MAX_RECONCILE_ATTEMPTS)
                .filter(|row| is_retry_due(row.pending.next_retry_at, now))
                .take(limit)
                .map(|row| row.pending.clone())
                .collect())
        }

        async fn remove(&self, id: Uuid) -> Result<()> {
            self.rows
                .lock()
                .expect("outbox lock")
                .retain(|row| row.pending.id != id);
            Ok(())
        }

        async fn mark_failed(
            &self,
            id: Uuid,
            attempts: i32,
            next_retry_at: DateTime<Utc>,
        ) -> Result<()> {
            let mut rows = self.rows.lock().expect("outbox lock");
            if let Some(row) = rows.iter_mut().find(|row| row.pending.id == id) {
                row.pending.attempts = attempts;
                row.pending.next_retry_at = Some(next_retry_at);
            }
            Ok(())
        }

        async fn dead_letter(&self, id: Uuid, at: DateTime<Utc>) -> Result<()> {
            let mut rows = self.rows.lock().expect("outbox lock");
            let row = rows
                .iter_mut()
                .find(|row| row.pending.id == id)
                .expect("dead-letter target must still be retained");
            row.dead_lettered_at = Some(at);
            row.vector_state = "dead_lettered";
            Ok(())
        }
    }

    fn pending_entry() -> PendingEmbedding {
        PendingEmbedding {
            id: Uuid::new_v4(),
            content: "queued after a Qdrant failure".into(),
            metadata: json!({ "kind": "outbox-regression" }),
            embedding: vec![0.1; 384],
            attempts: 0,
            next_retry_at: None,
            vector_state: "pending",
        }
    }

    fn disabled_embedder() -> crate::embedding::EmbeddingClient {
        let config = crate::config::ContextConfig::default();
        let client = crate::embedding::EmbeddingClient::from_config(&config)
            .expect("client builds without a configured endpoint");
        assert!(!client.is_enabled());
        client
    }

    #[tokio::test]
    async fn qdrant_failure_queues_memory_in_outbox() {
        let memory = entry_with_expiry(None);
        let outbox = MemoryOutbox::default();
        let sink = ScriptedSink::new(vec![false]);

        let err = store_memory_with_sinks(&memory, &sink, &outbox)
            .await
            .expect_err("an injected Qdrant failure must surface");
        assert!(format!("{err:#}").contains("injected Qdrant upsert failure"));

        let queued = outbox.list(10).await.expect("list outbox");
        assert_eq!(queued.len(), 1, "the failed upsert must be queued");
        assert_eq!(queued[0].id, memory.id);
        assert_eq!(queued[0].content, memory.content);
        assert_eq!(queued[0].metadata, memory.metadata);
    }

    #[tokio::test]
    async fn successful_upsert_does_not_queue_memory() {
        let memory = entry_with_expiry(None);
        let outbox = MemoryOutbox::default();
        let sink = ScriptedSink::new(vec![true]);

        store_memory_with_sinks(&memory, &sink, &outbox)
            .await
            .expect("upsert succeeds");
        assert_eq!(outbox.count(), 0, "no failure means nothing to reconcile");
    }

    #[tokio::test]
    async fn reconcile_removes_pending_row_only_after_success() {
        let pending = pending_entry();
        let outbox = MemoryOutbox::with_rows(vec![pending.clone()]);
        let embedder = disabled_embedder();
        let dim = pending.embedding.len();

        // First pass: Qdrant still fails, the row must survive.
        reconcile_pending(
            &outbox,
            &ScriptedSink::new(vec![false]),
            Some(&embedder),
            dim,
        )
        .await;
        assert_eq!(outbox.count(), 1, "a failed retry must remain queued");

        // Second pass: upsert succeeds and the row is deleted.
        reconcile_pending(
            &outbox,
            &ScriptedSink::new(vec![true]),
            Some(&embedder),
            dim,
        )
        .await;
        assert_eq!(outbox.count(), 0, "a successful retry must clear the row");
    }

    /// The dimension-match replay branch of `vector_for_pending` must validate
    /// the stored vector. A non-finite component (including an f32 saturation)
    /// is rejected with the shared `InvalidInput` instead of being replayed into
    /// Qdrant. Mock embedder only; no endpoint, DB or Qdrant is touched.
    #[tokio::test]
    async fn vector_for_pending_rejects_non_finite_stored_vector() {
        let embedder = disabled_embedder();
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e100_f64 as f32] {
            let mut pending = pending_entry();
            pending.embedding[0] = bad;
            let err = vector_for_pending(&pending, Some(&embedder), 384)
                .await
                .expect_err("a non-finite stored vector must not be replayed");
            assert!(
                err.downcast_ref::<crate::config::validation::InvalidInput>()
                    .is_some(),
                "the fixed InvalidInput contract must be preserved: {err:#}"
            );
        }
    }

    /// Round 61 / migration 005: dead-lettering is an audit decision, not a
    /// delete. The persistence-mirroring store must keep the row, write both
    /// terminal columns, and exclude it from every later `list` while leaving
    /// live rows selectable. No PostgreSQL/Qdrant connection is involved.
    #[tokio::test]
    async fn dead_letter_retains_row_with_markers_and_hides_it_from_list() {
        let poisoned = pending_entry();
        let poisoned_id = poisoned.id;
        let live = pending_entry();
        let live_id = live.id;
        let outbox = RetainingOutbox::new(vec![poisoned, live]);

        assert_eq!(
            outbox
                .list(10)
                .await
                .expect("list before dead-letter")
                .len(),
            2,
            "both rows must be selectable before the terminal decision"
        );

        outbox
            .dead_letter(poisoned_id, Utc::now())
            .await
            .expect("dead-letter must succeed");

        let rows = outbox.snapshot();
        assert_eq!(rows.len(), 2, "no row may be deleted by dead-letter");
        let terminal = rows
            .iter()
            .find(|row| row.pending.id == poisoned_id)
            .expect("the dead-lettered row must still be retained");
        assert!(
            terminal.dead_lettered_at.is_some(),
            "dead_lettered_at must be persisted for audit"
        );
        assert_eq!(
            terminal.vector_state, "dead_lettered",
            "vector_state must record the terminal decision"
        );

        let after = outbox.list(10).await.expect("list after dead-letter");
        assert_eq!(after.len(), 1, "only the live row may still be selected");
        assert_eq!(after[0].id, live_id, "the live row must remain selectable");
        assert!(
            !after.iter().any(|row| row.id == poisoned_id),
            "the dead-lettered id must never be returned again"
        );
    }

    /// I61-B / I63-B / I63-A: the stored vector is 384-dim, so
    /// `vector_for_pending` only re-validates it and never re-embeds. This poison
    /// is genuinely unrecoverable, so the terminal decision must RETAIN the row
    /// with audit columns and the selection gate must hide it from every later
    /// batch — never delete it. The persistence-mirroring outbox and mock sink
    /// keep this free of live DB/Qdrant access; an empty sink script means any
    /// upsert attempt would panic.
    #[tokio::test]
    async fn reconcile_dead_letters_pending_row_with_non_finite_vector() {
        let mut pending = pending_entry();
        pending.embedding[0] = f32::INFINITY;
        let id = pending.id;
        let outbox = RetainingOutbox::new(vec![pending]);
        let embedder = disabled_embedder();

        reconcile_pending(&outbox, &ScriptedSink::new(vec![]), Some(&embedder), 384).await;

        let rows = outbox.snapshot();
        assert_eq!(rows.len(), 1, "the poisoned row must be retained: {rows:?}");
        assert_eq!(rows[0].pending.id, id);
        assert!(
            rows[0].dead_lettered_at.is_some(),
            "the terminal decision must persist dead_lettered_at"
        );
        assert_eq!(
            rows[0].vector_state, "dead_lettered",
            "the terminal decision must persist vector_state"
        );

        let after = outbox.list(10).await.expect("list after dead-letter");
        assert!(
            !after.iter().any(|row| row.id == id),
            "a dead-lettered row must not reappear in a later batch"
        );
    }

    /// I63-B regression: a poisoned vector stored at an old dimension must NOT
    /// be dead-lettered before the embedder gets a chance to repair it. With no
    /// endpoint configured the row is deferred (retry path), not deleted, so a
    /// later pass with an embedder can still recover the memory.
    #[tokio::test]
    async fn reconcile_defers_stale_dimension_poison_instead_of_dead_lettering() {
        let mut pending = pending_entry();
        pending.embedding = vec![f32::NAN; 1536]; // poisoned, stale dimension
        let outbox = MemoryOutbox::with_backoff_filter(vec![pending]);
        let embedder = disabled_embedder();

        reconcile_pending(&outbox, &ScriptedSink::new(vec![]), Some(&embedder), 384).await;

        let rows = outbox.snapshot();
        assert_eq!(
            rows.len(),
            1,
            "a stale-dimension poison must stay on the retry path, not be deleted: {rows:?}"
        );
        assert_eq!(
            rows[0].attempts, 1,
            "the row must be deferred for a later recovery attempt"
        );
    }

    /// I63-B regression: the core of `vector_for_pending` repairs a non-finite
    /// snapshot at a stale dimension by re-embedding the content. The injected
    /// embedder returns a finite 384-dim vector, so no endpoint, DB or Qdrant is
    /// contacted and the recovery is deterministic.
    #[tokio::test]
    async fn stale_dimension_poison_is_repaired_by_reembedding() {
        struct FakeEmbedder {
            vector: Vec<f32>,
        }

        #[async_trait::async_trait]
        impl Reembed for FakeEmbedder {
            async fn reembed(&self, _content: &str) -> Result<Vec<f32>> {
                Ok(self.vector.clone())
            }
        }

        let fresh = vec![0.25_f32; 384];
        let fake = FakeEmbedder {
            vector: fresh.clone(),
        };
        let mut pending = pending_entry();
        pending.embedding = vec![f32::NAN; 1536];

        let vector = vector_for_pending_with(&pending, Some(&fake), 384)
            .await
            .expect("a stale-dimension poison must be repaired by re-embedding");
        assert_eq!(
            vector, fresh,
            "the fresh embedding must be replayed, not the poisoned snapshot"
        );
    }

    #[tokio::test]
    async fn reconcile_without_vector_or_endpoint_keeps_row() {
        let mut pending = pending_entry();
        pending.embedding.clear();
        let outbox = MemoryOutbox::with_rows(vec![pending]);
        let embedder = disabled_embedder();

        // The sink is scripted empty; if it were called the test would panic.
        reconcile_pending(&outbox, &ScriptedSink::new(vec![]), Some(&embedder), 384).await;
        assert_eq!(
            outbox.count(),
            1,
            "no vector and no endpoint means retry later"
        );
    }

    #[tokio::test]
    async fn reconcile_clears_stale_row_after_a_direct_store_success() {
        let memory = entry_with_expiry(None);
        let outbox = MemoryOutbox::with_rows(vec![PendingEmbedding {
            id: memory.id,
            content: memory.content.clone(),
            metadata: memory.metadata.clone(),
            embedding: Vec::new(),
            attempts: 0,
            next_retry_at: None,
            vector_state: "pending",
        }]);

        store_memory_with_sinks(&memory, &ScriptedSink::new(vec![true]), &outbox)
            .await
            .expect("upsert succeeds");
        assert_eq!(outbox.count(), 0, "a stale queued row must be cleared");
    }

    // --- I30-C: retry backoff / head-of-line blocking (pure, no live services) ---

    #[test]
    fn retry_backoff_doubles_and_is_capped() {
        assert_eq!(retry_backoff(1).num_minutes(), 2);
        assert_eq!(retry_backoff(2).num_minutes(), 4);
        assert_eq!(retry_backoff(3).num_minutes(), 8);
        assert_eq!(retry_backoff(64).num_minutes(), MAX_RETRY_BACKOFF_MINUTES);
        // A hostile/corrupt attempt count must not panic or overflow.
        assert_eq!(
            retry_backoff(i32::MAX).num_minutes(),
            MAX_RETRY_BACKOFF_MINUTES
        );
    }

    #[test]
    fn next_retry_state_bumps_attempts_and_defers_into_the_future() {
        let now = fixed_now();
        let (attempts, next) = next_retry_state(0, now);
        assert_eq!(attempts, 1);
        assert_eq!(next, now + chrono::Duration::minutes(2));

        let (attempts, next) = next_retry_state(attempts, now);
        assert_eq!(attempts, 2);
        assert_eq!(next, now + chrono::Duration::minutes(4));
    }

    #[test]
    fn is_retry_due_respects_the_backoff_boundary() {
        let now = fixed_now();
        let past = Some(now - chrono::Duration::seconds(1));
        let future = Some(now + chrono::Duration::seconds(1));
        assert!(is_retry_due(None, now), "never failed is due");
        assert!(is_retry_due(Some(now), now), "due at the boundary");
        assert!(is_retry_due(past, now));
        assert!(!is_retry_due(future, now));
    }

    /// A failed retry records one attempt and defers the row, so the next
    /// selection pass can skip it. Pure: no clock injection or DB involved
    /// beyond the in-memory store and a future `next_retry_at`.
    #[tokio::test]
    async fn failed_retry_records_attempts_and_defers_the_row() {
        let pending = pending_entry();
        let outbox = MemoryOutbox::with_backoff_filter(vec![pending.clone()]);
        let embedder = disabled_embedder();
        let dim = pending.embedding.len();

        reconcile_pending(
            &outbox,
            &ScriptedSink::new(vec![false]),
            Some(&embedder),
            dim,
        )
        .await;

        let rows = outbox.snapshot();
        assert_eq!(rows.len(), 1, "a failed retry must not be deleted");
        assert_eq!(rows[0].attempts, 1, "the failure must be counted");
        assert!(
            rows[0].next_retry_at.is_some_and(|at| at > Utc::now()),
            "the row must be deferred into the future"
        );
    }

    /// Head-of-line blocking regression (I30-C): a full batch of permanently
    /// failing rows (old-dimension vector + no embedder) must not starve a
    /// later, retryable row across successive startup passes. The in-memory
    /// store applies the same due-time gate as the production SQL query; no
    /// PostgreSQL or Qdrant connection is involved.
    #[tokio::test]
    async fn reconcile_does_not_head_of_line_block_on_permanently_failing_rows() {
        let mut rows: Vec<PendingEmbedding> = (0..RECONCILE_BATCH_LIMIT)
            .map(|_| {
                let mut row = pending_entry();
                row.embedding.clear(); // stale/old-dimension vector
                row
            })
            .collect();
        let good = pending_entry(); // valid 384-dim vector: retryable
        let good_id = good.id;
        rows.push(good);

        let outbox = MemoryOutbox::with_backoff_filter(rows);
        // Disabled endpoint: the first batch can never produce a vector, the
        // exact permanent-failure scenario from the adversarial review.
        let embedder = disabled_embedder();

        // Pass 1: the whole batch of failing rows is deferred, not deleted.
        reconcile_pending(&outbox, &ScriptedSink::new(vec![]), Some(&embedder), 384).await;
        assert_eq!(
            outbox.count(),
            RECONCILE_BATCH_LIMIT + 1,
            "deferring is not deletion"
        );

        let after_first = outbox.snapshot();
        let failing: Vec<_> = after_first.iter().filter(|r| r.id != good_id).collect();
        assert_eq!(failing.len(), RECONCILE_BATCH_LIMIT);
        assert!(
            failing.iter().all(|r| r.attempts == 1),
            "each failing row must record exactly one attempt"
        );
        assert!(
            failing
                .iter()
                .all(|r| r.next_retry_at.is_some_and(|at| at > Utc::now())),
            "each failing row must be deferred out of the next batch"
        );
        assert!(
            after_first
                .iter()
                .find(|r| r.id == good_id)
                .is_some_and(|r| r.attempts == 0 && r.next_retry_at.is_none()),
            "the un-reached good row must be untouched"
        );

        // Pass 2: deferred rows are not due, so the good row reaches the sink,
        // succeeds, and is removed. Without backoff this row would be starved
        // forever by the full batch of permanent failures.
        reconcile_pending(
            &outbox,
            &ScriptedSink::new(vec![true]),
            Some(&embedder),
            384,
        )
        .await;
        let remaining = outbox.snapshot();
        assert_eq!(remaining.len(), RECONCILE_BATCH_LIMIT);
        assert!(
            !remaining.iter().any(|r| r.id == good_id),
            "a retryable row must not be starved by the failing head of the queue"
        );
    }

    // --- I61-B: poison dead-lettering + bounded retry attempts (pure/mock) ---

    /// I63-A: the dead-letter retirement is verified behaviourally — the
    /// persistence-mirroring store's `list` must hide the retained row — instead
    /// of by matching `dead_lettered_at IS NULL` in the SQL text. The two
    /// backoff/budget gates stay as pure query assertions. No DB is contacted.
    #[tokio::test]
    async fn pending_selection_query_requires_live_rows() {
        let pending = pending_entry();
        let id = pending.id;
        let outbox = RetainingOutbox::new(vec![pending]);
        outbox
            .dead_letter(id, Utc::now())
            .await
            .expect("dead-letter");
        let after = outbox.list(10).await.expect("list");
        assert!(
            !after.iter().any(|row| row.id == id),
            "a dead-lettered row must never be retried"
        );

        assert!(
            LIST_PENDING_SQL.contains("attempts < $2"),
            "exhausted rows must stay out of the batch: {LIST_PENDING_SQL}"
        );
        assert!(
            LIST_PENDING_SQL.contains("next_retry_at IS NULL OR next_retry_at <= NOW()"),
            "deferred rows must only be selected when due: {LIST_PENDING_SQL}"
        );
    }

    /// C61-R2-01: a corrupt embedding JSON used to be silently turned into an
    /// empty vector. The pure helper exposes the failing branch so `list` can
    /// emit one fixed-vocabulary warning; no log capture is needed here. The
    /// empty vector still flows into the re-embed/retry path as before.
    #[test]
    fn corrupt_embedding_json_is_detected_for_warning() {
        assert!(
            parse_embedding_json(json!("not-an-array")).is_none(),
            "a JSON string is not a vector and must take the warn branch"
        );
        assert!(
            parse_embedding_json(json!([0.1, "oops"])).is_none(),
            "a partially invalid array must take the warn branch"
        );
        assert_eq!(
            parse_embedding_json(json!([])),
            Some(Vec::new()),
            "a valid empty array is not corrupt"
        );
        assert_eq!(
            parse_embedding_json(json!([1.0, 2.0, 3.0])),
            Some(vec![1.0_f32, 2.0, 3.0]),
            "a valid vector must still decode unchanged"
        );
    }

    #[test]
    fn is_non_finite_vector_only_matches_actual_poison() {
        // An empty vector is the recoverable stale-dimension case and must keep
        // using the embedder/retry path, not the dead-letter branch.
        assert!(!is_non_finite_vector(&[]));
        assert!(!is_non_finite_vector(&[0.1; 384]));
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e100_f64 as f32] {
            assert!(
                is_non_finite_vector(&[bad, 0.2, 0.3]),
                "a non-finite component must be detected"
            );
        }
    }

    /// I63-B: the terminal decision is limited to poison at the configured
    /// dimension, where no re-embed happens. Poison at a stale dimension, finite
    /// vectors and empty vectors all stay on the recoverable/retry path.
    #[test]
    fn unrecoverable_poison_requires_matching_dimension() {
        assert!(is_unrecoverable_poison(&[f32::NAN; 384], 384));
        assert!(is_unrecoverable_poison(&[f32::INFINITY; 384], 384));
        assert!(!is_unrecoverable_poison(&[f32::NAN; 1536], 384));
        assert!(!is_unrecoverable_poison(&[0.1; 384], 384));
        assert!(!is_unrecoverable_poison(&[], 384));
    }

    #[test]
    fn next_retry_state_caps_attempts_at_the_reconcile_limit() {
        let now = fixed_now();
        let (attempts, next) = next_retry_state(MAX_RECONCILE_ATTEMPTS - 1, now);
        assert_eq!(attempts, MAX_RECONCILE_ATTEMPTS);
        assert_eq!(next, now + retry_backoff(MAX_RECONCILE_ATTEMPTS));

        // A hostile/corrupt stored count cannot climb past the cap either.
        let (attempts, _) = next_retry_state(i32::MAX, now);
        assert_eq!(attempts, MAX_RECONCILE_ATTEMPTS);
    }

    /// Head-of-line regression for the Round 57 poison-row bug (I61-B): a full
    /// batch of non-finite rows is dead-lettered (deleted) on pass 1, so the
    /// healthy row just past `RECONCILE_BATCH_LIMIT` reaches the sink on pass 2.
    /// Under the old defer policy the batch stayed full of rows retried every
    /// 24h and this PG-only memory was never reflected into Qdrant.
    #[tokio::test]
    async fn poison_rows_do_not_starve_the_healthy_row() {
        let mut rows: Vec<PendingEmbedding> = (0..RECONCILE_BATCH_LIMIT)
            .map(|_| {
                let mut row = pending_entry();
                row.embedding[0] = f32::NAN; // unrecoverable poison
                row
            })
            .collect();
        let good = pending_entry(); // valid 384-dim vector: retryable
        let good_id = good.id;
        rows.push(good);

        let outbox = MemoryOutbox::with_backoff_filter(rows);
        let embedder = disabled_embedder();

        // Pass 1: only the first RECONCILE_BATCH_LIMIT rows are listed; all are
        // poison and are deleted. The sink script is empty, so any attempted
        // upsert would panic. The good row is past the limit and untouched.
        reconcile_pending(&outbox, &ScriptedSink::new(vec![]), Some(&embedder), 384).await;
        let after_first = outbox.snapshot();
        assert_eq!(
            after_first.len(),
            1,
            "all poison rows must be dead-lettered, not deferred"
        );
        assert_eq!(after_first[0].id, good_id);
        assert_eq!(
            after_first[0].attempts, 0,
            "the un-reached healthy row must be untouched"
        );

        // Pass 2: the healthy row is now in the batch and reaches the sink.
        reconcile_pending(
            &outbox,
            &ScriptedSink::new(vec![true]),
            Some(&embedder),
            384,
        )
        .await;
        assert!(
            outbox.snapshot().is_empty(),
            "the healthy row must be reconciled and removed"
        );
    }

    #[tokio::test]
    async fn attempts_exhausted_rows_are_excluded_from_the_batch() {
        let mut exhausted = pending_entry();
        exhausted.attempts = MAX_RECONCILE_ATTEMPTS;
        let exhausted_id = exhausted.id;
        let good = pending_entry();
        let good_id = good.id;
        let outbox = MemoryOutbox::with_backoff_filter(vec![exhausted, good]);
        let embedder = disabled_embedder();

        // The exhausted row is never listed; only the good row reaches the sink.
        reconcile_pending(
            &outbox,
            &ScriptedSink::new(vec![true]),
            Some(&embedder),
            384,
        )
        .await;

        let rows = outbox.snapshot();
        assert_eq!(
            rows.len(),
            1,
            "the exhausted row stays in the table but out of the batch"
        );
        assert_eq!(rows[0].id, exhausted_id);
        assert!(!rows.iter().any(|row| row.id == good_id));
    }

    #[tokio::test]
    async fn repeated_failures_stop_at_the_attempts_cap() {
        let pending = pending_entry();
        let outbox = MemoryOutbox::with_rows(vec![pending]);
        let embedder = disabled_embedder();

        for _ in 0..MAX_RECONCILE_ATTEMPTS {
            reconcile_pending(
                &outbox,
                &ScriptedSink::new(vec![false]),
                Some(&embedder),
                384,
            )
            .await;
        }
        let rows = outbox.snapshot();
        assert_eq!(rows.len(), 1, "a failed retry must not be deleted");
        assert_eq!(
            rows[0].attempts, MAX_RECONCILE_ATTEMPTS,
            "attempts must saturate at the cap"
        );
        assert_eq!(
            rows[0].vector_state, "exhausted",
            "retiring a row from retries must persist the terminal state (I65-B)"
        );

        // One more pass with an empty sink script: the exhausted row is not
        // listed, so no upsert is attempted and the row is left for inspection.
        reconcile_pending(&outbox, &ScriptedSink::new(vec![]), Some(&embedder), 384).await;
        let rows = outbox.snapshot();
        assert_eq!(rows[0].attempts, MAX_RECONCILE_ATTEMPTS);
        assert_eq!(
            rows[0].vector_state, "exhausted",
            "the durable state must survive later reconcile passes"
        );
    }

    /// I65-B: only a row that actually spent its whole retry budget is retired.
    /// A single transient failure keeps the fixed `'pending'` state so it remains
    /// part of the recoverable backlog reported by `jarvis_pending_embeddings`.
    #[tokio::test]
    async fn a_transient_failure_keeps_the_row_recoverable() {
        let pending = pending_entry();
        let outbox = MemoryOutbox::with_rows(vec![pending]);
        let embedder = disabled_embedder();

        reconcile_pending(
            &outbox,
            &ScriptedSink::new(vec![false]),
            Some(&embedder),
            384,
        )
        .await;

        let rows = outbox.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attempts, 1);
        assert_eq!(
            rows[0].vector_state, "pending",
            "a retryable row must not be counted as exhausted"
        );
    }

    /// I65-B: the gauge queries partition non-dead-lettered rows by durable
    /// state, and the backfill retires legacy cap-reaching rows. Pure string
    /// assertions keep this free of a PostgreSQL connection.
    #[test]
    fn outbox_state_queries_partition_pending_and_exhausted() {
        assert!(
            OUTBOX_STATE_COUNTS_SQL.contains("dead_lettered_at IS NULL"),
            "dead-lettered rows must be excluded from both gauges"
        );
        assert!(
            OUTBOX_STATE_COUNTS_SQL.contains("vector_state = 'pending'"),
            "the pending gauge must not count retired rows"
        );
        assert!(
            OUTBOX_STATE_COUNTS_SQL.contains("vector_state = 'exhausted'"),
            "a dedicated exhausted count is required: {OUTBOX_STATE_COUNTS_SQL}"
        );
        assert!(
            BACKFILL_EXHAUSTED_SQL.contains("attempts >= $1"),
            "legacy cap-reaching rows must be backfilled: {BACKFILL_EXHAUSTED_SQL}"
        );
        assert!(
            BACKFILL_EXHAUSTED_SQL.contains("vector_state = 'pending'"),
            "backfill must be idempotent: {BACKFILL_EXHAUSTED_SQL}"
        );
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::{attach_scores, reorder_by_ids};
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
            score: None,
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

    /// P1-1: relevance order and scores must survive the PostgreSQL refetch
    /// together. Pure test: no Qdrant/PostgreSQL connection is involved.
    #[test]
    fn reorder_then_attach_scores_keeps_order_and_scores() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let fetched = vec![entry(c), entry(a), entry(b)];
        let scores =
            std::collections::HashMap::from([(a, Some(0.9_f32)), (b, Some(0.5)), (c, Some(0.7))]);

        let ordered = attach_scores(reorder_by_ids(fetched, &[b, a, c]), &scores);

        assert_eq!(
            ordered.iter().map(|e| (e.id, e.score)).collect::<Vec<_>>(),
            vec![(b, Some(0.5)), (a, Some(0.9)), (c, Some(0.7))],
            "score must follow its id after reordering"
        );
    }

    /// A zero score is a real vector-search result and must not be confused
    /// with a missing score from a non-ranked (PostgreSQL-only) path.
    #[test]
    fn attach_scores_distinguishes_zero_from_missing() {
        let ranked = Uuid::new_v4();
        let unranked = Uuid::new_v4();
        let scores = std::collections::HashMap::from([(ranked, Some(0.0_f32))]);

        let attached = attach_scores(vec![entry(ranked), entry(unranked)], &scores);

        assert_eq!(attached[0].score, Some(0.0));
        assert_eq!(attached[1].score, None);
    }

    /// C60-R2-01: a score that Qdrant could not supply is carried as `None`
    /// all the way to `MemoryEntry`; it must never be fabricated as `0.0`.
    #[test]
    fn attach_scores_keeps_explicit_none_from_qdrant() {
        let ranked = Uuid::new_v4();
        let unscored = Uuid::new_v4();
        let scores = std::collections::HashMap::from([(ranked, Some(0.0_f32)), (unscored, None)]);

        let attached = attach_scores(vec![entry(ranked), entry(unscored)], &scores);

        assert_eq!(attached[0].score, Some(0.0));
        assert_eq!(attached[1].score, None);
    }
}

#[cfg(test)]
mod log_safety_tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// A credential-bearing URL that would appear in the captured logs if any
    /// builder warning ever formatted the raw error chain.
    // Split the credential-like key name so the repository secret scanner does
    // not flag this deliberate leak-detection fixture as a real credential.
    const PROBE: &str = concat!(
        "https://admin:s3cr3t@qdrant.internal:6333/private",
        "?api",
        "_key=DO_NOT_LOG"
    );

    #[derive(Clone)]
    struct SharedLogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedLogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `future` under a thread-local WARN subscriber and return what it
    /// wrote. A current-thread runtime keeps every event on the subscribed
    /// thread, so the capture is deterministic and isolated from other tests.
    fn capture_warns<F>(future: F) -> String
    where
        F: std::future::Future<Output = ()>,
    {
        // tracing's thread-local dispatcher and the current-thread runtime can
        // interfere across parallel tests; serialize all log-capture helpers.
        static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedLogBuffer(buffer.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        tracing::subscriber::with_default(subscriber, || runtime.block_on(future));
        let captured = buffer.lock().expect("log buffer lock").clone();
        String::from_utf8(captured).expect("captured logs are UTF-8")
    }

    struct OkSink;

    #[async_trait::async_trait]
    impl VectorSink for OkSink {
        async fn upsert(&self, _memory: &MemoryEntry) -> Result<()> {
            Ok(())
        }
    }

    /// Fails with the credential-bearing URL in the error chain; if any builder
    /// warning ever formats that chain, `PROBE` shows up in the captured logs.
    struct LeakSink;

    #[async_trait::async_trait]
    impl VectorSink for LeakSink {
        async fn upsert(&self, _memory: &MemoryEntry) -> Result<()> {
            anyhow::bail!("upsert failed for {PROBE} with a response fragment")
        }
    }

    /// Configurable outbox seam; each failure also carries `PROBE`. Dead-letter
    /// ids are tracked so `list` mirrors the production retention gate instead
    /// of falling through to the delete-based trait default.
    struct LeakStore {
        rows: Mutex<Vec<PendingEmbedding>>,
        dead_lettered: Mutex<Vec<Uuid>>,
        fail_list: bool,
        fail_remove: bool,
        fail_mark_failed: bool,
    }

    impl LeakStore {
        fn new(rows: Vec<PendingEmbedding>) -> Self {
            Self {
                rows: Mutex::new(rows),
                dead_lettered: Mutex::new(Vec::new()),
                fail_list: false,
                fail_remove: false,
                fail_mark_failed: false,
            }
        }

        fn failing_list() -> Self {
            Self {
                fail_list: true,
                ..Self::new(Vec::new())
            }
        }

        fn with_remove_failure(mut self) -> Self {
            self.fail_remove = true;
            self
        }

        fn with_mark_failed_failure(mut self) -> Self {
            self.fail_mark_failed = true;
            self
        }
    }

    #[async_trait::async_trait]
    impl PendingStore for LeakStore {
        async fn record(&self, _memory: &MemoryEntry, _embedding: &[f32]) -> Result<()> {
            Ok(())
        }

        async fn list(&self, _limit: usize) -> Result<Vec<PendingEmbedding>> {
            if self.fail_list {
                anyhow::bail!("list failed for {PROBE} with statement details");
            }
            let dead = self.dead_lettered.lock().expect("dead lock").clone();
            Ok(self
                .rows
                .lock()
                .expect("rows lock")
                .iter()
                .filter(|row| !dead.contains(&row.id))
                .cloned()
                .collect())
        }

        async fn remove(&self, _id: Uuid) -> Result<()> {
            if self.fail_remove {
                anyhow::bail!("delete failed for {PROBE} with connection details");
            }
            Ok(())
        }

        async fn mark_failed(
            &self,
            _id: Uuid,
            _attempts: i32,
            _next_retry_at: DateTime<Utc>,
        ) -> Result<()> {
            if self.fail_mark_failed {
                anyhow::bail!("deferred-retry write failed for {PROBE}");
            }
            Ok(())
        }

        async fn dead_letter(&self, id: Uuid, _at: DateTime<Utc>) -> Result<()> {
            // Production retains the row and excludes it via the SELECT gate;
            // never fall through to the trait's delete-based default here.
            self.dead_lettered.lock().expect("dead lock").push(id);
            Ok(())
        }
    }

    fn memory() -> MemoryEntry {
        MemoryEntry {
            id: Uuid::new_v4(),
            content: "private payload".into(),
            embedding: vec![0.1; 384],
            metadata: json!({}),
            source: "test".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            expires_at: None,
            score: None,
        }
    }

    fn pending() -> PendingEmbedding {
        PendingEmbedding {
            id: Uuid::new_v4(),
            content: format!("private payload {PROBE}"),
            metadata: json!({}),
            embedding: vec![0.1; 384],
            attempts: 0,
            next_retry_at: None,
            vector_state: "pending",
        }
    }

    #[test]
    fn builder_warnings_never_log_the_raw_error_chain() {
        let logs = capture_warns(async {
            // Direct store: Qdrant upsert failure (sink) and stale-row removal
            // failure (outbox).
            let _ = store_memory_with_sinks(&memory(), &LeakSink, &LeakStore::new(vec![])).await;
            let _ = store_memory_with_sinks(
                &memory(),
                &OkSink,
                &LeakStore::new(vec![]).with_remove_failure(),
            )
            .await;

            // Reconcile: list failure, Qdrant failure + deferred-retry failure,
            // reconciled remove failure, and a stale vector with no embedder.
            reconcile_pending(&LeakStore::failing_list(), &OkSink, None, 384).await;
            reconcile_pending(
                &LeakStore::new(vec![pending()]).with_mark_failed_failure(),
                &LeakSink,
                None,
                384,
            )
            .await;
            reconcile_pending(
                &LeakStore::new(vec![pending()]).with_remove_failure(),
                &OkSink,
                None,
                384,
            )
            .await;
            let mut stale = pending();
            stale.embedding.clear();
            reconcile_pending(&LeakStore::new(vec![stale]), &OkSink, None, 384).await;
        });

        assert!(!logs.contains(PROBE), "raw error text leaked: {logs}");
        assert!(!logs.contains("private payload"), "payload leaked: {logs}");
        // Do not pin the textual rendering of structured fields: it depends on
        // the tracing-subscriber formatter version. The contract is the absence
        // of raw error text; the fixed `error_kind` fields are set at the sites.
    }

    /// I61-B / I63-A: a poison row emits exactly one decision warning, carrying
    /// only `id`, `attempts` and `error_kind` — never the vector value, a raw
    /// error or the row payload. The row is retained with an audit marker, and
    /// the dead-letter gate hides it from later `list` calls, so a second pass
    /// emits no warning and the row is still present.
    #[test]
    fn dead_letter_warns_once_with_fixed_vocabulary() {
        let mut poison = pending();
        poison.embedding[0] = f32::NAN;
        let id = poison.id;
        let store = LeakStore::new(vec![poison]);

        let logs = capture_warns(async {
            reconcile_pending(&store, &OkSink, None, 384).await;
            reconcile_pending(&store, &OkSink, None, 384).await;
        });

        assert_eq!(
            logs.lines().count(),
            1,
            "a dead-lettered row must warn exactly once: {logs}"
        );
        assert_eq!(
            store.rows.lock().expect("rows lock").len(),
            1,
            "the dead-lettered row must be retained, not deleted"
        );
        assert_eq!(
            *store.dead_lettered.lock().expect("dead lock"),
            vec![id],
            "the row must carry the terminal decision marker"
        );
        assert!(logs.contains("dead-lettering"), "{logs}");
        assert!(!logs.contains(PROBE), "raw error text leaked: {logs}");
        assert!(!logs.contains("private payload"), "payload leaked: {logs}");
        assert!(!logs.contains("NaN"), "a vector value leaked: {logs}");
        assert!(
            logs.contains(&id.to_string()),
            "the fixed id field must be present: {logs}"
        );
        assert!(logs.contains("error_kind"), "{logs}");
        assert!(logs.contains("attempts"), "{logs}");
    }

    #[test]
    fn error_kind_maps_context_error_variants_without_reading_their_text() {
        // The Qdrant variant can only be built from the private-token newtype,
        // so even the error's own rendering cannot carry the hostile probe.
        let err: anyhow::Error =
            crate::error::ContextError::qdrant(crate::error::QdrantErrorKind::FAILED).into();
        assert_eq!(error_kind(&err, ErrorKind::Persistence), "qdrant");
        assert!(!format!("{err:#}").contains(PROBE));

        let err: anyhow::Error = crate::error::ContextError::Pii(PROBE.to_owned()).into();
        assert_eq!(error_kind(&err, ErrorKind::Persistence), "pii");

        let err: anyhow::Error = anyhow::anyhow!("{PROBE}");
        assert_eq!(error_kind(&err, ErrorKind::Persistence), "persistence");
        assert_eq!(error_kind(&err, ErrorKind::Embedding), "embedding");
    }

    /// Ordering log shared by a scripted outbox and a recording counter sink.
    #[derive(Clone, Default)]
    struct Events(Arc<Mutex<Vec<&'static str>>>);

    impl Events {
        fn push(&self, event: &'static str) {
            self.0.lock().expect("events lock").push(event);
        }

        fn snapshot(&self) -> Vec<&'static str> {
            self.0.lock().expect("events lock").clone()
        }
    }

    /// `dead_letter` either succeeds (recording `"persisted"`) or fails with an
    /// error carrying `PROBE`, so the failure warn can be checked for leaks too.
    struct ScriptedDeadLetterStore {
        fail: bool,
        events: Events,
    }

    #[async_trait::async_trait]
    impl PendingStore for ScriptedDeadLetterStore {
        async fn record(&self, _memory: &MemoryEntry, _embedding: &[f32]) -> Result<()> {
            Ok(())
        }

        async fn list(&self, _limit: usize) -> Result<Vec<PendingEmbedding>> {
            Ok(Vec::new())
        }

        async fn remove(&self, _id: Uuid) -> Result<()> {
            Ok(())
        }

        async fn mark_failed(
            &self,
            _id: Uuid,
            _attempts: i32,
            _next_retry_at: DateTime<Utc>,
        ) -> Result<()> {
            Ok(())
        }

        async fn dead_letter(&self, _id: Uuid, _at: DateTime<Utc>) -> Result<()> {
            if self.fail {
                anyhow::bail!("dead-letter write failed for {PROBE}");
            }
            self.events.push("persisted");
            Ok(())
        }
    }

    /// Test sink replacing the process-wide Prometheus recorder: records which
    /// counter was incremented, and when, without installing a global recorder.
    struct RecordingCounters(Events);

    impl DeadLetterCounters for RecordingCounters {
        fn persisted(&self) {
            self.0.push("success");
        }

        fn persistence_failed(&self) {
            self.0.push("failure");
        }
    }

    /// I65-B: a terminal decision is counted as durable only after the outbox
    /// write has actually succeeded — never merely because it was attempted.
    #[tokio::test]
    async fn dead_letter_success_counter_follows_the_successful_write() {
        let events = Events::default();
        let outbox = ScriptedDeadLetterStore {
            fail: false,
            events: events.clone(),
        };
        let counters = RecordingCounters(events.clone());

        dead_letter_retry_with(&outbox, Uuid::new_v4(), 0, &counters).await;

        assert_eq!(
            events.snapshot(),
            vec!["persisted", "success"],
            "the success counter must be incremented after durability, not before"
        );
    }

    /// I65-B: a failed DB write is accounted for separately and never inflates
    /// the success counter; the failure warning is still emitted and stays free
    /// of raw error text.
    #[test]
    fn dead_letter_persistence_failure_is_counted_separately_and_still_warns() {
        let events = Events::default();
        let outbox = ScriptedDeadLetterStore {
            fail: true,
            events: events.clone(),
        };
        let counters = RecordingCounters(events.clone());
        let id = Uuid::new_v4();

        let logs = capture_warns(async {
            dead_letter_retry_with(&outbox, id, 3, &counters).await;
        });

        assert_eq!(
            events.snapshot(),
            vec!["failure"],
            "a non-persisted decision must only touch the failure counter"
        );
        assert!(
            logs.contains("could not persist a dead-letter decision"),
            "the failure warning must be maintained: {logs}"
        );
        assert!(logs.contains(&id.to_string()), "fixed id label: {logs}");
        assert!(!logs.contains(PROBE), "raw error text leaked: {logs}");
    }

    // --- I69-D: outbox gauge freshness after terminal transitions ---
    // Mock store + pure event order: no PostgreSQL, Qdrant or Prometheus
    // recorder is installed, so the refresh policy is asserted deterministically.

    /// Records every terminal outbox write and every gauge refresh as an event
    /// so the "mutations first, one refresh last" ordering is observable.
    struct GaugeRefreshOutbox {
        events: Events,
        rows: Mutex<Vec<PendingEmbedding>>,
    }

    impl GaugeRefreshOutbox {
        fn new(rows: Vec<PendingEmbedding>) -> Self {
            Self {
                events: Events::default(),
                rows: Mutex::new(rows),
            }
        }
    }

    #[async_trait::async_trait]
    impl PendingStore for GaugeRefreshOutbox {
        async fn record(&self, _memory: &MemoryEntry, _embedding: &[f32]) -> Result<()> {
            Ok(())
        }

        async fn list(&self, _limit: usize) -> Result<Vec<PendingEmbedding>> {
            Ok(self.rows.lock().expect("rows lock").clone())
        }

        async fn remove(&self, _id: Uuid) -> Result<()> {
            self.events.push("remove");
            Ok(())
        }

        async fn mark_failed(
            &self,
            _id: Uuid,
            _attempts: i32,
            _next_retry_at: DateTime<Utc>,
        ) -> Result<()> {
            self.events.push("mark_failed");
            Ok(())
        }

        async fn dead_letter(&self, _id: Uuid, _at: DateTime<Utc>) -> Result<()> {
            self.events.push("dead_letter");
            Ok(())
        }

        async fn mark_exhausted(&self, _id: Uuid) -> Result<()> {
            self.events.push("mark_exhausted");
            Ok(())
        }

        async fn observe_state_counts(&self) {
            self.events.push("observe");
        }
    }

    /// A successful reconcile deletes the row and refreshes the gauges exactly
    /// once, after the write — not once per row.
    #[tokio::test]
    async fn reconcile_refreshes_outbox_gauges_once_after_remove() {
        let outbox = GaugeRefreshOutbox::new(vec![pending()]);
        reconcile_pending(&outbox, &OkSink, None, 384).await;
        assert_eq!(
            outbox.events.snapshot(),
            vec!["remove", "observe"],
            "the refresh must follow the successful delete and run once"
        );
    }

    /// A terminal dead-letter also refreshes after the durable decision, so a
    /// retired row no longer inflates the recoverable pending gauge.
    #[tokio::test]
    async fn reconcile_refreshes_outbox_gauges_after_dead_letter() {
        let mut poison = pending();
        poison.embedding[0] = f32::NAN;
        let outbox = GaugeRefreshOutbox::new(vec![poison]);
        reconcile_pending(&outbox, &OkSink, None, 384).await;
        assert_eq!(
            outbox.events.snapshot(),
            vec!["dead_letter", "observe"],
            "dead-letter must be followed by exactly one gauge refresh"
        );
    }

    /// Exhausting the retry budget writes the terminal `exhausted` state; the
    /// single end-of-loop refresh then publishes the new state split.
    #[tokio::test]
    async fn reconcile_refreshes_outbox_gauges_after_exhausted_transition() {
        let mut stale = pending();
        stale.embedding.clear();
        stale.attempts = MAX_RECONCILE_ATTEMPTS - 1;
        let outbox = GaugeRefreshOutbox::new(vec![stale]);
        reconcile_pending(&outbox, &OkSink, None, 384).await;
        assert_eq!(
            outbox.events.snapshot(),
            vec!["mark_failed", "mark_exhausted", "observe"],
            "the exhausted write must precede the single gauge refresh"
        );
    }

    /// The store path lies outside `reconcile_pending`; a confirmed stale-row
    /// remove there must refresh the gauges too.
    #[tokio::test]
    async fn store_path_refreshes_outbox_gauges_after_remove() {
        let outbox = GaugeRefreshOutbox::new(Vec::new());
        store_memory_with_sinks(&memory(), &OkSink, &outbox)
            .await
            .expect("upsert succeeds");
        assert_eq!(
            outbox.events.snapshot(),
            vec!["remove", "observe"],
            "a stale-row delete must be followed by exactly one refresh"
        );
    }

    /// I49-B / I51-D: all five Qdrant call sites (build, list, create, upsert,
    /// search) share this mapper. Feed it a raw error whose `Display` carries
    /// the credential probe and a response fragment: neither may survive into
    /// the returned error, and the fixed `error_kind` token must. Non-leakage
    /// is now enforced at the type boundary (`QdrantErrorKind`'s private token
    /// and the crate-private constructor), so the mapper's `_` discard is a
    /// single funnel rather than a convention. No live Qdrant is needed for
    /// this deterministic regression.
    #[test]
    fn qdrant_error_mapper_discards_raw_client_text() {
        let raw: std::result::Result<(), String> =
            Err(format!("POST {PROBE} -> 500 {{response fragment}}"));
        let err = discard_qdrant_error(raw).expect_err("raw Qdrant error must fail closed");
        let err: anyhow::Error = err.into();

        let rendered = format!("{err:#}");
        assert!(
            !rendered.contains(PROBE),
            "raw Qdrant error text leaked: {rendered}"
        );
        assert!(
            !rendered.contains("response fragment"),
            "Qdrant response fragment leaked: {rendered}"
        );
        assert!(
            rendered.contains("qdrant"),
            "fixed error_kind token must survive: {rendered}"
        );
        assert_eq!(error_kind(&err, ErrorKind::Persistence), "qdrant");
    }

    /// I51-C: the multi-vector startup rejection must use the fixed vocabulary
    /// and never echo a raw response fragment or a named-vector identifier.
    #[test]
    fn multi_vector_rejection_never_logs_raw_text() {
        use qdrant_client::qdrant::vectors_config::Config;
        use qdrant_client::qdrant::{
            CollectionConfig, CollectionInfo, CollectionParams, VectorParams, VectorParamsMap,
            VectorsConfig,
        };
        use std::collections::HashMap;

        // Put the leak probe in the vector names: the rejection must report
        // only the count, never the identifiers.
        let multi = CollectionInfo {
            config: Some(CollectionConfig {
                params: Some(CollectionParams {
                    vectors_config: Some(VectorsConfig {
                        config: Some(Config::ParamsMap(VectorParamsMap {
                            map: HashMap::from([
                                (
                                    format!("dense-{PROBE}"),
                                    VectorParams {
                                        size: 384,
                                        ..Default::default()
                                    },
                                ),
                                (
                                    "sparse".to_string(),
                                    VectorParams {
                                        size: 384,
                                        ..Default::default()
                                    },
                                ),
                            ]),
                        })),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let logs = capture_warns(async {
            let _ = memory::reject_multi_vector_collection("jarvis_memories", &multi);
        });
        assert!(!logs.contains(PROBE), "raw text leaked: {logs}");
        assert!(
            !logs.contains("response"),
            "response fragment leaked: {logs}"
        );
        assert!(
            !logs.contains("dense"),
            "named-vector identifiers must not be logged: {logs}"
        );
    }

    /// I49-B / I51-D: keep every Qdrant call site pinned to the shared mapper
    /// and forbid reintroducing a raw `Display` read at any of them.
    ///
    /// This source scan is no longer the confidentiality boundary: since I51-D
    /// a raw client string cannot be stored in `ContextError::Qdrant` at all,
    /// because `QdrantErrorKind` has a private token and only the crate-private
    /// `ContextError::qdrant` constructor can build the variant. The scan is a
    /// belt-and-suspenders regression guard on the *call sites* only: it keeps
    /// them on the single mapper so a future edit cannot leak client text into
    /// a different error type (e.g. `anyhow`) that is not type-guarded here.
    #[test]
    fn all_qdrant_call_sites_use_fixed_kind_mapper() {
        const SOURCE: &str = include_str!("builder.rs");
        // Build every needle at runtime: a literal would appear in this very
        // source file and make the scan match itself.
        let raw_needle = String::from("Qdrant(e.") + "to_string())";
        assert!(
            !SOURCE.contains(&raw_needle),
            "raw Qdrant error must never be formatted into a ContextError"
        );
        let closure_needle = String::from("map_err(") + "|e|";
        assert!(
            !SOURCE.contains(&closure_needle),
            "no mapper may read the raw Qdrant error's Display"
        );
        let needle = String::from("discard_qdrant_") + "error(";
        let uses = SOURCE.matches(&needle).count();
        assert!(
            uses >= 5,
            "expected five Qdrant mapper call sites plus this test, found {uses}"
        );
    }
}
