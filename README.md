# mdrv-ds — Linux DualSense driver

PipeWire-based Bluetooth audio sink, haptic relay, input remapper, and
chord engine for the Sony DualSense (PS5) controller on Linux.

## What it does

| Feature                                                     | Status                                                  |
| ----------------------------------------------------------- | ------------------------------------------------------- |
| **Bluetooth audio sink** (Opus over HID output reports)     | Partial — audio plays but ~1 s stutter persists         |
| **Haptic feedback relay** (kernel FF → BT 0x31 reports)     | Working — rumble reaches pad, micro-stutters at ~100 ms |
| **Input remapping** (evdev → uinput, with PS-button chords) | Working                                                 |
| **Touchpad-as-mouse**                                       | Working                                                 |
| **USB passthrough** (uhid virtual device)                   | Working                                                 |
| **Audio via 3.5 mm jack**                                   | Confirmed audible, but same ~1 s stutter                |

## Architecture

```
┌────────────┐     ┌────────────┐     ┌──────────┐     ┌─────────┐
│ PipeWire   │────▶│ mdrv-ds    │────▶│ Opus     │────▶│ hidraw  │──▶ BT
│ (source)   │     │ ring buf   │     │ encoder  │     │ 0x35/39 │
└────────────┘     └────────────┘     └──────────┘     └─────────┘
                         ▲                                    │
                         │          ┌────────────┐            │
                         └──────────│ kernel FF  │◀───────────┘
                                    │ (evdev)    │
                                    └────────────┘
```

**Pipeline**: PipeWire delivers 48 kHz f32 quad (FL/FR/RL/RR) audio to a
custom sink. The sink resamples to 45 kHz mono/stereo i16, encodes with
libopus (CBR 160 kbps), frames into HID output reports (0x35 or 0x39),
and writes to `/dev/hidrawN` at the pad's polling interval (~93 Hz).

**Haptic relay**: Kernel force-feedback effects are captured from evdev,
translated to DualSense 0x31 state reports, and written to hidraw.
They share the same ACL channel as audio.

## Files

| File                 | Purpose                                                             |
| -------------------- | ------------------------------------------------------------------- |
| `src/sink.rs`        | PipeWire capture thread, ring buffer, writer thread, state tracking |
| `src/audio.rs`       | Opus encoder init, 0x35/0x39 report builders, CRC                   |
| `src/hid.rs`         | HID output report writer, reader, feature reports                   |
| `src/proxy.rs`       | evdev → uinput input relay, chord engine, mouse emulation           |
| `src/config.rs`      | Configuration deserialization                                       |
| `src/main.rs`        | CLI, systemd integration, IPC server                                |
| `config/config.toml` | Default configuration (copy to `~/.config/mdrv-ds/`)                |

## Configuration

Key `[audio]` options (in `~/.config/mdrv-ds/config.toml`):

```toml
[audio]
output = true # enable BT audio sink
bitrate = 160000 # Opus CBR bps
# report = "0x35"   # HID report ID (0x35 default; 0x39 for 547B ladder)
# interval_us = 10667  # writer pacing in µs (default 10667 ≈ 93.7 Hz)
```

## Build & run

```bash
cargo build --release
systemctl --user start mdrv-ds   # or: mdrv-ds proxy --daemon
```

Requires: PipeWire dev libraries (`libpipewire-0.3-dev`), Rust ≥1.70.

## Known issues

### 1. ~1-second audio stutter (BLOCKING)

**The primary open problem.** Audio plays through the pad's speaker (and
3.5 mm jack when connected) but stutters with a ~1 s period.

**What we know:**

- Host-side stats are clean: 0 underruns, 0 overflows, ring fill stable at
  3–5 frames. Peak instrumentation proves real audio flows from PipeWire
  through the ring, resampler, and encoder (`peak in 0.900 pcm 0.400` with
  a 440 Hz test tone).
- The stutter persists across report IDs (0x35 vs 0x39), interval tuning
  (10667 vs 10669 µs), and dual-frame batching attempts.
- The Opus stream is spec-compliant: correct TLV framing, advancing seq,
  counter, and CRC. The pad accepts the reports without error.
- The host-side encoding pipeline is healthy — the problem is pad-side.

**Hypotheses (not confirmed):**

- **Pacing mismatch**: Our fixed 10667 µs interval doesn't match this
  pad's crystal tolerance (dsneo measured ±200 ppm per unit). The pad's
  internal audio renderer buffer over/underflows when the gap drifts.
- **ACL contention**: The 478 Hz input report traffic (buttons, sticks,
  gyro) shares the Bluetooth ACL channel with our 94 reports/s output.
  Scheduling jitter causes burst-then-gap patterns at ~1 s.
- **Pad-side audio renderer state**: After hours of experimental report
  streams (VBR garbage, 0x36 floods, mask experiments), the renderer may
  be wedged. A full power-cycle (hold PS ~10 s) may help but hasn't been
  confirmed as a fix.

**What hasn't worked:**

- Switching between 0x35 and 0x39 report IDs
- Interval tuning: 10667 → 10669 µs
- Dual-frame 0x39 batching (ghost button presses; pad misread the layout)
- Ring buffer sizing changes
- VBR → CBR Opus (fixed a separate decoder bug, not this stutter)

### 2. Rumble micro-stutter (~100 ms)

Haptic feedback reaches the pad and is continuous ("finally continuous"
per user testing) but exhibits micro-stutters at ~100 ms period. This is
separate from the audio stutter. Likely related to the same ACL scheduling
or pad-side renderer timing.

### 3. DualSense audio renderer "wedging"

After extended testing sessions with various experimental report formats
(VBR, wrong masks, 0x36 floods), the pad's internal audio renderer may
enter a state where it stops producing audible output even with
spec-correct input. This is suspected but not confirmed — a full pad
power-cycle (not just BT reconnect) has not been verified as a fix.

## Test infrastructure

```bash
# Generate a quad-channel test tone (440 Hz FL/FR, 45 Hz RL/RR)
python3 -c "
import struct, math
frames = 20*480
out = bytearray()
for i in range(frames):
    t = i/48000
    fl = fr = 0.4*math.sin(2*math.pi*440*t)
    rl = rr = 0.9*math.sin(2*math.pi*45*t)
    out += struct.pack('<4f', fl, fr, rl, rr)
open('/tmp/quad.f32','wb').write(bytes(out))
"

# Play through the BT sink
pw-play --raw --format f32 --rate 48000 --channels 4 \
  --channel-map FL,FR,RL,RR \
  --target mdrv-ds.dualsense-bt \
  /tmp/quad.f32
```

Peak instrumentation in the logs confirms audio health:

```
sink: stats fill 3 (min 2 max 4), underruns +0 (0 total),
       overflows +0 (0 total), peak in 0.900 pcm 0.400
```

## References

- [dsneo spec](https://github.com/forgerpl/dualsense-neo/blob/main/SPEC.md) —
  DualSense HID protocol documentation
- [DualSense BLE HID spec](https://controllers.fandom.com/wiki/Sony_DualSense) —
  Input/output report formats
- [vds (Virtual DualSense)](https://github.com/hhao14/vds) —
  Reference implementation using direct L2CAP (proves audio is possible)

## License

TBD.
