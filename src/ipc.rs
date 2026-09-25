//! Unix-socket IPC between the proxy and the virtual-pad holder daemon.
//!
//! Frame format: `[u32 LE len][u8 tag][payload…]` where len counts the tag
//! byte plus payload. Small on purpose — input reports at 250 Hz, feature
//! round-trips at init, output reports on demand.
//!
//! Tags (proxy → holder):
//!   C create/reuse the virtual pad      I input report → UHID_INPUT2
//!   N store neutral report (empty clears)  G GET_REPORT reply
//!   A SET_REPORT reply
//! Tags (holder → proxy):
//!   c create ack (0=created 1=reused 2=error+msg)
//!   O output report payload            Q GET_REPORT request (id, rnum)
//!   T SET_REPORT request (id, rnum, data)  X notice/log line

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

pub const TAG_CREATE: u8 = b'C';
pub const TAG_INPUT: u8 = b'I';
pub const TAG_NEUTRAL: u8 = b'N';
pub const TAG_GET_REPLY: u8 = b'G';
pub const TAG_SET_ACK: u8 = b'A';
/// proxy → holder: destroy the virtual pad (XInput instance teardown on
/// live disable; the default DS holder never receives this today) — the
/// holder answers nothing and resets its create key/neutral state.
pub const TAG_DESTROY: u8 = b'D';
pub const TAG_CREATE_ACK: u8 = b'c';
pub const TAG_OUTPUT: u8 = b'O';
pub const TAG_GET_REQ: u8 = b'Q';
pub const TAG_SET_REQ: u8 = b'T';
pub const TAG_NOTICE: u8 = b'X';
const MAX_FRAME: usize = 16384;

/// Instance-aware socket path: "" is the default DualSense holder,
/// "xi" is the second (XInput) holder — separate socket, separate pad,
/// zero interaction with the default instance.
pub fn sock_path_for(instance: &str) -> PathBuf {
    let name = if instance.is_empty() {
        "mdrv-ds.sock"
    } else {
        "mdrv-ds-xi.sock"
    };
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(name)
}

pub fn send<W: Write>(w: &mut W, tag: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.extend_from_slice(&((1 + payload.len()) as u32).to_le_bytes());
    frame.push(tag);
    frame.extend_from_slice(payload);
    w.write_all(&frame)
}

pub fn recv<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut lenb = [0u8; 4];
    r.read_exact(&mut lenb)?;
    let len = u32::from_le_bytes(lenb) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad frame length",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok((buf[0], buf[1..].to_vec()))
}

pub fn connect_for(instance: &str) -> io::Result<UnixStream> {
    UnixStream::connect(sock_path_for(instance))
}

/// Connect to the holder, spawning one (detached) if the socket is down.
/// The systemd `mdrv-ds-holder.service` unit is the canonical holder; the
/// spawn fallback covers CLI/dev runs of the proxy.
pub fn ensure_holder() -> io::Result<UnixStream> {
    ensure_holder_for("")
}

/// Same as `ensure_holder` for an instance: spawns `mdrv-ds holder <instance>`
/// (detached) when the instance socket is down. Used by the XInput relay; the
/// default instance keeps its systemd unit + spawn fallback unchanged.
pub fn ensure_holder_for(instance: &str) -> io::Result<UnixStream> {
    if let Ok(s) = connect_for(instance) {
        return Ok(s);
    }
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("holder");
    if !instance.is_empty() {
        cmd.arg(instance);
    }
    let _ = cmd.stdin(Stdio::null()).stdout(Stdio::null()).spawn();
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(s) = connect_for(instance) {
            return Ok(s);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "holder daemon did not come up",
    ))
}

pub struct CreateMsg {
    pub bus: u16,
    pub vendor: u32,
    pub product: u32,
    pub version: u32,
    pub name: String,
    pub uniq: String,
    pub rdesc: Vec<u8>,
}

pub fn encode_create(m: &CreateMsg) -> Vec<u8> {
    let mut p = Vec::with_capacity(m.rdesc.len() + m.name.len() + m.uniq.len() + 16);
    p.extend_from_slice(&m.bus.to_le_bytes());
    p.extend_from_slice(&m.vendor.to_le_bytes());
    p.extend_from_slice(&m.product.to_le_bytes());
    p.extend_from_slice(&m.version.to_le_bytes());
    p.push(m.name.len() as u8);
    p.extend_from_slice(m.name.as_bytes());
    p.push(m.uniq.len() as u8);
    p.extend_from_slice(m.uniq.as_bytes());
    p.extend_from_slice(&(m.rdesc.len() as u16).to_le_bytes());
    p.extend_from_slice(&m.rdesc);
    p
}

