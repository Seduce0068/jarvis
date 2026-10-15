#!/usr/bin/env bash
# Compatibility launcher; backup implementation lives in rust/scripts/backup.rs.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
exec rust-script "$REPO_ROOT/rust/scripts/backup.rs" "$@"
