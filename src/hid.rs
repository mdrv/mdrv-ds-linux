//! DualSense hidraw helpers: discovery, feature ioctls, sysfs metadata.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Transport {
    Usb,
    Bluetooth,
}

impl Transport {
    pub fn name(self) -> &'static str {
        match self {
            Transport::Usb => "usb",
            Transport::Bluetooth => "bluetooth",
        }
    }
}

/// USB product id: DualShock 4.
pub const PRODUCT_DS4: u32 = 0x05c4;
/// USB product id: DualSense (kept for named symmetry; DS5 paths use it
/// implicitly via PadInfo defaults).
#[allow(dead_code)]
pub const PRODUCT_DUALSENSE: u32 = 0x0ce6;

pub struct PadInfo {
    pub path: PathBuf,
    pub uniq: String,
    pub transport: Transport,
    /// USB product id (0x0ce6 DualSense, 0x05c4 DualShock 4).
    pub product: u32,
    pub fw_version: u32,
    pub hw_version: u32,
    pub rdesc: Vec<u8>,
}

/// Parse the USB product id from a hidraw node's uevent (HID_ID=bus:vid:pid).
fn read_product_sysfs(hidraw: &std::path::Path) -> u32 {
    let uevent = std::fs::read_to_string(hidraw.join("device/uevent")).unwrap_or_default();
    for line in uevent.lines() {
        if let Some(id) = line.strip_prefix("HID_ID=") {
            if let Some(pid) = id.split(':').nth(2) {
                if let Ok(v) = u32::from_str_radix(pid.trim(), 16) {
                    return v;
                }
            }
        }
    }
    0x0ce6
}

fn hid_ioc(dir: u32, nr: u8, len: usize) -> u64 {
    ((dir << 30) | ((len as u32) << 16) | (b'H' as u32) << 8 | nr as u32) as u64
}

/// HIDIOCGFEATURE(len): _IOWR('H', 0x07) — dir MUST be 3, plain _IOC_READ is EINVAL'd.
pub fn get_feature(file: &File, rid: u8, buf: &mut [u8]) -> std::io::Result<usize> {
    if buf.is_empty() {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    buf[0] = rid;
    let req = hid_ioc(3, 0x07, buf.len());
    let r = unsafe { libc::ioctl(file.as_raw_fd(), req, buf.as_mut_ptr() as *mut libc::c_void) };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(r as usize)
    }
}

/// HIDIOCSFEATURE(len): _IOWR('H', 0x06). buf[0] is the report id.
pub fn set_feature(file: &File, buf: &mut [u8]) -> std::io::Result<usize> {
    let req = hid_ioc(3, 0x06, buf.len());
    let r = unsafe { libc::ioctl(file.as_raw_fd(), req, buf.as_ptr() as *mut libc::c_void) };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(r as usize)
    }
}

/// HIDIOCGRDESCSIZE: _IOR('H', 0x01, int). Cheap liveness probe for a hidraw
/// fd — ENOTTY means the fd is NOT a hidraw (e.g. we opened /dev/null
/// through a leftover hide-mount that cleanup couldn't remove).
pub fn rdesc_size(file: &File) -> std::io::Result<usize> {
    let mut sz: libc::c_int = 0;
    let req = hid_ioc(2, 0x01, 4);
    let r = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            req,
            &mut sz as *mut libc::c_int as *mut libc::c_void,
        )
    };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(sz as usize)
    }
}

/// Find every DualSense hidraw (054c:0ce6); USB entries sorted first.
/// Virtual pads (mdrv-ds or any locally-administered-MAC clone) are skipped.
/// True when the MAC's first octet has the locally-administered bit set
/// (our virtual pads toggle bit 0x02 of the real MAC's first octet).
fn is_virtual_mac(uniq: &str) -> bool {
    uniq.split(':')
        .next()
        .and_then(|o| u8::from_str_radix(o, 16).ok())
        .is_some_and(|o| o & 0x02 != 0)
}

