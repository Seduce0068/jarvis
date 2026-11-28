#!/usr/bin/env bash
# Regression tests for ci/scripts/lint-md.sh (R5 contract).
#
# Contract under test:
#   bash ci/scripts/lint-md.sh [--all] [--staged]
#   - default: tracked *.md from a git diff against ${MD_LINT_BASE:-HEAD}
#     plus untracked *.md (deduplicated); zero files -> exit 0
#   - --all: every tracked *.md
#   - staged (--staged / MD_LINT_MODE=staged): staged blobs are expanded with
#     `git show :<path>` into a scratch dir and linted there; findings are
#     mapped back to the original path; untracked *.md stay real files
#   - git listings use `git -c core.quotepath=false ... -z` (NUL-safe names)
#   - MARKDOWNLINT_VERSION pins BOTH probe and real lint to
#     markdownlint-cli@<version>; the probe version must match the pin
#   - invalid MD_LINT_BASE / mode / args and git/npx failures -> fail-closed
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
newline_file=$'docs/line\nbreak.md'
missing_file='docs/missing.md'
diff_files=''
all_files=''

# Per-case overrides. They are exported only when explicitly set so an empty
# value can never be mistaken for an intentionally configured base/mode.
md_lint_base=''
md_lint_mode=''
markdownlint_version=''
stub_valid_base='HEAD'
stub_untracked_files=''
stub_nul_diff_file=''
stub_probe_version=''
stub_show_mode=''
stub_require_marker=''
path_override=''

bash_bin="$(command -v bash)"

reset_overrides() {
  md_lint_base=''
  md_lint_mode=''
  markdownlint_version=''
  stub_valid_base='HEAD'
  stub_untracked_files=''
  stub_nul_diff_file=''
  stub_probe_version=''
  stub_show_mode=''
  stub_require_marker=''
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
printf '# staged worktree (clean)\n\nclean line\n' > "$mini/$staged_file"
printf '# newline\n\nclean line\n' > "$mini/$newline_file"
printf 'fn main() {}\n' > "$mini/src/main.rs"

git_log="$tmp/git-calls.log"
npx_log="$tmp/npx-calls.log"
: > "$git_log"
: > "$npx_log"

# --- git stub: canned tracked-file lists, dispatched by real subcommand -----
# Emits NUL-delimited names when the gate passes -z, newline otherwise, so the
# same stub serves both the R2 newline contract and the R5 quotepath/-z path.
# `show` reproduces an index blob (used by staged mode); STUB_NUL_DIFF_FILE
# injects one verbatim NUL-delimited name that may contain a newline.
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

if [ -n "${STUB_NUL_DIFF_FILE:-}" ]; then
  for arg in "$@"; do
    if [ "$arg" = diff ]; then
      printf '%s\0' "$STUB_NUL_DIFF_FILE"
      exit 0
    fi
  done
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
    show) cmd='show' ;;
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
  show)
    if [ "${STUB_SHOW_MODE:-}" = fail ]; then
      printf 'git stub: show failure\n' >&2
      exit 1
    fi
    printf '# STAGED_BLOB_MARKER\n\nstaged blob line\n'
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

# --- npx stub: canned success / violation / multi / unavailable / failure ---
# --version is a capability probe: it reports the version parsed from the
# pinned spec so the gate's version check succeeds unless a case overrides it.
# Real-lint findings use the Markdown argument the gate actually passed, which
# is the expanded scratch path in staged mode.
cat > "$bin/npx" <<'STUB'
#!/usr/bin/env bash
set -u
if [ -n "${STUB_NPX_LOG:-}" ]; then
  {
    printf 'CALL:'
    for arg in "$@"; do
      printf ' %q' "$arg"
    done
    printf '\n'
  } >> "$STUB_NPX_LOG"
fi

case "$*" in
  *--version*)
    case "${STUB_NPX_MODE:-ok}" in
      failprobe)
        printf 'npx stub probe failure\n' >&2
        exit 1
        ;;
    esac
    spec=''
    for arg in "$@"; do
      case "$arg" in
        markdownlint-cli@*) spec="${arg#markdownlint-cli@}" ;;
        markdownlint@*) spec="${arg#markdownlint@}" ;;
      esac
    done
    if [ -n "${STUB_PROBE_VERSION:-}" ]; then
      printf '%s\n' "$STUB_PROBE_VERSION"
    else
      printf '%s\n' "$spec"
    fi
    exit 0
    ;;
esac

mdarg=''
for arg in "$@"; do
  case "$arg" in
    *.md) mdarg="$arg"; break ;;
  esac
