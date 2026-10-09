#!/usr/bin/env bash
# JARVIS bootstrap for:
#   - Linux/OCI/VPS: OpenClaw gateway + local JARVIS config scaffold
#   - Android/Termux: Termux:API + android-shizuku-mcp scaffold
#
# Sources verified 2026-09-27:
#   OpenClaw install:
#     https://docs.openclaw.ai/install
#   OpenClaw Docker/hosting guidance:
#     https://docs.openclaw.ai/install/docker-vm-runtime
#   Home Assistant MCP:
#     https://www.home-assistant.io/integrations/mcp_server
#   android-shizuku-mcp:
#     https://github.com/shizzgar/shizuku-mcp
#
# Security model:
#   - Do not embed API keys in this script.
#   - Keep generated secret files mode 0600.
#   - Do not expose OpenClaw Gateway or Android MCP directly to the public Internet.
#   - Prefer Tailscale/WireGuard/SSH tunnels.
#   - Shizuku/rish grants adb-shell-class privileges. Treat it as privileged access.

set -Eeuo pipefail
IFS=$'\n\t'
umask 077

SCRIPT_NAME="${0##*/}"
TARGET=""
DRY_RUN=0
INSTALL_OPENCLAW=1
INSTALL_ANDROID_MCP=1
SETUP_BOOT=0
JARVIS_ROOT="${JARVIS_ROOT:-$HOME/jarvis}"
OPENCLAW_INSTALL_URL="${OPENCLAW_INSTALL_URL:-https://openclaw.ai/install.sh}"
ANDROID_MCP_REPO="${ANDROID_MCP_REPO:-https://github.com/shizzgar/shizuku-mcp.git}"

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
  ./jarvis-bootstrap.sh --target server [options]
  ./jarvis-bootstrap.sh --target termux [options]

Targets:
  server   Linux/OCI/VPS側を構築
  termux   Android Termux側を構築

Options:
  --dry-run              実行せずコマンドだけ表示
  --skip-openclaw        serverでOpenClawをインストールしない
  --skip-android-mcp     termuxでandroid-shizuku-mcpを入れない
  --setup-boot           termuxでリポジトリのsetup_boot.shも実行
  --root DIR             作業ディレクトリを変更
  -h, --help             ヘルプ

Examples:
  chmod +x jarvis-bootstrap.sh
  ./jarvis-bootstrap.sh --target server
  ./jarvis-bootstrap.sh --target termux
  ./jarvis-bootstrap.sh --target termux --setup-boot

After server setup:
  openclaw onboard --install-daemon
  openclaw doctor
  openclaw gateway status

Important:
  APIキー、Home Assistant Long-Lived Access Token等は
  ~/jarvis/config/jarvis.env に手動設定してください。
EOF
}

while (($#)); do
  case "$1" in
    --target)
      [[ $# -ge 2 ]] || die "--target requires an argument"
      TARGET="$2"
      shift 2
      ;;
    --root)
      [[ $# -ge 2 ]] || die "--root requires an argument"
      JARVIS_ROOT="$2"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --skip-openclaw)
      INSTALL_OPENCLAW=0
      shift
      ;;
    --skip-android-mcp)
      INSTALL_ANDROID_MCP=0
      shift
      ;;
    --setup-boot)
      SETUP_BOOT=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

[[ -n "$TARGET" ]] || { usage; die "--target is required"; }

is_termux() {
  [[ -n "${TERMUX_VERSION:-}" ]] ||
  [[ "${PREFIX:-}" == *"com.termux"* ]] ||
  [[ -d "/data/data/com.termux/files/usr" ]]
}

detect_pkg_manager() {
  if command -v apt-get >/dev/null 2>&1; then
    echo apt
  elif command -v dnf >/dev/null 2>&1; then
    echo dnf
  elif command -v yum >/dev/null 2>&1; then
    echo yum
  elif command -v pacman >/dev/null 2>&1; then
    echo pacman
  else
    echo unknown
  fi
}

install_linux_packages() {
  local pm
  pm="$(detect_pkg_manager)"
  log "package manager: $pm"

  case "$pm" in
    apt)
      run sudo apt-get update
      run sudo apt-get install -y \
        ca-certificates curl git jq openssl openssh-client python3
      ;;
    dnf)
      run sudo dnf install -y \
        ca-certificates curl git jq openssl openssh-clients python3
      ;;
    yum)
      run sudo yum install -y \
        ca-certificates curl git jq openssl openssh-clients python3
      ;;
    pacman)
      run sudo pacman -Sy --needed --noconfirm \
        ca-certificates curl git jq openssl openssh python
      ;;
    *)
      die "unsupported Linux package manager"
      ;;
  esac
}

