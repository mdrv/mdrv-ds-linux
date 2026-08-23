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
    r[4] = 0xFE; // audio enable mask (mic off)
    r[9] = counter; // += 2 per report, wraps
    r[11] = if jack { 0x96 } else { 0x93 }; // 0x16 | sized / 0x13 | sized
    r[12] = 0xC8; // 200
    r[13..213].copy_from_slice(frame);
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
    r[4] = 0xFE; // audio enable mask (mic off)
    r[9] = counter;
    r[11] = if jack { 0x96 } else { 0x93 };
    r[12] = 0xC8; // 200
    r[13..213].copy_from_slice(frame);
    put_crc(&mut r);
    r
}

/// 0x39 (547 B) carrying two 200 B Opus frames (twoFrames mode).
///
/// Layout (dsneo §3 + DS5Dongle header bit 6):
///   [0]=0x39 [1]=seq<<4
///   [2..10]  control TLV: 0x91, len=7, flag0|counter|zeros
///   [11..140] haptics TLV: 0xD2 (0x12|twoFrames|sized), len=128, 2×64B
///   [141..542] speaker TLV: 0xD6 (0x16|twoFrames|sized), len=200, 2×200B
///   [543..546] CRC-32 LE
///
/// When the `twoFrames` header bit is set the pad reads 2× the length byte
/// for that TLV, giving us two 10 ms Opus frames per report — halving the
/// report rate from 94 to 47/s and dramatically reducing ACL packet count.
pub fn audio_report_0x39_dual(
    seq: u8,
    counter: u8,
    hap1: &[u8; 64],
    hap2: &[u8; 64],
    frame1: &[u8; OPUS_BYTES],
    frame2: &[u8; OPUS_BYTES],
    jack: bool,
) -> Vec<u8> {
    let mut r = vec![0u8; REPORT_0X39];
    r[0] = 0x39;
    r[1] = seq << 4;
    // Control TLV
    r[2] = 0x91; // 0x11 | sized
    r[3] = 0x07; // len
    r[4] = 0xFE; // audio enable mask (mic off)
    r[9] = counter;
    // Haptics TLV — twoFrames (bit 6) + sized (bit 7)
    r[11] = 0xD2; // 0x12 | 0x40 | 0x80
    r[12] = 128; // 2 × 64
    r[13..77].copy_from_slice(hap1);
    r[77..141].copy_from_slice(hap2);
    // Speaker TLV — twoFrames + sized
    r[141] = if jack { 0xD6 } else { 0xD3 }; // 0x16|0xC0 / 0x13|0xC0
    r[142] = 200; // per-frame; pad reads 2 × len when twoFrames set
    r[143..343].copy_from_slice(frame1);
    r[343..543].copy_from_slice(frame2);
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
