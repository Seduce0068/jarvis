-- Round 95: the Rust ingress validates `source` up to MAX_SOURCE_BYTES (4096),
-- but 001_initial.sql declared VARCHAR(100). Anything between 101 and 4096
-- bytes passed validation and then failed at INSERT with a 500. Widen the
-- column to TEXT so the database accepts everything the application accepts.
-- Widening VARCHAR(n) -> TEXT is non-destructive and safe to re-run.
ALTER TABLE memories ALTER COLUMN source TYPE TEXT;
