#!/usr/bin/env bash
# Compatibility launcher. Bootstrap logic is implemented in bootstrap.rs.
set -euo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
exec rust-script "$SCRIPT_DIR/bootstrap.rs" --target termux --source-root "$PROJECT_ROOT" "$@"
