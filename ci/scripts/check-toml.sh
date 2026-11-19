#!/usr/bin/env bash
# Validate every tracked *.toml file with a stdlib-grade TOML parser (I29-B).
# Parser selection is fail-closed: tomllib (Python 3.11+) is preferred, the
# tomli backport is accepted as a fallback, and if neither is importable the
# gate fails instead of silently passing. A parse error reports the file name
# only; TOML values and parser messages are never emitted.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo" || exit 1

# Resolve the parser once, probing each candidate with the same
# `python3 -c 'import <module>'` form. No parser (or no python3) => fail closed.
if ! command -v python3 >/dev/null 2>&1; then
  echo "FAIL: python3 is not installed (required by check-toml.sh)" >&2
  exit 1
fi
if python3 -c 'import tomllib' 2>/dev/null; then
  toml_module="tomllib"
elif python3 -c 'import tomli' 2>/dev/null; then
  toml_module="tomli"
else
  echo "FAIL: no TOML parser available (need Python 3.11+ tomllib or the tomli backport)" >&2
  exit 1
fi

# Resolve the target list via a private scratch file so a failing `git
# ls-files` fails the gate (fail-closed) instead of silently passing. NUL
# delimiters keep paths with spaces/newlines intact. The EXIT trap cleans up
# on success, failure, or early exit.
tmpdir="$(mktemp -d)" || { echo "FAIL: mktemp -d failed" >&2; exit 1; }
trap 'rm -rf "$tmpdir"' EXIT
list_file="$tmpdir/toml-files"
if ! git ls-files -z '*.toml' > "$list_file"; then
  echo "FAIL: git ls-files -z '*.toml' failed" >&2
  exit 1
fi
mapfile -d '' -t toml_files < "$list_file"
if [ "${#toml_files[@]}" -eq 0 ]; then
  echo "FAIL: no tracked *.toml files to check" >&2
  exit 1
fi

python3 - "$toml_module" "${toml_files[@]}" <<'PY'
import importlib
import sys

toml = importlib.import_module(sys.argv[1])
failed = 0
ok = 0

for path in sys.argv[2:]:
    try:
        with open(path, "rb") as fh:
            toml.load(fh)
    except Exception:
        # Emit the file name only. Parser messages and file values are never
        # printed, so a syntax error cannot leak configuration contents.
        print(path, file=sys.stderr)
        failed += 1
    else:
        ok += 1

print(f"check-toml: ok={ok} fail={failed}")
sys.exit(1 if failed else 0)
PY
