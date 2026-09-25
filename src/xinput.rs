//! XInput pad emulation: a SECOND virtual pad (Xbox 360 wired, 045e:028e)
//! fed by the live DualSense input stream, so XInput-only games work under
//! wine/Proton without Steam Input. SDL (bundled with Proton) claims this
//! VID/PID over /dev/hidraw via its HIDAPI driver and parses the classic
//! 20-byte-ish xpad packet our 14-byte HID report mirrors byte-for-byte.
//!
//! Strictly additive and default-OFF: a separate holder instance owns the
//! uhid device (see ipc/holder), the DS-native path is untouched, and with
//! `xinput` disabled nothing here runs at all.

use std::path::PathBuf;

use crate::ipc;
use crate::proxy::InputLayout;
use crate::uhid::BUS_USB;

/// Xbox 360 wired HID report descriptor. The real device is vendor-class
/// USB (no on-wire descriptor); this is the free60/xbox360hid transcription
/// used by the ecosystem, whose input layout is BIT-IDENTICAL to the xpad
/// packet (data[0..14]): 2-byte counted-buffer header, then dpad/buttons,
/// triggers, sticks. A minimal 8-byte vendor output report is appended
/// inside the application collection so hidraw rumble writes (SDL sends
/// {0x00,0x08,...}) have a declared home.
pub const XINPUT_RDESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x05, 0x01, 0x09, 0x3a, 0xa1, 0x02, 0x75, 0x08, 0x95, 0x02,
    0x05, 0x01, 0x09, 0x3f, 0x09, 0x3b, 0x81, 0x01, 0x75, 0x01, 0x15, 0x00, 0x25, 0x01, 0x35, 0x00,
    0x45, 0x01, 0x95, 0x04, 0x05, 0x09, 0x19, 0x0c, 0x29, 0x0f, 0x81, 0x02, 0x75, 0x01, 0x15, 0x00,
    0x25, 0x01, 0x35, 0x00, 0x45, 0x01, 0x95, 0x04, 0x05, 0x09, 0x09, 0x09, 0x09, 0x0a, 0x09, 0x07,
    0x09, 0x08, 0x81, 0x02, 0x75, 0x01, 0x15, 0x00, 0x25, 0x01, 0x35, 0x00, 0x45, 0x01, 0x95, 0x03,
    0x05, 0x09, 0x09, 0x05, 0x09, 0x06, 0x09, 0x0b, 0x81, 0x02, 0x75, 0x01, 0x95, 0x01, 0x81, 0x01,
    0x75, 0x01, 0x15, 0x00, 0x25, 0x01, 0x35, 0x00, 0x45, 0x01, 0x95, 0x04, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x04, 0x81, 0x02, 0x75, 0x08, 0x15, 0x00, 0x26, 0xff, 0x00, 0x35, 0x00, 0x46, 0xff, 0x00,
    0x95, 0x02, 0x05, 0x01, 0x09, 0x32, 0x09, 0x35, 0x81, 0x02, 0x75, 0x10, 0x16, 0x00, 0x80, 0x26,
    0xff, 0x7f, 0x36, 0x00, 0x80, 0x46, 0xff, 0x7f, 0x05, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x95, 0x02,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x81, 0x02, 0xc0, 0x05, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x95,
    0x02, 0x05, 0x01, 0x09, 0x33, 0x09, 0x34, 0x81, 0x02, 0xc0, 0xc0, 0x06, 0x00, 0xff, 0x09, 0x01,
    0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x08, 0x91, 0x02, 0xc0,
];

/// Live override file (mirrors the speaker pattern): "on"/"off" wins over
/// the config value; absent/invalid falls back to config.
pub fn override_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("mdrv-ds-xinput")
}

pub fn effective(cfg: bool) -> bool {
    match std::fs::read_to_string(override_path()) {
        Ok(s) => s.trim() == "on",
        Err(_) => cfg,
    }
}

/// XInput pad identity: Xbox 360 wired (045e:028e), bcdDevice 0x0114. The
/// uniq derives from the DS virtual pad's MAC with one more bit flipped
/// (net ^0x06 vs the real pad) so the two virtuals never collide.
pub fn create_msg(ds_virtual_mac: &[u8; 6]) -> ipc::CreateMsg {
    let mut mac = *ds_virtual_mac;
    mac[0] ^= 0x04;
    let uniq = mac
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    ipc::CreateMsg {
        bus: BUS_USB,
        vendor: 0x045e,
        product: 0x028e,
        version: 0x0114,
        name: "Xbox 360 Controller".to_string(),
        uniq,
        rdesc: XINPUT_RDESC.to_vec(),
    }
}

