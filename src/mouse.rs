//! Standalone "touchpad as mouse" toggle — no mdrv-gm involved.
//!
//! The kernel exposes the DualSense touchpad as a regular evdev touchpad, so
//! the desktop (libinput) moves the cursor natively whenever it can read the
//! node. Therefore:
//!
//!   mouse ON  = no grab anywhere → libinput drives the cursor
//!   mouse OFF = a holder process keeps EVIOCGRAB on every DualSense
//!               touchpad node → touch events go only to the grabber
//!               (which drains and discards them)
//!
//! Works with the proxy on or off: the holder rescans every 500 ms and grabs
//! whichever DualSense touchpad nodes exist and are openable — the real pad's
//! node when the proxy is down, the virtual pad's node when it is up (the
//! proxy masks the real node with /dev/null, and EVIOCGRAB on /dev/null
//! ENOTTYs, so it is skipped naturally).
//!
//! The proxy additionally holds a permanent grab on the REAL touchpad node
//! from before it masks it, so libinput fds opened before masking go quiet
//! (EVIOCGRAB affects all clients of the device, not just future opens).

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// EVIOCGRAB = _IOW('E', 0x90, int)
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

/// Every evdev node whose input device name marks it as a DualSense touchpad
/// (real: "Sony Interactive Entertainment DualSense Wireless Controller
/// Touchpad", virtual: "mdrv-ds Virtual DualSense Touchpad").
pub fn touchpad_nodes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/sys/class/input") else {
        return out;
    };
    for entry in dir.flatten() {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("input") {
            continue;
        }
        let Ok(dev_name) = fs::read_to_string(p.join("name")) else {
            continue;
        };
        let dn = dev_name.to_lowercase();
        if !(dn.contains("dualsense") && dn.contains("touchpad")) {
            continue;
        }
        if let Ok(sub) = fs::read_dir(&p) {
            for e in sub.flatten() {
                if let Some(n) = e.file_name().to_str() {
                    if n.starts_with("event") {
                        out.push(PathBuf::from("/dev/input").join(n));
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Touchpad evdev nodes belonging to the HID device behind `hidraw`
/// (used by the proxy to grab the real touchpad before masking it).
pub fn touchpad_nodes_of(hidraw: &Path) -> Vec<PathBuf> {
    let base = Path::new("/sys/class/hidraw")
        .join(hidraw.file_name().unwrap_or_default())
        .join("device/input");
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(&base) else {
        return out;
    };
    for entry in dir.flatten() {
        let p = entry.path();
        if !p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("input"))
        {
            continue;
        }
        let Ok(dev_name) = fs::read_to_string(p.join("name")) else {
            continue;
        };
        let dn = dev_name.to_lowercase();
        if !(dn.contains("dualsense") && dn.contains("touchpad")) {
            continue;
        }
        if let Ok(sub) = fs::read_dir(&p) {
            for e in sub.flatten() {
                if let Some(n) = e.file_name().to_str() {
                    if n.starts_with("event") {
                        out.push(PathBuf::from("/dev/input").join(n));
                    }
                }
            }
        }
    }
    out
}

fn grab(path: &Path) -> Option<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let r = unsafe { libc::ioctl(f.as_raw_fd(), EVIOCGRAB, 1) };
    if r < 0 {
        // busy (someone else grabbed it) or not an evdev (masked node) — skip
        None
    } else {
        Some(f)
    }
}

/// Grab the given touchpad nodes for the caller's process lifetime.
/// Used by the proxy on the REAL pad's touchpad so stale libinput fds
/// (opened before masking) stop receiving events.
pub fn grab_permanently(nodes: &[PathBuf]) -> Vec<File> {
    nodes.iter().filter_map(|p| grab(p)).collect()
}

/// Discard queued events (a grabber is the only recipient, so it must drain).
fn drain(f: &mut File) -> bool {
    let mut buf = [0u8; 24 * 64];
    loop {
        match f.read(&mut buf) {
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
            // ENODEV/ENODEV/EIO: device gone
            Err(_) => return false,
        }
    }
}

fn pid_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("mdrv-ds-mouse.pid")
}

fn holder_pid() -> Option<i32> {
    let s = fs::read_to_string(pid_path()).ok()?;
    let pid: i32 = s.trim().parse().ok()?;
    // kill(pid, 0): 0 or EPERM = alive, ESRCH = dead
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
        Some(pid)
    } else {
        None
    }
}

/// The resident grab holder (hidden subcommand). Never exits on its own.
fn holder() -> ! {
    let mut held: Vec<(PathBuf, File)> = Vec::new();
    loop {
        let live = touchpad_nodes();
        // drop entries whose node vanished or whose fd went dead
        held.retain(|(p, f)| {
            let ok = p.exists() && {
                let mut ff = f;
                // a healthy idle fd has nothing to read; errors mean gone
                let mut buf = [0u8; 24];
                match (&mut ff).read(&mut buf) {
                    Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => false,
                    _ => true,
                }
            };
            ok
        });
        // grab newly appeared nodes (e.g. the virtual pad when the proxy starts)
        for p in &live {
            if !held.iter().any(|(hp, _)| hp == p) {
                if let Some(f) = grab(p) {
                    eprintln!("mouse holder: grabbed {} (touchpad hidden)", p.display());
                    held.push((p.clone(), f));
                }
            }
        }
        for (_, f) in held.iter_mut() {
            drain(f);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub fn cmd(rest: &[String]) {
    if rest.first().map(String::as_str) == Some("--hold-grab") {
        holder();
    }
    let on = || holder_pid().is_none(); // no holder = touchpad free = cursor moves
    let pidfile = pid_path();
    match rest.first().map(String::as_str) {
        Some("on") => {
            if let Some(pid) = holder_pid() {
                unsafe { libc::kill(pid, libc::SIGTERM) };
                let _ = fs::remove_file(&pidfile);
                eprintln!("mouse holder stopped ({pid}) — touchpad free");
            }
        }
        Some("off") => {
            if holder_pid().is_none() {
                match spawn_holder() {
                    Ok(pid) => eprintln!("mouse holder started ({pid}) — touchpad grabbed"),
                    Err(e) => {
                        eprintln!("failed to start mouse holder: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Some("toggle") | None => {
            if holder_pid().is_some() {
                cmd(&["on".to_string()]);
            } else {
                cmd(&["off".to_string()]);
            }
        }
        Some("status") => {}
        Some(other) => {
            eprintln!("unknown mouse arg: {other} (on|off|toggle|status)");
            std::process::exit(2);
        }
    }
    if on() {
        println!("touchpad-mouse: on (touchpad moves the cursor)");
    } else {
        println!("touchpad-mouse: off (touchpad grabbed, inert)");
    }
}

fn spawn_holder() -> std::io::Result<i32> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let child = Command::new(exe)
        .arg("mouse")
        .arg("--hold-grab")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0) // detach from the caller's terminal
        .spawn()?;
    let pid = child.id() as i32;
    std::thread::sleep(Duration::from_millis(200));
    // verify it survived startup
    if unsafe { libc::kill(pid, 0) } != 0 {
        let _ = std::fs::remove_file(pid_path());
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "holder exited immediately",
        ));
    }
    fs::write(pid_path(), format!("{pid}\n"))?;
    Ok(pid)
}
