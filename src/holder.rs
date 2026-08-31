//! Long-lived virtual-pad holder daemon.
//!
//! Owns the /dev/uhid fd and therefore the virtual DualSense. The proxy
//! connects over a unix socket (see `ipc.rs`) and drives it; when the proxy
//! exits or restarts (binary deploys, crashes, `systemctl restart`), THIS
//! process keeps the virtual pad alive — games and mdrv-gm hold its device
//! nodes and never re-enumerate, so they never notice. The pad is destroyed
//! only by a shape change (USB ↔ BT descriptor swap), this daemon's own
//! shutdown, or a crash.
//!
//! On proxy disconnect the holder relays one stored NEUTRAL input report so
//! the kernel's input state can't freeze at last-pressed values (the
//! "launcher drives right forever" bug).

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ipc;
use crate::uhid::{self, UhidEvent};

static EXIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    if sig == libc::SIGHUP {
        return; // proxy reloads on SIGHUP; the holder has nothing to reload
    }
    EXIT.store(true, Ordering::Relaxed);
}

struct Holder {
    uhid: Option<File>,
    /// create payload of the current pad — equality means "reuse, don't touch"
    key: Option<Vec<u8>>,
    /// neutral report relayed on proxy disconnect (None = cleared)
    neutral: Option<Vec<u8>>,
    started: bool,
}

impl Holder {
    fn new() -> Self {
        Holder {
            uhid: None,
            key: None,
            neutral: None,
            started: false,
        }
    }

