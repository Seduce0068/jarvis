-- JARVIS Database Schema
-- Migration 001: Initial schema

-- Memories table for long-term storage
CREATE TABLE IF NOT EXISTS memories (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    content TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}',
    source VARCHAR(100) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_memories_source ON memories(source);
CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_memories_expires_at ON memories(expires_at) WHERE expires_at IS NOT NULL;

-- Audit log table for security/compliance
CREATE TABLE IF NOT EXISTS audit_log (
    id BIGSERIAL PRIMARY KEY,
    event_id UUID NOT NULL DEFAULT gen_random_uuid(),
    correlation_id VARCHAR(64),
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    principal VARCHAR(256) NOT NULL,
    action VARCHAR(128) NOT NULL,
    resource VARCHAR(512) NOT NULL,
    decision VARCHAR(32) NOT NULL, -- allow, deny, approval_required, approved
    context JSONB,
    policy_version VARCHAR(64),
    latency_ms INTEGER
);

CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_audit_principal ON audit_log(principal);
CREATE INDEX IF NOT EXISTS idx_audit_correlation ON audit_log(correlation_id);

-- Approval requests table
CREATE TABLE IF NOT EXISTS approval_requests (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    principal VARCHAR(256) NOT NULL,
    action VARCHAR(128) NOT NULL,
    resource VARCHAR(512) NOT NULL,
    context JSONB NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    decided_at TIMESTAMPTZ,
    decision VARCHAR(32), -- approved, denied, expired
    decided_by VARCHAR(256),
    reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_approval_pending ON approval_requests(expires_at) WHERE decided_at IS NULL;

-- Kill switch state
CREATE TABLE IF NOT EXISTS kill_switch (
    id INTEGER PRIMARY KEY DEFAULT 1,
    activated BOOLEAN NOT NULL DEFAULT FALSE,
    activated_at TIMESTAMPTZ,
    reason TEXT,
    activated_by VARCHAR(256),
    deactivated_at TIMESTAMPTZ,
    deactivated_by VARCHAR(256),
    CONSTRAINT single_row CHECK (id = 1)
);

INSERT INTO kill_switch (id, activated) VALUES (1, FALSE) ON CONFLICT DO NOTHING;

-- MCP allowlist
CREATE TABLE IF NOT EXISTS mcp_allowlist (
    tool_name VARCHAR(256) PRIMARY KEY,
    description TEXT,
    allowed_principals TEXT[] NOT NULL DEFAULT '{}',
    max_args_size INTEGER NOT NULL DEFAULT 1048576,
    timeout_ms INTEGER NOT NULL DEFAULT 30000,
    requires_approval BOOLEAN NOT NULL DEFAULT FALSE,
    sandbox_type VARCHAR(32) NOT NULL DEFAULT 'none',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Policy version tracking
CREATE TABLE IF NOT EXISTS policy_versions (
    version VARCHAR(64) PRIMARY KEY,
    cedar_policies_hash VARCHAR(64) NOT NULL,
    casbin_policies_hash VARCHAR(64) NOT NULL,
    deployed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deployed_by VARCHAR(256) NOT NULL,
    description TEXT
);
