# mdrv-ds — Linux DualSense driver

Bluetooth (L2CAP) DualSense proxy for Linux: input passthrough, adaptive
triggers, rumble, speaker/jack audio, and — with the bundled game/Wine
patches — **full audio-based haptic feedback over Bluetooth**, including in
games that officially restrict haptics to USB (FF16, Stellar Blade).

## What it does

| Feature                                                       | Status  |
| ------------------------------------------------------------- | ------- |
| Bluetooth proxy (self-owned L2CAP HID channels)               | Working |
| Input over BT (BT-native virtual pad, raw report passthrough) | Working |
| Adaptive triggers + rumble over BT                            | Working |
| Speaker / 3.5 mm jack audio over BT                           | Working |
| Audio-based haptics over BT (games: FF16, Stellar Blade)      | Working |
| USB cable takeover (cabled pad switches to hidraw relay)      | Working |
| Touchpad-as-mouse, PS-button chords, input gating             | Working |

## Architecture

```
real pad ──BT L2CAP(0x11 ctrl /0x13 intr)──▶ mdrv-ds proxy ──IPC──▶ holder ──uhid──▶ virtual pad
                ▲  │                        │   ▲                (owns /dev/uhid,
     features/  │  └── 0x31/0x36 outputs    │   └─ feature GET/     survives restarts)
     SET fwd    │                           │      SET serving
                │                           ▼
Wine/games ◀── kernel evdev/hidraw ◀── PipeWire capture ◀── sink writer thread
                                            (F32 48k quad)   │ Opus CBR 160k
                                                             ▼
                                             0x36 combined reports ─▶ pad
                                             (state + haptics s8 +
                                              speaker/jack Opus)
```

- **L2CAP session** (`src/l2cap.rs`): mdrv-ds binds PSM 0x11/0x13 itself
  (`bluetoothd --noplugin=input`, see `scripts/bt-input-off.sh`), fetches
  feature reports 0x09/0x20/0x05, sends the vds-parity INIT 0x32, and keeps
  the control channel open for the whole session (drained by the relay).
