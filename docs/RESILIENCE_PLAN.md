# Starlight Resilience Plan

## Purpose

`starlight` runs on a Raspberry Pi 4 with constrained thermal headroom and a
hardware-facing output path. It should survive normal warm operation, recover
cleanly from crashes, and provide a stable Unix socket surface for local
supervision.

This note defines the operational target before additional implementation work.

## Current State

As documented in [README.md](../README.md), `starlight` already has:

- packet capture from a selected network interface
- Sense HAT framebuffer output
- thermal monitoring from `/sys/class/thermal/thermal_zone*/temp`
- configurable warning and critical thresholds
- an optional Unix domain socket for newline-delimited JSON thermal updates

That is useful, but not yet resilient enough for unattended operation.

## Failure Model

The main risks on a Pi 4 are:

- process crash while the board is still healthy
- process exit after transient resource pressure
- hot-but-not-fatal operation causing bad behavior before actual thermal
  throttling or shutdown
- stale clients holding on to a dead Unix socket path
- partial startup leaving the process unavailable until a human intervenes

The target is not "never die." The target is:

- fail predictably
- restart automatically
- shed work before collapse
- preserve a stable control path for observers

## Operating Bands

Use three thermal bands rather than treating every warm condition as fatal.

### Normal

- below `75 C`
- packet capture and display run normally
- normal update cadence

### Warm

- `75 C` to `82 C`
- continue running
- reduce background or optional work
- reduce control-message frequency on the Unix socket if needed
- prefer simpler visual updates over expensive transforms

### Critical

- above `82 C`
- stop nonessential work
- preserve only thermal reporting and minimal heartbeat behavior
- if temperature continues to rise or internal errors accumulate, exit cleanly
  and rely on the supervisor to restart later

The current README defaults of warn `80 C` and stop `85 C` are still useful,
but the implementation should move toward these operating bands so the process
degrades instead of falling off a cliff.

## Supervisor Model

`starlight` should be treated as a supervised service, not a manually launched
foreground process.

Recommended `systemd` behavior:

- `Restart=on-failure`
- `RestartSec=2`
- moderate start-limit protection to avoid restart storms
- persistent journald logging
- `WatchdogSec=` once the main loop can emit regular heartbeats

The process should exit nonzero on:

- unrecoverable framebuffer initialization failure
- unrecoverable packet-capture initialization failure
- internal consistency failure

It should not exit just because the board is warm.

## Unix Socket Resilience

The Unix socket is the most useful local control and observation path, so its
lifecycle needs to be explicit.

Requirements:

- fixed documented socket path
- stale socket cleanup on startup
- bind failure should produce a clear log message
- clients must expect reconnects
- messages should include enough state to determine whether the process is:
  - starting
  - normal
  - warm
  - critical
  - shutting down

Recommended direction:

- keep the current newline-delimited JSON transport
- expand it from thermal-only messages to a small status stream
- include:
  - `state`
  - `temp_c`
  - `warn_c`
  - `crit_c`
  - `uptime_s`
  - `restart_epoch`
  - `capture_enabled`
  - `framebuffer_enabled`
  - `ts`
  - `recommendation`

Example:

```json
{"state":"warm","temp_c":79.4,"warn_c":80.0,"crit_c":85.0,"uptime_s":1432,"restart_epoch":3,"capture_enabled":true,"framebuffer_enabled":true,"ts":1710000000,"recommendation":"throttle"}
```

Longer-term, this socket can also expose one-shot commands such as:

- `status`
- `pause-capture`
- `resume-capture`
- `set-thresholds`

But read-only status should come first.

## Crash Recovery Expectations

On restart, `starlight` should:

1. clean up any stale socket path
2. rebind the Unix socket
3. reinitialize thermal monitoring
4. attempt packet capture setup
5. attempt framebuffer setup
6. publish a startup status event

If packet capture fails but the socket and thermal monitor are healthy, consider
remaining alive in degraded mode so operators can still inspect state instead of
losing the entire process.

If framebuffer output fails but capture works, the same logic applies: prefer a
degraded, observable state over immediate silent death.

## Logging And Diagnostics

The process should emit explicit logs for:

- startup configuration
- selected interface and framebuffer path
- thermal band transitions
- socket bind/unbind events
- capture start/stop events
- degraded-mode transitions
- panic or fatal-exit reasons

At minimum, every unexpected exit should leave a reason visible in `journalctl`.

## Backoff Policy

When the board enters the warm band:

- reduce optional work first
- avoid repeated retries in tight loops
- avoid heavy filesystem scans or expensive diagnostics

When the board enters the critical band:

- suspend nonessential output activity
- continue thermal publication if possible
- if needed, exit cleanly after a short cooldown grace period rather than
  thrashing

The goal is to avoid making the hot condition worse through recovery work.

## Implementation Order

1. Add a `systemd` service file for supervised restart.
2. Add a `systemd` socket file if socket activation is adopted.
3. Extend the Unix socket payload from thermal-only to general status.
4. Implement explicit warm-band throttling.
5. Implement degraded startup and degraded runtime states.
6. Add watchdog heartbeat support.

## Exit Criteria

`starlight` can be considered operationally resilient on Pi 4 when:

- warm operation does not cause unnecessary exits
- a crash is restarted automatically
- clients can reconnect to the Unix socket after restart
- thermal state changes are externally visible
- packet capture and framebuffer failures are reported clearly
- the process can remain observable even when one subsystem is degraded
