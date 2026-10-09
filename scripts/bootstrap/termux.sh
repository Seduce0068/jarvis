#!/usr/bin/env bash
# JARVIS Termux Bootstrap — Android Termux
# Termux:API + android-shizuku-mcp scaffold

set -Eeuo pipefail
IFS=$'\n\t'
umask 077

SCRIPT_NAME="${0##*/}"
DRY_RUN=0
INSTALL_ANDROID_MCP=1
SETUP_BOOT=0
JARVIS_ROOT="${JARVIS_ROOT:-$HOME/jarvis}"
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
  ./termux.sh [options]

Options:
  --dry-run              実行せずコマンドだけ表示
  --skip-android-mcp     android-shizuku-mcpをインストールしない
  --setup-boot           リポジトリの setup_boot.sh も実行
  --root DIR             作業ディレクトリを変更 (既定: ~/jarvis)
  -h, --help             ヘルプ

Examples:
  chmod +x scripts/bootstrap/termux.sh
  ./scripts/bootstrap/termux.sh
  ./scripts/bootstrap/termux.sh --setup-boot
EOF
}

while (($#)); do
  case "$1" in
    --dry-run)           DRY_RUN=1; shift ;;
    --skip-android-mcp)  INSTALL_ANDROID_MCP=0; shift ;;
    --setup-boot)        SETUP_BOOT=1; shift ;;
    --root)              [[ $# -ge 2 ]] || die "--root requires an argument"; JARVIS_ROOT="$2"; shift 2 ;;
    -h|--help)           usage; exit 0 ;;
    *)                   die "unknown option: $1" ;;
  esac
done

is_termux() {
  [[ -n "${TERMUX_VERSION:-}" ]] ||
  [[ "${PREFIX:-}" == *"com.termux"* ]] ||
  [[ -d "/data/data/com.termux/files/usr" ]]
}

is_termux || die "termux target must be run inside Termux"

termux_install_packages() {
  command -v pkg >/dev/null 2>&1 || die "Termux pkg command not found"
  log "updating Termux packages"
  run pkg update -y
  run pkg install -y git curl jq openssh python termux-api
}

create_termux_layout() {
  log "creating mobile layout under $JARVIS_ROOT"
  run mkdir -p "$JARVIS_ROOT"/{bin,config,repos}

  if (( ! DRY_RUN )); then
    # phone-status-json
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

    # mobile.env
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
  (( INSTALL_ANDROID_MCP )) || { log "skipping android-shizuku-mcp install"; return; }

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
  local candidates=("$HOME/bin/rish" "$PREFIX/bin/rish")
  local found=0
  for p in "${candidates[@]}"; do
    if [[ -x "$p" ]]; then
      found=1
      log "rish found: $p"
      if (( ! DRY_RUN )); then chmod go-w "$p" 2>/dev/null || true; fi
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

main() {
  termux_install_packages
  create_termux_layout
  install_android_mcp
  check_rish
  termux_next_steps
}

main "$@"