pub fn find_pads() -> Vec<(PathBuf, String, Transport)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return out;
    };
    for e in entries.flatten() {
        let Some(name) = e.file_name().into_string().ok() else {
            continue;
        };
        if !name.starts_with("hidraw") {
            continue;
        }
        let Ok(uevent) = fs::read_to_string(e.path().join("device/uevent")) else {
            continue;
        };
        let mut transport = None;
        let mut uniq = String::new();
        let mut dev_name = String::new();
        for line in uevent.lines() {
            if line == "HID_ID=0003:0000054C:00000CE6" {
                transport = Some(Transport::Usb);
            } else if line == "HID_ID=0005:0000054C:00000CE6" {
                transport = Some(Transport::Bluetooth);
            }
            if let Some(v) = line.strip_prefix("HID_UNIQ=") {
                uniq = v.trim().to_string();
            }
            if let Some(v) = line.strip_prefix("HID_NAME=") {
                dev_name = v.trim().to_string();
            }
        }
        let Some(t) = transport else {
            continue;
        };
        // Never auto-pick a virtual pad: a second proxy chaining onto another
        // proxy's virtual pad causes kernel MAC-duplicate rejects (-EEXIST)
        // and masks the working virtual pad's nodes. Identify by name and by
        // the locally-administered MAC bit (our virtual MACs toggle bit 0x02).
        if dev_name.contains("mdrv-ds Virtual") || is_virtual_mac(&uniq) {
            continue;
        }
        out.push((PathBuf::from("/dev").join(&name), uniq, t));
    }
    out.sort_by_key(|(_, _, t)| match t {
        Transport::Usb => 0,
        Transport::Bluetooth => 1,
    });
    out
}

pub fn open_pad(explicit: Option<&str>) -> std::io::Result<(File, PadInfo)> {
    let (path, uniq, transport) = match explicit {
        Some(p) => {
            let path = PathBuf::from(p);
            let uniq = sysfs_attr(&path, "uniq")
                .filter(|s| !s.is_empty())
                .or_else(|| uevent_uniq(&path))
                .unwrap_or_default();
            let t = match sysfs_attr(&path, "uevent").as_deref() {
                Some(u) if u.contains("HID_ID=0005:") => Transport::Bluetooth,
                _ => Transport::Usb,
            };
            (path, uniq, t)
        }
        None => find_pads()
            .into_iter()
            .next()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENODEV))?,
    };
    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let info = PadInfo {
        product: read_product_sysfs(&path),
        fw_version: read_hex_sysfs(&path, "firmware_version").unwrap_or(0),
        hw_version: read_hex_sysfs(&path, "hardware_version").unwrap_or(0),
        rdesc: read_rdesc(&path),
        uniq,
        path,
        transport,
    };
    Ok((file, info))
}

pub fn sysfs_attr(hidraw: &Path, attr: &str) -> Option<String> {
    let name = hidraw.file_name()?;
    let s = fs::read_to_string(
        Path::new("/sys/class/hidraw")
            .join(name)
            .join("device")
            .join(attr),
    )
    .ok()?;
    Some(s.trim().to_string())
}

/// MAC from the uevent (HID_UNIQ=...) — the plain `uniq` sysfs attr can be empty.
pub fn uevent_uniq(hidraw: &Path) -> Option<String> {
    sysfs_attr(hidraw, "uevent")?
        .lines()
        .find_map(|l| l.strip_prefix("HID_UNIQ="))
        .map(|v| v.trim().to_string())
}

pub fn read_hex_sysfs(hidraw: &Path, attr: &str) -> Option<u32> {
    u32::from_str_radix(sysfs_attr(hidraw, attr)?.trim_start_matches("0x"), 16).ok()
}

pub fn read_rdesc(hidraw: &Path) -> Vec<u8> {
    let Some(name) = hidraw.file_name() else {
        return Vec::new();
    };
    fs::read(
        Path::new("/sys/class/hidraw")
            .join(name)
            .join("device")
            .join("report_descriptor"),
    )
    .unwrap_or_default()
}

