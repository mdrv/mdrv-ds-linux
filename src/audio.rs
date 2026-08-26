//! Phase 1 audio: BT test tone to the DualSense headset jack / speaker.
//!
//! Wire protocol (per /tmp/opencode/dsneo_spec.md, OBSERVED):
//! - Ladder report 0x35 (334 B): [0]=0x35 [1]=seq<<4 [2]=0x91 (0x11|sized)
//!   [3]=0x07 [4]=0xFE (audio mask) [5..8]=0 [9]=counter (starts 0xff, +=2)
//!   [10]=0 [11]=0x96 (0x16|sized, jack) or 0x93 (0x13|sized, speaker)
//!   [12]=0xC8 [13..212]=Opus 200 B [213..329]=0 [330..333]=CRC32 LE
//! - CRC32 over [0xA2] + report[..len-4], poly 0xEDB88320 reflected,
//!   init 0xFFFFFFFF, final complement.
//! - Opus: 48 kHz stereo, CBR 160 kbit/s, VBR off, 10 ms frames = exactly
//!   200 B. Pad DAC runs at 45000 Hz → generate pitch * 48/45 to land true.
//! - Required once: 0x31 (78 B) with valid_flag0 bit 4 (AllowHeadphoneVolume)
//!   + headphone volume byte, else the jack stays silent on PC-paired pads.
//! - Pacing: 480/45000 = 10667 µs per frame; write() success means nothing.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::AudioConfig;

const FRAME_SAMPLES: usize = 480; // 10 ms @ 48 kHz
const OPUS_BYTES: usize = 200;
const SEND_INTERVAL: Duration = Duration::from_nanos(10_666_667); // 480/45000
const REPORT_0X35: usize = 334;
const REPORT_0X39: usize = 547;
const REPORT_0X31: usize = 78;
const REPORT_0X32: usize = 142;

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
// NB: opus 1.6 renumbered VBR ctl (4006); ≤1.5 used 4004. Try both.
const OPUS_SET_VBR_NEW: libc::c_int = 4006;
const OPUS_SET_VBR_OLD: libc::c_int = 4004;

struct Encoder(*mut libc::c_void);
unsafe impl Send for Encoder {}

impl Encoder {
    fn new() -> Option<Encoder> {
        unsafe {
            let mut err: libc::c_int = 0;
            let st = opus_encoder_create(48000, 2, OPUS_APPLICATION_AUDIO, &mut err);
            if st.is_null() {
                eprintln!("audio: opus_encoder_create failed (err {err})");
                return None;
            }
            opus_encoder_ctl(st, OPUS_SET_BITRATE_REQUEST, 200 * 8 * 100);
            if opus_encoder_ctl(st, OPUS_SET_VBR_NEW, 0) != 0 {
                opus_encoder_ctl(st, OPUS_SET_VBR_OLD, 0);
            }
            Some(Encoder(st))
        }
    }
    /// Encode one interleaved stereo frame; CBR ⇒ exactly 200 B.
    fn encode(&mut self, pcm: &[i16; FRAME_SAMPLES * 2]) -> Option<[u8; OPUS_BYTES]> {
        let mut out = [0u8; OPUS_BYTES];
        let n = unsafe {
            opus_encode(
                self.0,
                pcm.as_ptr(),
                FRAME_SAMPLES as libc::c_int,
                out.as_mut_ptr(),
                OPUS_BYTES as libc::c_int,
            )
        };
        if n != OPUS_BYTES as libc::c_int {
            eprintln!("audio: opus_encode returned {n} (expected {OPUS_BYTES})");
            return None;
        }
        Some(out)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.0) };
    }
}

// ---- CRC-32 (HIDP seed byte + payload, complemented) ------------------------

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

// ---- Reports ---------------------------------------------------------------

/// 0x31 (78 B) enabling audio volumes. Bit 4 of valid_flag0
/// (AllowHeadphoneVolume) is required or the jack stays silent.
/// 0x31 built from arbitrary 47 B state — lets the sink re-assert audio
/// config WITHOUT zeroing motor bytes (constant rumble must survive the
/// periodic unlock).
pub fn state_report(seq: u8, state: &[u8; 47]) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X31];
    r[0] = 0x31;
    r[1] = seq << 4;
    r[2] = 0x10;
    r[3..50].copy_from_slice(state);
    put_crc(&mut r);
    r
}

