#!/usr/bin/env bash
# I89-B: automated regression for ci/scripts/check-workflow-pins.sh (I87-A).
#
# The pin gate reads .github/workflows/*.yml relative to its own path and talks
# to the GitHub REST API through curl. To exercise every branch without network
# access or touching the real workflow files, this test copies the gate into a
# throwaway mini repository, supplies mini workflows, and shadows curl with a
# canned-response stub on PATH. python3 is used only by the gate to parse the
# canned JSON bodies.
#
# Assertions are the fixed fail-closed vocabulary plus the exit code
# (SHA_MISMATCH, PIN_RECORD_MISSING, API_RESOLVE_FAILED, ...). All SHAs and
# response bodies are synthetic; no credentials, no real API calls, and no
# writes outside the temporary directory.
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
gate="$repo/ci/scripts/check-workflow-pins.sh"

if [ ! -f "$gate" ]; then
  printf 'FAIL: GATE_MISSING\n'
  exit 1
fi

tmp="$(mktemp -d)" || { printf 'FAIL: TMP_UNAVAILABLE\n'; exit 1; }
trap 'rm -rf "$tmp"' EXIT

ok=0
bad=0
pins_offline=''

# Synthetic, deterministic 40-hex values (not real commits or tag objects).
commit_sha='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
tag_obj_sha='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
wrong_sha='cccccccccccccccccccccccccccccccccccccccc'

responses="$tmp/responses"
stub_bin="$tmp/bin"
curl_log="$tmp/curl-calls.log"
mkdir -p "$responses" "$stub_bin"
: > "$curl_log"

# --- canned GitHub API response bodies -------------------------------------
printf '{"object":{"sha":"%s","type":"commit"}}\n' "$commit_sha" > "$responses/light_ref.json"
printf '{"object":{"sha":"%s","type":"tag"}}\n' "$tag_obj_sha" > "$responses/annot_ref.json"
printf '{"object":{"sha":"%s"}}\n' "$commit_sha" > "$responses/annot_tag.json"
printf '{"object":{"sha":"%s","type":"commit"}}\n' "$wrong_sha" > "$responses/mismatch_ref.json"
printf '%s\n' 'not json' > "$responses/bad_json.txt"

# --- curl stub: canned body by endpoint plus a caller-selected return code --
cat > "$stub_bin/curl" <<'STUB'
#!/usr/bin/env bash
# Canned curl replacement for the pin-gate regression (never opens a socket).
set -u
url=''
for arg in "$@"; do
  url="$arg"
done
if [ -n "${STUB_LOG:-}" ]; then
  printf '%s\n' "$url" >> "$STUB_LOG"
fi
if [ "${STUB_RC:-0}" != '0' ]; then
  exit "$STUB_RC"