/// Map feature report id → total ioctl length (report id byte + data bytes),
/// parsed from the HID report descriptor. hidraw GETFEATURE requires the exact
/// length, so this is the source of truth for feature ioctls.
pub fn feature_lengths(rdesc: &[u8]) -> std::collections::HashMap<u8, usize> {
    use std::collections::HashMap;
    let mut out = HashMap::new();
    let mut cur_id: u8 = 0;
    let mut cur_size: usize = 0; // bits per field
    let mut cur_count: usize = 0; // fields
    let mut i = 0;
    while i < rdesc.len() {
        let b = rdesc[i];
        i += 1;
        let tag = b & 0xF0;
        let typ = (b >> 2) & 0x3;
        let mut size = (b & 0x3) as usize;
        if size == 3 {
            size = 4;
        }
        if i + size > rdesc.len() {
            break;
        }
        let val = match size {
            1 => rdesc[i] as u64,
            2 => u16::from_le_bytes([rdesc[i], rdesc[i + 1]]) as u64,
            4 => u32::from_le_bytes([rdesc[i], rdesc[i + 1], rdesc[i + 2], rdesc[i + 3]]) as u64,
            _ => 0,
        };
        i += size;
        match (typ, tag) {
            (1, 0x80) => cur_id = val as u8,       // global Report ID
            (1, 0x70) => cur_size = val as usize,  // global Report Size (bits)
            (1, 0x90) => cur_count = val as usize, // global Report Count
            (0, 0xB0) => {
                // main Feature item
                let bytes = cur_size * cur_count / 8;
                out.insert(cur_id, bytes + 1); // + report id byte
            }
            _ => {}
        }
    }
    out
}

pub fn info() {
    let pads = find_pads();
    if pads.is_empty() {
        println!("no DualSense found");
        return;
    }
    for (path, uniq, transport) in pads {
        println!("== {} ({}) uniq={}", path.display(), transport.name(), uniq);
        let rdesc = read_rdesc(&path);
        println!(
            "   firmware=0x{:08x} hardware=0x{:08x} rdesc={} bytes",
            read_hex_sysfs(&path, "firmware_version").unwrap_or(0),
            read_hex_sysfs(&path, "hardware_version").unwrap_or(0),
            rdesc.len()
        );
        let flens = feature_lengths(&rdesc);
        let mut ids: Vec<u8> = flens.keys().copied().collect();
        ids.sort();
        println!(
            "   feature ids: {}",
            ids.iter()
                .map(|r| format!("0x{r:02x}={}", flens[r]))
                .collect::<Vec<_>>()
                .join(" ")
        );
        match open_pad(Some(path.to_str().unwrap())) {
            Ok((mut file, _)) => {
                for rid in ids {
                    let len = flens[&rid];
                    let mut buf = vec![0u8; len];
                    match get_feature(&file, rid, &mut buf) {
                        Ok(n) => println!("   feature 0x{rid:02x} ({n}B): {}", hex(&buf[..n])),
                        Err(e) => println!("   feature 0x{rid:02x} ({len}B): ERR {e}"),
                    }
                }
                let fd = file.as_raw_fd();
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let r = unsafe { libc::poll(&mut pfd, 1, 500) };
                if r > 0 {
                    let mut buf = [0u8; 512];
                    if let Ok(n) = file.read(&mut buf) {
                        println!("   input ({}B): {}", n, hex(&buf[..n]));
                    }
                } else {
                    println!("   input: (none within 500ms)");
                }
            }
            Err(e) => println!("   open failed: {e}"),
        }
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

/// Every evdev device node (/dev/input/eventN, /dev/input/jsN) belonging to
/// the HID device behind `hidraw` (e.g. /dev/hidraw3). Walks
/// /sys/class/hidraw/hidrawN/device/input/input*/ and maps each child to its
/// /dev/input node. Used by the proxy to hide the real pad's evdev presence
/// so games enumerate only the virtual pad.
pub fn pad_evdev_nodes(hidraw: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(name) = hidraw.file_name().and_then(|s| s.to_str()) else {
        return out;
    };
    let input_dir = Path::new("/sys/class/hidraw")
        .join(name)
        .join("device/input");
    let Ok(entries) = fs::read_dir(&input_dir) else {
        return out;
    };
    for e in entries.flatten() {
        let Ok(children) = fs::read_dir(e.path()) else {
            continue;
        };
        for c in children.flatten() {
            let fname = c.file_name();
            let Some(fname) = fname.to_str() else {
                continue;
            };
            if fname.starts_with("event") || fname.starts_with("js") {
                out.push(PathBuf::from("/dev/input").join(fname));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
