#!/usr/bin/env bash
set -euo pipefail

FB="$("$(dirname "$0")/scripts/sense-hat-fb.sh")"

# The matrix keeps its last frame after we exit; clear it on the way out.
trap '"$(dirname "$0")/scripts/blank-sense-hat.sh" || true' EXIT

sudo \
  STARLIGHT_SIGNAL_SOCKET=/tmp/starlight-thermal.sock \
  STARLIGHT_SIGNAL_SOCKET_OWNER=jay \
  ./target/release/starlight wlan0 "$FB"
