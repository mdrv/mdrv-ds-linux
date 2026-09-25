//! Phase 2: a real PipeWire sink "DualSense (BT)" over the HID audio ladder.
//!
//! Graph view: a CAPTURE stream (Direction::Input) whose node carries
//! media.class=Audio/Sink — the same construction as the FT5 filter-chain's
//! effect_input.ft5 — so pavucontrol lists it under Output Devices and the
//! session manager routes playback into it.
//!
//! Data path: PipeWire (F32 @ 48 k stereo) → ring → writer thread resamples
//! 48 k→45 k (linear, persistent phase) → 480-sample frames into Opus CBR
//! → 0x35 ladder reports, paced at 10667 µs and steered by ring fill so the
//! graph clock sets the average rate (the pad exposes no consumption signal).
//!
//! Routing: the relay publishes jack-detect (BT input 0x31, byte 56 bit0 —
//! kernel dualsense_input_report.status[1], NOT the spec's byte 55) into
//! JACK_PLUGGED; plugged → 0x16 (jack), else 0x13 (speaker).
//!
//! Stop: pw_main_loop_quit is thread-safe; the raw mainloop pointer is
//! smuggled out as usize and reconstituted in Sink::stop.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::AudioConfig;

use pipewire as pw;
use pw::context::ContextRc;
use pw::core::CoreRc;
use pw::main_loop::MainLoopRc;
use pw::properties::properties;
use pw::stream::{StreamFlags, StreamRc};

use libspa::param::audio::{AudioFormat, AudioInfoRaw};
use libspa::param::ParamType;
use libspa::pod::serialize::PodSerializer;
use libspa::pod::{Object, Pod, Value};
use libspa::utils::{Direction, SpaTypes};

pub const FRAME_SAMPLES: usize = 480; // 10 ms @ 48 k label
/// Ring target fill (480-sample frames) — absorbs jitter between the graph
/// clock and the paced writer.
/// Ring target: 4 frames ≈ 40 ms of buffered audio. Kept low on purpose —
/// haptic feedback rides this stream, and every extra buffered frame is felt
/// as rumble outlasting the on-screen action.
const TARGET_FILL_FRAMES: i64 = 4;
const RING_CAP: usize = 64;

/// Diagnostics: underruns (writer starved after priming) and overflows
/// (ring hit cap, oldest frame dropped) since sink start.
static UNDERRUNS: AtomicU64 = AtomicU64::new(0);
static OVERFLOWS: AtomicU64 = AtomicU64::new(0);
// Peak |sample| seen (bit-cast to u64 bits) — distinguishes "ring healthy but
// carrying silence" (bad input path / paused source) from real audio.
static PEAK_IN: AtomicU32 = AtomicU32::new(0);
static PEAK_PCM: AtomicU32 = AtomicU32::new(0);

fn peak_store(slot: &AtomicU32, v: f32) {
    let a = v.abs();
    let cur_bits = slot.load(Ordering::Relaxed);
    let cur = f32::from_bits(cur_bits);
    if a > cur {
        slot.store(a.to_bits(), Ordering::Relaxed);
    }
}

// ---- libopus (system) ------------------------------------------------------

#[link(name = "opus")]
extern "C" {
    fn opus_encoder_create(
        fs: libc::c_int,
        channels: libc::c_int,
        application: libc::c_int,
        error: *mut libc::c_int,
    ) -> *mut libc::c_void;
    fn opus_encoder_destroy(st: *mut libc::c_void);
    fn opus_encode(
        st: *mut libc::c_void,
        pcm: *const i16,
        frame_size: libc::c_int,
        data: *mut u8,
        max_data_bytes: libc::c_int,
    ) -> libc::c_int;
    fn opus_encoder_ctl(st: *mut libc::c_void, request: libc::c_int, ...) -> libc::c_int;
}

const OPUS_APPLICATION_AUDIO: libc::c_int = 2049;
const OPUS_SET_BITRATE_REQUEST: libc::c_int = 4002;
// NB: opus 1.6 renumbered VBR ctl (4006/4007); ≤1.5 used 4004/4005.
// 4004 on 1.6 is SET_BANDWIDTH → BAD_ARG, so try 4006 first, then 4004.
const OPUS_SET_VBR_NEW: libc::c_int = 4006;
const OPUS_SET_VBR_OLD: libc::c_int = 4004;

struct Encoder(*mut libc::c_void);
unsafe impl Send for Encoder {}

impl Encoder {
    fn new(bitrate: i32) -> Option<Encoder> {
        unsafe {
            let mut err: libc::c_int = 0;
            let st = opus_encoder_create(48000, 2, OPUS_APPLICATION_AUDIO, &mut err);
            if st.is_null() {
                eprintln!("sink: opus_encoder_create failed (err {err})");
                return None;
            }
            if opus_encoder_ctl(st, OPUS_SET_BITRATE_REQUEST, bitrate) != 0 {
                eprintln!("sink: opus SET_BITRATE failed");
                return None;
            }
            if opus_encoder_ctl(st, OPUS_SET_VBR_NEW, 0) != 0
                && opus_encoder_ctl(st, OPUS_SET_VBR_OLD, 0) != 0
            {
                eprintln!("sink: opus SET_VBR(0) failed — encoder would run VBR");
                return None;
            }
            Some(Encoder(st))
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.0) };
    }
}

// ---- reports ---------------------------------------------------------------

