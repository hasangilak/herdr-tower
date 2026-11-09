//! `tower phone`: turns the phone API on and off, pairs phones, lists and
//! revokes them. The API itself runs in the voice daemon (`daemon::lan`).
//! The orb's panel (its Phone view) does the same through the functions
//! below, which the command uses too.

use anyhow::{Result, bail};
use serde_json::Value;

use crate::config;
use crate::daemon::lan::{
    auth::{self, Device, Devices},
    tls,
};
use crate::protocol::{self, Request};

/// Where the phone API stands, for the panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// `[lan] enabled` in config.toml: it starts with the voice.
    pub enabled: bool,
    /// The voice daemon is running (the API lives in it).
    pub voice: bool,
    /// Where it listens (`https://ip:port`) while it's on: the first of `hosts`.
    pub url: Option<String>,
    /// Every address it listens on, in order (the LAN's, the tailnet's).
    pub hosts: Vec<Host>,
    /// Tailscale on this Mac.
    pub tailscale: Tailscale,
    /// Seconds left on the open pairing code (none open: `None`).
    pub pairing_expires_in_s: Option<u64>,
    pub devices: Vec<Device>,
}

/// One address the API listens on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Host {
    /// `lan` or `tailscale`.
    pub kind: String,
    pub host: String,
    pub port: u16,
    /// The MagicDNS name, for the tailnet's.
    pub dns_name: Option<String>,
}

impl Host {
    pub fn of(v: &Value) -> Option<Host> {
        Some(Host {
            kind: v["kind"].as_str()?.to_string(),
            host: v["host"].as_str()?.to_string(),
            port: v["port"].as_u64()? as u16,
            dns_name: v["dns_name"].as_str().map(String::from),
        })
    }

    /// `Tailscale 100.68.110.33:7843 (mac.tail1234.ts.net)`.
    pub fn line(&self) -> String {
        let kind = if self.kind == "tailscale" { "Tailscale" } else { "Local network" };
        let dns = self.dns_name.as_ref().map(|n| format!(" ({n})")).unwrap_or_default();
        format!("{kind} {}:{}{dns}", self.host, self.port)
    }
}

/// Tailscale as the phone API sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tailscale {
    pub installed: bool,
    pub running: bool,
    /// `running`, `stopped`, `needs login`, `not installed`, …
    pub state: String,
    pub ip: Option<String>,
    pub dns_name: Option<String>,
    /// `[lan] tailscale`: `auto` or `off`.
    pub mode: String,
    /// The API listens on the tailnet address now.
    pub listening: bool,
    /// Why it doesn't, though Tailscale runs.
    pub error: Option<String>,
}

impl Tailscale {
    fn of(v: &Value) -> Tailscale {
        Tailscale {
            installed: v["installed"].as_bool().unwrap_or(false),
            running: v["running"].as_bool().unwrap_or(false),
            state: v["state"].as_str().unwrap_or("not installed").to_string(),
            ip: v["ip"].as_str().map(String::from),
            dns_name: v["dns_name"].as_str().map(String::from),
            mode: v["mode"].as_str().unwrap_or("auto").to_string(),
            listening: v["listening"].as_bool().unwrap_or(false),
            error: v["error"].as_str().map(String::from),
        }
    }

    /// Asked here (the voice isn't running to ask).
    fn here() -> Tailscale {
        let mode = config::Config::load().map(|c| c.lan.tailscale).unwrap_or_else(|_| "auto".into());
        let mut v = crate::daemon::lan::tailscale::detect().view();
        v["mode"] = Value::from(if mode.trim() == "off" { "off" } else { "auto" });
        Tailscale::of(&v)
    }

