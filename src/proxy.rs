//! Transport-matching DualSense proxy.
//!
//! Real pad (USB or BT) → UHID virtual pad with the SAME transport, the REAL
//! report descriptor, and the REAL firmware/hardware versions. All reports
//! relay verbatim; feature GETs/SETs are forwarded to the real pad (never
//! canned data — stale replies are what broke haptics in the first mdrv-gm
//! proxy after a pad firmware update).
//!
//! Optional gameplay-input gating (overlay) and stick deadzone, offsets
//! per transport (BT reports carry a 2-byte header).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::audio;
use crate::config;
use crate::hid::{self, PadInfo, Transport};
use crate::ipc;
use crate::l2cap;
use crate::mouse;
use crate::sink;
use crate::uhid;

pub struct ProxyOpts {
    pub pad: Option<String>,
    pub gate_file: Option<PathBuf>,
    pub deadzone: bool,
    pub hide: bool,
    pub verbose: bool,
}

static EXIT: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGHUP {
        RELOAD.store(true, Ordering::Relaxed);
    } else {
        EXIT.store(true, Ordering::Relaxed);
    }
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }
}

/// Mirror everything written to stderr into a log file, wherever mdrv-ds runs
/// (systemd journal, terminal, or detached). A background thread copies a pipe
/// (dup2'd onto fd 2) to both the original stderr and the file, so child
/// processes (chord commands) inherit the same tee'd stderr.
/// File: `$XDG_STATE_HOME|~/.local/state`/mdrv-ds/proxy.log (truncated at 1 MiB).
pub fn tee_stderr_to_file(name: &str) {
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;

    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("mdrv-ds");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(name);
    if let Ok(md) = fs::metadata(&path) {
        if md.len() > 1 << 20 {
            let _ = fs::File::create(&path); // cap: truncate oversized log
        }
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };

    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return;
    }
    let orig = unsafe { libc::dup(2) }; // original stderr: journal / terminal
    if orig < 0 {
        return;
    }
    unsafe { libc::dup2(fds[1], 2) }; // stderr (fd 2) is now the pipe
    unsafe { libc::close(fds[1]) };

    let mut reader = unsafe { File::from_raw_fd(fds[0]) };
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = file.write_all(&buf[..n]);
                    let _ = unsafe { libc::write(orig, buf.as_ptr() as *const _, n) };
                }
            }
        }
    });
    eprintln!("--- mdrv-ds starting (log: {})", path.display());
}

/// Pidfile for `mdrv-ds keymap reload` (SIGHUP) — removed on exit.
/// Write the pidfile, holding an exclusive flock on it for our whole process
/// lifetime. A second proxy instance fails the lock and exits — two proxies
/// chain onto each other's virtual pads and break games (kernel rejects the
/// duplicate MAC, bind-mounts mask the working virtual pad's nodes).
fn write_pidfile() {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let path = base.join("mdrv-ds.pid");
    let Ok(f) = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    else {
        eprintln!("pidfile {:?} not writable; running unlocked", path);
        return;
    };
    use std::os::unix::io::AsRawFd;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        let holder = fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok());
        match holder {
            Some(pid) if unsafe { libc::kill(pid, 0) } == 0 => {
                eprintln!(
                    "FATAL: another mdrv-ds proxy is running (pid {pid}) — refusing to start. \
                     Stop it first: systemctl --user stop mdrv-ds (or kill {pid})"
                );
            }
            _ => eprintln!("FATAL: pidfile lock held but owner unknown ({e}) — refusing to start"),
        }
        std::process::exit(1);
    }
    let _ = fs::write(&path, format!("{}\n", std::process::id()));
    std::mem::forget(f); // keep the lock until process exit
}

fn remove_pidfile() {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let _ = std::fs::remove_file(base.join("mdrv-ds.pid"));
}

/// PS+button chord engine. Runs on RAW input reports (before gating), fires
/// shell actions on the press edge while PS is held, and swallows the fired
/// button + PS bit from the relayed report until PS is released — so neither
/// the game nor mdrv-gm's PS-tap sees the chord at all.
struct ChordState {
    bindings: Vec<(String, usize, u8, String)>,
    prev_pressed: Vec<bool>,
    /// bits to strip per button-byte while a chord is active ([byte0,byte1,byte2])
    swallow: [u8; 3],
    /// analog L2/R2 to zero while their chord is active
    swallow_trig: [bool; 2],
    /// force the d-pad hat to neutral while a hat chord is active
    swallow_hat: bool,
    /// on reload, treat first report as "everything already pressed" (no refire)
    skip_edges: bool,
    /// [notify] event-callback config (reported on every chord fire)
    notify: config::NotifyConfig,
    /// [ps] tap/hold commands (normalized, 'shell:' stripped)
    ps_tap: Option<String>,
    ps_hold: Option<String>,
    /// solo-PS state machine: pressed / press instant / paired with any
    /// other button (chord or game input) / hold already fired
    ps_held: bool,
    ps_held_since: Option<std::time::Instant>,
    ps_paired: bool,
    ps_hold_fired: bool,
}

/// Analog-trigger press threshold (0-255); released rests at 0.
const TRIG_THRESHOLD: u8 = 0x40;
const TRIG_RELEASE: u8 = 0x30;

/// How long PS must be held solo before [ps] hold fires.
const PS_HOLD_SECS: u64 = 3;

impl ChordState {
    fn new(
        bindings: Vec<(String, usize, u8, String)>,
        notify: config::NotifyConfig,
        ps_tap: Option<String>,
        ps_hold: Option<String>,
    ) -> Self {
        let n = bindings.len();
        ChordState {
            bindings,
            prev_pressed: vec![false; n],
            swallow: [0; 3],
            swallow_trig: [false; 2],
            swallow_hat: false,
            skip_edges: true,
            notify,
            ps_tap,
            ps_hold,
            ps_held: false,
            ps_held_since: None,
            ps_paired: false,
            ps_hold_fired: false,
        }
    }

    /// Returns the chord name if a `ps:toggle` chord fired (caller flips the
    /// mode and reports it).
    fn feed(&mut self, report: &[u8], l: &InputLayout) -> Option<String> {
        if report.len() < l.btn0 + 3 {
            return None;
        }
        let mut toggle_ps = None;
        let ps_down = report[l.btn0 + 2] & 0x01 != 0;
        for (i, (name, byte, bit, cmd)) in self.bindings.iter().enumerate() {
            // L2/R2 are analog (no digital bit): pressed = value over threshold.
            // Hysteresis: a held trigger hovering near the threshold must not
            // flap pressed/released and re-fire the chord — release only under
            // the lower threshold once latched.
            let pressed = match *byte {
                config::TRIG_L2 => {
                    let prev = self.prev_pressed[i];
                    report
                        .get(l.trig0)
                        .is_some_and(|&v| v > if prev { TRIG_RELEASE } else { TRIG_THRESHOLD })
                }
                config::TRIG_R2 => {
                    let prev = self.prev_pressed[i];
                    report
                        .get(l.trig0 + 1)
                        .is_some_and(|&v| v > if prev { TRIG_RELEASE } else { TRIG_THRESHOLD })
                }
                // d-pad hats: compass nibble, pressed = direction active
                config::HAT_UP => crate::audiobridge::hat_dirs(report[l.btn0] & 0x0f)[0],
                config::HAT_DOWN => crate::audiobridge::hat_dirs(report[l.btn0] & 0x0f)[2],
                config::HAT_LEFT => crate::audiobridge::hat_dirs(report[l.btn0] & 0x0f)[3],
                config::HAT_RIGHT => crate::audiobridge::hat_dirs(report[l.btn0] & 0x0f)[1],
                _ => report[l.btn0 + byte] & bit != 0,
            };
            let fire = ps_down && pressed && !self.prev_pressed[i] && !self.skip_edges;
            if fire {
                if cmd == config::PS_TOGGLE {
                    toggle_ps = Some(name.clone());
                    eprintln!("chord {name}: toggling [ps] mode");
                } else {
                    eprintln!("chord {name} → sh -c {cmd:?}");
                    exec_and_notify(name, cmd, &self.notify);
                }
                // swallow this button and the PS press until PS is released
                if *byte == config::TRIG_L2 {
                    self.swallow_trig[0] = true;
                } else if *byte == config::TRIG_R2 {
                    self.swallow_trig[1] = true;
                } else if matches!(
                    *byte,
                    config::HAT_UP | config::HAT_DOWN | config::HAT_LEFT | config::HAT_RIGHT
                ) {
                    self.swallow_hat = true;
                } else {
                    self.swallow[*byte] |= bit;
                }
                self.swallow[2] |= 0x01;
            }
            self.prev_pressed[i] = pressed;
        }
        // --- solo PS tap / hold ------------------------------------------
        // "Paired" = any non-PS input active during the press (chord button
        // or game input) — cancels both tap and hold. feed() runs on the RAW
        // report, so a fired chord's button still reads as pressed here.
        let paired_now = (report[l.btn0] & 0x0f) != 0x08 // d-pad nibble (0x08 = neutral)
            || report[l.btn0] & 0xf0 != 0 // square/cross/circle/triangle
            || report[l.btn0 + 1] != 0 // L1/R1/share/options/L3/R3
            || report[l.btn0 + 2] & !0x01 != 0 // non-PS bits of the PS byte
            || report.get(l.trig0).is_some_and(|&v| v > TRIG_THRESHOLD)
            || report.get(l.trig0 + 1).is_some_and(|&v| v > TRIG_THRESHOLD);
        let now = std::time::Instant::now();
        if ps_down {
            if !self.ps_held {
                self.ps_held = true;
                self.ps_held_since = Some(now);
                // conservative on reload: PS already down = unknown history
                self.ps_paired = self.skip_edges;
                self.ps_hold_fired = false;
            }
            if paired_now {
                self.ps_paired = true;
            }
            if !self.ps_paired
                && !self.ps_hold_fired
                && self.ps_hold.is_some()
                && self.ps_held_since.is_some_and(|s| {
                    now.duration_since(s) >= std::time::Duration::from_secs(PS_HOLD_SECS)
                })
            {
                self.ps_hold_fired = true;
                let cmd = self.ps_hold.clone().unwrap();
                eprintln!("ps-hold → sh -c {cmd:?}");
                exec_and_notify("ps-hold", &cmd, &self.notify);
            }
        } else if self.ps_held {
            // release edge: a quick solo tap fires [ps] tap
            if !self.ps_paired && !self.ps_hold_fired {
                if let Some(cmd) = self.ps_tap.clone() {
                    eprintln!("ps-tap → sh -c {cmd:?}");
                    exec_and_notify("ps-tap", &cmd, &self.notify);
                }
            }
            self.ps_held = false;
            self.ps_held_since = None;
            self.ps_paired = false;
            self.ps_hold_fired = false;
        }
        if !ps_down {
            self.swallow = [0; 3];
            self.swallow_trig = [false; 2];
            self.swallow_hat = false;
        }
        self.skip_edges = false;
        toggle_ps
    }

