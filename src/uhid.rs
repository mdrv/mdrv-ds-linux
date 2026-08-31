//! UHID event codec: create2/destroy/input2 + output/get_report/set_report parsing.
//! All offsets per packed struct uhid_event (linux/uhid.h).

use std::fs::File;
use std::io::Write;

pub const UHID_DESTROY: u32 = 1;
pub const UHID_START: u32 = 2;
pub const UHID_STOP: u32 = 3;
pub const UHID_OUTPUT: u32 = 6;
pub const UHID_GET_REPORT: u32 = 9;
pub const UHID_GET_REPORT_REPLY: u32 = 10;
pub const UHID_CREATE2: u32 = 11;
pub const UHID_INPUT2: u32 = 12;
pub const UHID_SET_REPORT: u32 = 13;
pub const UHID_SET_REPORT_REPLY: u32 = 14;

pub const EV_SIZE: usize = 4376;
pub const DATA_MAX: usize = 4096;

// create2 union offsets
const C2_NAME: usize = 4;
const C2_PHYS: usize = 132;
const C2_UNIQ: usize = 196;
const C2_RD_SIZE: usize = 260;
const C2_BUS: usize = 262;
const C2_VENDOR: usize = 264;
const C2_PRODUCT: usize = 268;
const C2_VERSION: usize = 272;
const C2_COUNTRY: usize = 276;
const C2_RD_DATA: usize = 280;

// input2 union offsets
const I2_SIZE: usize = 4;
const I2_DATA: usize = 6;

// get_report_reply / set_report_reply union offsets
const GRR_ID: usize = 4;
const GRR_ERR: usize = 8;
const GRR_SIZE: usize = 10;
const GRR_DATA: usize = 12;

// get_report / set_request share id/rnum layout
const GR_ID: usize = 4;
const GR_RNUM: usize = 8;
const SR_SIZE: usize = 10;
const SR_DATA: usize = 12;

// output union offsets — uhid_output_req: data[4096] FIRST, then size
const OUT_SIZE: usize = 4100; // 4 + 4096
const OUT_DATA: usize = 4;

pub const BUS_USB: u16 = 0x03;
pub const BUS_BLUETOOTH: u16 = 0x05;

pub struct CreateParams<'a> {
    pub name: &'a str,
    pub uniq: &'a str,
    pub rdesc: &'a [u8],
    pub bus: u16,
    pub vendor: u32,
    pub product: u32,
    pub version: u32,
}

pub fn create2(uhid: &mut File, p: &CreateParams) -> std::io::Result<()> {
    let mut buf = [0u8; EV_SIZE];
    write_u32(&mut buf, 0, UHID_CREATE2);
    set_cstr(&mut buf, C2_NAME, 128, p.name);
    set_cstr(&mut buf, C2_PHYS, 64, "");
    set_cstr(&mut buf, C2_UNIQ, 64, p.uniq);
    write_u16(&mut buf, C2_RD_SIZE, p.rdesc.len() as u16);
    write_u16(&mut buf, C2_BUS, p.bus);
    write_u32(&mut buf, C2_VENDOR, p.vendor);
    write_u32(&mut buf, C2_PRODUCT, p.product);
    write_u32(&mut buf, C2_VERSION, p.version);
    write_u32(&mut buf, C2_COUNTRY, 0);
    if C2_RD_DATA + p.rdesc.len() > EV_SIZE {
        return Err(std::io::Error::from_raw_os_error(libc::EOVERFLOW));
    }
    buf[C2_RD_DATA..C2_RD_DATA + p.rdesc.len()].copy_from_slice(p.rdesc);
    uhid.write_all(&buf)
}

pub fn destroy(uhid: &mut File) -> std::io::Result<()> {
    let mut buf = [0u8; EV_SIZE];
    write_u32(&mut buf, 0, UHID_DESTROY);
    uhid.write_all(&buf)
}

pub fn input2(uhid: &mut File, report: &[u8]) -> std::io::Result<()> {
    let payload = report.len().min(DATA_MAX);
    let mut buf = [0u8; EV_SIZE];
    write_u32(&mut buf, 0, UHID_INPUT2);
    write_u16(&mut buf, I2_SIZE, payload as u16);
    buf[I2_DATA..I2_DATA + payload].copy_from_slice(&report[..payload]);
    uhid.write_all(&buf)
}

pub enum UhidEvent<'a> {
    Start,
    Stop,
    /// (report bytes)
    Output(&'a [u8]),
    /// (request id, report number)
    GetReport(u32, u8),
    /// (request id, report number, report bytes)
    SetReport(u32, u8, &'a [u8]),
    Other(#[allow(dead_code)] u32),
}

pub fn parse_event(ev: &[u8]) -> UhidEvent<'_> {
    if ev.len() < EV_SIZE {
        // caller always reads full events; defensive
        return UhidEvent::Other(u32::MAX);
    }
    match read_u32(ev, 0) {
        UHID_START => UhidEvent::Start,
        UHID_STOP => UhidEvent::Stop,
        UHID_OUTPUT => {
            let size = read_u16(ev, OUT_SIZE) as usize;
            UhidEvent::Output(&ev[OUT_DATA..OUT_DATA + size.min(DATA_MAX)])
        }
        UHID_GET_REPORT => UhidEvent::GetReport(read_u32(ev, GR_ID), ev[GR_RNUM]),
        UHID_SET_REPORT => {
            let size = read_u16(ev, SR_SIZE) as usize;
            UhidEvent::SetReport(
                read_u32(ev, GR_ID),
                ev[GR_RNUM],
                &ev[SR_DATA..SR_DATA + size.min(DATA_MAX)],
            )
        }
        other => UhidEvent::Other(other),
    }
}

pub fn reply_get_report(uhid: &mut File, id: u32, err: u16, data: &[u8]) -> std::io::Result<()> {
    let mut buf = [0u8; EV_SIZE];
    write_u32(&mut buf, 0, UHID_GET_REPORT_REPLY);
    write_u32(&mut buf, GRR_ID, id);
    write_u16(&mut buf, GRR_ERR, err);
    if err == 0 {
        let len = data.len().min(DATA_MAX);
        write_u16(&mut buf, GRR_SIZE, len as u16);
        buf[GRR_DATA..GRR_DATA + len].copy_from_slice(&data[..len]);
    }
    uhid.write_all(&buf)
}

pub fn reply_set_report(uhid: &mut File, id: u32, err: u16) -> std::io::Result<()> {
    let mut buf = [0u8; EV_SIZE];
    write_u32(&mut buf, 0, UHID_SET_REPORT_REPLY);
    write_u32(&mut buf, GRR_ID, id);
    write_u16(&mut buf, GRR_ERR, err);
    uhid.write_all(&buf)
}

pub fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> i32 {
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) }
}

pub fn write_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes());
    buf[off + 2..off + 4].copy_from_slice(&((v >> 16) as u16).to_le_bytes());
}

pub fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

pub fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn set_cstr(buf: &mut [u8], off: usize, max: usize, s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(max - 1);
    buf[off..off + n].copy_from_slice(&bytes[..n]);
}
