# Starlight on Raspberry Pi + Sense HAT

`starlight` is a Raspberry Pi display process that watches a network interface,
compresses packet data into a compact numeric frame, and renders the result on
the Sense HAT 8x8 LED matrix through the framebuffer.

It is intended for small, hardware-facing network visualization experiments on
Pi hardware rather than as a generic packet-capture daemon.

## Runtime model

Every I/O source runs on one `Proactor<IoUringPort>` from
[`loadngo-proactor`](https://github.com/jaylauffer/loadngo): packet capture is
an `IoPort::recv` on an `AF_PACKET` socket, framebuffer updates are
`IoPort::write`, the thermal interval is a proactor deferred timer, thermal
subscribers arrive via `IoPort::accept` and are fed with `IoPort::send`. The
process runs a single thread.
See [src/runtime.rs](src/runtime.rs) for the mapping and
[docs/RESILIENCE_PLAN.md](docs/RESILIENCE_PLAN.md) for the operational target.

## Requirements

- Raspberry Pi with Sense HAT attached and working under Raspberry Pi OS.
- Rust toolchain on the Pi (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`).
- Capturing on a network interface (`CAP_NET_RAW`), which typically requires root unless the
  binary is granted capabilities.
- A Linux kernel with `io_uring` available to userspace.
- The Sense HAT framebuffer device (see below).

## Prepare Sense HAT framebuffer

1. Enable the Sense HAT overlay in `/boot/config.txt` (or use Raspberry Pi config tools):
   - ensure `dtoverlay=rpi-sense`.
2. Reboot.
3. Find the framebuffer **by name, not by index**:

```bash
for f in /sys/class/graphics/fb*; do echo "$f -> $(cat "$f/name")"; done
```

The Sense HAT matrix is whichever one reports `RPi-Sense FB`. It is an 8x8,
16bpp, 128-byte device.

The index is **not stable**: it moves with HDMI state and kernel/overlay
changes, and it is commonly `/dev/fb0` rather than the `/dev/fb1` older notes
assume. Passing a path that does not exist makes `starlight` initialize its
thermal socket and then exit with `No such file or directory` before capture
ever starts, so resolve the name rather than hardcoding an index.

## Build

From the repo root:

```bash
cargo build --release
```

## Launch on Raspberry Pi

`starlight` expects two positional args:

1. network interface name
2. framebuffer path

Example launch:

```bash
cargo run --release -- eth0 "$(scripts/sense-hat-fb.sh)"
```

If you prefer to avoid `sudo` for the long-running process:

```bash
sudo chown root:root starlight/target/release/starlight
sudo chmod u+s starlight/target/release/starlight
sudo setcap cap_net_raw,cap_net_admin+eip starlight/target/release/starlight
```

Then run:

```bash
starlight/target/release/starlight eth0 "$(scripts/sense-hat-fb.sh)"
```

Use your actual interface name (`ip link show`) if it is not `eth0`.

## CPU temperature monitoring (Sense HAT airflow safety)

The Raspberry Pi can overheat quickly with the HAT in place, so `starlight` includes a
background monitor for `/sys/class/thermal/thermal_zone*/temp`.

Defaults:

- Warn at `80.0`°C (`STARLIGHT_WARN_TEMP_C`)
- Stop at `85.0`°C (`STARLIGHT_CRIT_TEMP_C`)
- Check interval `5` seconds (`STARLIGHT_TEMP_CHECK_INTERVAL_SECS`)
- Optional Unix domain signal socket (`STARLIGHT_SIGNAL_SOCKET`)
- Shortest gap between rendered frames, in ms (`STARLIGHT_MIN_FRAME_INTERVAL_MS`, default `100`; `0` disables)
- Signal socket owner user (`STARLIGHT_SIGNAL_SOCKET_OWNER`, default: current effective user)

When warning or critical thresholds are reached, `starlight` overrides the
network visualization and pulses the full 8x8 grid:

- Warning: yellow
- Critical: red (and capture stops)

All values are optional and can be overridden at launch time:

```bash
STARLIGHT_WARN_TEMP_C=78 STARLIGHT_CRIT_TEMP_C=84 STARLIGHT_TEMP_CHECK_INTERVAL_SECS=2 \
sudo cargo run --release -- eth0 "$(scripts/sense-hat-fb.sh)"
```

If `STARLIGHT_SIGNAL_SOCKET` is set, Starlight binds that Unix socket path itself and publishes
newline-delimited JSON thermal status messages to connected clients on state changes and once per
monitor interval:

```json
{"state":"normal","temp_c":63.2,"warn_c":80.0,"crit_c":85.0,"ts":1710000000,"recommendation":"normal"}
```

Recommendation values:

- `normal`: run normally
- `throttle`: reduce command intensity/cadence
- `pause`: pause non-essential work

Example receiver:

```bash
socat -u UNIX-CONNECT:/tmp/starlight-thermal.sock STDOUT
```

Then launch `starlight` with signaling enabled:

```bash
STARLIGHT_SIGNAL_SOCKET=/tmp/starlight-thermal.sock \
sudo cargo run --release -- eth0 "$(scripts/sense-hat-fb.sh)"
```

When signaling is enabled, Starlight creates the socket path and attempts to set ownership of it to
`STARLIGHT_SIGNAL_SOCKET_OWNER` (default: current effective user).

Useful on-console checks:

```bash
cat /sys/class/thermal/thermal_zone0/temp
vcgencmd measure_temp
```

## Cost, and sharing a board

Capture is promiscuous, so without a limit the projection runs for every
packet the link carries — work set by other hosts' traffic rather than by
anything starlight needs. That matters when the board also runs CI.

Measured on a Pi 4 (`agnes`):

| | per packet | one core saturated at | clean rebuild alongside |
|---|---|---|---|
| before | 76.9 ms | ~70 pkt/s | 13.0 s (vs 10.1 s idle) |
| after | 0.92 ms | ~1100 pkt/s | 10.1 s — no measurable impact |

Two changes got there. The hidden layers were narrowed so the weights
(~0.63 MB) stay resident in L2 instead of streaming 8.9 MB per packet —
worth 84x, far more than the 14x the arithmetic alone predicts, because
the old shape was bounded by memory rather than compute.
`STARLIGHT_MIN_FRAME_INTERVAL_MS` then caps how often a packet is allowed
to become a frame; an 8x8 matrix shows nothing useful above a few frames
per second, and at the 100 ms default starlight used 3% of one core under
load rather than 100%.

`packaging/starlight.service` additionally sets `CPUQuota=25%` as a
backstop. Note `Nice=` alone was measured as insufficient — it is a
scheduling weight, so a build's own parallel jobs still crowd it out.

## Notes

- The program writes directly to the framebuffer; if the device is missing, confirm the
  kernel overlay and Sense HAT connection, and re-resolve the path by name (see
  "Prepare Sense HAT framebuffer") rather than assuming an index.
- On successful start you should see immediate LED activity and then live packet-driven updates.
- Runtime hardening and recovery expectations are documented in
  [docs/RESILIENCE_PLAN.md](docs/RESILIENCE_PLAN.md).
