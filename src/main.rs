mod audio;
mod audiobridge;
mod config;
mod hid;
mod holder;
mod ipc;
mod l2cap;
mod mouse;
mod proxy;
mod sink;
mod uhid;
mod xinput;

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match cmd {
        "status" => hid::info(),
        "feature" => feature_cmd(&args[1..]),
        "mouse" => mouse::cmd(&args[1..]),
        "holder" => std::process::exit(holder::run(args.get(1).map(String::as_str))),
        "keymap" => keymap_cmd(&args[1..]),
        "speaker" => speaker_cmd(&args[1..]),
        "gain" => gain_cmd(&args[1..]),
        "xinput" => xinput_cmd(&args[1..]),
        "proxy" => {
            let mut opts = proxy::ProxyOpts {
                pad: None,
                gate_file: None,
                deadzone: false,
                hide: true,
                verbose: false,
            };
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--pad" => opts.pad = it.next().cloned(),
                    "--gate-file" => opts.gate_file = it.next().map(PathBuf::from),
                    "--deadzone" => opts.deadzone = true,
                    "--no-hide" => opts.hide = false,
                    "--verbose" | "-v" => opts.verbose = true,
                    other => {
                        eprintln!("unknown proxy flag: {other}");
                        std::process::exit(2);
                    }
                }
            }
            std::process::exit(proxy::run(opts));
        }
        "help" | "--help" | "-h" => usage(),
        other => {
            eprintln!("unknown command: {other}");
            usage();
            std::process::exit(2);
        }
    }
}

fn keymap_cmd(rest: &[String]) {
    match rest.first().map(String::as_str) {
        Some("test") => {
            let Some(name) = rest.get(1) else {
                eprintln!("usage: mdrv-ds keymap test <button-name>");
                std::process::exit(2);
            };
            proxy::fire_test(name);
        }
        Some("reload") => {
            if let Some(pid) = hup_proxy() {
                println!("SIGHUP → proxy (pid {pid}): reloading keymap");
            }
        }
        None | Some("list") | Some("status") => {
            let cfg = config::load();
            if cfg.chords.is_empty() {
                println!("no chords configured ({:?})", config::path());
            } else {
                println!(
                    "chords from {:?} (edit + `mdrv-ds keymap reload`):",
                    config::path()
                );
                for (name, action) in &cfg.chords {
                    println!("  PS + {name:<10} = {action}");
                }
            }
        }
        Some(other) => {
            eprintln!("unknown keymap arg: {other} (list|reload|test <name>)");
            std::process::exit(2);
        }
    }
}

/// SIGHUP the running proxy (pidfile in $XDG_RUNTIME_DIR). Exits 1 with a
/// message when no proxy is running. Returns the pid signalled.
fn hup_proxy() -> Option<i32> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let pidfile = base.join("mdrv-ds.pid");
    let pid: i32 = match std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse().ok())
    {
        Some(p) => p,
        None => {
            eprintln!("no running proxy (pidfile {:?} missing)", pidfile);
            std::process::exit(1);
        }
    };
    unsafe { libc::kill(pid, libc::SIGHUP) };
    Some(pid)
}

/// Runtime speaker_output override: writes a volatile marker file in
/// $XDG_RUNTIME_DIR and SIGHUPs the proxy, which recomputes the live mode
/// (`sink::current_mode`: override file first, config value as fallback).
/// `reset` clears the override back to the config value.
fn speaker_cmd(rest: &[String]) {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let file = base.join("mdrv-ds-speaker");
    match rest.first().map(String::as_str) {
        Some(mode @ ("pad" | "forward" | "mute")) => {
            std::fs::write(&file, mode).expect("write override file");
            if let Some(pid) = hup_proxy() {
                println!("speaker_output = {mode} (override) → proxy pid {pid}");
            }
        }
        Some("reset") => {
            let _ = std::fs::remove_file(&file);
            if let Some(pid) = hup_proxy() {
                println!("speaker_output override cleared → config value (proxy pid {pid})");
            }
        }
        None | Some("status") => {
            let override_ = std::fs::read_to_string(&file)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let cfg = config::load();
            let cfg_val = cfg.audio.speaker_output.as_deref().unwrap_or("pad");
            let pid = std::fs::read_to_string(base.join("mdrv-ds.pid"))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            match override_ {
                Some(o) => println!(
                    "speaker_output = {o} (override; config says {cfg_val}) proxy pid {:?}",
                    pid
                ),
                None => println!(
                    "speaker_output = {cfg_val} (config; no override) proxy pid {:?}",
                    pid
                ),
            }
        }
        Some(other) => {
            eprintln!("unknown speaker arg: {other} (pad|forward|mute|reset|status)");
            std::process::exit(2);
        }
    }
}

/// Runtime haptic-gain override (speaker pattern): writes a volatile
/// marker file in $XDG_RUNTIME_DIR and SIGHUPs the proxy, which
/// recomputes the live gain (`sink::current_haptic_gain`: override file
/// first, config value as fallback). `reset` clears the override.
fn gain_cmd(rest: &[String]) {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let file = base.join("mdrv-ds-gain");
    match rest.first().map(String::as_str) {
        Some("reset") => {
            let _ = std::fs::remove_file(&file);
            if let Some(pid) = hup_proxy() {
                println!("haptic_gain override cleared → config value (proxy pid {pid})");
            }
        }
        None | Some("status") => {
            let override_ = std::fs::read_to_string(&file)
                .ok()
                .and_then(|s| s.trim().parse::<f32>().ok());
            let cfg = config::load();
            let cfg_val = cfg.audio.haptic_gain.unwrap_or(1.0);
            let pid = std::fs::read_to_string(base.join("mdrv-ds.pid"))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            match override_ {
                Some(o) => println!(
                    "haptic_gain = {o:.2} (override; config says {cfg_val}) proxy pid {:?}",
                    pid
                ),
                None => println!(
                    "haptic_gain = {:.2} (config; no override) proxy pid {:?}",
                    cfg_val, pid
                ),
            }
        }
        Some(v) => match v.parse::<f32>() {
            Ok(g) if g.is_finite() && (0.0..=8.0).contains(&g) => {
                std::fs::write(&file, format!("{g}")).expect("write override file");
                if let Some(pid) = hup_proxy() {
                    println!("haptic_gain = {g} (override) → proxy pid {pid}");
                }
            }
            _ => {
                eprintln!("invalid gain: {v} (want 0..=8)");
                std::process::exit(2);
            }
        },
    }
}

