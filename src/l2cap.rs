//! Direct-L2CAP transport (self-owned HID session).
//!
//! Proven recipe (takeover26-29, vds parity): bluetoothd runs with
//! `--noplugin=input` (so it keeps SDP + link management but never binds the
//! HID PSMs); we bind PSM 0x11 (control) + 0x13 (interrupt) ourselves and the
//! pad connects INBOUND (PS press from sleep, or auto re-page while awake
//! when the previous owner died). Session open:
//!   1. accept ctrl (0x11), then intr (0x13) within seconds
//!   2. feature GETs [0x43,id] for 0x09/0x20/0x05, matched on 0xA3 replies
//!   3. INIT report 0x32 (seq FIXED 0x10, TLV 0x90/63 + observed 63 B state)
//! The sink writer then owns ALL interrupt-channel output (mic handshake +
//! 0x36 audio dialect, unified sequence nibble).
//!
//! Inputs arrive on the interrupt socket as 0xA1-prefixed HID reports; the
//! proxy strips the prefix and feeds the existing relay path.
//!
//! Privileges: binding PSMs < 0x1001 needs CAP_NET_BIND_SERVICE
//! (setcap cap_net_bind_service+ep on the installed binary).

use std::collections::HashMap;

/// Canonical DualSense HID report descriptor (USB, same report IDs for BT).
/// Embedded here so L2CAP mode can fabricate a PadInfo without a prior hidraw
/// session's bt-profile.bin cache.  UHID_CREATE2 requires a non-empty rdesc;
/// the kernel parses it to set up the /dev/hidrawN node.
/// The real USB 289 B descriptor. Used for the USB-chimera view
/// (force_bus = "usb"): the virtual pad claims bus=USB and serves this
/// descriptor, so games see exactly the cabled pad's report shapes.
pub const DS5_HID_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xff, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3b, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0f, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0f, 0x81, 0x02, 0x06,
    0x00, 0xff, 0x09, 0x21, 0x95, 0x0d, 0x81, 0x02, 0x06, 0x00, 0xff, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xff, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x2f, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xb1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2f, 0xb1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xb1, 0x02, 0x85, 0x0a, 0x09, 0x25, 0x95, 0x1a, 0xb1, 0x02,
    0x85, 0x0b, 0x09, 0x41, 0x95, 0x29, 0xb1, 0x02, 0x85, 0x0c, 0x09, 0x42, 0x95, 0x29, 0xb1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xb1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x82, 0x09, 0x2a, 0x95, 0x09, 0xb1, 0x02,
    0x85, 0x83, 0x09, 0x2b, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x84, 0x09, 0x2c, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0x85, 0x09, 0x2d, 0x95, 0x02, 0xb1, 0x02, 0x85, 0xa0, 0x09, 0x2e, 0x95, 0x01, 0xb1, 0x02,
    0x85, 0xe0, 0x09, 0x2f, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf0, 0x09, 0x30, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0xf1, 0x09, 0x31, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf2, 0x09, 0x32, 0x95, 0x0f, 0xb1, 0x02,
    0x85, 0xf4, 0x09, 0x35, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf5, 0x09, 0x36, 0x95, 0x03, 0xb1, 0x02,
    0xc0,
];