    fn strip(&self, report: &mut [u8], l: &InputLayout) {
        if report.len() < l.btn0 + 3 {
            return;
        }
        for (i, mask) in self.swallow.iter().enumerate() {
            report[l.btn0 + i] &= !*mask;
        }
        // zero swallowed analog triggers
        if self.swallow_trig[0] && report.len() > l.trig0 {
            report[l.trig0] = 0;
        }
        if self.swallow_trig[1] && report.len() > l.trig0 + 1 {
            report[l.trig0 + 1] = 0;
        }
        // neutralize the hat nibble while a d-pad chord is active
        if self.swallow_hat {
            report[l.btn0] = (report[l.btn0] & 0xf0) | 0x08;
        }
    }

    /// Report a `ps:toggle` flip through [notify].
    fn notify_ps_toggle(&self, chord: &str, swallow: bool) {
        let mode = if swallow { "swallow" } else { "pass" };
        notify_event(
            &self.notify,
            chord,
            "ps:toggle",
            true,
            0,
            &format!("PS mode: {mode}"),
        );
    }
}

/// Run a chord command, log its result, and report it through [notify].
/// Used both by live chord fires and `mdrv-ds keymap test`.
fn exec_and_notify(name: &str, cmd: &str, ncfg: &config::NotifyConfig) {
    let name = name.to_string();
    let cmd = cmd.to_string();
    let ncfg = ncfg.clone();
    std::thread::spawn(move || {
        // stdout/stderr are captured (for MDRV_OUTPUT) and re-logged here —
        // the tee'd stderr picks this text up for journal + log file.
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(out) => {
                let success = out.status.success();
                let code = out.status.code().unwrap_or(-1);
                let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if !err.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&err);
                }
                if text.len() > 500 {
                    text.truncate(500);
                    text.push('…');
                }
                let verdict = if success { "ok" } else { "FAILED" };
                if text.is_empty() {
                    eprintln!("chord {name}: exit {verdict} ({code})");
                } else {
                    eprintln!("chord {name}: exit {verdict} ({code}), output:\n{text}");
                }
                notify_event(&ncfg, &name, &cmd, success, code, &text);
            }
            Err(e) => {
                eprintln!("chord {name}: spawn failed: {e}");
                notify_event(&ncfg, &name, &cmd, false, -1, &e.to_string());
            }
        }
    });
}

/// Run the user's [notify] command with the event as MDRV_* env vars.
fn notify_event(
    cfg: &config::NotifyConfig,
    chord: &str,
    cmd: &str,
    success: bool,
    code: i32,
    output: &str,
) {
    if !cfg.wants(success) {
        return;
    }
    let line = cfg.command.clone();
    let (chord, cmd, output) = (chord.to_string(), cmd.to_string(), output.to_string());
    std::thread::spawn(move || {
        let st = std::process::Command::new("sh")
            .arg("-c")
            .arg(&line)
            .env("MDRV_CHORD", &chord)
            .env("MDRV_COMMAND", &cmd)
            .env("MDRV_STATUS", if success { "success" } else { "failed" })
            .env("MDRV_CODE", code.to_string())
            .env("MDRV_OUTPUT", &output)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match st {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("notify: exit {s}"),
            Err(e) => eprintln!("notify: spawn failed: {e}"),
        }
    });
}

/// `mdrv-ds keymap test <name>` — fire one chord exactly as a live PS+button
/// press would (execute + notify), no pad needed.
pub fn fire_test(name: &str) {
    let cfg = config::load();
    let chords = config::parse_chords(&cfg);
    let Some((n, _, _, cmd)) = chords.list.into_iter().find(|(n, _, _, _)| n == name) else {
        eprintln!("no chord named {name:?} in {}", config::path().display());
        std::process::exit(1);
    };
    if cmd == config::PS_TOGGLE {
        println!("{n} = ps:toggle (internal action, fires only from the pad)");
        return;
    }
    println!("firing {n} → sh -c {cmd:?}");
    exec_and_notify(&n, &cmd, &cfg.notify);
    // give the detached executor a moment so CLI users see the log output
    std::thread::sleep(Duration::from_millis(1500));
}

/// Bind-mount /dev/null over the real hidraw (or undo) via the sudoers-gated
/// helper. Our already-open fd stays valid; only future open()s are affected —
/// clients that grabbed the pad BEFORE the proxy started keep racing us until
/// they re-open (reconnect the pad or restart the client).
fn hide_hidraw(path: &Path, action: &str) {
    let p = path.to_string_lossy().into_owned();
    match std::process::Command::new("sudo")
        .args(["-n", "/usr/local/bin/mdrv-ds-hide-hidraw", &p, action])
        .status()
    {
        Ok(s) if s.success() => eprintln!("hidraw {action} ok: {p}"),
        Ok(s) => eprintln!(
            "hidraw {action} FAILED (code {:?}) — clients may see the real pad",
            s.code()
        ),
        Err(_) => eprintln!("sudo helper unavailable — clients may see the real pad"),
    }
}

/// Clean up stale /dev/null bind-mounts from a previous crashed proxy run.
/// A hard crash (SIGKILL/OOM) leaves mounts behind; if a later device reuses
/// the same /dev/input/eventN or /dev/hidrawN number it would be invisible.
/// Scans /proc/mounts for devtmpfs mounts on our node namespaces and unmounts
/// them before we create fresh ones.
fn cleanup_stale_mounts() {
    let mounts = match fs::read_to_string("/proc/mounts") {
        Ok(m) => m,
        Err(_) => return,
    };
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (_dev, target, fstype) = match (fields.next(), fields.next(), fields.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => continue,
        };
        if fstype != "devtmpfs" {
            continue;
        }
        let t = target.replace("\\040", " ");
        if t.starts_with("/dev/hidraw") || t.starts_with("/dev/input/") {
            eprintln!("stale hide-mount found: {t}, removing");
            hide_hidraw(Path::new(&t), "unbind");
        }
    }
}

const DS_VENDOR: u32 = 0x054c;

// gameplay-field offsets relative to report start, per transport
// USB input: [0]=0x01, sticks 1..=6, buttons 8/9/10
// BT input:  [0]=0x31, [1]=seq, sticks 2..=7, buttons 9/10/11
pub(crate) struct InputLayout {
    pub(crate) stick0: usize,
    pub(crate) btn0: usize,
    /// analog trigger byte offset (L2 at trig0, R2 at trig0+1)
    pub(crate) trig0: usize,
    report_id: u8,
}

fn layout_for(transport: Transport) -> InputLayout {
    match transport {
        Transport::Usb => InputLayout {
            stick0: 1,
            btn0: 8,
            trig0: 5,
            report_id: 0x01,
        },
        Transport::Bluetooth => InputLayout {
            stick0: 2,
            btn0: 9,
            trig0: 6,
            report_id: 0x31,
        },
    }
}

/// MDRV_DUMP_INPUT capture helper: appends `usb <hex>` / `raw <hex>` lines.
fn dump_input(out: &mut Option<(std::fs::File, usize)>, rpt: &[u8], raw: &[u8]) {
    use std::io::Write;
    let Some((f, n)) = out else { return };
    if *n >= 200_000 {
        return;
    }
    *n += 1;
    let _ = writeln!(
        f,
        "usb {} raw {}",
        hid::hex(rpt),
        hid::hex(&raw[..raw.len().min(78)])
    );
}

