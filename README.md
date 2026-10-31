# JARVIS

JARVIS is a personal AI system built on OpenCode V2 as the agent runtime,
with Rust sidecar services enforcing security and context boundaries.

## Architecture

- **Agent runtime:** OpenCode V2 (`opencode serve`)
- **Rust sidecars:**
  - `jarvis-policy` — authorization (Cedar/Casbin), approvals, kill switch
  - `jarvis-mcp-gw` — MCP tool allowlist, policy checks, sandboxing
  - `jarvis-event` — Home Assistant / Android / webhook event normalization
  - `jarvis-context` — PostgreSQL + Qdrant memory, PII filtering
- **Infrastructure:** OCI Always Free (Ampere A1), OpenTofu, systemd,
  1Password CLI, Tailscale

The unified decision is recorded in
[adr/0001-unified-architecture.md](adr/0001-unified-architecture.md).

## Layout

| Path | Contents |
| --- | --- |
| `rust/` | Cargo workspace: `jarvis-core` plus the four sidecar services |
| `infra/` | OpenTofu modules, cloud-init, systemd units, deployed configs |
| `adr/` | Architecture decision records |
| `scripts/` | rust-script tooling (bootstrap, doctor, phone status) |
| `config/` | Shell/git locale helpers and environment templates |
| `docs/` | Architecture reports, calculations, and verification scripts |
| `ci/` | Review-flow pipeline definitions |
| `Makefile` | Common tasks (bootstrap, doctor, test, lint, deploy) |

## Verification

```bash
cd rust
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Deployment

See [infra/README.md](infra/README.md). Production services bind to
`127.0.0.1` and are reached over Tailscale.

## Development conventions

See [AGENTS.md](AGENTS.md) for repository-specific workflows, service ports,
and operational commands.
