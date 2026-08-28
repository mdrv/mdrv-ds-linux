mod audiobridge;
mod audio;
mod config;
mod hid;
mod holder;
mod ipc;
mod l2cap;
mod mouse;
mod proxy;
mod sink;
mod uhid;

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match cmd {
        "status" => hid::info(),
        "feature" => feature_cmd(&args[1..]),
        "mouse" => mouse::cmd(&args[1..]),
        "holder" => std::process::exit(holder::run()),
        "keymap" => keymap_cmd(&args[1..]),
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
            println!("SIGHUP → proxy (pid {pid}): reloading keymap");
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
