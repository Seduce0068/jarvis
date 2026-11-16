#!/usr/bin/env bash
# Compatibility launcher for the LEGACY remote deploy implementation.
#
# Fails closed: `rust/scripts/deploy.rs` targets a remote `jarvis-host`
# (Tailscale/OCI-era workflow). ADR-0004 forbids OCI and the current runtime
# is local systemd; use `deploy-mcp-gw-safe.sh` (dry-run by default) instead.
set -euo pipefail

cat >&2 <<'MSG'
ERROR: the legacy remote deploy launcher is disabled (ADR-0004).

Use the local, dry-run-by-default deployment instead:
  bash infra/scripts/deploy-mcp-gw-safe.sh
  sudo bash infra/scripts/deploy-mcp-gw-safe.sh --apply

The legacy implementation is retained for reference and can be inspected:
  rust-script rust/scripts/deploy.rs --help
MSG
exit 2
