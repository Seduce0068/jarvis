#!/usr/bin/env python3
"""Scan tracked files for accidentally committed secrets.

Safety properties:
- Only files tracked by Git are inspected.
- Secret values are NEVER printed: the report shows file, line, key name and
  a classification only.
- Known-safe matches (placeholders, documented examples, and the repo's own
  PII-detection regex definitions) are classified as such, not as findings.
  Documentation under docs/ is NOT a blanket safe path (I7-B): placeholder
  examples there are recognised via the placeholder patterns below, but a
  value-like literal in documentation fails the gate just like one in source.

Usage: python3 ci/scripts/secret_scan.py (runnable from any CWD)

The repo root is derived from this script's own path and `git ls-files` is
anchored there with `git -C <root>`. This anchoring (repo_root() + `-C root`)
is the PRIMARY defense against CWD-dependent partial scans (I81-B): a
subdirectory CWD or a franken CWD in another git repo cannot shrink the set.

The index-count check below is only a regression guard on the tracked_files()
implementation. It compares against a second `git -C <root> ls-files --cached`
call, which returns the same cached index today, so it is NOT an independent
detector of CWD-induced shrinking. A mismatch means tracked_files() was changed
to a different listing (e.g. CWD-relative or filtered); it fails closed with
fixed vocabulary that never contains a path or a credential value.

Exit 0 only when at least one tracked file exists, no value-like credentials
are found, every tracked file was read successfully and the set count equals
the cached-index count; 1 otherwise (empty set / count mismatch / unreadable
files all fail closed, I79-C / I81-B).
"""

from __future__ import annotations

import os
import re
import subprocess
import sys

ASSIGNMENT = re.compile(
    r"(?i)\b(BWS_ACCESS_TOKEN|OP_SERVICE_ACCOUNT_TOKEN|GITHUB_TOKEN|"
    r"GRAFANA_CLOUD_TOKEN|PASSWORD|API_KEY|ACCESS_TOKEN|SECRET|"
    r"PRIVATE_KEY|CLIENT_SECRET)\s*[:=]\s*(.+)"
)
PRIVATE_KEY = re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")
# Placeholder shapes are split in two (b4-scripts / 2026-11-28). Word tokens
# use explicit alphanumeric boundaries: plain `\b` counts `_` as a word
# character, so `dummy_value_123` must stay a placeholder while a token inside
# a real value (`A1B2C3exampleXYZ`) must no longer fail open as one. Structural
# shapes keep search semantics for `your_`/`xxx`, but interpolation/reference
# syntax (`<...>`, `${...}`, `op://`) is only recognised at the start of the
# value because the same syntax can occur inside a real credential.
PLACEHOLDER_WORD = re.compile(
    r"(?i)(?<![A-Za-z0-9])(?:changeme|replace[_-]?me|example|placeholder|"
    r"dummy|redact|template|none|null|todo|tbd)(?![A-Za-z0-9])"
)
PLACEHOLDER_STRUCT = re.compile(
    r"(?i)(?:(?<![A-Za-z0-9])xxx+(?![A-Za-z0-9])|(?<![A-Za-z0-9])your[_-]|^<[^>]+>|^\$\{|^op://|"
    r"^\s*['\"]?\s*['\"]?\s*$)"
)
# These files define detection regexes or document credential NAMES, not values.
# docs/ is intentionally NOT a safe path (I7-B): placeholder examples in docs
# are recognised via the patterns above, but a value-like literal in
# documentation must fail the gate just like one in source code.
# Membership is EXACT, not prefix-based: a look-alike path such as
# `AGENTS.md.bak` or `ci/review-gate.yml.template` must not silently inherit
# the exemption of its namesake (static-guard coverage gap).
SAFE_PATHS = (
    # Scanners/pattern definitions legitimately contain credential-shaped
    # literals (e.g. `PRIVATE_KEY = re.compile(...)`); treat as review-only.
    "ci/scripts/secret_scan.py",
    "infra/config/pii_patterns.toml",
    "AGENTS.md",
    "ci/review-gate.yml",
    "infra/README.md",
)


# Fixed vocabulary for fail-closed gates: tests match these strings and they
# never embed a file path or a credential value.
ERR_NO_ROOT = "ERROR: cannot determine repo root from script path"
ERR_INDEX_MISMATCH = "ERROR: scan set/index mismatch"


