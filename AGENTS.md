---
type: Reference
title: "AGENTS.md for jarvis"
description: "Repository-specific guidance and developer workflows for JARVIS Personal AI."
tags:
  - agents
  - jarvis
  - opencode
  - rust
  - oci
generated:
  by: Sovereign AI Agent
  at: "2026-10-09T00:00:00+09:00"
---

## Overview

- **Repository:** `jarvis` — JARVIS Personal AI System
- **Architecture:** Unified ADR at `adr/0001-unified-architecture.md`
- **Core Decision:** OpenCode V2 = Official Agent Runtime; JARVIS Rust services = Policy/MCP/Event/Context sidecars

## Project Structure

```
jarvis/
├── adr/                          # Architecture Decision Records
│   └── 0001-unified-architecture.md
├── rust/                         # JARVIS Rust Services (Cargo Workspace)
│   ├── Cargo.toml                # Workspace root
│   ├── jarvis-policy/            # Policy Engine (Cedar/Casbin, Kill Switch)
│   ├── jarvis-mcp-gw/            # MCP Gateway (Allowlist, Sandbox)
│   ├── jarvis-event/             # Event Normalizer (HA, Android, Webhooks)
│   └── jarvis-context/           # Context Builder (PostgreSQL, Qdrant, PII)
├── infra/                        # Infrastructure as Code
│   ├── opentofu/                 # OpenTofu modules (OCI)
│   │   ├── environments/prod/    # Production env
│   │   └── modules/              # compute, network, security, storage, observability
│   ├── systemd/                  # Systemd service units
│   ├── scripts/                  # Compatibility launchers for rust-script tools
│   ├── config/                   # Config templates (pii_patterns, mcp_allowlist, policies)
│   └── cloud-init.yaml           # OCI instance initialization
├── ci/                           # CI/CD pipelines
│   └── review-gate.yml           # GitHub Actions with review-flow
├── jarvis_personal_ai_architecture.md
└── opencode-v2-oci-code-server-architecture-report.md
```

## Development Workflows

### Rust Services (Cargo Workspace)

```bash
# Build all services (cross-compile for Arm64)
cd rust
cargo build --release --workspace --target aarch64-unknown-linux-gnu

# Run tests
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all --check

# Check types
cargo check --workspace
```

### Infrastructure (OpenTofu)

```bash
cd infra/opentofu/environments/prod

# Initialize
tofu init

# Format & validate
tofu fmt -recursive
tofu validate

# Plan
tofu plan -var-file=terraform.tfvars

# Apply
tofu apply -var-file=terraform.tfvars

# Deploy services after instance provisioned
../../scripts/deploy.sh jarvis-host
```

### CI/CD (GitHub Actions + review-flow)

The `ci/review-gate.yml` implements a 3-stage pipeline:

1. **Stage 1 - Automated Gates** (lint, typecheck, test, build, vuln scan, secret scan)
2. **Stage 2 - Cross Review** (security, arch, ops subagents in parallel)
3. **Stage 3 - Merge Gate** (re-run Stage 1, then auto-merge)

```bash
# Run gate-check locally (requires OpenCode CLI)
opencode run "/review-flow gate-check <pr-number> --json"

# Run cross-review locally
opencode run "/cross-review"

# View metrics
opencode run "/review-flow metrics --days 30 --json"
```

### Operational Scripts

```bash
# Deploy Rust services to host
rust-script rust/scripts/deploy.rs --rust-dir rust jarvis-host deploy

# Manual backup
ssh jarvis@jarvis-host "sudo systemctl start jarvis-backup"

# View logs
ssh jarvis@jarvis-host "sudo journalctl -u jarvis-policy -u jarvis-mcp-gw -u jarvis-event -u jarvis-context -f"

# Review a restore plan (pass --confirm only after verifying the backup)
rust-script rust/scripts/restore.rs latest --dry-run
```

## Key Commands

| Task | Command |
|------|---------|
| Build all Rust services | `cd rust && cargo build --release --workspace --target aarch64-unknown-linux-gnu` |
| Run tests | `cd rust && cargo test --workspace` |
| Lint | `cd rust && cargo clippy --workspace -- -D warnings` |
| Format check | `cd rust && cargo fmt --all --check` |
| OpenTofu plan | `cd infra/opentofu/environments/prod && tofu plan` |
| OpenTofu apply | `cd infra/opentofu/environments/prod && tofu apply` |
| Deploy services | `rust-script rust/scripts/deploy.rs --rust-dir rust jarvis-host deploy` |
| Run gate-check | `opencode run "/review-flow gate-check <pr>"` |
| Run cross-review | `opencode run "/cross-review"` |
| SSH to host | `tailscale ssh jarvis@jarvis-host` |

## Service Ports (localhost only, accessed via Tailscale)

| Service | Port | Health Endpoint |
|---------|------|-----------------|
| jarvis-policy | 50051 (gRPC), 8081 (HTTP) | http://localhost:8081/health |
| jarvis-mcp-gw | 50052 (gRPC), 8082 (HTTP) | http://localhost:8082/health |
| jarvis-event | 50053 (gRPC), 8083 (HTTP) | http://localhost:8083/health |
| jarvis-context | 50054 (gRPC), 8084 (HTTP) | http://localhost:8084/health |
| OpenCode V2 | 49374 (Web) | http://localhost:49374 |
| code-server | 8080 | http://localhost:8080 |
| OpenTelemetry | 4317 (gRPC), 4318 (HTTP) | — |

## Configuration

All secrets managed via **1Password CLI** (`op://` references in `.env` files).

Required 1Password items:
- `JARVIS/Policy/admin_token`
- `JARVIS/PostgreSQL/connection_string`
- `JARVIS/Home Assistant/long_lived_token`
- `JARVIS/Gmail/client_id`, `client_secret`
- `JARVIS/Google Calendar/client_id`, `client_secret`
- `GRAFANA_CLOUD_INSTANCE`, `GRAFANA_CLOUD_TOKEN`

Config files in `/etc/jarvis/` on the host (populated by cloud-init + 1Password).

## Architecture Boundaries

| Component | Responsibility | Technology |
|-----------|----------------|------------|
| **OpenCode V2** | Agent runtime, sessions, MCP client, model router, UI | TypeScript/Node |
| **jarvis-policy** | Authorization (Cedar/Casbin), approval gates, kill switch | Rust |
| **jarvis-mcp-gw** | MCP tool allowlist, sandboxing (firejail/bwrap/docker) | Rust |
| **jarvis-event** | Event normalization (HA, Android, Gmail, Calendar) | Rust |
| **jarvis-context** | Memory (PostgreSQL + Qdrant), PII filtering, prompt assembly | Rust |
| **code-server** | Browser IDE for human development | TypeScript/Node |

## Security Notes

- All services bind to `127.0.0.1` only; external access via Tailscale
- systemd hardening: `NoNewPrivileges`, `ProtectSystem=strict`, `PrivateTmp`, etc.
- MCP tools require allowlist + policy approval; destructive ops need explicit approval
- Kill switch (`jarvis-policy`) can disable all automated actions instantly
- PII filtering on all context sent to LLMs
- Audit logging to PostgreSQL + OCI Object Storage

## References

- [ADR-0001: Unified Architecture](adr/0001-unified-architecture.md)
- [OpenCode V2 Docs](https://opencode.ai/v2/docs)
- [Cedar Policy Language](https://www.cedarpolicy.com/)
- [rmcp - Rust MCP SDK](https://github.com/rmcp-rs/rmcp)
- [OpenTofu](https://opentofu.org/)
- [OCI Always Free](https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier_topic-Always_Free_Resources.htm)
