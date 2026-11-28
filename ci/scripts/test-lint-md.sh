#!/usr/bin/env bash
# Regression tests for ci/scripts/lint-md.sh (R5 contract).
#
# Contract under test:
#   bash ci/scripts/lint-md.sh [--all]
#   - default: tracked *.md from a git diff against ${MD_LINT_BASE:-HEAD};
#     zero files -> exit 0
#   - --all: every tracked *.md
#   - MD_LINT_MODE=staged: git diff --cached plus
#     git ls-files --others --exclude-standard (untracked files included)
#   - git listings use `git -c core.quotepath=false ... -z` (NUL-safe names)
#   - MARKDOWNLINT_VERSION pins npx to markdownlint@<version>
#   - invalid MD_LINT_BASE fails via `git rev-parse --verify`
#   - npx/markdownlint unavailable, empty findings, or git failure ->
#     fail-closed exit 1
#   - output may contain file names and rule IDs only, never the offending
#     Markdown body (stdout and stderr combined)
#
# All git/npx calls are served by stubs on a temporary PATH: no network, no
# production repository access and no real npx invocation. The target script
# is copied into a throwaway mini repository so script-relative and CWD-based
# repo resolution both land on the same inert tree. On failure only an error
# code and the case id are printed: captured output, file contents and the
# sentinel value are never echoed.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
gate="$repo/ci/scripts/lint-md.sh"

if [ ! -f "$gate" ]; then
  printf 'FAIL: GATE_MISSING\n'
  exit 1
fi

tmp="$(mktemp -d)" || { printf 'FAIL: TMP_UNAVAILABLE\n'; exit 1; }
trap 'rm -rf "$tmp"' EXIT

ok=0
fail=0
sentinel='PROBE_DO_NOT_LEAK'
bad_file='docs/bad.md'
ok_file='docs/ok.md'
all_only_file='docs/all-only.md'
untracked_file='docs/untracked.md'
staged_file='docs/staged.md'
diff_files=''
all_files=''

# Per-case overrides. They are exported only when explicitly set so an empty
# value can never be mistaken for an intentionally configured base/mode.
md_lint_base=''
md_lint_mode=''
markdownlint_version=''
stub_valid_base='HEAD'
stub_untracked_files=''
path_override=''

bash_bin="$(command -v bash)"

reset_overrides() {
  md_lint_base=''
  md_lint_mode=''
  markdownlint_version=''
  stub_valid_base='HEAD'
  stub_untracked_files=''
  path_override=''
}

mini="$tmp/mini"
bin="$tmp/bin"
mkdir -p "$mini/ci/scripts" "$mini/docs" "$mini/src" "$bin"
gate_copy="$mini/ci/scripts/lint-md.sh"
cp "$gate" "$gate_copy"

# Fixture files. The bad one carries the sentinel so a script that echoes
# Markdown content leaks it; the npx stub also emits it in canned lint output
# to exercise the same pass-through path.
printf '# ok\n\nclean line\n' > "$mini/$ok_file"
printf '# bad\n\n%s here\n' "$sentinel" > "$mini/$bad_file"
printf '# all only\n\nclean line\n' > "$mini/$all_only_file"
printf '# untracked\n\nclean line\n' > "$mini/$untracked_file"
printf '# staged\n\nclean line\n' > "$mini/$staged_file"
printf 'fn main() {}\n' > "$mini/src/main.rs"

git_log="$tmp/git-calls.log"
npx_log="$tmp/npx-calls.log"
: > "$git_log"
: > "$npx_log"

# --- git stub: canned tracked-file lists, dispatched by real subcommand -----
# Emits NUL-delimited names when the gate passes -z, newline otherwise, so the
# same stub serves both the R2 newline contract and the R5 quotepath/-z path.
cat > "$bin/git" <<'STUB'
#!/usr/bin/env bash
set -u
if [ -n "${STUB_GIT_LOG:-}" ]; then
  printf '%s\n' "$*" >> "$STUB_GIT_LOG"
fi
if [ "${STUB_GIT_MODE:-}" = fail ]; then
  printf 'git stub failure\n' >&2
  exit 1
fi

z=0
others=0
verify=0
cmd=''
for arg in "$@"; do
  case "$arg" in
    -z) z=1 ;;
    --others) others=1 ;;
    --verify) verify=1 ;;
    diff) cmd='diff' ;;
    ls-files|ls-tree) cmd='all' ;;
    rev-parse) cmd='rev-parse' ;;
  esac
done

