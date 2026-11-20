#!/usr/bin/env bash
# I87-A: workflow pin drift gate.
#
# Every `uses: <owner>/<repo>/...`@<ref> entry in .github/workflows/*.yml whose
# ref is not an immutable full-SHA pin (exactly 40 hex) is treated as mutable.
# Its expected commit SHA must be recorded in the nearest preceding comment
# block as `refs/tags/<ref> -> <40hex>` (see ci.yml); a missing or malformed
# record fails closed with PIN_RECORD_MISSING / PIN_RECORD_MALFORMED. This
# gate resolves the current SHA with the read-only GitHub REST API and compares
# it with that record, so a silently retagged ref is detected instead of being
# merely documented in a comment.
#
# Fixed fail-closed vocabulary (no dynamic messages on failure paths):
#   BAD_USAGE, NO_WORKFLOW_FILES, WORKFLOW_READ_FAILED,
#   PIN_RECORD_MISSING, PIN_RECORD_MALFORMED, SHA_MISMATCH, API_RESOLVE_FAILED
# SHA values are allowed in diagnostics; credentials and other secrets are
# never read into output, temporary files, or messages. If GITHUB_TOKEN or
# GH_TOKEN is present it is used only as an Authorization header and is never
# printed (it avoids unauthenticated rate limits on CI).
#
# Offline behaviour (explicit): set WORKFLOW_PINS_OFFLINE=1 or pass --offline.
# The API comparison is skipped, each entry is reported as UNVERIFIED, and the
# gate exits 0 so offline/local work is not blocked. CI is expected to have API
# reachability, so this is an opt-out for offline use, not a CI default.
# Network-unreachable curl failures (DNS/connect/timeout, rc 6/7/28/35) during
# an ONLINE run mark only that single entry UNVERIFIED; there is no sticky
# unreachable flag, so later entries retry the API independently and full
# comparison resumes as soon as the runner regains reachability. Only the
# explicit offline opt-out suppresses API comparison for every entry.
# Local record checks (PIN_RECORD_MISSING / PIN_RECORD_MALFORMED) are never
# downgraded by network state in an online run: they always fail closed.
# HTTP errors and malformed responses still fail closed as API_RESOLVE_FAILED.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root" || exit 1

offline=0
case "${WORKFLOW_PINS_OFFLINE:-0}" in
  1|true|TRUE|yes|YES|on|ON) offline=1 ;;
esac

if [ "$#" -gt 1 ]; then
  echo "FAIL BAD_USAGE expected at most one argument (--offline)" >&2
  exit 1
fi
if [ "$#" -eq 1 ]; then
  if [ "$1" = "--offline" ]; then
    offline=1
  else
    echo "FAIL BAD_USAGE unknown argument" >&2
    exit 1
  fi
fi

# curl/python3 are only required when an API comparison will actually run.
if [ "$offline" -eq 0 ]; then
  if ! command -v curl >/dev/null 2>&1; then
    echo "FAIL API_RESOLVE_FAILED curl is required for online verification" >&2
    exit 1
  fi
  if ! command -v python3 >/dev/null 2>&1; then
    echo "FAIL API_RESOLVE_FAILED python3 is required to parse API responses" >&2
    exit 1
  fi
fi