    /// One line for people: what Tailscale is doing and what that means for the phone.
    pub fn line(&self, api_on: bool) -> String {
        let ip = self.ip.as_deref().map(|i| format!(" ({i}{})", self.dns_name.as_ref().map(|n| format!(", {n}")).unwrap_or_default())).unwrap_or_default();
        match (self.installed, self.running) {
            (false, _) => "Tailscale: not installed. Phones reach the tower on the local network only.".into(),
            _ if self.mode == "off" => format!("Tailscale: {}{ip}, but [lan] tailscale = \"off\": not listened on.", self.state),
            (true, false) => format!("Tailscale: {}{ip}. Phones reach the tower on the local network only; start Tailscale to reach it from anywhere.", self.state),
            (true, true) if self.listening => format!("Tailscale: running{ip}. Phones reach the tower over it too, from anywhere on the tailnet."),
            (true, true) if !api_on => format!("Tailscale: running{ip}. The phone API listens on it too once it's on."),
            (true, true) => format!(
                "Tailscale: running{ip}, but not listened on{}.",
                self.error.as_ref().map(|e| format!(" ({e}); it's tried again every 20 s")).unwrap_or_default()
            ),
        }
    }
}

pub fn status() -> Status {
    let info = daemon("status", "").ok();
    Status {
        enabled: config::Config::load().map(|c| c.lan.enabled).unwrap_or(false),
        voice: info.is_some(),
        url: info.as_ref().and_then(|v| v["url"].as_str().map(String::from)),
        hosts: info.as_ref().and_then(|v| v["hosts"].as_array().map(|a| a.iter().filter_map(Host::of).collect())).unwrap_or_default(),
        tailscale: match &info {
            Some(v) => Tailscale::of(&v["tailscale"]),
            None => Tailscale::here(),
        },
        pairing_expires_in_s: info.as_ref().and_then(|v| v["pairing_expires_in_s"].as_u64()),
        devices: Devices::load_from(&Devices::path()).unwrap_or_default().devices,
    }
}

/// Turns it on (in config.toml, and now if the voice runs): where it
/// listens and its certificate; `None` when the voice isn't running.
pub fn enable() -> Result<Option<Value>> {
    set_enabled(true)?;
    match daemon("on", "") {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.to_string().contains("not running") => Ok(None),
        Err(e) => bail!("enabled in config.toml, but it didn't start: {e:#}"),
    }
}

/// Turns it off. Paired phones stay paired.
pub fn disable() -> Result<()> {
    set_enabled(false)?;
    let _ = daemon("off", "");
    Ok(())
}

/// Opens a one-time pairing code: `code`, `host`, `port` (the first
/// address), `hosts` (every address, in order), `magic_dns`, `fingerprint`,
/// `link` (what the QR carries), `expires_in_s`, `tailscale`
/// (docs/mobile-api.md). Secret: never logged.
pub fn pair() -> Result<Value> {
    daemon("pair", "")
}

/// Forgets a paired phone (or `all`): its token stops working and its open
/// streams close. How many were revoked.
pub fn revoke(id: &str) -> Result<usize> {
    // Under the lock the daemon takes too, so a phone's `last seen` can't put it back.
    let (n, ids) = auth::update(&Devices::path(), |d| {
        let ids: Vec<String> = d.devices.iter().filter(|x| id == "all" || x.id == id).map(|x| x.id.clone()).collect();
        (d.revoke(id), ids)
    })?;
    if n == 0 {
        bail!("no paired device {id}");
    }
    for i in ids {
        let _ = daemon("revoked", &i);
    }
    Ok(n)
}

/// Where a paired phone's replies are heard: `phone` (on the phone only; the
/// Mac stays silent for what it sends) or `mac`. Kept in devices.json.
pub fn set_route(id: &str, route: &str) -> Result<()> {
    if !auth::valid_route(route) {
        bail!("where to hear replies: phone or mac, not {route:?}");
    }
    if !auth::update(&Devices::path(), |d| d.set_route(id, route))? {
        bail!("no paired device {id}");
    }
    let _ = daemon("route", id);
    Ok(())
}