/// DualSense Bluetooth HID report descriptor (279 B) — what a real pad
/// serves over the BT control channel. BT input is 0x31/78B-framed and
/// outputs use report ids 0x31..0x39; games can tell transports apart by
/// this shape, so L2CAP sessions must present it natively.
/// Provenance block: native BT descriptor (also docs/ds5_bt_rdesc.hex); the
/// vendor-output half lives on inside the merged descriptor.
#[allow(dead_code)]
const DS5_BT_HID_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0x09, 0x39, 0x15, 0x00, 0x25,
    0x07, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0E, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0E, 0x81, 0x02,
    0x75, 0x06, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01, 0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF,
    0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
    0x08, 0x95, 0x4D, 0x85, 0x31, 0x09, 0x31, 0x91, 0x02, 0x09, 0x3B, 0x81, 0x02, 0x85, 0x32, 0x09,
    0x32, 0x95, 0x8D, 0x91, 0x02, 0x85, 0x33, 0x09, 0x33, 0x95, 0xCD, 0x91, 0x02, 0x85, 0x34, 0x09,
    0x34, 0x96, 0x0D, 0x01, 0x91, 0x02, 0x85, 0x35, 0x09, 0x35, 0x96, 0x4D, 0x01, 0x91, 0x02, 0x85,
    0x36, 0x09, 0x36, 0x96, 0x8D, 0x01, 0x91, 0x02, 0x85, 0x37, 0x09, 0x37, 0x96, 0xCD, 0x01, 0x91,
    0x02, 0x85, 0x38, 0x09, 0x38, 0x96, 0x0D, 0x02, 0x91, 0x02, 0x85, 0x39, 0x09, 0x39, 0x96, 0x22,
    0x02, 0x91, 0x02, 0x06, 0x80, 0xFF, 0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xB1, 0x02, 0x85, 0x08,
    0x09, 0x34, 0x95, 0x2F, 0xB1, 0x02, 0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xB1, 0x02, 0x85, 0x20,
    0x09, 0x26, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x22, 0x09, 0x40, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x80,
    0x09, 0x28, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x81, 0x09, 0x29, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x82,
    0x09, 0x2A, 0x95, 0x09, 0xB1, 0x02, 0x85, 0x83, 0x09, 0x2B, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF1,
    0x09, 0x31, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2, 0x09, 0x32, 0x95, 0x0F, 0xB1, 0x02, 0x85, 0xF0,
    0x09, 0x30, 0x95, 0x3F, 0xB1, 0x02, 0xC0,
];

/// DualShock 4 Bluetooth HID report descriptor (418 B), extracted from the
/// kernel's hid-sony.c `dualshock4_bt_rdesc[]` (v4.6-v4.11, since removed —
/// hid-playstation parses genuine descriptors without fixups).
pub const DS4_BT_RDESC: &[u8] = include_bytes!("../docs/ds4_bt_rdesc.bin");

/// Merged descriptor: USB rdesc + BT vendor block (0x31..0x39 reports) so the
/// pad declares BOTH USB-style usages (input 0x21/0x22, output usage 0x23 report
/// 0x02, features) AND the BT report family the kernel needs for raw 0x31
/// passthrough input/output on bus=BLUETOOTH.
#[allow(dead_code)] // superseded: plain BT descriptor restores natural libScePad classification
pub const DS5_HID_REPORT_DESCRIPTOR_MERGED: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xff, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3b, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0f, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0f, 0x81, 0x02, 0x06,
    0x00, 0xff, 0x09, 0x21, 0x95, 0x0d, 0x81, 0x02, 0x06, 0x00, 0xff, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xff, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x3f, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xb1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2f, 0xb1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xb1, 0x02, 0x85, 0x0a, 0x09, 0x25, 0x95, 0x1a, 0xb1, 0x02,
    0x85, 0x0b, 0x09, 0x41, 0x95, 0x29, 0xb1, 0x02, 0x85, 0x0c, 0x09, 0x42, 0x95, 0x29, 0xb1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xb1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x82, 0x09, 0x2a, 0x95, 0x09, 0xb1, 0x02,
    0x85, 0x83, 0x09, 0x2b, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x84, 0x09, 0x2c, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0x85, 0x09, 0x2d, 0x95, 0x02, 0xb1, 0x02, 0x85, 0xa0, 0x09, 0x2e, 0x95, 0x01, 0xb1, 0x02,
    0x85, 0xe0, 0x09, 0x2f, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf0, 0x09, 0x30, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0xf1, 0x09, 0x31, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf2, 0x09, 0x32, 0x95, 0x0f, 0xb1, 0x02,
    0x85, 0xf4, 0x09, 0x35, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf5, 0x09, 0x36, 0x95, 0x03, 0xb1, 0x02,
    0xc0, 0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x06, 0x00, 0xff, 0x15, 0x00, 0x26, 0xff, 0x00,
    0x75, 0x08, 0x95, 0x4d, 0x85, 0x31, 0x09, 0x31, 0x91, 0x02, 0x09, 0x3b, 0x81, 0x02, 0x85, 0x32,
    0x09, 0x32, 0x95, 0x8d, 0x91, 0x02, 0x85, 0x33, 0x09, 0x33, 0x95, 0xcd, 0x91, 0x02, 0x85, 0x34,
    0x09, 0x34, 0x96, 0x0d, 0x01, 0x91, 0x02, 0x85, 0x35, 0x09, 0x35, 0x96, 0x4d, 0x01, 0x91, 0x02,
    0x85, 0x36, 0x09, 0x36, 0x96, 0x8d, 0x01, 0x91, 0x02, 0x85, 0x37, 0x09, 0x37, 0x96, 0xcd, 0x01,
    0x91, 0x02, 0x85, 0x38, 0x09, 0x38, 0x96, 0x0d, 0x02, 0x91, 0x02, 0x85, 0x39, 0x09, 0x39, 0x96,
    0x22, 0x02, 0x91, 0x02, 0x06, 0x80, 0xff, 0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xb1, 0x02, 0x85,
    0x08, 0x09, 0x34, 0x95, 0x2f, 0xb1, 0x02, 0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xb1, 0x02, 0x85,
    0x20, 0x09, 0x26, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x22, 0x09, 0x40, 0x95, 0x3f, 0xb1, 0x02, 0x85,
    0x80, 0x09, 0x28, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x81, 0x09, 0x29, 0x95, 0x3f, 0xb1, 0x02, 0x85,
    0x82, 0x09, 0x2a, 0x95, 0x09, 0xb1, 0x02, 0x85, 0x83, 0x09, 0x2b, 0x95, 0x3f, 0xb1, 0x02, 0x85,
    0xf1, 0x09, 0x31, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf2, 0x09, 0x32, 0x95, 0x0f, 0xb1, 0x02, 0x85,
    0xf0, 0x09, 0x30, 0x95, 0x3f, 0xb1, 0x02, 0xc0,
];

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::audio;
use crate::hid::{PadInfo, Transport};

