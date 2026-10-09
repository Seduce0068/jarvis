#!/usr/bin/env bash
# jarvis-doctor — 環境診断スクリプト（サーバー用）

set -u

JARVIS_ROOT="${JARVIS_ROOT:-$HOME/jarvis}"

printf '%-24s %s\n' "component" "status"
printf '%-24s %s\n' "------------------------" "----------------"

# OpenClaw
if command -v openclaw >/dev/null 2>&1; then
  printf '%-24s %s\n' "OpenClaw" "$(openclaw --version 2>/dev/null || echo installed)"
else
  printf '%-24s %s\n' "OpenClaw" "MISSING"
fi

# 基本コマンド
for cmd in curl git jq python3; do
  if command -v "$cmd" >/dev/null 2>&1; then
    printf '%-24s %s\n' "$cmd" "OK"
  else
    printf '%-24s %s\n' "$cmd" "MISSING"
  fi
done

# Tailscale
if command -v tailscale >/dev/null 2>&1; then
  printf '%-24s %s\n' "Tailscale" "installed"
else
  printf '%-24s %s\n' "Tailscale" "optional / not installed"
fi

# 設定ファイル
ENV_FILE="${JARVIS_ROOT}/config/jarvis.env"
if [[ -f "$ENV_FILE" ]]; then
  printf '%-24s %s\n' "jarvis.env" "present"
else
  printf '%-24s %s\n' "jarvis.env" "MISSING"
fi

# ポリシーファイル
POLICY_FILE="${JARVIS_ROOT}/policies/tool-policy.json"
if [[ -f "$POLICY_FILE" ]]; then
  printf '%-24s %s\n' "tool-policy.json" "present"
else
  printf '%-24s %s\n' "tool-policy.json" "MISSING"
fi