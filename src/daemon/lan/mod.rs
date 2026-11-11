//! The phone API: the tower over the local network, for the Android app.
//! The contract, the pairing flow and the security model are in
//! `docs/mobile-api.md`.
//!
//! Off unless `[lan] enabled = true` (`tower phone enable`). HTTPS only, on
//! this Mac's private LAN address, and on its Tailscale address too while
//! Tailscale runs (`[lan] tailscale = "auto"`, see `tailscale`); never on
//! every interface. Every call but pairing needs a paired device's token.

pub mod audio;
pub mod auth;
pub mod http;
pub mod hub;
pub mod multipart;
mod quiet;
pub mod tailscale;
pub mod tls;
pub mod tower;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::mic::{RATE, Source, Utterance};
use super::state::{Shared, now_ms};
use super::tts::Mouth;
use auth::{PairError, Pairing};
use http::{Request, Response};
use hub::Hub;
use tls::Identity;

/// The API version in every path (`/v1/...`).
pub const API: u32 = 1;
/// What this tower's API has beyond the first `/v1` (`GET /v1/hello`'s
/// `features`): an app checks a name here before it uses that part.
/// `quiet`: the decisions, missions, threads, launch, tell level and model
/// calls, a request's `space`, and the events that go with them.
pub const FEATURES: &[&str] = &["quiet", "acts"];
const MAX_CONNECTIONS: usize = 32;
/// Connections open at once from one address.
const MAX_PER_IP: usize = 8;
/// The TLS handshake and the request head must arrive within this, however slowly they trickle.
const HEAD_DEADLINE: Duration = Duration::from_secs(10);
/// Longest wait for a read once the caller is known.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// An event stream whose phone hasn't taken what was sent for this long is gone.
const STREAM_STUCK: Duration = Duration::from_secs(30);
/// Failed logins (bad token or code) from one address per minute before it's refused for the minute.
const MAX_FAILURES: u32 = 20;

/// What the API needs from the daemon.
#[derive(Clone)]
pub struct Ctx {
    pub shared: Shared,
    pub mouth: Mouth,
    pub hub: Arc<Hub>,
    pub stt: Sender<Utterance>,
    /// The open question sets (blocked sessions' dialogs).
    pub asks: super::asks::Asks,
    /// The PRs the sessions opened (`crate::prs`).
    pub prs: crate::prs::Prs,
}

/// Where the API can be reached: this Mac's local network, or its tailnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Lan,
    Tailscale,
}

impl Kind {
    pub fn word(&self) -> &'static str {
        match self {
            Kind::Lan => "lan",
            Kind::Tailscale => "tailscale",
        }
    }
}

/// One listening socket: the flag that ends its accept loop, and that loop,
/// which owns the socket (it's closed when the loop ends).
struct Listener {
    kind: Kind,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

struct Running {
    /// The LAN's first (when there is one), then the tailnet's.
    listeners: Vec<Listener>,
    /// The whole API: every accept loop, open connection and event stream ends.
    stop: Arc<AtomicBool>,
    fingerprint: String,
    tls: Arc<rustls::ServerConfig>,
    cfg: crate::config::Lan,
    /// Tailscale as last seen.
    tailscale: tailscale::Tailscale,
    /// Why the tailnet address isn't listened on though Tailscale runs (a bind that failed).
    tailscale_error: Option<String>,
}

impl Running {
    fn primary(&self) -> SocketAddr {
        self.listeners[0].addr
    }

    fn of(&self, kind: Kind) -> Option<&Listener> {
        self.listeners.iter().find(|l| l.kind == kind)
    }
}

/// How the phone API asks about Tailscale (tests give their own).
pub type Detect = Arc<dyn Fn() -> tailscale::Tailscale + Send + Sync>;

/// How it asks this Mac's address on the network it is on now (`lan_ip`; tests give their own).
pub type LanDetect = Arc<dyn Fn() -> Option<IpAddr> + Send + Sync>;

/// Tailscale is looked at again this often while the API is on: it starts
/// (or stops) listening on the tailnet as Tailscale comes and goes.
const TAILSCALE_RECHECK: Duration = Duration::from_secs(20);

/// How often an accept loop looks at its stop flag. The loop never blocks in
/// `accept`: a listener whose address is gone (Tailscale stopped) can't be
/// woken by connecting to it, and a loop that never woke kept its socket, so
/// the address couldn't be listened on again when it came back.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

#[derive(Clone)]
pub struct Lan {
    ctx: Ctx,
    running: Arc<Mutex<Option<Running>>>,
    pairing: Arc<Mutex<Pairing>>,
    failures: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
    conns: Arc<Mutex<Conns>>,
    /// `devices.json` (elsewhere in tests).
    devices: Arc<auth::Store>,
    /// The certificate's folder (elsewhere in tests).
    identity_dir: std::path::PathBuf,
    detect: Detect,
    /// This Mac's address on the network it is on now, for `bind = "auto"`.
    lan_detect: LanDetect,
    /// The rest of the tower (`tower::Tower`): the daemon, once it is whole.
    tower: Arc<std::sync::OnceLock<Arc<dyn tower::Tower>>>,
    /// `history.jsonl` (elsewhere in tests).
    history: std::path::PathBuf,
    /// The launches and closes the phones started.
    actions: tower::Actions,
}

/// The connections open now, in all and by address.
#[derive(Default)]
struct Conns {
    total: usize,
    by_ip: HashMap<IpAddr, usize>,
    /// When a refused connection was last logged.
    warned: Option<Instant>,
}

/// A connection's place in `Conns`, given back when it's dropped (however
/// the connection ends, a panic included).
struct Slot {
    conns: Arc<Mutex<Conns>>,
    ip: IpAddr,
}

impl Slot {
    /// None: too many open, in all or from this address.
    fn take(conns: &Arc<Mutex<Conns>>, ip: IpAddr) -> Option<Slot> {
        let mut c = conns.lock().unwrap_or_else(|e| e.into_inner());
        let mine = c.by_ip.get(&ip).copied().unwrap_or(0);
        if c.total >= MAX_CONNECTIONS || mine >= MAX_PER_IP {
            if c.warned.is_none_or(|w| w.elapsed() > Duration::from_secs(60)) {
                c.warned = Some(Instant::now());
                eprintln!("[phone] refusing connections from {ip}: {mine} open from there, {} in all", c.total);
            }
            return None;
        }
        c.total += 1;
        *c.by_ip.entry(ip).or_insert(0) += 1;
        Some(Slot { conns: conns.clone(), ip })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut c = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.by_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                c.by_ip.remove(&self.ip);
            }
        }
    }
}

/// A connection's socket. Until the caller is known, its reads share one
/// deadline (a client that trickles a byte at a time can't hold it open).
struct Sock {
    tcp: TcpStream,
    deadline: Option<Instant>,
}

impl Sock {
    fn no_deadline(&mut self) {
        self.deadline = None;
        let _ = self.tcp.set_read_timeout(Some(READ_TIMEOUT));
    }
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(d) = self.deadline {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "too slow"));
            }
            self.tcp.set_read_timeout(Some(left.min(READ_TIMEOUT)))?;
        }
        self.tcp.read(buf)
    }
}

impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tcp.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.tcp.flush()
    }
}

type Stream = rustls::StreamOwned<rustls::ServerConnection, Sock>;

/// TCP keepalive, and a cap on how long sent data may go unacknowledged:
/// a phone that vanished (Wi-Fi off, out of range) is noticed in about 30 s.
fn keepalive(tcp: &TcpStream) {
    let fd = tcp.as_raw_fd();
    let set = |level: libc::c_int, name: libc::c_int, v: libc::c_int| unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            &v as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    #[cfg(target_os = "macos")]
    {
        // `TCP_RXT_CONNDROPTIME` (netinet/tcp.h): drop after retransmitting this many seconds.
        const TCP_RXT_CONNDROPTIME: libc::c_int = 0x80;
        set(libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, 10);
        set(libc::IPPROTO_TCP, TCP_RXT_CONNDROPTIME, STREAM_STUCK.as_secs() as libc::c_int);
    }
    #[cfg(target_os = "linux")]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 10);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5);
        set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    }
}

/// Bytes written to the socket that the phone hasn't acknowledged yet (macOS).
fn unsent(fd: RawFd) -> Option<i32> {
    #[cfg(target_os = "macos")]
    {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let r =
            unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_NWRITE, &mut v as *mut libc::c_int as *mut libc::c_void, &mut len) };
        (r == 0).then_some(v)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = fd;
        None
    }
}

/// Text from the phone without control characters (but newlines and tabs):
/// it is typed into terminals, where an escape sequence would be obeyed.
pub fn clean_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t')).collect()
}

/// `clean_text` on every string in a JSON value.
fn clean_json(v: &mut Value) {
    match v {
        Value::String(s) => *s = clean_text(s),
        Value::Array(a) => a.iter_mut().for_each(clean_json),
        Value::Object(o) => o.values_mut().for_each(clean_json),
        _ => {}
    }
}