done
[ -n "$mdarg" ] || mdarg="${STUB_BAD_FILE:-docs/bad.md}"

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
  violation|multi)
    # Optional content gate: only report a finding if a passed *.md file
    # actually contains the marker (proves the staged blob was expanded).
    if [ -n "${STUB_REQUIRE_MARKER:-}" ]; then
      found=0
      for arg in "$@"; do
        case "$arg" in
          *.md)
            if grep -qF -- "$STUB_REQUIRE_MARKER" "$arg" 2>/dev/null; then
              found=1
            fi
            ;;
        esac
      done
      [ "$found" -eq 1 ] || exit 0
    fi
    # Real markdownlint-cli writes findings to stderr with a severity word
    # ("error MD013/..."). Mirror that so the gate's stream/severity handling
    # is exercised, not just an idealised stdout stub.
    if [ "${STUB_NPX_MODE:-}" = multi ]; then
      # Multiple findings plus an exact duplicate line: only distinct
      # "path: MDxxxx" pairs may survive, and the sentinel must not leak.
      {
        printf '%s:3:1 error %s/first-line-heading First line [Context: "%s"]\n' \
          "$mdarg" 'MD041' "${STUB_SENTINEL:-PROBE_DO_NOT_LEAK}"
        printf '%s:5:1 error %s/multiple-consecutive-blank-lines Blank lines\n' \
          "$mdarg" 'MD012'
        printf '%s:3:1 error %s/first-line-heading duplicate [Context: "%s"]\n' \
          "$mdarg" 'MD041' "${STUB_SENTINEL:-PROBE_DO_NOT_LEAK}"
        printf '%s:7:1 error %s/line-length Line length\n' \
          "$mdarg" 'MD013'
      } >&2
      exit 1
    fi
    printf '%s:3:1 error %s/first-line-heading First line should be a top-level heading [Context: "%s"]\n' \
      "$mdarg" 'MD041' "${STUB_SENTINEL:-PROBE_DO_NOT_LEAK}" >&2
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
    export STUB_NUL_DIFF_FILE="$stub_nul_diff_file"
    export STUB_PROBE_VERSION="$stub_probe_version"
    export STUB_SHOW_MODE="$stub_show_mode"
    export STUB_REQUIRE_MARKER="$stub_require_marker"
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
if [ "$version_code" -eq 0 ] && grep -F -- 'markdownlint-cli@9.9.9' "$npx_log" >/dev/null; then
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

# --- Adopted review findings (R5 round 2) ----------------------------------

# 15. CLI --staged flag selects the staged scope (--cached + --others).
reset_overrides
stub_untracked_files="$untracked_file"
diff_files="$staged_file"
: > "$git_log"; : > "$npx_log"
if invoke ok diff --staged >/dev/null 2>&1; then cli_staged_code=0; else cli_staged_code=$?; fi
if [ "$cli_staged_code" -eq 0 ] \
  && grep -q -- '--cached' "$git_log" \
  && grep -q -- '--others' "$git_log"; then
  printf 'PASS: cli-staged-flag\n'
  ok=$((ok+1))
else
  printf 'FAIL: cli-staged-flag (CLI_STAGED exit=%s)\n' "$cli_staged_code"
  fail=$((fail+1))
fi
reset_overrides

# 16. --all combined with --staged is a usage error (fail-closed).
diff_files="$bad_file"
check_case 'all-staged-mutually-exclusive' 1 ok diff --all --staged

# 17. Unknown CLI argument fails closed.
check_case 'invalid-arg-fails-closed' 1 ok diff --bogus

# 18. Invalid MD_LINT_MODE fails closed.
md_lint_mode='bogus'
check_case 'invalid-mode-fails-closed' 1 ok diff
reset_overrides

# 19. failprobe: --version alone exits 1, so the gate must die at the probe
#     before issuing the real lint call (npx log has exactly one CALL line).
diff_files="$ok_file"
check_case 'probe-failure-fails-closed' 1 failprobe diff
if [ "$(grep -c '^CALL:' "$npx_log")" -eq 1 ]; then
  printf 'PASS: probe-failure-no-lint-call\n'
  ok=$((ok+1))
else
  printf 'FAIL: probe-failure-no-lint-call (PROBE_SCOPE)\n'
  fail=$((fail+1))
fi

# 20. multi: duplicate "path: MDxxxx" lines are collapsed and no sentinel
#     reaches combined output.
multi_guard() {
  local out code='' count
  diff_files="$bad_file"
  : > "$git_log"; : > "$npx_log"
  if out="$(invoke multi diff 2>&1)"; then code=0; else code=$?; fi
  [ "$code" -eq 1 ] || return 1
  case "$out" in
    *"$sentinel"*) return 1 ;;
  esac
  count="$(printf '%s\n' "$out" | grep -c .)" || return 1
  [ "$count" -eq 3 ] || return 1
  for rule in MD041 MD012 MD013; do
    [ "$(printf '%s\n' "$out" | grep -Fc "$bad_file: $rule")" -eq 1 ] || return 1
  done
  return 0
}
if multi_guard; then
  printf 'PASS: multi-findings-deduped-no-leak\n'
  ok=$((ok+1))
else
  printf 'FAIL: multi-findings-deduped-no-leak (MULTI_CONTRACT)\n'
  fail=$((fail+1))
fi

