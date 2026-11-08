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
pub mod multipart;
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
    /// The open question sets (blocked sessions' dialogs).
    pub asks: super::asks::Asks,
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
    /// Who listens over the network (for events other parts of the daemon send).
    pub fn hub(&self) -> Arc<Hub> {
        self.ctx.hub.clone()
    }

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
                    "audio_route": device.audio_route,
                }),
            ),
            ("GET", ["state"]) => Response::json(200, &self.state(&device)),
            // Where this phone's replies are heard: `phone` (only there) or `mac`.
            ("GET", ["device", "audio"]) => Response::json(200, &json!({ "route": device.audio_route })),
            ("PUT", ["device", "audio"]) | ("POST", ["device", "audio"]) => {
                let body = req.json().unwrap_or_default();
                let route = body["route"].as_str().unwrap_or("");
                if !auth::valid_route(route) {
                    return Some(Response::error(400, "bad_route", "route is phone or mac"));
                }
                match self.set_route(&device.id, route) {
                    Ok(()) => Response::json(200, &json!({ "route": route })),
                    Err(e) => Response::error(503, "save", &format!("{e:#}")),
                }
            }
            // The user's behaviors (see docs/behaviors.md).
            ("GET", ["behaviors"]) => Response::json(200, &crate::behaviors::listing()),
            ("GET", ["behaviors", "history"]) => {
                let changes: Vec<Value> = crate::behaviors::Store::open().history().iter().rev().take(limit(50)).map(|c| c.view()).collect();
                Response::json(200, &json!({ "changes": changes }))
            }
            ("GET", ["behaviors", id]) => match crate::behaviors::Store::open().get(id) {
                Some(b) => Response::json(200, &serde_json::to_value(b).unwrap_or_default()),
                None => Response::error(404, "no_behavior", "no behavior with that id"),
            },
            ("POST", ["behaviors"]) => {
                let v = req.json().unwrap_or_default();
                let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
                behavior_change(crate::behaviors::Op::Add { title: s("title"), category: s("category"), summary: s("summary"), text: s("text") })
            }
            ("POST", ["behaviors", "undo"]) => behavior_change(crate::behaviors::Op::Undo),
            // What an edit would change, without saving it.
            ("POST", ["behaviors", id, "preview"]) => {
                let Some(b) = crate::behaviors::Store::open().get(id) else { return Some(Response::error(404, "no_behavior", "no behavior with that id")) };
                let text = req.json().unwrap_or_default()["text"].as_str().unwrap_or(&b.text).to_string();
                Response::json(200, &json!({ "id": id, "diff": crate::behaviors::diff(&b.text, &text) }))
            }
            ("PUT", ["behaviors", id]) => match serde_json::from_slice::<crate::behaviors::Patch>(&req.body) {
                Ok(patch) => behavior_change(crate::behaviors::Op::Edit { id: id.to_string(), patch }),
                Err(e) => Response::error(400, "bad_behavior", &format!("{e}")),
            },
            ("DELETE", ["behaviors", id]) => behavior_change(crate::behaviors::Op::Remove { id: id.to_string() }),
            // What blocked sessions ask (see docs/questions.md).
            ("GET", ["questions"]) => {
                let v: Vec<Value> = self.ctx.asks.list().iter().map(|s| serde_json::to_value(s).unwrap_or_default()).collect();
                Response::json(200, &json!({ "questions": v }))
            }
            ("GET", ["questions", id]) => match self.ctx.asks.get(id) {
                Some(s) => Response::json(200, &serde_json::to_value(s).unwrap_or_default()),
                None => Response::error(404, "no_question", "no open question set with that id"),
            },
            ("POST", ["questions", id, "answer"]) => return Some(self.answer_questions(req, id)),
            ("DELETE", ["questions", id]) => match self.ctx.asks.dismiss(id) {
                Ok(()) => Response::json(200, &json!({ "dismissed": id })),
                Err(_) => Response::error(404, "no_question", "no open question set with that id"),
            },
            // How questions are handled: normal (90/10), keep-moving, ask-everything.
            ("GET", ["questions-policy"]) => Response::json(200, &json!({ "policy": self.ctx.asks.policy() })),
            ("PUT", ["questions-policy"]) => {
                let p = req.json().unwrap_or_default()["policy"].as_str().unwrap_or("").to_string();
                if !["normal", "keep-moving", "ask-everything"].contains(&p.as_str()) {
                    return Some(Response::error(400, "bad_policy", "policy is normal, keep-moving or ask-everything"));
                }
                match crate::phone::set_question_policy(&p) {
                    Ok(()) => {
                        self.ctx.asks.set_policy(&p);
                        Response::json(200, &json!({ "policy": p }))
                    }
                    Err(e) => Response::error(503, "save", &format!("{e:#}")),
                }
            }
            // A spoken answer for a tab: text (from /v1/transcribe), or a WAV take.
            ("POST", ["questions", id, "interpret"]) => {
                let Some(set) = self.ctx.asks.get(id) else { return Some(Response::error(404, "no_question", "no open question set with that id")) };
                let body = req.json().unwrap_or_default();
                let tab = body["tab"].as_u64().unwrap_or(0) as usize;
                let Some(t) = set.tabs.get(tab) else { return Some(Response::error(400, "bad_tab", "no such tab")) };
                let p = crate::questions::interpret(body["text"].as_str().unwrap_or(""), t);
                Response::json(200, &json!({ "tab": tab, "choice": { "options": p.options, "other": p.other } }))
            }
            ("POST", ["questions", id, "voice"]) => {
                let Some(set) = self.ctx.asks.get(id) else { return Some(Response::error(404, "no_question", "no open question set with that id")) };
                let tab = q("tab").and_then(|t| t.parse::<usize>().ok()).unwrap_or(0);
                let Some(t) = set.tabs.get(tab).cloned() else { return Some(Response::error(400, "bad_tab", "no such tab")) };
                let heard = self.voice(req, false, &device);
                if heard.status != 200 {
                    return Some(heard);
                }
                let v: Value = serde_json::from_slice(&heard.body).unwrap_or_default();
                let text = v["heard"].as_str().unwrap_or("").to_string();
                let p = crate::questions::interpret(&text, &t);
                Response::json(200, &json!({ "tab": tab, "heard": text, "choice": { "options": p.options, "other": p.other } }))
            }
            // The main chat thread, as the panel shows it: newest last, in pages.
            ("GET", ["chat"]) => {
                let all = crate::chat::thread(&crate::chat::load(), &crate::history::attachments_dir());
                let (messages, more) = crate::chat::page(&all, q("before"), limit(50).min(200));
                Response::json(200, &json!({ "messages": messages, "more": more }))
            }
            // Clips this phone may have missed (no stream open): the last 15 minutes.
            ("GET", ["audio"]) => Response::json(200, &json!({ "clips": self.ctx.hub.clips_for(&device.id, q("after")) })),
            ("GET", ["attachments", folder, name]) => attachment(&crate::history::attachments_dir(), folder, name),
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
            // Bring a session on screen in Herdr (as `tower show <t>`).
            ("POST", ["sessions", address, "show"]) => {
                if !valid_address(address) {
                    return Some(Response::error(400, "bad_address", "an address looks like t3 or vps/t3"));
                }
                match crate::show_terminal(address) {
                    Ok(()) => Response::json(200, &json!({ "shown": address })),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg.starts_with("no terminal") {
                            Response::error(404, "no_session", &msg)
                        } else {
                            Response::error(503, "herdr", &msg)
                        }
                    }
                }
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
            ("GET", ["screenshots"]) => {
                let session = q("session");
                let entries = crate::history::load();
                let rows: Vec<Value> = crate::screenshots::recent(&entries, usize::MAX)
                    .into_iter()
                    .filter(|e| session.is_none_or(|s| e.session.as_deref() == Some(s)))
                    .take(limit(30))
                    .map(crate::screenshots::row)
                    .collect();
                Response::json(200, &json!({ "screenshots": rows }))
            }
            ("GET", ["screenshots", id]) => {
                if !crate::screenshots::valid_id(id) {
                    return Some(Response::error(400, "bad_id", "a screenshot id is its file name, from /v1/screenshots"));
                }
                match std::fs::read(crate::screenshots::dir().join(id)) {
                    Ok(bytes) => Response::bytes(crate::screenshots::content_type(id), bytes),
                    Err(_) => Response::error(404, "no_screenshot", "no such screenshot (they are kept for 30 days)"),
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
                let (text, files) = match ask_body(req) {
                    Ok(x) => x,
                    Err(r) => return Some(r),
                };
                if text.is_empty() && files.is_empty() {
                    return Some(Response::error(400, "empty", "send {\"text\": \"...\"}, or multipart text and files"));
                }
                let saved = match save_files(&files, &crate::history::attachments_dir(), now_ms()) {
                    Ok(p) => p,
                    Err(r) => return Some(r),
                };
                let paths: Vec<String> = saved.iter().map(|f| f.path.clone()).collect();
                let markers: Vec<String> = saved.iter().map(|f| f.marker.clone()).collect();
                let text = with_markers(&text, &markers);
                let mut resp = self.ask(&device, &crate::history::compose(&text, &paths));
                if resp.status == 202 {
                    resp = Response::json(202, &json!({ "sent": text, "attachments": paths }));
                }
                resp
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
                self.ask(&device, &format!("{verb} what {address} is asking for.{note}"))
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
            // Transcribe only: the text comes back, nothing goes to the tower.
            ("POST", ["voice"]) => self.voice(req, q("send") != Some("0"), &device),
            ("POST", ["transcribe"]) => self.voice(req, false, &device),
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

    fn ask(&self, device: &auth::Device, text: &str) -> Response {
        crate::daemon::timing::start("asked");
        let source = format!("phone:{}", device.id);
        match super::stt::deliver_from(&self.ctx.shared, &self.ctx.mouth, text, self.ctx.ack, heard_on_phone(device), &source) {
            Ok(()) => Response::json(202, &json!({ "sent": text })),
            Err(e) => Response::error(503, "brain", &crate::brain::spoken_error(&format!("{e:#}"))),
        }
    }

    /// A recording from the phone: transcribed by the tower's Whisper, then
    /// handed to the brain like a press-to-talk take.
    /// The state with what the brain is doing now (`work`, the panel's working line).
    fn state(&self, device: &auth::Device) -> Value {
        let mut s = self.ctx.shared.json();
        s["work"] = work_value();
        s["audio_route"] = json!(device.audio_route);
        s
    }

    /// `POST /v1/questions/{id}/answer`: JSON `{answers, submit}`, or
    /// multipart with that JSON as a `json` part and files for a tab's typed
    /// answer as parts named `tab<N>` (saved like the panel's attachments).
    fn answer_questions(&self, req: &Request, id: &str) -> Response {
        if self.ctx.asks.get(id).is_none() {
            return Response::error(404, "no_question", "no open question set with that id");
        }
        let (body, files): (Value, Vec<multipart::Part>) = match req.header("content-type").and_then(multipart::boundary) {
            Some(b) => match multipart::parse(&req.body, &b) {
                Ok(parts) => {
                    let (json_part, rest): (Vec<_>, Vec<_>) = parts.into_iter().partition(|p| p.name == "json");
                    let Some(j) = json_part.into_iter().next() else {
                        return Response::error(400, "no_json", "a multipart answer needs a `json` part with {answers, submit}");
                    };
                    (serde_json::from_slice(&j.data).unwrap_or_default(), rest)
                }
                Err(e) => return Response::error(400, "bad_multipart", &format!("{e:#}")),
            },
            None => (req.json().unwrap_or_default(), vec![]),
        };
        let mut answers: Vec<crate::questions::Answer> = match serde_json::from_value(body["answers"].clone()) {
            Ok(a) => a,
            Err(e) => return Response::error(400, "bad_answers", &format!("answers: {e}")),
        };
        // Files for tab N: saved, then attached to that tab's typed answer.
        for a in answers.iter_mut() {
            let mine: Vec<multipart::Part> =
                files.iter().filter(|p| p.name == format!("tab{}", a.tab)).map(|p| multipart::Part { name: p.name.clone(), filename: p.filename.clone(), content_type: p.content_type.clone(), data: p.data.clone() }).collect();
            if mine.is_empty() {
                continue;
            }
            match save_files(&mine, &crate::history::attachments_dir(), now_ms()) {
                Ok(saved) => a.attachments.extend(saved.into_iter().map(|s| s.path)),
                Err(r) => return r,
            }
        }
        let submit = body["submit"].as_bool().unwrap_or(true);
        match self.ctx.asks.answer(id, &answers, submit) {
            Ok(set) => Response::json(200, &json!({ "ok": true, "id": set.id, "submitted": submit })),
            Err(e) => {
                let msg = format!("{e:#}");
                let code = if msg.starts_with("stopped at") { "stopped" } else if msg.contains("changed") { "changed" } else { "cannot_answer" };
                Response::error(409, code, &msg)
            }
        }
    }

    /// Sets where a device's replies are heard, on file and for its open streams.
    pub fn set_route(&self, id: &str, route: &str) -> Result<()> {
        let mut d = Devices::load_from(&self.devices_path);
        if !d.set_route(id, route) {
            bail!("no paired device {id}, or route {route:?} isn't phone or mac");
        }
        d.save_to(&self.devices_path)?;
        self.ctx.hub.set_route(id, route == "phone");
        eprintln!("[phone] {id} hears its replies on the {route}");
        Ok(())
    }

    /// Another process changed the route on file (`tower phone route`, the panel).
    pub fn route_changed(&self, id: &str) {
        if let Some(d) = Devices::load_from(&self.devices_path).devices.iter().find(|d| d.id == id) {
            self.ctx.hub.set_route(id, d.audio_route == "phone");
        }
    }

    fn voice(&self, req: &Request, send: bool, device: &auth::Device) -> Response {
        // A WAV body, or multipart: the WAV as `audio`, files beside it.
        let (wav, images): (Vec<u8>, Vec<multipart::Part>) = match req.header("content-type").and_then(multipart::boundary) {
            Some(b) => match multipart::parse(&req.body, &b) {
                Ok(parts) => {
                    let (audio, rest): (Vec<_>, Vec<_>) = parts.into_iter().partition(|p| p.name == "audio");
                    let Some(audio) = audio.into_iter().next() else {
                        return Response::error(400, "no_audio", "a multipart voice request needs an `audio` part (the WAV)");
                    };
                    (audio.data, rest.into_iter().filter(is_file_part).collect())
                }
                Err(e) => return Response::error(400, "bad_multipart", &format!("{e:#}")),
            },
            None => (req.body.clone(), vec![]),
        };
        let saved = if send {
            match save_files(&images, &crate::history::attachments_dir(), now_ms()) {
                Ok(p) => p,
                Err(r) => return r,
            }
        } else {
            vec![]
        };
        let attachments: Vec<String> = saved.iter().map(|f| f.path.clone()).collect();
        let markers: Vec<String> = saved.iter().map(|f| f.marker.clone()).collect();
        let (samples, rate) = match audio::wav_decode(&wav) {
            Ok(x) => x,
            Err(e) => return Response::error(415, "bad_audio", &format!("{e:#}: send audio/wav, 16-bit PCM, mono, 16 kHz preferred")),
        };
        let secs = samples.len() as f32 / rate as f32;
        if !(0.2..=120.0).contains(&secs) {
            return Response::error(400, "bad_length", "a recording is 0.2 to 120 seconds");
        }
        let samples = if rate == RATE { samples } else { super::mic::resample(&samples, rate, RATE) };
        let (tx, rx) = std::sync::mpsc::channel();
        let source = Source::Phone { reply: tx, attachments: attachments.clone(), markers, send, phone: heard_on_phone(device), device: device.id.clone() };
        if self.ctx.stt.send(Utterance { samples, source }).is_err() {
            return Response::error(503, "stt", "the transcriber isn't running");
        }
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(heard) if send => Response::json(200, &json!({ "heard": heard, "sent": true, "attachments": attachments })),
            Ok(heard) => Response::json(200, &json!({ "heard": heard, "sent": false })),
            // Nothing worth sending (silence, noise, a stop word).
            Err(_) => Response::json(200, &json!({ "heard": "", "sent": false })),
        }
    }

    /// `GET /v1/events`: the panel's live feed, and with `voice=1` the tower's voice.
    fn events(&self, device: &auth::Device, voice: bool, w: &mut impl Write) {
        if http::write_sse_head(w).is_err() {
            return;
        }
        let (sid, rx) = self.ctx.hub.subscribe(&device.id, voice, device.audio_route == "phone");
        if voice {
            self.ctx.shared.update(|s| s.phone = Some(device.name.clone()));
        }
        let mut n = 0u64;
        let mut send = |w: &mut dyn Write, event: &str, data: &Value| -> bool {
            n += 1;
            w.write_all(http::sse_event(n, event, data).as_bytes()).and_then(|_| w.flush()).is_ok()
        };
        let state = self.state(device);
        let mut version = state["version"].as_u64().unwrap_or(0);
        let mut work = state["work"].clone();
        let mut ok = send(w, "hello", &json!({ "api": API, "device": device.id, "voice": voice })) && send(w, "state", &state);
        let history = crate::history::path();
        let mut offset = std::fs::metadata(&history).map(|m| m.len()).unwrap_or(0);
        // The chat's latest messages as last sent: a new or changed one is a `chat` event.
        let mut chat = ChatTail::default();
        chat.diff(&crate::chat::thread(&crate::chat::load(), &crate::history::attachments_dir()));
        let (mut last_ping, mut last_check, mut last_history) = (Instant::now(), Instant::now(), Instant::now());
        while ok {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(out) => ok = send(w, &out.event, &out.data),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                // Dropped by the hub: revoked.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let v = self.ctx.shared.get().version;
            let now_work = work_value();
            if ok && (v != version || now_work != work) {
                version = v;
                work = now_work;
                ok = send(w, "state", &self.state(device));
            }
            if ok && last_history.elapsed() > Duration::from_secs(1) {
                last_history = Instant::now();
                let (entries, end) = new_history(&history, offset);
                offset = end;
                let grew = !entries.is_empty();
                for e in entries {
                    if !ok {
                        break;
                    }
                    ok = send(w, "history", &e);
                }
                if grew {
                    for (op, m) in chat.diff(&crate::chat::thread(&crate::chat::load(), &crate::history::attachments_dir())) {
                        if !ok {
                            break;
                        }
                        ok = send(w, "chat", &json!({ "op": op, "message": m }));
                    }
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

/// The chat messages a stream has seen (the latest ones), to send only what is new or changed.
#[derive(Default)]
struct ChatTail {
    seen: std::collections::HashMap<String, String>,
}

impl ChatTail {
    /// `add` for a message not seen, `update` for one that changed (a screenshot card that grew).
    fn diff(&mut self, thread: &[Value]) -> Vec<(&'static str, Value)> {
        let mut out = vec![];
        for m in thread.iter().rev().take(60).collect::<Vec<_>>().into_iter().rev() {
            let id = m["id"].as_str().unwrap_or("").to_string();
            let body = m.to_string();
            match self.seen.insert(id, body.clone()) {
                None => out.push(("add", m.clone())),
                Some(before) if before != body => out.push(("update", m.clone())),
                Some(_) => {}
            }
        }
        if self.seen.len() > 500 {
            let keep: std::collections::HashSet<String> = thread.iter().rev().take(60).filter_map(|m| m["id"].as_str().map(String::from)).collect();
            self.seen.retain(|k, _| keep.contains(k));
        }
        out
    }
}

/// A change to the behaviors from the phone: applied, then the daemon tells everyone.
fn behavior_change(op: crate::behaviors::Op) -> Response {
    let adding = matches!(op, crate::behaviors::Op::Add { .. });
    match crate::behaviors::apply(op, "user") {
        Ok((b, c)) => {
            let view = c.view();
            let _ = crate::protocol::send(&crate::protocol::Request::BehaviorsChanged { by: "user".into(), change: view.clone() });
            Response::json(if adding { 201 } else { 200 }, &json!({ "behavior": b, "change": view }))
        }
        Err(e) => {
            let msg = format!("{e:#}");
            Response::error(if msg.starts_with("no behavior") { 404 } else { 400 }, "behavior", &msg)
        }
    }
}

/// What a phone sends is answered on that phone only when its route is `phone`.
fn heard_on_phone(device: &auth::Device) -> Option<String> {
    (device.audio_route == "phone").then(|| device.id.clone())
}

/// Most files one message carries.
pub const MAX_FILES: usize = 6;
/// A video, PDF or other file (an image is 15 MB at most).
pub const MAX_FILE_BYTES: usize = 50 * 1024 * 1024;

/// What the brain is doing now, as the panel's working line shows it:
/// `{word, detail, since_ms, at_ms, done_ms}`, or null between turns.
fn work_value() -> Value {
    match crate::activity::read() {
        Some(w) if w.since_ms > 0 => json!({ "word": w.word, "detail": w.detail, "since_ms": w.since_ms, "at_ms": w.at_ms, "done_ms": w.done_ms }),
        _ => Value::Null,
    }
}

/// A part that is a file (a field with a file name, or one named `image`/`file`).
fn is_file_part(p: &multipart::Part) -> bool {
    p.filename.is_some() || p.name.starts_with("image") || p.name.starts_with("file")
}

/// The text and files of `POST /v1/ask`: JSON `{"text"}`, or multipart with
/// a `text` field and file parts.
fn ask_body(req: &Request) -> Result<(String, Vec<multipart::Part>), Response> {
    match req.header("content-type").and_then(multipart::boundary) {
        Some(b) => {
            let parts = multipart::parse(&req.body, &b).map_err(|e| Response::error(400, "bad_multipart", &format!("{e:#}")))?;
            let text = parts.iter().find(|p| p.name == "text" && p.filename.is_none()).map(|p| String::from_utf8_lossy(&p.data).trim().to_string());
            Ok((text.unwrap_or_default(), parts.into_iter().filter(|p| p.name != "text" && is_file_part(p)).collect()))
        }
        None => {
            let body = req.json().map_err(|_| Response::error(400, "bad_json", "send {\"text\": \"...\"}"))?;
            Ok((body["text"].as_str().unwrap_or("").trim().to_string(), vec![]))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FileKind {
    Image,
    Video,
    Other,
}

/// As the panel tells them apart: images by their bytes (or name), videos
/// by name or type, anything else is a file.
pub fn file_kind(name: &str, content_type: Option<&str>, head: &[u8]) -> FileKind {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    if crate::screenshots::kind_of(head).is_some() || matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "bmp" | "tiff") {
        FileKind::Image
    } else if matches!(ext.as_str(), "mov" | "mp4" | "m4v" | "webm" | "mkv" | "avi") || content_type.is_some_and(|t| t.starts_with("video/")) {
        FileKind::Video
    } else {
        FileKind::Other
    }
}

/// The panel's token for the Nth file: `[Image #1]`, `[Video #2]`, `[File #3: notes.pdf]`.
pub fn marker(kind: FileKind, n: usize, name: &str) -> String {
    let name: String = name.chars().filter(|c| !matches!(c, '[' | ']' | '\n' | '\r')).collect();
    match kind {
        FileKind::Image => format!("[Image #{n}]"),
        FileKind::Video => format!("[Video #{n}]"),
        FileKind::Other => format!("[File #{n}: {}]", if name.trim().is_empty() { "file" } else { name.trim() }),
    }
}

/// The text with a token for every file it doesn't mark yet, in front (as
/// the panel puts a token where you attach). A token is there when the text
/// has its `[Image #N]`, `[Video #N]` or `[File #N:` already.
pub fn with_markers(text: &str, markers: &[String]) -> String {
    // Only images, numbered in order: the same as the panel's image tokens.
    if markers.iter().enumerate().all(|(i, m)| *m == format!("[Image #{}]", i + 1)) {
        return crate::history::with_image_markers(text, markers.len());
    }
    let present = |m: &String| match m.split_once(':') {
        Some((head, _)) if m.starts_with("[File #") => text.contains(&format!("{head}:")),
        _ => text.contains(m.as_str()),
    };
    let missing: Vec<&String> = markers.iter().filter(|m| !present(m)).collect();
    let text = text.trim();
    match (missing.is_empty(), text.is_empty()) {
        (true, _) => text.to_string(),
        (false, true) => missing.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(" "),
        (false, false) => format!("{} {text}", missing.iter().map(|m| m.as_str()).collect::<Vec<_>>().join(" ")),
    }
}

/// A file saved for a message: where, and its token.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub path: String,
    pub marker: String,
}

/// Checks files and saves them as the panel saves its attachments:
/// `<attachments>/<ms>-phone/<n>-<name>`, never pruned. Six at most; an
/// image is a real PNG, JPEG, WebP or GIF of 15 MB at most; a video or other
/// file is 50 MB at most.
pub fn save_files(parts: &[multipart::Part], attachments: &std::path::Path, at_ms: u64) -> Result<Vec<Saved>, Response> {
    if parts.is_empty() {
        return Ok(vec![]);
    }
    if parts.len() > MAX_FILES {
        return Err(Response::error(413, "too_many_files", &format!("{MAX_FILES} files at most")));
    }
    let mut plan = vec![];
    for (i, p) in parts.iter().enumerate() {
        let original = p.filename.clone().unwrap_or_else(|| "file".into());
        let head = &p.data[..p.data.len().min(16)];
        let kind = file_kind(&original, p.content_type.as_deref(), head);
        let ext = match kind {
            FileKind::Image => {
                if p.data.len() as u64 > crate::screenshots::MAX_BYTES {
                    return Err(Response::error(413, "image_too_large", &format!("file {} is an image over 15 MB", i + 1)));
                }
                let Some(ext) = crate::screenshots::kind_of(head) else {
                    return Err(Response::error(415, "not_an_image", &format!("file {} is named as an image but isn't a PNG, JPEG, WebP or GIF", i + 1)));
                };
                ext.to_string()
            }
            _ => {
                if p.data.len() > MAX_FILE_BYTES {
                    return Err(Response::error(413, "file_too_large", &format!("file {} is over 50 MB", i + 1)));
                }
                let e: String = original.rsplit_once('.').map(|(_, e)| e).unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
                if e.is_empty() { "bin".into() } else { e.to_ascii_lowercase() }
            }
        };
        plan.push((kind, original, ext));
    }
    let dir = attachments.join(format!("{at_ms}-phone"));
    std::fs::create_dir_all(&dir).map_err(|e| Response::error(503, "save", &e.to_string()))?;
    let mut out = vec![];
    for (i, (p, (kind, original, ext))) in parts.iter().zip(plan).enumerate() {
        // `<ms>-<name>.<ext>` from the screenshot naming, without the ms (the folder has it).
        let name = crate::screenshots::stored_name(0, std::path::Path::new(&original), &ext);
        let name = format!("{}-{}", i + 1, name.trim_start_matches("0-"));
        let path = dir.join(&name);
        std::fs::write(&path, &p.data).map_err(|e| Response::error(503, "save", &e.to_string()))?;
        let shown = std::path::Path::new(&original).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
        out.push(Saved { path: path.to_string_lossy().into_owned(), marker: marker(kind, i + 1, &shown) });
    }
    Ok(out)
}

/// `GET /v1/attachments/{folder}/{name}`: a picture from the attachments
/// folder, for a sent message. Only a file directly in one of its folders,
/// only an image, and only if it really is inside (no `..`, no links out).
pub fn attachment(root: &std::path::Path, folder: &str, name: &str) -> Response {
    let ok = |s: &str| !s.is_empty() && s.len() <= 255 && !s.starts_with('.') && !s.contains(['/', '\\', '\0']);
    if !ok(folder) || !ok(name) {
        return Response::error(400, "bad_path", "an attachment is <folder>/<name>, as in its path under attachments/");
    }
    let (Ok(root), Ok(path)) = (root.canonicalize(), root.join(folder).join(name).canonicalize()) else {
        return Response::error(404, "no_attachment", "no such attachment");
    };
    if !path.starts_with(&root) || path.parent().and_then(|p| p.parent()) != Some(root.as_path()) || !path.is_file() {
        return Response::error(404, "no_attachment", "no such attachment");
    }
    let Ok(bytes) = std::fs::read(&path) else { return Response::error(404, "no_attachment", "no such attachment") };
    match crate::screenshots::kind_of(&bytes[..bytes.len().min(16)]) {
        Some(ext) => Response::bytes(crate::screenshots::content_type(&format!("x.{ext}")), bytes),
        None => Response::error(415, "not_an_image", "only pictures are served"),
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

    /// One HTTPS call with a binary body and a content type.
    fn call_raw(addr: SocketAddr, pin: &str, method: &str, path: &str, token: &str, ctype: &str, body: &[u8]) -> Result<(u16, Value)> {
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
        write!(s, "{method} {path} HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n", body.len())?;
        s.write_all(body)?;
        s.flush()?;
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let text = String::from_utf8_lossy(&out);
        let status: u16 = text.split(' ').nth(1).unwrap_or("0").parse()?;
        Ok((status, serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap_or(Value::Null)))
    }

    #[test]
    fn pairing_and_tokens_over_https() {
        let dir = std::env::temp_dir().join(format!("tower-lan-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, _rx) = std::sync::mpsc::channel();
        let shared = Shared::new(super::super::state::State::new("ptt", "af_bella", "test"));
        let asks = super::super::asks::Asks::new(shared.clone(), hub.clone(), "normal");
        let ctx = Ctx { shared, mouth: Mouth::silent(hub.clone()), hub, ack: None, stt, asks };
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
        // Multipart asks: images are checked before anything reaches the tower.
        let mp = |path: &str, files: &[(&str, &str, &str, &[u8])]| {
            let body = multipart::build("XB", &[("text", "what is this?")], files);
            call_raw(addr, &fp, "POST", path, &token, "multipart/form-data; boundary=XB", &body).unwrap()
        };
        assert_eq!(mp("/v1/ask", &[("image", "a.png", "image/png", b"not an image")]).0, 415);
        let seven: Vec<(&str, &str, &str, &[u8])> = (0..7).map(|_| ("image", "a.png", "image/png", &b"\x89PNG\r\n\x1a\n"[..])).collect();
        assert_eq!(mp("/v1/ask", &seven).0, 413);
        assert_eq!(mp("/v1/voice", &[("image", "a.png", "image/png", b"\x89PNG\r\n\x1a\n")]).0, 400, "voice needs its audio part");
        assert_eq!(call(addr, &fp, "POST", "/v1/transcribe", Some(&token), "not audio").unwrap().0, 415);
        assert_eq!(call(addr, &fp, "POST", "/v1/voice?send=0", Some(&token), "not audio").unwrap().0, 415);
        assert_eq!(call(addr, &fp, "GET", "/v1/attachments/..%2F..%2Fx/y.png", Some(&token), "").unwrap().0, 400);
        assert_eq!(call(addr, &fp, "GET", "/v1/attachments/0-none/none.png", None, "").unwrap().0, 401);
        let (st, v) = call(addr, &fp, "GET", "/v1/state", Some(&token), "").unwrap();
        assert!(st == 200 && v.get("work").is_some(), "the state carries the working line: {v}");
        // Questions: listed, and an unknown id is refused before anything is typed.
        let (st, v) = call(addr, &fp, "GET", "/v1/questions", Some(&token), "").unwrap();
        assert!(st == 200 && v["questions"].is_array());
        assert_eq!(call(addr, &fp, "GET", "/v1/questions/qnope", Some(&token), "").unwrap().0, 404);
        let (st, v) = call(addr, &fp, "POST", "/v1/questions/qnope/answer", Some(&token), r#"{"answers":[{"tab":0,"choice":0}]}"#).unwrap();
        assert_eq!((st, v["error"]["code"].as_str()), (404, Some("no_question")));
        assert_eq!(call(addr, &fp, "POST", "/v1/questions/qnope/interpret", Some(&token), r#"{"tab":0,"text":"the first"}"#).unwrap().0, 404);
        // Where this phone hears its replies: the phone by default, kept on file.
        let (st, v) = call(addr, &fp, "GET", "/v1/device/audio", Some(&token), "").unwrap();
        assert_eq!((st, v["route"].as_str()), (200, Some("phone")));
        assert_eq!(call(addr, &fp, "PUT", "/v1/device/audio", Some(&token), r#"{"route":"speaker"}"#).unwrap().0, 400);
        let (st, v) = call(addr, &fp, "PUT", "/v1/device/audio", Some(&token), r#"{"route":"mac"}"#).unwrap();
        assert_eq!((st, v["route"].as_str()), (200, Some("mac")));
        assert_eq!(call(addr, &fp, "GET", "/v1/hello", Some(&token), "").unwrap().1["audio_route"], "mac");
        assert_eq!(call(addr, &fp, "GET", "/v1/state", Some(&token), "").unwrap().1["audio_route"], "mac");
        assert!(std::fs::read_to_string(dir.join("devices.json")).unwrap().contains("\"audio_route\": \"mac\""), "it survives a restart");
        let (st, v) = call(addr, &fp, "GET", "/v1/audio", Some(&token), "").unwrap();
        assert!(st == 200 && v["clips"].is_array());
        // Showing a session: an unknown one is a 404, a bad address a 400, and a token is needed.
        assert_eq!(call(addr, &fp, "POST", "/v1/sessions/qnope-no-such-terminal/show", Some(&token), "").unwrap().0, 404);
        assert_eq!(call(addr, &fp, "POST", "/v1/sessions/a%3Bb/show", Some(&token), "").unwrap().0, 400);
        assert_eq!(call(addr, &fp, "POST", "/v1/sessions/t1/show", None, "").unwrap().0, 401);
        // Screenshots: by id, never by path; the token is needed as everywhere.
        assert_eq!(call(addr, &fp, "GET", "/v1/screenshots/..%2F..%2Fdevices.json", Some(&token), "").unwrap().0, 400);
        assert_eq!(call(addr, &fp, "GET", "/v1/screenshots/0-no-such-shot.png", Some(&token), "").unwrap().0, 404);
        assert_eq!(call(addr, &fp, "GET", "/v1/screenshots", None, "").unwrap().0, 401);
        let (st, v) = call(addr, &fp, "GET", "/v1/screenshots?limit=2", Some(&token), "").unwrap();
        assert!(st == 200 && v["screenshots"].is_array(), "{st} {v}");

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
        let asks = super::super::asks::Asks::new(shared.clone(), hub.clone(), "normal");
        let ctx = Ctx { shared: shared.clone(), mouth: Mouth::silent(hub.clone()), hub: hub.clone(), ack: None, stt, asks };
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

        let id = hub.push_audio("ack", "Okay.", &[0.1; 4800], 24_000, None);
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
            hub.push_audio("reply", "Anyone there?", &[0.0; 240], 24_000, None);
            thread::sleep(Duration::from_millis(100));
        }
        assert!(!hub.voice(), "back to the Mac");
        assert!(shared.get().phone.is_none());
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_phone_files_like_panel_attachments() {
        let dir = std::env::temp_dir().join(format!("tower-lan-att-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let png: &[u8] = b"\x89PNG\r\n\x1a\n....";
        let part = |f: &str, ct: Option<&str>, d: &[u8]| multipart::Part {
            name: "file".into(),
            filename: Some(f.into()),
            content_type: ct.map(String::from),
            data: d.to_vec(),
        };
        let saved = save_files(
            &[
                part("../../etc/Screen Shot.png", None, png),
                part("clip.MOV", Some("video/quicktime"), b"\0\0\0 ftypqt"),
                part("Q3 report [final].pdf", Some("application/pdf"), b"%PDF-1.7"),
                part("noext", None, b"abc"),
            ],
            &dir,
            42,
        )
        .unwrap();
        let tail = |s: &Saved| s.path.rsplit_once("/42-phone/").map(|(_, t)| t.to_string()).unwrap();
        assert_eq!(saved.iter().map(tail).collect::<Vec<_>>(), ["1-Screen-Shot.png", "2-clip.mov", "3-Q3-report--final.pdf", "4-noext.bin"]);
        assert_eq!(
            saved.iter().map(|s| s.marker.as_str()).collect::<Vec<_>>(),
            ["[Image #1]", "[Video #2]", "[File #3: Q3 report final.pdf]", "[File #4: noext]"],
            "the panel's tokens"
        );
        assert!(saved.iter().all(|p| p.path.starts_with(dir.to_str().unwrap())), "names can't climb out of the folder");
        assert_eq!(std::fs::read(&saved[0].path).unwrap(), png);
        assert_eq!(save_files(&[part("x.png", None, b"text")], &dir, 43).unwrap_err().status, 415, "named as an image, isn't one");
        let seven: Vec<_> = (0..7).map(|i| part(&format!("{i}.png"), None, png)).collect();
        assert_eq!(save_files(&seven, &dir, 44).unwrap_err().status, 413);
        assert_eq!(save_files(&[part("big.mp4", None, &vec![0; MAX_FILE_BYTES + 1])], &dir, 46).unwrap_err().status, 413);
        assert!(save_files(&[], &dir, 45).unwrap().is_empty());
        // Screenshot pruning never looks here.
        assert_eq!(crate::screenshots::prune(&dir, Duration::from_secs(0)), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markers_like_the_panel() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(with_markers("what is this?", &m(&["[Image #1]", "[Video #2]"])), "[Image #1] [Video #2] what is this?");
        assert_eq!(with_markers("see [File #2: a.pdf] and [Image #1]", &m(&["[Image #1]", "[File #2: a.pdf]"])), "see [File #2: a.pdf] and [Image #1]");
        assert_eq!(with_markers("see [File #1: renamed.pdf]", &m(&["[File #1: a.pdf]"])), "see [File #1: renamed.pdf]", "a file token counts by its number");
        assert_eq!(with_markers("", &m(&["[File #1: a.pdf]"])), "[File #1: a.pdf]");
        assert_eq!(with_markers("x", &m(&["[Image #1]", "[Image #2]"])), crate::history::with_image_markers("x", 2));
        assert_eq!(with_markers("plain", &[]), "plain");
    }

    #[test]
    fn serves_only_pictures_inside_attachments() {
        let root = std::env::temp_dir().join(format!("tower-lan-get-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let png: &[u8] = b"\x89PNG\r\n\x1a\n....";
        let att = root.join("attachments");
        std::fs::create_dir_all(att.join("1794-phone")).unwrap();
        std::fs::write(att.join("1794-phone/1-a.png"), png).unwrap();
        std::fs::write(att.join("1794-phone/Screen Shot 10.00.png"), png).unwrap();
        std::fs::write(att.join("1794-phone/notes.pdf"), b"%PDF").unwrap();
        std::fs::write(root.join("secret.png"), png).unwrap();
        std::os::unix::fs::symlink(root.join("secret.png"), att.join("1794-phone/link.png")).unwrap();
        std::os::unix::fs::symlink(&root, att.join("up")).unwrap();
        let r = attachment(&att, "1794-phone", "1-a.png");
        assert_eq!((r.status, r.content_type, r.body.as_slice()), (200, "image/png", png));
        assert_eq!(attachment(&att, "1794-phone", "Screen Shot 10.00.png").status, 200, "the panel keeps the original name");
        assert_eq!(attachment(&att, "1794-phone", "notes.pdf").status, 415);
        assert_eq!(attachment(&att, "1794-phone", "missing.png").status, 404);
        for (f, n) in [("..", "secret.png"), ("1794-phone", ".."), ("1794-phone/..", "x"), ("", "a"), (".hidden", "a.png"), ("a\\b", "c")] {
            assert_eq!(attachment(&att, f, n).status, 400, "{f}/{n}");
        }
        assert_eq!(attachment(&att, "1794-phone", "link.png").status, 404, "a link out of attachments isn't followed");
        assert_eq!(attachment(&att, "up", "secret.png").status, 404, "nor a folder that links out");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_ask_bodies_both_ways() {
        let json_req = Request { body: br#"{"text":" hi "}"#.to_vec(), ..Default::default() };
        let (t, imgs) = ask_body(&json_req).ok().unwrap();
        assert_eq!((t.as_str(), imgs.len()), ("hi", 0));
        let body = multipart::build("BB", &[("text", "what's this?")], &[("image", "a.png", "image/png", b"\x89PNG\r\n\x1a\n"), ("images", "b.gif", "image/gif", b"GIF89a")]);
        let mut req = Request { body, ..Default::default() };
        req.headers.insert("content-type".into(), "multipart/form-data; boundary=BB".into());
        let (t, files) = ask_body(&req).ok().unwrap();
        assert_eq!((t.as_str(), files.len()), ("what's this?", 2));
        let bad = Request { body: b"nope".to_vec(), ..Default::default() };
        assert_eq!(ask_body(&bad).err().unwrap().status, 400);
    }

    #[test]
    fn chat_events_add_and_update() {
        let mut tail = ChatTail::default();
        let m = |id: &str, n: usize| json!({ "id": id, "screenshots": vec![0; n] });
        assert_eq!(tail.diff(&[m("h1", 0), m("h2", 1)]).len(), 2, "the first look: everything is new");
        assert!(tail.diff(&[m("h1", 0), m("h2", 1)]).is_empty(), "nothing changed: nothing sent");
        let d = tail.diff(&[m("h1", 0), m("h2", 2), m("h5", 0)]);
        assert_eq!(d.iter().map(|(op, m)| (*op, m["id"].as_str().unwrap())).collect::<Vec<_>>(), [("update", "h2"), ("add", "h5")]);
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
