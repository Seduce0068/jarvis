#!/usr/bin/env bash
# Regression tests for the temporary-workdir contract of
# infra/scripts/setup-selfhost.sh (R1/R2 follow-up).
#
# Static checks only. The target script requires root and performs real
# installs, so it is never executed here and no stubs are involved. A stable
# snapshot is copied into a private mktemp -d (removed by the EXIT trap) so a
# concurrent edit cannot change the file mid-run. Assertions:
#   1. the four legacy fixed /tmp artifact strings are absent
#   2. `mktemp -d /tmp/jarvis-setup.XXXXXX` and the WORK_DIR EXIT trap exist
#   3. `rm -rf` appears only on that trap line (no fixed-path cleanup)
#   4. `install -o root -g root` places the otelcol binary
#
# On failure only the case id and a fixed error code are printed. No matched
# source line, file content or artifact value is ever echoed.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target="$repo/infra/scripts/setup-selfhost.sh"

if [ ! -f "$target" ]; then
  printf 'FAIL: target-present (TARGET_MISSING)\n'
  exit 1
fi

tmp="$(mktemp -d)" || { printf 'FAIL: tmp-available (TMP_UNAVAILABLE)\n'; exit 1; }
trap 'rm -rf "$tmp"' EXIT

snapshot="$tmp/setup-selfhost.sh"
if ! cp "$target" "$snapshot"; then
  printf 'FAIL: target-copy (COPY_FAILED)\n'
  exit 1
fi

ok=0
fail=0

fail_case() { # case-id error-code
  printf 'FAIL: %s (%s)\n' "$1" "$2"
  fail=$((fail+1))
}

# Kept in a variable so the exact trap text is asserted without ever printing
# a matching source line.
trap_expected="trap 'rm -rf -- \"\$WORK_DIR\"' EXIT"

# 1. Legacy fixed /tmp artifact strings must be gone (literal -F match).
for path_case in \
  'fixed-tmp-qdrant|FIXED_TMP_QDRANT|/tmp/qdrant.tar.gz' \
  'fixed-tmp-otelcol-deb|FIXED_TMP_OTELCOL_DEB|/tmp/otelcol.deb' \
  'fixed-tmp-otelcol-extract|FIXED_TMP_OTELCOL_EXTRACT|/tmp/otelcol-extract' \
  'fixed-tmp-bws-zip|FIXED_TMP_BWS_ZIP|/tmp/bws.zip'
do
  IFS='|' read -r case_id code needle <<< "$path_case"
  if grep -Fq -- "$needle" "$snapshot"; then
    fail_case "$case_id" "$code"
  else
    ok=$((ok+1))
  fi
done

# 2. Private workdir creation and its EXIT cleanup trap.
if grep -Fq -- 'mktemp -d /tmp/jarvis-setup.XXXXXX' "$snapshot"; then
  ok=$((ok+1))
else
  fail_case 'workdir-mktemp' 'MKTEMP_MISSING'
fi
if grep -Fq -- "$trap_expected" "$snapshot"; then
  ok=$((ok+1))
else
  fail_case 'workdir-exit-trap' 'TRAP_MISSING'
fi

# Logical command lines: skip full-line comments and join backslash
# continuations, so a multi-line install or an explanatory comment cannot skew
# results. Nothing from these lines is printed.
awk '
  /^[[:space:]]*#/ { next }
  {
    line = $0
    sub(/[[:space:]]+$/, "", line)
    if (continued) { buf = buf " " line } else { buf = line }
    if (buf ~ /\\$/) {
      sub(/\\$/, "", buf)
      continued = 1
    } else {
      print buf
      buf = ""
      continued = 0
    }
  }
  END { if (continued) print buf }
' "$snapshot" > "$tmp/logical.txt"

# 3. rm -rf is confined to the single WORK_DIR EXIT trap.
rm_outside="$(awk -v trap_mark="$trap_expected" '
  index($0, "rm -rf") && index($0, trap_mark) == 0 { n++ }
  END { print n+0 }
' "$tmp/logical.txt")"
if [ "$rm_outside" -eq 0 ]; then
  ok=$((ok+1))
else
  fail_case 'rm-rf-trap-only' 'RM_RF_OUTSIDE_TRAP'
fi

# 4. The otelcol binary is installed with explicit root ownership.
otel_install="$(awk '
  index($0, "install -o root -g root") && index($0, "otelcol") { n++ }
  END { print n+0 }
' "$tmp/logical.txt")"
if [ "$otel_install" -ge 1 ]; then
  ok=$((ok+1))
else
  fail_case 'otelcol-install-root-owner' 'INSTALL_OTELCOL_MISSING'
fi

printf 'result: ok=%d fail=%d\n' "$ok" "$fail"
[ "$fail" -eq 0 ]