- **Virtual pad**: UHID device presented bus=BLUETOOTH with the plain BT HID
  descriptor (279 B). Raw BT input reports pass through untouched; the kernel
  parses them exactly like a real BT pad. (A merged USB+BT descriptor was
  tried and reverted — it broke natural per-transport classification in
  Sony's libScePad; see `DS5_HID_REPORT_DESCRIPTOR_MERGED` in l2cap.rs.)
  Feature GETs are served from the live cache (MAC rewritten to the virtual
  address, kernel-convention CRCs).
- **USB-chimera view** (`force_bus = "usb"` in config.toml): on a BT L2CAP
  DualSense session, create the virtual pad as bus=USB with the real USB
  descriptor instead, translate BT 0x31 input frames to USB 0x01 reports,
  and translate game USB outputs (48 B 0x02/0x05) and feature reports back
  to BT shapes. For games that gate adaptive triggers/haptics on "pad is
  Bluetooth" (RE Engine: PRAGMATA) while keeping FF16's native-BT path
  (default) untouched. Takes effect on the next pad (re)connect.
- **Cross/circle swap** (`swap_cross_circle = true`): swaps X/O on the
  virtual pad (JP-style confirm). Applied in the relay before every
  consumer — game, kernel, chords — so games visibly respond to the
  swapped button; prompt glyphs are not redrawn. Chimera sessions,
  default off.
- **Music forwarding** (`audio.speaker_output = "forward"`, optional
  `audio.speaker_target = "<node.name>"`): plays the pad-stream's music
  channels on the normal system output while haptics keep riding the pad
  link (rerouting the game's stream in pavucontrol breaks rumble).
  `"pad"` (default) keeps the pad speaker; `"mute"` drops the music.
- **Sink** (`src/sink.rs`): captures the PipeWire stream on our impersonated
  endpoint (F32 48k quad), gates silence, encodes speaker audio as Opus CBR,
  packs RL/RR as s8 haptics into 398 B 0x36 reports, and overlays kernel
  output state so rumble/triggers ride the same reports. Idle state falls
  back to plain 0x31 relays (vds parity).
- **Holder** (`src/holder.rs`): owns `/dev/uhid`; the virtual pad survives
  proxy restarts and transport switches.

Reference implementation for the BT protocol: [vds](https://github.com/hhao14/vds).

## Files

| File                        | Purpose                                                         |
| --------------------------- | --------------------------------------------------------------- |
| `src/proxy.rs`              | Session loop, input/output/feature relay, chords, mouse, gating |
| `src/l2cap.rs`              | L2CAP listeners/session, HID descriptors, feature cache         |
| `src/sink.rs`               | PipeWire capture, ring buffer, 0x36 writer, kernel-output merge |
| `src/audio.rs`              | Report builders (0x31/0x32/0x35/0x36), CRC                      |
| `src/holder.rs`             | uhid daemon keeping the virtual pad alive                       |
| `src/ipc.rs`                | proxy↔holder framing protocol                                   |
| `src/uhid.rs`               | uhid event codec                                                |
| `src/hid.rs`                | Pad discovery, feature lengths                                  |
| `scripts/ds5-haptics-patch` | Idempotent binary patcher (games + GE-Proton), see below        |
| `scripts/bt-input-off.sh`   | bluetoothd `--noplugin=input` override install                  |

## Build & run

```bash
make release        # cargo build --release + setcap cap_net_bind_service,cap_net_raw
systemctl --user restart mdrv-ds-holder.service mdrv-ds.service
```

Requires: Rust, libpipewire, bluez with `bluetoothd --noplugin=input`
(installed by `scripts/bt-input-off.sh`). Config in
`~/.config/mdrv-ds/config.toml` (see `config/config.toml`).

## Game patches: DualSense audio-haptics over Bluetooth

Windows games gate PS5 audio-haptics (the quad-channel F32/48k stream whose
RL/RR channels carry haptic waveforms) on the controller looking like a USB
DualSense. Over plain Bluetooth they never open that stream. mdrv-ds already
delivers the audio path itself (L2CAP 0x36 reports to speaker + haptics
actuators); the patches make the _games_ use it.

`scripts/ds5-haptics-patch` is idempotent, auto-backs up to
`<target>.hapticsbak`, detects game updates via byte signatures, and supports
`status` / `patch` / `restore`. Four profiles:

```bash
scripts/ds5-haptics-patch status              # everything
scripts/ds5-haptics-patch patch ff16          # game: ffxvi.exe
scripts/ds5-haptics-patch patch stellarblade  # game: libScePad.dll
scripts/ds5-haptics-patch patch ge-winepulse ge-winebus  # GE-Proton libs
```

- **ff16** — NOPs four conditional jumps guarding the audio engine's
  endpoint-shopper Init (`0x141041135/48/59/7a`). The shopper normally only
  re-runs when the WASAPI endpoint count changes after launch, which never
  happens over BT; patched, it always runs and finds our sink
  ("Speakers (DualSense Wireless Controller)") and opens the quad stream.
- **stellarblade** — libScePad.dll: classification is left natural (the
  BT-shaped pad classifies as Bluetooth, so reports parse correctly on BT and
  as USB when cabled); the patch only NOPs the `bus == USB` check in
  `scePadIsSupportedAudioFunction()` (`0x18000ae30`) so BT-classified pads
  get haptics/speaker. pid validation is kept.
- **ge-winepulse** — `get_container_id()` stubbed to return a constant
  container GUID `{0CE6054C-0000-FFFF-C0FF-EE0CE6054C00}` for every endpoint
  (virtual endpoints have no udev usb_device parent, so Wine derived
  GUID_NULL and any container-ID matching fails).
- **ge-winebus** — pad stamped with the same constant GUID every reconnect;
  `BTHENUM` ancestry string → `USB` so games walking `CM_Get_Parent` see a
  USB parent. Haptics were verified working _with all three GE patches
  installed_; they were never isolated one-by-one — do not selectively revert
  without retesting.

Re-apply after game or GE-Proton updates (`status` shows MISMATCH when bytes
change). Pristine backups also live in `~/.local/state/mdrv-ds/debug/`.

## Test infrastructure

```bash
# Quad-channel test tone (440 Hz FL/FR, 45 Hz RL/RR)
python3 -c "
import struct, math
out = bytearray()
for i in range(20*480):
    t = i/48000
    out += struct.pack('<4f',
        0.4*math.sin(2*math.pi*440*t), 0.4*math.sin(2*math.pi*440*t),
        0.9*math.sin(2*math.pi*45*t),  0.9*math.sin(2*math.pi*45*t))
open('/tmp/quad.f32','wb').write(bytes(out))
"
pw-play --raw --format f32 --rate 48000 --channels 4 \
  --channel-map FL,FR,RL,RR --target mdrv-ds.dualsense-bt /tmp/quad.f32
```

## References

- [vds](https://github.com/hhao14/vds) — reference BT-protocol implementation
- [dsneo SPEC](https://github.com/forgerpl/dualsense-neo/blob/main/SPEC.md) —
  DualSense HID protocol documentation
- [DualSense wiki](https://controllers.fandom.com/wiki/Sony_DualSense) —
  report formats, feature reports, BT rdesc

## License

TBD.

## Manual test guide

Expected behaviors, verified end-to-end. Deploy first:

```bash
make release   # or: cargo build --release &&
sudo setcap cap_net_bind_service,cap_sys_ptrace+ep target/release/mdrv-ds
systemctl --user restart mdrv-ds.service   # pad needs a PS press to reconnect
```

Debug helpers: `MDRV_DUMP_INPUT=<file>` (raw BT input frames),
`MDRV_DS_DEBUG=1`; journals: `journalctl --user -u mdrv-ds.service`.

### Connection & input

1. Press PS on the pad → connects via the proxy (journal: session + INIT);
   the virtual pad appears (`/dev/input` "Wireless Controller", BT bus).
   Games/`evtest` see full input: buttons, sticks, touchpad, adaptive
   triggers, rumble.
2. Restart `mdrv-ds.service` mid-session → virtual pad SURVIVES (holder owns
   uhid); press PS to re-link the pad; input continues seamlessly.
3. Plug a USB cable → the pad is taken over via hidraw relay (no BT
   session); unplug → BT resumes on PS press.

### Audio & jack routing

4. Play anything into the `mdrv-ds.dualsense-bt` sink (it is the default) →
   audio from the pad's speaker; mdrv-ds journal shows `primed`.
5. **Plug a headset into the 3.5 mm jack** → within ~1 s audio moves to the
   headset, the speaker goes SILENT, journal logs `jack engage → headset`,
   and a "Headset connected" toast fires.
6. Flutter immunity: with the headset plugged, audio must stay on the jack
   indefinitely (the HP-detect bit flutters at ~10 Hz; the integrator
   debouncer — +1/high, −4/low, arm at 16, disarm after 800 ms with zero
   highs — must never flip routing on noise).
7. **Unplug** → after ≤1 s audio returns to the speaker and STAYS there
   (spurious one-off high readings after unplug must not re-engage the
   headset; the 800 ms zero-high disarm holds). "Headset disconnected"
   toast fires.
8. Haptics: in a patched game (FF16/Stellar Blade), effects ride audio —
   triggers/rumble work over BT while music plays (see game patches below).

### Housekeeping

9. Idle keep-alive: leave the pad idle (no audio) for 10+ min → the pad must
   NOT power off (writer sends a zero-flag 0x31 every 2 s).
10. `mdrv-ds mouse on|off|toggle|status`: on = touchpad moves the desktop
    cursor (no holder); off = holder grabs the virtual pad (games get raw
    touchpad). `status` agrees with the settings overlay row.
11. Chords from `config.toml` (e.g. PS+R2 → audio overlay toggle) fire the
    bound command exactly once per press.

### Known-good notes

- USB DualSense does NOT appear in pavucontrol as an audio device — audio
  is L2CAP-only by design (USB isochronous audio unimplemented).
- `setcap` needs BOTH capabilities above after every rebuild; a missing
  `cap_sys_ptrace` shows up as EACCES binding PSM 0x11/0x13.
