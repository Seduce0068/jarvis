#!/usr/bin/env bash
# Source this compatibility shim to import locale settings and aliases.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
eval "$(rust-script "$script_dir/shell-locale.rs")"
unset script_dir