pub fn cmd(action: &str, id: Option<&str>, value: Option<&str>, json: bool) -> Result<()> {
    match action {
        "enable" | "on" => {
            match enable()? {
                Some(v) => println!(
                    "Phone API on at {} (certificate {}).\nPair a phone: tower phone pair",
                    v["url"].as_str().unwrap_or("?"),
                    tls::grouped(&v["fingerprint"].as_str().unwrap_or("")[..16.min(v["fingerprint"].as_str().unwrap_or("").len())])
                ),
                None => println!("Enabled: it starts with the voice (tower up)."),
            }
            Ok(())
        }
        "disable" | "off" => {
            disable()?;
            println!("Phone API off. Paired phones stay paired (tower phone revoke all to forget them).");
            Ok(())
        }
        "pair" => {
            let v = pair()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let code = v["code"].as_str().unwrap_or("");
            let link = v["link"].as_str().unwrap_or("");
            if let Ok(q) = qrcode::QrCode::new(link.as_bytes()) {
                println!("{}", q.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build());
            }
            println!("Scan this in the Tower app, or enter:");
            let hosts: Vec<Host> = v["hosts"].as_array().map(|a| a.iter().filter_map(Host::of).collect()).unwrap_or_default();
            if hosts.len() > 1 {
                for (i, h) in hosts.iter().enumerate() {
                    println!("  {:<12} {}", if i == 0 { "address" } else { "or" }, h.line());
                }
            } else {
                println!("  address      {}:{}", v["host"].as_str().unwrap_or(""), v["port"]);
            }
            println!("  code         {} {}", &code[..4.min(code.len())], &code[4.min(code.len())..]);
            println!("  certificate  {}", tls::grouped(v["fingerprint"].as_str().unwrap_or("")));
            println!("The code works once, for {} minutes.", v["expires_in_s"].as_u64().unwrap_or(300) / 60);
            Ok(())
        }
        "devices" | "status" | "ls" => {
            let info = daemon("status", "").ok();
            let st = status();
            if json {
                let mut v = info.clone().unwrap_or_else(|| serde_json::json!({ "on": false }));
                if info.is_none() {
                    v["tailscale"] = serde_json::json!({
                        "installed": st.tailscale.installed, "running": st.tailscale.running, "state": st.tailscale.state,
                        "ip": st.tailscale.ip, "dns_name": st.tailscale.dns_name, "mode": st.tailscale.mode,
                        "listening": false, "error": null,
                    });
                    v["hosts"] = serde_json::json!([]);
                }
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            match info.as_ref().and_then(|v| v["url"].as_str()) {
                Some(url) => {
                    println!(
                        "Phone API on at {url} ({} stream(s), {} as the voice).",
                        info.as_ref().unwrap()["streams"],
                        info.as_ref().unwrap()["voice_streams"]
                    );
                    for h in &st.hosts {
                        println!("  listening on {}", h.line());
                    }
                }
                None => println!(
                    "Phone API off{}.",
                    if config::Config::load().map(|c| c.lan.enabled).unwrap_or(false) {
                        " (enabled: it starts with the voice)"
                    } else {
                        ""
                    }
                ),
            }
            println!("{}", st.tailscale.line(st.url.is_some()));
            let d = Devices::load_from(&Devices::path())?;
            if d.devices.is_empty() {
                println!("No paired phones.");
            }
            for x in &d.devices {
                println!("  {:<12} {:<24} replies on the {:<5}  last seen {}", x.id, x.name, x.audio_route, ago(x.last_seen_ms));
            }
            Ok(())
        }
        "route" => {
            let (Some(id), Some(route)) = (id, value) else { bail!("tower phone route <device id> phone|mac") };
            set_route(id, route)?;
            println!("{id}: replies to what it sends are heard on the {route}.");
            Ok(())
        }
        "revoke" => {
            let Some(id) = id else { bail!("tower phone revoke <device id|all> (ids: tower phone devices)") };
            let n = revoke(id)?;
            println!("Revoked {n} device(s): their tokens no longer work, and their open streams are closed.");
            Ok(())
        }
        other => bail!("tower phone {other}: use enable, disable, pair, devices, route or revoke"),
    }
}

fn daemon(action: &str, arg: &str) -> Result<Value> {
    if !protocol::is_running() {
        bail!("the tower's voice is not running");
    }
    let r = protocol::send(&Request::Phone { action: action.into(), arg: arg.into() })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    Ok(r.state.unwrap_or_default())
}

pub fn ago(ms: u64) -> String {
    let s = crate::history::now_ms().saturating_sub(ms) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

/// Saves how questions are handled (`question_policy` in config.toml).
pub fn set_question_policy(policy: &str) -> Result<()> {
    config::Config::write_default_if_missing()?;
    let path = config::config_file();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, with_top_level(&text, "question_policy", &format!("\"{policy}\"")))?;
    Ok(())
}

/// Where speech plays, kept in config.toml (`voice_on_mac`).
pub fn set_voice_on_mac(on: bool) -> Result<()> {
    config::Config::write_default_if_missing()?;
    let path = config::config_file();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, with_top_level(&text, "voice_on_mac", if on { "true" } else { "false" }))?;
    Ok(())
}