create_server_layout() {
  log "creating JARVIS server layout under $JARVIS_ROOT"
  run mkdir -p \
    "$JARVIS_ROOT/bin" \
    "$JARVIS_ROOT/config" \
    "$JARVIS_ROOT/state" \
    "$JARVIS_ROOT/logs" \
    "$JARVIS_ROOT/policies"

  if (( ! DRY_RUN )); then
    cat >"$JARVIS_ROOT/config/jarvis.env" <<'EOF'
# chmod 600 is applied by bootstrap.
#
# Do not commit this file.
#
# Home Assistant MCP:
# HA_MCP_URL=https://homeassistant.example/api/mcp
# HA_TOKEN=REPLACE_ME
#
# Android MCP should normally be reached through VPN/tunnel.
# ANDROID_MCP_URL=http://127.0.0.1:8765/mcp
# ANDROID_MCP_TOKEN=REPLACE_ME
#
# Optional model/provider credentials.
# Prefer OpenClaw's own provider credential store/onboarding where supported.
# OPENAI_API_KEY=
# ANTHROPIC_API_KEY=
# GEMINI_API_KEY=
# OPENROUTER_API_KEY=
EOF
    chmod 600 "$JARVIS_ROOT/config/jarvis.env"

    cat >"$JARVIS_ROOT/policies/tool-policy.json" <<'EOF'
{
  "default": "deny",
  "classes": {
    "read_only": {
      "decision": "allow",
      "examples": [
        "read calendar",
        "read device state",
        "read Home Assistant entity state"
      ]
    },
    "notify": {
      "decision": "allow",
      "examples": [
        "send local notification",
        "speak TTS"
      ]
    },
    "write_low_risk": {
      "decision": "approval_required",
      "examples": [
        "toggle ordinary light",
        "change non-security smart-home state"
      ]
    },
    "external_side_effect": {
      "decision": "approval_required",
      "examples": [
        "send email",
        "post message",
        "delete file",
        "install package",
        "modify Android secure settings"
      ]
    },
    "financial": {
      "decision": "deny_without_explicit_session_approval",
      "examples": [
        "purchase",
        "transfer",
        "payment"
      ]
    }
  }
}
EOF

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
  (( INSTALL_OPENCLAW )) || {
    log "skipping OpenClaw install"
    return
  }

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

  local installer
  installer="$(mktemp)"
  trap 'rm -f "${installer:-}"' RETURN
  curl --fail --show-error --location \
    --proto '=https' --tlsv1.2 \
    "$OPENCLAW_INSTALL_URL" \
    --output "$installer"
  chmod 700 "$installer"

  # Download-then-execute rather than curl | bash so the fetched artifact
  # exists as a discrete file during execution.
  bash "$installer" --no-onboard
  rm -f "$installer"
  trap - RETURN

  command -v openclaw >/dev/null 2>&1 ||
    warn "OpenClaw installer completed but 'openclaw' is not currently on PATH."
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
       Endpoint:
         /api/mcp
       Use a Home Assistant token and expose only required entities.

  5. Keep Gateway private:
       Use Tailscale, WireGuard or an SSH tunnel.
       Do not directly expose the OpenClaw Gateway port to the public Internet.

  6. Local scaffold doctor:
       ${JARVIS_ROOT}/bin/jarvis-doctor

EOF
}

setup_server() {
  is_termux && die "server target must be run on Linux, not Termux"
  [[ "$(uname -s)" == "Linux" ]] || die "server target currently supports Linux"

  install_linux_packages
  create_server_layout
  install_openclaw
  server_next_steps
}

termux_install_packages() {
  command -v pkg >/dev/null 2>&1 || die "Termux pkg command not found"

  log "updating Termux packages"
  run pkg update -y
  run pkg install -y \
    git curl jq openssh python termux-api
}

