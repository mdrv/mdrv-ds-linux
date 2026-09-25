//! mdrv-ds configuration (`$XDG_CONFIG_HOME/mdrv-ds/config.toml`).
//!
//! Currently only PS-button chords: `PS + <button> = action`, fired by the
//! proxy while relaying input. Actions are shell commands (`shell:<cmd>`);
//! the fired button and the PS press are swallowed from the relayed report
//! (until PS is released), so neither the game nor mdrv-gm's PS-tap sees
//! them.
//!
//! Reload with `mdrv-ds keymap reload` (SIGHUP); parse failures keep the
//! previous config.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// "hidraw" (default — relay through the kernel hid-playstation node,
    /// bluetoothd owns the L2CAP session) or "l2cap" (self-owned HID
    /// session: bind PSM 0x11/0x13 ourselves, bluetoothd runs with
    /// --noplugin=input; requires scripts/bt-input-off.sh + setcap).
    #[serde(default)]
    pub transport: Option<String>,
    /// Present the virtual pad on a different bus than the physical link:
    /// "usb" makes an L2CAP (Bluetooth) DualSense session create the virtual
    /// pad as bus=USB with the USB report descriptor, translates BT input
    /// frames to USB input reports and game USB outputs back to BT — for
    /// games (RE Engine: PRAGMATA) that gate adaptive triggers/haptics on
    /// "pad is Bluetooth". None/"bluetooth" = native presentation (default;
    /// FF16 requires this). Applies to DualSense L2CAP sessions only; takes
    /// effect on the next pad session.
    #[serde(default)]
    pub force_bus: Option<String>,
    /// Swap the cross (X) and circle (O) face buttons on the virtual pad —
    /// JP-style confirm (O confirms) on games that use the western layout.
    /// Applied in the relay before ANY consumer sees the report (game HID
    /// reads, kernel evdev, chords), so games visibly respond to the
    /// swapped button. DualSense chimera sessions only; default false.
    #[serde(default)]
    pub swap_cross_circle: Option<bool>,
    #[serde(default)]
    pub chords: HashMap<String, String>,
    #[serde(default)]
    pub ps: PsConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub input: InputConfig,
    /// XInput pad emulation: expose a SECOND virtual pad (Xbox 360 wired,
    /// 045e:028e) translated from the live DualSense stream so XInput-only
    /// games work under wine/Proton without Steam Input. Strictly additive:
    /// a separate holder instance owns the device; the DS-native path is
    /// untouched. Default false; live-toggle via `mdrv-ds xinput on|off|reset`.
    #[serde(default)]
    pub xinput: Option<bool>,
}

impl Config {
    pub fn l2cap(&self) -> bool {
        self.transport.as_deref() == Some("l2cap")
    }

    /// True when XInput emulation is enabled by config (the runtime
    /// override file can still flip it live — see xinput::effective).
    pub fn xinput(&self) -> bool {
        self.xinput.unwrap_or(false)
    }

    /// True when force_bus asks for a USB-presented virtual pad.
    /// Unknown values are rejected with a warning (native presentation).
    pub fn force_usb(&self) -> bool {
        match self.force_bus.as_deref() {
            Some(v) => match v {
                "usb" => true,
                "bluetooth" | "" => false,
                other => {
                    eprintln!("config: force_bus: unknown value {other:?} (want \"usb\" or \"bluetooth\") — ignoring");
                    false
                }
            },
            None => false,
        }
    }
}

/// `[input]` table — analog-stick shaping applied on relay (all pads,
/// both transports). Values are fractions of stick half-range.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct InputConfig {
    /// Master switch: false = NO translation at all — raw stick bytes are
    /// relayed verbatim (pre-deadzone-feature behaviour).
    #[serde(default = "dz_enabled")]
    pub enabled: bool,
    /// Inner deadzone 0.0..~0.95: |v| ≤ inner maps to exact centre.
    #[serde(default = "dz_zero")]
    pub inner_dz: f32,
    /// Outer deadzone 0.0..1.0: |v| ≥ outer maps to FULL deflection, so
    /// pads whose sticks never physically reach 1.0 (typical: ~0.99 on
    /// DualSense) still register 100% in games. The in-between mapping is
    /// linear and continuous (no cliffs).
    #[serde(default = "dz_outer")]
    pub outer_dz: f32,
}

fn dz_enabled() -> bool {
    true
}
fn dz_zero() -> f32 {
    0.0
}
fn dz_outer() -> f32 {
    0.9
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            inner_dz: 0.0,
            outer_dz: 0.9,
        }
    }
}