/// Neutral 14-byte report: header + all-zero fields (sticks center at 0 in
/// the signed 16-bit convention, hat released, triggers 0).
pub fn neutral() -> [u8; 14] {
    let mut r = [0u8; 14];
    r[1] = 0x14; // xpad packet total size (counted-buffer header)
    r
}

/// DualSense report (any transport shape — offsets via `l`) → 14-byte xpad
/// packet. Mirrors kernel xpad.c bit-for-bit:
///   [2] dpad U/D/L/R | start | back | L3 | R3
///   [3] LB | RB | guide | A | B | X | Y
///   [4] LT, [5] RT (u8), [6..14] LX/LY/RX/RY i16 LE (Y wire-inverted)
pub fn translate(report: &[u8], l: &InputLayout) -> [u8; 14] {
    let get = |i: usize| report.get(i).copied().unwrap_or(0);
    let b0 = get(l.btn0);
    let b1 = get(l.btn0 + 1);
    let b2 = get(l.btn0 + 2);
    let mut r = neutral();
    // hat compass (low nibble) → dpad bits, diagonals combine
    r[2] |= match b0 & 0x0f {
        0 => 0x01, // N
        1 => 0x09, // NE
        2 => 0x08, // E
        3 => 0x0a, // SE
        4 => 0x02, // S
        5 => 0x06, // SW
        6 => 0x04, // W
        7 => 0x05, // NW
        _ => 0x00, // released (0x08) / fault
    };
    if b1 & 0x20 != 0 {
        r[2] |= 0x10;
    } // options → start
    if b1 & 0x10 != 0 {
        r[2] |= 0x20;
    } // create/share → back
    if b2 & 0x02 != 0 {
        r[2] |= 0x20;
    } // touchpad click → back (same as share)
    if b1 & 0x40 != 0 {
        r[2] |= 0x40;
    } // L3
    if b1 & 0x80 != 0 {
        r[2] |= 0x80;
    } // R3
    if b1 & 0x01 != 0 {
        r[3] |= 0x01;
    } // L1 → LB
    if b1 & 0x02 != 0 {
        r[3] |= 0x02;
    } // R1 → RB
    if b2 & 0x01 != 0 {
        r[3] |= 0x04;
    } // PS → guide
    if b0 & 0x20 != 0 {
        r[3] |= 0x10;
    } // cross → A
    if b0 & 0x40 != 0 {
        r[3] |= 0x20;
    } // circle → B
    if b0 & 0x10 != 0 {
        r[3] |= 0x40;
    } // square → X
    if b0 & 0x80 != 0 {
        r[3] |= 0x80;
    } // triangle → Y
    r[4] = get(l.trig0); // L2 → LT
    r[5] = get(l.trig0 + 1); // R2 → RT
    let fwd = |v: u8| -> i16 { (((v as i32) - 128) * 257).clamp(-32768, 32767) as i16 };
    // stick up = 0x00 on the wire; xpad convention is up = +32767
    let inv = |v: u8| -> i16 { (((128 - v as i32) * 257).clamp(-32768, 32767)) as i16 };
    r[6..8].copy_from_slice(&fwd(get(l.stick0)).to_le_bytes()); // LX
    r[8..10].copy_from_slice(&inv(get(l.stick0 + 1)).to_le_bytes()); // LY
    r[10..12].copy_from_slice(&fwd(get(l.stick0 + 2)).to_le_bytes()); // RX
    r[12..14].copy_from_slice(&inv(get(l.stick0 + 3)).to_le_bytes()); // RY
    r
}

/// Decode an 8-byte hidraw output write into (strong, weak) motor bytes.
/// SDL/Proton writes {0x00,0x08,0,[strong],[weak],0,0,0}; kernel xpad tools
/// write {0x00,0x08,[strong],[weak],0,0,0,0}. A nonzero [2] marks the
/// kernel form; anything else parses as the SDL form. Returns None for
/// non-rumble payloads (e.g. LED packet {0x01,0x03,mode}).
pub fn decode_rumble(out: &[u8]) -> Option<(u8, u8)> {
    if out.len() < 4 || out[0] != 0x00 || out[1] != 0x08 {
        return None;
    }
    if out[2] != 0 {
        Some((out[2], out[3])) // kernel xpad form
    } else {
        Some((out[3], *out.get(4).unwrap_or(&0))) // SDL form
    }
}

/// 48-byte USB 0x02 DS output report carrying ONLY rumble (flag0 0x03,
/// motor bytes; every other section's enable bits are zero → untouched).
/// Used for direct hidraw writes on USB-transport sessions.
pub fn rumble_report(strong: u8, weak: u8) -> [u8; 48] {
    let mut r = [0u8; 48];
    r[0] = 0x02;
    r[1] = 0x03; // valid_flag0: compatible-vibration motors
    r[3] = weak; // [2]=motor_right(weak), [3]=motor_left(strong)
    r[4] = strong;
    r
}