const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_L2CAP: libc::c_int = 0;
const PSM_CTRL: u16 = 0x11;
const PSM_INTR: u16 = 0x13;

/// vds-parity pristine initial DsState (kInitialSetStateData, 47 B, then
/// zeros to 63): session-INIT 0x32 payload and the tracked state63 the sink
/// starts from (kernel output reports then overlay its first 47 B).
/// OBSOLETE BT_STATE_OBS (on-air v27 capture with audio active) replaced:
/// vds always starts sessions from this exact state.
pub const BT_STATE_INIT: [u8; 63] = {
    let mut s = [0u8; 63];
    s[0] = 0xFD; // valid_flag0: hp+speaker+mic volume + audio-control enables
    s[1] = 0xF7; // valid_flag1: LED / power-save / audio-control2 enables
    s[4] = 0x7F; // headphone volume
    s[5] = 0x64; // speaker volume
    s[6] = 0x08; // mic volume
    s[7] = 0x09; // audio control (path 0x00 = headphones)
    s[9] = 0x0F; // power-save control
    s[37] = 0x01; // audio_control2 (preamp 1)
    s[38] = 0x07;
    s[41] = 0x02;
    s[42] = 0x01;
    s[44] = 0xFF;
    s[45] = 0xD7;
    s
};

/// Mic-state 0x31 head (v28 handshake, vdsd "mic-active" state:
/// AllowMicVolume|AllowAudioControl + internal mic). Audio-control byte
/// [7] carries the OUTPUT PATH (mask 0x30): the pad re-announces a
/// headset plug (HP-detect bursts) until the host answers with the
/// matching path here — 0x39 speaker / 0x09 headphone jack (vds parity).
pub fn mic_state(jack_path: bool) -> [u8; 47] {
    let mut s = [0u8; 47];
    s[0] = 0xC0; // AllowMicVolume | AllowAudioControl
    s[1] = 0x83; // AllowMuteLight | AllowAudioMute | AllowAudioControl2
    s[6] = 0x08; // mic volume
    s[7] = if jack_path { 0x09 } else { 0x39 }; // audio control: jack / speaker path
    s[9] = 0x0F; // power-save control
    s[37] = 0x01; // audio_control2 (vds sets it whenever flag1 bit7 is set)
    s
}