/// `[audio]` table — BT audio. USB pads already expose native USB audio;
/// this only applies on Bluetooth.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// EXPERIMENTAL: physical speaker / 3.5mm jack output. The PipeWire sink
    /// itself always exists on BT (games route haptic audio through it);
    /// this only gates the physical transducer volume bytes. Haptic frames
    /// keep flowing either way.
    pub output: bool,
    /// Play a 440 Hz test tone continuously (verification aid, Phase 1).
    pub test_tone: bool,
    /// true → force internal speaker (0x13) even with a headset plugged;
    /// false/absent → auto-route by jack detect (jack 0x16 / speaker 0x13).
    pub speaker: Option<bool>,
    /// Headphone volume 0x00..0x7f (speaker uses a sensible fixed value).
    pub volume: Option<u8>,
    /// Opus CBR bitrate in bps. 160000 (200 B/10 ms frame) is the
    /// hardware-verified value; 96000 also encodes to a stable 120 B.
    pub bitrate: Option<i32>,
    /// BT audio report style: "0x35" (dsneo-style audio-only reports — the
    /// only mode PROVEN audible through the kernel hidraw path; kernel 0x31s
    /// are relayed directly) or "0x36" (vds-style combined state+haptics+
    /// audio reports — proven only over direct L2CAP). Default "0x35".
    pub report: Option<String>,
    /// pavucontrol label for the sink (default "Wireless Controller" — Sony's real
    /// UAC name on Windows/USB; RE Engine matches on it for haptics).
    pub node_description: Option<String>,
    /// Writer pacing interval in microseconds (default 10667 = 480/45000).
    /// The dsneo spec shows 200 ppm session-specific clock variance between
    /// pad units; a per-pad sweep (10667→10669→…) can flatten stutter.
    pub interval_us: Option<u64>,
    /// false → do not spawn the BT audio/haptics sink at all. Kernel 0x31s
    /// (rumble/LED) are then relayed directly by the proxy — the pre-audio
    /// code path. Diagnostic: isolates whether the sink's writer detour
    /// (queue + unified-seq restamp) causes rumble micro-stutter.
    pub sink: Option<bool>,
    /// Where the pad-stream's front (music) channels go: "pad" (default —
    /// Opus to the pad speaker), "forward" (play them on the normal system
    /// output instead; pad speaker silent, haptics unaffected — rerouting in
    /// pavucontrol kills rumble, this doesn't) or "mute" (drop them).
    /// Haptics (rear channels) always ride the pad link.
    pub speaker_output: Option<String>,
    /// PipeWire node.name to play on when speaker_output="forward"
    /// (absent → system default output).
    pub speaker_target: Option<String>,
    /// Gain on the pad's haptic channels (rear RL/RR pair) only, so game
    /// or system volume can drop without thinning haptics (game master
    /// 30% + haptic_gain 3.0 ≈ unchanged actuator swing). Music channels
    /// pass through untouched. Live-switchable: `mdrv-ds gain <0..8>`
    /// (volatile override + SIGHUP). Default 1.0.
    pub haptic_gain: Option<f32>,
}

/// `[notify]` table — event callback for chord fires. The command runs via
/// `sh -c` with the event as env vars:
///   MDRV_CHORD   button name (share, l3, touchpad, …)
///   MDRV_COMMAND the executed command (`shell:` payload, or `ps:toggle`)
///   MDRV_STATUS  success | failed
///   MDRV_CODE    exit code (-1 on spawn failure)
///   MDRV_OUTPUT  merged stdout+stderr, trimmed, capped at 500 bytes
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enabled: bool,
    /// "always" (default) | "on-failure"
    pub when: String,
    pub command: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        NotifyConfig {
            enabled: false,
            when: "always".into(),
            command: r#"notify-send -a mdrv-ds -u low "mdrv-ds" "$MDRV_CHORD: $MDRV_STATUS${MDRV_OUTPUT:+ — $MDRV_OUTPUT}""#.into(),
        }
    }
}

impl NotifyConfig {
    pub fn wants(&self, success: bool) -> bool {
        self.enabled
            && match self.when.as_str() {
                "on-failure" => !success,
                _ => true,
            }
    }
}

/// `[ps]` table — who gets to see the PS button.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PsConfig {
    /// "pass" (default) — relay PS presses to games/mdrv-gm as before.
    /// "swallow" — strip the PS bit from relayed reports; only mdrv-ds
    /// chords can see the PS button (allowlist of exactly mdrv-ds).
    pub mode: Option<String>,
    /// shell command fired on a solo PS tap (press+release without any
    /// other button). Optional; syntax same as [chords] ("shell:…").
    pub tap: Option<String>,
    /// shell command fired when PS is held solo for 3 s. Optional.
    pub hold: Option<String>,
}

