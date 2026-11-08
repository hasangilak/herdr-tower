//! The phone API: the tower over the local network, for the Android app.
//! The contract, the pairing flow and the security model are in
//! `docs/mobile-api.md`.
//!
//! Off unless `[lan] enabled = true` (`tower phone enable`). HTTPS only, on
//! this Mac's private LAN address; every call but pairing needs a paired
//! device's token.

pub mod audio;
pub mod auth;
pub mod http;
pub mod hub;
pub mod tls;

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::mic::{RATE, Source, Utterance};
use super::state::{Shared, now_ms};
use super::tts::Mouth;
use auth::{Devices, PairError, Pairing};
use http::{Request, Response};
use hub::Hub;
use tls::Identity;

/// The API version in every path (`/v1/...`).
pub const API: u32 = 1;
const MAX_CONNECTIONS: usize = 32;
/// Failed logins (bad token or code) from one address per minute before it's refused for the minute.
const MAX_FAILURES: u32 = 20;

/// What the API needs from the daemon.
#[derive(Clone)]
pub struct Ctx {
    pub shared: Shared,
    pub mouth: Mouth,
    pub hub: Arc<Hub>,
    pub ack: Option<Duration>,
    pub stt: Sender<Utterance>,
}

struct Running {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    fingerprint: String,
}

#[derive(Clone)]
pub struct Lan {
    ctx: Ctx,
    running: Arc<Mutex<Option<Running>>>,
    pairing: Arc<Mutex<Pairing>>,
    failures: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
    connections: Arc<AtomicUsize>,
    /// `devices.json` and the certificate's folder (elsewhere in tests).
    devices_path: std::path::PathBuf,
    identity_dir: std::path::PathBuf,
}

/// This Mac's address on the network its default route uses (no packet is sent).
pub fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").or_else(|_| s.connect("8.8.8.8:53")).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// A private, link-local or carrier-grade NAT (Tailscale) address.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private() || v.is_link_local() || v.is_loopback() || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00 || (v.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// The address to listen on, as `[lan]` asks.
pub fn choose_ip(cfg: &crate::config::Lan) -> Result<IpAddr> {
    let ip = if cfg.bind.is_empty() || cfg.bind == "auto" {
        lan_ip().context("this Mac has no network address (is Wi-Fi on?)")?
    } else {
        cfg.bind.parse().with_context(|| format!("[lan] bind = {:?} isn't an IP address", cfg.bind))?
    };
    if ip.is_unspecified() && !cfg.allow_public {
        bail!("[lan] bind = {ip} would listen on every network; give this Mac's LAN address, or set allow_public");
    }
    if !ip.is_unspecified() && !is_private(ip) && !cfg.allow_public {
        bail!("{ip} isn't a private address: the phone API listens only on the local network (see allow_public)");
    }
    Ok(ip)
}

impl Lan {
    pub fn new(ctx: Ctx) -> Lan {
        Self::with_paths(ctx, Devices::path(), Identity::dir())
    }

