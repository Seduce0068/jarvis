#!/usr/bin/env bash
# Compatibility launcher. Diagnostic logic is implemented in jarvis-doctor.rs.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
rust-script "$script_dir/jarvis-doctor.rs" "$@"