emit() { # list
  local list="$1" f
  [ -n "$list" ] || return 0
  if [ "$z" -eq 1 ]; then
    while IFS= read -r f; do
      [ -n "$f" ] || continue
      printf '%s\0' "$f"
    done <<< "$list"
  else
    printf '%s\n' "$list"
  fi
}

case "$cmd" in
  rev-parse)
    if [ "$verify" -eq 1 ]; then
      vb="${STUB_VALID_BASE:-HEAD}"
      for arg in "$@"; do
        case "$arg" in -*) continue ;; esac
        if [ "$arg" = "$vb" ] || [ "$arg" = "$vb^{commit}" ]; then
          printf '0123456789abcdef0123456789abcdef01234567\n'
          exit 0
        fi
      done
      printf 'git stub: bad revision\n' >&2
      exit 1
    fi
    for arg in "$@"; do
      if [ "$arg" = '--is-inside-work-tree' ]; then
        printf 'true\n'
        exit 0
      fi
    done
    printf '%s\n' "${STUB_REPO:-.}"
    ;;
  all)
    if [ "$others" -eq 1 ]; then
      emit "${STUB_UNTRACKED_FILES:-}"
    elif [ -n "${STUB_ALL_FILES:-}" ]; then
      emit "$STUB_ALL_FILES"
    fi
    ;;
  diff|*)
    emit "${STUB_DIFF_FILES:-}"
    ;;
esac
exit 0
STUB
chmod +x "$bin/git"

# --- npx stub: canned success / violation / unavailable / failure -----------
cat > "$bin/npx" <<'STUB'
#!/usr/bin/env bash
set -u
if [ -n "${STUB_NPX_LOG:-}" ]; then
  printf '%s\n' "$*" >> "$STUB_NPX_LOG"
fi
# --version is a capability probe (lint-md.sh:75) and succeeds on a real
# markdownlint regardless of the canned lint mode. Keep the stub faithful so
# the actual lint invocation (lint-md.sh:81) is exercised.
case "$*" in
  *--version*) exit 0 ;;
esac
case "${STUB_NPX_MODE:-ok}" in
  fail127)
    # Simulates "command not found" even though the name exists on PATH.
    printf 'npx: command not found\n' >&2
    exit 127
    ;;
  fail)
    # stderr only, empty stdout, exit 1: exercises the empty-findings die path.
    printf 'markdownlint stub failure\n' >&2
    exit 1
    ;;
  violation)
    # Realistic markdownlint line: file, rule id, message and the offending
    # line as context. Only file name and rule id may survive filtering.
    printf '%s:3:1 %s/first-line-heading First line should be a top-level heading [Context: "%s"]\n' \
      "${STUB_BAD_FILE:-docs/bad.md}" 'MD041' "${STUB_SENTINEL:-PROBE_DO_NOT_LEAK}"
    exit 1
    ;;
esac
exit 0
STUB
chmod +x "$bin/npx"

# --- PATH with git stub and core utilities but no npx -----------------------
# Used to hit `command -v npx` directly: real npx is excluded by never adding
# the host PATH, while mktemp/awk/sort/dirname etc. are symlinked in so the
# gate reaches the availability check instead of dying at an earlier step.
bin_no_npx="$tmp/bin-no-npx"
mkdir -p "$bin_no_npx"
cp "$bin/git" "$bin_no_npx/git"
chmod +x "$bin_no_npx/git"
for tool in bash env dirname mktemp awk sort rm cat grep head tail tr cut wc \
  sed find readlink realpath chmod cp mv tee xargs ls date uname basename \
  touch mkdir ln; do
  p="$(command -v "$tool" 2>/dev/null || true)"
  [ -n "$p" ] || continue
  if [ -e "$bin_no_npx/$tool" ]; then
    continue
  fi
  ln -s "$p" "$bin_no_npx/$tool" 2>/dev/null || true
done

invoke() { # npx_mode git_mode [script args...]
  local npx_mode="$1" git_mode="$2"; shift 2
  local use_path="${path_override:-$bin:$PATH}"
  (
    cd "$mini" || exit 99
    export PATH="$use_path"
    unset -f npx 2>/dev/null || true
    export STUB_GIT_LOG="$git_log"
    export STUB_NPX_LOG="$npx_log"
    export STUB_GIT_MODE="$git_mode"
    export STUB_NPX_MODE="$npx_mode"
    export STUB_DIFF_FILES="$diff_files"
    export STUB_ALL_FILES="$all_files"
    export STUB_UNTRACKED_FILES="$stub_untracked_files"
    export STUB_BAD_FILE="$bad_file"
    export STUB_REPO="$mini"
    export STUB_SENTINEL="$sentinel"
    export STUB_VALID_BASE="$stub_valid_base"
    if [ -n "$md_lint_base" ]; then
      export MD_LINT_BASE="$md_lint_base"
    fi
    if [ -n "$md_lint_mode" ]; then
      export MD_LINT_MODE="$md_lint_mode"
    fi
    if [ -n "$markdownlint_version" ]; then
      export MARKDOWNLINT_VERSION="$markdownlint_version"
    fi
    "$bash_bin" "$gate_copy" "$@"
  )
}

