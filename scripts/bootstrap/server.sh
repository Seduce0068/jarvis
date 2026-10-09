#!/usr/bin/env bash
# JARVIS Server Bootstrap — Linux/OCI/VPS
# OpenClaw gateway + local JARVIS config scaffold

set -Eeuo pipefail
IFS=$'\n\t'
umask 077

SCRIPT_NAME="${0##*/}"
DRY_RUN=0
INSTALL_OPENCLAW=1
JARVIS_ROOT="${JARVIS_ROOT:-$HOME/jarvis}"
OPENCLAW_INSTALL_URL="${OPENCLAW_INSTALL_URL:-https://openclaw.ai/install.sh}"

log()  { printf '[%s] %s\n' "$SCRIPT_NAME" "$*"; }
warn() { printf '[%s] WARNING: %s\n' "$SCRIPT_NAME" "$*" >&2; }
die()  { printf '[%s] ERROR: %s\n' "$SCRIPT_NAME" "$*" >&2; exit 1; }

run() {
  if (( DRY_RUN )); then
    printf '+'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

usage() {
  cat <<'EOF'
Usage:
  ./server.sh [options]

Options:
  --dry-run           実行せずコマンドだけ表示
  --skip-openclaw     OpenClawをインストールしない
  --root DIR          作業ディレクトリを変更 (既定: ~/jarvis)
  -h, --help          ヘルプ

Examples:
  chmod +x scripts/bootstrap/server.sh
  ./scripts/bootstrap/server.sh
  ./scripts/bootstrap/server.sh --dry-run --root /opt/jarvis
EOF
}

while (($#)); do
  case "$1" in
    --dry-run)      DRY_RUN=1; shift ;;
    --skip-openclaw) INSTALL_OPENCLAW=0; shift ;;
    --root)         [[ $# -ge 2 ]] || die "--root requires an argument"; JARVIS_ROOT="$2"; shift 2 ;;
    -h|--help)      usage; exit 0 ;;
    *)              die "unknown option: $1" ;;
  esac
done

[[ "$(uname -s)" == "Linux" ]] || die "server target currently supports Linux"

detect_pkg_manager() {
  if command -v apt-get >/dev/null 2>&1; then echo apt
  elif command -v dnf >/dev/null 2>&1; then echo dnf
  elif command -v yum >/dev/null 2>&1; then echo yum
  elif command -v pacman >/dev/null 2>&1; then echo pacman
  else echo unknown; fi
}

install_linux_packages() {
  local pm; pm="$(detect_pkg_manager)"
  log "package manager: $pm"
  case "$pm" in
    apt)  run sudo apt-get update && run sudo apt-get install -y ca-certificates curl git jq openssl openssh-client python3 ;;
    dnf)  run sudo dnf install -y ca-certificates curl git jq openssl openssh-clients python3 ;;
    yum)  run sudo yum install -y ca-certificates curl git jq openssl openssh-clients python3 ;;
    pacman) run sudo pacman -Sy --needed --noconfirm ca-certificates curl git jq openssl openssh python ;;
    *) die "unsupported Linux package manager" ;;
  esac
}

