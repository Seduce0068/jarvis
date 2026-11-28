#!/usr/bin/env bash
# Self-hosted host setup for JARVIS (Ubuntu/Debian, x86_64).
#
# Run as root on the target host:
#   sudo JARVIS_USER=jarvis bash setup-selfhost.sh
#
# Installs the runtime dependencies, creates the JARVIS directories, and
# provisions PostgreSQL. Building and deploying the Rust services is handled
# by build-selfhost.sh.
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
  echo "run as root (sudo)" >&2
  exit 1
fi

JARVIS_USER="${JARVIS_USER:-jarvis}"
QDRANT_VERSION="${QDRANT_VERSION:-v1.14.1}"
BWS_VERSION="${BWS_VERSION:-2.1.0}"
ARCH="$(uname -m)"

if [[ ${ARCH} != "x86_64" ]]; then
  echo "this script targets x86_64; detected ${ARCH}" >&2
  exit 1
fi

WORK_DIR="$(mktemp -d /tmp/jarvis-setup.XXXXXX)" || { echo "FAIL: mktemp -d failed" >&2; exit 1; }
trap 'rm -rf -- "$WORK_DIR"' EXIT

apt-get update
# I95 task (c): libssl-dev is deliberately NOT installed. CI dropped it when the
# rationale disappeared: no `openssl-sys` dependency remains in the workspace
# (no OpenSSL FFI to compile against), and the TLS backend is `aws-lc-sys`,
# which is self-contained (vendored sources; no system OpenSSL headers needed).
# Keep this apt list in sync with the shared rust-gates CI apt-dependency input.
# Ref: todo.md Round 88 ("libssl-dev の根拠消滅" / CI apt dependency cleanup).
DEBIAN_FRONTEND=noninteractive apt-get install -y \
  build-essential pkg-config protobuf-compiler \
  postgresql postgresql-client firejail bubblewrap \
  curl unzip jq ca-certificates

# --- Release artifact integrity (R4-DP3, supply chain) -------------------
# The third-party release artifacts fetched in this script (Qdrant below, plus
# the otelcol-contrib .deb and bws .zip further down, and the code-server
# install.sh that is piped to sh) are retrieved over HTTPS but are NOT
# checksum/signature verified.
#
# As of this revision the repository has NO authoritative verification source
# for these artifacts: there is no SHA256SUMS/.sha256 file, no published
# expected digest, and no signing key or signature in README/docs/release
# config. Verification therefore cannot be turned on without inventing a hash
# or adding an untrusted fetch, so none is done here (no value is hardcoded).
#
# Recommended follow-up (blocked on the release side): publish a vendor-signed
# checksum manifest (e.g. SHA256SUMS, ideally with a GPG/sigstore signature) for
# each pinned release, pin that manifest in this repo (or fetch it from the same
# trusted release), and abort before install when `sha256sum -c` / signature
# verification fails. Until such a source exists, these downloads remain
# unverified by design. See todo.md (R4-DP3) and
# docs/integration/remaining-tasks.md ("supply chain リスク").
# -------------------------------------------------------------------------

# Qdrant (musl static binary)
QDRANT_ARCHIVE="$WORK_DIR/qdrant.tar.gz"
curl -fsSL -o "$QDRANT_ARCHIVE" \
  "https://github.com/qdrant/qdrant/releases/download/${QDRANT_VERSION}/qdrant-x86_64-unknown-linux-musl.tar.gz"
tar -xzf "$QDRANT_ARCHIVE" -C /usr/local/bin qdrant
chmod 0755 /usr/local/bin/qdrant

# code-server
if ! command -v code-server >/dev/null 2>&1; then
  curl -fsSL https://code-server.dev/install.sh | sh
fi

# code-server password (generated once; the unit reads config.yaml)
CODE_SERVER_CONFIG="/home/${JARVIS_USER}/.config/code-server/config.yaml"
if [[ ! -f "${CODE_SERVER_CONFIG}" ]]; then
  CODE_SERVER_PASSWORD="$(python3 -c 'import secrets; print(secrets.token_urlsafe(24))')"
  install -d -o "${JARVIS_USER}" -g "${JARVIS_USER}" \
    "/home/${JARVIS_USER}/.config/code-server" \
    "/home/${JARVIS_USER}/.local/share/code-server" \
    "/home/${JARVIS_USER}/repos"
  cat > "${CODE_SERVER_CONFIG}" <<EOF
