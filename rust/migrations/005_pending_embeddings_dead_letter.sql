-- Migration 005: durable dead-letter state for the pending_embeddings outbox.
--
-- Migration 004 deferred permanently failing rows with a retry timestamp, but
-- the retry budget is now capped (MAX_RECONCILE_ATTEMPTS) and poison rows are
-- terminal. Without a durable marker those rows would either be retried
-- forever or silently vanish from the outbox, leaving the corresponding
-- `memories` row absent from Qdrant with no audit trail.
--
-- `dead_lettered_at` makes the terminal decision durable: rows with a non-NULL
-- value are excluded from the reconcile selection together with exhausted
-- attempts and deferred rows. `vector_state` records the last known state for
-- dashboards. The effective selection query is now:
--   WHERE dead_lettered_at IS NULL
--     AND attempts < MAX_RECONCILE_ATTEMPTS
--     AND (next_retry_at IS NULL OR next_retry_at <= NOW())
--
-- All statements are `IF NOT EXISTS`, matching 001/002/004 so replaying the
-- sqlx baseline is a no-op.

ALTER TABLE pending_embeddings
    ADD COLUMN IF NOT EXISTS dead_lettered_at TIMESTAMPTZ;

ALTER TABLE pending_embeddings
    ADD COLUMN IF NOT EXISTS vector_state TEXT NOT NULL DEFAULT 'pending';

CREATE INDEX IF NOT EXISTS idx_pending_embeddings_dead_lettered_at
    ON pending_embeddings (dead_lettered_at)
    WHERE dead_lettered_at IS NOT NULL;