fi
case "$url" in
  */git/tags/*) body="${STUB_TAG_BODY:-}" ;;
  */git/ref/tags/*) body="${STUB_REF_BODY:-}" ;;
  *)
    printf 'STUB_UNEXPECTED_URL\n' >&2
    exit 22
    ;;
esac
if [ -z "$body" ] || [ ! -r "$body" ]; then
  printf 'STUB_NO_BODY\n' >&2
  exit 22
fi
cat "$body"
exit 0
STUB
chmod +x "$stub_bin/curl"

# --- mini repositories ------------------------------------------------------
make_mini() { # dir mode  (mode: record | missing | malformed | empty | main-missing)
  local mini="$1" mode="$2"
  mkdir -p "$mini/ci/scripts" "$mini/.github/workflows"
  cp "$gate" "$mini/ci/scripts/check-workflow-pins.sh"
  [ "$mode" = 'empty' ] && return 0
  {
    printf '%s\n' 'name: mini' 'on: workflow_dispatch' 'jobs:' '  gates:'
    case "$mode" in
      record)
        printf '    # refs/tags/v1 -> %s\n' "$commit_sha"
        printf '%s\n' '    uses: Seduce0068/workflows/.github/workflows/rust-gates.yml@v1'
        ;;
      missing)
        printf '%s\n' '    uses: Seduce0068/workflows/.github/workflows/rust-gates.yml@v1'
        ;;
      malformed)
        printf '%s\n' '    uses: badspec@v1'
        ;;
      main-missing)
        # I91-C: a mutable @main ref with no record must be checked, not
        # silently skipped by the old ^vN$-only filter.
        printf '%s\n' '    uses: Seduce0068/workflows/.github/workflows/rust-gates.yml@main'
        ;;
    esac
  } > "$mini/.github/workflows/ci.yml"
}

mini_record="$tmp/mini-record"
mini_missing="$tmp/mini-missing"
mini_malformed="$tmp/mini-malformed"
mini_empty="$tmp/mini-empty"
mini_main_missing="$tmp/mini-main-missing"
make_mini "$mini_record" record
make_mini "$mini_missing" missing
make_mini "$mini_malformed" malformed
make_mini "$mini_empty" empty
make_mini "$mini_main_missing" main-missing

# --- harness ----------------------------------------------------------------
invoke() { # mini ref_body tag_body rc [args...]
  local mini="$1" ref_body="$2" tag_body="$3" rc="$4"; shift 4
  env -u GITHUB_TOKEN -u GH_TOKEN \
    PATH="$stub_bin:$PATH" \
    WORKFLOW_PINS_OFFLINE="$pins_offline" \
    STUB_LOG="$curl_log" \
    STUB_REF_BODY="$ref_body" \
    STUB_TAG_BODY="$tag_body" \
    STUB_RC="$rc" \
    bash "$mini/ci/scripts/check-workflow-pins.sh" "$@"
}

check_case() { # id mini ref tag rc expected_exit pattern [absent] [args...]
  local id="$1" mini="$2" ref_body="$3" tag_body="$4" rc="$5"
  local expected_exit="$6" pattern="$7" absent="${8-}"
  if [ "$#" -ge 8 ]; then
    shift 8
  else
    shift 7
  fi
  local out code='' problem=''
  : > "$curl_log"
  if out="$(invoke "$mini" "$ref_body" "$tag_body" "$rc" "$@" 2>&1)"; then
    code=0
  else
    code=$?
  fi
  if [ "$code" -ne "$expected_exit" ]; then
    problem='EXIT_CODE'
  elif ! grep -q -- "$pattern" <<< "$out"; then
    problem='MISSING_TOKEN'
  elif [ -n "$absent" ] && grep -q -- "$absent" <<< "$out"; then
    problem='FORBIDDEN_TOKEN'
  fi
  if [ -n "$problem" ]; then
    printf 'FAIL: %s (%s exit=%s)\n' "$id" "$problem" "$code"
    bad=$((bad+1))
  else
    printf 'PASS: %s\n' "$id"
    ok=$((ok+1))
  fi
}

# --- cases ------------------------------------------------------------------
# Lightweight tag: ref object is the commit; recorded SHA matches.
check_case 'lightweight-match' "$mini_record" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 0 0 '^OK ' 'FAIL'

# Annotated tag: the ref object is a tag object and must be followed to the
# commit. Without the follow-up call current would be the tag SHA and mismatch.
check_case 'annotated-follow' "$mini_record" \
  "$responses/annot_ref.json" "$responses/annot_tag.json" 0 0 '^OK ' 'FAIL'
annot_calls="$(awk 'END { print NR+0 }' "$curl_log")"
if [ "$annot_calls" -eq 2 ]; then
  printf 'PASS: annotated-follow-two-api-calls\n'
  ok=$((ok+1))
else
  printf 'FAIL: annotated-follow-two-api-calls (calls=%s)\n' "$annot_calls"
  bad=$((bad+1))
fi

# Recorded SHA differs from the resolved commit SHA.
check_case 'sha-mismatch' "$mini_record" \
  "$responses/mismatch_ref.json" "$responses/annot_tag.json" 0 1 'SHA_MISMATCH'

# No preceding pin record must fail closed in online mode.
check_case 'pin-record-missing' "$mini_missing" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 0 1 'PIN_RECORD_MISSING'

# I91-C: a mutable @main ref is in scope (only full-SHA pins are skipped).
# With no record it must fire PIN_RECORD_MISSING and exit 1; the old ^vN$
# filter silently skipped it and exited 0, so this is the scope regression.
check_case 'mutable-main-ref-missing' "$mini_main_missing" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 0 1 'PIN_RECORD_MISSING'

# Network-unreachable curl codes degrade to UNVERIFIED, not API_RESOLVE_FAILED.
check_case 'curl-rc6-unverified' "$mini_record" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 6 0 \
  'network unreachable (rc=6)' 'API_RESOLVE_FAILED'
check_case 'curl-rc28-unverified' "$mini_record" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 28 0 \
  'network unreachable (rc=28)' 'API_RESOLVE_FAILED'

# --offline: skip the API entirely, report UNVERIFIED, still exit 0.
check_case 'offline-flag' "$mini_record" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 0 0 \
  'UNVERIFIED.*API skipped' 'FAIL' --offline
if [ ! -s "$curl_log" ]; then
  printf 'PASS: offline-flag-no-curl\n'
  ok=$((ok+1))
else
  printf 'FAIL: offline-flag-no-curl\n'
  bad=$((bad+1))
fi

# Environment-variable form of offline mode.
pins_offline='1'
check_case 'offline-env' "$mini_record" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 0 0 \
  'UNVERIFIED.*API skipped' 'FAIL'
pins_offline=''
if [ ! -s "$curl_log" ]; then
  printf 'PASS: offline-env-no-curl\n'
  ok=$((ok+1))
else
  printf 'FAIL: offline-env-no-curl\n'
  bad=$((bad+1))
fi

# Reachable-but-failed API and malformed JSON must fail closed.
check_case 'api-http-error' "$mini_record" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 22 1 'API_RESOLVE_FAILED'
check_case 'api-bad-json' "$mini_record" \
  "$responses/bad_json.txt" "$responses/annot_tag.json" 0 1 'API_RESOLVE_FAILED'

# Other fail-closed vocabulary branches.
check_case 'no-workflow-files' "$mini_empty" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 0 1 'NO_WORKFLOW_FILES'
check_case 'malformed-uses' "$mini_malformed" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 0 1 'PIN_RECORD_MALFORMED'
check_case 'bad-usage' "$mini_record" \
  "$responses/light_ref.json" "$responses/annot_tag.json" 0 1 'BAD_USAGE' '' --offline --offline

printf 'result: ok=%d fail=%d\n' "$ok" "$bad"
[ "$bad" -eq 0 ]