#[repr(C)]
struct SockaddrL2 {
    family: u16,
    psm: u16,
    bdaddr: [u8; 6],
    cid: u16,
    addr_type: u8,
}

fn l2cap_listener(psm: u16) -> std::io::Result<File> {
    let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET, BTPROTO_L2CAP) };
    // vds parity: SO_REUSEADDR so a rebind right after a dropped session
    // cannot lose the race with the kernel releasing the PSM.
    let reuse: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &reuse as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sa = SockaddrL2 {
        family: AF_BLUETOOTH as u16,
        psm: psm.to_le(),
        bdaddr: [0; 6], // BDADDR_ANY
        cid: 0,
        addr_type: 0,
    };
    let ret = unsafe {
        libc::bind(
            fd,
            &sa as *const SockaddrL2 as *const libc::sockaddr,
            std::mem::size_of::<SockaddrL2>() as libc::socklen_t,
        )
    };
    if ret != 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    if unsafe { libc::listen(fd, 1) } != 0 {
        let e = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn accept_with_log(listener: &File, what: &str, timeout: Duration) -> Option<File> {
    let deadline = Instant::now() + timeout;
    let fd = listener.as_raw_fd_ext();
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 250) };
        if ret < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if ret < 0 {
            eprintln!("l2cap: poll {what}: {}", std::io::Error::last_os_error());
            return None;
        }
        if ret > 0 && pfd.revents & libc::POLLIN != 0 {
            let cfd = unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) };
            if cfd < 0 {
                let e = std::io::Error::last_os_error();
                eprintln!("l2cap: accept {what}: {e}");
                return None;
            }
            return Some(unsafe { File::from_raw_fd(cfd) });
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

trait AsRawFdExt {
    fn as_raw_fd_ext(&self) -> libc::c_int;
}
impl AsRawFdExt for File {
    fn as_raw_fd_ext(&self) -> libc::c_int {
        std::os::unix::io::AsRawFd::as_raw_fd(self)
    }
}

/// Read one datagram from `f` with a timeout.
fn read_frame(f: &mut File, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let fd = f.as_raw_fd_ext();
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, 250) };
        if ret < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if ret < 0 || pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return None;
        }
        if ret > 0 && pfd.revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 1024];
            return match f.read(&mut buf) {
                Ok(n) if n > 0 => Some(buf[..n].to_vec()),
                _ => None,
            };
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

pub struct Session {
    /// Control channel (PSM 0x11). MUST stay open for the whole session —
    /// vds keeps both fds in its epoll set until teardown; closing ctrl
    /// tears the pad's HID session down (the short-lived-session bug).
    /// The relay loop polls and drains it (0xA3 replies refresh the cache).
    pub ctrl: File,
    pub intr: File,
    pub features: HashMap<u8, Vec<u8>>,
    pub info: PadInfo,
}

impl Session {
    /// Feature GET on the control channel: [0x43,id], match 0xA3 reply.
    /// Cached value is the report as hidraw would return it ([0]=id).
    fn get_feature(ctrl: &mut File, id: u8) -> Option<Vec<u8>> {
        for _ in 0..3 {
            if ctrl.write_all(&[0x43, id]).is_err() {
                return None;
            }
            // skip non-matching frames (handshake acks etc.)
            for _ in 0..16 {
                let frame = read_frame(ctrl, Duration::from_secs(2))?;
                if frame.len() >= 2 && frame[0] == 0xA3 && frame[1] == id {
                    return Some(frame[1..].to_vec());
                }
                // 1-byte frame = HIDP HANDSHAKE: the device answered the GET
                // itself (0x00 ok-no-data / 0x01..0x0f error, e.g. 0x02
                // unsupported report id). No data report will follow — fail
                // this id NOW instead of burning 16×2 s timeouts (a non-
                // DualSense pad answering handshakes would otherwise stall
                // the accept loop for minutes).
                if frame.len() == 1 {
                    eprintln!(
                        "l2cap: feature 0x{id:02x}: handshake 0x{:02x} — not supported",
                        frame[0]
                    );
                    return None;
                }
            }
        }
        None
    }
}

