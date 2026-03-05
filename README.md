# Starlight on Raspberry Pi + Sense HAT

This crate reads packets from a network interface and streams a compressed payload to the
Sense HAT framebuffer.

## Requirements

- Raspberry Pi with Sense HAT attached and working under Raspberry Pi OS.
- Rust toolchain on the Pi (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`).
- Capturing on a network interface (`CAP_NET_RAW`), which typically requires root unless the
  binary is granted capabilities.
- Sense HAT framebuffer exposed as `/dev/fb1` (common default for Sense HAT).

## Prepare Sense HAT framebuffer

1. Enable the Sense HAT overlay in `/boot/config.txt` (or use Raspberry Pi config tools):
   - ensure `dtoverlay=rpi-sense`.
2. Reboot.
3. Verify framebuffer devices:

```bash
ls /dev/fb*
```

You should see `/dev/fb1` for Sense HAT.

## Build

From the repo root:

```bash
cargo build --release --manifest-path starlight/Cargo.toml
```

## Launch on Raspberry Pi

`starlight` expects two positional args:

1. network interface name
2. framebuffer path

Example launch:

```bash
cd /home/jay/pudding
sudo cargo run --manifest-path starlight/Cargo.toml --release -- eth0 /dev/fb1
```

If you prefer to avoid `sudo` for the long-running process:

```bash
sudo chown root:root starlight/target/release/starlight
sudo chmod u+s starlight/target/release/starlight
sudo setcap cap_net_raw,cap_net_admin+eip starlight/target/release/starlight
```

Then run:

```bash
starlight/target/release/starlight eth0 /dev/fb1
```

Use your actual interface name (`ip link show`) if it is not `eth0`.

## CPU temperature monitoring (Sense HAT airflow safety)

The Raspberry Pi can overheat quickly with the HAT in place, so `starlight` includes a
background monitor for `/sys/class/thermal/thermal_zone*/temp`.

Defaults:

- Warn at `80.0`°C (`STARLIGHT_WARN_TEMP_C`)
- Stop at `85.0`°C (`STARLIGHT_CRIT_TEMP_C`)
- Check interval `5` seconds (`STARLIGHT_TEMP_CHECK_INTERVAL_SECS`)
- Optional Unix datagram signal socket (`STARLIGHT_SIGNAL_SOCKET`)

When warning/critical thresholds are reached, Starlight overrides the network visualization and pulses the full 8x8 grid:

- Warning: yellow
- Critical: red (and capture stops)

All values are optional and can be overridden at launch time:

```bash
STARLIGHT_WARN_TEMP_C=78 STARLIGHT_CRIT_TEMP_C=84 STARLIGHT_TEMP_CHECK_INTERVAL_SECS=2 \
sudo cargo run --manifest-path starlight/Cargo.toml --release -- eth0 /dev/fb1
```

If `STARLIGHT_SIGNAL_SOCKET` is set, Starlight publishes JSON thermal status messages to that
Unix datagram socket on state changes and once per monitor interval:

```json
{"state":"normal","temp_c":63.2,"warn_c":80.0,"crit_c":85.0,"ts":1710000000,"recommendation":"normal"}
```

Recommendation values:

- `normal`: run normally
- `throttle`: reduce command intensity/cadence
- `pause`: pause non-essential work

Example receiver:

```bash
socat -u UNIX-RECV:/tmp/starlight-thermal.sock -
```

Then launch Starlight with signaling enabled:

```bash
STARLIGHT_SIGNAL_SOCKET=/tmp/starlight-thermal.sock \
sudo cargo run --manifest-path starlight/Cargo.toml --release -- eth0 /dev/fb1
```

Useful on-console checks:

```bash
cat /sys/class/thermal/thermal_zone0/temp
vcgencmd measure_temp
```

## Notes

- The current program writes directly to the framebuffer; if `/dev/fb1` is missing or a
  different device, confirm the kernel overlay and Sense HAT connection.
- On successful start you should see immediate LED activity and then live packet-driven updates.