impl PsConfig {
    pub fn swallow(&self) -> bool {
        self.mode.as_deref() == Some("swallow")
    }
    /// Normalized solo-tap command ('shell:' prefix stripped), if configured.
    pub fn tap_cmd(&self) -> Option<String> {
        ps_solo_cmd("tap", self.tap.as_deref())
    }
    /// Normalized solo-hold command ('shell:' prefix stripped), if configured.
    pub fn hold_cmd(&self) -> Option<String> {
        ps_solo_cmd("hold", self.hold.as_deref())
    }
}

fn ps_solo_cmd(which: &str, action: Option<&str>) -> Option<String> {
    let action = action?;
    let cmd = action.strip_prefix("shell:").unwrap_or(action);
    if cmd.trim().is_empty() {
        eprintln!("ps: {which}: empty command (skipped)");
        return None;
    }
    Some(cmd.to_string())
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("mdrv-ds").join("config.toml")
}

pub fn load() -> Config {
    let p = path();
    match std::fs::read_to_string(&p) {
        Ok(s) => match toml::from_str(&s) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("config parse error ({:?}): {e} — using previous/default", p);
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

/// A chord button: (byte offset from btn0, bit mask).
pub fn chord_button(name: &str) -> Option<(usize, u8)> {
    Some(match name.to_lowercase().as_str() {
        // byte 0: hat (low nibble) + face buttons
        "west" | "square" => (0, 0x10),
        "south" | "cross" => (0, 0x20),
        "east" | "circle" => (0, 0x40),
        "north" | "triangle" => (0, 0x80),
        // byte 1: shoulders / create+options / stick clicks
        "l1" => (1, 0x01),
        "r1" => (1, 0x02),
        "share" | "create" => (1, 0x10),
        "options" | "start" => (1, 0x20),
        "l3" => (1, 0x40),
        "r3" => (1, 0x80),
        // byte 2: PS, touchpad click
        "touchpad" => (2, 0x02),
        // analog triggers: no digital bit — the proxy treats these offsets as
        // "pressed when analog value crosses a threshold" (report bytes
        // stick0+4 / stick0+5) and zeroes the analog byte while swallowed
        "l2" => (TRIG_L2, 0),
        "r2" => (TRIG_R2, 0),
        // D-pad directions: hat-switch compass (low nibble of byte 0);
        // sentinels resolved by the chord engine, hat neutralized while
        // such a chord is active
        "up" | "dpad_up" => (HAT_UP, 0),
        "down" | "dpad_down" => (HAT_DOWN, 0),
        "left" | "dpad_left" => (HAT_LEFT, 0),
        "right" | "dpad_right" => (HAT_RIGHT, 0),
        _ => return None,
    })
}

/// Sentinel byte-offsets marking analog-trigger chords (L2/R2).
pub const TRIG_L2: usize = 0xF0;
pub const TRIG_R2: usize = 0xF1;
/// Sentinel byte-offsets marking d-pad hat chords.
pub const HAT_UP: usize = 0xF2;
pub const HAT_DOWN: usize = 0xF3;
pub const HAT_LEFT: usize = 0xF4;
pub const HAT_RIGHT: usize = 0xF5;

/// Parsed chord bindings: (name, byte offset, bit, shell command).
pub struct Chords {
    pub list: Vec<(String, usize, u8, String)>,
}

/// Sentinel command meaning "toggle [ps] mode" (not a shell command).
pub const PS_TOGGLE: &str = "\0ps:toggle";

pub fn parse_chords(cfg: &Config) -> Chords {
    let mut list = Vec::new();
    for (name, action) in &cfg.chords {
        let Some((byte, bit)) = chord_button(name) else {
            eprintln!("chords: unknown button name {name:?} (skipped)");
            continue;
        };
        let cmd = match action.strip_prefix("shell:") {
            Some(cmd) => {
                if cmd.trim().is_empty() {
                    continue;
                }
                cmd.to_string()
            }
            // internal action: toggles [ps] mode at runtime (useful e.g. on
            // PS+L3 to undo "swallow" and let PS reach games again)
            None if action == "ps:toggle" => PS_TOGGLE.to_string(),
            None => {
                eprintln!("chords: {name}: action must start with 'shell:' or be 'ps:toggle' (got {action:?}, skipped)");
                continue;
            }
        };
        list.push((name.clone(), byte, bit, cmd));
    }
    Chords { list }
}