fn crc32_seed(seed: u8, data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for b in std::iter::once(seed).chain(data.iter().copied()) {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn put_crc(report: &mut [u8]) {
    let n = report.len();
    let crc = crc32_seed(0xA2, &report[..n - 4]);
    report[n - 4..n].copy_from_slice(&crc.to_le_bytes());
}

/// USB jack engage: a full 48 B USB output report (id 0x02 + 47 B state,
/// no seq/CRC — captured kernel USB outputs' tails are zeros). The plain
/// mic-state engage left the HP amp unconfigured; this mirrors vds
/// `set_audio_out_stream_active()` on top of the proven STATE_INIT base:
/// jack path enables the headphone volume and zeroes/disables the speaker,
/// speaker path the reverse.
pub fn usb_engage_report(jack_path: bool) -> [u8; 48] {
    let mut rpt = [0u8; 48];
    rpt[0] = 0x02;
    rpt[1..48].copy_from_slice(&STATE_INIT);
    let s = &mut rpt[1..48];
    if jack_path {
        s[0] = (s[0] | 0x10) & !0x20; // HP vol on, SPEAKER vol off
        s[1] &= !0x80; // audio-control-2 off
        s[4] = 0x7F; // headphone volume
        s[5] = 0x00; // speaker muted
        s[7] = (s[7] & !0x30) | 0x00; // path: headphones
        s[37] = 0x00;
    } else {
        s[0] = (s[0] | 0x20) & !0x10; // SPEAKER vol on, HP vol off
        s[1] |= 0x80; // audio-control-2 on
        s[5] = 0x64; // speaker volume
        s[7] = (s[7] & !0x30) | 0x30; // path: speaker
        s[37] = 0x01;
    }
    rpt
}

/// Minimal USB re-assert heartbeat: only the audio-config bits — flag0
/// enables + volumes + path — so interleaved game rumble/trigger reports
/// are never stomped. Sent every 2 s by the USB relay because the pad
/// drops audio-config state when its UAC stream re-opens (alt-setting
/// change), e.g. at game launch.
pub fn usb_reassert_report(jack_path: bool) -> [u8; 48] {
    let mut rpt = [0u8; 48];
    rpt[0] = 0x02;
    let s = &mut rpt[1..48];
    if jack_path {
        s[0] = 0x80 | 0x10; // audioctl + HP volume enable
        s[4] = 0x7F; // headphone volume
        s[7] = 0x09; // path: headphones
    } else {
        s[0] = 0x80 | 0x20; // audioctl + speaker volume enable
        s[5] = 0x64; // speaker volume
        s[7] = 0x39; // path: speaker
    }
    rpt
}

/// 0x31 volume unlock (valid_flag0 bit4 = AllowHeadphoneVolume is required
/// or the jack stays silent on PC-only-paired pads).
/// vds-proven initial 47-byte `dualsense_output_report_common` state:
/// flag0 = hp+speaker+mic volume + audio-control enables, flag1 = full LED /
/// power-save / audio-control2 enables, sane volumes, preamp 1.
const STATE_INIT: [u8; 47] = {
    let mut s = [0u8; 47];
    s[0] = 0xFD; // valid_flag0: 0x10|0x20|0x40|0x80
    s[1] = 0xF7; // valid_flag1: 0x01|0x02|0x04|0x08|0x80
    s[4] = 0x7F; // headphone volume
    s[5] = 0x64; // speaker volume
    s[6] = 0x08; // mic volume
    s[7] = 0x09; // mic audio control
    s[9] = 0x0F; // power-save control
    s[37] = 0x01; // audio_control2 (preamp 1)
    s[38] = 0x07;
    s[41] = 0x02;
    s[42] = 0x01;
    s[44] = 0xFF;
    s[45] = 0xD7;
    s
};

/// Overlay our audio settings on a (kernel-supplied or initial) state block so
/// every combined report re-asserts volume/path control alongside whatever the
/// kernel drivers (rumble/LED/trigger) put there.
fn merge_audio_state(state: &mut [u8; 47], hp_volume: u8, output: bool, jack: bool) {
    state[0] |= 0x10 | 0x20 | 0x40 | 0x80; // volume + audio-control enables
    state[1] |= 0x80; // audio_control2 enable
                      // When physical output is gated (experimental off) volumes are zeroed so
                      // speaker/jack stay silent — haptic frames still ride the 0x13 TLV and
                      // drive the grips via the pad-side renderer.
    state[4] = if output { hp_volume.min(0x7F) } else { 0 };
    state[5] = if output { 0x64 } else { 0 };
    // audio_control OUTPUT_PATH_SEL (bits 5:4, mask 0x30): 0x30 = X-X-R →
    // internal speaker, 0x00 = L+R → headphone jack (kernel jack patch).
    // STATE_INIT ships 0x09 (path 00 = jack) — without this the pad routes
    // audio to the unplugged jack and EVERYTHING is silent.
    let path = if jack { 0x00 } else { 0x30 };
    state[7] = (state[7] & !0x30) | path;
    state[37] = 5; // speaker preamp gain (clean ceiling, hardware-tested here)
}

/// vds-proven combined BT report 0x36 (398 B): control TLV + embedded 47-byte
/// pad state (rumble/LED/trigger ride the audio cadence — no interleaved
/// 0x31s) + 64-byte haptics PCM + one Opus speaker/jack frame.
fn audio_report_0x36(
    seq: u8,
    counter: u8,
    state: &[u8; 47],
    hap: &[u8; 64],
    frame: &[u8],
    jack: bool,
) -> Vec<u8> {
    let mut r = vec![0u8; 398];
    r[0] = 0x36;
    r[1] = seq << 4;
    r[2] = 0x91; // 0x11 | sized — control sub-packet
    r[3] = 0x07;
    r[4] = 0xFE; // audio sections on, MIC OFF — 0xFF enables the mic section
                 // and the pad then streams mic-variant input reports that the
                 // kernel misparses as buttons (ghost presses). Mic is phase 3.
    r[5] = 64; // pad-side buffer lengths (five)
    r[6] = 64;
    r[7] = 64;
    r[8] = 64;
    r[9] = 64;
    r[10] = counter;
    r[11] = 0x90; // 0x10 | sized — embedded pad state
    r[12] = 47;
    r[13..60].copy_from_slice(state);
    r[76] = 0x92; // 0x12 | sized — haptics PCM (s8 interleaved, 3 kHz)
    r[77] = 64;
    r[78..142].copy_from_slice(hap);
    r[142] = if jack { 0x96 } else { 0x93 }; // 0x16/0x13 | sized
    r[143] = frame.len() as u8;
    r[144..144 + frame.len()].copy_from_slice(frame);
    put_crc(&mut r);
    r
}

// ---- shared state ----------------------------------------------------------

/// Jack detect published by the relay thread (BT input 0x31 byte 56 bit0).
pub static JACK_PLUGGED: AtomicBool = AtomicBool::new(false);

struct Shared {
    /// Quad interleaved: FL FR RL RR per frame (RL/RR = haptic channels,
    /// matching the USB Pro-Audio layout games target).
    ring: Mutex<VecDeque<[f32; FRAME_SAMPLES * 4]>>,
    /// Kernel-originated BT output reports (rumble/LED 0x31s relayed by the
    /// proxy) waiting to be sequence-stamped and sent by the writer thread.
    /// The pad tracks ONE sequence counter for all interrupt-channel output
    /// reports, so the kernel's own numbering must not go out alongside ours.
    out_q: Mutex<VecDeque<Vec<u8>>>,
    /// speaker_output="forward": front (music) channel pairs FL/FR waiting
    /// for the PipeWire playback stream's process callback. Not consumed in
    /// pad/mute mode (the capture listener doesn't push then).
    fwd: Mutex<VecDeque<[f32; 2]>>,
    /// Set once the pad sink's format negotiation completed (a game linked
    /// its stream). The forward playback thread waits for this before it
    /// connects — autoconnecting earlier races the fresh sink and the link
    /// attempt EINVALs the forward node permanently (verified live).
    sink_negotiated: AtomicBool,
    /// Live pad link (L2CAP interrupt-channel fd), swapped across pad
    /// sessions by `attach_pad`/`detach_pad`. `None` = no pad: the writer
    /// pauses (no writes, no keepalive) while the sink node stays alive so
    /// games never see the audio endpoint die mid-run.
    pub intr: Mutex<Option<Arc<OwnedFd>>>,
    /// Bumped on every `attach_pad`; the writer drops buffered audio and
    /// re-runs its per-link init (handshake) when it changes.
    pub gen: AtomicU64,
    /// Live speaker_output mode (MODE_* below). Written by the proxy on
    /// RELOAD/SIGHUP and by `mdrv-ds speaker <mode>`; read per audio window
    /// by the capture callback (fwd push gate) and the writer (pad-speaker
    /// Opus gate) and polled by forward_thread.
    mode: AtomicU8,
    /// Haptic-channel gain (f32 bits) applied to the rear RL/RR pair at
    /// capture, so game/system volume drops don't thin haptics. Written
    /// by the proxy on RELOAD and by `mdrv-ds gain <v>`; read per audio
    /// window by the capture callback.
    gain: AtomicU32,
    /// True when the live pad link is L2CAP (BT interrupt channel). Chooses
    /// the wire shape `rumble()` emits: 48B 0x02 USB-style report via the
    /// out_q (L2CAP writer restamps), vs a full 78B 0x31 BT frame built with
    /// audio::state_report for hidraw-BT pads.
    l2cap: AtomicBool,
}

/// Handle the proxy uses to hand kernel output reports to the live sink.
static OUT_RELAY: Mutex<Option<Arc<Shared>>> = Mutex::new(None);
const OUT_Q_CAP: usize = 16;

// ---- speaker_output mode switching (live) -----------------------------------

pub const MODE_PAD: u8 = 0;
pub const MODE_FORWARD: u8 = 1;
pub const MODE_MUTE: u8 = 2;

/// Relay for live speaker-mode updates from the proxy process side (the
/// Sink handle never leaves run(), so the CLI/RELOAD paths reach the shared
/// mode cell through this static, mirroring OUT_RELAY).
static MODE_RELAY: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

/// Relay for live haptic-gain updates (same pattern as MODE_RELAY).
static GAIN_RELAY: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

/// Where `mdrv-ds speaker <mode>` drops the runtime override (volatile by
/// design: `speaker reset` or a service restart returns to config.toml).
fn speaker_override_path() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| {
        let mut p = std::path::PathBuf::from(d);
        p.push("mdrv-ds-speaker");
        p
    })
}

/// Config-string → mode code ("pad" default, unknown warns).
pub fn speaker_mode_code(value: Option<&str>) -> u8 {
    match value.unwrap_or("pad") {
        "forward" => MODE_FORWARD,
        "mute" => MODE_MUTE,
        "pad" => MODE_PAD,
        other => {
            eprintln!(
                "sink: audio.speaker_output: unknown value {other:?} (want \"pad\", \"forward\" or \"mute\") — using \"pad\""
            );
            MODE_PAD
        }
    }
}

/// Effective mode: runtime override file (if present) else the config value.
/// Only read at sink start / reload — the hot path uses Shared.mode.
pub fn current_mode(cfg_value: Option<&str>) -> u8 {
    if let Some(p) = speaker_override_path() {
        if let Ok(s) = std::fs::read_to_string(&p) {
            let t = s.trim();
            if !t.is_empty() {
                let code = match t {
                    "forward" => MODE_FORWARD,
                    "mute" => MODE_MUTE,
                    "pad" => MODE_PAD,
                    _ => {
                        eprintln!(
                            "sink: speaker override file holds unknown mode {t:?} — ignoring"
                        );
                        return speaker_mode_code(cfg_value);
                    }
                };
                return code;
            }
        }
    }
    speaker_mode_code(cfg_value)
}

pub fn mode_name(mode: u8) -> &'static str {
    match mode {
        MODE_FORWARD => "forward",
        MODE_MUTE => "mute",
        _ => "pad",
    }
}

/// Flip the live mode (no-op when no sink session is running).
pub fn set_speaker_mode(mode: u8) -> bool {
    let guard = MODE_RELAY.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(shared) => {
            shared.mode.store(mode, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

// ---- haptic gain (live) -----------------------------------------------------

fn gain_override_path() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| {
        let mut p = std::path::PathBuf::from(d);
        p.push("mdrv-ds-gain");
        p
    })
}

/// Clamp to the sane range; NaN/inf → 1.0.
fn sanitize_gain(g: f32) -> f32 {
    if g.is_finite() {
        g.clamp(0.0, 8.0)
    } else {
        1.0
    }
}

/// Effective haptic gain: runtime override file (if present) else the
/// config value (else 1.0). Only read at sink start / reload — the hot
/// path uses Shared.gain.
pub fn current_haptic_gain(cfg_value: Option<f32>) -> f32 {
    if let Some(p) = gain_override_path() {
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(g) = s.trim().parse::<f32>() {
                return sanitize_gain(g);
            }
        }
    }
    sanitize_gain(cfg_value.unwrap_or(1.0))
}

/// Set the live haptic gain (no-op when no sink session is running).
pub fn set_haptic_gain(gain: f32) -> bool {
    let guard = GAIN_RELAY.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(shared) => {
            shared
                .gain
                .store(sanitize_gain(gain).to_bits(), Ordering::Relaxed);
            true
        }
        None => false,
    }
}

// ---- persistent pad-link management (sink survives pad sessions) ------------
//
// The sink node is a process-lifetime resource on L2CAP: pad sessions come
// and go, but tearing the sink down mid-run kills the game's 4ch haptics
// stream, and RE Engine never re-commits AudioClient_Initialize (verified:
// 2026-08-30 PRAGMATA reconnect loss). The proxy attaches the session's
// interrupt-channel fd here and detaches it on teardown; the writer thread
// polls the generation counter and re-inits per link.