shopt -s nullglob
workflows=(.github/workflows/*.yml)
shopt -u nullglob
if [ "${#workflows[@]}" -eq 0 ]; then
  echo "FAIL NO_WORKFLOW_FILES" >&2
  exit 1
fi

auth_args=()
if [ -n "${GITHUB_TOKEN-}" ]; then
  auth_args=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
elif [ -n "${GH_TOKEN-}" ]; then
  auth_args=(-H "Authorization: Bearer ${GH_TOKEN}")
fi

api_get() {
  # stdout: response body; return: curl status. Errors are suppressed so only
  # the fixed vocabulary above reaches stderr from the callers.
  curl -fsSL --connect-timeout 10 --max-time 30 \
    -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2022-11-28' \
    "${auth_args[@]}" "$1" 2>/dev/null
}

parse_ref_object() {
  # stdin: GET /git/ref/tags/<ref> response; stdout: "<sha> <type>"
  python3 -c '
import json
import sys
try:
    data = json.load(sys.stdin)
    obj = data["object"]
    print(obj["sha"], obj.get("type", ""))
except Exception:
    sys.exit(1)
'
}

parse_tag_object() {
  # stdin: GET /git/tags/<tag-sha> response; stdout: <commit sha>
  python3 -c '
import json
import sys
try:
    data = json.load(sys.stdin)
    print(data["object"]["sha"])
except Exception:
    sys.exit(1)
'
}

resolve_current_sha() {
  # args: owner repo ref ; stdout: current commit sha ; return: 0 or curl rc.
  local owner="$1" repo_name="$2" ref="$3"
  local response rc parsed sha obj_type

  rc=0
  response="$(api_get "https://api.github.com/repos/${owner}/${repo_name}/git/ref/tags/${ref}")" || rc=$?
  if [ "$rc" -ne 0 ]; then
    return "$rc"
  fi

  parsed="$(printf '%s' "$response" | parse_ref_object)" || return 1
  read -r sha obj_type <<< "$parsed"

  # Annotated tags point to a tag object; follow it to the underlying commit.
  if [ "$obj_type" = "tag" ]; then
    rc=0
    response="$(api_get "https://api.github.com/repos/${owner}/${repo_name}/git/tags/${sha}")" || rc=$?
    if [ "$rc" -ne 0 ]; then
      return "$rc"
    fi
    sha="$(printf '%s' "$response" | parse_tag_object)" || return 1
  fi

  printf '%s\n' "$sha"
}

checked=0
failed=0
unverified=0

# Adopted policy (I89-A): per-entry API retry, no run-wide sticky flag.
# `offline` is the only explicit opt-out (WORKFLOW_PINS_OFFLINE / --offline).
# `api_unreachable` is scoped to a single pin: it is recomputed from `offline`
# at the start of every entry and is consulted ONLY to skip that entry's API
# comparison. It never gates local PIN_RECORD_MISSING / PIN_RECORD_MALFORMED
# checks, which stay fail closed in an online run regardless of curl results.
# A transient DNS/connect/timeout failure therefore marks just the current
# entry UNVERIFIED; the next entry resets the flag and retries the API, so
# verification resumes automatically once the runner is reachable again.

for wf in "${workflows[@]}"; do
  if [ ! -r "$wf" ]; then
    echo "FAIL WORKFLOW_READ_FAILED $wf" >&2
    failed=$((failed+1))
    continue
  fi
  mapfile -t lines < "$wf"

  for ((i = 0; i < ${#lines[@]}; i++)); do
    line="${lines[$i]}"
    if [[ ! "$line" =~ ^[[:space:]]*-?[[:space:]]*uses:[[:space:]]*([^[:space:]#]+)@([^[:space:]#]+) ]]; then
      continue
    fi
    spec="${BASH_REMATCH[1]}"
    ref="${BASH_REMATCH[2]}"

    # Adopted scope policy (I91-C): the ONLY refs excluded from checking are
    # immutable full-SHA pins (exactly 40 hex). Every other ref is mutable and
    # must carry a `refs/tags/<ref> -> <40hex>` record, otherwise the local
    # checks below fire PIN_RECORD_MISSING / PIN_RECORD_MALFORMED (fail closed).
    # This covers @main / @master / @v1-beta / @release/* / @v1 and any other
    # non-full-SHA ref; such refs are no longer silently skipped.
    # No "acceptable ref" allowlist is used: any non-full-SHA ref is in scope.
    # (The outer `uses:` expression regex already restricts refs to tokens
    # without whitespace or '#', so expression forms with spaces cannot match.)
    if [[ "$ref" =~ ^[0-9a-fA-F]{40}$ ]]; then
      continue
    fi

    location="${wf}:$((i + 1))"
    owner="${spec%%/*}"
    rest="${spec#*/}"
    repo_name="${rest%%/*}"
    if [ -z "$owner" ] || [ "$owner" = "$spec" ] || [ -z "$repo_name" ]; then
      echo "FAIL PIN_RECORD_MALFORMED $location owner/repo not resolvable" >&2
      failed=$((failed+1))
      continue
    fi

    # Walk backwards through the nearest comment block (blank lines do not
    # end it) and take the closest `refs/tags/<ref> -> <40hex>` record.
    expected=""
    ref_pattern="refs/tags/${ref}"
    j=$((i - 1))
    while [ "$j" -ge 0 ]; do
      prev="${lines[$j]}"
      if [[ "$prev" =~ ^[[:space:]]*$ ]]; then
        j=$((j - 1))
        continue
      fi
      if [[ ! "$prev" =~ ^[[:space:]]*# ]]; then
        break
      fi
      # Literal substring search avoids regex-escaping the `->` separator.
      # The char after the ref must not extend it (v1 must not match v10).
      if [[ "$prev" == *"$ref_pattern"* ]]; then
        tail="${prev#*"$ref_pattern"}"
        if [[ "$tail" =~ ^[^0-9A-Za-z.] ]] && [[ "$tail" =~ ([0-9a-fA-F]{40}) ]]; then
          expected="${BASH_REMATCH[1]}"
          break
        fi
      fi
      j=$((j - 1))
    done

    checked=$((checked+1))
    # Per-entry API availability: recomputed from `offline` for every pin so a
    # previous entry's network failure can never leak into this one.
    api_unreachable="$offline"

    if [ -z "$expected" ]; then
      # Local record check: gated only by the explicit offline opt-out, never
      # by network state, so an online run always fails closed here.
      if [ "$offline" -eq 1 ]; then
        echo "WARN UNVERIFIED $location $spec PIN_RECORD_MISSING (API skipped)" >&2
        unverified=$((unverified+1))
      else
        echo "FAIL PIN_RECORD_MISSING $location $spec" >&2
        failed=$((failed+1))
      fi
      continue
    fi

    if [ "$api_unreachable" -eq 1 ]; then
      echo "WARN UNVERIFIED $location $spec expected=$expected (API skipped)" >&2
      unverified=$((unverified+1))
      continue
    fi

    rc=0
    current="$(resolve_current_sha "$owner" "$repo_name" "$ref")" || rc=$?
    if [ "$rc" -ne 0 ]; then
      case "$rc" in
        6|7|28|35)
          echo "WARN UNVERIFIED $location $spec network unreachable (rc=$rc); API skipped" >&2
          unverified=$((unverified+1))
          # Per-entry only: never set a run-wide sticky flag. The next entry
          # recomputes api_unreachable from `offline` and retries the API, so
          # comparison of later pins resumes once reachability returns.
          ;;
        *)
          echo "FAIL API_RESOLVE_FAILED $location $spec (rc=$rc)" >&2
          failed=$((failed+1))
          ;;
      esac
      continue
    fi
    if [[ ! "$current" =~ ^[0-9a-fA-F]{40}$ ]]; then
      echo "FAIL API_RESOLVE_FAILED $location $spec unexpected SHA format" >&2
      failed=$((failed+1))
      continue
    fi

    expected_lc="${expected,,}"
    current_lc="${current,,}"
    if [ "$expected_lc" != "$current_lc" ]; then
      echo "FAIL SHA_MISMATCH $location $spec expected=$expected_lc current=$current_lc" >&2
      failed=$((failed+1))
    else
      echo "OK $location $spec"
    fi
  done
done

echo "== workflow pins: checked=$checked failed=$failed unverified=$unverified =="
if [ "$failed" -gt 0 ]; then
  exit 1
fi
exit 0