/// Sets a top-level `key = value` (before any `[section]`), keeping the rest.
pub fn with_top_level(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = {value}");
    let mut out: Vec<String> = vec![];
    let mut done = false;
    let mut in_table = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            if !done {
                out.push(line.clone());
                done = true;
            }
            in_table = true;
        } else if !in_table && !done && t.split('=').next().map(str::trim) == Some(key) {
            out.push(line.clone());
            done = true;
            continue;
        }
        out.push(l.to_string());
    }
    if !done {
        out.push(line);
    }
    out.join("\n") + "\n"
}

/// Sets `[lan] enabled` in config.toml, keeping the rest of the file as it is.
fn set_enabled(on: bool) -> Result<()> {
    config::Config::write_default_if_missing()?;
    let path = config::config_file();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, with_enabled(&text, on))?;
    Ok(())
}

pub fn with_enabled(text: &str, on: bool) -> String {
    let line = format!("enabled = {on}");
    let mut out: Vec<String> = vec![];
    let (mut in_lan, mut done, mut saw_lan) = (false, false, false);
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            if in_lan && !done {
                out.push(line.clone());
                done = true;
            }
            in_lan = t == "[lan]";
            saw_lan |= in_lan;
        } else if in_lan && !done && t.split('=').next().map(str::trim) == Some("enabled") {
            out.push(line.clone());
            done = true;
            continue;
        }
        out.push(l.to_string());
    }
    if in_lan && !done {
        out.push(line.clone());
        done = true;
    }
    if !saw_lan && !done {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push("[lan]".into());
        out.push(line);
    }
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::with_enabled;

    #[test]
    fn top_level_keys_stay_above_the_tables() {
        use super::with_top_level;
        assert_eq!(
            with_top_level("voice = \"x\"\n[lan]\nenabled = true\n", "question_policy", "\"keep-moving\""),
            "voice = \"x\"\nquestion_policy = \"keep-moving\"\n[lan]\nenabled = true\n"
        );
        assert_eq!(
            with_top_level("question_policy = \"normal\"\n[lan]\nquestion_policy = 1\n", "question_policy", "\"ask-everything\""),
            "question_policy = \"ask-everything\"\n[lan]\nquestion_policy = 1\n"
        );
        let t = with_top_level("", "question_policy", "\"normal\"");
        let v: toml::Value = toml::from_str(&t).unwrap();
        assert_eq!(v["question_policy"].as_str(), Some("normal"));
    }

    #[test]
    fn edits_only_the_lan_switch() {
        assert_eq!(with_enabled("voice = \"af_bella\"\n", true), "voice = \"af_bella\"\n\n[lan]\nenabled = true\n");
        let t = "voice = \"x\"\n\n[lan]\nport = 9000\nenabled = false\n\n[vad]\nratio = 3.0\n";
        assert_eq!(with_enabled(t, true), "voice = \"x\"\n\n[lan]\nport = 9000\nenabled = true\n\n[vad]\nratio = 3.0\n");
        let t = "[lan]\nport = 9000\n[vad]\nratio = 3.0\n";
        assert_eq!(with_enabled(t, true), "[lan]\nport = 9000\nenabled = true\n[vad]\nratio = 3.0\n");
        let t = "[vad]\nenabled = true\n";
        assert_eq!(
            with_enabled(t, false),
            "[vad]\nenabled = true\n\n[lan]\nenabled = false\n",
            "another section's `enabled` is left alone"
        );
        let back = with_enabled(&with_enabled("", true), false);
        let cfg: toml::Value = toml::from_str(&back).unwrap();
        assert_eq!(cfg["lan"]["enabled"].as_bool(), Some(false));
    }
}
