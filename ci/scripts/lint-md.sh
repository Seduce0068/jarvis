#!/usr/bin/env bash
# Markdown lint gate for tracked Markdown files (R2/R5).
#
# Usage:
#   bash ci/scripts/lint-md.sh [--all] [--staged]
#
# Modes:
#   working (default): tracked *.md changed between MD_LINT_BASE (default HEAD)
#                      and the worktree, plus untracked *.md.
#   staged (--staged or MD_LINT_MODE=staged): staged *.md blobs are expanded
#                      from the index (`git show :<path>`) into a private
#                      scratch dir; untracked *.md are passed as real files.
#                      Findings are mapped back by dropping the scratch prefix.
#   --all: every tracked *.md (baseline included); base and mode are ignored.
# --all and --staged are mutually exclusive (usage error / fail-closed).
#
# Git paths are fetched with core.quotepath=false and -z (NUL-delimited) and
# read with mapfile -d '' -t, so quoted/non-ASCII/newline paths are never
# silently excluded.
#
# The npx invocation is pinned via MARKDOWNLINT_VERSION (default 0.50.1,
# measured 2026-11-28) to the real package `markdownlint-cli` for BOTH the
# --version probe and the real lint. `markdownlint@...` is a different,
# non-existent spec on npm and would fail in CI. The probe's reported version
# must match the pin; an unparseable or mismatched probe fails closed.
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
files_raw="$tmpdir/files"
untracked_raw="$tmpdir/untracked"
: > "$files_raw"
: > "$untracked_raw"
if [ "$all" -eq 1 ]; then
  if ! git -c core.quotepath=false ls-files -z -- '*.md' > "$files_raw" 2>/dev/null; then
    die "git ls-files '*.md' failed"
  fi
elif [ "$staged" -eq 1 ]; then
  if ! git -c core.quotepath=false diff --cached --name-only --diff-filter=ACMR -z HEAD -- '*.md' > "$files_raw" 2>/dev/null; then
    die "git diff --cached --diff-filter=ACMR HEAD failed"
  fi
  if ! git -c core.quotepath=false ls-files --others --exclude-standard -z -- '*.md' > "$untracked_raw" 2>/dev/null; then
    die "git ls-files --others --exclude-standard '*.md' failed"
  fi
else
  if ! git -c core.quotepath=false diff --name-only --diff-filter=ACMR -z "$base" -- '*.md' > "$files_raw" 2>/dev/null; then
    die "git diff --name-only --diff-filter=ACMR failed"
  fi
  # Untracked Markdown is invisible to git diff; include it explicitly.
  if ! git -c core.quotepath=false ls-files --others --exclude-standard -z -- '*.md' > "$untracked_raw" 2>/dev/null; then
    die "git ls-files --others --exclude-standard '*.md' failed"
  fi
fi

# Real git -z output always NUL-terminates every entry, so a newline inside a
# filename stays in one field. A shim that ignores -z emits plain newline
# separated names; detect that fallback without splitting genuine NUL fields.
normalize() { # <src> <dst>
  local src="$1" dst="$2" line
  if [ "$(LC_ALL=C tr -cd '\000' < "$src" | wc -c)" -gt 0 ]; then
    cp "$src" "$dst"
  else
    : > "$dst"
    while IFS= read -r line || [ -n "$line" ]; do
      [ -n "$line" ] || continue
      printf '%s\0' "$line" >> "$dst"
    done < "$src"
  fi
}
norm_tracked="$tmpdir/files.norm"
norm_untracked="$tmpdir/untracked.norm"
normalize "$files_raw" "$norm_tracked"
normalize "$untracked_raw" "$norm_untracked"
mapfile -d '' -t tracked < "$norm_tracked"
mapfile -d '' -t untracked < "$norm_untracked"

# In staged mode each staged path is expanded from the index blob, NOT from the
# worktree (which may have been edited after staging). Untracked files have no
# index blob and are passed as real files. Deduplicate either way.
staged_root=''
prefix=''
if [ "$staged" -eq 1 ]; then
  staged_root="$tmpdir/staged"
  mkdir -p "$staged_root" || die "cannot create staged scratch dir"
  prefix="$staged_root/"
fi

