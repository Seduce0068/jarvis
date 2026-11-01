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
sudo cp -r "${REPO_DIR}/infra/config/policy" /etc/jarvis/
sudo cp "${REPO_DIR}/infra/config/pii_patterns.toml" /etc/jarvis/
sudo cp "${REPO_DIR}/infra/config/mcp_allowlist.toml" /etc/jarvis/

for unit in \
  "${REPO_DIR}"/infra/systemd/jarvis-policy.service \
  "${REPO_DIR}"/infra/systemd/jarvis-mcp-gw.service \
  "${REPO_DIR}"/infra/systemd/jarvis-event.service \
  "${REPO_DIR}"/infra/systemd/jarvis-context.service \
  "${REPO_DIR}"/infra/systemd/qdrant.service \
  "${REPO_DIR}"/infra/systemd/opencode.service; do
  name="$(basename "${unit}")"
  sed -e "s/^User=jarvis$/User=${JARVIS_USER}/" \
      -e "s/^Group=jarvis$/Group=${JARVIS_USER}/" \
      -e "s|/home/jarvis|/home/${JARVIS_USER}|g" \
      -e "s/--port 49374/--port 49375/" \
      "${unit}" \
    | sudo tee "/etc/systemd/system/${name}" >/dev/null
done

sudo systemctl daemon-reload

echo "binaries and units installed; create /etc/jarvis/bws.env before starting services"
echo "start with:"
echo "  sudo systemctl enable --now qdrant opencode jarvis-policy jarvis-mcp-gw jarvis-event jarvis-context"
