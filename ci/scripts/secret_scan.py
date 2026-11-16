#!/usr/bin/env python3
"""Scan tracked files for accidentally committed secrets.

Safety properties:
- Only files tracked by Git are inspected.
- Secret values are NEVER printed: the report shows file, line, key name and
  a classification only.
- Known-safe matches (placeholders, documented examples, and the repo's own
  PII-detection regex definitions) are classified as such, not as findings.

Usage: python3 ci/scripts/secret_scan.py
Exit 0 when no value-like credentials are found, 1 otherwise.
"""

from __future__ import annotations

import re
import subprocess
import sys

ASSIGNMENT = re.compile(
    r"(?i)\b(BWS_ACCESS_TOKEN|OP_SERVICE_ACCOUNT_TOKEN|GITHUB_TOKEN|"
    r"GRAFANA_CLOUD_TOKEN|PASSWORD|API_KEY|ACCESS_TOKEN|SECRET|"
    r"PRIVATE_KEY|CLIENT_SECRET)\s*[:=]\s*(.+)"
)
PRIVATE_KEY = re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")
PLACEHOLDER = re.compile(
    r"(?i)(changeme|your[_-]|xxx+|<[^>]+>|\$\{|op://|example|placeholder|"
    r"dummy|redact|template|none|null|todo|tbd|^\s*['\"]?\s*['\"]?\s*$)"
)
# These files define detection regexes or document credential NAMES, not values.
SAFE_PATHS = (
    "infra/config/pii_patterns.toml",
    "AGENTS.md",
    "docs/",
    "ci/review-gate.yml",
    "infra/README.md",
)


def tracked_files() -> list[str]:
    result = subprocess.run(
        ["git", "ls-files"], capture_output=True, text=True, check=True
    )
    return result.stdout.splitlines()


def classify(value: str) -> str:
    stripped = value.strip().strip("\"'").split("#", 1)[0].strip()
    if not stripped or PLACEHOLDER.search(stripped) or len(stripped) < 8:
        return "placeholder"
    return "VALUE-LIKE"


def main() -> int:
    findings: list[tuple[str, int, str, str]] = []
    for path in tracked_files():
        try:
            text = open(path, encoding="utf-8", errors="replace").read()
        except OSError:
            continue
        safe = path.startswith(SAFE_PATHS)
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
    print(f" scanned {len(tracked_files())} tracked files, {len(findings)} matches")
    if value_like:
        print(f" FAIL: {len(value_like)} value-like credential(s); values withheld")
        return 1
    print(" OK: no value-like credentials found")
    return 0


if __name__ == "__main__":
    sys.exit(main())
