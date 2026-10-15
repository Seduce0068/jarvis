#!/usr/bin/env bash
# Compatibility launcher; may be sourced as in the previous version.
(
  script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
  rust-script "$script_dir/git-utf8.rs" "$@"
)
