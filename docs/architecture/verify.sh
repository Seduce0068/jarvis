#!/usr/bin/env bash
# Compatibility launcher. Verification logic is implemented in verify.rs.
set -euo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec rust-script "$SCRIPT_DIR/verify.rs" "$SCRIPT_DIR"