/// Desktop toast for 3.5mm jack (dis)connect. Best-effort: off-thread so
/// the relay loop never blocks on the notifier, output discarded.
fn notify_jack(plugged: bool) {
    let (summary, body) = if plugged {
        ("Headset connected", "Routing audio to the controller jack")
    } else {
        (
            "Headset disconnected",
            "Routing audio to the controller speaker",
        )
    };
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("/g/mdrv-ds-notify/scripts/mdrv-notify");
        cmd.args(["-a", "mdrv-ds", "-t", "2500", summary, body]);
        if std::env::var("WAYLAND_DISPLAY").is_err() {
            // Service env may lack the compositor socket (notifications
            // need it); wayland-1 is this session's display.
            cmd.env("WAYLAND_DISPLAY", "wayland-1");
        }
        let _ = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

/// All-neutral input report for a transport (USB only — BT input reports
/// carry a CRC the receiving driver validates).
fn neutral_report(l: &InputLayout) -> Option<Vec<u8>> {
    if l.report_id != 0x01 {
        return None;
    }
    let mut r = vec![0u8; 64];
    r[0] = l.report_id;
    for i in l.stick0..l.stick0 + 6 {
        r[i] = 0x80;
    }
    r[l.btn0] = 0x08; // hat neutral
                      // Battery nibble: 0 would report "5% discharging" (kernel: n*10+5) and
                      // trigger low-battery notifications before real reports arrive. Use
                      // 0x0A (=100%), matching the kernel's initial power_supply value.
    r[53] = 0x0A;
    Some(r)
}

/// Best-effort suite toast: `notif {json}` line on the overlay's notify
/// socket ($XDG_RUNTIME_DIR/mdrv-ds-notify.sock — protocol per mdrv-ds-notify;
/// rendered by the overlay daemon). Fixed ASCII payloads only, so no JSON
/// escaping is needed. Silent when the overlay is down.
fn pad_toast(summary: &str, body: &str) {
    use std::io::Write as _;
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join("mdrv-ds-notify.sock");
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(&path) else {
        return;
    };
    let _ = s.set_write_timeout(Some(std::time::Duration::from_millis(300)));
    let line = format!(
        "notif {{\"app\":\"mdrv-ds\",\"summary\":\"{summary}\",\"body\":\"{body}\",\"icon\":\"\",\"timeout_ms\":2500}}\n",
    );
    if s.write_all(line.as_bytes()).is_err() {
        eprintln!("notify: toast submit failed");
    }
}

pub fn run(opts: ProxyOpts) -> i32 {
    install_signal_handlers();
    tee_stderr_to_file("proxy.log");
    write_pidfile();
    crate::audiobridge::start();
    cleanup_stale_mounts();
    let cfg = config::load();
    let mut chords = ChordState::new(
        config::parse_chords(&cfg).list,
        cfg.notify.clone(),
        cfg.ps.tap_cmd(),
        cfg.ps.hold_cmd(),
    );
    let mut ps_swallow = cfg.ps.swallow();
    let mut stick_dz = cfg.input;
    let mut tone: Option<audio::Tone> = None;
    let mut audio_sink: Option<sink::Sink> = None;
    if !chords.bindings.is_empty() {
        eprintln!("config: {} chord(s) loaded", chords.bindings.len());
    }
    if ps_swallow {
        eprintln!("config: ps=swallow (PS hidden from games/mdrv-gm)");
    }
    // Persistent-virtual-pad state moved into the holder daemon.
    // A cabled DualSense (USB hidraw) never shows up on L2CAP — USB forces
    // wired mode. When one is present (or appears while we wait on the PSM
    // listeners), serve it through the hidraw path instead and keep the PSMs
    // unbound for the next BT session.
    let usb_pad_present = || {
        opts.pad.is_none()
            && hid::find_pads()
                .iter()
                .any(|(_, _, t)| *t == Transport::Usb)
    };

    loop {
        // `ctrl` (L2CAP control channel) must outlive the whole session:
        // dropped only when relay_loop returns and the session is torn down.
        let (mut real, mut ctrl, info, mut l2cap_features) = if cfg.l2cap() && !usb_pad_present() {
            match l2cap::open(&|| EXIT.load(Ordering::Relaxed) || usb_pad_present()) {
                Ok(session) => {
                    eprintln!(
                        "proxy: L2CAP session open (rdesc={}B, features={})",
                        session.info.rdesc.len(),
                        session.features.len()
                    );
                    let features = session.features;
                    (
                        session.intr,
                        Some(session.ctrl),
                        session.info,
                        Some(features),
                    )
                }
                Err(e) => {
                    if EXIT.load(Ordering::Relaxed) {
                        return proxy_exit(&mut tone, &mut audio_sink);
                    }
                    if usb_pad_present() {
                        eprintln!("proxy: USB DualSense detected — hidraw session");
                        continue; // loop top takes the hidraw branch below
                    }
                    eprintln!("{e}");
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            }
        } else {
            match open_real(&opts.pad) {
                OpenReal::Ok(f, i) => (f, None, i, None),
                OpenReal::Waiting => {
                    if EXIT.load(Ordering::Relaxed) {
                        return proxy_exit(&mut tone, &mut audio_sink);
                    }
                    eprintln!("waiting for DualSense…");
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                OpenReal::Masked(msg) => {
                    eprintln!("FATAL: {msg}");
                    return 1;
                }
            }
        };
        eprintln!(
            "proxy: {} uniq={} fw=0x{:08x} hw=0x{:08x} rdesc={}B",
            info.path.display(),
            info.uniq,
            info.fw_version,
            info.hw_version,
            info.rdesc.len()
        );
        if matches!(info.transport, Transport::Usb) {
            // USB: the pad's audio is the kernel UAC sink ("Wireless
            // Controller") — make it default so audio follows the pad.
            follow_pad_sink(true);
        }

        let virtual_uniq = derive_virtual_mac(&info.uniq);
        let virtual_mac: [u8; 6] =
            parse_mac(&virtual_uniq).unwrap_or([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01]);
        // USB-chimera view (force_bus = "usb"): present the virtual pad as
        // bus=USB with the real USB rdesc even though the physical link is
        // BT — RE Engine (PRAGMATA) gates adaptive triggers/haptics on "pad
        // is Bluetooth". The relay translates BT input frames to USB input
        // reports and USB-shaped outputs/feature reports back to BT. DS5
        // only; FF16 needs the native BT presentation, which stays the
        // default.
        let usb_view = ctrl.is_some() && info.product != crate::hid::PRODUCT_DS4 && cfg.force_usb();
        if usb_view {
            eprintln!("usb view: virtual pad presents USB over the BT link (force_bus=\"usb\")");
        }
        // Chords/gating speak the layout the virtual pad presents natively
        // in both modes (USB-shaped when the chimera view is on).
        let layout = layout_for(if usb_view {
            Transport::Usb
        } else {
            info.transport
        });

        // The virtual pad lives in the holder daemon (see holder.rs) and
        // survives proxy restarts — games hold its nodes and never
        // re-enumerate. Each (re)connect announces the desired shape; the
        // holder reuses the existing pad when the shape matches and
        // recreates only on a USB ↔ BT change.
        let mut sock = match ipc::ensure_holder() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("holder unavailable: {e}; retrying");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let msg = ipc::CreateMsg {
            // BT-native presentation: FF16 accepts input ONLY from a pad that
            // is genuinely Bluetooth end-to-end (bus=BLUETOOTH + BT rdesc +
            // 0x31 frames). A USB-ancestry chimera was rejected by both the
            // input path and the haptics gate (tested 19:41 build). PRAGMATA
            // is the opposite case: force_bus = "usb" opts INTO the chimera
            // (usb_view above) — bus=USB + real USB rdesc + translated 0x01
            // input reports, i.e. exactly the cabled pad this game accepts.
            bus: if usb_view {
                uhid::BUS_USB
            } else {
                match info.transport {
                    crate::hid::Transport::Usb => uhid::BUS_USB,
                    _ => uhid::BUS_BLUETOOTH,
                }
            },
            vendor: DS_VENDOR,
            product: info.product,
            version: info.fw_version,
            // Exact stock names: games (FF7R/FF16) name-match "DualSense
            // Wireless Controller" for DualSense-specific input routing.
            name: if info.product == crate::hid::PRODUCT_DS4 {
                "DualShock 4 Wireless Controller".to_string()
            } else {
                "DualSense Wireless Controller".to_string()
            },
            uniq: virtual_uniq.clone(),
            rdesc: if usb_view {
                l2cap::DS5_HID_REPORT_DESCRIPTOR.to_vec()
            } else {
                info.rdesc.clone()
            },
        };
        if ipc::send(&mut sock, ipc::TAG_CREATE, &ipc::encode_create(&msg)).is_err() {
            eprintln!("holder: create send failed; retrying");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        match ipc::recv(&mut sock) {
            Ok((ipc::TAG_CREATE_ACK, a)) if a.first() == Some(&1) => eprintln!(
                "holder: virtual pad reused ({}, uniq={virtual_uniq})",
                info.transport.name()
            ),
            Ok((ipc::TAG_CREATE_ACK, a)) if a.first() == Some(&0) => eprintln!(
                "holder: virtual DualSense created ({}, uniq={virtual_uniq})",
                info.transport.name()
            ),
            Ok((ipc::TAG_CREATE_ACK, a)) => {
                eprintln!(
                    "holder: create failed: {}; retrying",
                    String::from_utf8_lossy(&a[1..])
                );
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            other => {
                eprintln!("holder: unexpected create reply {other:?}; retrying");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        }
        // neutral report for this transport (empty payload clears — BT has none)
        let neutral = neutral_report(&layout).unwrap_or_default();
        let _ = ipc::send(&mut sock, ipc::TAG_NEUTRAL, &neutral);

        // XInput emulation (default OFF): raise the second virtual pad
        // only when enabled (config + live override). Strictly additive —
        // the DS path above is untouched; disabled means nothing spawns.
        let mut xi = XiRelay::none();
        if crate::xinput::effective(cfg.xinput()) {
            xi.raise(&virtual_mac);
        }

        // Device nodes to hide: the real hidraw + every evdev node (event*/js*)
        // under its HID device. Bind-mounting /dev/null makes future open()s
        // fail/EOF, so games can only see the virtual pad. We already hold the
        // hidraw fd; evdev nodes aren't needed after open.
        // Grab the REAL touchpad evdev node BEFORE masking: EVIOCGRAB affects
        // all clients of the device, so this silences libinput fds opened
        // before we masked the node (masking only blocks future opens).
        // Kept alive for this connection's whole lifetime.
        // L2CAP: no hidraw device to hide or grab.
        // Session, not config: a cabled pad served through the hidraw path
        // (even with transport="l2cap") must still hide/grab like before.
        let session_is_l2cap = ctrl.is_some();
        let _touchpad_grab = if !session_is_l2cap {
            Some(mouse::grab_permanently(&mouse::touchpad_nodes_of(
                &info.path,
            )))
        } else {
            None
        };

        let hidden: Vec<PathBuf> = if !session_is_l2cap && opts.hide {
            let mut nodes = vec![info.path.clone()];
            nodes.extend(hid::pad_evdev_nodes(&info.path));
            for n in &nodes {
                hide_hidraw(n, "unbind"); // clear stale binds from a crash
            }
            for n in &nodes {
                hide_hidraw(n, "bind");
            }
            // Independent verification: every node must actually be masked.
            let missing: Vec<String> = nodes
                .iter()
                .filter(|n| !node_mounted(n))
                .map(|n| n.display().to_string())
                .collect();
            if !missing.is_empty() {
                eprintln!(
                    "ERROR: hide-mount not active for {missing:?} — games may see the real pad!"
                );
            }
            nodes
        } else {
            Vec::new()
        };

        let gate = Arc::new(AtomicBool::new(false));
        if opts.gate_file.is_some() {
            let g = gate.clone();
            let path = opts.gate_file.clone().unwrap();
            std::thread::spawn(move || loop {
                g.store(read_gate(&path), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
            });
        }

        // Poke the freshly (re)connected pad with a zeroed output report
        // (report id 0x02, all valid-flags 0 → nothing applied). Pads
        // replugged mid-session can stream pegged/garbage axes until their
        // first output report normalizes them; this makes every reconnect
        // self-healing instead of waiting for a game to send one.
        if info.transport == Transport::Usb {
            let mut poke = [0u8; 48];
            poke[0] = 0x02;
            let _ = real.write_all(&poke);
            // Re-assert the jack path on (re)connect — the pad forgets it
            // across power cycles and game outputs hardcode the speaker
            // path (see the in-flight rewrite in the relay below).
            let jack = sink::JACK_PLUGGED.load(Ordering::Relaxed);
            let _ = real.write_all(&sink::usb_engage_report(jack));
            eprintln!(
                "jack engage (usb) → {} (connect)",
                if jack { "headset" } else { "speaker" }
            );
        }

        // Phase-1 BT audio: continuous 440 Hz test tone (config-gated).
        if let Some(t) = tone.as_mut() {
            t.stop();
        }
        tone = None;
        if matches!(info.transport, Transport::Bluetooth)
            && info.product != crate::hid::PRODUCT_DS4
            && cfg.audio.test_tone
        {
            tone = Some(audio::start(&real, &cfg.audio));
        }

        // Phase-2 BT audio: PipeWire sink → Opus ladder (speaker/jack).
        // L2CAP: the sink is a process-lifetime resource — pad sessions
        // attach/detach their link fd without ever destroying the node, so
        // games never lose the 4ch haptics endpoint on a pad reconnect
        // (RE Engine never re-commits AudioClient_Initialize mid-run).
        // Legacy hidraw transport keeps the old session-scoped sink.
        if session_is_l2cap
            && info.product != crate::hid::PRODUCT_DS4
            && cfg.audio.sink != Some(false)
        {
            audio_sink = Some(sink::ensure_started(&cfg.audio));
            if sink::attach_pad(real.as_raw_fd()) {
                eprintln!("sink: pad link attached (persistent node)");
            }
            follow_pad_sink(false);
        } else if let Some(s) = audio_sink.as_mut() {
            s.stop();
            audio_sink = None;
        } else {
            audio_sink = None;
        }
        if matches!(info.transport, Transport::Bluetooth)
            && !session_is_l2cap
            && info.product != crate::hid::PRODUCT_DS4
            && cfg.audio.sink != Some(false)
        {
            audio_sink = Some(sink::start(
                &real,
                &cfg.audio,
                &hid::feature_lengths(&info.rdesc),
                false,
            ));
            follow_pad_sink(false);
        }

        // Suite toast: pad session fully established (pads + sink + gate).
        let pad_name = if info.product == crate::hid::PRODUCT_DS4 {
            "DualShock 4"
        } else {
            "DualSense"
        };
        pad_toast(&format!("{pad_name} connected"), info.transport.name());

        let code = relay_loop(
            &mut real,
            ctrl.as_mut(),
            &mut sock,
            &info,
            &gate,
            &opts,
            &virtual_mac,
            &mut chords,
            &mut ps_swallow,
            &mut stick_dz,
            session_is_l2cap,
            usb_view,
            cfg.swap_cross_circle.unwrap_or(false),
            l2cap_features.as_mut(),
            &mut xi,
        );
        eprintln!("teardown: relay loop returned (pad fd closed or exit requested)");
        if code != 3 {
            // 3 = holder recreate — the pad link itself stays up; no toast.
            pad_toast(&format!("{pad_name} disconnected"), "");
        }
        // Persistent sink: release the pad link but keep the node alive so
        // the game's audio endpoint survives into the next pad session.
        if session_is_l2cap {
            sink::detach_pad();
            eprintln!("sink: pad link detached (node stays up)");
        }
        // Undo the real pad's node masking; the virtual pad itself is owned
        // by the holder and outlives this proxy process.
        for n in &hidden {
            hide_hidraw(n, "unbind");
        }
        eprintln!("teardown: hidraw unmounts done");
        drop(sock); // holder emits the stored neutral report on this EOF
        drop(xi); // xi holder replays its stored neutral on this EOF
        if EXIT.load(Ordering::Relaxed) || code == 2 {
            eprintln!("proxy: exit (virtual pad kept alive by holder)");
            if let Some(t) = tone.as_mut() {
                t.stop();
            }
            if let Some(s) = audio_sink.as_mut() {
                s.stop();
            }
            remove_pidfile();
            return 0;
        }
        if code == 3 {
            eprintln!("proxy: holder connection lost — recreating (games may need restart)");
            continue;
        }
        eprintln!(
            "proxy: pad gone (code {code}); virtual pad kept alive, waiting for reconnection…"
        );
    }
}

enum OpenReal {
    Ok(File, PadInfo),
    Waiting,
    /// Node is masked by a leftover /dev/null hide-mount — fatal, fail fast.
    Masked(String),
}

/// Clean shutdown while waiting for a pad (no session is live): stop the
/// test tone / audio sink, drop the pidfile. The holder keeps the virtual
/// pad alive across proxy exits.
/// Transport-change hook: point the PulseAudio/PipeWire default sink at the
/// pad so game/desktop audio follows it across BT ↔ USB swaps — each
/// transport exposes a DIFFERENT sink (ours on BT, the kernel UAC device on
/// USB) and streams left behind on the vanished one get bounced by
/// WirePlumber. Fired once per pad (re)connect only; manual default
/// switches afterwards are never fought. Off-thread with brief retries —
/// PW registration lags hidraw discovery and sink::start.
fn follow_pad_sink(usb: bool) {
    std::thread::spawn(move || {
        let pactl = |args: &[&str]| {
            std::process::Command::new("pactl")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .output()
        };
        // WirePlumber occasionally applies the pad card's profile at boot
        // but fails to create the sink node (card present, mic source
        // present, sink missing). Cycling off→profile re-creates it.
        // Verified live 2026-08-29. Prefer the jack-aware "Default"
        // profiles (ports + split FL/FR → 3.5mm, stereo-friendly streams)
        // over raw pro-audio (4ch, no port routing, no jack audio, and
        // games copy the 4ch layout so later sink moves can fail).
        let mut cycled = false;
        for _ in 0..10 {
            let Ok(out) = pactl(&["list", "sinks", "short"]) else {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };
            let sinks = String::from_utf8_lossy(&out.stdout);
            let target = if usb {
                sinks.lines().find_map(|l| {
                    let mut f = l.split_whitespace();
                    f.next()?;
                    let name = f.next()?;
                    (name.starts_with("alsa_output.usb-Sony")
                        && (name.contains("DualSense") || name.contains("DualShock")))
                    .then(|| name.to_string())
                })
            } else {
                // BT sink name since Fix A: the real cabled pad's UAC
                // impersonation (see sink.rs PAD_SINK_NODE — PRAGMATA's
                // libScePad exact-matches "Wireless Controller").
                const PAD_SINK: &str = "alsa_output.usb-Sony_Interactive_Entertainment_DualSense_Wireless_Controller-00.Default__Speaker__sink";
                sinks
                    .lines()
                    .any(|l| l.split_whitespace().nth(1) == Some(PAD_SINK))
                    .then(|| PAD_SINK.to_string())
            };
            if let Some(name) = target {
                match pactl(&["set-default-sink", &name]) {
                    Ok(_) => eprintln!("sink: default → {name}"),
                    Err(e) => eprintln!("sink: set-default failed: {e}"),
                }
                return;
            }
            // Sink node missing but card may exist — profile-cycle once.
            if usb && !cycled {
                if let Ok(cards) = pactl(&["list", "cards", "short"]) {
                    let text = String::from_utf8_lossy(&cards.stdout);
                    if let Some(line) = text.lines().find(|l| {
                        l.split_whitespace()
                            .any(|f| f.starts_with("alsa_card.usb-Sony"))
                    }) {
                        if let Some(idx) = line.split_whitespace().next() {
                            cycled = true;
                            eprintln!("sink: pad card without sink node — cycling profile");
                            let _ = pactl(&["set-card-profile", idx, "off"]);
                            std::thread::sleep(Duration::from_millis(800));
                            let mut ok = false;
                            for prof in [
                                "pro-audio",
                                "Default (Headphones, Mic)",
                                "Default (Mic, Speaker)",
                            ] {
                                if pactl(&["set-card-profile", idx, prof]).is_ok() {
                                    ok = true;
                                    eprintln!("sink: profile cycle → {prof}");
                                    break;
                                }
                            }
                            if !ok {
                                eprintln!("sink: profile cycle failed");
                            }
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        eprintln!("sink: default-switch skipped (pad sink not seen)");
    });
}

fn proxy_exit(tone: &mut Option<audio::Tone>, audio_sink: &mut Option<sink::Sink>) -> i32 {
    sink::stop_all(); // persistent sink: stub handles can't stop it
    eprintln!("proxy: exit (virtual pad kept alive by holder)");
    if let Some(t) = tone.as_mut() {
        t.stop();
    }
    if let Some(s) = audio_sink.as_mut() {
        s.stop();
    }
    remove_pidfile();
    0
}

fn open_real(explicit: &Option<String>) -> OpenReal {
    let picked = match explicit {
        Some(p) => Some(p.clone()),
        None => hid::find_pads()
            .into_iter()
            .next()
            .map(|(p, _, _)| p.display().to_string()),
    };
    let Some(p) = picked else {
        return OpenReal::Waiting;
    };
    match hid::open_pad(Some(&p)) {
        Ok((file, info)) => {
            // Fail fast if the fd is not a live hidraw: opening through a
            // stale hide-mount yields /dev/null — reads EOF silently and
            // ioctls ENOTTY, which is exactly the "gamepad dead" failure.
            match hid::rdesc_size(&file) {
                Ok(_) => OpenReal::Ok(file, info),
                Err(e) => OpenReal::Masked(format!(
                    "{p} opened but not a hidraw ({e}): a leftover /dev/null hide-mount\nis masking the node and a running client (game/Steam) holds it busy.\nClose the holders and rerun, or: sudo /usr/local/bin/mdrv-ds-hide-hidraw {p} unbind"
                )),
            }
        }
        Err(e) => {
            eprintln!("open {p}: {e}");
            OpenReal::Waiting
        }
    }
}

/// Second virtual pad relay (XInput emulation; see xinput.rs). Owned by
/// the relay loop; `sock: None` = disabled — nothing spawned, nothing
/// polled, zero interaction with the DS-native path.
struct XiRelay {
    sock: Option<UnixStream>,
}

impl XiRelay {
    fn none() -> Self {
        XiRelay { sock: None }
    }

    /// Connect to the xi holder (spawning it detached if needed), create
    /// the Xbox 360 pad and arm its neutral report. The holder keeps the
    /// pad across proxy restarts exactly like the DS holder does.
    fn raise(&mut self, virtual_mac: &[u8; 6]) {
        if self.sock.is_some() {
            return;
        }
        let mut s = match ipc::ensure_holder_for("xi") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("xinput: holder unavailable: {e}");
                return;
            }
        };
        let msg = crate::xinput::create_msg(virtual_mac);
        if ipc::send(&mut s, ipc::TAG_CREATE, &ipc::encode_create(&msg)).is_err() {
            eprintln!("xinput: create send failed");
            return;
        }
        // Frame order is preserved — the neutral arms before any input
        // report can arrive, exactly like the DS path.
        let n = crate::xinput::neutral();
        let _ = ipc::send(&mut s, ipc::TAG_NEUTRAL, &n);
        eprintln!("xinput: Xbox 360 pad raised (uniq {})", msg.uniq);
        self.sock = Some(s);
    }

    /// Live-disable: destroy the pad (holder resets its create key) and
    /// drop the link. Safe to call repeatedly.
    fn lower(&mut self) {
        if let Some(mut s) = self.sock.take() {
            let _ = ipc::send(&mut s, ipc::TAG_DESTROY, &[]);
            eprintln!("xinput: Xbox 360 pad destroyed (live disable)");
        }
    }

    /// Park the kernel state at neutral without dropping the link (overlay
    /// hold: the DS report stream stops here; a frozen xpad state would
    /// leave stuck buttons/sticks in XInput games).
    fn park_neutral(&mut self) {
        if let Some(s) = self.sock.as_mut() {
            let n = crate::xinput::neutral();
            let _ = ipc::send(s, ipc::TAG_INPUT, &n);
        }
    }
}

fn relay_loop(
    real: &mut File,
    // L2CAP control channel (PSM 0x11); polled and drained for the whole
    // session (vds parity) - 0xA3 feature replies refresh `features`.
    mut ctrl: Option<&mut File>,
    sock: &mut UnixStream,
    info: &PadInfo,
    gate: &Arc<AtomicBool>,
    opts: &ProxyOpts,
    virtual_mac: &[u8; 6],
    chords: &mut ChordState,
    ps_swallow: &mut bool,
    stick_dz: &mut crate::config::InputConfig,
    l2cap: bool,
    usb_view: bool,
    swap_ox: bool,
    mut features: Option<&mut std::collections::HashMap<u8, Vec<u8>>>,
    xi: &mut XiRelay,
) -> i32 {
    // The virtual pad carries the transport's NATIVE descriptor on L2CAP
    // (BT presentation — see run()), so everything downstream speaks the
    // transport's layout; a DualShock 4 L2CAP session speaks 0x11 instead.
    // usb_view (force_bus="usb"): it carries the USB descriptor instead and
    // input is translated to USB 0x01 reports before this layout applies.
    let ds4 = l2cap && info.product == crate::hid::PRODUCT_DS4;
    let layout = if ds4 {
        InputLayout {
            stick0: 3,
            btn0: 7,
            trig0: 10,
            report_id: 0x11,
        }
    } else {
        layout_for(if usb_view {
            Transport::Usb
        } else {
            info.transport
        })
    };
    let flens = hid::feature_lengths(&info.rdesc);
    let real_fd = real.as_raw_fd();
    let sock_fd = sock.as_raw_fd();
    // fd < 0 makes poll(2) ignore the entry (POSIX) — used when no ctrl.
    let ctrl_fd = ctrl.as_ref().map(|c| c.as_raw_fd()).unwrap_or(-1);
    let mut in_buf = [0u8; 512];
    let mut ctrl_buf = [0u8; 1024];
    // Input pacing (L2CAP): forward at most one report per pace window
    // (default 12 ms, real-USB cadence), MDRV_INPUT_PACE_US to override.
    let pace_ns: u64 = std::env::var("MDRV_INPUT_PACE_US")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(12_000)
        * 1000;
    let now0 = std::time::Instant::now();
    let mut last_fwd = now0
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or(now0);
    let mut input_dump = std::env::var("MDRV_DUMP_INPUT").ok().and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
            .map(|f| (f, 0usize))
    });
    // Post-arbitration hold-break: after the overlay (hold client) closes
    // or the gate file releases, keep swallowing frames until every button
    // is up, so the closing press (e.g. cross) never reaches the game.
    let mut pause_break = false;
    let mut gate_break = false;
    let mut eofs: u32 = 0;
    // Output-log dedup: kernel state workers resend identical 0x02 output
    // reports in high-frequency bursts (mute-LED worker: ~25/s observed).
    // Log each distinct payload once and report the repeat count when it
    // changes — forwarding is unaffected.
    let mut last_out: Vec<u8> = Vec::new();
    let mut out_reps: u32 = 0;
    // Synthetic USB sequence counter for the chimera view (see translation
    // below): real cabled DualSense pads roll [7] every input report.
    let mut usb_seq: u8 = 0;
    // Jack-detect debounce (integrator): the HP-detect byte (input 0x31
    // byte 56) is noisy — while PLUGGED the bit is high-dominant but
    // flutters low (byte jumps 0x01/0x65/0x11/0x64), and around an
    // UNPLUG isolated high-garbage frames appear (0x19 observed). So:
    // charge +1 per high frame (max 16), discharge -4 per low frame;
    // engage at 16 (≈25 ms of high-dominant input — single garbage
    // frames can't reach it), disengage only after 800 ms with ZERO
    // high frames (flutter can never trip it).
    let mut jack_int: u32 = 0;
    // USB: periodic jack-path re-assert. The pad can drop audio-config
    // state when the UAC stream re-opens (alt-setting change); BT re-
    // asserts per 0x36, USB needs this heartbeat (audio-config bits
    // only — never touches rumble/trigger bytes).
    let mut usb_reassert_at = std::time::Instant::now();
    let mut jack_raw_since = now0;
    let mut jack_latched = false;

    loop {
        let now = std::time::Instant::now();
        // fd < 0 makes poll(2) ignore the entry (POSIX) — used when no
        // ctrl channel or the XInput tap is down.
        let xi_fd = xi.sock.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1);
        let mut fds = [
            libc::pollfd {
                fd: real_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sock_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: ctrl_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: xi_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if EXIT.load(Ordering::Relaxed) {
            return 2;
        }
        if RELOAD.swap(false, Ordering::Relaxed) {
            let cfg = config::load();
            *chords = ChordState::new(
                config::parse_chords(&cfg).list,
                cfg.notify.clone(),
                cfg.ps.tap_cmd(),
                cfg.ps.hold_cmd(),
            );
            *ps_swallow = cfg.ps.swallow();
            *stick_dz = cfg.input;
            let mode = sink::current_mode(cfg.audio.speaker_output.as_deref());
            let live = sink::set_speaker_mode(mode);
            let gain = sink::current_haptic_gain(cfg.audio.haptic_gain);
            let gain_live = sink::set_haptic_gain(gain);
            eprintln!(
                "config reloaded: {} chord(s), ps={}, sticks(enabled={}, inner={:.2}, outer={:.2}), speaker={}{}, gain={:.2}{}",
                chords.bindings.len(),
                if *ps_swallow { "swallow" } else { "pass" },
                stick_dz.enabled,
                stick_dz.inner_dz,
                stick_dz.outer_dz,
                sink::mode_name(mode),
                if live { " (live)" } else { "" },
                gain,
                if gain_live { " (live)" } else { "" }
            );
            // XInput emulation live-switch (override file + SIGHUP):
            // raise or tear down the second virtual pad in place.
            let want_xi = crate::xinput::effective(cfg.xinput());
            if want_xi && xi.sock.is_none() {
                xi.raise(virtual_mac);
            } else if !want_xi && xi.sock.is_some() {
                xi.lower();
            }
        }
        let timeout_ms: i32 = 250;
        let ret = uhid::poll(&mut fds, timeout_ms);
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            eprintln!("poll: {e}");
            return 1;
        }

        // real → holder (verbatim, optional gating)
        if fds[0].revents & libc::POLLIN != 0 {
            match real.read(&mut in_buf) {
                Ok(n) if n > 0 => {
                    eofs = 0;
                    // L2CAP: raw frames arrive with a 0xA1 HIDP header byte
                    // prepended; strip it so the rest of the relay path sees
                    // the same report-id-first layout as hidraw.
                    let raw = &in_buf[..n];
                    let mut report = if l2cap && raw.first() == Some(&0xA1) {
                        raw[1..].to_vec()
                    } else {
                        raw.to_vec()
                    };
                    // Jack detect for the BT audio sink: HP-detect is
                    // BT frame byte 56 bit0 (= report[55]; kernel
                    // dualsense_input_report.status[2] with the USB payload
                    // starting at raw[3]). Verified empirically over 93k
                    // frames: plugged → raw[56]=0x01 constant, unplugged →
                    // 0x00. The OLD code read report[56] (raw[57]) — constant
                    // 0 in every state, hence "speaker always". Neighbors
                    // raw[52..53]/[58..61] are counters/CRC noise. Hysteresis
                    // (see decl above): integrator arms plug at 16 net
                    // highs; 800 ms with zero highs unlatches.
                    let jack_byte = if report.len() > 55 && report[0] == 0x31 {
                        Some(report[55])
                    } else if report.len() > 54 && report[0] == 0x01 {
                        // USB input 0x01: HP-detect at byte 54 bit0 (vds
                        // kUsbInputHeadsetOffset=54 counts the report id).
                        Some(report[54])
                    } else {
                        None
                    };
                    if let Some(jb) = jack_byte {
                        let raw = jb & 0x01 != 0;
                        let before = jack_latched;
                        if raw {
                            jack_int = (jack_int + 1).min(16);
                            jack_raw_since = now;
                            if !jack_latched && jack_int >= 16 {
                                jack_latched = true;
                            }
                        } else {
                            jack_int = jack_int.saturating_sub(4);
                            if jack_latched
                                && now.duration_since(jack_raw_since)
                                    >= std::time::Duration::from_millis(800)
                            {
                                jack_latched = false;
                            }
                        }
                        if jack_latched != before {
                            eprintln!("jack: latch -> {} (byte {:#04x})", jack_latched, jb);
                            notify_jack(jack_latched);
                            if !l2cap {
                                // USB engage: the same mic-state bytes as
                                // the BT 0x31 engage, framed as a 48 B 0x02
                                // output report on the pad's hidraw.
                                if let Err(e) =
                                    real.write_all(&sink::usb_engage_report(jack_latched))
                                {
                                    eprintln!("jack engage (usb) write: {e}");
                                } else {
                                    eprintln!(
                                        "jack engage (usb) → {}",
                                        if jack_latched { "headset" } else { "speaker" }
                                    );
                                }
                            }
                        }
                        sink::JACK_PLUGGED.store(jack_latched, Ordering::Relaxed);
                    }
                    if !l2cap
                        && now.duration_since(usb_reassert_at)
                            >= std::time::Duration::from_millis(2000)
                    {
                        usb_reassert_at = now;
                        let jack = sink::JACK_PLUGGED.load(Ordering::Relaxed);
                        let _ = real.write_all(&sink::usb_reassert_report(jack));
                    }
                    // L2CAP native passthrough: the virtual pad carries the
                    // BT descriptor, so control frames relay verbatim in
                    // their native 0x31 shape. Audio/mic payloads (byte-1
                    // low nibble 0x02/0x03) and other frames are dropped —
                    // the kernel would misparse them as input.
                    if l2cap {
                        let control = if report.first() == Some(&0x11) && report.len() == 78 {
                            // DualShock 4 control frame (CRC intact, kernel-checked)
                            true
                        } else {
                            report.len() >= 65 && report[0] == 0x31 && report[1] & 0x0F == 0x01
                        };
                        if !control {
                            continue;
                        }
                    }
                    // Chimera view: translate BT 0x31 input frames into USB
                    // 0x01 input reports. The payloads are offset-aligned
                    // (usb[i+1] = bt[i+2] for the 63-byte state), so this is
                    // a header/seq/CRC re-frame, not a field remap.
                    if usb_view && report.first() == Some(&0x31) && report.len() >= 65 {
                        let mut usb = Vec::with_capacity(64);
                        usb.push(0x01);
                        usb.extend_from_slice(&report[2..65]);
                        // USB input reports carry an incrementing sequence
                        // counter at [7]; BT frames park that slot at 1 (BT
                        // sequences via the L2CAP header byte instead). Games
                        // (RE Engine/PRAGMATA) treat a frozen counter as a
                        // silent pad and drop all input — synthesize it.
                        usb_seq = usb_seq.wrapping_add(1);
                        usb[7] = usb_seq;
                        // USB-view fidelity: a few fields in the pad's BT
                        // frames tell transport truths that contradict the
                        // USB costume (kernel hid-playstation.c layout):
                        //  - touch points carry stale bytes while inactive
                        //    (contact low bits + coordinates); cabled pads
                        //    report an idle point as 80 00 00 00.
                        //  - reserved3[0] ([0x29]) streams 0xff on BT, 0x00
                        //    when cabled.
                        //  - status[1] ([0x36]) bit3 marks external power:
                        //    only set when cabled. RE Engine reads it — a USB
                        //    pad claiming battery power is rejected.
                        for point in [0x21usize, 0x25] {
                            if usb[point] & 0x80 != 0 {
                                usb[point..point + 4].copy_from_slice(&[0x80, 0, 0, 0]);
                            }
                        }
                        usb[0x29] = 0;
                        usb[0x36] |= 0x08;
                        // O/X swap: buttons[0] bit5 = cross, bit6 = circle
                        // (kernel hid-playstation layout; USB[8] = buttons[0]).
                        // Runs before every consumer — game HID reads, kernel
                        // evdev, chords — so the game visibly confirms with
                        // the swapped button.
                        if swap_ox {
                            let b = usb[8];
                            usb[8] = (b & !0x60) | ((b & 0x20) << 1) | ((b & 0x40) >> 1);
                        }
                        report = usb;
                    }
                    // chords fire on the raw report (work even while the
                    // gate is active); fired bits are swallowed below
                    if let Some(tname) = chords.feed(&report, &layout) {
                        *ps_swallow = !*ps_swallow;
                        let mode = if *ps_swallow { "swallow" } else { "pass" };
                        eprintln!("ps mode → {mode}");
                        chords.notify_ps_toggle(&tname, *ps_swallow);
                    }
                    chords.strip(&mut report, &layout);
                    // Bridge feed BEFORE the PS strip: watch clients
                    // (launcher) need the PS edge; hold clients also get
                    // sticks. Chord-swallowed bits are already zeroed.
                    crate::audiobridge::feed(&report, &layout);
                    // PS allowlist: "swallow" strips the PS bit from every
                    // relayed report, so games/mdrv-gm never see PS at
                    // all — only mdrv-ds chords (fed above, raw) do.
                    if report.len() > layout.btn0 + 2 {
                        if *ps_swallow {
                            report[layout.btn0 + 2] &= !0x01;
                        }
                    }
                    // Overlay (hold client) open: the game gets nothing.
                    if crate::audiobridge::paused() {
                        if !pause_break {
                            if input_dbg() {
                                eprintln!("in: paused engage (overlay hold) — dropping frames");
                            }
                            // Park the XInput pad at neutral — its report
                            // stream stops here and a frozen xpad state
                            // would stick in XInput games.
                            xi.park_neutral();
                        }
                        pause_break = true;
                        continue;
                    }
                    if pause_break {
                        if buttons_live(&report, &layout) {
                            continue;
                        }
                        pause_break = false;
                        if input_dbg() {
                            eprintln!("in: pause released (pad neutral)");
                        }
                    }
                    if gate.load(Ordering::Relaxed) {
                        if !gate_break && input_dbg() {
                            eprintln!("in: gate engage (focus file)");
                        }
                        gate_break = true;
                        gate_input(&mut report, &layout);
                    } else {
                        if gate_break {
                            if buttons_live(&report, &layout) {
                                gate_input(&mut report, &layout);
                            } else {
                                gate_break = false;
                                if input_dbg() {
                                    eprintln!("in: gate released (pad neutral)");
                                }
                            }
                        }
                        if stick_dz.enabled {
                            scale_sticks(
                                &mut report,
                                &layout,
                                stick_dz.inner_dz,
                                stick_dz.outer_dz,
                            );
                        }
                    }
                    if l2cap {
                        dump_input(&mut input_dump, &report, &in_buf[..n]);
                        // USB-like cadence: real USB pads deliver a report
                        // every ~12 ms; the pad's BT high-rate mode bursts
                        // every ~1.4 ms. Full-state reports mean pacing only
                        // adds ≤12 ms latency, no event loss.
                        if now.duration_since(last_fwd).as_nanos() < pace_ns as u128 {
                            continue;
                        }
                        last_fwd = now;
                    }
                    // XInput tap: translate the same post-processed
                    // report (chimera translation, chords, PS swallow,
                    // gate and deadzones all applied above) into the xpad
                    // packet for the second virtual pad. Tap errors never
                    // touch the DS session — the tap just goes dark.
                    if xi.sock.is_some() {
                        let mut xsock = xi.sock.take().unwrap();
                        let xr = crate::xinput::translate(&report, &layout);
                        if ipc::send(&mut xsock, ipc::TAG_INPUT, &xr).is_err() {
                            eprintln!("xinput: holder link lost — tap disabled until reload");
                        } else {
                            xi.sock = Some(xsock);
                        }
                    }

                    if ipc::send(sock, ipc::TAG_INPUT, &report).is_err() {
                        return 3;
                    }
                    if opts.verbose {
                        eprintln!("in  ({}B): {}", n, hid::hex(&in_buf[..n]));
                    }
                }
                Ok(0) => {
                    // /dev/null (masked node) reads EOF forever — a real
                    // hidraw never returns 0 here. Bail out loudly.
                    eofs += 1;
                    if eofs > 50 {
                        eprintln!("real pad fd EOF x{eofs} — masked/dead node, bailing out");
                        return 1;
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("hidraw read: {e}"),
            }
        }

        // holder → real (outputs verbatim, features forwarded to real pad)
        if fds[1].revents & libc::POLLIN != 0 {
            match ipc::recv(sock) {
                Ok((ipc::TAG_OUTPUT, data)) => {
                    // Dedup: see last_out/out_reps decl above.
                    if data != last_out {
                        if out_reps > 1 {
                            eprintln!("out: last identical report ×{out_reps}");
                        }
                        if !data.is_empty() {
                            eprintln!("out ({}B): {}", data.len(), hid::hex(&data));
                        }
                        last_out.clear();
                        last_out.extend_from_slice(&data);
                        out_reps = 1;
                    } else {
                        out_reps += 1;
                    }
                    // On BT with the audio sink running, hand kernel output
                    // reports to the sink writer so ALL interrupt-channel
                    // reports share one sequence counter (two independent
                    // counters make the pad drop reports — rumble AND audio).
                    // L2CAP: kernel outputs arrive as 48 B USB 0x02 reports
                    // (virtual pad carries the USB rdesc; [1..48] = 47 B
                    // state); hidraw BT keeps feeding 78 B 0x31s. Both go to
                    // the sink writer so ALL interrupt-channel reports share
                    // one sequence counter (two independent counters make
                    // the pad drop reports — rumble AND audio).
                    // DualShock 4 L2CAP session: forward kernel outputs
                    // verbatim on the interrupt channel (kernel-built CRC is
                    // already valid; no sink/haptics path exists for DS4).
                    if l2cap
                        && info.product == crate::hid::PRODUCT_DS4
                        && data.len() == 78
                        && data.first() == Some(&0x11)
                    {
                        let mut wire = Vec::with_capacity(79);
                        wire.push(0xA2u8);
                        wire.extend_from_slice(&data);
                        if let Err(e) = real.write_all(&wire) {
                            eprintln!("ds4 output write: {e}");
                        }
                        continue;
                    }
                    let bt_state = info.transport == Transport::Bluetooth
                        && data.len() == 78
                        && data.first() == Some(&0x31);
                    // Merged-rdesc pad on L2CAP: kernel/game may emit 48 B
                    // USB 0x02 outputs (report 0x02 declared in the USB part
                    // of the merged rdesc). Accept ≥48 B 0x02 shapes; the
                    // sink extracts the 47 B state from either shape.
                    let usb_state = l2cap && data.len() >= 48 && data.first() == Some(&0x02);
                    // Game trigger-effect reports (USB 0x05, same 47 B state
                    // layout as 0x02): decode through the sink writer too —
                    // the pad applies each section gated by its enable flags,
                    // so a 0x05 only touches the trigger-FFB section of the
                    // BT 0x31 state.
                    let usb_trigger = l2cap && data.len() == 48 && data.first() == Some(&0x05);
                    if bt_state && sink::enqueue_output(&data) {
                        // sent via the sink writer
                    } else if (usb_state || usb_trigger) && sink::enqueue_output(&data[..48]) {
                        // sent via the sink writer
                    } else if l2cap {
                        eprintln!(
                            "l2cap: dropped unexpected output shape ({}B): {}",
                            data.len(),
                            hid::hex(&data[..data.len().min(8)])
                        );
                    } else {
                        // USB relay: keep the engaged jack path sticky —
                        // game/wine audio-control writes hardcode the
                        // speaker path (captured: flag0 0x8d, [7]=0x30).
                        // On BT the sink re-merges the path on every 0x36;
                        // on USB rewrite the path nibble in flight.
                        let mut data = data;
                        if info.transport == Transport::Usb
                            && data.len() == 48
                            && data.first() == Some(&0x02)
                        {
                            let jack = sink::JACK_PLUGGED.load(Ordering::Relaxed);
                            data[7] = (data[7] & !0x30) | if jack { 0x00 } else { 0x30 };
                        }
                        // 63 B 0x02 outputs (Edge-shaped) share the common
                        // block at [1..]; rewrite their path nibble too.
                        if info.transport == Transport::Usb
                            && data.len() == 63
                            && data.first() == Some(&0x02)
                        {
                            let jack = sink::JACK_PLUGGED.load(Ordering::Relaxed);
                            data[7] = (data[7] & !0x30) | if jack { 0x00 } else { 0x30 };
                        }
                        if let Err(e) = real.write_all(&data) {
                            eprintln!("hidraw write output: {e}");
                        }
                    }
                }
                Ok((ipc::TAG_GET_REQ, p)) => {
                    let Some((id, rnum)) = ipc::dec_req(&p) else {
                        continue;
                    };
                    if opts.verbose {
                        eprintln!("get_report 0x{rnum:02x}");
                    }
                    // L2CAP: serve directly from the features cache filled
                    // during Session::open (no hidraw to forward to).
                    if l2cap {
                        eprintln!(
                            "get_report 0x{rnum:02x}: cache {}B",
                            features
                                .as_ref()
                                .and_then(|f| f.get(&rnum))
                                .map(|v| v.len())
                                .unwrap_or(0)
                        );
                        if let Some(feat) = features.as_ref().and_then(|f| f.get(&rnum)) {
                            let mut data = feat.clone();
                            if (rnum == 0x09 || rnum == 0x0b) && data.len() >= 7 {
                                for i in 0..6 {
                                    data[1 + i] = virtual_mac[5 - i];
                                }
                                if !usb_view {
                                    // MAC rewrite invalidates the trailing
                                    // CRC — restamp (kernel seed-0xA3 form).
                                    fix_bt_feature_crc_kernel(&mut data);
                                }
                            }
                            if usb_view && data.len() > 4 {
                                // USB feature reports have no CRC trailer. The
                                // BT reply is the SAME total length with the
                                // last 4 payload bytes replaced by CRC (the
                                // missing tail is zero padding on the real USB
                                // pad) — zero it back, keep the length.
                                let n = data.len();
                                data[n - 4..].fill(0);
                            }
                            eprintln!(
                                "get_report 0x{rnum:02x} ({}): {}B: {}",
                                if usb_view { "usb" } else { "bt" },
                                data.len(),
                                hid::hex(&data)
                            );
                            let _ = ipc::send(
                                sock,
                                ipc::TAG_GET_REPLY,
                                &ipc::enc_get_reply(id, 0, &data),
                            );
                        } else if let Some(c) = ctrl.as_mut() {
                            // Probe-time cache miss: fetch from the pad over
                            // the ctrl channel (blocks the relay ≤ ~1.5 s).
                            eprintln!("get_report 0x{rnum:02x}: cache miss — fetching from pad");
                            match l2cap::fetch_feature_once(c, rnum) {
                                Some(mut data) => {
                                    if let Some(f) = features.as_deref_mut() {
                                        f.insert(rnum, data.clone());
                                    }
                                    if (rnum == 0x09 || rnum == 0x0b) && data.len() >= 7 {
                                        for i in 0..6 {
                                            data[1 + i] = virtual_mac[5 - i];
                                        }
                                        if !usb_view {
                                            // MAC rewrite invalidates the
                                            // trailing CRC — restamp (kernel
                                            // seed-0xA3 form).
                                            fix_bt_feature_crc_kernel(&mut data);
                                        }
                                    }
                                    if usb_view && data.len() > 4 {
                                        // Same-total USB shape: zero the CRC
                                        // trailer, keep the length.
                                        let n = data.len();
                                        data[n - 4..].fill(0);
                                    }
                                    let _ = ipc::send(
                                        sock,
                                        ipc::TAG_GET_REPLY,
                                        &ipc::enc_get_reply(id, 0, &data),
                                    );
                                }
                                None => {
                                    eprintln!("get_report 0x{rnum:02x}: pad did not answer");
                                    let _ = ipc::send(
                                        sock,
                                        ipc::TAG_GET_REPLY,
                                        &ipc::enc_get_reply(id, libc::EINVAL as u16, &[]),
                                    );
                                }
                            }
                        } else {
                            eprintln!("get_report 0x{rnum:02x}: not in L2CAP features cache");
                            let _ = ipc::send(
                                sock,
                                ipc::TAG_GET_REPLY,
                                &ipc::enc_get_reply(id, libc::EINVAL as u16, &[]),
                            );
                        }
                        continue;
                    }
                    // hidraw: unknown ids and write-only reports (e.g. 0x0c) get a
                    // negative reply; everything else forwards to the real
                    // pad with the exact rdesc-derived length.
                    match flens.get(&rnum) {
                        Some(&len) => {
                            let mut buf = vec![0u8; len];
                            match hid::get_feature(real, rnum, &mut buf) {
                                Ok(n) => {
                                    let mut data = buf[..n].to_vec();
                                    if (rnum == 0x09 || rnum == 0x0b) && data.len() >= 7 {
                                        for i in 0..6 {
                                            data[1 + i] = virtual_mac[5 - i];
                                        }
                                        if info.transport == Transport::Bluetooth {
                                            fix_bt_feature_crc(&mut data);
                                        }
                                    }
                                    {
                                        // Games/wine poll feature reports
                                        // continuously (~4/s); log the first
                                        // few replies per process only.
                                        static N: std::sync::atomic::AtomicU32 =
                                            std::sync::atomic::AtomicU32::new(0);
                                        let n = N.fetch_add(1, Ordering::Relaxed);
                                        if n < 4 || n % 512 == 0 {
                                            eprintln!(
                                                "get_report 0x{rnum:02x} (usb): {}B: {}",
                                                data.len(),
                                                hid::hex(&data)
                                            );
                                        }
                                    }
                                    let _ = ipc::send(
                                        sock,
                                        ipc::TAG_GET_REPLY,
                                        &ipc::enc_get_reply(id, 0, &data),
                                    );
                                }
                                Err(e) => {
                                    eprintln!("get_report 0x{rnum:02x} from real pad: {e}");
                                    let _ = ipc::send(
                                        sock,
                                        ipc::TAG_GET_REPLY,
                                        &ipc::enc_get_reply(
                                            id,
                                            e.raw_os_error().unwrap_or(libc::EIO) as u16,
                                            &[],
                                        ),
                                    );
                                }
                            }
                        }
                        None => {
                            eprintln!("get_report 0x{rnum:02x}: not in rdesc, EINVAL");
                            let _ = ipc::send(
                                sock,
                                ipc::TAG_GET_REPLY,
                                &ipc::enc_get_reply(id, libc::EINVAL as u16, &[]),
                            );
                        }
                    }
                }
                Ok((ipc::TAG_SET_REQ, p)) => {
                    let Some((id, rnum, data)) = ipc::dec_set_req(&p) else {
                        continue;
                    };
                    eprintln!(
                        "set_report 0x{rnum:02x} req ({}B): {}",
                        data.len(),
                        hid::hex(&data)
                    );
                    // On BT the kernel emits its 0x31 outputs as SET_REPORT;
                    // route them through the sink writer unified sequence (same
                    // as TAG_OUTPUT) or the shared seq counter breaks and the
                    // pad drops rumble AND audio.
                    let err: u16 = if l2cap {
                        if data.len() == 78
                            && data.first() == Some(&0x31)
                            && sink::enqueue_output(&data)
                        {
                            // legacy kernel BT-format state (bus=USB now,
                            // but keep the shape working)
                            0
                        } else {
                            // Feature SET on L2CAP. Forwarding to the pad was
                            // disabled after it killed the ctrl channel — but
                            // that was always combined with the constant
                            // silence-0x36 stream (now gated). Retest vds
                            // parity: 0x53 + report with the feature CRC
                            // re-stamped in place. MDRV_SWALLOW_SET=1 falls
                            // back to the old swallow-with-ACK behavior.
                            if std::env::var("MDRV_SWALLOW_SET").is_ok() {
                                eprintln!(
                                    "set_report 0x{rnum:02x} ({}B) swallowed: {}",
                                    data.len(),
                                    hid::hex(&data)
                                );
                                0
                            } else {
                                let mut body = data.clone();
                                // BT form keeps the SAME total length as the
                                // USB shape: stamp the host→pad CRC over the
                                // last 4 bytes in place (for USB-shaped SETs
                                // this replaces zero payload tail bytes).
                                fix_bt_feature_crc(&mut body);
                                let sent = match ctrl.as_mut() {
                                    Some(c) => {
                                        let a = c.write_all(&[0x53]);
                                        let b = if a.is_ok() {
                                            c.write_all(&body)
                                        } else {
                                            Ok(())
                                        };
                                        a.is_ok() && b.is_ok()
                                    }
                                    None => false,
                                };
                                if sent {
                                    eprintln!(
                                        "set_report 0x{rnum:02x} ({}B) forwarded to ctrl: {}",
                                        data.len(),
                                        hid::hex(&body)
                                    );
                                    0
                                } else {
                                    eprintln!("set_report 0x{rnum:02x} forward failed — ctrl gone");
                                    libc::EIO as u16
                                }
                            }
                        }
                    } else if info.transport == Transport::Bluetooth
                        && data.len() == 78
                        && data.first() == Some(&0x31)
                        && sink::enqueue_output(&data)
                    {
                        0
                    } else {
                        match real.write_all(&data) {
                            Ok(()) => {
                                eprintln!("set_report 0x{rnum:02x} relayed ({}B)", data.len());
                                0
                            }
                            Err(e) => {
                                eprintln!("set_report 0x{rnum:02x} write failed: {e}");
                                libc::EIO as u16
                            }
                        }
                    };
                    let _ = ipc::send(sock, ipc::TAG_SET_ACK, &ipc::enc_ack(id, err));
                }
                Ok((ipc::TAG_NOTICE, m)) => {
                    eprintln!("holder: {}", String::from_utf8_lossy(&m));
                }
                Ok(_) => {}
                Err(_) => {
                    eprintln!("holder connection lost");
                    return 3;
                }
            }
        }

        if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            eprintln!("holder connection lost");
            return 3;
        }

        // XInput holder link: rumble + control-plane replies. Any error
        // here only disables the tap until the next reload — never the
        // DS relay.
        if fds[3].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 && xi.sock.is_some()
        {
            let mut xs = xi.sock.take().unwrap();
            match ipc::recv(&mut xs) {
                Ok((ipc::TAG_OUTPUT, data)) => {
                    if let Some((strong, weak)) = crate::xinput::decode_rumble(&data) {
                        if !sink::rumble(strong, weak) {
                            // USB DualSense: no sink — write the 48 B 0x02
                            // output report directly, like a game would.
                            // (DS4 sessions have no DS5-shaped output path;
                            // rumble there stays on the DS side.)
                            if info.transport == Transport::Usb
                                && info.product == crate::hid::PRODUCT_DUALSENSE
                            {
                                let _ = real.write_all(&crate::xinput::rumble_report(strong, weak));
                            } else {
                                eprintln!("xinput: rumble dropped (no sink)");
                            }
                        }
                    } // LED packets and other non-rumble writes: swallowed
                    xi.sock = Some(xs);
                }
                Ok((ipc::TAG_CREATE_ACK, a)) => {
                    if a.first() == Some(&2) {
                        eprintln!(
                            "xinput: create failed: {}",
                            String::from_utf8_lossy(&a[1..])
                        );
                    }
                    xi.sock = Some(xs);
                }
                Ok((ipc::TAG_GET_REQ, p)) => {
                    if let Some((id, _)) = ipc::dec_req(&p) {
                        let _ = ipc::send(
                            &mut xs,
                            ipc::TAG_GET_REPLY,
                            &ipc::enc_get_reply(id, libc::EINVAL as u16, &[]),
                        );
                    }
                    xi.sock = Some(xs);
                }
                Ok((ipc::TAG_SET_REQ, p)) => {
                    if let Some((id, _, _)) = ipc::dec_set_req(&p) {
                        let _ = ipc::send(&mut xs, ipc::TAG_SET_ACK, &ipc::enc_ack(id, 0));
                    }
                    xi.sock = Some(xs);
                }
                Ok((ipc::TAG_NOTICE, m)) => {
                    eprintln!("xinput holder: {}", String::from_utf8_lossy(&m));
                    xi.sock = Some(xs);
                }
                Ok(_) => {
                    xi.sock = Some(xs);
                }
                Err(e) => {
                    eprintln!("xinput: holder link error ({e}) — tap disabled until reload");
                }
            }
        }

        // L2CAP control channel: keep it drained for the whole session
        // (vds handle_bt_control): every 0xA3 frame is cached — unprompted
        // too — so later GET_REQs are served fresh; other frames logged.
        // A closed/erroring ctrl channel means the pad tore the HID session
        // down: reconnect.
        if ctrl.is_some() {
            if fds[2].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                eprintln!("l2cap: control channel closed — session gone");
                return 1;
            }
            if fds[2].revents & libc::POLLIN != 0 {
                let c = ctrl.as_mut().unwrap();
                match c.read(&mut ctrl_buf) {
                    Ok(0) => {
                        eprintln!("l2cap: control channel EOF — session gone");
                        return 1;
                    }
                    Ok(n) => {
                        let frame = &ctrl_buf[..n];
                        if frame.len() >= 2 && frame[0] == 0xA3 {
                            let id = frame[1];
                            let rep = frame[1..].to_vec();
                            eprintln!("l2cap: ctrl feature 0x{id:02x} ({} B) cached", rep.len());
                            if let Some(f) = features.as_deref_mut() {
                                f.insert(id, rep);
                            }
                        } else {
                            eprintln!("l2cap: ctrl frame ({} B): {}", n, hid::hex(frame));
                        }
                    }
                    Err(e) => {
                        eprintln!("l2cap: ctrl read: {e}");
                        return 1;
                    }
                }
            }
        }

        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            // Real pad gone: return 1 — run() drops the holder socket at the
            // end of this cycle, and the holder relays the stored NEUTRAL
            // report then (kernel state can never freeze at last-pressed
            // values, even across proxy restarts).
            eprintln!(
                "real pad fd hangup: POLLHUP|POLLERR revents={} — pad dropped the link",
                fds[0].revents
            );
            return 1;
        }
    }
}

/// Neutralize gameplay fields (sticks, triggers, buttons) while gate is active.
/// Trigger analog bytes sit at stick0+4/stick0+5 within the 6-axis block.
/// Any face/shoulder button down (hat nibble excluded) or a trigger past
/// the idle threshold — used by the hold-break so the frame that closes
/// the overlay doesn't surface in-game as a fresh press.
fn input_dbg() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("MDRV_INPUT_DEBUG").as_deref() == Ok("1"))
}

fn buttons_live(report: &[u8], l: &InputLayout) -> bool {
    if report.len() < l.btn0 + 3 {
        return false;
    }
    if report[l.btn0] & 0xF0 != 0 || report[l.btn0 + 1] != 0 || report[l.btn0 + 2] != 0 {
        return true;
    }
    // triggers: L2/R2 live at stick0+4/+5
    report.len() >= l.stick0 + 6 && (report[l.stick0 + 4] > 0x10 || report[l.stick0 + 5] > 0x10)
}

fn gate_input(report: &mut [u8], l: &InputLayout) {
    if report.len() < l.btn0 + 3 {
        return;
    }
    for i in l.stick0..l.stick0 + 6 {
        report[i] = 0x80;
    }
    report[l.btn0] = 0x08; // hat neutral
    report[l.btn0 + 1] = 0x00;
    report[l.btn0 + 2] = 0x00;
}

/// Per-axis stick shaping (inner + outer deadzone, linear in between):
/// |v| ≤ inner → centre; |v| ≥ outer → full deflection; else rescaled
/// (v-inner)/(outer-inner) so the curve is continuous — with the 0.0/0.9
/// defaults, 0.45 maps to 0.5. Sticks are 8-bit, centre 0x80.
fn scale_sticks(report: &mut [u8], l: &InputLayout, inner: f32, outer: f32) {
    let inner = inner.clamp(0.0, 0.95);
    let outer = outer.clamp(inner + 0.01, 1.0);
    let span = outer - inner;
    for base in [l.stick0, l.stick0 + 2] {
        if report.len() < base + 2 {
            continue;
        }
        for o in 0..2 {
            let v = (report[base + o] as f32 - 128.0) / 128.0;
            let a = v.abs();
            let out = if a <= inner {
                0.0
            } else if a >= outer {
                1.0
            } else {
                (a - inner) / span
            };
            let scaled = out * v.signum();
            report[base + o] = (128.0 + scaled * 128.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Is /dev/null currently bind-mounted over this node?
fn node_mounted(node: &Path) -> bool {
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return false;
    };
    let n = node.to_string_lossy();
    mounts
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(n.as_ref()))
}

fn read_gate(path: &Path) -> bool {
    match fs::read_to_string(path) {
        Ok(s) => {
            let s = s.trim();
            !s.is_empty() && s != "none"
        }
        Err(_) => false,
    }
}

/// Recompute the Bluetooth feature-report CRC32 the vds way (host→pad SET
/// packets): crc32_seeded over the report bytes with seed 0xeada2d49's
/// equivalent init, complemented, stored LE in the last 4 bytes.
fn fix_bt_feature_crc(buf: &mut [u8]) {
    if buf.len() < 5 {
        return;
    }
    let crc = bt_feature_crc(&buf[..buf.len() - 4]);
    let n = buf.len();
    buf[n - 4..n].copy_from_slice(&crc.to_le_bytes());
}

/// Sony feature-report CRC over `data` (report id first, no 0xA3/0x53 prefix):
/// crc32_seeded(data, 0x2060efc3) complemented — the LE value the pad expects
/// in the trailing 4 bytes of a feature report.
fn bt_feature_crc(data: &[u8]) -> u32 {
    let mut crc = 0xDF9F_103Cu32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & if crc & 1 != 0 { u32::MAX } else { 0 });
        }
    }
    !crc
}

/// Kernel-convention feature CRC (hid-playstation ps_check_crc32, seed byte
/// 0xA3): standard reflected CRC32 of [0xA3] ++ data, complemented — the LE
/// value the KERNEL expects in the trailing 4 bytes of a BT feature reply.
fn bt_feature_crc_kernel(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for b in std::iter::once(&0xA3u8).chain(data.iter()) {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & if crc & 1 != 0 { u32::MAX } else { 0 });
        }
    }
    !crc
}

fn fix_bt_feature_crc_kernel(buf: &mut [u8]) {
    let n = buf.len();
    if n < 5 {
        return;
    }
    let crc = bt_feature_crc_kernel(&buf[..n - 4]);
    buf[n - 4..n].copy_from_slice(&crc.to_le_bytes());
}

fn derive_virtual_mac(real: &str) -> String {
    let parts: Vec<&str> = real.split(':').collect();
    if parts.len() == 6 {
        if let Ok(first) = u8::from_str_radix(parts[0], 16) {
            return format!("{:02x}", first ^ 0x02) + ":" + &parts[1..].join(":");
        }
    }
    "aa:bb:cc:dd:ee:01".to_string()
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}