# 21. Both the probe and the real lint carry markdownlint-cli@<version>.
markdownlint_version='9.9.9'
diff_files="$ok_file"
: > "$git_log"; : > "$npx_log"
if invoke ok diff >/dev/null 2>&1; then both_code=0; else both_code=$?; fi
if [ "$both_code" -eq 0 ] \
  && [ "$(grep -Fc 'markdownlint-cli@9.9.9' "$npx_log")" -ge 2 ]; then
  printf 'PASS: cli-spec-on-probe-and-lint\n'
  ok=$((ok+1))
else
  printf 'FAIL: cli-spec-on-probe-and-lint (SPEC_NOT_SHARED exit=%s)\n' "$both_code"
  fail=$((fail+1))
fi
reset_overrides

# 22. Default pin is markdownlint-cli@0.50.1, never the broken markdownlint@...
diff_files="$ok_file"
: > "$git_log"; : > "$npx_log"
if invoke ok diff >/dev/null 2>&1; then def_code=0; else def_code=$?; fi
if [ "$def_code" -eq 0 ] \
  && [ "$(grep -Fc 'markdownlint-cli@0.50.1' "$npx_log")" -ge 2 ] \
  && ! grep -Fq 'markdownlint@0.50.1' "$npx_log"; then
  printf 'PASS: default-cli-spec\n'
  ok=$((ok+1))
else
  printf 'FAIL: default-cli-spec (DEFAULT_SPEC exit=%s)\n' "$def_code"
  fail=$((fail+1))
fi

# 23. A probe version that differs from the pin fails closed.
stub_probe_version='1.2.3'
diff_files="$ok_file"
check_case 'probe-version-mismatch-fails-closed' 1 ok diff

# 24. An unparseable probe version fails closed.
stub_probe_version='not-a-version'
diff_files="$ok_file"
check_case 'probe-version-unparseable-fails-closed' 1 ok diff
reset_overrides

# 25. A newline inside a filename stays one argument (NUL-safe listing).
stub_nul_diff_file="$newline_file"
diff_files=''
: > "$git_log"; : > "$npx_log"
if invoke ok diff >/dev/null 2>&1; then nl_code=0; else nl_code=$?; fi
if [ "$nl_code" -eq 0 ] && grep -qF 'docs/line\nbreak.md' "$npx_log"; then
  printf 'PASS: newline-path-single-arg\n'
  ok=$((ok+1))
else
  printf 'FAIL: newline-path-single-arg (NEWLINE_ARG exit=%s)\n' "$nl_code"
  fail=$((fail+1))
fi
reset_overrides

# 26. A listed but absent file is skipped; the existing file is still linted.
diff_files="$missing_file
$ok_file"
: > "$git_log"; : > "$npx_log"
if invoke ok diff >/dev/null 2>&1; then miss_code=0; else miss_code=$?; fi
if [ "$miss_code" -eq 0 ] \
  && ! grep -qF "$missing_file" "$npx_log" \
  && grep -qF "$ok_file" "$npx_log"; then
  printf 'PASS: missing-file-skipped\n'
  ok=$((ok+1))
else
  printf 'FAIL: missing-file-skipped (MISSING_ARG exit=%s)\n' "$miss_code"
  fail=$((fail+1))
fi
reset_overrides

# 27. staged mode expands `git show :<path>` into scratch and lints THAT blob
#     (the worktree copy is clean), then maps findings back to the source path.
md_lint_mode='staged'
stub_require_marker='STAGED_BLOB_MARKER'
diff_files="$staged_file"
staged_blob_guard() {
  local out code=''
  : > "$git_log"; : > "$npx_log"
  if out="$(invoke violation diff 2>&1)"; then code=0; else code=$?; fi
  [ "$code" -eq 1 ] || return 1
  grep -q -- 'show :docs/staged.md' "$git_log" || return 1
  grep -qE -- 'CALL:.*staged/docs/staged\.md' "$npx_log" || return 1
  case "$out" in
    *"$sentinel"*) return 1 ;;
  esac
  case "$out" in
    *'docs/staged.md: MD041'*) ;;
    *) return 1 ;;
  esac
  case "$out" in
    *'/staged/docs/staged.md: MD041'*) return 1 ;;
  esac
  return 0
}
if staged_blob_guard; then
  printf 'PASS: staged-blob-expanded-and-mapped\n'
  ok=$((ok+1))
else
  printf 'FAIL: staged-blob-expanded-and-mapped (STAGED_BLOB)\n'
  fail=$((fail+1))
fi
reset_overrides

# 28. A failed `git show :<path>` fails closed without calling npx.
md_lint_mode='staged'
stub_show_mode='fail'
diff_files="$staged_file"
check_case 'staged-expand-failure-fails-closed' 1 ok diff
if [ ! -s "$npx_log" ]; then
  printf 'PASS: staged-expand-failure-no-lint-call\n'
  ok=$((ok+1))
else
  printf 'FAIL: staged-expand-failure-no-lint-call (NPX_CALL)\n'
  fail=$((fail+1))
fi
reset_overrides

printf 'result: ok=%d fail=%d\n' "$ok" "$fail"
[ "$fail" -eq 0 ]
