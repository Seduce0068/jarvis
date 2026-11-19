-- Migration 003: record the decision actor in the audit log.
--
-- 001 already stores `approval_requests.decided_by`; this adds the same column
-- to `audit_log` so the CAS + audit INSERT can be captured atomically by
-- jarvis-policy. `IF NOT EXISTS` keeps the migration replay safe and leaves
-- 001/002 untouched.

ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS decided_by VARCHAR(256);