    /// Handle one frame from the proxy. Returns Err(()) if the connection
    /// is broken and should be dropped.
    fn frame(&mut self, tag: u8, data: &[u8], client: &mut UnixStream) -> Result<(), ()> {
        match tag {
            ipc::TAG_CREATE => {
                if self.key.as_deref() == Some(data) {
                    eprintln!("holder: virtual pad reused (same shape)");
                    let _ = ipc::send(client, ipc::TAG_CREATE_ACK, &[1]);
                    return Ok(());
                }
                if self.uhid.is_some() {
                    eprintln!("holder: pad shape changed — destroying virtual pad");
                    let _ = uhid::destroy(self.uhid.as_mut().unwrap());
                    self.started = false;
                }
                let Some(msg) = ipc::decode_create(data) else {
                    let _ = ipc::send(client, ipc::TAG_CREATE_ACK, &[2, b'?']);
                    return Ok(());
                };
                let mut f = match OpenOptions::new().read(true).write(true).open("/dev/uhid") {
                    Ok(f) => f,
                    Err(e) => {
                        let m = format!("open /dev/uhid: {e}");
                        eprintln!("holder: {m}");
                        let mut p = vec![2u8];
                        p.extend_from_slice(m.as_bytes());
                        let _ = ipc::send(client, ipc::TAG_CREATE_ACK, &p);
                        return Ok(());
                    }
                };
                let cp = uhid::CreateParams {
                    name: &msg.name,
                    uniq: &msg.uniq,
                    rdesc: &msg.rdesc,
                    bus: msg.bus,
                    vendor: msg.vendor,
                    product: msg.product,
                    version: msg.version,
                };
                if let Err(e) = uhid::create2(&mut f, &cp) {
                    let m = format!("uhid create2: {e}");
                    eprintln!("holder: {m}");
                    let mut p = vec![2u8];
                    p.extend_from_slice(m.as_bytes());
                    let _ = ipc::send(client, ipc::TAG_CREATE_ACK, &p);
                    return Ok(());
                }
                eprintln!(
                    "holder: virtual DualSense created (bus {}, uniq {}, rdesc {}B)",
                    msg.bus,
                    msg.uniq,
                    msg.rdesc.len()
                );
                self.uhid = Some(f);
                self.key = Some(data.to_vec());
                self.started = false;
                let _ = ipc::send(client, ipc::TAG_CREATE_ACK, &[0]);
            }
            ipc::TAG_DESTROY => {
                // Explicit teardown (XInput live-disable). Resets the create
                // key so a later CREATE rebuilds from scratch.
                if self.uhid.is_some() {
                    eprintln!("holder: pad destroyed on request");
                    let _ = uhid::destroy(self.uhid.as_mut().unwrap());
                }
                self.uhid = None;
                self.key = None;
                self.neutral = None;
                self.started = false;
            }
            ipc::TAG_INPUT => {
                if self.started {
                    if let Some(u) = self.uhid.as_mut() {
                        let _ = uhid::input2(u, data);
                    }
                }
            }
            ipc::TAG_NEUTRAL => {
                self.neutral = if data.is_empty() {
                    None
                } else {
                    Some(data.to_vec())
                };
            }
            ipc::TAG_GET_REPLY => {
                if let Some((id, err, rdata)) = ipc::dec_get_reply(data) {
                    eprintln!("holder: get_reply id={id} err={err} len={}", rdata.len());
                    if let Some(u) = self.uhid.as_mut() {
                        let _ = uhid::reply_get_report(u, id, err, &rdata);
                    }
                }
            }
            ipc::TAG_SET_ACK => {
                if let Some((id, err)) = ipc::dec_ack(data) {
                    if let Some(u) = self.uhid.as_mut() {
                        let _ = uhid::reply_set_report(u, id, err);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// One readable /dev/uhid event: log it, forward to the proxy, or answer
    /// directly when no proxy is connected.
    fn uhid_event(&mut self, client: &mut Option<UnixStream>) {
        let Some(u) = self.uhid.as_mut() else { return };
        let mut buf = [0u8; uhid::EV_SIZE];
        if u.read_exact(&mut buf).is_err() {
            return;
        }
        match uhid::parse_event(&buf) {
            UhidEvent::Start => {
                self.started = true;
                eprintln!("uhid: START");
            }
            UhidEvent::Stop => {
                // The kernel destroyed the device (probe failure or real
                // teardown). Forget the create key so the next CREATE
                // rebuilds instead of answering "reused" into a corpse.
                self.started = false;
                self.key = None;
                eprintln!("uhid: STOP (key reset — next CREATE rebuilds)");
            }
            UhidEvent::Output(data) => {
                if let Some(c) = client.as_mut() {
                    let _ = ipc::send(c, ipc::TAG_OUTPUT, data);
                }
            }
            UhidEvent::GetReport(id, rnum) => match client.as_mut() {
                Some(c) => {
                    let _ = ipc::send(c, ipc::TAG_GET_REQ, &ipc::enc_req(id, rnum));
                }
                None => {
                    // no proxy → no real pad behind us; fail the request
                    let _ = uhid::reply_get_report(u, id, libc::EINVAL as u16, &[]);
                }
            },
            UhidEvent::SetReport(id, rnum, data) => match client.as_mut() {
                Some(c) => {
                    let _ = ipc::send(c, ipc::TAG_SET_REQ, &ipc::enc_set_req(id, rnum, data));
                }
                None => {
                    let _ = uhid::reply_set_report(u, id, libc::EIO as u16);
                }
            },
            UhidEvent::Other(_) => {}
        }
    }

    /// Proxy went away: keep the pad, park the kernel state at neutral.
    fn client_gone(&mut self) {
        eprintln!("holder: proxy disconnected — virtual pad kept alive");
        if self.started {
            if let Some(n) = self.neutral.as_ref() {
                if let Some(u) = self.uhid.as_mut() {
                    let _ = uhid::input2(u, n);
                }
            }
        }
    }
}

/// `instance` selects which pad this daemon holds: None/"" = the default
/// DualSense holder (socket mdrv-ds.sock, log holder.log); "xi" = the
/// second, XInput-emulation holder (socket mdrv-ds-xi.sock, log
/// holder-xi.log). Behavior of the default instance is unchanged.
pub fn run(instance: Option<&str>) -> i32 {
    let instance = instance.unwrap_or("");
    let log = if instance.is_empty() {
        "holder.log"
    } else {
        "holder-xi.log"
    };
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
        // ignored
    }
    crate::proxy::tee_stderr_to_file(log);
    if ipc::connect_for(instance).is_ok() {
        eprintln!("holder: already running — nothing to do");
        return 0;
    }
    let path = ipc::sock_path_for(instance);
    let _ = fs::remove_file(&path); // stale socket from a crash
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("holder: bind {}: {e}", path.display());
            return 1;
        }
    };
    eprintln!("holder: listening on {}", path.display());

    let mut h = Holder::new();
    let mut client: Option<UnixStream> = None;

    loop {
        if EXIT.load(Ordering::Relaxed) {
            if let Some(u) = h.uhid.as_mut() {
                let _ = uhid::destroy(u);
            }
            let _ = fs::remove_file(&path);
            eprintln!("holder: exit (virtual pad destroyed)");
            return 0;
        }
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: client.as_ref().map(|c| c.as_raw_fd()).unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: h.uhid.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 3, 250) };
        if ret < 0 {
            continue;
        }

        if fds[0].revents & libc::POLLIN != 0 {
            if let Ok((s, _)) = listener.accept() {
                if client.is_some() {
                    let mut s = s;
                    let _ = ipc::send(
                        &mut s,
                        ipc::TAG_NOTICE,
                        b"another proxy is already connected",
                    );
                } else {
                    eprintln!("holder: proxy connected");
                    client = Some(s);
                }
            }
        }

        if client.is_some() && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
        {
            let mut c = client.take().unwrap();
            match ipc::recv(&mut c) {
                Ok((tag, data)) => {
                    if h.frame(tag, &data, &mut c).is_ok() {
                        client = Some(c);
                    } else {
                        h.client_gone();
                    }
                }
                Err(_) => h.client_gone(),
            }
        }

        if h.uhid.is_some() && fds[2].revents & libc::POLLIN != 0 {
            h.uhid_event(&mut client);
        }
    }
}