pub fn volume_report(seq: u8, hp_volume: u8, speaker: bool) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X31];
    r[0] = 0x31;
    r[1] = seq << 4; // kernel: high nibble seq (0..15), low nibble tag 0
    r[2] = 0x10; // BT tag
    let c = &mut r[3..50]; // dualsense_output_report_common
    c[0] = 0x10 // AllowHeadphoneVolume
        | 0x20 // speaker volume
        | 0x40 // mic volume
        | 0x80; // audio control
    c[1] = 0x80; // valid_flag1: audio_control2 enable
    c[4] = hp_volume.min(0x7F); // headphone 0x0..0x7f
    c[5] = if speaker { 0x50 } else { 0x40 }; // speaker
    c[6] = 0x20; // mic 0x0..0x40
    c[7] = 0x00; // audio_control (path sel = 0)
    c[37] = if speaker { 5 } else { 0 }; // audio_control2: speaker preamp gain
    put_crc(&mut r);
    r
}

/// 0x35 (334 B) carrying one 200 B Opus frame.
pub fn audio_report(seq: u8, counter: u8, frame: &[u8; OPUS_BYTES], jack: bool) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X35];
    r[0] = 0x35;
    r[1] = seq << 4;
    r[2] = 0x91; // 0x11 | sized — control sub-packet
    r[3] = 0x07; // len
    r[4] = 0xFE; // flags0: bits 0,1 REQUIRED for pad operation; bits 4-7: allow volumes/audio
    r[9] = counter; // += 2 per report, wraps
    r[11] = if jack { 0x96 } else { 0x93 }; // 0x16 | sized / 0x13 | sized
    r[12] = 0xC8; // 200
    r[13..213].copy_from_slice(frame);
    put_crc(&mut r);
    r
}

/// 0x32 (142 B) carrying control + one 64 B haptics PCM frame, NO Opus —
/// the SAxense-style haptics-only stream (dsneo §3.1: `0x11` + `0x12`,
/// no audio TLV). Hardware-verified shape: dsneo §2.2.2 buzzed the pad
/// with 25 such reports. 57% less airtime than 0x35, 74% less than 0x39.
pub fn haptics_report_0x32(seq: u8, counter: u8, hap: &[u8; 64]) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X32];
    r[0] = 0x32;
    r[1] = seq << 4;
    r[2] = 0x91; // 0x11 | sized — control
    r[3] = 0x07; // len
    r[4] = 0xFE; // flags0: bits 0,1 REQUIRED for pad operation; bits 4-7: allow volumes/audio
    r[9] = counter;
    r[11] = 0x92; // 0x12 | sized — haptics PCM
    r[12] = 0x40; // 64
    r[13..77].copy_from_slice(hap);
    put_crc(&mut r);
    r
}

/// 0x39 (547 B) carrying one 200 B Opus frame — same content as 0x35 but
/// padded to the larger report size.
pub fn audio_report_0x39(seq: u8, counter: u8, frame: &[u8; OPUS_BYTES], jack: bool) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X39];
    r[0] = 0x39;
    r[1] = seq << 4;
    r[2] = 0x91; // 0x11 | sized — control sub-packet
    r[3] = 0x07; // len
    r[4] = 0xFE; // flags0: bits 0,1 REQUIRED for pad operation; bits 4-7: allow volumes/audio
    r[9] = counter;
    r[11] = if jack { 0x96 } else { 0x93 };
    r[12] = 0xC8; // 200
    r[13..213].copy_from_slice(frame);
    put_crc(&mut r);
    r
}

// ---- L2CAP dialect (vds parity, takeover26-29 proven) -----------------------

/// Session-open INIT: 0x32 (142 B) with a FIXED seq 0x10 and a 63 B state
/// TLV (0x90/63). Proven by vdsd on every session open.
pub fn init_report_032(state63: &[u8; 63]) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X32];
    r[0] = 0x32;
    r[1] = 0x10; // FIXED sequence (own family — never shared with 0x31/0x36)
    r[2] = 0x90; // 0x10 | sized — state sub-packet
    r[3] = 63; // len
    r[4..67].copy_from_slice(state63);
    put_crc(&mut r);
    r
}

/// Mic report 0x32 (142 B), own sequence family: opens/closes the pad mic.
/// vdsd sends mic-open before audio starts. `mic_seq` is echoed at [4] and
/// [10]; the caller increments it after each send.
pub fn mic_report_032(mic_seq: u8, active: bool) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X32];
    r[0] = 0x32;
    r[1] = mic_seq << 4;
    r[2] = 0x91; // 0x11 | sized — control
    r[3] = 0x07;
    r[4] = if active { 0xFF } else { 0xFE }; // mic open / close
    r[5] = 64; // audio buffer length ×5 (kBtAudioBufferLength)
    r[6] = 64;
    r[7] = 64;
    r[8] = 64;
    r[9] = 64;
    r[10] = mic_seq;
    r[11] = 0x92; // 0x12 | sized — haptics
    r[12] = 0x40; // 64
    put_crc(&mut r);
    r
}

