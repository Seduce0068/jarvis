#!/data/data/com.termux/files/usr/bin/bash
# Compatibility launcher. Device status logic is implemented in phone-status-json.rs.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
rust-script "$script_dir/phone-status-json.rs" "$@"
