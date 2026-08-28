//! Control bridge between the mdrv-ds proxy and the suite's UI overlays
//! (mdrv-ds-audio's mixer, mdrv-ds-notify's history panel, mdrv-ds-launcher).
//!
//! This side owns the *bridge* socket. Overlays connect as clients and pick
//! a mode:
//!
//! * **hold** — the classic overlay mode: while ANY hold client is
//!   connected the proxy suppresses input forwarding to the virtual pad
//!   (the game sees nothing) and streams button/stick events instead.
//!   Resume happens when the last hold client disconnects (or switches to
//!   watch mode). Chords keep working while paused because they are fed on
//!   the raw report before the pause gate.
//! * **watch** — non-pausing: the game keeps running, but the client still
//!   receives button edges (including PS, which games never see). Used by
//!   mdrv-ds-launcher to toggle its own visibility on the PS button.
//!
//! Mode is chosen by the client's first line (`mode watch` / `mode hold`,
//! within the negotiation window) and may be switched at any time with
//! the same line. Clients that connect silently default to hold (the
//! original protocol).

use crate::proxy::InputLayout;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static PAUSED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// buttons only, game keeps running
    Watch,
    /// buttons + sticks, game input suppressed while any hold client lives
    Hold,
}

fn parse_mode(line: &str) -> Option<Mode> {
    match line.trim() {
        "mode watch" => Some(Mode::Watch),
        "mode hold" => Some(Mode::Hold),
        _ => None,
    }
}

/// Wire names for pad buttons, grouped by the three button bytes of the BT
/// input report ([btn0], [btn0+1], [btn0+2]).
const BTN_MAP: [[(u8, &str); 4]; 3] = [
    [
        (0x10, "square"),
        (0x20, "cross"),
        (0x40, "circle"),
        (0x80, "triangle"),
    ],
    [
        (0x01, "l1"),
        (0x02, "r1"),
        (0x10, "share"),
        (0x20, "options"),
    ],
    [(0x01, "ps"), (0x02, "touchpad"), (0x40, "l3"), (0x80, "r3")],
];

struct Client {
    fd: i32,
    conn: UnixStream,
    mode: Mode,
}

struct BridgeState {
    /// connected overlay clients
    clients: Vec<Client>,
    /// last seen button-byte snapshot for edge detection (shared by all
    /// clients; reset when the first client of a session connects)
    prev_btn: [u8; 3],
    /// last seen d-pad hat value (low nibble of first button byte)
    prev_hat: u8,
    /// last sent stick values (raw 0..255)
    prev_stick: [u8; 2],
}

impl BridgeState {
    const fn new() -> Self {
        Self {
            clients: Vec::new(),
            prev_btn: [0; 3],
            prev_hat: 0,
            prev_stick: [128, 128],
        }
    }

    fn any_hold(&self) -> bool {
        self.clients.iter().any(|c| c.mode == Mode::Hold)
    }
}

static STATE: Mutex<BridgeState> = Mutex::new(BridgeState::new());

fn bridge_path() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(dir).join("mdrv-ds-bridge.sock")
}

/// Spawn the accept-loop thread. Call once per proxy process.
pub fn start() {
    let path = bridge_path();
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else {
        eprintln!(
            "bridge: cannot bind {} — overlay pause unavailable",
            path.display()
        );
        return;
    };
    eprintln!("bridge: listening on {}", path.display());
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(conn) = client else {
                continue;
            };
            accept_client(conn);
        }
    });
}

fn accept_client(conn: UnixStream) {
    // Mode negotiation: suite clients send their mode line immediately
    // after connecting. Wait briefly; silence means hold (legacy clients
    // never write). A late mode line still lands via the reader thread.
    let mut mode = Mode::Hold;
    if conn
        .set_read_timeout(Some(std::time::Duration::from_millis(250)))
        .is_ok()
    {
        let mut probe = match conn.try_clone() {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut reader = BufReader::new(&mut probe);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            if let Some(m) = parse_mode(&line) {
                mode = m;
            }
        }
        let _ = conn.set_read_timeout(None);
    }

    let key = conn.as_raw_fd();
    let watch = conn.try_clone().ok();
    let was_empty = {
        let mut st = STATE.lock().unwrap();
        let was = st.clients.is_empty();
        if was {
            // fresh overlay session: restart edge detection
            st.prev_btn = [0; 3];
            st.prev_hat = 0;
            st.prev_stick = [128, 128];
        }
        st.clients.push(Client {
            fd: key,
            conn,
            mode,
        });
        sync_paused(&st);
        was
    };
    eprintln!(
        "bridge: client connected (mode={:?}){}",
        mode,
        if was_empty { "" } else { " — additional" }
    );

    // Reader thread on a clone: watches for EOF AND applies runtime mode
    // switches ("mode watch" / "mode hold" lines).
    if let Some(watch) = watch {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(watch);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Some(m) = parse_mode(&line) {
                            let mut st = STATE.lock().unwrap();
                            if let Some(c) = st.clients.iter_mut().find(|c| c.fd == key) {
                                if c.mode != m {
                                    c.mode = m;
                                    eprintln!("bridge: client switched to {m:?}");
                                }
                                sync_paused(&st);
                            }
                        }
                    }
                }
            }
            let mut st = STATE.lock().unwrap();
            st.clients.retain(|c| c.fd != key);
            sync_paused(&st);
            eprintln!("bridge: client disconnected");
        });
    }
}