    pub fn with_paths(ctx: Ctx, devices_path: std::path::PathBuf, identity_dir: std::path::PathBuf) -> Lan {
        Lan {
            devices_path,
            identity_dir,
            ctx,
            running: Arc::new(Mutex::new(None)),
            pairing: Arc::new(Mutex::new(Pairing::default())),
            failures: Arc::new(Mutex::new(HashMap::new())),
            connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn start(&self, cfg: &crate::config::Lan) -> Result<SocketAddr> {
        if let Some(r) = self.running.lock().unwrap().as_ref() {
            return Ok(r.addr);
        }
        let identity = Identity::load_or_create(&self.identity_dir)?;
        let tls = identity.server_config()?;
        let ip = choose_ip(cfg)?;
        let listener = TcpListener::bind((ip, cfg.port)).with_context(|| format!("listening on {ip}:{}", cfg.port))?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        *self.running.lock().unwrap() = Some(Running { addr, stop: stop.clone(), fingerprint: identity.fingerprint() });
        self.ctx.shared.update(|s| s.lan = format!("https://{addr}"));
        let me = self.clone();
        thread::Builder::new().name("lan".into()).spawn(move || {
            for conn in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                if me.connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    continue;
                }
                let (me, tls) = (me.clone(), tls.clone());
                me.connections.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    me.connection(conn, tls);
                    me.connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
        })?;
        eprintln!("[phone] API on https://{addr} (certificate {})", tls::grouped(&identity.fingerprint()[..16]));
        Ok(addr)
    }

    pub fn stop(&self) {
        if let Some(r) = self.running.lock().unwrap().take() {
            r.stop.store(true, Ordering::SeqCst);
            // Wake the accept loop so it sees the flag.
            let _ = TcpStream::connect_timeout(&r.addr, Duration::from_millis(300));
            self.pairing.lock().unwrap().close();
            self.ctx.shared.update(|s| {
                s.lan.clear();
                s.phone = None;
            });
            eprintln!("[phone] API off");
        }
    }

    /// Where it listens and who is connected.
    pub fn info(&self) -> Value {
        let r = self.running.lock().unwrap();
        let (streams, voice) = self.ctx.hub.counts();
        let devices = Devices::load_from(&self.devices_path);
        json!({
            "on": r.is_some(),
            "url": r.as_ref().map(|r| format!("https://{}", r.addr)),
            "fingerprint": r.as_ref().map(|r| r.fingerprint.clone()),
            "devices": devices.devices.iter().map(|d| json!({ "id": d.id, "name": d.name, "created_ms": d.created_ms, "last_seen_ms": d.last_seen_ms })).collect::<Vec<_>>(),
            "streams": streams,
            "voice_streams": voice,
            "pairing_expires_in_s": self.pairing.lock().unwrap().expires_in().map(|d| d.as_secs()),
        })
    }

    /// Opens a pairing code: what the Mac shows (and the QR carries).
    pub fn pair(&self) -> Result<Value> {
        let r = self.running.lock().unwrap();
        let Some(r) = r.as_ref() else { bail!("the phone API is off: tower phone enable") };
        let code = self.pairing.lock().unwrap().open();
        let host = r.addr.ip().to_string();
        let port = r.addr.port();
        let link = format!("herdertower://pair?h={host}&p={port}&c={code}&fp={}", r.fingerprint);
        eprintln!("[phone] pairing open for {} s", auth::CODE_TTL.as_secs());
        Ok(json!({
            "code": code,
            "host": host,
            "port": port,
            "fingerprint": r.fingerprint,
            "link": link,
            "expires_in_s": auth::CODE_TTL.as_secs(),
        }))
    }

    /// A device was revoked (`tower phone revoke`): its open streams end.
    pub fn revoked(&self, device: &str) {
        self.ctx.hub.drop_device(device);
    }

    fn connection(&self, tcp: TcpStream, tls: Arc<rustls::ServerConfig>) {
        let peer = tcp.peer_addr().map(|a| a.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(20)));
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(10)));
        let _ = tcp.set_nodelay(true);
        let Ok(conn) = rustls::ServerConnection::new(tls) else { return };
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        let req = match http::read_request(&mut stream) {
            Ok(r) => r,
            Err(e) => {
                // A TLS failure (a phone that doesn't trust us yet, a scanner) has nothing to answer.
                if stream.conn.is_handshaking() {
                    return;
                }
                let _ = http::write_response(&mut stream, &Response::error(400, "bad_request", &format!("{e:#}")));
                return;
            }
        };
        if self.refused(peer) {
            let _ = http::write_response(&mut stream, &Response::error(429, "too_many_failures", "too many failed logins; wait a minute"));
            return;
        }
        let resp = self.route(&req, peer, &mut stream);
        if let Some(resp) = resp {
            let _ = http::write_response(&mut stream, &resp);
        }
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }

    fn refused(&self, ip: IpAddr) -> bool {
        let f = self.failures.lock().unwrap();
        f.get(&ip).is_some_and(|(n, since)| *n >= MAX_FAILURES && since.elapsed() < Duration::from_secs(60))
    }

    fn failed(&self, ip: IpAddr) {
        let mut f = self.failures.lock().unwrap();
        let e = f.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() > Duration::from_secs(60) {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }

    /// The answer, or None when the handler wrote its own (the event stream).
    fn route(&self, req: &Request, peer: IpAddr, stream: &mut impl Write) -> Option<Response> {
        let seg = req.segments();
        let seg: Vec<&str> = seg.iter().map(String::as_str).collect();
        if seg.first() != Some(&"v1") {
            return Some(Response::error(404, "not_found", "the API is under /v1"));
        }
        let m = req.method.as_str();
        if (m, &seg[1..]) == ("POST", &["pair"][..]) {
            return Some(self.pair_device(req, peer));
        }
        // Everything else: a paired device only.
        let devices = Devices::load_from(&self.devices_path);
        let device = match auth::authorize(&devices, req.header("authorization")) {
            Ok(d) => d,
            Err(e) => {
                self.failed(peer);
                thread::sleep(Duration::from_millis(300));
                return Some(Response::error(401, "unauthorized", &e.to_string()));
            }
        };
        self.seen(&device);
        let q = |k: &str| req.query.get(k).map(String::as_str);
        let limit = |default: usize| q("limit").and_then(|l| l.parse().ok()).unwrap_or(default).min(1000);
        let resp = match (m, &seg[1..]) {
            ("GET", ["hello"]) => Response::json(
                200,
                &json!({
                    "api": API,
                    "tower": env!("CARGO_PKG_VERSION"),
                    "device": { "id": device.id, "name": device.name },
                    "voice": self.ctx.shared.get().voice,
                    "phone_is_voice": self.ctx.hub.voice(),
                }),
            ),
            ("GET", ["state"]) => Response::json(200, &self.ctx.shared.json()),
            ("GET", ["agents"]) => Response::json(200, &crate::agents_value(q("all") == Some("1"))),
            ("GET", ["spaces"]) => Response::json(200, &crate::spaces::spaces_value()),
            ("GET", ["accounts"]) => match crate::accounts::list() {
                Ok(l) => Response::json(200, &json!({ "default": l.first().map(|a| &a.name), "accounts": l })),
                Err(e) => Response::error(503, "accounts_unavailable", &format!("{e:#}")),
            },
            ("GET", ["jobs"]) => {
                let want = q("status").unwrap_or("all");
                let session = q("session");
                let rows: Vec<Value> = crate::history::jobs(&crate::history::load())
                    .into_iter()
                    .filter(|j| match want {
                        "open" => j.open(),
                        "finished" => !j.open(),
                        _ => true,
                    })
                    .filter(|j| session.is_none_or(|s| j.session == s))
                    .take(limit(50))
                    .map(|j| j.row())
                    .collect();
                Response::json(200, &json!({ "jobs": rows }))
            }
            ("GET", ["history"]) => {
                let (session, kind) = (q("session"), q("kind"));
                let before = q("before_ms").and_then(|b| b.parse::<u64>().ok()).unwrap_or(u64::MAX);
                let mut list: Vec<_> = crate::history::load()
                    .into_iter()
                    .rev()
                    .filter(|e| e.at < before)
                    .filter(|e| session.is_none_or(|s| e.session.as_deref() == Some(s)))
                    .filter(|e| kind.is_none_or(|k| k.split(',').any(|k| k == e.kind)))
                    .take(limit(100))
                    .collect();
                list.reverse();
                Response::json(200, &json!({ "entries": list }))
            }
            ("GET", ["sessions", address, "screen"]) => {
                if !valid_address(address) {
                    return Some(Response::error(400, "bad_address", "an address looks like t3 or vps/t3"));
                }
                let lines = q("lines").and_then(|l| l.parse::<u32>().ok()).unwrap_or(60).min(400).to_string();
                match crate::hc::run(&["read", address, "--lines", &lines]) {
                    Ok(o) if o.code == 0 => Response::json(200, &json!({ "address": address, "screen": o.stdout })),
                    Ok(o) => Response::error(404, "no_session", o.stderr.trim()),
                    Err(e) => Response::error(503, "hc", &format!("{e:#}")),
                }
            }
            ("GET", ["audio", id]) => match self.ctx.hub.clip(id) {
                Some(wav) => Response::bytes("audio/wav", wav),
                None => Response::error(404, "no_clip", "that clip is gone (they are kept for 15 minutes)"),
            },
            ("GET", ["events"]) => {
                self.events(&device, q("voice") == Some("1"), stream);
                return None;
            }
            ("POST", ["ask"]) => {
                let body = req.json().unwrap_or_default();
                let text = body["text"].as_str().unwrap_or("").trim().to_string();
                if text.is_empty() {
                    return Some(Response::error(400, "empty", "send {\"text\": \"...\"}"));
                }
                self.ask(&text)
            }
            ("POST", ["agents", address, "answer"]) => {
                if !valid_address(address) {
                    return Some(Response::error(400, "bad_address", "an address looks like t3 or vps/t3"));
                }
                let body = req.json().unwrap_or_default();
                let verb = match body["decision"].as_str() {
                    Some("approve") => "Approve",
                    Some("refuse") => "Refuse",
                    _ => return Some(Response::error(400, "bad_decision", "decision is approve or refuse")),
                };
                let note = body["note"].as_str().map(str::trim).filter(|n| !n.is_empty()).map(|n| format!(" {n}")).unwrap_or_default();
                self.ask(&format!("{verb} what {address} is asking for.{note}"))
            }
            ("POST", ["focus"]) => {
                let body = req.json().unwrap_or_default();
                match crate::spaces::set_focus(body["space"].as_str()) {
                    Ok(f) => Response::json(200, &json!({ "focus": f })),
                    Err(e) => Response::error(404, "no_space", &format!("{e:#}")),
                }
            }
            ("POST", ["hush"]) => {
                self.ctx.mouth.hush();
                Response::json(200, &json!({ "ok": true }))
            }
            ("POST", ["mode"]) => {
                let body = req.json().unwrap_or_default();
                match body["mode"].as_str() {
                    Some(mode @ ("ptt" | "handsfree" | "off")) => {
                        match crate::protocol::send(&crate::protocol::Request::Mode { mode: mode.into() }) {
                            Ok(_) => Response::json(200, &json!({ "mode": mode })),
                            Err(e) => Response::error(503, "daemon", &format!("{e:#}")),
                        }
                    }
                    _ => Response::error(400, "bad_mode", "mode is ptt, handsfree or off"),
                }
            }
            ("POST", ["voice"]) => self.voice(req),
            ("DELETE", ["device"]) => {
                let mut d = Devices::load_from(&self.devices_path);
                d.revoke(&device.id);
                if let Err(e) = d.save_to(&self.devices_path) {
                    return Some(Response::error(503, "save", &format!("{e:#}")));
                }
                self.ctx.hub.drop_device(&device.id);
                Response::json(200, &json!({ "revoked": device.id }))
            }
            _ => Response::error(404, "not_found", &format!("no {m} {}", req.path)),
        };
        Some(resp)
    }

    fn pair_device(&self, req: &Request, peer: IpAddr) -> Response {
        let body = req.json().unwrap_or_default();
        let code = body["code"].as_str().unwrap_or("");
        let taken = self.pairing.lock().unwrap().take(code);
        match taken {
            Ok(()) => {
                let mut d = Devices::load_from(&self.devices_path);
                let (device, token) = d.add(body["name"].as_str().unwrap_or("phone"), now_ms());
                if let Err(e) = d.save_to(&self.devices_path) {
                    return Response::error(503, "save", &format!("{e:#}"));
                }
                eprintln!("[phone] paired {} ({}) from {peer}", device.name, device.id);
                let fp = self.running.lock().unwrap().as_ref().map(|r| r.fingerprint.clone());
                Response::json(
                    200,
                    &json!({ "token": token, "device": { "id": device.id, "name": device.name }, "fingerprint": fp, "api": API }),
                )
            }
            Err(PairError::NoCode) => {
                self.failed(peer);
                Response::error(403, "pairing_closed", "no pairing code is open: run `tower phone pair` on the Mac")
            }
            Err(PairError::Wrong { left }) => {
                self.failed(peer);
                thread::sleep(Duration::from_secs(1));
                Response::json(
                    403,
                    &json!({ "error": { "code": "wrong_code", "message": "that isn't the code on the Mac", "tries_left": left } }),
                )
            }
        }
    }

    /// Remembers when a device was last heard from (written at most once a minute).
    fn seen(&self, device: &auth::Device) {
        let now = now_ms();
        if now.saturating_sub(device.last_seen_ms) < 60_000 {
            return;
        }
        let mut d = Devices::load_from(&self.devices_path);
        if let Some(x) = d.devices.iter_mut().find(|x| x.id == device.id) {
            x.last_seen_ms = now;
            let _ = d.save_to(&self.devices_path);
        }
    }

    fn ask(&self, text: &str) -> Response {
        crate::daemon::timing::start("asked");
        match super::stt::deliver(&self.ctx.shared, &self.ctx.mouth, text, self.ctx.ack) {
            Ok(()) => Response::json(202, &json!({ "sent": text })),
            Err(e) => Response::error(503, "brain", &crate::brain::spoken_error(&format!("{e:#}"))),
        }
    }

    /// A recording from the phone: transcribed by the tower's Whisper, then
    /// handed to the brain like a press-to-talk take.
    fn voice(&self, req: &Request) -> Response {
        let (samples, rate) = match audio::wav_decode(&req.body) {
            Ok(x) => x,
            Err(e) => return Response::error(415, "bad_audio", &format!("{e:#}: send audio/wav, 16-bit PCM, mono, 16 kHz preferred")),
        };
        let secs = samples.len() as f32 / rate as f32;
        if !(0.2..=120.0).contains(&secs) {
            return Response::error(400, "bad_length", "a recording is 0.2 to 120 seconds");
        }
        let samples = if rate == RATE { samples } else { super::mic::resample(&samples, rate, RATE) };
        let (tx, rx) = std::sync::mpsc::channel();
        if self.ctx.stt.send(Utterance { samples, source: Source::Phone(tx) }).is_err() {
            return Response::error(503, "stt", "the transcriber isn't running");
        }
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(heard) => Response::json(200, &json!({ "heard": heard, "sent": true })),
            // Nothing worth sending (silence, noise, a stop word).
            Err(_) => Response::json(200, &json!({ "heard": "", "sent": false })),
        }
    }

    /// `GET /v1/events`: the panel's live feed, and with `voice=1` the tower's voice.
    fn events(&self, device: &auth::Device, voice: bool, w: &mut impl Write) {
        if http::write_sse_head(w).is_err() {
            return;
        }
        let (sid, rx) = self.ctx.hub.subscribe(&device.id, voice);
        if voice {
            self.ctx.shared.update(|s| s.phone = Some(device.name.clone()));
        }
        let mut n = 0u64;
        let mut send = |w: &mut dyn Write, event: &str, data: &Value| -> bool {
            n += 1;
            w.write_all(http::sse_event(n, event, data).as_bytes()).and_then(|_| w.flush()).is_ok()
        };
        let state = self.ctx.shared.json();
        let mut version = state["version"].as_u64().unwrap_or(0);
        let mut ok = send(w, "hello", &json!({ "api": API, "device": device.id, "voice": voice })) && send(w, "state", &state);
        let history = crate::history::path();
        let mut offset = std::fs::metadata(&history).map(|m| m.len()).unwrap_or(0);
        let (mut last_ping, mut last_check, mut last_history) = (Instant::now(), Instant::now(), Instant::now());
        while ok {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(out) => ok = send(w, &out.event, &out.data),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                // Dropped by the hub: revoked.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let s = self.ctx.shared.json();
            let v = s["version"].as_u64().unwrap_or(0);
            if ok && v != version {
                version = v;
                ok = send(w, "state", &s);
            }
            if ok && last_history.elapsed() > Duration::from_secs(1) {
                last_history = Instant::now();
                let (entries, end) = new_history(&history, offset);
                offset = end;
                for e in entries {
                    if !ok {
                        break;
                    }
                    ok = send(w, "history", &e);
                }
            }
            if ok && last_ping.elapsed() > Duration::from_secs(10) {
                last_ping = Instant::now();
                ok = w.write_all(b": ping\n\n").and_then(|_| w.flush()).is_ok();
            }
            // Revoked from the terminal meanwhile.
            if last_check.elapsed() > Duration::from_secs(5) {
                last_check = Instant::now();
                if !Devices::load_from(&self.devices_path).devices.iter().any(|d| d.id == device.id) {
                    break;
                }
            }
        }
        self.ctx.hub.unsubscribe(sid);
        if voice && !self.ctx.hub.voice() {
            self.ctx.shared.update(|s| s.phone = None);
        }
    }
}