# Only the exit code is reported here; output is intentionally discarded so a
# regressed script cannot leak the sentinel through this test's own messages.
check_case() { # id expected_exit npx_mode git_mode [script args...]
  local id="$1" expected="$2" npx_mode="$3" git_mode="$4"; shift 4
  local code problem=''
  : > "$git_log"
  : > "$npx_log"
  if invoke "$npx_mode" "$git_mode" "$@" >/dev/null 2>&1; then
    code=0
  else
    code=$?
  fi
  if [ "$code" -ne "$expected" ]; then
    problem='EXIT_CODE'
  fi
  if [ -n "$problem" ]; then
    printf 'FAIL: %s (%s exit=%s)\n' "$id" "$problem" "$code"
    fail=$((fail+1))
  else
    printf 'PASS: %s\n' "$id"
    ok=$((ok+1))
  fi
}

reset_overrides

# 1. No changed Markdown -> exit 0, with npx never called.
diff_files=''
check_case 'no-changed-md' 0 ok diff
if [ ! -s "$npx_log" ]; then
  printf 'PASS: no-changed-md-npx-not-invoked\n'
  ok=$((ok+1))
else
  printf 'FAIL: no-changed-md-npx-not-invoked (NPX_CALL)\n'
  fail=$((fail+1))
fi

# 2. A changed Markdown file with a lint violation -> exit 1, and the file is
#    actually handed to npx (so a script that always fails cannot pass).
diff_files="$bad_file"
check_case 'violation-fails' 1 violation diff
if grep -q -- "$bad_file" "$npx_log"; then
  printf 'PASS: violation-fails-npx-got-file\n'
  ok=$((ok+1))
else
  printf 'FAIL: violation-fails-npx-got-file (NPX_ARG_MISSING)\n'
  fail=$((fail+1))
fi

# 3. All changed Markdown clean -> exit 0.
diff_files="$ok_file"
check_case 'clean-passes' 0 ok diff

# 4. npx unavailable while work exists -> fail-closed exit 1.
diff_files="$bad_file"
check_case 'npx-unavailable-fails-closed' 1 fail127 diff

# 5. The sentinel embedded in the offending line must never reach combined
#    output (stdout+stderr), AND the filtered finding must really be present:
#    a regression that silently emits nothing fails this positive control.
sentinel_guard() {
  local out code=''
  : > "$git_log"; : > "$npx_log"
  if out="$(invoke violation diff 2>&1)"; then code=0; else code=$?; fi
  [ "$code" -eq 1 ] || return 1
  case "$out" in
    *"$sentinel"*) return 1 ;;
  esac
  case "$out" in
    *'First line should be a top-level heading'*) return 1 ;;
  esac
  case "$out" in
    *"$bad_file: MD041"*) ;;
    *) return 1 ;;
  esac
  return 0
}
if sentinel_guard; then
  printf 'PASS: sentinel-not-leaked\n'
  ok=$((ok+1))
else
  printf 'FAIL: sentinel-not-leaked (LEAK_OR_EXIT_OR_NO_FINDING)\n'
  fail=$((fail+1))
fi

# 6. --all targets every tracked *.md (verified via git and npx stub logs),
#    while the default mode stays scoped to the changed-file set.
all_guard() {
  local code=''
  diff_files="$bad_file"
  all_files="$ok_file
$all_only_file"
  : > "$git_log"; : > "$npx_log"
  if invoke ok diff --all >/dev/null 2>&1; then code=0; else code=$?; fi
  [ "$code" -eq 0 ] || return 1
  grep -qE -- 'ls-(files|tree)' "$git_log" || return 1
  grep -q -- "$ok_file" "$npx_log" || return 1
  grep -q -- "$all_only_file" "$npx_log" || return 1
  : > "$git_log"; : > "$npx_log"
  if invoke ok diff >/dev/null 2>&1; then code=0; else code=$?; fi
  [ "$code" -eq 0 ] || return 1
  if grep -q -- "$all_only_file" "$npx_log"; then return 1; fi
  return 0
}
if all_guard; then
  printf 'PASS: all-targets-tracked\n'
  ok=$((ok+1))