/// Recompute the global pause flag from the client list (state locked).
fn sync_paused(st: &BridgeState) {
    let any_hold = st.any_hold();
    let was = PAUSED.swap(any_hold, Ordering::SeqCst);
    if was && !any_hold {
        eprintln!("bridge: game input resumed");
    } else if !was && any_hold {
        eprintln!("bridge: game input paused (hold client active)");
    }
}

pub fn paused() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

/// Extract button/stick deltas from one control report and push them to
/// the connected clients. Called from the relay loop for EVERY control
/// report, before the PS-swallow strip:
///
/// * button edges (incl. PS) go to all clients (watch mode needs PS);
/// * stick samples go to hold clients only;
/// * game input suppression is NOT handled here — the relay loop calls
///   `paused()` separately and drops the forward while any hold client
///   is connected.
pub fn feed(report: &[u8], layout: &InputLayout) {
    if report.len() < layout.btn0 + 3 || report.len() < layout.stick0 + 2 {
        return;
    }
    let b = &report[layout.btn0..layout.btn0 + 3];
    let lx = report[layout.stick0];
    let ly = report[layout.stick0 + 1];

    let mut st = STATE.lock().unwrap();
    if st.clients.is_empty() {
        return;
    }
    let want_sticks = st.any_hold();

    let mut btn_lines = String::new();
    for k in 0..3 {
        let changed = b[k] ^ st.prev_btn[k];
        for (mask, name) in BTN_MAP[k] {
            if changed & mask != 0 {
                btn_lines.push_str(&format!("pad {name} {}\n", u8::from(b[k] & mask != 0)));
            }
        }
    }
    st.prev_btn = [b[0], b[1], b[2]];
    // D-pad: hat-switch low nibble of the first button byte (both pads).
    // Compass encoding: 0=N,1=NE,2=E,3=SE,4=S,5=SW,6=W,7=NW; >=8 = neutral.
    let hat = b[0] & 0x0f;
    if hat_dirs(hat) != hat_dirs(st.prev_hat) {
        const DIRS: [(usize, &str); 4] = [(0, "up"), (1, "right"), (2, "down"), (3, "left")];
        for (i, name) in DIRS {
            if hat_dirs(hat)[i] != hat_dirs(st.prev_hat)[i] {
                btn_lines.push_str(&format!("pad {name} {}\n", u8::from(hat_dirs(hat)[i])));
            }
        }
        st.prev_hat = hat;
    }

    let mut stick_lines = String::new();
    if want_sticks && (lx.abs_diff(st.prev_stick[0]) >= 2 || ly.abs_diff(st.prev_stick[1]) >= 2) {
        let nx = ((i16::from(lx) - 128) as f32 / 127.).clamp(-1.0, 1.0);
        let ny = ((i16::from(ly) - 128) as f32 / 127.).clamp(-1.0, 1.0);
        stick_lines.push_str(&format!("stick {nx:.3} {ny:.3}\n"));
        st.prev_stick = [lx, ly];
    }

    if btn_lines.is_empty() && stick_lines.is_empty() {
        return;
    }
    let full_lines = format!("{btn_lines}{stick_lines}");
    // Fan out; drop clients that went away.
    let mut dead: Vec<i32> = Vec::new();
    for c in st.clients.iter_mut() {
        let payload = if c.mode == Mode::Hold {
            full_lines.as_str()
        } else {
            btn_lines.as_str()
        };
        if payload.is_empty() {
            continue;
        }
        let _ = c
            .conn
            .set_write_timeout(Some(std::time::Duration::from_millis(20)));
        if c.conn.write_all(payload.as_bytes()).is_err() {
            dead.push(c.fd);
        }
    }
    if !dead.is_empty() {
        st.clients.retain(|c| !dead.contains(&c.fd));
        sync_paused(&st);
    }
}

/// Map a hat-switch compass value to [up, right, down, left].
pub(crate) fn hat_dirs(v: u8) -> [bool; 4] {
    if v > 7 {
        return [false; 4];
    }
    [
        matches!(v, 0 | 1 | 7),
        matches!(v, 1 | 2 | 3),
        matches!(v, 3 | 4 | 5),
        matches!(v, 5 | 6 | 7),
    ]
}
