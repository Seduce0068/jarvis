---
type: Reference
title: "AGENTS.md for jarvis"
description: "Repository-specific guidance and developer workflows for JARVIS Personal AI."
tags:
  - agents
  - jarvis
  - opencode
  - rust
generated:
  by: Sovereign AI Agent
  at: "2026-10-09T00:00:00+09:00"
---

## Global Settings

- Common rules are inherited from the global `~/.config/opencode/AGENTS.md`.
- This file records only project-specific deltas. Do not duplicate global
  rules; override them only where this project differs.
- The global TODO list is `~/.config/opencode/todo.md`. Keep cross-project
  tasks there and track project-specific tasks in this repository.
- If this file conflicts with the global rules, the project-specific
  statement here takes precedence.

## Overview

- **Repository:** `jarvis` — JARVIS Personal AI System
- **Architecture:** Unified ADR at `adr/0001-unified-architecture.md`; local-only infra in `adr/0004-local-only-no-oci.md`; GPUI desktop UI in `adr/0005-gpui-desktop-adoption.md`
- **Core Decision:** OpenCode V2 = Official Agent Runtime; JARVIS Rust services = Policy/MCP/Event/Context sidecars

## Project Structure

```
jarvis/
├── adr/                          # Architecture Decision Records
│   ├── 0001-unified-architecture.md
│   └── 0004-local-only-no-oci.md
├── rust/                         # JARVIS Rust Services (Cargo Workspace)
│   ├── Cargo.toml                # Workspace root
│   ├── jarvis-policy/            # Policy Engine (Cedar/Casbin, Kill Switch)
│   ├── jarvis-mcp-gw/            # MCP Gateway (Allowlist, Sandbox)
│   ├── jarvis-event/             # Event Normalizer (HA, Android, Webhooks)
│   └── jarvis-context/           # Context Builder (PostgreSQL, Qdrant, PII)
├── infra/                        # Infrastructure (local systemd)
│   ├── systemd/                  # Systemd service units (現行の実行基盤)
│   ├── scripts/                  # デプロイ・検証・互換ランチャー
│   ├── config/                   # Config templates (pii_patterns, mcp_allowlist, policies)
│   ├── opentofu/                 # 未使用レガシー（OCI IaC / cloud-init, ADR-0004）
│   └── grafana/                  # Grafana dashboards
├── ui/                           # GPUI approval console (standalone crate)
│   ├── Cargo.toml                # gpui-pre / gpui-pre-platform / ureq
│   └── src/                      # approvals.rs, policy_client.rs, main.rs
├── ci/                           # Review-flow pipeline definitions
│   ├── review-gate.yml           # review-flow pipeline (not wired to Actions yet)
│   └── scripts/                  # review-run.sh (runtime adapter), secret_scan.py
├── jarvis_personal_ai_architecture.md
└── opencode-v2-oci-code-server-architecture-report.md
```

## Development Workflows

### Rust Services (Cargo Workspace)

```bash
# Build all services (local x86_64)
cd rust
cargo build --release --workspace

# Run tests
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all --check

# Check types
cargo check --workspace
```

### Infrastructure (local systemd; OCI unused)

OCI is not used ([ADR-0004](adr/0004-local-only-no-oci.md)). Services run as
systemd units on the local Kubuntu desktop; `infra/opentofu/` and its
cloud-init are frozen legacy. Install/update a unit and restart it locally:

```bash
# Install/update a unit (root required)
sudo install -o root -g root -m 0644 infra/systemd/jarvis-mcp-gw.service \
  /etc/systemd/system/jarvis-mcp-gw.service
sudo systemctl daemon-reload
sudo systemctl restart jarvis-mcp-gw

# Safe jarvis-mcp-gw deploy (dry-run by default; --apply needs root)
bash infra/scripts/deploy-mcp-gw-safe.sh
sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply
```

### CI/CD

Pushes to `master` and pull requests run `.github/workflows/ci.yml`, which
delegates to the shared `Seduce0068/workflows` `rust-gates.yml@v1` workflow
(fmt, check, test, clippy) with `working_directory: rust`.

`ci/review-gate.yml` defines a 3-stage review pipeline (automated gates, cross
review, merge gate), but it is not wired into `.github/workflows/` yet because
it requires OpenCode credentials and a runner with the JARVIS toolchain
(tracked as CI-1/CI-2 in [docs/integration/remaining-tasks.md](docs/integration/remaining-tasks.md)). Run it
manually:

1. **Stage 1 - Automated Gates** (lint, typecheck, test, build, vuln scan, secret scan)
2. **Stage 2 - Cross Review** (security, arch, ops subagents in parallel)
3. **Stage 3 - Merge Gate** (re-run Stage 1, then auto-merge)

```bash
# Runtime-agnostic adapter (JARVIS_REVIEW_RUNNER, default: opencode)
bash ci/scripts/review-run.sh "/review-flow gate-check <pr-number>" --json
bash ci/scripts/review-run.sh "/cross-review"
bash ci/scripts/review-run.sh "/review-flow metrics --days 30 --json"
```

### Operational Scripts

```bash
# Deploy/verify jarvis-mcp-gw locally (dry-run by default; --apply needs root)
bash infra/scripts/deploy-mcp-gw-safe.sh
sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply
bash infra/scripts/check-deploy-drift.sh

# View logs
sudo journalctl -u jarvis-policy -u jarvis-mcp-gw -u jarvis-event -u jarvis-context -f

# Backup: 保存先未決定のため未運用（ADR-0004）。
# rust/scripts/backup.rs / restore.rs は OCI Object Storage 前提のレガシー。
```

## Key Commands

| Task | Command |
|------|---------|
| Build all Rust services | `cd rust && cargo build --release --workspace` |
| Run tests | `cd rust && cargo test --workspace` |
| Lint | `cd rust && cargo clippy --workspace -- -D warnings` |
| Format check | `cd rust && cargo fmt --all --check` |
| Deploy jarvis-mcp-gw | `bash infra/scripts/deploy-mcp-gw-safe.sh`（適用は `sudo ... --apply`） |
| Deploy drift check | `bash infra/scripts/check-deploy-drift.sh` |
| Run gate-check | `bash ci/scripts/review-run.sh "/review-flow gate-check <pr>" --json` |
| Run cross-review | `bash ci/scripts/review-run.sh "/cross-review"` |
| Build/run approval UI | `make ui-build` / `make ui-run`（詳細は [ui/README.md](ui/README.md)） |
| Verify UI gates | `make ui-test` |
| Local CI gates (CI 停止時の代替) | `make ci-local` |
| Secret scan (値を出力しない) | `make secret-scan` |
| Backup preflight (read-only) | `make backup-preflight` |

## Service Ports (localhost only, accessed via Tailscale)

| Service | Port | Health Endpoint |
|---------|------|-----------------|
| jarvis-policy | 50051 (gRPC), 8081 (HTTP) | http://localhost:8081/health |
| jarvis-mcp-gw | 50052 (gRPC), 8082 (HTTP) | http://localhost:8082/health |
| jarvis-event | 50053 (gRPC), 8083 (HTTP), 8085 (webhooks) | http://localhost:8083/health |
| jarvis-context | 50054 (gRPC), 8084 (HTTP) | http://localhost:8084/health |
| OpenCode V2 | 49374 (Web) | http://localhost:49374 |
| code-server | 8080 | http://localhost:8080 |
| OpenTelemetry | 4317 (gRPC), 4318 (HTTP) | — |

## Configuration

Rust services receive secrets from **Bitwarden Secrets Manager** via
`bws run`, using the root-owned `/etc/jarvis/bws.env` (`BWS_ACCESS_TOKEN`).
`otelcol` may still use 1Password (`op run`) until migrated.

The `infra/config/*.env` files are reference templates and retain legacy
`op://` references; the current systemd units do not use them directly.

Config files live in `/etc/jarvis/` on the local host and are provisioned by the
deploy scripts.

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
- Audit logging to PostgreSQL（OCI は使用しない）

## References

- [ADR-0001: Unified Architecture](adr/0001-unified-architecture.md)
- [ADR-0004: Local-only (no OCI)](adr/0004-local-only-no-oci.md)
- [ADR-0005: GPUI desktop adoption](adr/0005-gpui-desktop-adoption.md)
- [OpenCode V2 Docs](https://opencode.ai/v2/docs)
- [Cedar Policy Language](https://www.cedarpolicy.com/)
- [rmcp - Rust MCP SDK](https://github.com/rmcp-rs/rmcp)
