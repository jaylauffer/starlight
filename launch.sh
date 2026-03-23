#!/usr/bin/env bash
set -euo pipefail

sudo \
  STARLIGHT_SIGNAL_SOCKET=/tmp/starlight-thermal.sock \
  STARLIGHT_SIGNAL_SOCKET_OWNER=jay \
  ./target/release/starlight wlan0 /dev/fb1