/// Stop flag for the persistent sink (proxy exit path mirrors Sink::stop).
static SINK_STOP: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// Install a fresh pad link (dups `fd`; caller keeps ownership). Bumps the
/// generation so the writer re-runs its per-link init. Returns false when no
/// persistent sink is live.
pub fn attach_pad(fd: std::os::unix::io::RawFd) -> bool {
    let guard = MODE_RELAY.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(shared) => {
            let dup = unsafe { libc::dup(fd) };
            if dup < 0 {
                return false;
            }
            *shared.intr.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Arc::new(unsafe { OwnedFd::from_raw_fd(dup) }));
            shared.gen.fetch_add(1, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Mark the pad gone: the writer pauses (no writes, no keepalive) while the
/// sink node stays alive for games.
pub fn detach_pad() -> bool {
    let guard = MODE_RELAY.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(shared) => {
            *shared.intr.lock().unwrap_or_else(|e| e.into_inner()) = None;
            true
        }
        None => false,
    }
}

/// Stop the persistent sink (proxy exit path; a stub Sink handle can't).
pub fn stop_all() {
    let flag = SINK_STOP.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(flag) = flag {
        flag.store(true, Ordering::Relaxed);
    }
    let _ = OUT_RELAY.lock().unwrap_or_else(|e| e.into_inner()).take();
    let _ = MODE_RELAY.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// Try to route a kernel BT output report through the sink writer (unified
/// sequence numbering). Returns false when no sink is running — caller should
/// write the report directly instead.
pub fn enqueue_output(report: &[u8]) -> bool {
    let guard = OUT_RELAY.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(shared) => {
            let mut q = shared.out_q.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= OUT_Q_CAP {
                q.pop_front(); // keep the newest rumble state
            }
            q.push_back(report.to_vec());
            true
        }
        None => false,
    }
}

/// XInput rumble injection: enqueue a minimal DS output state carrying
/// ONLY the rumble motors (flag0 0x03, motor bytes; every other section's
/// enable bits zero → LEDs/audio untouched), shaped for the live sink
/// flavor — 48 B USB 0x02 on L2CAP (kernel-output shape), 78 B BT 0x31 on
/// hidraw so idle relays stay well-formed. Returns false when no sink is
/// running (USB sessions — caller writes the pad's hidraw directly).
pub fn rumble(strong: u8, weak: u8) -> bool {
    let guard = OUT_RELAY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(shared) = guard.as_ref() else {
        return false;
    };
    let mut st = [0u8; 47];
    st[0] = 0x03; // valid_flag0: compatible-vibration motors
    st[2] = weak; // motor_right (weak)
    st[3] = strong; // motor_left (strong)
    let rpt = if shared.l2cap.load(Ordering::Relaxed) {
        let mut v = Vec::with_capacity(48);
        v.push(0x02);
        v.extend_from_slice(&st);
        v
    } else {
        crate::audio::state_report(0, &st)
    };
    let mut q = shared.out_q.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= OUT_Q_CAP {
        q.pop_front(); // keep the newest rumble state
    }
    q.push_back(rpt);
    true
}

/// User-data for the PipeWire process callback: a partial-frame accumulator
/// plus the shared ring.
struct CbState {
    acc: Vec<f32>,
    shared: Arc<Shared>,
    /// Front-channel pairs stashed per callback while the live mode is
    /// "forward", then dumped into the fwd ring (one lock per callback).
    fwd_acc: Vec<[f32; 2]>,
}

pub struct Sink {
    /// `None` on stub handles handed out by `ensure_started` after the first
    /// spawn — the original handle (or `stop_all`) owns the threads.
    stop: Option<Arc<AtomicBool>>,
    /// Raw pw_main_loop pointer for cross-thread quit (0 until connected).
    quit_ptr: Option<Arc<AtomicUsize>>,
    handles: Vec<JoinHandle<()>>,
}

impl Sink {
    fn stub() -> Sink {
        Sink {
            stop: None,
            quit_ptr: None,
            handles: Vec::new(),
        }
    }

    pub fn stop(&mut self) {
        let Some(stop) = self.stop.as_ref() else {
            return; // stub: the persistent sink keeps running by design
        };
        stop.store(true, Ordering::Relaxed);
        // detach the output relay first so no report gets queued for a dying writer
        let _ = OUT_RELAY.lock().unwrap_or_else(|e| e.into_inner()).take();
        let _ = MODE_RELAY.lock().unwrap_or_else(|e| e.into_inner()).take();
        let _ = SINK_STOP.lock().unwrap_or_else(|e| e.into_inner()).take();
        let p = self
            .quit_ptr
            .as_ref()
            .map(|q| q.load(Ordering::Relaxed))
            .unwrap_or(0);
        if p != 0 {
            unsafe { pw::sys::pw_main_loop_quit(p as *mut pw::sys::pw_main_loop) };
        }
        for h in self.handles.drain(..) {
            if h.join().is_err() {
                eprintln!("sink: thread panicked during stop");
            }
        }
        eprintln!("sink: all threads joined");
    }
}

/// Sequence-stamp a kernel BT output report with the unified counter and
/// refresh its CRC (seed 0xA2, same algorithm as everything else we send).
fn restamp_kernel_output(report: &mut [u8], seq: u8) {
    if report.len() == 78 && report[0] == 0x31 {
        report[1] = seq << 4;
        put_crc(report);
    }
}

/// Persistent entry (L2CAP): the sink node lives for the whole proxy run so
/// pad sessions can come and go without games losing the 4ch endpoint. The
/// pad link itself attaches/detaches per session via
/// `attach_pad`/`detach_pad`. Later calls return an inert stub — the first
/// handle (or `stop_all` at exit) owns the threads.
pub fn ensure_started(cfg: &AudioConfig) -> Sink {
    if MODE_RELAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
    {
        eprintln!("sink: persistent sink already up — reusing node");
        return Sink::stub();
    }
    spawn_sink(cfg, None, None, Vec::new())
}

/// Legacy session-scoped entry (hidraw transports).
pub fn start(
    real: &File,
    cfg: &AudioConfig,
    flens: &std::collections::HashMap<u8, usize>,
    l2cap: bool,
) -> Sink {
    let sizes: Vec<(u8, usize)> = [0x09u8, 0x20, 0x05]
        .iter()
        .filter_map(|id| flens.get(id).map(|&s| (*id, s + 1)))
        .collect();
    let vds_real = if !l2cap && !sizes.is_empty() {
        Some(real)
    } else {
        None
    };
    spawn_sink(cfg, Some(real), vds_real, sizes)
}

fn spawn_sink(
    cfg: &AudioConfig,
    pad_fd: Option<&File>,
    vds_real: Option<&File>,
    vds_sizes: Vec<(u8, usize)>,
) -> Sink {
    let stop = Arc::new(AtomicBool::new(false));
    let quit_ptr = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // vds keeps the ACL link warm by polling feature reports {0x09, 0x20,
    // 0x05} every 5 s; a link that naps mid-stream is a stutter source.
    // (hidraw mode only: on L2CAP the pad naps nothing — v29 streamed 30 s
    // with no polls and the socket is not an hidraw anyway.)
    if let Some(real) = vds_real {
        let fd = unsafe { libc::dup(real.as_raw_fd()) };
        let mut file = unsafe { File::from_raw_fd(fd) };
        let p_stop = stop.clone();
        let sizes = vds_sizes;
        handles.push(thread::spawn(move || {
            while !p_stop.load(Ordering::Relaxed) {
                for (id, size) in &sizes {
                    let mut buf = vec![0u8; *size];
                    let _ = crate::hid::get_feature(&mut file, *id, &mut buf);
                }
                thread::sleep(Duration::from_millis(5000));
            }
        }));
    }

    let intr_fd = pad_fd.map(|f| {
        let fd = unsafe { libc::dup(f.as_raw_fd()) };
        Arc::new(unsafe { OwnedFd::from_raw_fd(fd) })
    });
    let shared = Arc::new(Shared {
        ring: Mutex::new(VecDeque::new()),
        out_q: Mutex::new(VecDeque::new()),
        fwd: Mutex::new(VecDeque::new()),
        sink_negotiated: AtomicBool::new(false),
        intr: Mutex::new(intr_fd),
        gen: AtomicU64::new(u64::from(pad_fd.is_some())),
        mode: AtomicU8::new(current_mode(cfg.speaker_output.as_deref())),
        gain: AtomicU32::new(current_haptic_gain(cfg.haptic_gain).to_bits()),
        l2cap: AtomicBool::new(pad_fd.is_none()),
    });
    let _ = OUT_RELAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(shared.clone());
    let _ = MODE_RELAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(shared.clone());
    let _ = GAIN_RELAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(shared.clone());
    let _ = SINK_STOP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(stop.clone());
    eprintln!(
        "sink: speaker_output = {} (live-switchable: mdrv-ds speaker pad|forward|mute|reset)",
        mode_name(shared.mode.load(Ordering::Relaxed))
    );
    eprintln!(
        "sink: haptic_gain = {:.2} (live-switchable: mdrv-ds gain <0..8>|reset)",
        f32::from_bits(shared.gain.load(Ordering::Relaxed))
    );

    let pw_shared = shared.clone();
    let pw_stop = stop.clone();
    let pw_quit = quit_ptr.clone();
    let node_desc = cfg
        .node_description
        .clone()
        .unwrap_or_else(|| "Wireless Controller".into());
    let pw_desc = node_desc.clone();
    handles.push(thread::spawn(move || {
        pw_thread(pw_shared, pw_stop, pw_quit, pw_desc)
    }));

    // Forward playback lives in its OWN context/thread and is spawned in
    // EVERY mode: it self-gates on Shared.mode (polls 250 ms), so a live
    // switch pad→forward comes up without restarting the session. It must
    // only connect once the pad sink negotiated (Shared::sink_negotiated)
    // and must never target the pad sink itself (that would loop captured
    // music back into the capture — a feedback howl).
    {
        let f_shared = shared.clone();
        let f_stop = stop.clone();
        let f_target = cfg.speaker_target.clone();
        handles.push(thread::spawn(move || {
            forward_thread(f_shared, f_stop, f_target)
        }));
    }

    let w_shared = shared.clone();
    let w_stop = stop.clone();
    let l2cap = pad_fd.is_none(); // persistent path is L2CAP-only by design
    let bitrate = cfg.bitrate.unwrap_or(160_000).clamp(6_000, 510_000);
    let combined = l2cap || cfg.report.as_deref() == Some("0x36");
    let report_id: u8 = match cfg.report.as_deref() {
        Some("0x36") => 0x36,
        Some("0x39") => 0x39,
        _ => 0x35,
    };
    // The pad consumes 480-sample Opus frames at its 45 kHz slot clock (one
    // per ~10.667 ms — dsneo 480/45000; hidraw-era sweep confirmed on this
    // pad). vds hits the same cadence indirectly: its 10 ms is only a rate
    // LIMIT, the real pacing is USB-audio production (512-frame windows @
    // 48 kHz = 10.667 ms/block). Our metronom must be the 45 kHz trim on
    // every transport; a flat 10 ms overfeeds the pad by 6.25% and the
    // periodic pad-buffer drops sound like constant stutter.
    let interval_us = cfg.interval_us.unwrap_or(10_667);
    let hp_volume = cfg.volume.unwrap_or(0x64);
    let force_speaker = cfg.speaker == Some(true);
    let output = cfg.output;
    handles.push(thread::spawn(move || {
        writer_thread(
            &w_shared,
            &w_stop,
            bitrate,
            hp_volume,
            force_speaker,
            output,
            combined,
            report_id,
            interval_us,
            l2cap,
        )
    }));

    Sink {
        stop: Some(stop),
        quit_ptr: Some(quit_ptr),
        handles,
    }
}

// ---- PipeWire capture stream -----------------------------------------------

fn pw_thread(
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    quit_ptr: Arc<AtomicUsize>,
    node_desc: String,
) {
    let mainloop: MainLoopRc = match MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => return eprintln!("sink: mainloop init failed: {e}"),
    };
    let context: ContextRc = match ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => return eprintln!("sink: context init failed: {e}"),
    };
    let core: CoreRc = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => return eprintln!("sink: pipewire connect failed: {e}"),
    };
    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        // Impersonate the real cabled pad's PipeWire/UCM sink name so the
        // GE winepulse DualSense matcher (string_contains_dualsense_name +
        // "Speaker__sink" needles) accepts this endpoint as the controller's
        // speaker sink; PRAGMATA's audio/haptic init gates on finding it.
        *pw::keys::NODE_NAME => "alsa_output.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00.Default__Speaker__sink",
        *pw::keys::NODE_DESCRIPTION => node_desc.as_str(),
        // 10 ms quantum: haptic feedback latency = quantum + ring + opus frame;
        // the default (1024/48k ≈ 21 ms) is felt as rumble trailing the action.
        *pw::keys::NODE_LATENCY => "480/48000",
        // Impersonate the DualSense USB audio device so Wine's winepulse
        // classifies this endpoint as the controller's (games like FF16
        // gate audio-based haptics + secondary audio on finding it).
        // Read back by upstream fill_device_info(): PA_PROP_DEVICE_BUS /
        // DEVICE_VENDOR_ID / DEVICE_PRODUCT_ID.
        "device.bus" => "usb",
        "device.vendor.id" => "054c",
        "device.product.id" => "0ce6",
        // winepulse's pulse_add_device only queries get_container_id (GE
        // ds5-haptic patch) when a `sysfs.path` proplist key exists; without
        // it the endpoint registers with a NULL container and games cannot
        // match it to the pad. The patched winepulse stub ignores the value.
        "sysfs.path" => "/devices/pci-0000:00:14.0/usb3/3-2/3-2:1.0",
        "device.profile.description" => "Wireless Controller",
    };
    let stream: StreamRc = match StreamRc::new(
        core.clone(),
        &format!("mdrv-ds-sink-{}", std::process::id()),
        props,
    ) {
        Ok(s) => s,
        Err(e) => return eprintln!("sink: stream init failed: {e}"),
    };

    // speaker_output="forward": playback moved to forward_thread (own
    // context — see start()); the capture callback pushes front-channel
    // pairs into shared.fwd only while the live mode is MODE_FORWARD
    // (checked per callback — mode can flip at runtime).

    let cb_state = CbState {
        acc: Vec::with_capacity(FRAME_SAMPLES * 8),
        shared: shared.clone(),
        fwd_acc: Vec::new(),
    };
    let listener = stream
        .add_local_listener_with_user_data(cb_state)
        .state_changed(|_, _, old, new| {
            eprintln!("sink: state {old:?} → {new:?}");
        })
        .param_changed(|_stream, d, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            if param.is_some() {
                eprintln!("sink: format negotiated");
                // A game linked and drove the sink to a concrete format —
                // the graph around us is now safe for the forward playback
                // stream to connect (forward_thread waits on this).
                d.shared.sink_negotiated.store(true, Ordering::Relaxed);
            }
        })
        .process(|stream, d| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let stride = 16usize; // F32 quad interleaved (FL FR RL RR)

            // Valid region is the chunk (offset,size) the graph filled;
            // maxsize is the buffer capacity. Read before the mutable
            // `data()` borrow.
            let (off, size, max_frames) = {
                let c = data.chunk();
                (
                    c.offset() as usize,
                    c.size() as usize,
                    data.as_raw().maxsize as usize / stride,
                )
            };
            let Some(bytes) = data.data() else { return };
            let start = off.min(bytes.len() / stride / stride * stride);
            let frames = (size / stride)
                .min(bytes.len().saturating_sub(start) / stride)
                .min(max_frames);

            // Accumulate raw F32 quads; push whole 480-sample frames to the
            // ring (RT-safe: one lock per callback, no allocation beyond the
            // accumulator's spare capacity).
            let fwd_live = d.shared.mode.load(Ordering::Relaxed) == MODE_FORWARD;
            // Rear-pair-only gain: FL/FR (music) pass through untouched,
            // RL/RR (haptics) scale + clamp to the nominal F32 range.
            let gain = f32::from_bits(d.shared.gain.load(Ordering::Relaxed));
            let mut o = start;
            let end = start + frames * stride;
            while o < end {
                let mut quad = [0f32; 4];
                for k in 0..4 {
                    let s = f32::from_le_bytes([
                        bytes[o + k * 4],
                        bytes[o + k * 4 + 1],
                        bytes[o + k * 4 + 2],
                        bytes[o + k * 4 + 3],
                    ]);
                    peak_store(&PEAK_IN, s);
                    quad[k] = if k >= 2 && gain != 1.0 {
                        (s * gain).clamp(-1.0, 1.0)
                    } else {
                        s
                    };
                }
                d.acc.extend_from_slice(&quad);
                if fwd_live {
                    d.fwd_acc.push([quad[0], quad[1]]);
                }
                o += stride;
            }
            if fwd_live && !d.fwd_acc.is_empty() {
                let mut fwd = d.shared.fwd.lock().unwrap_or_else(|e| e.into_inner());
                for p in d.fwd_acc.drain(..) {
                    if fwd.len() >= 4800 {
                        fwd.pop_front(); // drop oldest under pressure (~100 ms)
                    }
                    fwd.push_back(p);
                }
            }
            let whole = d.acc.len() / (FRAME_SAMPLES * 4);
            if whole > 0 {
                let mut ring = d.shared.ring.lock().unwrap_or_else(|e| e.into_inner());
                for i in 0..whole {
                    let mut frame = [0f32; FRAME_SAMPLES * 4];
                    let start = i * FRAME_SAMPLES * 4;
                    frame.copy_from_slice(&d.acc[start..start + FRAME_SAMPLES * 4]);
                    if ring.len() >= RING_CAP {
                        ring.pop_front(); // drop oldest under pressure
                        OVERFLOWS.fetch_add(1, Ordering::Relaxed);
                    }
                    ring.push_back(frame);
                }
                d.acc.drain(..whole * FRAME_SAMPLES * 4);
            }
        })
        .register();
    if let Err(e) = listener {
        return eprintln!("sink: listener register failed: {e}");
    }

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(48000);
    info.set_channels(4);
    // USB Pro-Audio layout games target: rear pair carries the haptic tracks
    // (vds ships the same positions via its wireplumber rule).
    let mut position = [0; libspa::param::audio::MAX_CHANNELS];
    position[0] = libspa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = libspa::sys::SPA_AUDIO_CHANNEL_FR;
    position[2] = libspa::sys::SPA_AUDIO_CHANNEL_RL;
    position[3] = libspa::sys::SPA_AUDIO_CHANNEL_RR;
    info.set_position(position);
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let serialized =
        match PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj)) {
            Ok(s) => s.0.into_inner(),
            Err(_) => return eprintln!("sink: pod serialize failed"),
        };
    let mut params = [match Pod::from_bytes(&serialized) {
        Some(p) => p,
        None => return eprintln!("sink: pod from_bytes failed"),
    }];
    let flags = StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS;
    if let Err(e) = stream.connect(Direction::Input, None, flags, &mut params) {
        return eprintln!("sink: connect failed: {e}");
    }
    eprintln!("sink: PipeWire stream connected (Audio/Sink, desc={node_desc})");
    quit_ptr.store(mainloop.as_raw_ptr() as usize, Ordering::Relaxed);
    if stop.load(Ordering::Relaxed) {
        // stop() raced us: quit immediately
        unsafe { pw::sys::pw_main_loop_quit(mainloop.as_raw_ptr()) };
    }
    mainloop.run();
    eprintln!("sink: mainloop exited");
}