/// One-shot feature fetch for the proxy's GET miss path (probe time only —
/// blocks the relay up to ~1.5 s). Returns the cache format (id + payload
/// + CRC), same as get_feature and the relay's ctrl-drain cache inserts.
pub fn fetch_feature_once(ctrl: &mut File, id: u8) -> Option<Vec<u8>> {
    ctrl.write_all(&[0x43, id]).ok()?;
    for _ in 0..8 {
        let frame = read_frame(ctrl, Duration::from_millis(1500))?;
        if frame.len() >= 2 && frame[0] == 0xA3 && frame[1] == id {
            return Some(frame[1..].to_vec());
        }
    }
    None
}

/// Bind listeners and wait for the pad to connect inbound. Blocks until a
/// full session (ctrl+intr+features+INIT) is up. `poll_exit` is polled
/// between waits so the caller can shut down cleanly.
pub fn open(poll_exit: &dyn Fn() -> bool) -> Result<Session, String> {
    let (ctrl_l, intr_l) = match (l2cap_listener(PSM_CTRL), l2cap_listener(PSM_INTR)) {
        (Ok(c), Ok(i)) => (c, i),
        (Err(e), _) | (_, Err(e)) => {
            return Err(match e.raw_os_error() {
                Some(libc::EACCES) => format!(
                    "l2cap: bind PSM 0x11/0x13: EACCES — the binary needs\n\
                     sudo setcap cap_net_bind_service+ep <mdrv-ds path>"
                ),
                Some(libc::EADDRINUSE) => format!(
                    "l2cap: bind PSM 0x11/0x13: EADDRINUSE — bluetoothd still owns the HID\n\
                     PSMs. Run: sudo scripts/bt-input-off.sh  (installs --noplugin=input\n\
                     override and restarts bluetoothd), then restart mdrv-ds"
                ),
                _ => format!("l2cap: bind: {e}"),
            });
        }
    };
    eprintln!("l2cap: listeners ready (PSM 0x11/0x13) — waiting for pad (press PS)…");
    let mut ctrl = loop {
        if poll_exit() {
            return Err("exit".into());
        }
        match accept_with_log(&ctrl_l, "ctrl (0x11)", Duration::from_millis(250)) {
            Some(c) => break c,
            None => continue, // 250 ms tick: re-loop, re-check exit/other conditions
        }
    };
    eprintln!("l2cap: control channel accepted");
    let mut intr = match accept_with_log(&intr_l, "intr (0x13)", Duration::from_secs(15)) {
        Some(i) => i,
        None => {
            return Err("l2cap: pad opened control but no interrupt channel within 15 s".into());
        }
    };
    eprintln!("l2cap: interrupt channel accepted");

    // Feature GETs (vds parity): 0x09 serial, 0x20 firmware, 0x05 calibration.
    // 0x09 is identity-critical (MAC for the virtual pad): a pad that does
    // not answer it is not a DualSense — close the session instead of
    // fabricating a phantom virtual pad (DS4 and other pads pair fine but
    // speak a different report protocol; vds is DualSense-only too).
    let mut features = HashMap::new();
    let mut ds5 = true;
    for id in [0x09u8, 0x20, 0x05] {
        match Session::get_feature(&mut ctrl, id) {
            Some(rep) => {
                eprintln!("l2cap: feature 0x{id:02x}: {} B", rep.len());
                features.insert(id, rep);
            }
            None if id == 0x09 => {
                // No serial feature: try the DualShock 4 probe path (fw info
                // 0xA3 + calibration 0x05). Feature GETs also flip the pad
                // from minimal 0x01 frames to full 0x11 streaming.
                for did in [0xA3u8, 0x05] {
                    match Session::get_feature(&mut ctrl, did) {
                        Some(rep) => {
                            eprintln!("l2cap: ds4 feature 0x{did:02x}: {} B", rep.len());
                            features.insert(did, rep);
                        }
                        None => eprintln!("l2cap: ds4 feature 0x{did:02x}: no reply"),
                    }
                }
                if !features.contains_key(&0xA3) && !features.contains_key(&0x05) {
                    return Err(
                        "l2cap: pad answered neither DualSense 0x09 nor DS4 0xA3/0x05 — closing"
                            .into(),
                    );
                }
                ds5 = false;
                break;
            }
            None => eprintln!("l2cap: feature 0x{id:02x}: no reply (continuing)"),
        }
    }

    if !ds5 {
        return finish_ds4_session(ctrl, intr, features);
    }

    // INIT (session-open state, seq FIXED 0x10) on the interrupt channel.
    let init = audio::init_report_032(&BT_STATE_INIT);
    let mut wire = Vec::with_capacity(init.len() + 1);
    wire.push(0xA2);
    wire.extend_from_slice(&init);
    if let Err(e) = intr.write_all(&wire) {
        return Err(format!("l2cap: INIT write failed: {e}"));
    }
    eprintln!("l2cap: session open (INIT sent)");

    let uniq = features
        .get(&0x09)
        .and_then(|f09| {
            (f09.len() >= 7).then(|| {
                format!(
                    "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    f09[6], f09[5], f09[4], f09[3], f09[2], f09[1]
                )
            })
        })
        .unwrap_or_else(|| "00:00:00:00:00:00".into());
    // Plain BT descriptor: libScePad-class games classify the pad by HID-caps probe
    // (merged rdesc made every pad look USB → wrong report parsing on BT). FF16's
    // haptics path is rdesc-independent since the shopper gate patch.
    let rdesc = DS5_BT_HID_REPORT_DESCRIPTOR.to_vec();
    let info = PadInfo {
        path: PathBuf::from("/dev/l2cap-psm13"),
        uniq,
        product: 0x0ce6,
        // Real pads expose USB bcdDevice 0x0100.
        fw_version: 0x0100,
        hw_version: 0,
        rdesc,
        transport: Transport::Bluetooth,
    };
    Ok(Session {
        ctrl,
        intr,
        features,
        info,
    })
}

