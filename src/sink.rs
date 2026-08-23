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
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
const NOMINAL_INTERVAL_NS: u64 = 10_666_667; // 480/45000
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
        pcm: *const libc::int16_t,
        frame_size: libc::c_int,
        data: *mut libc::uint8_t,
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
    s[42] = 0x02;
    s[43] = 0x01;
    s[45] = 0xFF;
    s[46] = 0xD7;
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
}

/// Handle the proxy uses to hand kernel output reports to the live sink.
static OUT_RELAY: Mutex<Option<Arc<Shared>>> = Mutex::new(None);
const OUT_Q_CAP: usize = 16;

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

/// User-data for the PipeWire process callback: a partial-frame accumulator
/// plus the shared ring.
struct CbState {
    acc: Vec<f32>,
    shared: Arc<Shared>,
}

pub struct Sink {
    stop: Arc<AtomicBool>,
    /// Raw pw_main_loop pointer for cross-thread quit (0 until connected).
    quit_ptr: Arc<AtomicUsize>,
    handles: Vec<JoinHandle<()>>,
}

impl Sink {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // detach the output relay first so no report gets queued for a dying writer
        let _ = OUT_RELAY.lock().unwrap_or_else(|e| e.into_inner()).take();
        let p = self.quit_ptr.load(Ordering::Relaxed);
        if p != 0 {
            unsafe { pw::sys::pw_main_loop_quit(p as *mut pw::sys::pw_main_loop) };
        }
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
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

/// Entry point called from the proxy when transport == Bluetooth.
pub fn start(real: &File, cfg: &AudioConfig, flens: &std::collections::HashMap<u8, usize>) -> Sink {
    let stop = Arc::new(AtomicBool::new(false));
    let quit_ptr = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // vds keeps the ACL link warm by polling feature reports {0x09, 0x20,
    // 0x05} every 5 s; a link that naps mid-stream is a stutter source.
    {
        let fd = unsafe { libc::dup(real.as_raw_fd()) };
        let mut file = unsafe { File::from_raw_fd(fd) };
        let p_stop = stop.clone();
        let sizes: Vec<(u8, usize)> = [0x09u8, 0x20, 0x05]
            .iter()
            .filter_map(|id| flens.get(id).map(|&s| (*id, s + 1)))
            .collect();
        if !sizes.is_empty() {
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
    }

    let shared = Arc::new(Shared {
        ring: Mutex::new(VecDeque::new()),
        out_q: Mutex::new(VecDeque::new()),
    });
    let _ = OUT_RELAY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(shared.clone());

    let pw_shared = shared.clone();
    let pw_stop = stop.clone();
    let pw_quit = quit_ptr.clone();
    let node_desc = cfg
        .node_description
        .clone()
        .unwrap_or_else(|| "DualSense Wireless Controller".into());
    handles.push(thread::spawn(move || {
        pw_thread(pw_shared, pw_stop, pw_quit, node_desc)
    }));

    let fd = unsafe { libc::dup(real.as_raw_fd()) };
    let mut file = unsafe { File::from_raw_fd(fd) };
    let w_shared = shared.clone();
    let w_stop = stop.clone();
    let bitrate = cfg.bitrate.unwrap_or(160_000).clamp(6_000, 510_000);
    let combined = cfg.report.as_deref() == Some("0x36");
    let report_id: u8 = match cfg.report.as_deref() {
        Some("0x36") => 0x36,
        Some("0x39") => 0x39,
        _ => 0x35,
    };
    let interval_us = cfg.interval_us.unwrap_or(10667);
    let hp_volume = cfg.volume.unwrap_or(0x64);
    let force_speaker = cfg.speaker == Some(true);
    let output = cfg.output;
    handles.push(thread::spawn(move || {
        writer_thread(
            &mut file,
            &w_shared,
            &w_stop,
            bitrate,
            hp_volume,
            force_speaker,
            output,
            combined,
            report_id,
            interval_us,
        )
    }));

    Sink {
        stop,
        quit_ptr,
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
        *pw::keys::NODE_NAME => "mdrv-ds.dualsense-bt",
        *pw::keys::NODE_DESCRIPTION => node_desc.as_str(),
        // 10 ms quantum: haptic feedback latency = quantum + ring + opus frame;
        // the default (1024/48k ≈ 21 ms) is felt as rumble trailing the action.
        *pw::keys::NODE_LATENCY => "480/48000",
    };
    let stream: StreamRc = match StreamRc::new(
        core.clone(),
        &format!("mdrv-ds-sink-{}", std::process::id()),
        props,
    ) {
        Ok(s) => s,
        Err(e) => return eprintln!("sink: stream init failed: {e}"),
    };

    let cb_state = CbState {
        acc: Vec::with_capacity(FRAME_SAMPLES * 8),
        shared: shared.clone(),
    };
    let listener = stream
        .add_local_listener_with_user_data(cb_state)
        .state_changed(|_, _, old, new| {
            eprintln!("sink: state {old:?} → {new:?}");
        })
        .param_changed(|_stream, _d, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            if param.is_some() {
                eprintln!("sink: format negotiated");
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
            let mut o = start;
            let end = start + frames * stride;
            while o < end {
                for k in 0..4 {
                    let s = f32::from_le_bytes([
                        bytes[o + k * 4],
                        bytes[o + k * 4 + 1],
                        bytes[o + k * 4 + 2],
                        bytes[o + k * 4 + 3],
                    ]);
                    peak_store(&PEAK_IN, s);
                    d.acc.push(s);
                }
                o += stride;
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
fn flush_kernel_outputs(
    file: &mut File,
    shared: &Shared,
    seq: &mut u8,
    state: &mut [u8; 47],
    hp_volume: u8,
    output: bool,
    streaming: bool,
    combined: bool,
) -> bool {
    let mut dropped_keepalive = 0u64;
    let mut pending_rumble: Option<Vec<u8>> = None;
    loop {
        let item = shared
            .out_q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        let Some(mut rpt) = item else { break };
        if streaming && rpt.len() == 78 && rpt[0] == 0x31 {
            // Track ALL state (keep-alives carry LEDs/mic config too).
            state.copy_from_slice(&rpt[3..50]);
            merge_audio_state(state, hp_volume, output, false);
            if combined {
                // rides the next 0x36 state TLV — nothing to relay
            } else if rpt[3] & 0x03 != 0 {
                pending_rumble = Some(rpt); // coalesce: latest wins
            } else {
                dropped_keepalive += 1;
                if dropped_keepalive % 32 == 1 {
                    eprintln!("sink: absorbed {dropped_keepalive} non-rumble 0x31s during audio (browser/Proton keep-alives)");
                }
            }
        } else {
            // idle (or unexpected shape): relay as-is with unified sequence
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

fn writer_thread(
    file: &mut File,
    shared: &Shared,
    stop: &AtomicBool,
    bitrate: i32,
    hp_volume: u8,
    force_speaker: bool,
    output: bool,
    combined: bool,
    report_id: u8,
    interval_us: u64,
) {
    // 0x35/0x39 mode rides a fixed 200 B Opus frame (dsneo-proven shape).
    let bitrate = if combined { bitrate } else { 160_000 };
    let bytes_per_frame = (bitrate / 800) as usize; // 10 ms CBR
    let interval_ns = interval_us * 1000;
    let mode_name = match report_id {
        0x36 => "0x36 combined",
        0x39 => "0x39 single-frame",
        _ => "0x35 dsneo-proven",
    };
    let mut enc = match Encoder::new(bitrate) {
        Some(e) => e,
        None => return,
    };
    eprintln!(
        "sink: writer started ({mode_name}, opus CBR {bitrate} bps = {bytes_per_frame} B/frame, interval {interval_us} us)"
    );

    // Resampler: 48k input → 45k output (ratio 16/15), linear interpolation.
    // in_buf holds interleaved QUAD input samples (FL FR RL RR); in_pos is a
    // fractional position in input FRAMES. Ch0/1 feed the Opus speaker path;
    // ch2/3 (haptics) are box-averaged 16:1 into s8 PCM for the 0x12 TLV.
    let mut state = STATE_INIT;
    merge_audio_state(&mut state, hp_volume, output, false);
    let mut in_pos: f64 = 0.0;
    let mut in_buf: Vec<f32> = Vec::with_capacity(4096);
    let mut pcm = [0i16; FRAME_SAMPLES * 2];
    let mut hap = [0u8; 64];

    let mut seq: u8 = 0;
    let mut counter: u8 = 0;
    let mut next = Instant::now() + Duration::from_millis(500); // warm-up
    let mut was_jack = JACK_PLUGGED.load(Ordering::Relaxed);
    // Prime from the live edge: wait for real data, then drop any backlog
    // beyond the target fill (PipeWire may have queued a burst during warm-up)
    // so the pad buffer starts near-empty instead of overflowed.
    // Likewise, after ~300 ms of starvation we go idle (send nothing) so the
    // pad's own buffer can drain — the spec shows it persists across streams
    // and starting a new play on a full buffer crackles from the first second.
    let mut primed = false;
    let mut starved_frames: u32 = 0;
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
    let mut send_volume_unlock = |file: &mut File, seq: &mut u8, state: &[u8; 47]| {
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

    // Unlock audio immediately (before the first combined report).
    if !send_volume_unlock(file, &mut seq, &state) {
        return;
    }
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if frames_sent > 0 && frames_sent % 256 == 0 {
            // Re-assert the unlock periodically (~2.7 s).
            if !send_volume_unlock(file, &mut seq, &state) {
                break;
            }
        }
        // Kernel rumble/LED reports: absorbed into the combined report while
        // streaming (vds-style — no interleaved 0x31s on the interrupt
        // channel), relayed directly while idle.
        if !flush_kernel_outputs(
            file, shared, &mut seq, &mut state, hp_volume, output, primed, combined,
        ) {
            break;
        }
        // Gather input until one output frame can be produced.
        loop {
            let need_samples = (in_pos + (FRAME_SAMPLES as f64) * 16.0 / 15.0).ceil() as usize + 2;
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
                // no real audio — send nothing; pad buffer drains meanwhile
                starved_frames = 0;
                next = Instant::now() + Duration::from_millis(20);
                continue;
            }
            primed = true;
            starved_frames = 0;
            eprintln!("sink: primed (ring fill {fill})");
        }
        let need_samples = (in_pos + (FRAME_SAMPLES as f64) * 16.0 / 15.0).ceil() as usize + 2;
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

        // Produce 480 resampled stereo samples (speaker path, ch0/1).
        for s in 0..FRAME_SAMPLES {
            let i0 = in_pos.floor() as usize;
            let frac = (in_pos - i0 as f64) as f32;
            let l0 = in_buf[i0 * 4];
            let l1 = in_buf[i0 * 4 + 4];
            let r0 = in_buf[i0 * 4 + 1];
            let r1 = in_buf[i0 * 4 + 5];
            pcm[s * 2] = (lerp(l0, l1, frac).clamp(-1.0, 1.0) * 32767.0) as i16;
            pcm[s * 2 + 1] = (lerp(r0, r1, frac).clamp(-1.0, 1.0) * 32767.0) as i16;
            in_pos += 16.0 / 15.0;
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

        let mut frame = [0u8; 512];
        for s in pcm.iter() {
            peak_store(&PEAK_PCM, *s as f32 / 32768.0);
        }
        let n = unsafe {
            opus_encode(
                enc.0,
                pcm.as_ptr(),
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
        // EXPERIMENT(jack TLV): the 0x93 speaker TLV has never been audible on
        // this pad — every audible test (Phase-1 tone, Phase-2 music) used the
        // 0x96 jack TLV and the pad played it through the built-in speaker.
        // Force jack TLV + jack path until the speaker TLV is understood.
        let jack = true;
        let _ = force_speaker;
        if jack != was_jack {
            eprintln!(
                "sink: routing → {}",
                if jack { "headset jack" } else { "speaker" }
            );
            was_jack = jack;
        }
        // Re-assert audio state incl. path-select for this frame's routing.
        merge_audio_state(&mut state, hp_volume, output, jack);
        let ok = if combined {
            let report = audio_report_0x36(seq, counter, &state, &hap, &frame[..n as usize], jack);
            counter = counter.wrapping_add(1);
            file.write_all(&report).is_ok()
        } else {
            let mut f200 = [0u8; 200];
            f200.copy_from_slice(&frame[..n as usize]);
            let report = match report_id {
                0x39 => crate::audio::audio_report_0x39(seq, counter, &f200, jack),
                _ => crate::audio::audio_report(seq, counter, &f200, jack),
            };
            counter = counter.wrapping_add(2);
            file.write_all(&report).is_ok()
        };
        if !ok {
            eprintln!("sink: write failed — pad gone?");
            break;
        }
        seq = (seq + 1) & 0x0F;

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
    let _ = OUT_RELAY.lock().unwrap_or_else(|e| e.into_inner()).take();
    eprintln!("sink: writer exited");
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