// ---- forward playback thread (speaker_output="forward") ---------------------

/// Node name of our own pad-impersonating sink (pw_thread). The forward
/// stream must never target it: the capture listener fills shared.fwd FROM
/// that sink's capture, so a self-targeted playback would re-capture its
/// own output — a feedback loop.
const PAD_SINK_NODE: &str = "alsa_output.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00.Default__Speaker__sink";

/// Pick a playback target that is not our own pad sink: an explicitly
/// configured speaker_target wins; otherwise the current default sink if it
/// isn't ours; otherwise the first other sink present.
fn pick_forward_target(configured: &Option<String>) -> Option<String> {
    let pactl = |args: &[&str]| {
        std::process::Command::new("pactl")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if let Some(t) = configured.as_deref().filter(|t| !t.is_empty()) {
        if t == PAD_SINK_NODE {
            eprintln!(
                "sink: forward: speaker_target is the pad sink itself — ignoring (would feedback)"
            );
        } else {
            return Some(t.to_string());
        }
    }
    let sinks = pactl(&["list", "sinks", "short"])?;
    let names: Vec<&str> = sinks
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    if names.is_empty() {
        return None;
    }
    if let Some(def) = pactl(&["get-default-sink"]).filter(|d| !d.is_empty()) {
        if def != PAD_SINK_NODE && names.contains(&def.as_str()) {
            return Some(def);
        }
    }
    names
        .into_iter()
        .find(|n| *n != PAD_SINK_NODE)
        .map(|n| n.to_string())
}

/// Playback side of speaker_output="forward": a stereo F32 48k stream fed
/// from shared.fwd. Deliberately on its OWN PipeWire context/thread:
///  - spawned in every mode, self-gated on Shared.mode (250 ms poll): a
///    live `mdrv-ds speaker forward` brings it up without a session
///    restart, and any other mode parks it (stream torn down);
///  - it only connects after the pad sink negotiated (game linked), when
///    the graph is stable — an eager connect auto-links to the fresh pad
///    sink and the premature link EINVALs this node permanently (seen
///    live: stuck Connecting, no node ever registered);
///  - it always carries an explicit non-self target.object so neither
///    autoconnect nor WirePlumber's saved routing can loop it into the pad
///    sink;
///  - on stream error (or a mode flip away from forward) it tears down and
///    retries after a backoff.
fn forward_thread(shared: Arc<Shared>, stop: Arc<AtomicBool>, configured: Option<String>) {
    while !stop.load(Ordering::Relaxed) {
        if shared.mode.load(Ordering::Relaxed) != MODE_FORWARD {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        if !shared.sink_negotiated.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        let Some(target) = pick_forward_target(&configured) else {
            eprintln!("sink: forward: no non-pad sink available yet");
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        eprintln!("sink: forward: music → {target}");
        match forward_run(&shared, &stop, &target) {
            ForwardEnd::Stopped => break,
            ForwardEnd::Failed => thread::sleep(Duration::from_secs(2)),
        }
    }
    eprintln!("sink: forward thread exited");
}

enum ForwardEnd {
    Stopped,
    Failed,
}

fn forward_run(shared: &Arc<Shared>, stop: &Arc<AtomicBool>, target: &str) -> ForwardEnd {
    let mainloop: MainLoopRc = match MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("sink: forward mainloop init failed: {e}");
            return ForwardEnd::Failed;
        }
    };
    let context: ContextRc = match ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sink: forward context init failed: {e}");
            return ForwardEnd::Failed;
        }
    };
    let core: CoreRc = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sink: forward pipewire connect failed: {e}");
            return ForwardEnd::Failed;
        }
    };
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_CLASS => "Stream/Output/Audio",
        *pw::keys::NODE_NAME => "mdrv-ds.speaker-forward",
        *pw::keys::NODE_DESCRIPTION => "Controller music (forwarded)",
        *pw::keys::NODE_LATENCY => "480/48000",
    };
    props.insert("target.object", target);
    let stream: StreamRc = match StreamRc::new(core.clone(), "mdrv-ds-speaker-forward", props) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sink: forward stream init failed: {e}");
            return ForwardEnd::Failed;
        }
    };
    let mut finfo = AudioInfoRaw::new();
    finfo.set_format(AudioFormat::F32LE);
    finfo.set_rate(48000);
    finfo.set_channels(2);
    let mut fposition = [0; libspa::param::audio::MAX_CHANNELS];
    fposition[0] = libspa::sys::SPA_AUDIO_CHANNEL_FL;
    fposition[1] = libspa::sys::SPA_AUDIO_CHANNEL_FR;
    finfo.set_position(fposition);
    let fobj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: finfo.into(),
    };
    let Ok(fser) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(fobj))
    else {
        eprintln!("sink: forward pod serialize failed");
        return ForwardEnd::Failed;
    };
    let fwd_ser = fser.0.into_inner();
    let Some(pod) = Pod::from_bytes(&fwd_ser) else {
        eprintln!("sink: forward pod from_bytes failed");
        return ForwardEnd::Failed;
    };
    let mut fparams = [pod];
    // errored flag threaded through the listener so run() can return Failed.
    let errored = Arc::new(AtomicBool::new(false));
    let err_flag = errored.clone();
    let ud = shared.clone();
    let listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed(move |_, _, old, new| {
            eprintln!("sink: forward state {old:?} → {new:?}");
            if format!("{new:?}").contains("Error") {
                err_flag.store(true, Ordering::Relaxed);
            }
        })
        .param_changed(|_s, _d, id, param| {
            if id == ParamType::Format.as_raw() && param.is_some() {
                eprintln!("sink: forward format negotiated");
            }
        })
        .process(|stream, d| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let stride = 8usize; // F32 stereo interleaved
            let Some(bytes) = data.data() else { return };
            let frames = bytes.len() / stride;
            let mut ring = d.fwd.lock().unwrap_or_else(|e| e.into_inner());
            let mut o = 0usize;
            while o < frames {
                match ring.pop_front() {
                    Some([l, r]) => {
                        bytes[o * 8..o * 8 + 4].copy_from_slice(&l.to_le_bytes());
                        bytes[o * 8 + 4..o * 8 + 8].copy_from_slice(&r.to_le_bytes());
                    }
                    None => {
                        // underrun: silence the remainder — buffers
                        // recycle, stale samples would loop the last audio
                        for b in &mut bytes[o * 8..] {
                            *b = 0;
                        }
                        break;
                    }
                }
                o += 1;
            }
            *data.chunk_mut().size_mut() = (o * stride) as u32;
        })
        .register();
    if let Err(e) = listener {
        eprintln!("sink: forward listener register failed: {e}");
        return ForwardEnd::Failed;
    }
    let fflags = StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS;
    if let Err(e) = stream.connect(Direction::Output, None, fflags, &mut fparams) {
        eprintln!("sink: forward connect failed: {e}");
        return ForwardEnd::Failed;
    }
    eprintln!("sink: forward stream connected (music → {target})");
    // Quit watcher: stop flag or stream error ends run().
    let w_quit = Arc::new(AtomicUsize::new(0));
    let quit_raw = mainloop.as_raw_ptr() as usize;
    let w_stop = stop.clone();
    let w_err = errored.clone();
    let w_shared = shared.clone();
    // Detached by design: the watcher ends run(), not the other way round.
    let _watcher = thread::spawn(move || {
        while !w_stop.load(Ordering::Relaxed)
            && !w_err.load(Ordering::Relaxed)
            && w_shared.mode.load(Ordering::Relaxed) == MODE_FORWARD
        {
            thread::sleep(Duration::from_millis(250));
        }
        let _ = w_quit.compare_exchange(0, quit_raw, Ordering::Relaxed, Ordering::Relaxed);
        // mainloop may not be running yet — direct quit is safe regardless.
        unsafe { pw::sys::pw_main_loop_quit(quit_raw as *mut pw::sys::pw_main_loop) };
    });
    mainloop.run();
    eprintln!("sink: forward mainloop exited");
    if shared.mode.load(Ordering::Relaxed) != MODE_FORWARD {
        eprintln!("sink: forward: speaker mode changed — tearing down");
        return ForwardEnd::Failed;
    }
    if errored.load(Ordering::Relaxed) {
        return ForwardEnd::Failed;
    }
    if stop.load(Ordering::Relaxed) {
        return ForwardEnd::Stopped;
    }
    // run() returned without stop and without a flagged error (core
    // disconnect etc.) — treat as retryable.
    ForwardEnd::Failed
}

