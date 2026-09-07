#!/usr/bin/env bash
# Prints the Sense HAT LED matrix framebuffer device path.
#
# Resolves by name rather than by index, because the index is not stable:
# it moves with HDMI state and kernel/overlay changes. On agnes the matrix
# is /dev/fb0 even though older notes (and this repo's own launch scripts,
# until now) assumed /dev/fb1 -- and passing a nonexistent path makes
# starlight exit with "No such file or directory" only after it has already
# set up its thermal socket, which reads like a permissions problem rather
# than a wrong path.
set -euo pipefail

SENSE_HAT_FB_NAME="RPi-Sense FB"

for entry in /sys/class/graphics/fb*; do
  [ -r "$entry/name" ] || continue
  if [ "$(cat "$entry/name")" = "$SENSE_HAT_FB_NAME" ]; then
    echo "/dev/$(basename "$entry")"
    exit 0
  fi
done

echo "error: no framebuffer reporting '$SENSE_HAT_FB_NAME' found." >&2
echo "Check the rpi-sense overlay is loaded (lsmod | grep rpisense_fb) and that" >&2
echo "the HAT is seated. Devices present:" >&2
for entry in /sys/class/graphics/fb*; do
  [ -r "$entry/name" ] || continue
  echo "  $entry -> $(cat "$entry/name")" >&2
done
exit 1
