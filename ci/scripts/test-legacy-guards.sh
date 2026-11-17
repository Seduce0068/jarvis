#!/usr/bin/env bash
# Regression guard: the legacy OCI/destructive launchers must stay disabled.
# A future edit that re-enables delegation to backup.rs/restore.rs/deploy.rs
# would silently reintroduce data loss or an out-of-policy OCI upload.
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

ok=0; fail=0
check_guard() { # script expected-hint
  local script="$1" hint="$2" out
  out=$(bash "$repo/$script" 2>&1)
  local code=$?
  if [ "$code" -eq 2 ] && printf '%s' "$out" | grep -qi "$hint"; then
    echo "  PASS: $script remains fail-closed (exit 2, hint present)"
    ok=$((ok+1))
  else
    echo "  FAIL: $script code=$code hint='$hint' output=$(printf '%s' "$out" | head -1)"
    fail=$((fail+1))
  fi
}

check_guard infra/scripts/backup.sh 'backup-preflight'
check_guard infra/scripts/restore.sh 'backup-redesign-notes'
check_guard infra/scripts/deploy.sh 'deploy-mcp-gw-safe'

# The new local backup must stay non-destructive by default: a dry run must
# not create the destination directory at all.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
if bash "$repo/infra/scripts/backup-local.sh" --backup-dir "$tmp/dest" >/dev/null 2>&1 \
   && [ ! -e "$tmp/dest" ]; then
  echo "  PASS: backup-local.sh dry run writes nothing"
  ok=$((ok+1))
else
  echo "  FAIL: backup-local.sh dry run created files or failed"
  fail=$((fail+1))
fi

# Help output must work for all new scripts (cheap smoke test for syntax).
for script in backup-local.sh verify-backup.sh install-backup-timer.sh; do
  if bash "$repo/infra/scripts/$script" --help >/dev/null 2>&1; then
    echo "  PASS: $script --help"
    ok=$((ok+1))
  else
    echo "  FAIL: $script --help"
    fail=$((fail+1))
  fi
done

# install-backup-timer.sh must roll back on every explicit failure after the
# install phase. Static guard: no bare "exit 1" in the post-install region.
install_script="$repo/infra/scripts/install-backup-timer.sh"
post_install="$(sed -n '/^install -o root/,$p' "$install_script")"
if printf '%s' "$post_install" | grep -qE '[[:space:]]exit 1'; then
  echo "  FAIL: install-backup-timer.sh has bare exit 1 after install (rollback skipped)"
  fail=$((fail+1))
else
  echo "  PASS: install-backup-timer.sh uses fail() for post-install errors"
  ok=$((ok+1))
fi

# P0-9: drift check must flag a missing deployed allowlist as DANGER, not
# silently skip all allowlist/web.fetch checks.
drift_script="$repo/infra/scripts/check-deploy-drift.sh"
out=$(JARVIS_DRIFT_ALLOWLIST=/nonexistent/allowlist.toml bash "$drift_script" 2>&1)
if printf '%s' "$out" | grep -q "MISSING (deployed)"; then
  echo "  PASS: drift flags missing deployed allowlist"
  ok=$((ok+1))
else
  echo "  FAIL: drift silently skipped missing allowlist"
  fail=$((fail+1))
fi

# P2-12: drift check must compare Cedar policy/schema and PII patterns.
tmp_drift="$(mktemp -d)"
mkdir -p "$tmp_drift/policy"
cp "$repo/infra/config/policy/policies.cedar" "$tmp_drift/policy/"
cp "$repo/infra/config/policy/casbin_policy.csv" "$tmp_drift/policy/"
cp "$repo/infra/config/pii_patterns.toml" "$tmp_drift/pii_patterns.toml"
# Deliberately stale cedar schema -> must report DIFF.
printf '// stale\n' > "$tmp_drift/policy/cedar_schema.cedarschema"
out=$(JARVIS_DRIFT_POLICY_DIR="$tmp_drift/policy" \
      JARVIS_DRIFT_PII="$tmp_drift/pii_patterns.toml" \
      JARVIS_DRIFT_ALLOWLIST=/nonexistent \
      bash "$repo/infra/scripts/check-deploy-drift.sh" 2>&1)
if printf '%s' "$out" | grep -q "cedar_schema.cedarschema.*DIFF"; then
  echo "  PASS: drift detects stale cedar schema"
  ok=$((ok+1))
else
  echo "  FAIL: drift missed stale cedar schema"
  fail=$((fail+1))
fi
rm -rf "$tmp_drift"

echo "result: ok=$ok fail=$fail"
[ "$fail" -eq 0 ]
