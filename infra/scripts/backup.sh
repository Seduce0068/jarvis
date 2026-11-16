#!/usr/bin/env bash
# Compatibility launcher for the LEGACY backup implementation.
#
# This launcher intentionally FAILS CLOSED. The implementation it used to
# delegate to (`rust/scripts/backup.rs`) is not safe to run in the current
# local-only setup (ADR-0004):
#
#   - defaults to `--remote oci` (rclone) and `op inject` for credentials;
#   - `--dry-run` only skips remote upload/cleanup — local files are still
#     written and `cleanup_local()` still DELETES files older than 7 days;
#   - the backup destination / encryption / restore design is undecided
#     (BK-1 / BK-2 / SEC-2), so no periodic job is installed.
#
# For a read-only prerequisite check use `backup-preflight.sh`.
# See docs/integration/backup-redesign-notes.md for the redesign status.
set -euo pipefail

cat >&2 <<'MSG'
ERROR: the legacy backup launcher is disabled (ADR-0004 / BK-1).

Why: the old implementation defaults to OCI/rclone/op and still deletes
local files older than 7 days even with --dry-run. Running it would risk
unwanted deletion and an out-of-policy upload.

Use instead:
  bash infra/scripts/backup-preflight.sh   # read-only prerequisite check
  make backup-preflight                    # same, via Makefile

The legacy implementation itself is retained for reference only and can be
inspected directly with:
  rust-script rust/scripts/backup.rs --help
MSG
exit 2
