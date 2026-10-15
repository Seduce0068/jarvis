#!/usr/bin/env bash
# Compatibility launcher; deploy implementation lives in rust/scripts/deploy.rs.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
HOST="${1:-jarvis-host}"
COMMAND="${2:-deploy}"
shift "$(( $# > 0 ? 1 : 0 ))"
shift "$(( $# > 0 ? 1 : 0 ))"
exec rust-script "$REPO_ROOT/rust/scripts/deploy.rs" \
  --rust-dir "$REPO_ROOT/rust" "$HOST" "$COMMAND" "$@"
