#!/usr/bin/env bash
# Clears the Sense HAT LED matrix.
#
# A framebuffer keeps its last frame after the writing process exits --
# nothing in the kernel blanks it. So a stopped or crashed `starlight`
# leaves whatever it drew last sitting lit on the HAT indefinitely, which
# is indistinguishable from a live process that has hung. That misread
# cost real debugging time on 2026-09-08.
#
# Used by starlight.service's ExecStopPost (which also covers SIGKILL and
# crashes, unlike an in-process signal handler) and by the launch scripts'
# exit trap.
set -euo pipefail

FB="$("$(dirname "$0")/sense-hat-fb.sh")"
# 8x8 at 16bpp = 128 bytes.
dd if=/dev/zero of="$FB" bs=128 count=1 status=none
