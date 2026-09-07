#!/usr/bin/env bash
# Headless launch: no HDMI attached, so the framebuffer index in
# particular must be resolved by name rather than assumed.
set -euo pipefail

FB="$("$(dirname "$0")/scripts/sense-hat-fb.sh")"

sudo \
  STARLIGHT_SIGNAL_SOCKET=/tmp/starlight-thermal.sock \
  STARLIGHT_SIGNAL_SOCKET_OWNER=jay \
  ./target/release/starlight wlan0 "$FB"
