#!/usr/bin/env bash
# Compatibility launcher for the LEGACY restore implementation.
#
# Fails closed for the same reasons as backup.sh: the implementation
# (`rust/scripts/restore.rs`) defaults to OCI/rclone/op, and a confirmed run
# overwrites configs, PostgreSQL, Qdrant and repositories and restarts
# services. The restore design is undecided (BK-1 / BK-2 / SEC-2).
set -euo pipefail

cat >&2 <<'MSG'
ERROR: the legacy restore launcher is disabled (ADR-0004 / BK-1).

A confirmed restore overwrites configs, databases and repositories and
restarts services; the old implementation also assumes OCI/rclone/op,
which is out of policy here. No restore path has been validated.

Inspect the legacy implementation directly (read-only):
  rust-script rust/scripts/restore.rs --help

See docs/integration/backup-redesign-notes.md for the redesign status.
MSG
exit 2