// ---- writer thread ---------------------------------------------------------

/// Absorb kernel-relayed output reports (rumble/LED/trigger 0x31s). While the
/// audio stream is live the state embeds into the next combined 0x36 report;
/// while idle it is forwarded directly (restamped) so rumble never waits on
/// the audio cadence. Returns false on write failure (pad gone).
///
/// While streaming, EVERY 78 B 0x31 feeds the state tracker (motors/LEDs
/// included) so the periodic unlock re-assert never zeroes live values.
/// vds-style coalescing: at most ONE rumble-bearing report is relayed per
/// wake (latest wins) — FF envelope ramp floods collapse into the cadence.
///
/// L2CAP mode (vds parity): while idle, kernel 0x31s overlay the tracked
/// 63 B state and a plain 0x31 (shared seq) is relayed ONLY when the 47 B
/// state changed — forward_bt_state_if_changed. The old v12-14 "plain 0x31
/// is dead" claim predated the unified sequence numbering; vds relays real
/// 0x31s this way. `last_state47` mirrors what the pad last received (0x31
/// relays and 0x36-embedded state alike) for the change detection.
fn flush_kernel_outputs(
    file: &mut File,
    shared: &Shared,
    seq: &mut u8,
    state: &mut [u8; 47],
    state63: &mut [u8; 63],
    last_state47: &mut [u8; 47],
    hp_volume: u8,
    output: bool,
    jack_path: bool,
    streaming: bool,
    combined: bool,
    l2cap: bool,
) -> bool {
    let write_l2cap = |file: &mut File, report: &[u8]| -> bool {
        let mut wire = Vec::with_capacity(report.len() + 1);
        wire.push(0xA2);
        wire.extend_from_slice(report);
        file.write_all(&wire).is_ok()
    };
    let mut dropped_keepalive = 0u64;
    let mut pending_rumble: Option<Vec<u8>> = None;
    loop {
        let item = shared
            .out_q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        let Some(mut rpt) = item else { break };
        // Kernel outputs: 78 B BT 0x31 (hidraw BT virtual pad, state at
        // [3..50]) or 48 B USB 0x02 (L2CAP virtual pad carries the USB
        // rdesc; state at [1..48]) — both decode to the same 47 B state.
        let st47: Option<[u8; 47]> = if rpt.len() == 78 && rpt[0] == 0x31 {
            Some(rpt[3..50].try_into().unwrap())
        } else if rpt.len() == 48 && (rpt[0] == 0x02 || rpt[0] == 0x05) {
            // 0x05 = game trigger-effect report: same 47 B state layout,
            // only the trigger-FFB enable bits set.
            Some(rpt[1..48].try_into().unwrap())
        } else {
            None
        };
        if let Some(st) = st47 {
            if streaming {
                if l2cap {
                    // Overlay kernel 47 B verbatim (flag0 carries the kernel's
                    // own rumble-valid bits — vds apply_usb_output_report) and
                    // keep the 63 B tail; it rides the next 0x36.
                    state63[..47].copy_from_slice(&st);
                    sink_merge63(state63, hp_volume, output, jack_path);
                } else {
                    // Track ALL state (keep-alives carry LEDs/mic config too).
                    state.copy_from_slice(&st);
                    merge_audio_state(state, hp_volume, output, false);
                    if combined {
                        // rides the next 0x36 state TLV — nothing to relay
                    } else if st[0] & 0x03 != 0 {
                        pending_rumble = Some(rpt); // coalesce: latest wins
                    } else {
                        dropped_keepalive += 1;
                        if dropped_keepalive % 32 == 1 {
                            eprintln!("sink: absorbed {dropped_keepalive} non-rumble outputs during audio (browser/Proton keep-alives)");
                        }
                    }
                }
            } else if l2cap {
                // idle: overlay + relay a plain 0x31, only if state changed
                state63[..47].copy_from_slice(&st);
                sink_merge63(state63, hp_volume, output, jack_path);
                if state63[..47] != *last_state47 {
                    let mut st47b = [0u8; 47];
                    st47b.copy_from_slice(&state63[..47]);
                    let report = crate::audio::state_report(*seq, &st47b);
                    *seq = (*seq + 1) & 0x0F;
                    if !write_l2cap(file, &report) {
                        eprintln!("sink: relay write failed — pad gone?");
                        return false;
                    }
                    *last_state47 = st47b;
                }
            } else {
                // idle hidraw: relay as-is with unified sequence
                restamp_kernel_output(&mut rpt, *seq);
                *seq = (*seq + 1) & 0x0F;
                if let Err(e) = file.write_all(&rpt) {
                    eprintln!("sink: relay write failed ({e}) — pad gone?");
                    return false;
                }
            }
        } else if l2cap {
            // unexpected shape: prefix-relay verbatim (no restamp)
            if !write_l2cap(file, &rpt) {
                eprintln!("sink: relay write failed — pad gone?");
                return false;
            }
        } else {
            // hidraw unexpected shape: relay as-is with unified sequence
            restamp_kernel_output(&mut rpt, *seq);
            *seq = (*seq + 1) & 0x0F;
            if let Err(e) = file.write_all(&rpt) {
                eprintln!("sink: relay write failed ({e}) — pad gone?");
                return false;
            }
        }
    }
    // one coalesced rumble relay per wake (0x35 mode)
    if let Some(mut rpt) = pending_rumble {
        restamp_kernel_output(&mut rpt, *seq);
        *seq = (*seq + 1) & 0x0F;
        if let Err(e) = file.write_all(&rpt) {
            eprintln!("sink: relay write failed ({e}) — pad gone?");
            return false;
        }
    }
    true
}

