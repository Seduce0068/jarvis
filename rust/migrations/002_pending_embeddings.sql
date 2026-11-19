-- Outbox for vector upserts that failed after the PostgreSQL row was written.
--
-- `jarvis-context` writes here when a Qdrant upsert fails in `store_memory`;
-- `reconcile_pending` retries at startup and deletes the row only after a
-- successful upsert. A failed retry is left in place (with a warning) for the
-- next startup, so a transient Qdrant outage cannot lose the embedding.
--
-- `IF NOT EXISTS` is kept for safety, matching `001_initial.sql`: operators
-- upgrading a database that already contains this table (or replaying 001/002
-- as the sqlx baseline) still start cleanly.

CREATE TABLE IF NOT EXISTS pending_embeddings (
    id UUID PRIMARY KEY,
    content TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    -- Serialized Vec<f32>. JSONB avoids a pgvector extension dependency; the
    -- vector is validated against the configured dimension before an upsert.
    embedding JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_pending_embeddings_created_at
    ON pending_embeddings (created_at ASC);