/// 0x36 (398 B) vds-dialect audio report — THE proven L2CAP format.
///
///   [2..10]   control TLV: 0x91, len 7, 0xFF (audio sections enable),
///             buffer length 64×5, packet counter
///   [11..76]  state TLV: 0x90, len 63, full 63 B state (speaker path etc.)
///   [76..142] haptics TLV: 0x92, len 64, s8 3 kHz interleaved L/R
///   [142..144] speaker block: 0x93 speaker / 0x96 jack (| sized), len 200
///   [144..344] Opus CBR 200 B
///   [394..398] CRC-32 LE
///
/// Sent with a 0xA2 HIDP prefix on the interrupt channel at a flat 10 ms.
pub const REPORT_0X36_BT: usize = 398;

pub fn audio_report_036_bt(
    seq: u8,
    counter: u8,
    state63: &[u8; 63],
    hap: &[u8; 64],
    frame: &[u8; OPUS_BYTES],
    jack: bool,
) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X36_BT];
    r[0] = 0x36;
    r[1] = seq << 4;
    r[2] = 0x91; // 0x11 | sized — control
    r[3] = 0x07;
    r[4] = 0xFF; // audio sections enable (mic included; vdsd always sends this)
    r[5] = 64; // buffer length ×5
    r[6] = 64;
    r[7] = 64;
    r[8] = 64;
    r[9] = 64;
    r[10] = counter; // packet counter, += 1 per report (wraps u8)
    r[11] = 0x90; // 0x10 | sized — state
    r[12] = 63; // len
    r[13..76].copy_from_slice(state63);
    r[76] = 0x92; // 0x12 | sized — haptics PCM
    r[77] = 64;
    r[78..142].copy_from_slice(hap);
    r[142] = if jack { 0x96 } else { 0x93 }; // 0x16 | sized / 0x13 | sized
    r[143] = 200;
    r[144..344].copy_from_slice(frame);
    put_crc(&mut r);
    r
}

// ---- Tone thread -----------------------------------------------------------

pub struct Tone {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Tone {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Spawn the test-tone thread writing to `real` (a dup is taken).
/// Only meaningful on Bluetooth (USB has native audio).
pub fn start(real: &File, cfg: &AudioConfig) -> Tone {
    let stop = Arc::new(AtomicBool::new(false));
    let thr_stop = stop.clone();
    let fd = unsafe { libc::dup(real.as_raw_fd()) };
    let mut file = unsafe { File::from_raw_fd(fd) };
    let jack = !cfg.speaker.unwrap_or(false);
    let hp_volume = cfg.volume.unwrap_or(0x64);
    let handle = thread::spawn(move || {
        let mut enc = match Encoder::new() {
            Some(e) => e,
            None => return,
        };
        eprintln!(
            "audio: test tone → {} (volume 0x{:02x}); restart the service to stop it",
            if jack { "headset jack" } else { "speaker" },
            hp_volume
        );
        // Required volume unlock; re-sent periodically to be safe.
        if let Err(e) = file.write_all(&volume_report(0, hp_volume, !jack)) {
            eprintln!("audio: volume report write failed: {e}");
            return;
        }
        // Pad DAC = 45000 Hz; generate 48 k PCM pitched 48/45 up so it lands true.
        let freq = 440.0f32 * 48000.0 / 45000.0;
        let mut phase = 0.0f32;
        let mut pcm = [0i16; FRAME_SAMPLES * 2];
        let mut seq: u8 = 0;
        let mut counter: u8 = 0xFF;
        let mut next = Instant::now();
        let mut since_volume = 0u32;
        loop {
            if thr_stop.load(Ordering::Relaxed) {
                break;
            }
            for s in 0..FRAME_SAMPLES {
                let v = (phase.sin() * 10000.0) as i16;
                pcm[s * 2] = v;
                pcm[s * 2 + 1] = v;
                phase += 2.0 * std::f32::consts::PI * freq / 48000.0;
                if phase > 2.0 * std::f32::consts::PI * 1000.0 {
                    phase -= 2.0 * std::f32::consts::PI * 1000.0;
                }
            }
            let frame = match enc.encode(&pcm) {
                Some(f) => f,
                None => break,
            };
            let report = audio_report(seq, counter, &frame, jack);
            if let Err(e) = file.write_all(&report) {
                eprintln!("audio: write failed ({e}) — pad gone? stopping tone");
                break;
            }
            seq = (seq + 1) & 0x0F;
            counter = counter.wrapping_add(2);
            since_volume += 1;
            if since_volume >= 256 {
                // ~2.7 s: re-assert the volume unlock (cheap, idempotent)
                let _ = file.write_all(&volume_report(seq, hp_volume, !jack));
                since_volume = 0;
            }
            next += SEND_INTERVAL;
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else {
                next = now; // fell behind (e.g. preemption): resync
            }
        }
        eprintln!("audio: tone thread exited");
    });
    Tone {
        stop,
        handle: Some(handle),
    }
}

use std::os::unix::io::{AsRawFd, FromRawFd};