/// L2CAP: overlay volume/path config on the 63 B observed state, vds
/// `set_audio_out_stream_active` parity:
/// * `[4]` always carries the headphone volume (enable bit gates it);
/// * speaker path: `[5]` = speaker volume, flag0 |= 0x20, audio-control-2
///   enabled (`[37]` = 0x01, flag1 |= 0x80);
/// * jack path: `[5]` = 0 and flag0 0x20 CLEARED (speaker must not sound),
///   flag0 |= 0x10, audio-control-2 disabled (flag1 &= !0x80, `[37]` = 0).
/// The observed tail bytes are v29-proven and left untouched.
pub fn sink_merge63(state: &mut [u8; 63], hp_volume: u8, output: bool, jack: bool) {
    state[4] = if output { hp_volume.min(0x7F) } else { 0 };
    let path = if jack { 0x00 } else { 0x30 };
    state[7] = (state[7] & !0x30) | path;
    if jack {
        state[0] = (state[0] | 0x10) & !0x20; // HP vol on, SPEAKER vol off
        state[1] &= !0x80; // audio-control-2 off
        state[37] = 0;
        state[5] = 0;
    } else {
        state[0] |= 0x20;
        state[1] |= 0x80;
        state[37] = 0x01; // audio_control2 default
        state[5] = if output { 0x64 } else { 0 };
    }
}

