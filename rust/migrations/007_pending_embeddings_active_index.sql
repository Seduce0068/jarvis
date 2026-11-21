-- Migration 007: selection-side partial index for the pending_embeddings outbox.
--
-- Migration 005 added `idx_pending_embeddings_dead_lettered_at`, a partial index
-- with `WHERE dead_lettered_at IS NOT NULL`. That index covers only dead-letter
-- rows, which are exactly the rows the reconcile selection excludes, so it does
-- NOT support the selection query LIST_PENDING_SQL (`WHERE dead_lettered_at IS
-- NULL ... ORDER BY created_at ASC LIMIT $1` in jarvis-context/src/builder.rs).
-- Migration 004's `idx_pending_embeddings_next_retry_at` already supports the
-- time-based gate; this migration adds the missing companion for the live-row
-- selection ordered by creation time.
--
-- Drop the misdirected 005 index (dead weight for selection) and add a partial
-- index on `created_at` restricted to the rows the selection actually reads.
-- Both statements are idempotent (`IF EXISTS` / `IF NOT EXISTS`), so replaying
-- the sqlx baseline is a no-op. Migrations 001-006 are left untouched.

DROP INDEX IF EXISTS idx_pending_embeddings_dead_lettered_at;

CREATE INDEX IF NOT EXISTS idx_pending_embeddings_active
    ON pending_embeddings (created_at)
    WHERE dead_lettered_at IS NULL;