bind-addr: 127.0.0.1:8080
auth: password
password: ${CODE_SERVER_PASSWORD}
cert: false
EOF
  chown "${JARVIS_USER}:${JARVIS_USER}" "${CODE_SERVER_CONFIG}"
  chmod 600 "${CODE_SERVER_CONFIG}"
  # Do not print the generated credential; it is stored in a 0600 config
  # file the operator can read directly if needed (P2-11).
  unset CODE_SERVER_PASSWORD
  echo "code-server password generated and stored in ${CODE_SERVER_CONFIG}"
fi

# OpenTelemetry Collector (deb package; the tarball was retired)
if ! command -v otelcol-contrib >/dev/null 2>&1; then
  OTELCOL_VERSION="${OTELCOL_VERSION:-0.164.0}"
  OTELCOL_DEB="$WORK_DIR/otelcol.deb"
  OTELCOL_EXTRACT="$WORK_DIR/otelcol-extract"
  curl -fsSL -o "$OTELCOL_DEB" \
    "https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v${OTELCOL_VERSION}/otelcol-contrib_${OTELCOL_VERSION}_linux_amd64.deb"
  dpkg -x "$OTELCOL_DEB" "$OTELCOL_EXTRACT"
  install -o root -g root -m 0755 "$OTELCOL_EXTRACT/usr/bin/otelcol-contrib" /usr/local/bin/otelcol-contrib
fi

# Bitwarden Secrets Manager CLI (musl static binary)
BWS_ZIP="$WORK_DIR/bws.zip"
curl -fsSL -o "$BWS_ZIP" \
  "https://github.com/bitwarden/sdk-sm/releases/download/bws-v${BWS_VERSION}/bws-x86_64-unknown-linux-musl-${BWS_VERSION}.zip"
unzip -o "$BWS_ZIP" -d /usr/local/bin
chmod 0755 /usr/local/bin/bws

# OpenCode is installed in the user's home by its installer.
if [[ -x "/home/${JARVIS_USER}/.local/bin/opencode" ]]; then
  ln -sf "/home/${JARVIS_USER}/.local/bin/opencode" /usr/local/bin/opencode
fi

# Directories
mkdir -p /opt/jarvis/{bin,data,logs,scripts,rust} /etc/jarvis /var/lib/jarvis
# I71-B: Limit the recursive chown to runtime DATA directories only.
# /opt/jarvis/bin and /opt/jarvis/scripts stay root-owned: install-backup-timer.sh
# installs backup-local.sh / verify-backup.sh there as root:root 0755 and
# jarvis-backup.service executes them as User=postgres. If the jarvis user owned
# those trees it could rewrite a script that is then run with postgres privileges
# (jarvis -> postgres escalation). Re-assert root:root on EVERY run so a re-run
# cannot undo the installer's `install -o root -g root` placement: this is the
# idempotent guard even when an older run chowned the whole tree to jarvis.
# root 所有を維持する理由: postgres 権限スクリプトの改ざん防止 (I71-B)。
chown -R "${JARVIS_USER}:${JARVIS_USER}" \
  /opt/jarvis/data /opt/jarvis/logs /var/lib/jarvis
chown -R root:root /opt/jarvis/bin /opt/jarvis/scripts

# PostgreSQL
systemctl enable --now postgresql
# P2-11: never hardcode a database password. Require the operator to supply
# one via the environment; peer auth / a secrets manager can also be used.
if [ -z "${JARVIS_DB_PASSWORD:-}" ]; then
  echo "JARVIS_DB_PASSWORD is not set; skipping PostgreSQL role creation." >&2
  echo "Set it to a strong value and re-run, or provision the role out-of-band." >&2
else
  sudo -u postgres psql -v pw="${JARVIS_DB_PASSWORD}" <<'SQL' || true
CREATE USER jarvis WITH PASSWORD :'pw';
SQL
  unset JARVIS_DB_PASSWORD
fi
sudo -u postgres psql -c "CREATE DATABASE jarvis OWNER jarvis;" || true
sudo -u postgres psql -c "GRANT ALL PRIVILEGES ON DATABASE jarvis TO jarvis;" || true

echo "base setup complete; run build-selfhost.sh as ${JARVIS_USER}"