/// History lines added since `offset` (whole lines only), and the new offset.
fn new_history(path: &std::path::Path, offset: u64) -> (Vec<Value>, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return (vec![], offset) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < offset {
        return (vec![], len);
    }
    if len == offset || f.seek(SeekFrom::Start(offset)).is_err() {
        return (vec![], offset);
    }
    let mut buf = Vec::new();
    let _ = f.take(4 * 1024 * 1024).read_to_end(&mut buf);
    let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else { return (vec![], offset) };
    let text = String::from_utf8_lossy(&buf[..=last_nl]);
    (text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect(), offset + last_nl as u64 + 1)
}

/// `t3`, `vps/t3`, a name: letters, digits, `-`, `_`, one `/`.
fn valid_address(a: &str) -> bool {
    !a.is_empty() && a.len() <= 64 && a.matches('/').count() <= 1 && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_/".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listens_only_on_private_addresses() {
        for ip in ["192.168.1.20", "10.0.0.5", "172.16.3.4", "100.101.102.103", "169.254.1.1", "fd00::1"] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "2001:db8::1"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
        let cfg = |bind: &str, allow_public| crate::config::Lan { enabled: true, port: 7843, bind: bind.into(), allow_public };
        assert!(choose_ip(&cfg("8.8.8.8", false)).is_err(), "a public address needs allow_public");
        assert!(choose_ip(&cfg("0.0.0.0", false)).is_err(), "every interface needs allow_public");
        assert!(choose_ip(&cfg("nonsense", false)).is_err());
        assert_eq!(choose_ip(&cfg("192.168.1.20", false)).unwrap().to_string(), "192.168.1.20");
    }

    /// A phone that pins the tower's certificate by its fingerprint, as the app does.
    #[derive(Debug)]
    struct Pin(String);

    impl rustls::client::danger::ServerCertVerifier for Pin {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            if auth::sha256_hex(end_entity.as_ref()) == self.0 {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General("not the paired tower".into()))
            }
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &rustls::pki_types::CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(m, c, d, &rustls::crypto::ring::default_provider().signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &rustls::pki_types::CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(m, c, d, &rustls::crypto::ring::default_provider().signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
        }
    }

    /// One HTTPS call: (status, JSON body), or Err when TLS refused.
    fn call(addr: SocketAddr, pin: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> Result<(u16, Value)> {
        use std::io::Read;
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin(pin.to_string())))
            .with_no_client_auth();
        let conn = rustls::ClientConnection::new(Arc::new(cfg), rustls::pki_types::ServerName::try_from("herder-tower")?.to_owned())?;
        let tcp = TcpStream::connect(addr)?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        write!(s, "{method} {path} HTTP/1.1\r\nHost: tower\r\n{auth}Content-Length: {}\r\n\r\n{body}", body.len())?;
        s.flush()?;
        let mut out = Vec::new();
        match s.read_to_end(&mut out) {
            Ok(_) => {}
            // The server closes after its close_notify; some stacks report the end as an error.
            Err(_) if !out.is_empty() => {}
            Err(e) => return Err(e.into()),
        }
        let text = String::from_utf8_lossy(&out);
        let status: u16 = text.split(' ').nth(1).unwrap_or("0").parse()?;
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
        Ok((status, serde_json::from_str(body).unwrap_or(Value::Null)))
    }

    #[test]
    fn pairing_and_tokens_over_https() {
        let dir = std::env::temp_dir().join(format!("tower-lan-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, _rx) = std::sync::mpsc::channel();
        let ctx = Ctx {
            shared: Shared::new(super::super::state::State::new("ptt", "af_bella", "test")),
            mouth: Mouth::silent(hub.clone()),
            hub,
            ack: None,
            stt,
        };
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan"));
        let addr = lan.start(&crate::config::Lan { enabled: true, port: 0, bind: "127.0.0.1".into(), allow_public: false }).unwrap();
        let fp = lan.info()["fingerprint"].as_str().unwrap().to_string();

        // Nothing without a token.
        let (st, v) = call(addr, &fp, "GET", "/v1/hello", None, "").unwrap();
        assert_eq!((st, v["error"]["code"].as_str()), (401, Some("unauthorized")));
        assert_eq!(call(addr, &fp, "GET", "/v1/state", Some("htk_madeupmadeupmadeupmadeupmadeupmadeupmad"), "").unwrap().0, 401);
        // No pairing until the Mac opens a code.
        let (st, v) = call(addr, &fp, "POST", "/v1/pair", None, r#"{"code":"12345678","name":"Pixel"}"#).unwrap();
        assert_eq!((st, v["error"]["code"].as_str()), (403, Some("pairing_closed")));

        let p = lan.pair().unwrap();
        let code = p["code"].as_str().unwrap().to_string();
        assert!(p["link"].as_str().unwrap().contains(&format!("fp={fp}")), "the QR carries the fingerprint to pin");
        let wrong = if code == "00000000" { "11111111" } else { "00000000" };
        let (st, v) = call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{wrong}"}}"#)).unwrap();
        assert_eq!((st, v["error"]["code"].as_str(), v["error"]["tries_left"].as_u64()), (403, Some("wrong_code"), Some(4)));
        let (st, v) = call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{code}","name":"Pixel 9"}}"#)).unwrap();
        assert_eq!(st, 200, "{v}");
        let token = v["token"].as_str().unwrap().to_string();
        let device = v["device"]["id"].as_str().unwrap().to_string();
        assert_eq!(v["fingerprint"].as_str(), Some(fp.as_str()));
        // The code worked once.
        assert_eq!(call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{code}"}}"#)).unwrap().0, 403);
        // The token is not on disk, only its hash.
        let file = std::fs::read_to_string(dir.join("devices.json")).unwrap();
        assert!(!file.contains(&token) && file.contains(&auth::sha256_hex(token.as_bytes())));

        let (st, v) = call(addr, &fp, "GET", "/v1/hello", Some(&token), "").unwrap();
        assert_eq!((st, v["device"]["id"].as_str(), v["device"]["name"].as_str()), (200, Some(device.as_str()), Some("Pixel 9")));
        let (st, v) = call(addr, &fp, "GET", "/v1/state", Some(&token), "").unwrap();
        assert_eq!((st, v["mode"].as_str()), (200, Some("ptt")));
        assert_eq!(call(addr, &fp, "GET", "/v1/nope", Some(&token), "").unwrap().0, 404);
        assert_eq!(call(addr, &fp, "POST", "/v1/voice", Some(&token), "not audio").unwrap().0, 415);

        // A phone that pins another certificate never talks to this tower.
        assert!(call(addr, &"0".repeat(64), "GET", "/v1/hello", Some(&token), "").is_err());

        // Revoked: the token stops working.
        let (st, _) = call(addr, &fp, "DELETE", "/v1/device", Some(&token), "").unwrap();
        assert_eq!(st, 200);
        assert_eq!(call(addr, &fp, "GET", "/v1/hello", Some(&token), "").unwrap().0, 401);

        lan.stop();
        assert!(lan.info()["on"] == false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_phone_takes_the_voice_while_its_stream_is_open() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("tower-lan-sse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, _rx) = std::sync::mpsc::channel();
        let shared = Shared::new(super::super::state::State::new("ptt", "af_bella", "test"));
        let ctx = Ctx { shared: shared.clone(), mouth: Mouth::silent(hub.clone()), hub: hub.clone(), ack: None, stt };
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan"));
        let addr = lan.start(&crate::config::Lan { enabled: true, port: 0, bind: "127.0.0.1".into(), allow_public: false }).unwrap();
        let fp = lan.info()["fingerprint"].as_str().unwrap().to_string();
        let code = lan.pair().unwrap()["code"].as_str().unwrap().to_string();
        let token =
            call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{code}"}}"#)).unwrap().1["token"].as_str().unwrap().to_string();

        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin(fp.clone())))
            .with_no_client_auth();
        let conn =
            rustls::ClientConnection::new(Arc::new(cfg), rustls::pki_types::ServerName::try_from("herder-tower").unwrap().to_owned())
                .unwrap();
        let tcp = TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut s = rustls::StreamOwned::new(conn, tcp);
        write!(s, "GET /v1/events?voice=1 HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n").unwrap();
        s.flush().unwrap();
        let mut seen = String::new();
        let mut read_until = |s: &mut rustls::StreamOwned<rustls::ClientConnection, TcpStream>, want: &str| {
            let mut buf = [0u8; 4096];
            while !seen.contains(want) {
                let n = s.read(&mut buf).expect("the stream keeps coming");
                assert!(n > 0, "stream ended before {want:?}: {seen}");
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            seen.clone()
        };
        let got = read_until(&mut s, "event: state");
        assert!(got.starts_with("HTTP/1.1 200 OK") && got.contains("text/event-stream") && got.contains("event: hello"), "{got}");
        assert!(hub.voice(), "an open voice stream makes the phone the voice");
        assert!(shared.get().phone.is_some());

        let id = hub.push_audio("ack", "Okay.", &[0.1; 4800], 24_000);
        let got = read_until(&mut s, "event: audio");
        assert!(got.contains(&format!("\"id\":\"{id}\"")) && got.contains("\"kind\":\"ack\""), "{got}");
        let (st, _) = call(addr, &fp, "GET", &format!("/v1/audio/{id}"), Some(&token), "").unwrap();
        assert_eq!(st, 200);

        // The phone goes away: the Mac speaks again.
        drop(s);
        for _ in 0..40 {
            if !hub.voice() {
                break;
            }
            hub.push_audio("reply", "Anyone there?", &[0.0; 240], 24_000);
            thread::sleep(Duration::from_millis(100));
        }
        assert!(!hub.voice(), "back to the Mac");
        assert!(shared.get().phone.is_none());
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn addresses() {
        assert!(valid_address("t3") && valid_address("vps/t2a") && valid_address("feed-filters"));
        assert!(!valid_address("") && !valid_address("a/b/c") && !valid_address("t3;rm") && !valid_address("../x"));
    }

    #[test]
    fn reads_new_history_lines_only() {
        let dir = std::env::temp_dir().join(format!("tower-lan-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("history.jsonl");
        std::fs::write(&p, "{\"kind\":\"a\"}\n").unwrap();
        let (_, off) = new_history(&p, 0);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"kind\":\"b\"}\n{\"kind\":\"c\"").unwrap();
        let (e, off2) = new_history(&p, off);
        assert_eq!(e.len(), 1, "the half-written line waits");
        assert_eq!(e[0]["kind"], "b");
        f.write_all(b"}\n").unwrap();
        let (e, _) = new_history(&p, off2);
        assert_eq!(e[0]["kind"], "c");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