def repo_root() -> str | None:
    """Resolve the repo root from this script's location, not the caller's CWD.

    Asking git from the script's own directory also keeps the throwaway repos
    in test-legacy-guards.sh working: the copied scanner sits at that repo's
    root, so the same lookup returns the intended root there too.
    """
    script_dir = os.path.dirname(os.path.realpath(__file__))
    try:
        result = subprocess.run(
            ["git", "-C", script_dir, "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True,
        )
    except (subprocess.CalledProcessError, OSError):
        return None
    return result.stdout.strip() or None


def _nul_lines(raw: str) -> list[str]:
    """Split NUL-delimited git output, dropping the trailing empty field."""
    return [item for item in raw.split("\0") if item]


def tracked_files(root: str) -> list[str]:
    """Full tracked-file list anchored at the repo root (I81-B).

    `-z` is NUL-delimited so unusual filenames cannot be split accidentally.
    Paths stay repo-relative: that preserves SAFE_PATHS exact matching and the
    report format; callers open them under `root`, never under the CWD.
    """
    result = subprocess.run(
        ["git", "-C", root, "ls-files", "-z"],
        capture_output=True, text=True, check=True,
    )
    return _nul_lines(result.stdout)


def index_count(root: str) -> int:
    """Cached-index count from the same root, for the tracked_files guard.

    This deliberately reads the same index as tracked_files(); the comparison
    in main() guards against a future change to that function, not against an
    independent CWD leak. repo_root() + `-C root` is the primary CWD defense.
    """
    result = subprocess.run(
        ["git", "-C", root, "ls-files", "--cached", "-z"],
        capture_output=True, text=True, check=True,
    )
    return len(_nul_lines(result.stdout))


def is_placeholder(value: str) -> bool:
    """Return True when a stripped value has a known placeholder shape.

    Pure helper: no scan, no output, no global state. The bounded word tokens
    close the b4-scripts / 2026-11-28 substring fail-open; structural shapes
    are anchored where mid-value syntax can belong to a real credential.
    """
    return bool(PLACEHOLDER_WORD.search(value) or PLACEHOLDER_STRUCT.search(value))


def classify(value: str) -> str:
    """Classify a credential-shaped value without ever returning it.

    b4-scripts / 2026-11-28: substring matching fail-opened on values such as
    `A1B2C3exampleXYZ`; bounded word tokens and value-anchored structural
    shapes replace it. The `len(stripped) < 8` baseline is unchanged.
    """
    # A bare '#' is part of the credential value; only a whitespace-preceded
    # '#' starts a trailing comment (R4 / 2026-11-28).
    stripped = re.split(r"\s+#", value.strip().strip("\"'"), 1)[0].strip()
    if not stripped or len(stripped) < 8 or is_placeholder(stripped):
        return "placeholder"
    return "VALUE-LIKE"


def main() -> int:
    findings: list[tuple[str, int, str, str]] = []
    errors: list[tuple[str, str]] = []
    root = repo_root()
    if root is None:
        # Fixed error only: no path and no value is ever echoed here.
        print(ERR_NO_ROOT, file=sys.stderr)
        return 1
    tracked = tracked_files(root)
    if not tracked:
        # Fail closed (I79-C): an empty tracked set is indistinguishable from
        # a broken/bypassed `git ls-files` (e.g. a repo with only `git init`).
        # The old path printed "scanned 0 tracked files ... OK" and exited 0,
        # unlike check-toml.sh / check-shell.sh which reject a zero-target
        # scan. Report only a fixed error on stderr -- never any value.
        print("ERROR: no tracked files to scan", file=sys.stderr)
        return 1
    # Regression guard for tracked_files(): both listings are anchored at
    # `root`, so this detects a future change to that function (e.g. a
    # CWD-relative or filtered listing), not an independent CWD leak.
    # CWD independence itself is provided above by repo_root() + `-C root`.
    if len(tracked) != index_count(root):
        print(ERR_INDEX_MISMATCH, file=sys.stderr)
        return 1
    for path in tracked:
        try:
            # Open under the derived root, never under the caller's CWD.
            full_path = os.path.join(root, path)
            text = open(full_path, encoding="utf-8", errors="replace").read()
        except OSError as exc:
            # An unreadable tracked file must not silently drop out of the
            # scan: record it so the gate fails closed. Report only the path
            # and exception class -- never file contents or a secret value.
            errors.append((path, type(exc).__name__))
            continue
        safe = path in SAFE_PATHS  # exact match, never a prefix
        for lineno, line in enumerate(text.splitlines(), 1):
            if PRIVATE_KEY.search(line):
                findings.append((path, lineno, "PRIVATE_KEY", "VALUE-LIKE"))
                continue
            match = ASSIGNMENT.search(line)
            if not match:
                continue
            kind = classify(match.group(2))
            # Docs/examples are allowed to show shapes, but flag a long
            # literal even there so copied credentials are caught.
            if kind == "VALUE-LIKE" and safe:
                kind = "review (safe path)"
            findings.append((path, lineno, match.group(1), kind))

    value_like = [f for f in findings if f[3] == "VALUE-LIKE"]
    for path, lineno, key, kind in findings:
        print(f"{path}:{lineno}: {key} -> {kind}")
    for path, exc_type in errors:
        print(f" ERROR: could not read {path} ({exc_type}); value withheld")
    print(
        f" scanned {len(tracked)} tracked files, {len(findings)} matches, "
        f"{len(errors)} read error(s)"
    )
    failed = False
    if value_like:
        print(f" FAIL: {len(value_like)} value-like credential(s); values withheld")
        failed = True
    if errors:
        print(f" FAIL: {len(errors)} tracked file(s) unreadable; scan incomplete")
        failed = True
    if failed:
        return 1
    print(" OK: no value-like credentials found")
    return 0


if __name__ == "__main__":
    sys.exit(main())
