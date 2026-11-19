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
# Caller contract (runtime-independent): argv[1] is the prompt, including the
# skill subcommand (e.g. "/review-flow gate-check 42"), and the only optional
# flag is --json. build_runner_args() maps that contract to each runner's
# native CLI shape, so switching backends requires no workflow/prompt change:
#   opencode (default): run <prompt> [--json]
#   dsh               : --profile <profile> [--json] <prompt>
#   other             : run <prompt> [--json]  (legacy-compatible default)
set -euo pipefail

prompt="${1:?usage: review-run.sh '<prompt>' [--json]}"
shift

runner="${JARVIS_REVIEW_RUNNER:-opencode}"
if ! command -v "$runner" >/dev/null 2>&1; then
  echo "ERROR: review runner '$runner' not found on PATH" >&2
  exit 127
fi

want_json=0
for arg in "$@"; do
  case "$arg" in
    --json) want_json=1 ;;
    *) echo "ERROR: unsupported flag: $arg" >&2; exit 2 ;;
  esac
done

# build_runner_args <runner> <prompt> <want_json>
# Sets the global array runner_args (argv after the executable name).
build_runner_args() {
  local name="$1" task="$2" json="$3"

  runner_args=()
  case "$name" in
    dsh)
      # The one-shot headless app owns --json (newline-delimited run events);
      # its help shows the flag before the task. The profile is overridable
      # for credential/policy overlays without editing this adapter.
      runner_args=(--profile "${JARVIS_REVIEW_RUNNER_PROFILE:-headless}")
      if [ "$json" -eq 1 ]; then
        runner_args+=(--json)
      fi
      runner_args+=("$task")
      ;;
    *)
      # OpenCode's native shape, retained as the default for generic runners
      # so existing CI steps and the stub contract tests keep working.
      runner_args=(run "$task")
      if [ "$json" -eq 1 ]; then
        runner_args+=(--json)
      fi
      ;;
  esac
}

build_runner_args "$runner" "$prompt" "$want_json"
exec "$runner" "${runner_args[@]}"
