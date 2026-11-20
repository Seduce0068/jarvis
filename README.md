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
- **Infrastructure:** 自宅 Kubuntu PC 上の systemd、Bitwarden Secrets Manager
  (`bws`)、Tailscale。OCI は使用しない（[ADR-0004](adr/0004-local-only-no-oci.md)）

The unified decision is recorded in
[adr/0001-unified-architecture.md](adr/0001-unified-architecture.md).

## Layout

| Path | Contents |
| --- | --- |
| `rust/` | Cargo workspace: `jarvis-core` plus the four sidecar services |
| `ui/` | GPUI approval console (standalone crate; see [ui/README.md](ui/README.md)) |
| `infra/` | systemd units, deploy/verify scripts, configs（OpenTofu/cloud-init は未使用レガシー） |
| `adr/` | Architecture decision records |
| `scripts/` | rust-script tooling (bootstrap, doctor, phone status) |
| `config/` | Shell/git locale helpers and environment templates |
| `docs/` | Architecture reports, calculations, and verification scripts |
| `ci/` | Review-flow pipeline definitions |
| `Makefile` | Common tasks incl. `ci-local`, `secret-scan`, `ui-*` |

## Verification

```bash
cd rust
cargo fmt --all --check
cargo check --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings -D clippy::undocumented_unsafe_blocks

# ローカル配置用バイナリ（x86_64）
cargo build --release --workspace --locked
```

## Deployment

OCI は使用しない（[ADR-0004](adr/0004-local-only-no-oci.md)）。実行基盤は
自宅 Kubuntu PC 上の systemd で、サービスは `127.0.0.1` bind、外部アクセスは
Tailscale 経由。手順は [infra/README.md](infra/README.md) を参照。

```bash
# jarvis-mcp-gw の安全デプロイ（乾燥実行が既定。適用は root が必要）
bash infra/scripts/deploy-mcp-gw-safe.sh
sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply
bash infra/scripts/check-deploy-drift.sh
```

## Development conventions

See [AGENTS.md](AGENTS.md) for repository-specific workflows, service ports,
and operational commands.
