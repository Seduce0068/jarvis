-- Migration 004: retry backoff for the pending_embeddings outbox.
--
-- Head-of-line blocking (adversarial review I30-C): `reconcile_pending` reads
-- at most RECONCILE_BATCH_LIMIT rows ordered by `created_at ASC`. If the first
-- batch contains rows that can never succeed (e.g. a stored vector of the old
-- embedding dimension plus no configured embedder), they were retried on every
-- startup and every later row was starved forever.
--
-- These two additive columns let reconcile defer a failed row: `attempts`
-- counts failures (driving an exponential, capped backoff) and `next_retry_at`
-- is the earliest instant the row is eligible again. The selection query is
-- `WHERE next_retry_at IS NULL OR next_retry_at <= NOW()`, so a deferred row
-- stops occupying the head of the batch and the next rows get their turn.
--
-- Both statements are `IF NOT EXISTS`, matching 001/002: replaying the sqlx
-- baseline against a database that already has these columns is a no-op, and
-- an operator who added them manually still starts cleanly. No existing
-- column, row, index or constraint is altered or dropped.

ALTER TABLE pending_embeddings
    ADD COLUMN IF NOT EXISTS attempts INTEGER NOT NULL DEFAULT 0;

ALTER TABLE pending_embeddings
    ADD COLUMN IF NOT EXISTS next_retry_at TIMESTAMPTZ;

-- Supports the time-based eligibility filter above. The index is partial
-- because rows with a NULL `next_retry_at` are always due and need no lookup.
CREATE INDEX IF NOT EXISTS idx_pending_embeddings_next_retry_at
    ON pending_embeddings (next_retry_at)
    WHERE next_retry_at IS NOT NULL;