/// Build a DualShock 4 session: MAC via getpeername on the control socket,
/// identity/fw from the cached feature replies, BT rdesc. No audio INIT —
/// the DS4 has no L2CAP audio path (SBC-over-HID is PS4-proprietary).
fn finish_ds4_session(
    ctrl: File,
    intr: File,
    mut features: HashMap<u8, Vec<u8>>,
) -> Result<Session, String> {
    // Peer Bluetooth address (the pad's own MAC) for the virtual-pad uniq —
    // hid-playstation requires a valid 17-char MAC string there on BT.
    let addr = peer_bdaddr(&ctrl);
    let uniq = match &addr {
        Some(a) => format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            a[5], a[4], a[3], a[2], a[1], a[0]
        ),
        None => "00:00:00:00:00:00".into(),
    };
    let le16 = |f: Option<&Vec<u8>>, at: usize| -> u32 {
        f.filter(|r| r.len() >= at + 2)
            .map(|r| u16::from_le_bytes([r[at], r[at + 1]]) as u32)
            .unwrap_or(0x0100)
    };
    let fw = le16(features.get(&0xA3), 41);
    let hw = le16(features.get(&0xA3), 35);
    eprintln!("l2cap: DualShock 4 session open (uniq={uniq} fw=0x{fw:04x} hw=0x{hw:04x})");
    let info = PadInfo {
        path: PathBuf::from("/dev/l2cap-psm13"),
        uniq,
        product: 0x05c4,
        fw_version: if fw == 0 { 0x0100 } else { fw },
        hw_version: hw,
        rdesc: DS4_BT_RDESC.to_vec(),
        transport: Transport::Bluetooth,
    };
    Ok(Session {
        ctrl,
        intr,
        features: std::mem::take(&mut features),
        info,
    })
}

/// Read the peer's Bluetooth address from an L2CAP socket.
fn peer_bdaddr(sock: &File) -> Option<[u8; 6]> {
    use std::os::fd::AsRawFd;
    let mut sa = SockaddrL2 {
        family: AF_BLUETOOTH as u16,
        psm: 0,
        bdaddr: [0; 6],
        cid: 0,
        addr_type: 0,
    };
    let mut len = std::mem::size_of::<SockaddrL2>() as u32;
    let rc = unsafe {
        libc::getpeername(
            sock.as_raw_fd(),
            &mut sa as *mut SockaddrL2 as *mut libc::sockaddr,
            &mut len,
        )
    };
    (rc == 0).then(|| sa.bdaddr)
}
