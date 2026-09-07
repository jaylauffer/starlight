#!/usr/bin/env bash
set -euo pipefail

FB="$("$(dirname "$0")/scripts/sense-hat-fb.sh")"

sudo \
  STARLIGHT_SIGNAL_SOCKET=/tmp/starlight-thermal.sock \
  STARLIGHT_SIGNAL_SOCKET_OWNER=jay \
  ./target/release/starlight wlan0 "$FB" 2>&1 | tee /tmp/starlight.log