fn writer_thread(
    shared: &Shared,
    stop: &AtomicBool,
    bitrate: i32,
    hp_volume: u8,
    force_speaker: bool,
    output: bool,
    combined: bool,
    report_id: u8,
    interval_us: u64,
    l2cap: bool,
) {
    // 0x35/0x39 mode rides a fixed 200 B Opus frame (dsneo-proven shape).
    let bitrate = if combined { bitrate } else { 160_000 };
    let bytes_per_frame = (bitrate / 800) as usize; // 10 ms CBR
    let interval_ns = interval_us * 1000;
    // Pad slot clock is 45 kHz on every transport (dsneo 480/45000; see
    // start()): consume 512 input samples per 480-sample output frame.
    let ratio = 16.0f64 / 15.0;
    // Debug capture (v15 replay experiment): MDRV_DUMP_REPORTS=<path> appends
    // "<monotonic ns> <hex>" per audio report written to the pad.
    let mut dump = std::env::var("MDRV_DUMP_REPORTS").ok().and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .map_err(|e| eprintln!("sink: dump open failed: {e}"))
            .ok()
    });
    let dump_t0 = std::time::Instant::now();
    let mut wdump = |file: &mut File, report: &[u8]| -> bool {
        if let Some(df) = dump.as_mut() {
            let mut hx = String::with_capacity(report.len() * 2);
            for b in report {
                hx.push_str(&format!("{b:02x}"));
            }
            let _ = writeln!(df, "{} {}", dump_t0.elapsed().as_nanos(), hx);
        }
        file.write_all(report).is_ok()
    };
    // output=false + non-combined = haptics-only: stream 142 B 0x32 reports
    // (control + 0x12 PCM, no Opus TLV — SAxense shape, dsneo §3.1/§2.2.2).
    // The PipeWire sink still exists, so game haptic feedback still routes
    // through the rear channels; only the audio sections stay off.
    let haptics_only = !output && !combined && !l2cap;
    let mode_name = if l2cap {
        "0x36 L2CAP (vds dialect)"
    } else if haptics_only {
        "0x32 haptics-only (no audio)"
    } else {
        match report_id {
            0x36 => "0x36 combined",
            0x39 => "0x39 single-frame",
            _ => "0x35 dsneo-proven",
        }
    };
    let enc = Encoder::new(bitrate);
    if !haptics_only && enc.is_none() {
        return;
    }
    eprintln!(
        "sink: writer started ({mode_name}, opus CBR {bitrate} bps = {bytes_per_frame} B/frame, interval {interval_us} us)"
    );

    // Pad slot clock is 45 kHz on every transport (see start()): resample
    // 48k input → 45k output (ratio 16/15), linear interpolation, so 512
    // input samples become one 480-sample frame per ~10.667 ms — exact
    // real-time consumption of the PipeWire stream at the pad's pace.
    // in_buf holds interleaved QUAD input samples (FL FR RL RR); in_pos is a
    // fractional position in input FRAMES. Ch0/1 feed the Opus speaker path;
    // ch2/3 (haptics) are box-averaged 16:1 into s8 PCM for the 0x12 TLV.
    let mut state = STATE_INIT;
    merge_audio_state(&mut state, hp_volume, output, false);
    // L2CAP: tracked 63 B state starts from the vds pristine init (the same
    // state the session INIT 0x32 carried); kernel rumble/LED overlays land
    // in its first 47 B via flush_kernel_outputs, then it rides every 0x36.
    let mut state63 = crate::l2cap::BT_STATE_INIT;
    sink_merge63(&mut state63, hp_volume, output, !force_speaker);
    // 47 B state the pad last received (0x31 relay or 0x36-embedded) — the
    // idle change-detection mirror (vds last_sent_bt_state).
    let mut last_state47 = [0u8; 47];
    let mut mic_seq: u8 = 0; // own family (v28 handshake)
    let mut in_pos: f64 = 0.0;
    let mut in_buf: Vec<f32> = Vec::with_capacity(4096);
    let mut pcm = [0i16; FRAME_SAMPLES * 2];
    let mut hap = [0u8; 64];
    let mut seq: u8 = 0;
    let mut counter: u8 = 0;
    let mut next = Instant::now() + Duration::from_millis(500); // warm-up
    let mut keepalive = Instant::now();
    let mut was_jack = JACK_PLUGGED.load(Ordering::Relaxed);
    let mut jack_dbg = was_jack;
    // Last path acknowledged to the pad via a mic-state 0x31 (vds parity:
    // the pad re-announces a headset plug until the host answers with the
    // matching audio path; see l2cap::mic_state).
    let mut engaged_jack: Option<bool> = None;
    // Prime from the live edge: wait for real data, then drop any backlog
    // beyond the target fill (PipeWire may have queued a burst during warm-up)
    // so the pad buffer starts near-empty instead of overflowed.
    // Likewise, after ~300 ms of starvation we go idle (send nothing) so the
    // pad's own buffer can drain — the spec shows it persists across streams
    // and starting a new play on a full buffer crackles from the first second.
    let mut primed = false;
    let mut starved_frames: u32 = 0;
    // Silence gating (vds parity): the pad's input-report cadence degrades
    // while 0x36 audio packets stream, so only stream REAL audio —
    // PipeWire delivers silence frames even when nothing plays.
    let silence_eps = 0.0005f32;
    let mut silent_windows: u32 = 0;
    let mut stat_underruns = 0u64;
    let mut stat_overflows = 0u64;
    let mut stat_fill_min = i64::MAX;
    let mut stat_fill_max = 0i64;
    let mut stat_frames = 0u32;
    // Real 0x31 volume/unlock reports on the interrupt channel. The dsneo
    // spec is explicit: without a 0x31 carrying valid_flag0 bit 4
    // (AllowHeadphoneVolume) + a headphone volume byte, the audio sections
    // stay silent on PC-only-paired pads. Since the 0x36 rework embeds ALL
    // state in the combined report, no real 0x31 ever reaches the pad —
    // which correlates exactly with the total-silence regression.
    let mut frames_sent: u64 = 0;
    let send_volume_unlock = |file: &mut File, seq: &mut u8, state: &[u8; 47]| {
        // Re-assert audio config FROM CURRENT TRACKED STATE — motor bytes
        // ride along so constant rumble survives the periodic re-assert
        // (a bare volume_report zeroes them and kills steady rumble ~2.7 s
        // in). This also replaces the audio-config duty the absorbed
        // browser/Proton keep-alives used to (accidentally) perform.
        let mut st = *state;
        merge_audio_state(&mut st, hp_volume, output, true);
        let rpt = crate::audio::state_report(*seq, &st);
        file.write_all(&rpt).is_ok()
    };
    // L2CAP audio-start handshake (v28, vdsd parity): a mic-state 0x31 then
    // a mic-open 0x32 — both prefixed — before the first (or re-primed)
    // stream. The 0x31 uses the SHARED sequence; the 0x32 its own family.
    let send_l2cap_handshake = |file: &mut File, seq: &mut u8, mic_seq: &mut u8| -> bool {
        let s31 = crate::audio::state_report(
            *seq,
            &crate::l2cap::mic_state(JACK_PLUGGED.load(Ordering::Relaxed)),
        );
        *seq = (*seq + 1) & 0x0F;
        let s32 = crate::audio::mic_report_032(*mic_seq, true);
        *mic_seq = (*mic_seq + 1) & 0x0F;
        let mut ok = file.write_all(&[0xA2]).is_ok();
        ok = ok && file.write_all(&s31).is_ok();
        ok = ok && file.write_all(&[0xA2]).is_ok();
        ok = ok && file.write_all(&s32).is_ok();
        ok
    };

    // ---- persistent pad-link loop ------------------------------------------
    // The sink outlives pad sessions: `attach_pad` installs a fresh link fd
    // (bumping Shared.gen), `detach_pad` clears it. Each outer iteration
    // runs one full link (state reset → handshake → streaming). When the
    // inner loop breaks (write failure = pad gone) we release the fd and
    // wait for the NEXT generation instead of exiting, so the game's 4ch
    // stream on the sink node survives and haptics resume on the new link.
    let mut cur_gen = 0u64;
    'pads: loop {
        // Wait for a fresh pad link (fd present AND generation moved).
        let (fd, gen) = loop {
            if stop.load(Ordering::Relaxed) {
                break 'pads;
            }
            let g = shared.gen.load(Ordering::Relaxed);
            let fd = shared
                .intr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(fd) = fd {
                if g != cur_gen {
                    break (fd, g);
                }
            }
            thread::sleep(Duration::from_millis(100));
        };
        cur_gen = gen;
        let dup = unsafe { libc::dup(fd.as_raw_fd()) };
        let mut file = unsafe { File::from_raw_fd(dup) };
        // Fresh link: reset per-link state so a re-attached pad doesn't get
        // stale buffered audio or jack/sequence state from the last link —
        // but KEEP `state`/`state63`: they hold the game's last wanted
        // output (adaptive-trigger effects, motors, lightbar), and the game
        // does NOT re-send its effect configuration after a pad reconnect.
        // Re-merging audio fields + zeroing `last_state47` makes change
        // detection replay the full state as a standalone 0x31 right after
        // the handshake, restoring trigger resistance on the new link.
        merge_audio_state(&mut state, hp_volume, output, false);
        sink_merge63(&mut state63, hp_volume, output, !force_speaker);
        last_state47 = [0u8; 47];
        mic_seq = 0;
        in_pos = 0.0;
        in_buf.clear();
        seq = 0;
        counter = 0;
        next = Instant::now() + Duration::from_millis(500); // warm-up
        keepalive = Instant::now();
        was_jack = JACK_PLUGGED.load(Ordering::Relaxed);
        jack_dbg = was_jack;
        engaged_jack = None;
        primed = false;
        starved_frames = 0;
        silent_windows = 0;
        frames_sent = 0;
        eprintln!("sink: writer attached to pad link (gen {gen})");
        // Unlock audio immediately (before the first combined report).
        if l2cap {
            if !send_l2cap_handshake(&mut file, &mut seq, &mut mic_seq) {
                eprintln!("sink: handshake failed (gen {gen}) — awaiting next link");
                thread::sleep(Duration::from_millis(100));
                continue 'pads;
            }
        } else if !send_volume_unlock(&mut file, &mut seq, &state) {
            thread::sleep(Duration::from_millis(100));
            continue 'pads;
        }
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Periodic volume unlock (~2.7 s) is 0x35-hidraw-path only; vds sends
            // the L2CAP mic handshake solely at stream start / re-prime, so the
            // L2CAP path sends nothing extra here.
            if !l2cap && frames_sent > 0 && frames_sent % 256 == 0 {
                if !send_volume_unlock(&mut file, &mut seq, &state) {
                    break;
                }
            }
            // Kernel rumble/LED reports: absorbed into the combined report while
            // streaming (vds-style — no interleaved 0x31s on the interrupt
            // channel), relayed directly while idle.
            let jack_plugged = JACK_PLUGGED.load(Ordering::Relaxed);
            if jack_plugged != jack_dbg {
                eprintln!("sink: JACK_PLUGGED load -> {}", jack_plugged);
                jack_dbg = jack_plugged;
            }
            if jack_plugged != was_jack {
                eprintln!("sink: JACK_PLUGGED load -> {}", jack_plugged);
                was_jack = jack_plugged;
            }
            let jack_path = if force_speaker { false } else { jack_plugged };
            // Jack engage: answer every plug/unplug edge with a mic-state 0x31
            // carrying the new path (works while idle too — the pad needs the
            // ack even when no audio is streaming). Runs before the flush so
            // the pad latches HP-detect before the next payload arrives.
            if l2cap && engaged_jack != Some(jack_path) {
                let ms = crate::audio::state_report(seq, &crate::l2cap::mic_state(jack_path));
                seq = (seq + 1) & 0x0F;
                if !(file.write_all(&[0xA2]).is_ok() && file.write_all(&ms).is_ok()) {
                    break;
                }
                eprintln!(
                    "sink: jack engage → {} (mic-state 0x31)",
                    if jack_path { "headset" } else { "speaker" }
                );
                engaged_jack = Some(jack_path);
            }
            // BT idle keep-alive: the pad powers off after a few minutes with
            // no output traffic. While streaming, the 10 ms 0x36 cadence keeps
            // it awake; while idle, re-assert a zero-flags 0x31 state report
            // (applies nothing) every 2 s to reset the pad's sleep timer —
            // the PC equivalent of the PS5's constant output stream.
            if l2cap && !primed && keepalive.elapsed() >= Duration::from_millis(2000) {
                keepalive = Instant::now();
                let noop = crate::audio::state_report(seq, &[0u8; 47]);
                seq = (seq + 1) & 0x0F;
                if !(file.write_all(&[0xA2]).is_ok() && file.write_all(&noop).is_ok()) {
                    break;
                }
            }
            if !flush_kernel_outputs(
                &mut file,
                shared,
                &mut seq,
                &mut state,
                &mut state63,
                &mut last_state47,
                hp_volume,
                output,
                jack_path,
                primed,
                combined,
                l2cap,
            ) {
                break;
            }
            // Gather input until one output frame can be produced.
            loop {
                let need_samples = (in_pos + (FRAME_SAMPLES as f64) * ratio).ceil() as usize + 2;
                if in_buf.len() / 4 >= need_samples {
                    break;
                }
                let got = {
                    let mut ring = shared.ring.lock().unwrap_or_else(|e| e.into_inner());
                    ring.pop_front()
                };
                match got {
                    Some(frame) => in_buf.extend_from_slice(&frame),
                    None => break,
                }
            }
            if !primed {
                let fill = {
                    let mut ring = shared.ring.lock().unwrap_or_else(|e| e.into_inner());
                    while ring.len() as i64 > TARGET_FILL_FRAMES {
                        ring.pop_front(); // discard backlog (not an overflow)
                    }
                    ring.len() as i64
                };
                if fill == 0 {
                    // no real audio — send nothing; pad buffer drains meanwhile.
                    // MUST sleep: `continue` skips the pacing sleep at the loop
                    // bottom, and without this the idle writer hot-spins, choking
                    // the out_q mutex the rumble relay contends on.
                    starved_frames = 0;
                    next = Instant::now() + Duration::from_millis(20);
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                // vds parity: prime only on non-silent audio (see silence
                // gate below) — priming on PipeWire's idle-silence frames
                // would flood the pad with silent 0x36s and wreck its input
                // cadence.
                let peak = in_buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                if l2cap && peak < silence_eps {
                    in_buf.clear();
                    in_pos = 0.0;
                    next = Instant::now() + Duration::from_millis(20);
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                primed = true;
                starved_frames = 0;
                eprintln!("sink: primed (ring fill {fill})");
                // L2CAP re-prime: re-send mic-state + mic-open handshake (v28, same
                // sequence as the start-of-stream handshake). The pad may have
                // dropped the previous handshakes after draining its buffer.
                if l2cap && !send_l2cap_handshake(&mut file, &mut seq, &mut mic_seq) {
                    break;
                }
            }
            let need_samples = (in_pos + (FRAME_SAMPLES as f64) * ratio).ceil() as usize + 2;
            if in_buf.len() / 4 < need_samples {
                // underrun: pad with silence, keep the clock running
                starved_frames += 1;
                if starved_frames > 30 {
                    // ~320 ms with no data: go idle — stop feeding so the pad
                    // buffer drains; re-prime when real audio returns
                    primed = false;
                    in_buf.clear();
                    in_pos = 0.0;
                    eprintln!("sink: idle (input starved — letting pad buffer drain)");
                    continue;
                }
                UNDERRUNS.fetch_add(1, Ordering::Relaxed);
                stat_underruns += 1;
                in_buf.resize(need_samples * 4, 0.0);
            } else {
                starved_frames = 0;
            }
            let main_silent = in_buf.iter().all(|s| s.abs() < silence_eps);
            // Silence gate (vds parity): skip the whole window when nothing
            // real is in it — no 0x36 leaves the host during quiet/idle audio,
            // which keeps the pad's input-report cadence clean (the pad bursts
            // its 0x31s between constant 0x36s, and games reject that).
            if l2cap && main_silent {
                in_buf.clear();
                in_pos = 0.0;
                starved_frames = 0;
                silent_windows += 1;
                if silent_windows >= 30 {
                    // ~320 ms of quiet: go idle — kernel outputs fall back to
                    // plain 0x31s until real audio re-primes the stream.
                    primed = false;
                    silent_windows = 0;
                    eprintln!("sink: idle (input silent — letting pad buffer drain)");
                }
                next += Duration::from_nanos(interval_ns);
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                } else if now.duration_since(next) > Duration::from_millis(200) {
                    next = now;
                }
                continue;
            }
            silent_windows = 0;
            // Produce 480 resampled stereo samples (speaker path, ch0/1).
            // speaker_output=forward/mute: the pad speaker path carries silence
            // (the Opus TLV must stay a valid frame — haptics ride the same
            // report); music goes out via the forward playback stream instead.
            // Live-switchable: re-read the mode every window.
            let pad_speaker = shared.mode.load(Ordering::Relaxed) == MODE_PAD;
            for s in 0..FRAME_SAMPLES {
                let i0 = in_pos.floor() as usize;
                let frac = (in_pos - i0 as f64) as f32;
                let l0 = in_buf[i0 * 4];
                let l1 = in_buf[i0 * 4 + 4];
                let r0 = in_buf[i0 * 4 + 1];
                let r1 = in_buf[i0 * 4 + 5];
                if pad_speaker {
                    pcm[s * 2] = (lerp(l0, l1, frac).clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm[s * 2 + 1] = (lerp(r0, r1, frac).clamp(-1.0, 1.0) * 32767.0) as i16;
                } else {
                    pcm[s * 2] = 0;
                    pcm[s * 2 + 1] = 0;
                }
                in_pos += ratio;
            }
            let consumed_pairs = in_pos.floor() as usize;

            // Haptics path (ch2/3): box-average the frames this output step
            // consumed into 32 haptic frames (vds resample_haptics_pcm): group
            // average → s16 → /256 → s8, interleaved L/R.
            {
                let n = consumed_pairs.min(in_buf.len() / 4);
                for h in 0..32usize {
                    let begin = h * n / 32;
                    let end = ((h + 1) * n / 32).max(begin + 1);
                    let mut lsum = 0i32;
                    let mut rsum = 0i32;
                    for j in begin..end.min(n) {
                        lsum += (in_buf[j * 4 + 2].clamp(-1.0, 1.0) * 32767.0) as i32;
                        rsum += (in_buf[j * 4 + 3].clamp(-1.0, 1.0) * 32767.0) as i32;
                    }
                    let count = (end.min(n) - begin.min(n)).max(1) as i32;
                    let l = (lsum / count) / 256;
                    let r = (rsum / count) / 256;
                    hap[h * 2] = l.clamp(-128, 127) as i8 as u8;
                    hap[h * 2 + 1] = r.clamp(-128, 127) as i8 as u8;
                }
            }
            in_buf.drain(..consumed_pairs * 4);
            in_pos -= consumed_pairs as f64;

            // Haptics-only: no Opus TLV, no audio path selection — just the
            // 0x12 PCM frame in a minimal 0x32 report.
            let ok = if haptics_only {
                let report = crate::audio::haptics_report_0x32(seq, counter, &hap);
                counter = counter.wrapping_add(2);
                wdump(&mut file, &report)
            } else {
                let mut frame = [0u8; 512];
                let pcm_out: &[i16] = &pcm;
                for s in pcm_out.iter() {
                    peak_store(&PEAK_PCM, *s as f32 / 32768.0);
                }
                let Some(enc_ref) = enc.as_ref() else {
                    break;
                };
                let n = unsafe {
                    opus_encode(
                        enc_ref.0,
                        pcm_out.as_ptr(),
                        FRAME_SAMPLES as libc::c_int,
                        frame.as_mut_ptr(),
                        512,
                    )
                };
                if n <= 0 {
                    eprintln!("sink: opus_encode failed ({n})");
                    continue;
                }
                if n as usize != bytes_per_frame {
                    // CBR must give a constant size; anything else means the pad will
                    // misdecode. Log loudly, drop the frame (keep the cadence).
                    eprintln!(
                    "sink: encoder returned {n}B, expected {bytes_per_frame}B (CBR off?) — frame dropped"
                );
                    continue;
                }
                // Routing follows plug state: jack or speaker per config.
                let jack = jack_path;
                if jack != was_jack {
                    eprintln!(
                        "sink: routing → {}",
                        if jack { "headset jack" } else { "speaker" }
                    );
                    was_jack = jack;
                }
                // Re-assert audio state incl. path-select for this frame's routing.
                if l2cap {
                    sink_merge63(&mut state63, hp_volume, output, jack);
                    let mut f200 = [0u8; 200];
                    f200[..n as usize].copy_from_slice(&frame[..n as usize]);
                    let report = crate::audio::audio_report_036_bt(
                        seq, counter, &state63, &hap, &f200, jack,
                    );
                    counter = counter.wrapping_add(1);
                    // Wire = HIDP prefix 0xA2 + raw report.
                    let mut wire = Vec::with_capacity(report.len() + 1);
                    wire.push(0xA2);
                    wire.extend_from_slice(&report);
                    wdump(&mut file, &wire)
                } else {
                    merge_audio_state(&mut state, hp_volume, output, jack);
                    if combined {
                        let report = audio_report_0x36(
                            seq,
                            counter,
                            &state,
                            &hap,
                            &frame[..n as usize],
                            jack,
                        );
                        counter = counter.wrapping_add(1);
                        wdump(&mut file, &report)
                    } else {
                        let mut f200 = [0u8; 200];
                        f200.copy_from_slice(&frame[..n as usize]);
                        let report = match report_id {
                            0x39 => crate::audio::audio_report_0x39(seq, counter, &f200, jack),
                            _ => crate::audio::audio_report(seq, counter, &f200, jack),
                        };
                        counter = counter.wrapping_add(2);
                        wdump(&mut file, &report)
                    }
                }
            };
            if !ok {
                eprintln!("sink: write failed — pad gone?");
                break;
            }
            seq = (seq + 1) & 0x0F;
            if l2cap {
                // this 0x36 delivered the tracked state — update the mirror
                last_state47.copy_from_slice(&state63[..47]);
            }

            // Pace: configurable interval (default 10667 µs), gently steered by
            // ring fill. Low gain: the spec's sweep shows ±tens of µs matters,
            // so correct drift slowly instead of chasing per-quantum sawtooth.
            let fill = {
                let ring = shared.ring.lock().unwrap_or_else(|e| e.into_inner());
                ring.len() as i64
            };
            stat_fill_min = stat_fill_min.min(fill);
            stat_fill_max = stat_fill_max.max(fill);
            let err = fill - TARGET_FILL_FRAMES;
            // Gentle steering on every transport: keeps the metronom glued to
            // the PipeWire production clock (fill error → ±tens of µs), which
            // also absorbs scheduler jitter. Flat pacing accumulates any
            // production/consumption mismatch until the pad drops frames.
            let adj = (interval_ns as i64 - err * 3_000).max(9_000_000);
            next += Duration::from_nanos(adj as u64);
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else if now.duration_since(next) > Duration::from_millis(200) {
                next = now; // way behind: resync
            }

            // Periodic health stats (~5.4 s window).
            stat_frames += 1;
            frames_sent += 1;
            if stat_frames >= 512 {
                stat_frames = 0;
                let un = UNDERRUNS.load(Ordering::Relaxed);
                let ov = OVERFLOWS.load(Ordering::Relaxed);
                let pin = f32::from_bits(PEAK_IN.swap(0, Ordering::Relaxed));
                let ppm = f32::from_bits(PEAK_PCM.swap(0, Ordering::Relaxed));
                eprintln!(
                "sink: stats fill {fill} (min {} max {}), underruns +{} ({} total), overflows +{} ({} total), peak in {pin:.3} pcm {ppm:.3}",
                if stat_fill_min == i64::MAX { -1 } else { stat_fill_min },
                stat_fill_max,
                un.saturating_sub(stat_underruns),
                un,
                ov.saturating_sub(stat_overflows),
                ov,
            );
                stat_underruns = un;
                stat_overflows = ov;
                stat_fill_min = i64::MAX;
                stat_fill_max = 0;
            }
        }
        // Inner loop broke: write failure = pad link dead. Keep the sink
        // node alive for the game, drop the fd, wait for the next attach.
        eprintln!("sink: pad link down (gen {gen}) — sink stays up, awaiting re-attach");
        thread::sleep(Duration::from_millis(50));
    }
    let _ = OUT_RELAY.lock().unwrap_or_else(|e| e.into_inner()).take();
    eprintln!("sink: writer exited");
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