pub fn decode_create(p: &[u8]) -> Option<CreateMsg> {
    let mut i = 0;
    fn take<'a>(p: &'a [u8], i: &mut usize, n: usize) -> Option<&'a [u8]> {
        let end = i.checked_add(n)?;
        if end > p.len() {
            return None;
        }
        let s = &p[*i..end];
        *i = end;
        Some(s)
    }
    let bus = u16::from_le_bytes(take(p, &mut i, 2)?.try_into().ok()?);
    let vendor = u32::from_le_bytes(take(p, &mut i, 4)?.try_into().ok()?);
    let product = u32::from_le_bytes(take(p, &mut i, 4)?.try_into().ok()?);
    let version = u32::from_le_bytes(take(p, &mut i, 4)?.try_into().ok()?);
    let nl = *take(p, &mut i, 1)?.first()?;
    let name = String::from_utf8(take(p, &mut i, nl as usize)?.to_vec()).ok()?;
    let ul = *take(p, &mut i, 1)?.first()?;
    let uniq = String::from_utf8(take(p, &mut i, ul as usize)?.to_vec()).ok()?;
    let rl = u16::from_le_bytes(take(p, &mut i, 2)?.try_into().ok()?) as usize;
    let rdesc = take(p, &mut i, rl)?.to_vec();
    Some(CreateMsg {
        bus,
        vendor,
        product,
        version,
        name,
        uniq,
        rdesc,
    })
}

// ---- small payload codecs ------------------------------------------------

/// GET_REPORT request/reply id+rnum: (id u32, rnum u8)
pub fn enc_req(id: u32, rnum: u8) -> Vec<u8> {
    let mut p = id.to_le_bytes().to_vec();
    p.push(rnum);
    p
}

pub fn dec_req(p: &[u8]) -> Option<(u32, u8)> {
    if p.len() != 5 {
        return None;
    }
    Some((u32::from_le_bytes(p[..4].try_into().ok()?), p[4]))
}

/// GET_REPORT reply: (id u32, err u16, data)
pub fn enc_get_reply(id: u32, err: u16, data: &[u8]) -> Vec<u8> {
    let mut p = id.to_le_bytes().to_vec();
    p.extend_from_slice(&err.to_le_bytes());
    p.extend_from_slice(&(data.len() as u16).to_le_bytes());
    p.extend_from_slice(data);
    p
}

pub fn dec_get_reply(p: &[u8]) -> Option<(u32, u16, Vec<u8>)> {
    if p.len() < 8 {
        return None;
    }
    let id = u32::from_le_bytes(p[..4].try_into().ok()?);
    let err = u16::from_le_bytes(p[4..6].try_into().ok()?);
    let n = u16::from_le_bytes(p[6..8].try_into().ok()?) as usize;
    if p.len() < 8 + n {
        return None;
    }
    Some((id, err, p[8..8 + n].to_vec()))
}

/// SET_REPORT request: (id u32, rnum u8, data)
pub fn enc_set_req(id: u32, rnum: u8, data: &[u8]) -> Vec<u8> {
    let mut p = enc_req(id, rnum);
    p.extend_from_slice(&(data.len() as u16).to_le_bytes());
    p.extend_from_slice(data);
    p
}

pub fn dec_set_req(p: &[u8]) -> Option<(u32, u8, Vec<u8>)> {
    if p.len() < 7 {
        return None;
    }
    let (id, rnum) = dec_req(&p[..5])?;
    let n = u16::from_le_bytes(p[5..7].try_into().ok()?) as usize;
    if p.len() < 7 + n {
        return None;
    }
    Some((id, rnum, p[7..7 + n].to_vec()))
}

/// SET_REPORT reply: (id u32, err u16)
pub fn enc_ack(id: u32, err: u16) -> Vec<u8> {
    let mut p = id.to_le_bytes().to_vec();
    p.extend_from_slice(&err.to_le_bytes());
    p
}

pub fn dec_ack(p: &[u8]) -> Option<(u32, u16)> {
    if p.len() != 6 {
        return None;
    }
    Some((
        u32::from_le_bytes(p[..4].try_into().ok()?),
        u16::from_le_bytes(p[4..6].try_into().ok()?),
    ))
}