/// Runtime XInput-emulation override (speaker pattern): writes a volatile
/// marker file in $XDG_RUNTIME_DIR and SIGHUPs the proxy, which raises or
/// tears down the second virtual pad live (see xinput::effective). `reset`
/// clears the override back to the config value.
fn xinput_cmd(rest: &[String]) {
    let file = xinput::override_path();
    match rest.first().map(String::as_str) {
        Some(on @ ("on" | "off")) => {
            std::fs::write(&file, on).expect("write override file");
            if let Some(pid) = hup_proxy() {
                println!("xinput = {on} (override) → proxy pid {pid}");
            }
        }
        Some("reset") => {
            let _ = std::fs::remove_file(&file);
            if let Some(pid) = hup_proxy() {
                println!("xinput override cleared → config value (proxy pid {pid})");
            }
        }
        None | Some("status") => {
            let override_ = std::fs::read_to_string(&file)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let cfg = config::load();
            let cfg_val = cfg.xinput();
            let pid = std::fs::read_to_string(
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join("mdrv-ds.pid"),
            )
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok());
            match override_ {
                Some(o) => println!(
                    "xinput = {o} (override; config says {cfg_val}) proxy pid {:?}",
                    pid
                ),
                None => println!(
                    "xinput = {cfg_val} (config; no override) proxy pid {:?}",
                    pid
                ),
            }
        }
        Some(other) => {
            eprintln!("unknown xinput arg: {other} (on|off|reset|status)");
            std::process::exit(2);
        }
    }
}
fn usage() {
    println!(
        "mdrv-ds — DualSense hidraw proxy (transport-matching, verbatim relay)\n\
         \n\
         USAGE:\n\
         \x20 mdrv-ds status                     list DualSense pads + live feature reports\n\
         \x20 mdrv-ds feature get <rid> [len]    read a feature report from the real pad\n\
         \x20 mdrv-ds feature set <rid> <hex>    write a feature report to the real pad\n\
         \x20 mdrv-ds proxy [flags]              run the UHID proxy (pad owned by holder)\n\
         \x20 mdrv-ds holder                     virtual-pad holder daemon (systemd unit)\n\
         \x20 mdrv-ds mouse [on|off|toggle|status] touchpad-as-mouse (standalone)\n\
         \x20 mdrv-ds keymap [list|reload|test <name>]   PS-chords: show / reload / fire one\n\
         \x20 mdrv-ds speaker [pad|forward|mute|reset]  live speaker_output switch (status default)\n\
         \x20 mdrv-ds gain [0..=8|reset]            live haptic-channel gain (status default)\n\
         \x20 mdrv-ds xinput [on|off|reset]      live XInput pad emulation switch (status default)\n\
         \n\
         PROXY FLAGS:\n\
         \x20 --pad <path>       explicit hidraw (default: first DualSense, USB preferred)\n\
         \x20 --gate-file <path> neutralize gameplay input while file is non-empty/not 'none'\n\
         \x20 --deadzone         apply 5% radial stick deadzone to relayed input\n\
         \x20 --no-hide          don't bind-mount /dev/null over the real hidraw\n\
         \x20 -v, --verbose      dump reports to stderr"
    );
}

fn feature_cmd(rest: &[String]) {
    let usage_err = || {
        eprintln!("usage: mdrv-ds feature get <rid> [len] | mdrv-ds feature set <rid> <hexdata>");
        std::process::exit(2);
    };
    let op = rest.first().map(String::as_str).unwrap_or("");
    let rid = rest
        .get(1)
        .and_then(|s| u8::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    let Some(rid) = rid else { usage_err() };
    let (file, _) = match hid::open_pad(None) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("open pad: {e}");
            std::process::exit(1);
        }
    };
    match op {
        "get" => {
            let len: usize = rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(65);
            let mut buf = vec![0u8; len.clamp(2, 4096)];
            match hid::get_feature(&file, rid, &mut buf) {
                Ok(n) => println!("{} ({}B)", hid::hex(&buf[..n]), n),
                Err(e) => {
                    eprintln!("get 0x{rid:02x}: {e}");
                    std::process::exit(1);
                }
            }
        }
        "set" => {
            let Some(hexdata) = rest.get(2) else {
                usage_err()
            };
            let data: Option<Vec<u8>> = (0..hexdata.len() / 2)
                .map(|i| u8::from_str_radix(&hexdata[i * 2..i * 2 + 2], 16).ok())
                .collect();
            let Some(mut data) = data else {
                eprintln!("bad hex");
                std::process::exit(2);
            };
            data.insert(0, rid);
            match hid::set_feature(&file, &mut data) {
                Ok(n) => println!("set ok ({n}B)"),
                Err(e) => {
                    eprintln!("set 0x{rid:02x}: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => usage_err(),
    }
}
