#!/usr/bin/env bash
# Build and deploy the JARVIS Rust sidecars on the self-hosted host.
#
# Run as the JARVIS user (not root):
#   bash build-selfhost.sh
#
# Expects the repository at ~/jarvis and setup-selfhost.sh to have run.
set -euo pipefail

REPO_DIR="${REPO_DIR:-$HOME/jarvis}"
JARVIS_USER="${JARVIS_USER:-$(id -un)}"

if [[ ! -d "${REPO_DIR}/rust" ]]; then
  echo "repository not found at ${REPO_DIR}" >&2
  exit 1
fi

cd "${REPO_DIR}/rust"
cargo build --release --workspace

for bin in jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context; do
  install -m 755 "target/release/${bin}" "/opt/jarvis/bin/${bin}"
done

sudo mkdir -p /etc/jarvis
# Copy the *contents* of infra/config/policy/ directly into /etc/jarvis/policy/.
# A plain `cp -r <src> /etc/jarvis/` nests <src>/policy inside an existing
# /etc/jarvis/policy, leaving the stale config effective (todo.md 2026-11-19).
POLICY_SRC="${REPO_DIR}/infra/config/policy"
POLICY_DST="/etc/jarvis/policy"
if [[ -e "${POLICY_DST}" || -L "${POLICY_DST}" ]]; then
  policy_backup="${POLICY_DST}.bak-$(date +%Y%m%d%H%M%S)"
  sudo mv "${POLICY_DST}" "${policy_backup}"
  echo "backed up existing policy dir to ${policy_backup}"
fi
sudo mkdir -p "${POLICY_DST}"
sudo cp -r "${POLICY_SRC}/." "${POLICY_DST}/"
sudo cp "${REPO_DIR}/infra/config/pii_patterns.toml" /etc/jarvis/
sudo cp "${REPO_DIR}/infra/config/mcp_allowlist.toml" /etc/jarvis/

for unit in \
  "${REPO_DIR}"/infra/systemd/jarvis-policy.service \
  "${REPO_DIR}"/infra/systemd/jarvis-mcp-gw.service \
  "${REPO_DIR}"/infra/systemd/jarvis-event.service \
  "${REPO_DIR}"/infra/systemd/jarvis-context.service \
  "${REPO_DIR}"/infra/systemd/qdrant.service \
  "${REPO_DIR}"/infra/systemd/opencode.service \
  "${REPO_DIR}"/infra/systemd/code-server.service \
  "${REPO_DIR}"/infra/systemd/otelcol.service; do
  name="$(basename "${unit}")"
  sed -e "s/^User=jarvis$/User=${JARVIS_USER}/" \
      -e "s/^Group=jarvis$/Group=${JARVIS_USER}/" \
      -e "s|/home/jarvis|/home/${JARVIS_USER}|g" \
      -e "/^EnvironmentFile=-\/etc\/jarvis\/op.env/d" \
      -e "/^Environment=PASSWORD_FILE=/d" \
      -e "s/^ProtectHome=yes$/ProtectHome=read-only/" \
      -e "s|^ExecStart=/usr/bin/op run --env-file=/etc/jarvis/otel.env -- |ExecStart=|" \
      "${unit}" \
    | sudo tee "/etc/systemd/system/${name}" >/dev/null
done

sudo systemctl daemon-reload

echo "binaries and units installed; create /etc/jarvis/bws.env before starting services"
echo "start with:"
echo "  sudo systemctl enable --now qdrant opencode jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context"
