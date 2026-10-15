#!/usr/bin/env bash
# Compatibility launcher. Bootstrap logic is implemented in scripts/bootstrap/bootstrap.rs.
set -euo pipefail
PROJECT_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec rust-script "$PROJECT_ROOT/scripts/bootstrap/bootstrap.rs" --source-root "$PROJECT_ROOT" "$@"
