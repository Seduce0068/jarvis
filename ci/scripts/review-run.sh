#!/usr/bin/env bash
# CI-2: runtime-agnostic adapter for review-flow commands.
#
# The review pipeline must not hardcode a specific agent-runtime CLI (ADR-0002).
# This wrapper centralises the invocation so the backend can be swapped via
# JARVIS_REVIEW_RUNNER without touching the workflow definition.
#
# Usage:
#   bash ci/scripts/review-run.sh "<prompt>" [--json]
#
# Contract for a runner: it must accept `run <prompt> [--json]` and write
# the result to stdout (JSON when --json is passed). The default backend is
# `opencode`; CI supplies credentials for whichever runner is configured.
set -euo pipefail

prompt="${1:?usage: review-run.sh '<prompt>' [--json]}"
shift || true

runner="${JARVIS_REVIEW_RUNNER:-opencode}"
if ! command -v "$runner" >/dev/null 2>&1; then
  echo "ERROR: review runner '$runner' not found on PATH" >&2
  exit 127
fi

# shellcheck disable=SC2086  # $extra is intentionally a flag list
extra=()
for arg in "$@"; do
  case "$arg" in
    --json) extra+=(--json) ;;
    *) echo "ERROR: unsupported flag: $arg" >&2; exit 2 ;;
  esac
done

exec "$runner" run "$prompt" "${extra[@]}"