create_server_layout() {
  log "creating JARVIS server layout under $JARVIS_ROOT"
  run mkdir -p "$JARVIS_ROOT"/{bin,config,state,logs,policies}

  if (( ! DRY_RUN )); then
    # jarvis.env
    cat >"$JARVIS_ROOT/config/jarvis.env" <<'EOF'
# chmod 600 is applied by bootstrap.
# Do not commit this file.

# Home Assistant MCP:
# HA_MCP_URL=https://homeassistant.example/api/mcp
# HA_TOKEN=REPLACE_ME

# Android MCP (via VPN/tunnel):
# ANDROID_MCP_URL=http://127.0.0.1:8765/mcp
# ANDROID_MCP_TOKEN=REPLACE_ME

# Optional model/provider credentials:
# OPENAI_API_KEY=
# ANTHROPIC_API_KEY=
# GEMINI_API_KEY=
# OPENROUTER_API_KEY=
EOF
    chmod 600 "$JARVIS_ROOT/config/jarvis.env"

    # tool-policy.json
    cat >"$JARVIS_ROOT/policies/tool-policy.json" <<'EOF'
{
  "default": "deny",
  "classes": {
    "read_only": {
      "decision": "allow",
      "examples": ["read calendar", "read device state", "read Home Assistant entity state"]
    },
    "notify": {
      "decision": "allow",
      "examples": ["send local notification", "speak TTS"]
    },
    "write_low_risk": {
      "decision": "approval_required",
      "examples": ["toggle ordinary light", "change non-security smart-home state"]
    },
    "external_side_effect": {
      "decision": "approval_required",
      "examples": ["send email", "post message", "delete file", "install package", "modify Android secure settings"]
    },
    "financial": {
      "decision": "deny_without_explicit_session_approval",
      "examples": ["purchase", "transfer", "payment"]
    }
  }
}
EOF

    # jarvis-doctor
    cat >"$JARVIS_ROOT/bin/jarvis-doctor" <<'EOF'
#!/usr/bin/env bash
set -u

printf '%-24s %s\n' "component" "status"
printf '%-24s %s\n' "------------------------" "----------------"

if command -v openclaw >/dev/null 2>&1; then
  printf '%-24s %s\n' "OpenClaw" "$(openclaw --version 2>/dev/null || echo installed)"
else
  printf '%-24s %s\n' "OpenClaw" "MISSING"
fi

for cmd in curl git jq python3; do
  if command -v "$cmd" >/dev/null 2>&1; then
    printf '%-24s %s\n' "$cmd" "OK"
  else
    printf '%-24s %s\n' "$cmd" "MISSING"
  fi
done

if command -v tailscale >/dev/null 2>&1; then
  printf '%-24s %s\n' "Tailscale" "installed"
else
  printf '%-24s %s\n' "Tailscale" "optional / not installed"
fi

ENV_FILE="${JARVIS_ROOT:-$HOME/jarvis}/config/jarvis.env"
if [[ -f "$ENV_FILE" ]]; then
  printf '%-24s %s\n' "jarvis.env" "present"
else
  printf '%-24s %s\n' "jarvis.env" "MISSING"
fi
EOF
    chmod 700 "$JARVIS_ROOT/bin/jarvis-doctor"
  fi
}

install_openclaw() {
  (( INSTALL_OPENCLAW )) || { log "skipping OpenClaw install"; return; }
  if command -v openclaw >/dev/null 2>&1; then
    log "OpenClaw already installed: $(openclaw --version 2>/dev/null || true)"
    return
  fi
  log "installing OpenClaw using the official installer"
  log "source: https://docs.openclaw.ai/install"
  if (( DRY_RUN )); then
    log "would download: $OPENCLAW_INSTALL_URL"
    log "would run installer with --no-onboard"
    return
  fi
  local installer; installer="$(mktemp)"
  trap 'rm -f "${installer:-}"' RETURN
  curl --fail --show-error --location --proto '=https' --tlsv1.2 "$OPENCLAW_INSTALL_URL" --output "$installer"
  chmod 700 "$installer"
  bash "$installer" --no-onboard
  rm -f "$installer"
  trap - RETURN
  command -v openclaw >/dev/null 2>&1 || warn "OpenClaw installer completed but 'openclaw' is not currently on PATH."
}

server_next_steps() {
  cat <<EOF

Server bootstrap complete.

Next:
  1. Configure model credentials interactively:
     openclaw onboard --install-daemon

  2. Validate:
     openclaw doctor
     openclaw gateway status

  3. Edit secrets/config:
     ${JARVIS_ROOT}/config/jarvis.env

  4. Home Assistant:
     Add "Model Context Protocol Server" in Home Assistant.
     Endpoint: /api/mcp
     Use a Home Assistant token and expose only required entities.

  5. Keep Gateway private:
     Use Tailscale, WireGuard or an SSH tunnel.
     Do not directly expose the OpenClaw Gateway port to the public Internet.

  6. Local scaffold doctor:
     ${JARVIS_ROOT}/bin/jarvis-doctor

EOF
}

main() {
  install_linux_packages
  create_server_layout
  install_openclaw
  server_next_steps
}

main "$@"