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
    #[serde(default)]
    pub chords: HashMap<String, String>,
    #[serde(default)]
    pub ps: PsConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
    #[serde(default)]
    pub audio: AudioConfig,
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
    /// pavucontrol label for the sink (default "DualSense Wireless Controller (BT)"; games match on the DualSense substring for haptic-audio routing).
    pub node_description: Option<String>,
    /// Writer pacing interval in microseconds (default 10667 = 480/45000).
    /// The dsneo spec shows 200 ppm session-specific clock variance between
    /// pad units; a per-pad sweep (10667→10669→…) can flatten stutter.
    pub interval_us: Option<u64>,
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
}

impl PsConfig {
    pub fn swallow(&self) -> bool {
        self.mode.as_deref() == Some("swallow")
    }
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
        // D-pad deliberately unsupported: mdrv-gm's overlay navigation uses it
        _ => return None,
    })
}

/// Sentinel byte-offsets marking analog-trigger chords (L2/R2).
pub const TRIG_L2: usize = 0xF0;
pub const TRIG_R2: usize = 0xF1;

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
            if matches!(
                name.to_lowercase().as_str(),
                "up" | "down"
                    | "left"
                    | "right"
                    | "dpad"
                    | "dpad_up"
                    | "dpad_down"
                    | "dpad_left"
                    | "dpad_right"
            ) {
                eprintln!("chords: D-pad ({name}) reserved for overlay navigation (skipped)");
            } else {
                eprintln!("chords: unknown button name {name:?} (skipped)");
            }
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
