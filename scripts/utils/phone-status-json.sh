#!/data/data/com.termux/files/usr/bin/bash
# phone-status-json — Android端末のステータスをJSONで出力

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