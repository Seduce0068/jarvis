#!/usr/bin/env bash
# Markdown lint gate for tracked Markdown files (R2/R5).
#
# Usage:
#   bash ci/scripts/lint-md.sh [--all] [--staged]
#
# Modes:
#   working (default): tracked *.md changed between MD_LINT_BASE (default HEAD)
#                      and HEAD, restricted to files that still exist.
#                      `git diff --name-only --diff-filter=ACMR "$base"` (base→作業ツリー。HEAD を省略して未コミット変更も対象にする。CI のクリーン checkout では作業ツリー＝HEAD のため base→HEAD と同義).
#   staged (--staged or MD_LINT_MODE=staged): staged *.md plus untracked *.md,
#                      for pre-commit use.
#   --all: every tracked *.md (baseline included); base and mode are ignored
#          when no conflicting staged request is present.
# --all and --staged are mutually exclusive (usage error / fail-closed).
#
# Git paths are fetched with core.quotepath=false and -z (NUL-delimited) and
# read with mapfile -d '' -t, so quoted/non-ASCII/newline paths are never
# silently excluded.
#
# The npx invocation is pinned via MARKDOWNLINT_VERSION (default 0.50.1,
# measured 2026-11-28) for both the --version probe and the real lint; no
# auto-fix, matching hooks.json's intent but with a reproducible tool version.
#
# Output contract: only "<file>: <rule-id>" lines on stdout. Violation text,
# expected/actual values and tool stderr are never emitted (they may contain
# file contents). A clean run emits one summary line at most. Infrastructure
# errors (mktemp, git, npx, markdownlint) fail closed with one stderr line.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo" || exit 1

# Private scratch directory: a predictable /tmp path could be symlink-poisoned
# by another user. The EXIT trap cleans up on success, failure and early exit.
tmpdir="$(mktemp -d)" || { echo "FAIL: mktemp -d failed" >&2; exit 1; }
trap 'rm -rf "$tmpdir"' EXIT

die() {
  echo "FAIL: $1" >&2
  exit 1
}

all=0
staged_cli=0
case "${1:-}" in
  "") ;;
  --all) all=1 ;;
  --staged) staged_cli=1 ;;
  *) die "usage: bash ci/scripts/lint-md.sh [--all] [--staged]" ;;
esac
if [ "$#" -gt 1 ]; then
  die "usage: bash ci/scripts/lint-md.sh [--all] [--staged]"
fi

mode="${MD_LINT_MODE:-working}"
case "$mode" in
  working|staged) ;;
  *) die "invalid MD_LINT_MODE" ;;
esac
staged=0
if [ "$staged_cli" -eq 1 ] || [ "$mode" = "staged" ]; then
  staged=1
fi
if [ "$all" -eq 1 ] && [ "$staged" -eq 1 ]; then
  die "usage: --all and --staged are mutually exclusive"
fi

# The base is only used by the working/diff mode. Validate it before any diff
# so an invalid (e.g. not-yet-fetched) ref fails closed instead of turning the
# run into a silent no-op on a clean CI checkout.
base="${MD_LINT_BASE:-HEAD}"
if [ "$all" -eq 0 ] && [ "$staged" -eq 0 ]; then
  if ! git rev-parse --verify "${base}^{commit}" >/dev/null 2>&1; then
    die "invalid MD_LINT_BASE"
  fi
fi

# Resolve the target list first. A failing git command fails closed instead of
# being mistaken for "no changes". core.quotepath=false + -z keeps paths raw;
# pathspec '*.md' restricts this to Markdown only.
list="$tmpdir/files"
if [ "$all" -eq 1 ]; then
  if ! git -c core.quotepath=false ls-files -z -- '*.md' > "$list" 2>/dev/null; then
    die "git ls-files '*.md' failed"
  fi
elif [ "$staged" -eq 1 ]; then
  if ! git -c core.quotepath=false diff --cached --name-only --diff-filter=ACMR -z HEAD -- '*.md' > "$list" 2>/dev/null; then
    die "git diff --cached --diff-filter=ACMR HEAD failed"
  fi
  if ! git -c core.quotepath=false ls-files --others --exclude-standard -z -- '*.md' >> "$list" 2>/dev/null; then
    die "git ls-files --others --exclude-standard '*.md' failed"
  fi
else
  if ! git -c core.quotepath=false diff --name-only --diff-filter=ACMR -z "$base" -- '*.md' > "$list" 2>/dev/null; then
    die "git diff --name-only --diff-filter=ACMR failed"
  fi
fi

# Real git -z output always NUL-terminates every entry, so a newline inside a
# filename stays in one field. A shim that ignores -z emits plain newline
# separated names; detect that fallback without splitting genuine NUL fields.
if [ "$(LC_ALL=C tr -cd '\000' < "$list" | wc -c)" -gt 0 ]; then
  mapfile -d '' -t raw < "$list"
else
  mapfile -t raw < "$list"
fi

# Keep only existing files (a rename/deletion must not be handed to npx) and
# preserve each path byte-for-byte, including newlines and non-ASCII names.
files=()
for f in "${raw[@]}"; do
  [ -n "$f" ] || continue
  if [ -f "$f" ]; then
    files+=("$f")
  fi
done
if [ "${#files[@]}" -eq 0 ]; then
  exit 0
fi

# Tool availability is checked only when work is actually due, so a no-op run
# stays silent and successful even on a host without npx. Both the capability
# probe and the real lint use the same pinned package spec.
npx_cmd=(npx --yes "markdownlint@${MARKDOWNLINT_VERSION:-0.50.1}")
if ! command -v npx >/dev/null 2>&1; then
  die "npx is not available"
fi
if ! "${npx_cmd[@]}" --version >/dev/null 2>&1; then
  die "markdownlint is not runnable via npx"
fi

out="$tmpdir/markdownlint.out"
err="$tmpdir/markdownlint.err"
if "${npx_cmd[@]}" "${files[@]}" > "$out" 2> "$err"; then
  echo "markdownlint: ${#files[@]} file(s) clean"
  exit 0
fi

# markdownlint emits "<file>:<line>:<column> <rule>/<alias> <description>".
# Strictly accept only a colon-free *.md path followed by "N:N<space>", then
# take just the leading MDxxxx token of the first token. Description/values,
# stderr and every non-conforming line are dropped. Empty findings fail closed.
findings="$(
  awk '
    {
      path = $0
      sub(/:.*/, "", path)
      if (path !~ /^[^:]+\.md$/) next
      rest = $0
      sub(/^[^:]*:/, "", rest)
      if (rest !~ /^[0-9]+:[0-9]+[[:space:]]+/) next
      sub(/^[0-9]+:[0-9]+[[:space:]]+/, "", rest)
      token = rest
      sub(/[[:space:]].*$/, "", token)
      if (match(token, /^MD[0-9]+/)) {
        print path ": " substr(token, 1, RLENGTH)
      }
    }
  ' "$out" | sort -u
)" || die "markdownlint execution failed"
if [ -z "$findings" ]; then
  die "markdownlint execution failed"
fi

printf '%s\n' "$findings"
exit 1