create_termux_layout() {
  log "creating mobile layout under $JARVIS_ROOT"
  run mkdir -p \
    "$JARVIS_ROOT/bin" \
    "$JARVIS_ROOT/config" \
    "$JARVIS_ROOT/repos"

  if (( ! DRY_RUN )); then
    cat >"$JARVIS_ROOT/bin/phone-status-json" <<'EOF'
#!/data/data/com.termux/files/usr/bin/bash
set -u

battery='null'
wifi='null'

if command -v termux-battery-status >/dev/null 2>&1; then
  battery="$(termux-battery-status 2>/dev/null || printf 'null')"
fi

if command -v termux-wifi-connectioninfo >/dev/null 2>&1; then
  wifi="$(termux-wifi-connectioninfo 2>/dev/null || printf 'null')"
fi

jq -n \
  --argjson battery "$battery" \
  --argjson wifi "$wifi" \
  --arg ts "$(date -Iseconds)" \
  '{
    timestamp: $ts,
    battery: $battery,
    wifi: $wifi
  }'
EOF
    chmod 700 "$JARVIS_ROOT/bin/phone-status-json"

    cat >"$JARVIS_ROOT/config/mobile.env" <<'EOF'
# Generated placeholder.
# android-shizuku-mcp creates its own .env containing MCP_AUTH_TOKEN.
#
# Do not expose port 8765 directly to the Internet.
# Keep bind address on loopback unless a protected transport is deliberately used.
ANDROID_MCP_BIND=127.0.0.1
ANDROID_MCP_PORT=8765
EOF
    chmod 600 "$JARVIS_ROOT/config/mobile.env"
  fi
}

install_android_mcp() {
  (( INSTALL_ANDROID_MCP )) || {
    log "skipping android-shizuku-mcp install"
    return
  }

  local repo_dir="$JARVIS_ROOT/repos/shizuku-mcp"

  if [[ -d "$repo_dir/.git" ]]; then
    log "android-shizuku-mcp already cloned; updating"
    if (( DRY_RUN )); then
      printf '+ git -C %q pull --ff-only\n' "$repo_dir"
    else
      git -C "$repo_dir" pull --ff-only
    fi
  else
    log "cloning android-shizuku-mcp"
    run git clone "$ANDROID_MCP_REPO" "$repo_dir"
  fi

  if (( DRY_RUN )); then
    printf '+ cd %q && ./install.sh\n' "$repo_dir"
  else
    (
      cd "$repo_dir"
      chmod +x install.sh run-server.sh 2>/dev/null || true
      ./install.sh
    )
  fi

  if (( SETUP_BOOT )); then
    if (( DRY_RUN )); then
      printf '+ cd %q && ./setup_boot.sh\n' "$repo_dir"
    else
      (
        cd "$repo_dir"
        chmod +x setup_boot.sh 2>/dev/null || true
        ./setup_boot.sh
      )
    fi
  fi
}

check_rish() {
  local candidates=(
    "$HOME/bin/rish"
    "$PREFIX/bin/rish"
  )

  local found=0
  for p in "${candidates[@]}"; do
    if [[ -x "$p" ]]; then
      found=1
      log "rish found: $p"
      if (( ! DRY_RUN )); then
        chmod go-w "$p" 2>/dev/null || true
      fi
      break
    fi
  done

  if (( ! found )); then
    warn "rish not found."
    warn "In Shizuku, export/copy rish to ~/bin/rish, then:"
    warn "  chmod 700 ~/bin/rish"
  fi
}

termux_next_steps() {
  cat <<EOF

Termux bootstrap complete.

Manual Android steps still required:
  1. Install the Termux:API Android app from the same trusted distribution
     family as Termux, then grant only permissions you need.

  2. Install/start Shizuku.
     Android 11+:
       Developer options -> Wireless debugging
       -> pair Shizuku -> start Shizuku service.

  3. Copy Shizuku 'rish' to:
       ~/bin/rish
     then:
       chmod 700 ~/bin/rish

  4. android-shizuku-mcp:
       ${JARVIS_ROOT}/repos/shizuku-mcp/run-server.sh

     Default upstream endpoint:
       http://127.0.0.1:8765/mcp

     The upstream installer creates a Bearer token in its .env.
     Do not expose this endpoint directly to the public Internet.

  5. Test basic phone telemetry:
       ${JARVIS_ROOT}/bin/phone-status-json

EOF
}

setup_termux() {
  is_termux || die "termux target must be run inside Termux"

  termux_install_packages
  create_termux_layout
  install_android_mcp
  check_rish
  termux_next_steps
}

case "$TARGET" in
  server)
    setup_server
    ;;
  termux)
    setup_termux
    ;;
  *)
    die "invalid target: $TARGET (expected server or termux)"
    ;;
esac