else
  printf 'FAIL: all-targets-tracked (ALL_SCOPE)\n'
  fail=$((fail+1))
fi

# 7. Additional fail-closed branch from the contract: git itself fails.
diff_files="$bad_file"
check_case 'git-failure-fails-closed' 1 ok fail

# 8. Empty findings: npx writes only stderr, no stdout, and exits 1. The gate
#    must die instead of reporting a clean run.
reset_overrides
diff_files="$bad_file"
check_case 'empty-findings-fails-closed' 1 fail diff

# 9. `command -v npx` branch: a PATH that contains the git stub and core
#    utilities but no npx must fail closed directly.
reset_overrides
path_override="$bin_no_npx"
diff_files="$bad_file"
check_case 'npx-missing-from-path-fails-closed' 1 ok diff
path_override=''

# 10. MD_LINT_BASE is forwarded to the git diff invocation.
reset_overrides
md_lint_base='refs/heads/base-X'
stub_valid_base='refs/heads/base-X'
diff_files="$ok_file"
: > "$git_log"; : > "$npx_log"
if invoke ok diff >/dev/null 2>&1; then base_code=0; else base_code=$?; fi
if [ "$base_code" -eq 0 ] && grep -E -- 'diff.*refs/heads/base-X' "$git_log" >/dev/null; then
  printf 'PASS: md-lint-base-forwarded\n'
  ok=$((ok+1))
else
  printf 'FAIL: md-lint-base-forwarded (BASE_NOT_FORWARDED exit=%s)\n' "$base_code"
  fail=$((fail+1))
fi
reset_overrides

# 11. An invalid base fails the git rev-parse --verify stub and exits 1.
md_lint_base='no-such-base-ref'
diff_files="$bad_file"
check_case 'invalid-base-fails-closed' 1 ok diff
reset_overrides

# 12. MD_LINT_MODE=staged calls git diff --cached and
#     git ls-files --others --exclude-standard, and the untracked file reaches
#     npx.
md_lint_mode='staged'
stub_untracked_files="$untracked_file"
diff_files="$staged_file"
: > "$git_log"; : > "$npx_log"
if invoke ok diff >/dev/null 2>&1; then staged_code=0; else staged_code=$?; fi
if [ "$staged_code" -eq 0 ] \
  && grep -q -- '--cached' "$git_log" \
  && grep -q -- '--others' "$git_log" \
  && grep -q -- '--exclude-standard' "$git_log" \
  && grep -q -- "$untracked_file" "$npx_log"; then
  printf 'PASS: staged-mode-scope\n'
  ok=$((ok+1))
else
  printf 'FAIL: staged-mode-scope (STAGED_SCOPE exit=%s)\n' "$staged_code"
  fail=$((fail+1))
fi
reset_overrides

# 13. MARKDOWNLINT_VERSION is reflected in the npx invocation.
markdownlint_version='9.9.9'
diff_files="$ok_file"
: > "$git_log"; : > "$npx_log"
if invoke ok diff >/dev/null 2>&1; then version_code=0; else version_code=$?; fi
if [ "$version_code" -eq 0 ] && grep -F -- 'markdownlint@9.9.9' "$npx_log" >/dev/null; then
  printf 'PASS: markdownlint-version-forwarded\n'
  ok=$((ok+1))
else
  printf 'FAIL: markdownlint-version-forwarded (VERSION_NOT_FORWARDED exit=%s)\n' "$version_code"
  fail=$((fail+1))
fi
reset_overrides

# 14. quotepath fail-open regression: git listings carry
#     `-c core.quotepath=false` and `-z`. Check the default path first, then
#     the staged path, with the log left accumulate across both.
diff_files="$ok_file"
: > "$git_log"; : > "$npx_log"
if invoke ok diff >/dev/null 2>&1; then quote_code=0; else quote_code=$?; fi
md_lint_mode='staged'
stub_untracked_files="$untracked_file"
diff_files="$staged_file"
if invoke ok diff >/dev/null 2>&1; then :; else quote_code=$?; fi
reset_overrides
if [ "$quote_code" -eq 0 ] \
  && grep -F -- 'core.quotepath=false' "$git_log" >/dev/null \
  && grep -E -- '(^|[[:space:]])-z([[:space:]]|$)' "$git_log" >/dev/null; then
  printf 'PASS: quotepath-nul-flags\n'
  ok=$((ok+1))
else
  printf 'FAIL: quotepath-nul-flags (FLAGS_MISSING exit=%s)\n' "$quote_code"
  fail=$((fail+1))
fi

printf 'result: ok=%d fail=%d\n' "$ok" "$fail"
[ "$fail" -eq 0 ]
