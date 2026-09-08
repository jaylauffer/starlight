# Thermal Behaviour

Written 2026-09-08, after agnes flashed thermal warnings while GitHub
Actions build jobs were running on the same board, and the reasonable
question was asked: **is the warning display itself making the heat
worse?**

Short answer: **no — the warning state does less CPU work than the normal
state, not more.** The detail is worth keeping because the intuition
("flashing an LED costs something, so warning about heat adds heat") is
sound in general and simply happens to be wrong here.

## What each state actually does

| state | per captured packet | matrix updates |
| --- | --- | --- |
| normal | `packet_to_frame_bytes` (compress + convert), then a framebuffer write, gated by `claim_frame_slot` | one per admitted packet |
| warning | capture only — the conversion and write are **skipped** | one pulse per 350 ms |
| critical | capture stops entirely (`on_packet` returns early) | pulse until `CRITICAL_PULSES_BEFORE_EXIT`, then stop |

The relevant branch is in `on_packet`:

```rust
if self.thermal_state.load(Ordering::Acquire) == THERMAL_STATE_NORMAL
    && self.claim_frame_slot()
{
    let frame = packet_to_frame_bytes(&self.compressor, &raw);
    self.write_frame(frame.to_vec());
}
// Warm/warning states keep capturing but leave the matrix
// to the pulse timer.
```

So entering `warning` **removes** the per-packet compression and
framebuffer write from the hot path and replaces them with a fixed
~2.9 Hz pulse. `PULSE_INTERVAL` is 350 ms, and each pulse writes
`FRAMEBUFFER_SIZE_BYTES` = `COMPRESSED_PACKET_SIZE * size_of::<f32>()` =
32 × 4 = **128 bytes** through the existing io_uring submission path,
plus one small `Vec` allocation.

Three ~128-byte writes per second cannot meaningfully move a Pi 4's die
temperature. The heat comes from whatever is saturating the four cores —
on agnes, that is a `cargo build`, not the LED matrix.

## What this does *not* claim

- It does not claim starlight is thermally free. Packet capture continues
  in `warning`; only `critical` stops it.
- `thermal_recommendation(WARNING)` returns `"throttle"`, but that is a
  recommendation **published to clients**. starlight does not reduce its
  own capture rate at `warning` — it only stops converting frames. If
  self-throttling at `warning` is ever wanted, that is a real change, not
  something the current code already does.
- The measurement here is a code reading plus arithmetic, not a profile.
  If this ever needs to be settled empirically, compare
  `/sys/class/thermal/thermal_zone0/temp` slopes with starlight stopped
  versus running under an identical build load; the difference should be
  within noise.

## Context: agnes is both a sensor host and a CI runner

agnes runs starlight against the Sense HAT *and* carries four GitHub
Actions runners. A build saturating four cores will heat the board past
the warning threshold on its own. Observed 2026-09-08 during a release
build: `vcgencmd get_throttled` returned `0x80000` — bit 19, "soft
temperature limit has occurred" — with no bits 0–3 set, i.e. the limit
had been hit at some point but the board was not throttling when
sampled (67.2 °C at low load).

If the two workloads need to stop competing, the lever is scheduling —
not the warning display, and not narrowing which machines may run a job.
Both boards deliberately remain eligible for every workflow they are
capable of, because a spare runner that is merely slower is worth more
than a marginally faster average build (see
`~/pudding/pi-github-runner-setup.md`).

## Thresholds

Defaults are `STARLIGHT_WARN_TEMP_C=82` and `STARLIGHT_CRIT_TEMP_C=85`.

Warning was raised from 80 to 82 on 2026-09-08. A Pi 4 saturating four
cores on a `cargo build` sits in the low 80s as a matter of course, so a
threshold of 80 fired on ordinary work rather than on trouble, and a
warning that fires routinely stops being read.

Critical stays at **85**, and should not follow it upward: that is at the
BCM2711's rated maximum, so there is no headroom to spend. Raising the
warning narrows the gap between "something to notice" and "stop now",
which is the real cost of this change — 3 °C of lead time at whatever
rate the board is climbing. If warnings still turn out to be noise, the
better next move is cooling or scheduling, not another 2 °C.
