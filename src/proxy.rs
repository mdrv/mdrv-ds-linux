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
        libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as libc::sighandler_t);
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
    /// on reload, treat first report as "everything already pressed" (no refire)
    skip_edges: bool,
    /// [notify] event-callback config (reported on every chord fire)
    notify: config::NotifyConfig,
}

/// Analog-trigger press threshold (0-255); released rests at 0.
const TRIG_THRESHOLD: u8 = 0x40;

impl ChordState {
    fn new(bindings: Vec<(String, usize, u8, String)>, notify: config::NotifyConfig) -> Self {
        let n = bindings.len();
        ChordState {
            bindings,
            prev_pressed: vec![false; n],
            swallow: [0; 3],
            swallow_trig: [false; 2],
            skip_edges: true,
            notify,
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
            // L2/R2 are analog (no digital bit): pressed = value over threshold
            let pressed = match *byte {
                config::TRIG_L2 => report
                    .get(l.stick0 + 4)
                    .is_some_and(|&v| v > TRIG_THRESHOLD),
                config::TRIG_R2 => report
                    .get(l.stick0 + 5)
                    .is_some_and(|&v| v > TRIG_THRESHOLD),
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
                } else {
                    self.swallow[*byte] |= bit;
                }
                self.swallow[2] |= 0x01;
            }
            self.prev_pressed[i] = pressed;
        }
        if !ps_down {
            self.swallow = [0; 3];
            self.swallow_trig = [false; 2];
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
        if self.swallow_trig[0] && report.len() > l.stick0 + 4 {
            report[l.stick0 + 4] = 0;
        }
        if self.swallow_trig[1] && report.len() > l.stick0 + 5 {
            report[l.stick0 + 5] = 0;
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
        .args(["-n", "/usr/local/bin/mdrv-gm-hide-hidraw", &p, action])
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
const DS_PRODUCT: u32 = 0x0ce6;

// gameplay-field offsets relative to report start, per transport
// USB input: [0]=0x01, sticks 1..=6, buttons 8/9/10
// BT input:  [0]=0x31, [1]=seq, sticks 2..=7, buttons 9/10/11
struct InputLayout {
    stick0: usize,
    btn0: usize,
    report_id: u8,
}

fn layout_for(transport: Transport) -> InputLayout {
    match transport {
        Transport::Usb => InputLayout {
            stick0: 1,
            btn0: 8,
            report_id: 0x01,
        },
        Transport::Bluetooth => InputLayout {
            stick0: 2,
            btn0: 9,
            report_id: 0x31,
        },
    }
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
    Some(r)
}

pub fn run(opts: ProxyOpts) -> i32 {
    install_signal_handlers();
    tee_stderr_to_file("proxy.log");
    write_pidfile();
    cleanup_stale_mounts();
    let cfg = config::load();
    let mut chords = ChordState::new(config::parse_chords(&cfg).list, cfg.notify.clone());
    let mut ps_swallow = cfg.ps.swallow();
    let mut tone: Option<audio::Tone> = None;
    let mut audio_sink: Option<sink::Sink> = None;
    if !chords.bindings.is_empty() {
        eprintln!("config: {} chord(s) loaded", chords.bindings.len());
    }
    if ps_swallow {
        eprintln!("config: ps=swallow (PS hidden from games/mdrv-gm)");
    }
    // Persistent-virtual-pad state moved into the holder daemon.
    loop {
        let (mut real, info) = match open_real(&opts.pad) {
            OpenReal::Ok(f, i) => (f, i),
            OpenReal::Waiting => {
                eprintln!("waiting for DualSense…");
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            OpenReal::Masked(msg) => {
                eprintln!("FATAL: {msg}");
                return 1;
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

        let virtual_uniq = derive_virtual_mac(&info.uniq);
        let virtual_mac: [u8; 6] =
            parse_mac(&virtual_uniq).unwrap_or([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01]);
        let layout = layout_for(info.transport);

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
            bus: match info.transport {
                Transport::Usb => uhid::BUS_USB,
                Transport::Bluetooth => uhid::BUS_BLUETOOTH,
            },
            vendor: DS_VENDOR,
            product: DS_PRODUCT,
            version: info.fw_version,
            name: "mdrv-ds Virtual DualSense".to_string(),
            uniq: virtual_uniq.clone(),
            rdesc: info.rdesc.clone(),
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

        // Device nodes to hide: the real hidraw + every evdev node (event*/js*)
        // under its HID device. Bind-mounting /dev/null makes future open()s
        // fail/EOF, so games can only see the virtual pad. We already hold the
        // hidraw fd; evdev nodes aren't needed after open.
        // Grab the REAL touchpad evdev node BEFORE masking: EVIOCGRAB affects
        // all clients of the device, so this silences libinput fds opened
        // before we masked the node (masking only blocks future opens).
        // Kept alive for this connection's whole lifetime.
        let _touchpad_grab = mouse::grab_permanently(&mouse::touchpad_nodes_of(&info.path));

        let hidden: Vec<PathBuf> = if opts.hide {
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
        }

        // Phase-1 BT audio: continuous 440 Hz test tone (config-gated).
        if let Some(t) = tone.as_mut() {
            t.stop();
        }
        tone = None;
        if matches!(info.transport, Transport::Bluetooth) && cfg.audio.test_tone {
            tone = Some(audio::start(&real, &cfg.audio));
        }

        // Phase-2 BT audio: PipeWire sink → Opus ladder (speaker/jack).
        if let Some(s) = audio_sink.as_mut() {
            s.stop();
        }
        audio_sink = None;
        if matches!(info.transport, Transport::Bluetooth) {
            audio_sink = Some(sink::start(
                &real,
                &cfg.audio,
                &hid::feature_lengths(&info.rdesc),
            ));
        }

        let code = relay_loop(
            &mut real,
            &mut sock,
            &info,
            &gate,
            &opts,
            &virtual_mac,
            &mut chords,
            &mut ps_swallow,
        );
        // Undo the real pad's node masking; the virtual pad itself is owned
        // by the holder and outlives this proxy process.
        for n in &hidden {
            hide_hidraw(n, "unbind");
        }
        drop(sock); // holder emits the stored neutral report on this EOF
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
                    "{p} opened but not a hidraw ({e}): a leftover /dev/null hide-mount\nis masking the node and a running client (game/Steam) holds it busy.\nClose the holders and rerun, or: sudo /usr/local/bin/mdrv-gm-hide-hidraw {p} unbind"
                )),
            }
        }
        Err(e) => {
            eprintln!("open {p}: {e}");
            OpenReal::Waiting
        }
    }
}

fn relay_loop(
    real: &mut File,
    sock: &mut UnixStream,
    info: &PadInfo,
    gate: &Arc<AtomicBool>,
    opts: &ProxyOpts,
    virtual_mac: &[u8; 6],
    chords: &mut ChordState,
    ps_swallow: &mut bool,
) -> i32 {
    let layout = layout_for(info.transport);
    let flens = hid::feature_lengths(&info.rdesc);
    let real_fd = real.as_raw_fd();
    let sock_fd = sock.as_raw_fd();
    let mut in_buf = [0u8; 512];
    let mut eofs: u32 = 0;

    loop {
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
        ];
        if EXIT.load(Ordering::Relaxed) {
            return 2;
        }
        if RELOAD.swap(false, Ordering::Relaxed) {
            let cfg = config::load();
            *chords = ChordState::new(config::parse_chords(&cfg).list, cfg.notify.clone());
            *ps_swallow = cfg.ps.swallow();
            eprintln!(
                "config reloaded: {} chord(s), ps={}",
                chords.bindings.len(),
                if *ps_swallow { "swallow" } else { "pass" }
            );
        }
        let ret = uhid::poll(&mut fds, 250);
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
                    let mut report = in_buf[..n].to_vec();
                    // Jack detect for the BT audio sink: input 0x31 byte 56
                    // bit0 (kernel dualsense_input_report.status[1]; the
                    // spec's "byte 55" is off by one). Inert on USB (0x01).
                    if report.len() > 56 && report[0] == 0x31 {
                        sink::JACK_PLUGGED.store(report[56] & 0x01 != 0, Ordering::Relaxed);
                        // Mic-variant 0x31s (audio sections enabled, low
                        // nibble 0x03) are Opus mic data the kernel misparses
                        // as button presses — never relay them.
                        if report[1] & 0x0F == 0x03 {
                            continue;
                        }
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
                    // PS allowlist: "swallow" strips the PS bit from every
                    // relayed report, so games/mdrv-gm never see PS at
                    // all — only mdrv-ds chords (fed above, raw) do.
                    if *ps_swallow {
                        report[layout.btn0 + 2] &= !0x01;
                    }
                    if gate.load(Ordering::Relaxed) {
                        gate_input(&mut report, &layout);
                    } else if opts.deadzone {
                        clamp_sticks(&mut report, &layout);
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
                    if opts.verbose && !data.is_empty() {
                        eprintln!("out ({}B): {}", data.len(), hid::hex(&data));
                    }
                    // On BT with the audio sink running, hand kernel output
                    // reports to the sink writer so ALL interrupt-channel
                    // reports share one sequence counter (two independent
                    // counters make the pad drop reports — rumble AND audio).
                    if info.transport == Transport::Bluetooth
                        && data.len() == 78
                        && data.first() == Some(&0x31)
                        && sink::enqueue_output(&data)
                    {
                        // sent via the sink writer
                    } else if let Err(e) = real.write_all(&data) {
                        eprintln!("hidraw write output: {e}");
                    }
                }
                Ok((ipc::TAG_GET_REQ, p)) => {
                    let Some((id, rnum)) = ipc::dec_req(&p) else {
                        continue;
                    };
                    if opts.verbose {
                        eprintln!("get_report 0x{rnum:02x}");
                    }
                    // Unknown ids and write-only reports (e.g. 0x0c) get a
                    // negative reply; everything else forwards to the real
                    // pad with the exact rdesc-derived length.
                    match flens.get(&rnum) {
                        Some(&len) => {
                            let mut buf = vec![0u8; len];
                            match hid::get_feature(real, rnum, &mut buf) {
                                Ok(n) => {
                                    let mut data = buf[..n].to_vec();
                                    // 0x09 (pairing) and 0x0b (identity)
                                    // embed the pad MAC reversed at [1..7].
                                    // hid-playstation dedups pads by this
                                    // MAC — leak the real one and the
                                    // virtual pad dies with -EEXIST.
                                    if (rnum == 0x09 || rnum == 0x0b) && data.len() >= 7 {
                                        for i in 0..6 {
                                            data[1 + i] = virtual_mac[5 - i];
                                        }
                                        // Over Bluetooth hid-playstation
                                        // validates a CRC32 (seed 0xA3,
                                        // kernel ps_check_crc32) over the
                                        // report with the LE32 result in
                                        // the last 4 bytes — the rewrite
                                        // above just invalidated the pad's
                                        // own CRC, so recompute it.
                                        if info.transport == Transport::Bluetooth {
                                            fix_bt_feature_crc(&mut data);
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
                    if opts.verbose {
                        eprintln!(
                            "set_report 0x{rnum:02x} ({}B): {}",
                            data.len(),
                            hid::hex(&data)
                        );
                    }
                    // On BT the kernel emits its 0x31 outputs as SET_REPORT;
                    // route them through the sink writer unified sequence (same
                    // as TAG_OUTPUT) or the shared seq counter breaks and the
                    // pad drops rumble AND audio.
                    let err: u16 = if info.transport == Transport::Bluetooth
                        && data.len() == 78
                        && data.first() == Some(&0x31)
                        && sink::enqueue_output(&data)
                    {
                        0
                    } else {
                        match real.write_all(&data) {
                            Ok(()) => {
                                if opts.verbose {
                                    eprintln!("set_report 0x{rnum:02x} relayed ({}B)", data.len());
                                }
                                0
                            }
                            Err(e) => {
                                eprintln!("set_report 0x{rnum:02x} write failed: {e}");
                                libc::EIO as u16
                            }
                        }
                    };
                    let _ = ipc::send(sock, ipc::TAG_SET_ACK, &ipc::enc_ack(id, err));
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

        if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            // Real pad gone: return 1 — run() drops the holder socket at the
            // end of this cycle, and the holder relays the stored NEUTRAL
            // report then (kernel state can never freeze at last-pressed
            // values, even across proxy restarts).
            return 1;
        }
    }
}

/// Neutralize gameplay fields (sticks, triggers, buttons) while gate is active.
/// Trigger analog bytes sit at stick0+4/stick0+5 within the 6-axis block.
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

const STICK_DEADZONE: f32 = 0.05;

/// Radial stick deadzone: clamp near-centre pairs to exact centre.
fn clamp_sticks(report: &mut [u8], l: &InputLayout) {
    for base in [l.stick0, l.stick0 + 2] {
        if report.len() < base + 2 {
            continue;
        }
        let dx = (report[base] as f32 - 128.0) / 128.0;
        let dy = (report[base + 1] as f32 - 128.0) / 128.0;
        if dx * dx + dy * dy < STICK_DEADZONE * STICK_DEADZONE {
            report[base] = 0x80;
            report[base + 1] = 0x80;
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

/// Toggle the locally-administered bit so the virtual MAC differs from the
/// real one (hid-playstation dedups pads by MAC).
/// Recompute the Bluetooth feature-report CRC32 the way hid-playstation
/// validates it (ps_check_crc32): crc32_le(0xFFFFFFFF, [0xA3]) continued
/// over the report bytes, complemented, stored LE in the last 4 bytes.
fn fix_bt_feature_crc(buf: &mut [u8]) {
    if buf.len() < 5 {
        return;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for b in std::iter::once(0xA3u8).chain(buf[..buf.len() - 4].iter().copied()) {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & if crc & 1 != 0 { u32::MAX } else { 0 });
        }
    }
    let n = buf.len();
    buf[n - 4..n].copy_from_slice(&(!crc).to_le_bytes());
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