declare -A seen=()
files=()
for f in "${tracked[@]}"; do
  [ -n "$f" ] || continue
  if [ "$staged" -eq 1 ]; then
    dest="$staged_root/$f"
    mkdir -p "$(dirname "$dest")" || die "cannot create staged path dir"
    if ! git show ":$f" > "$dest"; then
      die "git show :$f failed"
    fi
    if [ -n "${seen["$f"]+x}" ]; then
      continue
    fi
    seen["$f"]=1
    files+=("$dest")
  else
    [ -f "$f" ] || continue
    if [ -n "${seen["$f"]+x}" ]; then
      continue
    fi
    seen["$f"]=1
    files+=("$f")
  fi
done
for f in "${untracked[@]}"; do
  [ -n "$f" ] || continue
  [ -f "$f" ] || continue
  if [ -n "${seen["$f"]+x}" ]; then
    continue
  fi
  seen["$f"]=1
  files+=("$f")
done
# A few legacy docs carry a large pre-existing markdownlint baseline (P3-4):
# todo.md (task log), unknown-tasks-survey.md and local-operations-runbook.md
# (MD013/MD060 throughout). Until that baseline is resolved, any edit to them
# would block the gate. Exclude only these basenames; all other Markdown is
# still checked. Override with MD_LINT_EXCLUDE="" for a full check.
IFS=':' read -r -a excludes <<< "${MD_LINT_EXCLUDE:-todo.md:unknown-tasks-survey.md:local-operations-runbook.md}"
if [ "${#excludes[@]}" -gt 0 ]; then
  kept=()
  for f in "${files[@]}"; do
    base="${f##*/}"
    skip=0
    for x in "${excludes[@]}"; do
      if [ -n "$x" ] && [ "$base" = "$x" ]; then skip=1; break; fi
    done
    [ "$skip" -eq 1 ] && continue
    kept+=("$f")
  done
  files=("${kept[@]}")
fi
if [ "${#files[@]}" -eq 0 ]; then
  exit 0
fi

# Tool availability is checked only when work is actually due, so a no-op run
# stays silent and successful even on a host without npx. Both the capability
# probe and the real lint use the same pinned package spec and command array.
pin="markdownlint-cli@${MARKDOWNLINT_VERSION:-0.50.1}"
pin_version="${pin#markdownlint-cli@}"
npx_cmd=(npx --yes "$pin")
if ! command -v npx >/dev/null 2>&1; then
  die "npx is not available"
fi
probe_out="$tmpdir/probe.out"
if ! "${npx_cmd[@]}" --version > "$probe_out" 2>/dev/null; then
  die "markdownlint is not runnable via npx"
fi
probe_version="$(LC_ALL=C grep -Eo '[0-9]+\.[0-9]+\.[0-9]+' "$probe_out" 2>/dev/null | head -n 1 || true)"
if [ -z "$probe_version" ] || [ "$probe_version" != "$pin_version" ]; then
  die "markdownlint version mismatch"
fi

out="$tmpdir/markdownlint.out"
# markdownlint-cli writes findings to STDERR (measured 2026-11-28), so both
# streams must be parsed. Only "<file>: <rule-id>" is ever printed below.
if "${npx_cmd[@]}" "${files[@]}" > "$out" 2>&1; then
  echo "markdownlint: ${#files[@]} file(s) clean"
  exit 0
fi

# markdownlint emits "<file>:<line>:<column> <rule>/<alias> <description>".
# Strictly accept only a colon-free *.md path followed by "N:N<space>", then
# take just the leading MDxxxx token of the first token. In staged mode a
# scratch prefix is stripped so the original path is reported. Description/
# values, stderr and every non-conforming line are dropped. Empty findings
# fail closed.
findings="$(
  awk -v prefix="$prefix" '
    {
      path = $0
      sub(/:.*/, "", path)
      if (path !~ /^[^:]+\.md$/) next
      if (prefix != "" && index(path, prefix) == 1) {
        path = substr(path, length(prefix) + 1)
      }
      if (path == "") next
      rest = $0
      sub(/^[^:]*:/, "", rest)
      if (rest !~ /^[0-9]+:[0-9]+[[:space:]]+/) next
      sub(/^[0-9]+:[0-9]+[[:space:]]+/, "", rest)
      # Real markdownlint-cli prefixes the rule with a severity word
      # ("error MD013/..."); the stub originally omitted it. Match the first
      # MDxxxx token anywhere in the remainder instead of assuming token 1.
      if (match(rest, /MD[0-9]+/)) {
        print path ": " substr(rest, RSTART, RLENGTH)
      }
    }
  ' "$out" | sort -u
)" || die "markdownlint execution failed"
if [ -z "$findings" ]; then
  die "markdownlint execution failed"
fi

printf '%s\n' "$findings"
exit 1