/// The largest body a call may send: tiny before a device is known, a
/// recording's worth for a WAV, the most for multipart files, else JSON.
fn body_limit(req: &Request, seg: &[&str]) -> usize {
    let multipart = req.header("content-type").and_then(multipart::boundary).is_some();
    match (req.method.as_str(), seg.get(1..).unwrap_or(&[])) {
        ("POST", ["pair"]) => http::MAX_PAIR_BODY,
        ("POST", ["voice"] | ["transcribe"] | ["questions" | "decisions", _, "voice"]) if !multipart => http::MAX_VOICE_BODY,
        ("POST", ["ask"] | ["voice"] | ["transcribe"] | ["questions" | "decisions", _, "voice"] | ["questions" | "decisions", _, "answer"])
            if multipart =>
        {
            http::MAX_BODY
        }
        _ => http::MAX_JSON_BODY,
    }
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
#[cfg(test)]
pub fn choose_ip(cfg: &crate::config::Lan) -> Result<IpAddr> {
    choose_ip_from(cfg, if cfg.bind.is_empty() || cfg.bind == "auto" { lan_ip() } else { None })
}

/// `choose_ip` with this Mac's default-route address given (`auto`).
pub fn choose_ip_from(cfg: &crate::config::Lan, auto: Option<IpAddr>) -> Result<IpAddr> {
    let ip = if cfg.bind.is_empty() || cfg.bind == "auto" {
        auto.context("this Mac has no network address (is Wi-Fi on?)")?
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

/// Whether `[lan] tailscale` asks for the tailnet address: `auto` (or empty, or `on`) yes, `off` no.
pub fn tailscale_wanted(cfg: &crate::config::Lan) -> Result<bool> {
    match cfg.tailscale.trim() {
        "" | "auto" | "on" => Ok(true),
        "off" => Ok(false),
        other => bail!("[lan] tailscale = {other:?}: use \"auto\" or \"off\""),
    }
}

/// The tailnet address to listen on: Tailscale runs, it's wanted, and the address is in 100.64/10.
pub fn tailscale_ip(cfg: &crate::config::Lan, ts: &tailscale::Tailscale) -> Option<IpAddr> {
    let ip = ts.ipv4.filter(|ip| ts.running && (tailscale::is_tailnet(*ip) || loopback_tailnet(*ip)))?;
    tailscale_wanted(cfg).unwrap_or(false).then_some(IpAddr::V4(ip))
}

/// Tests may let 127.0.0.1 stand for the tailnet (no 100.x address here); never otherwise.
#[cfg(test)]
fn loopback_tailnet(ip: std::net::Ipv4Addr) -> bool {
    ip.is_loopback() && tests::LOOPBACK_TAILNET.with(|c| c.get())
}

#[cfg(not(test))]
fn loopback_tailnet(_: std::net::Ipv4Addr) -> bool {
    false
}

/// Where to listen: the LAN address as `[lan]` says (`lan`: the default
/// route's, for `auto`), then the tailnet's while Tailscale runs. Never an
/// unspecified (every-interface) address, never a public one without
/// `allow_public`. One of them is enough: no LAN (Wi-Fi off) with Tailscale
/// up still listens on the tailnet. A default route over the tailnet (an
/// exit node) is one address, the tailnet's.
pub fn plan(cfg: &crate::config::Lan, lan: Option<IpAddr>, ts: &tailscale::Tailscale) -> Result<Vec<(Kind, IpAddr)>> {
    tailscale_wanted(cfg)?;
    let auto = cfg.bind.is_empty() || cfg.bind == "auto";
    let lan = match choose_ip_from(cfg, lan) {
        // No network address for `auto` (Wi-Fi off): the tailnet may stand in.
        Err(e) if auto && lan.is_none() => Err(e),
        // A `bind` that isn't allowed is an error, Tailscale or not.
        Err(e) => return Err(e),
        Ok(ip) => Ok(ip),
    };
    let tail = tailscale_ip(cfg, ts);
    let out = match (lan, tail) {
        (Ok(l), Some(t)) if l == t => vec![(Kind::Tailscale, t)],
        (Ok(l), Some(t)) => vec![(Kind::Lan, l), (Kind::Tailscale, t)],
        (Ok(l), None) => vec![(Kind::Lan, l)],
        (Err(_), Some(t)) => vec![(Kind::Tailscale, t)],
        (Err(e), None) => return Err(e),
    };
    for (_, ip) in &out {
        if ip.is_unspecified() && !cfg.allow_public {
            bail!("{ip} would listen on every network");
        }
    }
    Ok(out)
}

/// The addresses as the pairing payload, `info` and `state.json` list them.
fn hosts_value(listeners: &[(Kind, SocketAddr)], ts: &tailscale::Tailscale) -> Vec<Value> {
    listeners
        .iter()
        .map(|(kind, a)| {
            let mut h = json!({ "kind": kind.word(), "host": a.ip().to_string(), "port": a.port() });
            if *kind == Kind::Tailscale
                && let Some(n) = &ts.dns_name
            {
                h["dns_name"] = json!(n);
            }
            h
        })
        .collect()
}

/// What the Mac shows to pair a phone, and the link its QR carries: the
/// first address as `host`/`port` (what older apps read), every address in
/// order as `hosts`, and the MagicDNS name while the tailnet is listened on.
pub fn pairing_payload(code: &str, listeners: &[(Kind, SocketAddr)], fingerprint: &str, ts: &tailscale::Tailscale, expires_in_s: u64) -> Value {
    let hosts = hosts_value(listeners, ts);
    let (host, port) = listeners.first().map(|(_, a)| (a.ip().to_string(), a.port())).unwrap_or_default();
    let on_tailnet = listeners.iter().any(|(k, _)| *k == Kind::Tailscale);
    let magic_dns = ts.dns_name.clone().filter(|_| on_tailnet);
    let mut link = format!("herdertower://pair?h={host}&p={port}&c={code}&fp={fingerprint}");
    if listeners.len() > 1 || on_tailnet {
        let list: Vec<String> = listeners.iter().map(|(k, a)| format!("{}:{}:{}", k.word(), a.ip(), a.port())).collect();
        link += &format!("&hosts={}", list.join(","));
    }
    if let Some(n) = &magic_dns {
        link += &format!("&dns={n}");
    }
    json!({
        "code": code,
        "host": host,
        "port": port,
        "hosts": hosts,
        "magic_dns": magic_dns,
        "fingerprint": fingerprint,
        "link": link,
        "expires_in_s": expires_in_s,
        "tailscale": ts.view(),
    })
}

impl Lan {
    /// Who listens over the network (for events other parts of the daemon send).
    pub fn hub(&self) -> Arc<Hub> {
        self.ctx.hub.clone()
    }

    pub fn new(ctx: Ctx) -> Lan {
        Self::with_paths(ctx, auth::Devices::path(), Identity::dir())
    }

    pub fn with_paths(ctx: Ctx, devices_path: std::path::PathBuf, identity_dir: std::path::PathBuf) -> Lan {
        Lan {
            devices: Arc::new(auth::Store::new(devices_path)),
            identity_dir,
            tower: Arc::default(),
            history: crate::history::path(),
            actions: tower::Actions::new(ctx.hub.clone()),
            ctx,
            running: Arc::new(Mutex::new(None)),
            pairing: Arc::new(Mutex::new(Pairing::default())),
            failures: Arc::new(Mutex::new(HashMap::new())),
            conns: Arc::new(Mutex::new(Conns::default())),
            // Tests never ask this Mac's Tailscale unless they say so (`with_detect`).
            detect: Arc::new(if cfg!(test) { tailscale::Tailscale::default } else { tailscale::detect as fn() -> tailscale::Tailscale }),
            lan_detect: Arc::new(lan_ip),
        }
    }

    /// Another way to ask this Mac's address on the network (tests: a network change).
    #[cfg(test)]
    pub fn with_lan_detect(mut self, detect: impl Fn() -> Option<IpAddr> + Send + Sync + 'static) -> Lan {
        self.lan_detect = Arc::new(detect);
        self
    }

    /// Another way to ask about Tailscale (tests).
    #[cfg(test)]
    pub fn with_detect(mut self, detect: impl Fn() -> tailscale::Tailscale + Send + Sync + 'static) -> Lan {
        self.detect = Arc::new(detect);
        self
    }

    /// Another history file (tests).
    #[cfg(test)]
    pub fn with_history(mut self, path: std::path::PathBuf) -> Lan {
        self.history = path;
        self
    }

    /// The rest of the tower, for what a phone does as the Mac's windows
    /// do: given once, when the daemon is whole (it holds this API itself).
    pub fn attach(&self, tower: Arc<dyn tower::Tower>) {
        let _ = self.tower.set(tower);
    }

    fn tower(&self) -> Option<Arc<dyn tower::Tower>> {
        self.tower.get().cloned()
    }

    /// Every history entry, oldest first.
    fn entries(&self) -> Vec<crate::history::Entry> {
        crate::history::load_from(&self.history)
    }

    /// The history with each entry's line number (a chat message's id).
    fn lines(&self) -> Vec<(usize, crate::history::Entry)> {
        std::fs::read_to_string(&self.history).map(|t| crate::chat::parse(&t)).unwrap_or_default()
    }

    /// The chat thread as it is now, oldest first.
    fn chat(&self) -> Vec<Value> {
        let spaces = self.tower().map(|t| t.remembered()).unwrap_or_default();
        let acts = self.ctx.shared.with(|s| s.acts.clone());
        crate::chat::thread_of(&self.lines(), &crate::history::attachments_dir(), &self.ctx.prs.map(), &spaces, &acts)
    }

    /// Starts listening: on the LAN address, and the tailnet's while Tailscale
    /// runs (re-checked every 20 s). Returns the first address.
    pub fn start(&self, cfg: &crate::config::Lan) -> Result<SocketAddr> {
        // One lock over the check and the start: two at once can't both bind.
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = running.as_ref() {
            return Ok(r.primary());
        }
        let identity = Identity::load_or_create(&self.identity_dir)?;
        let tls = identity.server_config()?;
        let ts = if tailscale_wanted(cfg)? { (self.detect)() } else { tailscale::Tailscale::default() };
        let auto_bind = cfg.bind.is_empty() || cfg.bind == "auto";
        let auto = if auto_bind { (self.lan_detect)() } else { None };
        let stop = Arc::new(AtomicBool::new(false));
        let mut listeners = vec![];
        let mut tailscale_error = None;
        for (kind, ip) in plan(cfg, auto, &ts)? {
            // The same port everywhere (the first one's, when it was 0).
            let port = listeners.first().map(|l: &Listener| l.addr.port()).unwrap_or(cfg.port);
            match self.listen(kind, ip, port, &tls, &stop) {
                Ok(l) => listeners.push(l),
                // The tailnet's address can't be had (Tailscale just stopped, say): the LAN goes on.
                Err(e) if kind == Kind::Tailscale && !listeners.is_empty() => {
                    eprintln!("[phone] not on the tailnet: {e:#}");
                    tailscale_error = Some(format!("{e:#}"));
                }
                Err(e) => {
                    for l in listeners.drain(..) {
                        Self::unlisten(l);
                    }
                    return Err(e);
                }
            }
        }
        let fingerprint = identity.fingerprint();
        *running = Some(Running { listeners, stop: stop.clone(), fingerprint: fingerprint.clone(), tls, cfg: cfg.clone(), tailscale: ts, tailscale_error });
        let r = running.as_ref().expect("just set");
        let addrs: Vec<String> = r.listeners.iter().map(|l| format!("https://{} ({})", l.addr, l.kind.word())).collect();
        let primary = r.primary();
        self.publish(r);
        drop(running);
        eprintln!("[phone] API on {} (certificate {})", addrs.join(", "), tls::grouped(&fingerprint[..16]));
        // Every 20 s: the network this Mac is on (home, the office: the LAN
        // listener follows it) and Tailscale (the tailnet's follows that).
        let tailnet = tailscale_wanted(cfg).unwrap_or(false);
        if tailnet || auto_bind {
            let me = self.clone();
            thread::Builder::new()
                .name("lan-follow".into())
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let until = Instant::now() + TAILSCALE_RECHECK;
                        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(250));
                        }
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        if auto_bind {
                            me.recheck_lan(&stop);
                        }
                        if tailnet {
                            me.recheck_tailscale(&stop);
                        }
                    }
                })
                .ok();
        }
        Ok(primary)
    }

    /// A socket on `ip:port` and its accept loop.
    fn listen(&self, kind: Kind, ip: IpAddr, port: u16, tls: &Arc<rustls::ServerConfig>, api_stop: &Arc<AtomicBool>) -> Result<Listener> {
        // Never every interface: `plan` refuses it, and so does this.
        if ip.is_unspecified() {
            bail!("refusing to listen on {ip}");
        }
        let listener = TcpListener::bind((ip, port)).with_context(|| format!("listening on {ip}:{port} ({})", kind.word()))?;
        let addr = listener.local_addr()?;
        // Polled (see `ACCEPT_POLL`), so stopping never depends on reaching the address.
        listener.set_nonblocking(true).with_context(|| format!("listening on {addr} ({})", kind.word()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let (me, tls, flag, all) = (self.clone(), tls.clone(), stop.clone(), api_stop.clone());
        let thread = thread::Builder::new().name(format!("lan-{}", kind.word())).spawn(move || {
            // The socket is dropped (closed) when this loop ends.
            let listener = listener;
            while !flag.load(Ordering::SeqCst) && !all.load(Ordering::SeqCst) {
                let conn = match listener.accept() {
                    Ok((conn, _)) => conn,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL);
                        continue;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // The address went away under it, or too many files: look again shortly.
                    Err(_) => {
                        thread::sleep(ACCEPT_POLL);
                        continue;
                    }
                };
                // On macOS an accepted socket inherits the listener's non-blocking mode.
                if conn.set_nonblocking(false).is_err() {
                    continue;
                }
                let ip = conn.peer_addr().map(|a| a.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
                // Too many open: closed unanswered.
                let Some(slot) = Slot::take(&me.conns, ip) else { continue };
                let (me, tls, all) = (me.clone(), tls.clone(), all.clone());
                let _ = thread::Builder::new().name("lan-conn".into()).spawn(move || {
                    let _slot = slot;
                    let run = std::panic::AssertUnwindSafe(|| me.connection(conn, tls, &all));
                    if std::panic::catch_unwind(run).is_err() {
                        eprintln!("[phone] a call from {ip} failed inside the tower; its connection is closed");
                    }
                });
            }
        })?;
        Ok(Listener { kind, addr, stop, thread: Some(thread) })
    }

    /// Ends a listener's accept loop and closes its socket before returning,
    /// so its address can be listened on again at once (its open connections
    /// go on). Never needs the address to be reachable.
    fn unlisten(mut l: Listener) {
        l.stop.store(true, Ordering::SeqCst);
        if let Some(t) = l.thread.take()
            && t.thread().id() != thread::current().id()
        {
            let _ = t.join();
        }
    }

    /// Where it listens, into the state (the panel, `state.json`, `GET /v1/state`).
    fn publish(&self, r: &Running) {
        let url = format!("https://{}", r.primary());
        let hosts = hosts_value(&r.listeners.iter().map(|l| (l.kind, l.addr)).collect::<Vec<_>>(), &r.tailscale);
        let ts = r.tailscale.view();
        self.ctx.shared.update(|s| {
            s.lan = url;
            s.lan_hosts = hosts;
            s.tailscale = Some(ts);
        });
    }

    /// Tailscale again: listens on the tailnet once it runs, stops when it
    /// stops or its address changes. The LAN listener is never touched.
    /// The network this Mac is on now, for `bind = "auto"`: when its address
    /// changed (another network), the LAN listener moves to the new one on
    /// the same port, and the pairing and the state show it. With no network
    /// (Wi-Fi off), the old one stays unless the tailnet's is there instead.
    fn recheck_lan(&self, api_stop: &Arc<AtomicBool>) {
        let now = (self.lan_detect)();
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = running.as_mut() else { return };
        // Another start since (this watcher's API was stopped), or an address set by hand.
        if !Arc::ptr_eq(&r.stop, api_stop) || !(r.cfg.bind.is_empty() || r.cfg.bind == "auto") {
            return;
        }
        let tail = r.of(Kind::Tailscale).map(|l| l.addr.ip());
        // As `plan` takes it: a private address (unless allow_public), and not the tailnet's own.
        let want = choose_ip_from(&r.cfg, now).ok().filter(|ip| Some(*ip) != tail);
        let have = r.of(Kind::Lan).map(|l| l.addr.ip());
        if want == have || r.listeners.is_empty() {
            return;
        }
        let port = r.primary().port();
        if let Some(pos) = r.listeners.iter().position(|l| l.kind == Kind::Lan)
            && (r.listeners.len() > 1 || want.is_some())
        {
            let l = r.listeners.remove(pos);
            eprintln!("[phone] no longer on {}: this Mac is on another network", l.addr);
            Self::unlisten(l);
        }
        if let Some(ip) = want {
            match self.listen(Kind::Lan, ip, port, &r.tls, &r.stop) {
                Ok(l) => {
                    eprintln!("[phone] API on https://{} (lan): this Mac's network changed", l.addr);
                    r.listeners.insert(0, l);
                }
                Err(e) => eprintln!("[phone] can't listen on the new network yet: {e:#}"),
            }
        }
        self.publish(r);
    }

    fn recheck_tailscale(&self, api_stop: &Arc<AtomicBool>) {
        let ts = (self.detect)();
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = running.as_mut() else { return };
        // Another start since (this watcher's API was stopped).
        if !Arc::ptr_eq(&r.stop, api_stop) {
            return;
        }
        let want = tailscale_ip(&r.cfg, &ts).filter(|ip| r.of(Kind::Lan).is_none_or(|l| l.addr.ip() != *ip));
        let have = r.of(Kind::Tailscale).map(|l| l.addr.ip());
        let mut changed = r.tailscale != ts;
        r.tailscale = ts;
        // Nothing to listen on: no error to keep (Tailscale stopped since a bind failed).
        if want.is_none() && r.tailscale_error.take().is_some() {
            changed = true;
        }
        if want == have {
            if changed {
                self.publish(r);
            }
            return;
        }
        // The old tailnet listener goes; the only one there is (no LAN) stays until another replaces it.
        if let Some(pos) = r.listeners.iter().position(|l| l.kind == Kind::Tailscale)
            && (r.listeners.len() > 1 || want.is_some())
        {
            let l = r.listeners.remove(pos);
            eprintln!("[phone] no longer on the tailnet ({}): Tailscale is {}", l.addr, r.tailscale.word());
            Self::unlisten(l);
        }
        r.tailscale_error = None;
        if let Some(ip) = want {
            let port = r.primary().port();
            match self.listen(Kind::Tailscale, ip, port, &r.tls, &r.stop) {
                Ok(l) => {
                    eprintln!("[phone] API on https://{} (tailscale)", l.addr);
                    r.listeners.push(l);
                }
                Err(e) => {
                    eprintln!("[phone] not on the tailnet: {e:#}");
                    r.tailscale_error = Some(format!("{e:#}"));
                }
            }
        }
        self.publish(r);
    }

    pub fn stop(&self) {
        if let Some(r) = self.running.lock().unwrap_or_else(|e| e.into_inner()).take() {
            r.stop.store(true, Ordering::SeqCst);
            for l in r.listeners {
                Self::unlisten(l);
            }
            // Open event streams end too: no phone stays the voice.
            self.ctx.hub.drop_all();
            self.pairing.lock().unwrap_or_else(|e| e.into_inner()).close();
            self.ctx.shared.update(|s| {
                s.lan.clear();
                s.lan_hosts.clear();
                s.phone = None;
            });
            eprintln!("[phone] API off");
        }
    }

    /// Where it listens and who is connected, and Tailscale (asked now when the API is off).
    pub fn info(&self) -> Value {
        let r = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let (streams, voice) = self.ctx.hub.counts();
        let devices = self.devices.load();
        let listeners: Vec<(Kind, SocketAddr)> = r.as_ref().map(|r| r.listeners.iter().map(|l| (l.kind, l.addr)).collect()).unwrap_or_default();
        let ts = match r.as_ref() {
            Some(r) => r.tailscale.clone(),
            None => {
                let wanted = crate::config::Config::load().map(|c| tailscale_wanted(&c.lan).unwrap_or(false)).unwrap_or(true);
                if wanted { (self.detect)() } else { tailscale::Tailscale::default() }
            }
        };
        let mode = r.as_ref().map(|r| r.cfg.tailscale.clone()).or_else(|| crate::config::Config::load().ok().map(|c| c.lan.tailscale)).unwrap_or_default();
        let mut tailscale = ts.view();
        tailscale["mode"] = json!(if mode.trim() == "off" { "off" } else { "auto" });
        tailscale["listening"] = json!(listeners.iter().any(|(k, _)| *k == Kind::Tailscale));
        tailscale["error"] = json!(r.as_ref().and_then(|r| r.tailscale_error.clone()));
        json!({
            "on": r.is_some(),
            "url": r.as_ref().map(|r| format!("https://{}", r.primary())),
            "hosts": hosts_value(&listeners, &ts),
            "tailscale": tailscale,
            "fingerprint": r.as_ref().map(|r| r.fingerprint.clone()),
            "devices": devices.devices.iter().map(|d| json!({ "id": d.id, "name": d.name, "created_ms": d.created_ms, "last_seen_ms": d.last_seen_ms })).collect::<Vec<_>>(),
            "streams": streams,
            "voice_streams": voice,
            "pairing_expires_in_s": self.pairing.lock().unwrap_or_else(|e| e.into_inner()).expires_in().map(|d| d.as_secs()),
        })
    }

    /// Opens a pairing code: what the Mac shows (and the QR carries).
    pub fn pair(&self) -> Result<Value> {
        let r = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = r.as_ref() else { bail!("the phone API is off: tower phone enable") };
        let code = self.pairing.lock().unwrap_or_else(|e| e.into_inner()).open();
        let listeners: Vec<(Kind, SocketAddr)> = r.listeners.iter().map(|l| (l.kind, l.addr)).collect();
        eprintln!("[phone] pairing open for {} s", auth::CODE_TTL.as_secs());
        Ok(pairing_payload(&code, &listeners, &r.fingerprint, &r.tailscale, auth::CODE_TTL.as_secs()))
    }

    /// A device was revoked (`tower phone revoke`): its open streams end.
    pub fn revoked(&self, device: &str) {
        self.ctx.hub.drop_device(device);
    }

    fn connection(&self, tcp: TcpStream, tls: Arc<rustls::ServerConfig>, stop: &AtomicBool) {
        let peer = tcp.peer_addr().map(|a| a.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
        keepalive(&tcp);
        let _ = tcp.set_write_timeout(Some(Duration::from_secs(10)));
        let _ = tcp.set_nodelay(true);
        let fd = tcp.as_raw_fd();
        let Ok(conn) = rustls::ServerConnection::new(tls) else { return };
        let mut stream = rustls::StreamOwned::new(conn, Sock { tcp, deadline: Some(Instant::now() + HEAD_DEADLINE) });
        let (mut req, rest) = match http::read_head(&mut stream) {
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
        let resp = self.handle(&mut req, rest, peer, &mut stream, fd, stop);
        if let Some(resp) = resp {
            let _ = http::write_response(&mut stream, &resp);
        }
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }

    /// Who is calling first, from the head alone; then the body, as much as that call may send; then the call.
    fn handle(
        &self,
        req: &mut Request,
        rest: Vec<u8>,
        peer: IpAddr,
        stream: &mut Stream,
        fd: RawFd,
        stop: &AtomicBool,
    ) -> Option<Response> {
        let seg = req.segments();
        let seg: Vec<&str> = seg.iter().map(String::as_str).collect();
        if seg.first() != Some(&"v1") {
            return Some(Response::error(404, "not_found", "the API is under /v1"));
        }
        let pairing = (req.method.as_str(), &seg[1..]) == ("POST", &["pair"][..]);
        // Everything but pairing: a paired device only.
        let device = if pairing {
            None
        } else {
            match auth::authorize(&self.devices.load(), req.header("authorization")) {
                Ok(d) => {
                    // Known: it may take its time from here (an upload, the event stream).
                    stream.sock.no_deadline();
                    Some(d)
                }
                Err(e) => {
                    self.failed(peer);
                    thread::sleep(Duration::from_millis(300));
                    return Some(Response::error(401, "unauthorized", &e.to_string()));
                }
            }
        };
        let max = body_limit(req, &seg);
        if req.content_length > max {
            return Some(Response::error(413, "too_large", &format!("this call takes a body of {} KiB at most", max / 1024)));
        }
        if let Err(e) = http::read_body(stream, req, rest, max) {
            return Some(Response::error(400, "bad_request", &format!("{e:#}")));
        }
        match device {
            None => Some(self.pair_device(req, peer)),
            Some(device) => {
                self.seen(&device);
                self.route(req, &device, stream, Some(fd), stop)
            }
        }
    }

    fn refused(&self, ip: IpAddr) -> bool {
        let f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        f.get(&ip).is_some_and(|(n, since)| *n >= MAX_FAILURES && since.elapsed() < Duration::from_secs(60))
    }

    fn failed(&self, ip: IpAddr) {
        let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        // A minute on, an address starts again (and the map doesn't grow without end).
        f.retain(|_, (_, since)| since.elapsed() <= Duration::from_secs(60));
        f.entry(ip).or_insert((0, Instant::now())).0 += 1;
    }

    /// A paired device's call: the answer, or None when the handler wrote
    /// its own (the event stream). `fd`: the socket, to notice a phone that vanished.
    fn route(
        &self,
        req: &Request,
        device: &auth::Device,
        stream: &mut impl Write,
        fd: Option<RawFd>,
        stop: &AtomicBool,
    ) -> Option<Response> {
        let seg = req.segments();
        let seg: Vec<&str> = seg.iter().map(String::as_str).collect();
        let m = req.method.as_str();
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
                    "features": FEATURES,
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
                let changes: Vec<Value> =
                    crate::behaviors::Store::open().history().iter().rev().take(limit(50)).map(|c| c.view()).collect();
                Response::json(200, &json!({ "changes": changes }))
            }
            ("GET", ["behaviors", id]) => match crate::behaviors::Store::open().get(id) {
                Some(b) => Response::json(200, &serde_json::to_value(b).unwrap_or_default()),
                None => Response::error(404, "no_behavior", "no behavior with that id"),
            },
            ("POST", ["behaviors"]) => {
                let v = req.json().unwrap_or_default();
                let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
                behavior_change(crate::behaviors::Op::Add {
                    title: s("title"),
                    category: s("category"),
                    summary: s("summary"),
                    text: s("text"),
                })
            }
            ("POST", ["behaviors", "undo"]) => behavior_change(crate::behaviors::Op::Undo),
            // What an edit would change, without saving it.
            ("POST", ["behaviors", id, "preview"]) => {
                let Some(b) = crate::behaviors::Store::open().get(id) else {
                    return Some(Response::error(404, "no_behavior", "no behavior with that id"));
                };
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
                let Some(set) = self.ctx.asks.get(id) else {
                    return Some(Response::error(404, "no_question", "no open question set with that id"));
                };
                let body = req.json().unwrap_or_default();
                let tab = body["tab"].as_u64().unwrap_or(0) as usize;
                let Some(t) = set.tabs.get(tab) else { return Some(Response::error(400, "bad_tab", "no such tab")) };
                let p = crate::questions::interpret(body["text"].as_str().unwrap_or(""), t);
                Response::json(200, &json!({ "tab": tab, "choice": { "options": p.options, "other": p.other } }))
            }
            ("POST", ["questions", id, "voice"]) => {
                let Some(set) = self.ctx.asks.get(id) else {
                    return Some(Response::error(404, "no_question", "no open question set with that id"));
                };
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
            // With `space`: the conversation with that space's own tower; with `main=1`:
            // the main one without those; neither: every message, as before.
            ("GET", ["chat"]) => {
                let mut all = self.chat();
                match (q("space").map(str::trim).filter(|s| !s.is_empty()), q("main") == Some("1")) {
                    (Some(space), _) => all.retain(|m| crate::chat::in_conversation(m, Some(space))),
                    (None, true) => all.retain(|m| crate::chat::in_conversation(m, None)),
                    (None, false) => {}
                }
                let (messages, more) = crate::chat::page(&all, q("before"), limit(50).min(200));
                Response::json(200, &json!({ "messages": messages, "more": more }))
            }
            // The PRs sessions opened (see docs/mobile-api.md, "PRs"): open ones and the last day's closed.
            ("GET", ["prs"]) => {
                let day_ago = crate::history::now_ms().saturating_sub(24 * 3_600_000);
                let mut list: Vec<crate::prs::Pr> =
                    self.ctx.prs.list().into_iter().filter(|p| p.open() || p.closed_ms.is_some_and(|c| c >= day_ago)).collect();
                list.sort_by_key(|p| (!p.open(), !p.waiting, std::cmp::Reverse(p.found_ms)));
                Response::json(200, &json!({ "prs": list.iter().map(crate::prs::Pr::view).collect::<Vec<_>>() }))
            }
            ("GET", ["prs", owner, repo, n]) => match self.ctx.prs.get(&format!("{owner}/{repo}#{n}")) {
                Some(p) => Response::json(200, &json!({ "pr": p.view() })),
                None => Response::error(404, "no_pr", "no PR the tower knows by that name"),
            },
            // Merging is the user's, in two steps: what it would do, then do it with the token.
            ("POST", ["prs", owner, repo, n, "confirm"]) => {
                let key = format!("{owner}/{repo}#{n}");
                if self.ctx.prs.get(&key).is_none() {
                    return Some(Response::error(404, "no_pr", "no PR the tower knows by that name"));
                }
                match self.ctx.prs.confirm(&key, &device.id) {
                    Ok((token, pr, gate, method)) => Response::json(
                        200,
                        &json!({
                            "token": token, "expires_ms": crate::history::now_ms() + 2 * 60_000, "pr": pr.view(), "method": method,
                            "can": gate.blocked.is_none(), "blocked": gate.blocked, "needs_override": gate.needs_override, "warnings": gate.warnings,
                        }),
                    ),
                    Err(e) => Response::error(503, "github", &format!("GitHub couldn't be asked: {e:#}")),
                }
            }
            ("POST", ["prs", owner, repo, n, "merge"]) => {
                let key = format!("{owner}/{repo}#{n}");
                let body = req.json().unwrap_or_default();
                let token = body["token"].as_str().unwrap_or("");
                let Some(sha) = self.ctx.prs.take_token(token, &key, &device.id) else {
                    return Some(Response::error(403, "bad_token", "confirm first (POST …/confirm): the token is missing, used, for another PR or device, or older than two minutes"));
                };
                let Some(number) = n.parse::<u64>().ok() else { return Some(Response::error(404, "no_pr", "bad PR number")) };
                let repo_name = format!("{owner}/{repo}");
                let override_failing = body["override"].as_bool().unwrap_or(false);
                match crate::prs::merge(self.ctx.prs.gh(), &repo_name, number, &sha, override_failing) {
                    Ok(_) => {
                        eprintln!("[phone] {} merged {key}", device.name);
                        let pr = self.ctx.prs.report_merged(&key, &format!("phone:{}", device.id)).or_else(|| self.ctx.prs.get(&key));
                        Response::json(200, &json!({ "merged": true, "pr": pr.map(|p| p.view()) }))
                    }
                    Err(r) => Response::error(409, r.code(), r.message()),
                }
            }
            // Clips this phone may have missed (no stream open): the last 15 minutes; none while muted.
            ("GET", ["audio"]) => {
                let clips = if self.ctx.mouth.muted() { vec![] } else { self.ctx.hub.clips_for(&device.id, q("after")) };
                Response::json(200, &json!({ "clips": clips }))
            }
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
                let rows: Vec<Value> = crate::history::jobs(&self.entries())
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
                // A space by its whole name, any case (the Record of one space).
                let space = q("space").map(str::trim).filter(|s| !s.is_empty());
                let before = q("before_ms").and_then(|b| b.parse::<u64>().ok()).unwrap_or(u64::MAX);
                let mut list: Vec<_> = self
                    .entries()
                    .into_iter()
                    .rev()
                    .filter(|e| e.at < before)
                    .filter(|e| session.is_none_or(|s| e.session.as_deref() == Some(s)))
                    .filter(|e| kind.is_none_or(|k| k.split(',').any(|k| k == e.kind)))
                    .filter(|e| space.is_none_or(|s| e.space.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(s))))
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
                let entries = self.entries();
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
            // Only a clip meant for this phone (or the voice's), and none while muted.
            ("GET", ["audio", id]) => match self.ctx.hub.clip(id, &device.id).filter(|_| !self.ctx.mouth.muted()) {
                Some(wav) => Response::bytes("audio/wav", wav),
                None => Response::error(404, "no_clip", "that clip is gone (they are kept for 15 minutes)"),
            },
            ("GET", ["events"]) => {
                self.events(&device, q("voice") == Some("1"), stream, fd, stop);
                return None;
            }
            ("POST", ["ask"]) => {
                let (text, space, files) = match ask_body(req) {
                    Ok(x) => x,
                    Err(r) => return Some(r),
                };
                if text.is_empty() && files.is_empty() {
                    return Some(Response::error(400, "empty", "send {\"text\": \"...\"}, or multipart text and files"));
                }
                // Asked in a space's view: a space the tower knows, by its whole name (before any file is saved).
                let space = match space.map(|want| self.space_named(&want)) {
                    Some(Ok(name)) => Some(name),
                    Some(Err(r)) => return Some(r),
                    None => None,
                };
                let saved = match save_files(&files, &crate::history::attachments_dir(), now_ms()) {
                    Ok(p) => p,
                    Err(r) => return Some(r),
                };
                let paths: Vec<String> = saved.iter().map(|f| f.path.clone()).collect();
                let markers: Vec<String> = saved.iter().map(|f| f.marker.clone()).collect();
                let text = with_markers(&text, &markers);
                let mut resp = self.ask(&device, &crate::history::compose(&text, &paths), space.as_deref());
                if resp.status == 202 {
                    let mut sent = json!({ "sent": text, "attachments": paths });
                    if let Some(space) = &space {
                        sent["space"] = json!(space);
                    }
                    resp = Response::json(202, &sent);
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
                let note = body["note"]
                    .as_str()
                    .map(clean_text)
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty())
                    .map(|n| format!(" {n}"))
                    .unwrap_or_default();
                self.ask(&device, &format!("{verb} what {address} is asking for.{note}"), None)
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
            ("GET", ["mute"]) => Response::json(200, &json!({ "muted": self.ctx.mouth.muted() })),
            // What waits in the send queue (`tower queue list`).
            ("GET", ["queue"]) => Response::json(200, &json!({ "items": crate::daemon::sendq::current_items() })),
            // The tower-wide mute (`tower mute`): JSON `{"muted": true|false}`.
            ("POST", ["mute"]) | ("PUT", ["mute"]) => {
                let body = req.json().unwrap_or_default();
                let Some(on) = body["muted"].as_bool() else {
                    return Some(Response::error(400, "bad_mute", "send {\"muted\": true} or {\"muted\": false}"));
                };
                match self.ctx.mouth.set_muted(on) {
                    Ok(()) => {
                        // What was said before the mute isn't fetched after it.
                        if on {
                            self.ctx.hub.drop_clips();
                        }
                        // The mute is the voice setting `off`: config.toml says what the mouth does.
                        // (Tests never write the user's config.)
                        if !cfg!(test)
                            && let Err(e) = crate::tell::set_voice(self.ctx.mouth.voice())
                        {
                            eprintln!("[phone] keeping the voice setting in config.toml: {e:#}");
                        }
                        Response::json(200, &json!({ "muted": on }))
                    }
                    Err(e) => Response::error(503, "save", &format!("{e:#}")),
                }
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
                if let Err(e) = self.devices.update(|d| d.revoke(&device.id)) {
                    return Some(Response::error(503, "save", &format!("{e:#}")));
                }
                self.ctx.hub.drop_device(&device.id);
                Response::json(200, &json!({ "revoked": device.id }))
            }
            // The queue, the missions, their threads, launching, the tell level, the model.
            _ => match self.quiet(req, device, &seg[1..]) {
                Some(resp) => resp,
                None => Response::error(404, "not_found", &format!("no {m} {}", req.path)),
            },
        };
        Some(resp)
    }

    fn pair_device(&self, req: &Request, peer: IpAddr) -> Response {
        let body = req.json().unwrap_or_default();
        let code = body["code"].as_str().unwrap_or("");
        let taken = self.pairing.lock().unwrap_or_else(|e| e.into_inner()).take(code, peer);
        match taken {
            Ok(()) => {
                let (device, token) = match self.devices.update(|d| d.add(body["name"].as_str().unwrap_or("phone"), now_ms())) {
                    Ok(x) => x,
                    Err(e) => return Response::error(503, "save", &format!("{e:#}")),
                };
                eprintln!("[phone] paired {} ({}) from {peer}", device.name, device.id);
                let fp = self.running.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.fingerprint.clone());
                Response::json(
                    200,
                    &json!({ "token": token, "device": { "id": device.id, "name": device.name }, "fingerprint": fp, "api": API, "features": FEATURES }),
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
        // Under the lock, on the file as it is now: a device revoked meanwhile stays revoked.
        let _ = self.devices.update(|d| {
            if let Some(x) = d.devices.iter_mut().find(|x| x.id == device.id) {
                x.last_seen_ms = now;
            }
        });
    }

    /// Hands what the phone's user asked to the tower (`space`: asked in
    /// that space's view, so its own tower gets it when it has one).
    fn ask(&self, device: &auth::Device, text: &str, space: Option<&str>) -> Response {
        let Some(tower) = self.tower() else { return starting() };
        let source = format!("phone:{}", device.id);
        match tower.ask(text, heard_on_phone(device), &source, space) {
            Ok(()) => Response::json(202, &json!({ "sent": text })),
            Err(e) => Response::error(503, "brain", &crate::brain::spoken_error(&format!("{e:#}"))),
        }
    }

    /// The space a call names, as the tower knows it: its whole name, in
    /// any case. A name no space has is a 404.
    fn space_named(&self, want: &str) -> Result<String, Response> {
        let Some(tower) = self.tower() else { return Err(starting()) };
        let want = want.trim();
        tower
            .spaces()
            .into_iter()
            .find(|s| !want.is_empty() && s.trim().eq_ignore_ascii_case(want))
            .ok_or_else(|| Response::error(404, "no_space", &format!("no space called {want:?}")))
    }

    /// The state with what the brain is doing now (`work`, the panel's working line).
    fn state(&self, device: &auth::Device) -> Value {
        let mut s = self.ctx.shared.json();
        // The big lists are too much to send with every change, and each has
        // its own call and event: the decisions (`decisions_count` stays),
        // the missions, the spaces' threads.
        if let Some(o) = s.as_object_mut() {
            o.remove("threads");
            o.remove("missions");
            o.remove("decisions");
            // What the tower opened lately: each is a chat message and an `act` event.
            o.remove("acts");
        }
        s["work"] = work_value();
        s["audio_route"] = json!(device.audio_route);
        s
    }

    /// `POST /v1/questions/{id}/answer`: JSON `{answers, submit}`, or
    /// multipart with that JSON as a `json` part and files for a tab's typed
    /// answer as parts named `tab<N>` (saved like the panel's attachments).
    fn answer_questions(&self, req: &Request, id: &str) -> Response {
        let Some(set) = self.ctx.asks.get(id) else {
            return Response::error(404, "no_question", "no open question set with that id");
        };
        let (body, files): (Value, Vec<multipart::Part>) = match req.header("content-type").and_then(multipart::boundary) {
            Some(b) => match multipart::parse(&req.body, &b) {
                Ok(parts) => {
                    let (json_part, rest): (Vec<_>, Vec<_>) = parts.into_iter().partition(|p| p.name == "json");
                    let Some(j) = json_part.into_iter().next() else {
                        return Response::error(400, "no_json", "a multipart answer needs a `json` part with {answers, submit}");
                    };
                    (serde_json::from_slice(j.data).unwrap_or_default(), rest)
                }
                Err(e) => return Response::error(400, "bad_multipart", &format!("{e:#}")),
            },
            None => (req.json().unwrap_or_default(), vec![]),
        };
        // Typed answers go into a terminal: no control characters.
        let mut raw = body.get("answers").cloned().unwrap_or(Value::Null);
        clean_json(&mut raw);
        let mut answers: Vec<crate::questions::Answer> = match serde_json::from_value(raw) {
            Ok(a) => a,
            Err(e) => return Response::error(400, "bad_answers", &format!("answers: {e}")),
        };
        if answers.iter().any(|a| a.tab >= set.tabs.len()) {
            return Response::error(400, "bad_tab", "no such tab");
        }
        if let Err(r) = attach_tab_files(&mut answers, &files) {
            return r;
        }
        let submit = body["submit"].as_bool().unwrap_or(true);
        match self.ctx.asks.answer(id, &answers, submit) {
            Ok(set) => Response::json(200, &json!({ "ok": true, "id": set.id, "submitted": submit })),
            Err(e) => {
                let msg = format!("{e:#}");
                Response::error(409, question_code(&msg), &msg)
            }
        }
    }

    /// Sets where a device's replies are heard, on file and for its open streams.
    pub fn set_route(&self, id: &str, route: &str) -> Result<()> {
        if !self.devices.update(|d| d.set_route(id, route))? {
            bail!("no paired device {id}, or route {route:?} isn't phone or mac");
        }
        self.ctx.hub.set_route(id, route == "phone");
        eprintln!("[phone] {id} hears its replies on the {route}");
        Ok(())
    }

    /// Another process changed the route on file (`tower phone route`, the panel).
    pub fn route_changed(&self, id: &str) {
        if let Some(d) = self.devices.load().devices.iter().find(|d| d.id == id) {
            self.ctx.hub.set_route(id, d.audio_route == "phone");
        }
    }

    /// A recording from the phone: transcribed by the tower's Whisper, then
    /// handed to the brain like a press-to-talk take (`send`), or only
    /// transcribed. Everything is checked before a file is saved.
    fn voice(&self, req: &Request, send: bool, device: &auth::Device) -> Response {
        // A WAV body, or multipart: the WAV as `audio`, files beside it.
        let (wav, files): (&[u8], Vec<multipart::Part>) = match req.header("content-type").and_then(multipart::boundary) {
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
            None => (&req.body, vec![]),
        };
        let wav = match audio::Wav::parse(wav) {
            Ok(w) => w,
            Err(e) => return Response::error(415, "bad_audio", &format!("{e:#}: send audio/wav, 16-bit PCM, mono, 16 kHz preferred")),
        };
        // From the header: a long recording is refused before it's decoded.
        if !(0.2..=120.0).contains(&wav.seconds()) {
            return Response::error(400, "bad_length", "a recording is 0.2 to 120 seconds");
        }
        let (samples, rate) = (wav.decode(), wav.rate);
        // Only now, with the take known good: checked, then saved (nothing is saved if one is refused).
        let saved = if send {
            match save_files(&files, &crate::history::attachments_dir(), now_ms()) {
                Ok(p) => p,
                Err(r) => return r,
            }
        } else {
            vec![]
        };
        let attachments: Vec<String> = saved.iter().map(|f| f.path.clone()).collect();
        let markers: Vec<String> = saved.iter().map(|f| f.marker.clone()).collect();
        let samples = if rate == RATE { samples } else { super::mic::resample(&samples, rate, RATE) };
        let (tx, rx) = std::sync::mpsc::channel();
        let source = Source::Phone {
            reply: tx,
            attachments: attachments.clone(),
            markers,
            send,
            phone: heard_on_phone(device),
            device: device.id.clone(),
        };
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
    /// It ends when the phone goes (a write fails, or what was sent isn't
    /// taken for `STREAM_STUCK`), the device is revoked, or the API is turned off.
    fn events(&self, device: &auth::Device, voice: bool, w: &mut impl Write, fd: Option<RawFd>, stop: &AtomicBool) {
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
        // What the state says besides its counter: only a change there is a `state` event.
        let mut said = stateless(&state);
        // The queue and the missions as last sent: each has its own event, sent when it changes.
        let (mut decisions, mut count, mut missions) = self.ctx.shared.with(|s| (s.decisions.clone(), s.decisions_count, s.missions.clone()));
        let mut ok = send(w, "hello", &json!({ "api": API, "device": device.id, "voice": voice, "features": FEATURES }))
            && send(w, "state", &state)
            && send(w, "decisions", &decisions_event(&decisions, count))
            && send(w, "missions", &missions);
        let history = self.history.clone();
        let mut tail = Tail::end(&history);
        // The chat's latest messages as last sent: a new or changed one is a `chat` event.
        let mut chat = ChatTail::default();
        chat.diff(&self.chat());
        // A PR's chip changes without the history growing.
        let mut prs_version = self.ctx.prs.version();
        let mut acts_sent = self.ctx.shared.with(|s| s.acts.clone());
        let (mut last_ping, mut last_check, mut last_history) = (Instant::now(), Instant::now(), Instant::now());
        let (mut drained, mut last_unsent) = (Instant::now(), Instant::now());
        while ok && !stop.load(Ordering::SeqCst) {
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
                let state = self.state(device);
                let now = stateless(&state);
                if now != said {
                    said = now;
                    ok = send(w, "state", &state);
                }
                let (d, c, m) = self.ctx.shared.with(|s| (s.decisions.clone(), s.decisions_count, s.missions.clone()));
                if ok && (d != decisions || c != count) {
                    (decisions, count) = (d, c);
                    ok = send(w, "decisions", &decisions_event(&decisions, count));
                }
                if ok && m != missions {
                    missions = m;
                    ok = send(w, "missions", &missions);
                }
            }
            if ok && last_history.elapsed() > Duration::from_secs(1) {
                last_history = Instant::now();
                let (entries, end) = new_history(&history, tail);
                tail = end;
                let grew = !entries.is_empty();
                for (line, e) in entries {
                    if !ok {
                        break;
                    }
                    ok = send(w, "history", &e);
                    // A message between sessions: its space's thread got it.
                    if let Some(t) = thread_event(line, &e).filter(|_| ok) {
                        ok = send(w, "thread", &t);
                    }
                }
                let prs_changed = self.ctx.prs.version() != prs_version;
                prs_version = self.ctx.prs.version();
                // An act changes without the history growing too (its session was given more work).
                let acts_now = self.ctx.shared.with(|s| s.acts.clone());
                let acts_changed = acts_now != acts_sent;
                acts_sent = acts_now;
                if grew || prs_changed || acts_changed {
                    for (op, m) in chat.diff(&self.chat()) {
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
            // The phone vanished: what was sent (the pings at least) stays unacknowledged.
            if let Some(fd) = fd.filter(|_| last_unsent.elapsed() >= Duration::from_secs(1)) {
                last_unsent = Instant::now();
                match unsent(fd) {
                    Some(0) | None => drained = Instant::now(),
                    Some(_) if drained.elapsed() > STREAM_STUCK => {
                        eprintln!("[phone] {} stopped taking its event stream: closing it", device.name);
                        break;
                    }
                    Some(_) => {}
                }
            }
            // Revoked from the terminal meanwhile.
            if last_check.elapsed() > Duration::from_secs(5) {
                last_check = Instant::now();
                if !self.devices.load().devices.iter().any(|d| d.id == device.id) {
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

/// The state without what moves with every change (`version`,
/// `updated_ms`): two that are equal here say the same to a phone.
fn stateless(state: &Value) -> Value {
    let mut s = state.clone();
    if let Some(o) = s.as_object_mut() {
        o.remove("version");
        o.remove("updated_ms");
    }
    s
}

/// The tower isn't whole yet (the daemon is starting): nothing can be asked of it.
fn starting() -> Response {
    Response::error(503, "starting", "the tower is still starting: try again in a moment")
}

/// Why a question set didn't take its answers, as a code: `stopped`
/// (typing stopped half way), `answered` (answered meanwhile, in the
/// session or from the tower), `changed` (the dialog asks something else
/// now), else `cannot_answer`.
fn question_code(msg: &str) -> &'static str {
    if msg.starts_with("stopped at") {
        "stopped"
    } else if msg.starts_with("already answered") {
        "answered"
    } else if msg.contains("changed") {
        "changed"
    } else {
        "cannot_answer"
    }
}

/// Files for tab N's typed answer (parts named `tab<N>`): every tab's are
/// checked before any is saved, then attached to that tab's answer.
fn attach_tab_files(answers: &mut [crate::questions::Answer], files: &[multipart::Part]) -> Result<(), Response> {
    let by_tab: Vec<(usize, Vec<multipart::Part>)> = answers
        .iter()
        .enumerate()
        .map(|(i, a)| (i, files.iter().filter(|p| p.name == format!("tab{}", a.tab)).cloned().collect::<Vec<_>>()))
        .filter(|(_, mine)| !mine.is_empty())
        .collect();
    for (_, mine) in &by_tab {
        check_files(mine)?;
    }
    for (i, mine) in by_tab {
        let saved = save_files(&mine, &crate::history::attachments_dir(), now_ms())?;
        answers[i].attachments.extend(saved.into_iter().map(|s| s.path));
    }
    Ok(())
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
            let keep: std::collections::HashSet<String> =
                thread.iter().rev().take(60).filter_map(|m| m["id"].as_str().map(String::from)).collect();
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
            let _ = crate::protocol::send(&crate::protocol::Request::BehaviorsChanged { by: "user".into(), change: view.clone(), from: None });
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
        Some(w) if w.since_ms > 0 => {
            json!({ "word": w.word, "detail": w.detail, "since_ms": w.since_ms, "at_ms": w.at_ms, "done_ms": w.done_ms })
        }
        _ => Value::Null,
    }
}

/// A part that is a file (a field with a file name, or one named `image`/`file`).
fn is_file_part(p: &multipart::Part) -> bool {
    p.filename.is_some() || p.name.starts_with("image") || p.name.starts_with("file")
}

/// What `POST /v1/ask` carries.
type Asked<'a> = (String, Option<String>, Vec<multipart::Part<'a>>);

/// The text, the space it was asked in (when one is named) and the files
/// of `POST /v1/ask`: JSON `{"text", "space"}`, or multipart with a `text`
/// field, a `space` field and file parts.
fn ask_body(req: &Request) -> Result<Asked<'_>, Response> {
    let named = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    match req.header("content-type").and_then(multipart::boundary) {
        Some(b) => {
            let parts = multipart::parse(&req.body, &b).map_err(|e| Response::error(400, "bad_multipart", &format!("{e:#}")))?;
            let field = |name: &str| parts.iter().find(|p| p.name == name && p.filename.is_none()).map(|p| clean_text(&String::from_utf8_lossy(p.data)).trim().to_string());
            let (text, space) = (field("text"), field("space").and_then(named));
            Ok((text.unwrap_or_default(), space, parts.into_iter().filter(|p| !matches!(p.name.as_str(), "text" | "space") && is_file_part(p)).collect()))
        }
        None => {
            let body = req.json().map_err(|_| Response::error(400, "bad_json", "send {\"text\": \"...\"}"))?;
            let space = body["space"].as_str().map(clean_text).and_then(named);
            Ok((clean_text(body["text"].as_str().unwrap_or("")).trim().to_string(), space, vec![]))
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
    if crate::screenshots::kind_of(head).is_some()
        || matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "bmp" | "tiff")
    {
        FileKind::Image
    } else if matches!(ext.as_str(), "mov" | "mp4" | "m4v" | "webm" | "mkv" | "avi")
        || content_type.is_some_and(|t| t.starts_with("video/"))
    {
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

/// Checks files as `save_files` saves them: six at most; an image is a real
/// PNG, JPEG, WebP or GIF of 15 MB at most; a video or other file is 50 MB
/// at most. For each: its kind, its name as sent, and the extension to save it with.
pub fn check_files(parts: &[multipart::Part]) -> Result<Vec<(FileKind, String, String)>, Response> {
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
                    return Err(Response::error(
                        415,
                        "not_an_image",
                        &format!("file {} is named as an image but isn't a PNG, JPEG, WebP or GIF", i + 1),
                    ));
                };
                ext.to_string()
            }
            _ => {
                if p.data.len() > MAX_FILE_BYTES {
                    return Err(Response::error(413, "file_too_large", &format!("file {} is over 50 MB", i + 1)));
                }
                let e: String =
                    original.rsplit_once('.').map(|(_, e)| e).unwrap_or("").chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
                if e.is_empty() { "bin".into() } else { e.to_ascii_lowercase() }
            }
        };
        plan.push((kind, original, ext));
    }
    Ok(plan)
}

/// Checks files (`check_files`) and saves them as the panel saves its
/// attachments: `<attachments>/<ms>-phone-<random>/<n>-<name>`, a new
/// folder each time, never pruned.
pub fn save_files(parts: &[multipart::Part], attachments: &std::path::Path, at_ms: u64) -> Result<Vec<Saved>, Response> {
    let plan = check_files(parts)?;
    if plan.is_empty() {
        return Ok(vec![]);
    }
    let fail = |e: std::io::Error| Response::error(503, "save", &e.to_string());
    std::fs::create_dir_all(attachments).map_err(fail)?;
    // A folder of its own, even for two messages in the same millisecond.
    let dir = loop {
        let dir = attachments.join(format!("{at_ms}-phone-{}", auth::hex(&auth::random_bytes::<4>())));
        match std::fs::create_dir(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(fail(e)),
        }
    };
    let mut out = vec![];
    for (i, (p, (kind, original, ext))) in parts.iter().zip(plan).enumerate() {
        // `<ms>-<name>.<ext>` from the screenshot naming, without the ms (the folder has it).
        let name = crate::screenshots::stored_name(0, std::path::Path::new(&original), &ext);
        let name = format!("{}-{}", i + 1, name.trim_start_matches("0-"));
        let path = dir.join(&name);
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(fail)?;
        f.write_all(p.data).map_err(fail)?;
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

/// Where a stream is in the history: the bytes it has read, and how many
/// lines they were (a line's number is its entry's id, `h<line>`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Tail {
    offset: u64,
    lines: usize,
}

impl Tail {
    /// The end of the history as it is now.
    fn end(path: &std::path::Path) -> Tail {
        let bytes = std::fs::read(path).unwrap_or_default();
        let end = bytes.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
        Tail { offset: end as u64, lines: bytes[..end].iter().filter(|b| **b == b'\n').count() }
    }
}

/// History lines added since `tail` (whole lines only), each with its line
/// number, and where the stream is then.
fn new_history(path: &std::path::Path, tail: Tail) -> (Vec<(usize, Value)>, Tail) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return (vec![], tail) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // Shorter than it was (compacted): from its end as it is now.
    if len < tail.offset {
        return (vec![], Tail::end(path));
    }
    if len == tail.offset || f.seek(SeekFrom::Start(tail.offset)).is_err() {
        return (vec![], tail);
    }
    let mut buf = Vec::new();
    let _ = f.take(4 * 1024 * 1024).read_to_end(&mut buf);
    let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else { return (vec![], tail) };
    let text = String::from_utf8_lossy(&buf[..=last_nl]);
    let read = text.lines().count();
    let entries = text.lines().enumerate().filter_map(|(i, l)| serde_json::from_str(l).ok().map(|v| (tail.lines + i + 1, v))).collect();
    (entries, Tail { offset: tail.offset + last_nl as u64 + 1, lines: tail.lines + read })
}

/// A message between sessions as the `thread` event carries it: its space,
/// and the message as `GET /v1/thread` lists it.
fn thread_event(line: usize, entry: &Value) -> Option<Value> {
    let e: crate::history::Entry = serde_json::from_value(entry.clone()).ok().filter(|e: &crate::history::Entry| e.kind == "peer")?;
    Some(json!({ "space": e.space, "message": crate::peers::message(line, &e) }))
}

/// The `decisions` event: every open decision in queue order, each saying
/// whether its space is muted, and how many are in the main queue.
fn decisions_event(list: &[Value], count: usize) -> Value {
    json!({ "count": count, "decisions": list })
}

/// `t3`, `vps/t3`, a name: letters, digits, `-`, `_`, one `/`, starting
/// with a letter or digit (never `-`, which a command would take as an option).
fn valid_address(a: &str) -> bool {
    a.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && a.len() <= 64
        && a.matches('/').count() <= 1
        && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_/".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// This test thread lets 127.0.0.1 stand for the tailnet (`loopback_tailnet`).
        pub static LOOPBACK_TAILNET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[test]
    fn listens_only_on_private_addresses() {
        for ip in ["192.168.1.20", "10.0.0.5", "172.16.3.4", "100.101.102.103", "169.254.1.1", "fd00::1"] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "2001:db8::1"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
        let cfg = |bind: &str, allow_public| crate::config::Lan { enabled: true, port: 7843, bind: bind.into(), allow_public, tailscale: "off".into() };
        assert!(choose_ip(&cfg("8.8.8.8", false)).is_err(), "a public address needs allow_public");
        assert!(choose_ip(&cfg("0.0.0.0", false)).is_err(), "every interface needs allow_public");
        assert!(choose_ip(&cfg("nonsense", false)).is_err());
        assert_eq!(choose_ip(&cfg("192.168.1.20", false)).unwrap().to_string(), "192.168.1.20");
    }

    /// A phone that pins the tower's certificate by its fingerprint, as the app does.
    #[derive(Debug)]
    pub(super) struct Pin(String);

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
    pub(super) fn call(addr: SocketAddr, pin: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> Result<(u16, Value)> {
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

    /// An open connection to the tower, pinned, with `head` sent (an event stream).
    pub(super) fn open(addr: SocketAddr, pin: &str, head: &str) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin(pin.to_string())))
            .with_no_client_auth();
        let conn =
            rustls::ClientConnection::new(Arc::new(cfg), rustls::pki_types::ServerName::try_from("herder-tower").unwrap().to_owned()).unwrap();
        let tcp = TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut s = rustls::StreamOwned::new(conn, tcp);
        s.write_all(head.as_bytes()).unwrap();
        s.flush().unwrap();
        s
    }

    /// One HTTPS call with a binary body and a content type.
    pub(super) fn call_raw(addr: SocketAddr, pin: &str, method: &str, path: &str, token: &str, ctype: &str, body: &[u8]) -> Result<(u16, Value)> {
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
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )?;
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
        let ctx = Ctx { shared, mouth: Mouth::silent(hub.clone()), hub, stt, asks, prs: test_prs() };
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan"));
        let addr = lan.start(&crate::config::Lan { enabled: true, port: 0, bind: "127.0.0.1".into(), allow_public: false, tailscale: "off".into() }).unwrap();
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
        assert_eq!(
            call(addr, &fp, "POST", "/v1/questions/qnope/interpret", Some(&token), r#"{"tab":0,"text":"the first"}"#).unwrap().0,
            404
        );
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
        let ctx = Ctx { shared: shared.clone(), mouth: Mouth::silent(hub.clone()), hub: hub.clone(), stt, asks, prs: test_prs() };
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan"));
        let addr = lan.start(&crate::config::Lan { enabled: true, port: 0, bind: "127.0.0.1".into(), allow_public: false, tailscale: "off".into() }).unwrap();
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
        fn part<'a>(f: &str, ct: Option<&str>, d: &'a [u8]) -> multipart::Part<'a> {
            multipart::Part { name: "file".into(), filename: Some(f.into()), content_type: ct.map(String::from), data: d }
        }
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
        let tail = |s: &Saved| {
            let (folder, name) = s.path.rsplit_once('/').unwrap();
            assert!(folder.rsplit_once('/').unwrap().1.starts_with("42-phone-"), "{folder}");
            name.to_string()
        };
        assert_eq!(
            saved.iter().map(tail).collect::<Vec<_>>(),
            ["1-Screen-Shot.png", "2-clip.mov", "3-Q3-report--final.pdf", "4-noext.bin"]
        );
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
        let big = vec![0; MAX_FILE_BYTES + 1];
        assert_eq!(save_files(&[part("big.mp4", None, &big)], &dir, 46).unwrap_err().status, 413);
        // Two messages in the same millisecond: two folders, nothing overwritten.
        let a = save_files(&[part("a.png", None, png)], &dir, 47).unwrap();
        let b = save_files(&[part("a.png", None, b"\x89PNG\r\n\x1a\nother")], &dir, 47).unwrap();
        assert_ne!(a[0].path, b[0].path);
        assert_eq!(std::fs::read(&a[0].path).unwrap(), png);
        // Nothing is saved when one file of several is refused.
        let before = std::fs::read_dir(&dir).unwrap().count();
        assert!(save_files(&[part("ok.png", None, png), part("x.png", None, b"text")], &dir, 48).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), before);
        assert!(save_files(&[], &dir, 45).unwrap().is_empty());
        // Screenshot pruning never looks here.
        assert_eq!(crate::screenshots::prune(&dir, Duration::from_secs(0)), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markers_like_the_panel() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(with_markers("what is this?", &m(&["[Image #1]", "[Video #2]"])), "[Image #1] [Video #2] what is this?");
        assert_eq!(
            with_markers("see [File #2: a.pdf] and [Image #1]", &m(&["[Image #1]", "[File #2: a.pdf]"])),
            "see [File #2: a.pdf] and [Image #1]"
        );
        assert_eq!(
            with_markers("see [File #1: renamed.pdf]", &m(&["[File #1: a.pdf]"])),
            "see [File #1: renamed.pdf]",
            "a file token counts by its number"
        );
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
        let (t, space, imgs) = ask_body(&json_req).ok().unwrap();
        assert_eq!((t.as_str(), space, imgs.len()), ("hi", None, 0));
        let body = multipart::build(
            "BB",
            &[("text", "what's this?")],
            &[("image", "a.png", "image/png", b"\x89PNG\r\n\x1a\n"), ("images", "b.gif", "image/gif", b"GIF89a")],
        );
        let mut req = Request { body, ..Default::default() };
        req.headers.insert("content-type".into(), "multipart/form-data; boundary=BB".into());
        let (t, space, files) = ask_body(&req).ok().unwrap();
        assert_eq!((t.as_str(), space, files.len()), ("what's this?", None, 2));
        let bad = Request { body: b"nope".to_vec(), ..Default::default() };
        assert_eq!(ask_body(&bad).err().unwrap().status, 400);
        // Asked in a space's view: the space comes with it, both ways; an empty one is none.
        let there = Request { body: br#"{"text":"what's left?","space":" B2C support\u001b "}"#.to_vec(), ..Default::default() };
        assert_eq!(ask_body(&there).ok().unwrap().1.as_deref(), Some("B2C support"));
        let nowhere = Request { body: br#"{"text":"what's left?","space":"  "}"#.to_vec(), ..Default::default() };
        assert_eq!(ask_body(&nowhere).ok().unwrap().1, None);
        let body = multipart::build("BB", &[("text", "and this?"), ("space", "B2C support")], &[("image", "a.png", "image/png", b"\x89PNG\r\n\x1a\n")]);
        let mut req = Request { body, ..Default::default() };
        req.headers.insert("content-type".into(), "multipart/form-data; boundary=BB".into());
        let (t, space, files) = ask_body(&req).ok().unwrap();
        assert_eq!((t.as_str(), space.as_deref(), files.len()), ("and this?", Some("B2C support"), 1), "the space is a field, never a file");
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
        assert!(
            !valid_address("-h") && !valid_address("--lines") && !valid_address("/t3") && !valid_address("_x"),
            "an address never reads as an option"
        );
    }

    #[test]
    fn reads_new_history_lines_only() {
        let dir = std::env::temp_dir().join(format!("tower-lan-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("history.jsonl");
        std::fs::write(&p, "{\"kind\":\"a\"}\n").unwrap();
        let (first, off) = new_history(&p, Tail::default());
        assert_eq!((first.len(), first[0].0, off), (1, 1, Tail::end(&p)));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"not json\n{\"kind\":\"b\"}\n{\"kind\":\"c\"").unwrap();
        let (e, off2) = new_history(&p, off);
        assert_eq!(e.len(), 1, "the half-written line waits");
        assert_eq!((e[0].0, &e[0].1["kind"]), (3, &json!("b")), "a line that doesn't read keeps its number");
        assert_eq!(Tail::end(&p), off2, "a stream that starts now starts after the last whole line");
        f.write_all(b"}\n").unwrap();
        let (e, off3) = new_history(&p, off2);
        assert_eq!((e[0].0, &e[0].1["kind"]), (4, &json!("c")));
        // Compacted (shorter than it was): from its end as it is now, nothing sent twice.
        std::fs::write(&p, "{\"kind\":\"c\"}\n").unwrap();
        let (e, off4) = new_history(&p, off3);
        assert_eq!((e.len(), off4), (0, Tail { offset: 13, lines: 1 }));
        // A message between sessions is a `thread` event too; nothing else is.
        let peer = json!({ "at": 5, "kind": "peer", "session": "t6", "name": "api", "from": "t5", "from_name": "web", "space": "Shop", "text": "which port?", "status": "delivered", "record": true });
        let ev = thread_event(7, &peer).unwrap();
        assert_eq!((ev["space"].as_str(), ev["message"]["id"].as_str(), ev["message"]["from_name"].as_str(), ev["message"]["delivered"].as_bool()), (Some("Shop"), Some("h7"), Some("web"), Some(true)));
        assert!(thread_event(8, &json!({ "at": 6, "kind": "note", "text": "x" })).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_from_the_phone_has_no_control_characters() {
        assert_eq!(clean_text("ls\u{1b}[2J\u{1b}]0;x\u{7}\r\nok\tdone\u{9b}31m\u{0}"), "ls[2J]0;x\nok\tdone31m");
        assert_eq!(clean_text("Grüße, 日本 ✓"), "Grüße, 日本 ✓");
        let mut v = json!([{ "tab": 0, "choice": { "other": "yes\u{1b}[A\u{3}" } }]);
        clean_json(&mut v);
        assert_eq!(v[0]["choice"]["other"], "yes[A");
        let req = Request { body: br#"{"text":"hi\u001b[31m there\r"}"#.to_vec(), ..Default::default() };
        assert_eq!(ask_body(&req).ok().unwrap().0, "hi[31m there");
    }

    #[test]
    fn each_call_has_its_body_limit() {
        let req = |m: &str, ctype: Option<&str>| {
            let mut r = Request { method: m.into(), ..Default::default() };
            if let Some(c) = ctype {
                r.headers.insert("content-type".into(), c.into());
            }
            r
        };
        let mp = Some("multipart/form-data; boundary=x");
        assert_eq!(body_limit(&req("POST", None), &["v1", "pair"]), http::MAX_PAIR_BODY);
        assert_eq!(body_limit(&req("POST", mp), &["v1", "pair"]), http::MAX_PAIR_BODY, "multipart doesn't widen pairing");
        assert_eq!(body_limit(&req("POST", Some("audio/wav")), &["v1", "voice"]), http::MAX_VOICE_BODY);
        assert_eq!(body_limit(&req("POST", None), &["v1", "questions", "q1", "voice"]), http::MAX_VOICE_BODY);
        assert_eq!(body_limit(&req("POST", mp), &["v1", "voice"]), http::MAX_BODY);
        assert_eq!(body_limit(&req("POST", mp), &["v1", "ask"]), http::MAX_BODY);
        assert_eq!(body_limit(&req("POST", mp), &["v1", "questions", "q1", "answer"]), http::MAX_BODY);
        // A decision's answer and its spoken answer take what a question's do.
        assert_eq!(body_limit(&req("POST", mp), &["v1", "decisions", "d0123456789ab", "answer"]), http::MAX_BODY);
        assert_eq!(body_limit(&req("POST", None), &["v1", "decisions", "d0123456789ab", "answer"]), http::MAX_JSON_BODY);
        assert_eq!(body_limit(&req("POST", Some("audio/wav")), &["v1", "decisions", "d0123456789ab", "voice"]), http::MAX_VOICE_BODY);
        assert_eq!(body_limit(&req("POST", mp), &["v1", "launch"]), http::MAX_JSON_BODY);
        assert_eq!(body_limit(&req("POST", None), &["v1", "ask"]), http::MAX_JSON_BODY);
        assert_eq!(body_limit(&req("PUT", mp), &["v1", "behaviors", "b1"]), http::MAX_JSON_BODY);
    }

    #[test]
    fn connections_are_capped_by_address_and_given_back() {
        let conns = Arc::new(Mutex::new(Conns::default()));
        let a: IpAddr = "192.168.1.20".parse().unwrap();
        let b: IpAddr = "192.168.1.21".parse().unwrap();
        let mine: Vec<Slot> = (0..MAX_PER_IP).map(|_| Slot::take(&conns, a).unwrap()).collect();
        assert!(Slot::take(&conns, a).is_none(), "one address can't take every connection");
        let theirs = Slot::take(&conns, b).unwrap();
        drop(mine);
        assert!(Slot::take(&conns, a).is_some(), "given back when dropped");
        // A handler that panics gives its place back too.
        let slot = Slot::take(&conns, b).unwrap();
        let t = thread::spawn(move || {
            let _slot = slot;
            panic!("a bug");
        });
        assert!(t.join().is_err());
        drop(theirs);
        let c = conns.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!((c.total, c.by_ip.len()), (0, 0));
    }

    #[test]
    fn a_slow_client_meets_one_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            // A byte every 100 ms: each read is quick, the whole never ends.
            let mut c = TcpStream::connect(addr).unwrap();
            for _ in 0..30 {
                if c.write_all(b"G").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        let mut sock = Sock { tcp, deadline: Some(Instant::now() + Duration::from_millis(500)) };
        let t = Instant::now();
        let err = http::read_head(&mut sock).unwrap_err();
        assert!(t.elapsed() < Duration::from_secs(2), "{err:#} after {:?}", t.elapsed());
        drop(sock);
        client.join().unwrap();
    }

    /// Writes a request's head (and `body`), and reads the status.
    fn call_head(addr: SocketAddr, pin: &str, head: &str, body: &[u8]) -> Result<(u16, Value, Duration)> {
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pin(pin.to_string())))
            .with_no_client_auth();
        let conn = rustls::ClientConnection::new(Arc::new(cfg), rustls::pki_types::ServerName::try_from("herder-tower")?.to_owned())?;
        let tcp = TcpStream::connect(addr)?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);
        let t = Instant::now();
        s.write_all(head.as_bytes())?;
        s.write_all(body)?;
        s.flush()?;
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let text = String::from_utf8_lossy(&out);
        let status: u16 = text.split(' ').nth(1).unwrap_or("0").parse()?;
        Ok((status, serde_json::from_str(text.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap_or(Value::Null), t.elapsed()))
    }

    /// A PR store of its own for a test (no gh: nothing is looked up).
    pub(super) fn test_prs() -> crate::prs::Prs {
        let dir = std::env::temp_dir().join(format!("tower-lan-prs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        crate::prs::Prs::at(dir.join("prs.json"), std::path::PathBuf::from("/nonexistent/gh"))
    }

    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn test_lan(name: &str) -> (Lan, std::path::PathBuf, SocketAddr, String) {
        test_lan_with(name, test_prs())
    }

    fn test_lan_with(name: &str, prs: crate::prs::Prs) -> (Lan, std::path::PathBuf, SocketAddr, String) {
        let dir = std::env::temp_dir().join(format!("tower-lan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, rx) = std::sync::mpsc::channel();
        std::mem::forget(rx);
        let shared = Shared::new(super::super::state::State::new("ptt", "af_bella", "test"));
        let asks = super::super::asks::Asks::new(shared.clone(), hub.clone(), "normal");
        let ctx = Ctx { shared, mouth: Mouth::silent(hub.clone()), hub, stt, asks, prs };
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan"));
        let addr = lan.start(&crate::config::Lan { enabled: true, port: 0, bind: "127.0.0.1".into(), allow_public: false, tailscale: "off".into() }).unwrap();
        let fp = lan.info()["fingerprint"].as_str().unwrap().to_string();
        (lan, dir, addr, fp)
    }

    fn ts(running: bool, ip: &str) -> tailscale::Tailscale {
        tailscale::Tailscale {
            cli: Some("/usr/local/bin/tailscale".into()),
            backend: if running { "Running" } else { "Stopped" }.into(),
            running,
            ipv4: ip.parse().ok(),
            dns_name: Some("mac.tail1234.ts.net".into()),
            error: None,
        }
    }

    fn lan_cfg(bind: &str, tailscale: &str) -> crate::config::Lan {
        crate::config::Lan { enabled: true, port: 7843, bind: bind.into(), allow_public: false, tailscale: tailscale.into() }
    }

    #[test]
    fn the_lan_and_a_running_tailnet_are_listened_on_and_nothing_else() {
        let home: IpAddr = "192.168.1.20".parse().unwrap();
        let tail: IpAddr = "100.68.110.33".parse().unwrap();
        let up = ts(true, "100.68.110.33");
        // Both, the LAN first.
        assert_eq!(plan(&lan_cfg("auto", "auto"), Some(home), &up).unwrap(), vec![(Kind::Lan, home), (Kind::Tailscale, tail)]);
        // Stopped, not installed, or off: the LAN only.
        assert_eq!(plan(&lan_cfg("auto", "auto"), Some(home), &ts(false, "100.68.110.33")).unwrap(), vec![(Kind::Lan, home)]);
        assert_eq!(plan(&lan_cfg("auto", "auto"), Some(home), &tailscale::Tailscale::default()).unwrap(), vec![(Kind::Lan, home)]);
        assert_eq!(plan(&lan_cfg("auto", "off"), Some(home), &up).unwrap(), vec![(Kind::Lan, home)]);
        // No LAN (Wi-Fi off): the tailnet alone; neither: an error.
        assert_eq!(plan(&lan_cfg("auto", "auto"), None, &up).unwrap(), vec![(Kind::Tailscale, tail)]);
        assert!(plan(&lan_cfg("auto", "auto"), None, &ts(false, "100.68.110.33")).is_err());
        // The default route over the tailnet (an exit node): one listener.
        assert_eq!(plan(&lan_cfg("auto", "auto"), Some(tail), &up).unwrap(), vec![(Kind::Tailscale, tail)]);
        // A "tailnet" address outside 100.64/10 is never taken.
        assert_eq!(plan(&lan_cfg("auto", "auto"), Some(home), &ts(true, "100.200.1.1")).unwrap(), vec![(Kind::Lan, home)]);
        // Every interface or a public address: refused, Tailscale or not.
        assert!(plan(&lan_cfg("0.0.0.0", "auto"), Some(home), &up).is_err());
        assert!(plan(&lan_cfg("8.8.8.8", "auto"), Some(home), &up).is_err());
        // The tailnet's address counts as private (100.64/10), as before.
        assert!(is_private(tail));
        assert!(plan(&lan_cfg("auto", "sometimes"), Some(home), &up).unwrap_err().to_string().contains("auto"));
        assert!(tailscale_wanted(&lan_cfg("auto", "")).unwrap() && tailscale_wanted(&lan_cfg("auto", "on")).unwrap());
    }

    #[test]
    fn the_pairing_payload_lists_every_address_and_keeps_host_and_port() {
        let fp = "ab".repeat(32);
        let both = [(Kind::Lan, "192.168.1.20:7843".parse().unwrap()), (Kind::Tailscale, "100.68.110.33:7843".parse().unwrap())];
        let v = pairing_payload("12345678", &both, &fp, &ts(true, "100.68.110.33"), 300);
        assert_eq!(
            v,
            json!({
                "code": "12345678",
                "host": "192.168.1.20",
                "port": 7843,
                "hosts": [
                    {"kind": "lan", "host": "192.168.1.20", "port": 7843},
                    {"kind": "tailscale", "host": "100.68.110.33", "port": 7843, "dns_name": "mac.tail1234.ts.net"}
                ],
                "magic_dns": "mac.tail1234.ts.net",
                "fingerprint": fp,
                "link": format!("herdertower://pair?h=192.168.1.20&p=7843&c=12345678&fp={fp}&hosts=lan:192.168.1.20:7843,tailscale:100.68.110.33:7843&dns=mac.tail1234.ts.net"),
                "expires_in_s": 300,
                "tailscale": {"installed": true, "running": true, "state": "running", "ip": "100.68.110.33", "dns_name": "mac.tail1234.ts.net"}
            })
        );
        // The LAN only (Tailscale stopped): the link as older apps know it, no MagicDNS name.
        let one = [(Kind::Lan, "192.168.1.20:7843".parse().unwrap())];
        let v = pairing_payload("12345678", &one, &fp, &ts(false, "100.68.110.33"), 300);
        assert_eq!(v["hosts"], json!([{"kind": "lan", "host": "192.168.1.20", "port": 7843}]));
        assert_eq!(v["magic_dns"], Value::Null);
        assert_eq!(v["link"], format!("herdertower://pair?h=192.168.1.20&p=7843&c=12345678&fp={fp}"));
        assert_eq!(v["tailscale"]["state"], "stopped");
        // The tailnet only: it is `host` too.
        let tail = [(Kind::Tailscale, "100.68.110.33:7843".parse().unwrap())];
        let v = pairing_payload("12345678", &tail, &fp, &ts(true, "100.68.110.33"), 300);
        assert_eq!((v["host"].as_str(), v["hosts"].as_array().unwrap().len()), (Some("100.68.110.33"), 1));
        assert!(v["link"].as_str().unwrap().ends_with("&hosts=tailscale:100.68.110.33:7843&dns=mac.tail1234.ts.net"));
        // The QR still holds it.
        assert!(qrcode::QrCode::new(pairing_payload("12345678", &both, &fp, &ts(true, "100.68.110.33"), 300)["link"].as_str().unwrap()).is_ok());
    }

    #[test]
    fn a_tailnet_address_that_cant_be_had_leaves_the_lan_listening() {
        let dir = std::env::temp_dir().join(format!("tower-lan-ts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, rx) = std::sync::mpsc::channel();
        std::mem::forget(rx);
        let shared = Shared::new(super::super::state::State::new("ptt", "af_bella", "test"));
        let asks = super::super::asks::Asks::new(shared.clone(), hub.clone(), "normal");
        let ctx = Ctx { shared: shared.clone(), mouth: Mouth::silent(hub.clone()), hub, stt, asks, prs: test_prs() };
        // "Running", with an address no interface here has (as just after Tailscale stops).
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan")).with_detect(|| ts(true, "100.100.100.100"));
        let addr = lan.start(&crate::config::Lan { port: 0, ..lan_cfg("127.0.0.1", "auto") }).unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        let info = lan.info();
        assert_eq!(info["hosts"], json!([{"kind": "lan", "host": "127.0.0.1", "port": addr.port()}]));
        assert_eq!(info["tailscale"]["running"], true);
        assert_eq!(info["tailscale"]["listening"], false);
        assert_eq!(info["tailscale"]["mode"], "auto");
        assert!(info["tailscale"]["error"].as_str().unwrap().contains("100.100.100.100"), "{info}");
        let st = shared.get();
        assert_eq!(st.lan, format!("https://{addr}"));
        assert_eq!(st.lan_hosts.len(), 1);
        assert_eq!(st.tailscale.as_ref().unwrap()["ip"], "100.100.100.100");
        let p = lan.pair().unwrap();
        assert_eq!((p["host"].as_str(), p["hosts"].as_array().unwrap().len(), &p["magic_dns"]), (Some("127.0.0.1"), 1, &Value::Null));
        // It answers over the LAN, pinned, as before.
        let fp = info["fingerprint"].as_str().unwrap().to_string();
        let (status, _) = call(addr, &fp, "GET", "/v1/hello", None, "").unwrap();
        assert_eq!(status, 401, "no token: refused, but over TLS with the pinned certificate");
        lan.stop();
        assert!(shared.get().lan_hosts.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tailscale_coming_and_going_is_followed_without_touching_the_lan() {
        let dir = std::env::temp_dir().join(format!("tower-lan-ts-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, rx) = std::sync::mpsc::channel();
        std::mem::forget(rx);
        let shared = Shared::new(super::super::state::State::new("ptt", "af_bella", "test"));
        let asks = super::super::asks::Asks::new(shared.clone(), hub.clone(), "normal");
        let ctx = Ctx { shared: shared.clone(), mouth: Mouth::silent(hub.clone()), hub, stt, asks, prs: test_prs() };
        let now = Arc::new(Mutex::new(ts(false, "100.100.100.100")));
        let seen = now.clone();
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan")).with_detect(move || seen.lock().unwrap().clone());
        let addr = lan.start(&crate::config::Lan { port: 0, ..lan_cfg("127.0.0.1", "auto") }).unwrap();
        let stop = lan.running.lock().unwrap().as_ref().unwrap().stop.clone();
        assert_eq!(lan.info()["tailscale"]["state"], "stopped");
        // It starts: the tower tries the tailnet address (none here has it: the error is kept, the LAN stays).
        *now.lock().unwrap() = ts(true, "100.100.100.100");
        lan.recheck_tailscale(&stop);
        let info = lan.info();
        assert_eq!((info["tailscale"]["running"].as_bool(), info["tailscale"]["listening"].as_bool()), (Some(true), Some(false)));
        assert!(info["tailscale"]["error"].as_str().is_some_and(|e| e.contains("100.100.100.100")));
        assert_eq!(info["hosts"], json!([{"kind": "lan", "host": "127.0.0.1", "port": addr.port()}]));
        assert_eq!(shared.get().tailscale.unwrap()["state"], "running");
        // It stops: the error goes, the LAN still answers.
        *now.lock().unwrap() = ts(false, "100.100.100.100");
        lan.recheck_tailscale(&stop);
        let info = lan.info();
        assert_eq!((info["tailscale"]["state"].as_str(), &info["tailscale"]["error"]), (Some("stopped"), &Value::Null));
        let fp = info["fingerprint"].as_str().unwrap().to_string();
        assert_eq!(call(addr, &fp, "GET", "/v1/hello", None, "").unwrap().0, 401);
        // A watcher of an API stopped since does nothing to the next one.
        lan.stop();
        let addr2 = lan.start(&crate::config::Lan { port: 0, ..lan_cfg("127.0.0.1", "auto") }).unwrap();
        *now.lock().unwrap() = ts(true, "100.100.100.100");
        lan.recheck_tailscale(&stop);
        assert_eq!(lan.info()["tailscale"]["error"], Value::Null, "the old watcher's look isn't this API's");
        assert_eq!(lan.info()["url"], format!("https://{addr2}"));
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_lan_listener_follows_this_macs_network_from_home_to_the_office() {
        let dir = std::env::temp_dir().join(format!("tower-lan-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, rx) = std::sync::mpsc::channel();
        std::mem::forget(rx);
        let shared = Shared::new(super::super::state::State::new("ptt", "af_bella", "test"));
        let asks = super::super::asks::Asks::new(shared.clone(), hub.clone(), "normal");
        let ctx = Ctx { shared: shared.clone(), mouth: Mouth::silent(hub.clone()), hub, stt, asks, prs: test_prs() };
        // "Home" and "the office": two addresses this Mac can listen on in a test.
        let home: IpAddr = "127.0.0.1".parse().unwrap();
        let office: IpAddr = "::1".parse().unwrap();
        let now = Arc::new(Mutex::new(Some(home)));
        let seen = now.clone();
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan")).with_lan_detect(move || *seen.lock().unwrap());
        let addr = lan.start(&crate::config::Lan { port: 0, ..lan_cfg("auto", "off") }).unwrap();
        assert_eq!(addr.ip(), home);
        let stop = lan.running.lock().unwrap().as_ref().unwrap().stop.clone();
        let fp = lan.info()["fingerprint"].as_str().unwrap().to_string();
        // Nothing changed: nothing moves.
        lan.recheck_lan(&stop);
        assert_eq!(lan.info()["url"], format!("https://{addr}"));
        // Wi-Fi off: no address now, and the only listener stays (it answers again when the network is back).
        *now.lock().unwrap() = None;
        lan.recheck_lan(&stop);
        assert_eq!(call(addr, &fp, "GET", "/v1/hello", None, "").unwrap().0, 401);
        // At the office: it listens there on the same port, the pairing offers that address, and the old one is let go.
        *now.lock().unwrap() = Some(office);
        lan.recheck_lan(&stop);
        let moved = SocketAddr::new(office, addr.port());
        assert_eq!(lan.info()["url"], format!("https://{moved}"));
        assert_eq!(shared.get().lan_hosts, vec![json!({"kind": "lan", "host": "::1", "port": addr.port()})]);
        assert_eq!(call(moved, &fp, "GET", "/v1/hello", None, "").unwrap().0, 401, "answers at the new address, same certificate");
        assert!(call(addr, &fp, "GET", "/v1/hello", None, "").is_err(), "the home address is let go");
        // An address set by hand is never moved.
        lan.stop();
        let fixed = lan.start(&crate::config::Lan { port: 0, ..lan_cfg("127.0.0.1", "off") }).unwrap();
        let stop = lan.running.lock().unwrap().as_ref().unwrap().stop.clone();
        lan.recheck_lan(&stop);
        assert_eq!(lan.info()["url"], format!("https://{fixed}"));
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stopped_listener_has_closed_its_socket_when_unlisten_returns() {
        let (lan, dir, _, _) = test_lan("unlisten");
        let (tls, api_stop) = {
            let r = lan.running.lock().unwrap();
            let r = r.as_ref().unwrap();
            (r.tls.clone(), r.stop.clone())
        };
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let mut port = 0;
        // Listened on, stopped, and the same address again at once, round after round:
        // the old socket is never still there (no "Address already in use").
        for _ in 0..20 {
            let l = lan.listen(Kind::Tailscale, ip, port, &tls, &api_stop).unwrap();
            let addr = l.addr;
            port = addr.port();
            Lan::unlisten(l);
            assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err(), "{addr} still answers");
        }
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_listener_whose_address_cant_be_reached_still_lets_its_socket_go() {
        let (lan, dir, _, _) = test_lan("unreachable");
        let (tls, api_stop) = {
            let r = lan.running.lock().unwrap();
            let r = r.as_ref().unwrap();
            (r.tls.clone(), r.stop.clone())
        };
        let l = lan.listen(Kind::Tailscale, "127.0.0.1".parse().unwrap(), 0, &tls, &api_stop).unwrap();
        let addr = l.addr;
        // Told to stop, and nothing connects to wake it (as when Tailscale stopped and its
        // address is gone): the loop still ends and closes the socket, so the address can
        // be listened on again (it was "Address already in use" against our own socket).
        l.stop.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            match TcpListener::bind(addr) {
                Ok(_) => break,
                Err(_) if Instant::now() < until => thread::sleep(Duration::from_millis(20)),
                Err(e) => panic!("{addr} is still held by the stopped listener: {e}"),
            }
        }
        Lan::unlisten(l);
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tailnet_is_listened_on_again_each_time_tailscale_comes_back() {
        LOOPBACK_TAILNET.with(|c| c.set(true));
        let dir = std::env::temp_dir().join(format!("tower-lan-ts-again-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, rx) = std::sync::mpsc::channel();
        std::mem::forget(rx);
        let shared = Shared::new(super::super::state::State::new("ptt", "af_bella", "test"));
        let asks = super::super::asks::Asks::new(shared.clone(), hub.clone(), "normal");
        let ctx = Ctx { shared: shared.clone(), mouth: Mouth::silent(hub.clone()), hub, stt, asks, prs: test_prs() };
        // 127.0.0.1 is the tailnet here; the LAN is ::1.
        let now = Arc::new(Mutex::new(ts(false, "127.0.0.1")));
        let seen = now.clone();
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan")).with_detect(move || seen.lock().unwrap().clone());
        let addr = lan.start(&crate::config::Lan { port: 0, ..lan_cfg("::1", "auto") }).unwrap();
        let stop = lan.running.lock().unwrap().as_ref().unwrap().stop.clone();
        let fp = lan.info()["fingerprint"].as_str().unwrap().to_string();
        let tail = SocketAddr::from(([127, 0, 0, 1], addr.port()));
        let tailnet = json!({"kind": "tailscale", "host": "127.0.0.1", "port": addr.port(), "dns_name": "mac.tail1234.ts.net"});
        // Off and on again, as the user does: each time it's listened on, never "Address already in use".
        for round in 0..3 {
            *now.lock().unwrap() = ts(true, "127.0.0.1");
            lan.recheck_tailscale(&stop);
            let info = lan.info();
            let seen = (info["tailscale"]["listening"].as_bool(), &info["tailscale"]["error"]);
            assert_eq!(seen, (Some(true), &Value::Null), "round {round}: {info}");
            assert_eq!(info["hosts"][1], tailnet, "round {round}");
            assert_eq!(call(tail, &fp, "GET", "/v1/hello", None, "").unwrap().0, 401, "round {round}: answers on the tailnet");
            *now.lock().unwrap() = ts(false, "127.0.0.1");
            lan.recheck_tailscale(&stop);
            let info = lan.info();
            let seen = (info["tailscale"]["listening"].as_bool(), &info["tailscale"]["error"]);
            assert_eq!(seen, (Some(false), &Value::Null), "round {round}");
            assert!(TcpStream::connect_timeout(&tail, Duration::from_millis(300)).is_err(), "round {round}: the old socket is closed");
            assert_eq!(call(addr, &fp, "GET", "/v1/hello", None, "").unwrap().0, 401, "round {round}: the LAN goes on");
        }
        // Its address changes while it runs: the old socket goes, the new address is tried.
        *now.lock().unwrap() = ts(true, "127.0.0.1");
        lan.recheck_tailscale(&stop);
        assert_eq!(lan.info()["tailscale"]["listening"], true);
        *now.lock().unwrap() = ts(true, "100.100.100.100");
        lan.recheck_tailscale(&stop);
        let info = lan.info();
        assert_eq!(info["tailscale"]["listening"], false);
        assert!(info["tailscale"]["error"].as_str().is_some_and(|e| e.contains("100.100.100.100")), "{info}");
        assert!(TcpStream::connect_timeout(&tail, Duration::from_millis(300)).is_err(), "the old address is let go");
        // And back to the first: listened on again.
        *now.lock().unwrap() = ts(true, "127.0.0.1");
        lan.recheck_tailscale(&stop);
        let info = lan.info();
        assert_eq!((info["tailscale"]["listening"].as_bool(), &info["tailscale"]["error"]), (Some(true), &Value::Null), "{info}");
        lan.stop();
        assert!(TcpStream::connect_timeout(&tail, Duration::from_millis(300)).is_err(), "stopping closes every socket");
        assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err(), "stopping closes every socket");
        LOOPBACK_TAILNET.with(|c| c.set(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn who_is_calling_is_known_before_the_body_is_read() {
        let (lan, dir, addr, fp) = test_lan("body");
        // No token, a huge body promised and never sent: refused at once, nothing waited for.
        let (st, _, took) =
            call_head(addr, &fp, &format!("POST /v1/ask HTTP/1.1\r\nContent-Length: {}\r\n\r\n", http::MAX_BODY), b"").unwrap();
        assert_eq!(st, 401);
        assert!(took < Duration::from_secs(5), "{took:?}");
        // Pairing takes a small body only.
        let (st, v, _) = call_head(addr, &fp, "POST /v1/pair HTTP/1.1\r\nContent-Length: 100000\r\n\r\n", b"").unwrap();
        assert_eq!((st, v["error"]["code"].as_str()), (413, Some("too_large")));
        // A paired phone: JSON calls are small too, and a body that is short of what it said is refused.
        let code = lan.pair().unwrap()["code"].as_str().unwrap().to_string();
        let token =
            call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{code}"}}"#)).unwrap().1["token"].as_str().unwrap().to_string();
        let auth = format!("Authorization: Bearer {token}\r\n");
        let big = format!("POST /v1/ask HTTP/1.1\r\n{auth}Content-Length: {}\r\n\r\n", http::MAX_JSON_BODY + 1);
        assert_eq!(call_head(addr, &fp, &big, b"").unwrap().0, 413);
        let wav =
            format!("POST /v1/voice HTTP/1.1\r\n{auth}Content-Type: audio/wav\r\nContent-Length: {}\r\n\r\n", http::MAX_VOICE_BODY + 1);
        assert_eq!(call_head(addr, &fp, &wav, b"").unwrap().0, 413);
        let (st, v, _) = call_head(addr, &fp, &format!("POST /v1/hush HTTP/1.1\r\n{auth}Content-Length: 2\r\n\r\n"), b"{}").unwrap();
        assert_eq!((st, v["ok"].as_bool()), (200, Some(true)));
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A phone merges a PR a session opened: listed, confirmed (a token for
    /// this phone, this PR, this commit, once), refused without the second
    /// confirmation while a check fails, then merged with it; gh is a fake.
    #[test]
    fn a_phone_merges_a_pr_in_two_steps() {
        use crate::prs::tests::{calls, fake_gh, serve};
        let gh = fake_gh("lan-merge");
        serve(&gh, 12, "OPEN", json!([{"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"FAILURE"}]), "abc");
        let prs = crate::prs::Prs::at(gh.with_file_name("prs.json"), gh.clone());
        let merged_hook = Arc::new(Mutex::new(Vec::<String>::new()));
        let m2 = merged_hook.clone();
        prs.on_merged(Box::new(move |pr, by| m2.lock().unwrap().push(format!("{} {by}", pr.key()))));
        prs.found(&[("o/app".into(), 12)], "t58", "mediaroom", Some("b2c-tickets"), true);
        let (lan, dir, addr, fp) = test_lan_with("prs", prs.clone());
        let code = lan.pair().unwrap()["code"].as_str().unwrap().to_string();
        let paired = call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{code}"}}"#)).unwrap().1;
        let (token, device) = (paired["token"].as_str().unwrap().to_string(), paired["device"]["id"].as_str().unwrap_or("").to_string());
        let t = Some(token.as_str());
        let (st, v) = call(addr, &fp, "GET", "/v1/prs", t, "").unwrap();
        assert_eq!((st, v["prs"][0]["key"].as_str(), v["prs"][0]["waiting"].as_bool()), (200, Some("o/app#12"), Some(true)));
        assert_eq!(v["prs"][0]["merge"]["needs_override"].as_str(), Some("1 check failing: lint. Merge anyway only if you know the failure is unrelated."));
        assert_eq!(call(addr, &fp, "GET", "/v1/prs/o/app/12", t, "").unwrap().1["pr"]["base"], "main");
        assert_eq!(call(addr, &fp, "GET", "/v1/prs/o/app/99", t, "").unwrap().0, 404);
        // No token: nothing happens.
        let (st, v) = call(addr, &fp, "POST", "/v1/prs/o/app/12/merge", t, r#"{"token":"made-up"}"#).unwrap();
        assert_eq!((st, v["error"]["code"].as_str()), (403, Some("bad_token")));
        // Confirmed: what it would do; the failing check needs the second confirmation.
        let (st, c) = call(addr, &fp, "POST", "/v1/prs/o/app/12/confirm", t, "{}").unwrap();
        assert_eq!((st, c["can"].as_bool(), c["method"].as_str()), (200, Some(true), Some("squash")));
        assert!(c["warnings"][0].as_str().unwrap().contains("into main"), "{c}");
        let body = format!(r#"{{"token":"{}"}}"#, c["token"].as_str().unwrap());
        let (st, v) = call(addr, &fp, "POST", "/v1/prs/o/app/12/merge", t, &body).unwrap();
        assert_eq!((st, v["error"]["code"].as_str()), (409, Some("needs_override")));
        // The token went with that try.
        assert_eq!(call(addr, &fp, "POST", "/v1/prs/o/app/12/merge", t, &body).unwrap().0, 403);
        assert!(!calls(&gh).contains("pr merge"));
        // Again, saying the failure is unrelated: merged, the chip moves, the session and the brain are told.
        let c = call(addr, &fp, "POST", "/v1/prs/o/app/12/confirm", t, "{}").unwrap().1;
        let body = format!(r#"{{"token":"{}","override":true}}"#, c["token"].as_str().unwrap());
        let (st, v) = call(addr, &fp, "POST", "/v1/prs/o/app/12/merge", t, &body).unwrap();
        assert_eq!((st, v["merged"].as_bool(), v["pr"]["state"].as_str()), (200, Some(true), Some("merged")), "{v}");
        assert_eq!(v["pr"]["merged_by"].as_str().map(String::from), Some(format!("phone:{device}")));
        assert_eq!(*merged_hook.lock().unwrap(), [format!("o/app#12 phone:{device}")]);
        let log = calls(&gh);
        assert!(log.contains("pr merge 12 --repo o/app --squash --match-head-commit abc") && !log.contains("--admin"), "{log}");
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pairing_tries_are_counted_by_address() {
        let (lan, dir, addr, fp) = test_lan("tries");
        let code = lan.pair().unwrap()["code"].as_str().unwrap().to_string();
        let wrong = if code == "00000000" { "11111111" } else { "00000000" };
        // Five wrong tries from here: the code is gone for this address only.
        for left in (0..auth::CODE_TRIES).rev() {
            let (st, v) = call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{wrong}"}}"#)).unwrap();
            assert_eq!((st, v["error"]["tries_left"].as_u64()), (403, Some(left as u64)));
        }
        assert_eq!(
            call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{code}"}}"#)).unwrap().1["error"]["code"],
            "pairing_closed"
        );
        assert!(lan.pairing.lock().unwrap_or_else(|e| e.into_inner()).take(&code, "192.168.1.30".parse().unwrap()).is_ok(), "another phone can still pair");
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turning_it_off_ends_every_stream() {
        let (lan, dir, addr, fp) = test_lan("off");
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
        let mut buf = [0u8; 4096];
        while !seen.contains("event: state") {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0);
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(lan.ctx.hub.voice());
        lan.stop();
        assert!(!lan.ctx.hub.voice(), "off: the Mac is the voice again");
        assert!(lan.ctx.shared.get().phone.is_none());
        // The stream ends (its end, or an error once the server is gone).
        let t = Instant::now();
        loop {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => assert!(t.elapsed() < Duration::from_secs(3), "the stream goes on after the API is off"),
            }
        }
        assert!(t.elapsed() < Duration::from_secs(3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
