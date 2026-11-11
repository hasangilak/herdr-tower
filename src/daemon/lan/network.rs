//! Which network this Mac is on, in words a person recognises, and what each
//! paired phone knows of where the tower is.
//!
//! macOS keeps the Wi-Fi's name (SSID) from apps without location access,
//! so a network is told by this Mac's address on it: the /24 of the LAN
//! listener's IPv4 (`10.10.30` for 10.10.30.116), or the address itself for
//! IPv6. The user can name one once (`[lan.networks]` in config.toml:
//! `"10.10.30" = "Office"`, `tower phone network name "Office"`); unnamed it
//! reads "the network 10.10.30.x".
//!
//! A phone only knows the addresses it was given (the pairing, the event
//! stream's `hosts`) or connected to. Each device keeps the networks it was
//! paired on or connected from (`Known`); `reach` says from those whether it
//! can reach the tower where the Mac is now.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::auth::Device;

/// The range a phone's networks entry has for the tailnet.
pub const TAILNET: &str = "tailnet";

/// What both windows say, once, when the phones in use can't reach the tower here.
pub const HINT: &str = "Phone: pair again on this network";

/// A phone "in use" was connected within this long (ms): only those count for the hint.
pub const RECENT_MS: u64 = 24 * 3_600_000;

/// At most this many networks are kept per phone (the ones seen longest ago go).
const MAX_KNOWN: usize = 12;

/// The network an address is on: the /24 of an IPv4 (`10.10.30`), an IPv6 itself.
pub fn range_of(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            format!("{}.{}.{}", o[0], o[1], o[2])
        }
        IpAddr::V6(v) => v.to_string(),
    }
}

/// How a network reads to a person: its name, else `the network 10.10.30.x`
/// (an IPv6 one by its address; the tailnet's is `Tailscale`).
pub fn label(range: &str, name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None if range == TAILNET => "Tailscale".into(),
        None if range.contains(':') => format!("the network {range}"),
        None => format!("the network {range}.x"),
    }
}

/// A name for a network as the user typed it: no control characters, at most 40 characters.
pub fn clean_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(40).collect::<String>().trim().to_string()
}

/// What the network asks of a phone to pair, in one line.
pub fn requirement(network: &str) -> String {
    format!("Your phone must be on the same Wi-Fi as this Mac ({network}), or both on Tailscale.")
}

/// `a`, `a and b`, `a, b and c`.
pub fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The networks a phone knows, the latest first: `Office, the network 192.168.2.x and Tailscale`.
pub fn knows_line(known: &[Known]) -> String {
    let mut k: Vec<&Known> = known.iter().collect();
    k.sort_by_key(|x| std::cmp::Reverse(x.last_ms));
    listed(&k.iter().map(|x| x.label()).collect::<Vec<_>>())
}

/// The network a phone was last on (paired or connected), as it reads.
pub fn last_on(known: &[Known]) -> Option<String> {
    known.iter().max_by_key(|k| k.last_ms).map(Known::label)
}

/// Where the tower can be reached now: the LAN listener (the network it is
/// on, and its name), and the tailnet's address while that is listened on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Here {
    /// The LAN listener's network (`10.10.30`); none without a LAN address.
    pub range: Option<String>,
    /// Its name, when the user gave it one.
    pub name: Option<String>,
    /// The LAN listener: host and port.
    pub lan: Option<(String, u16)>,
    /// The tailnet listener (Tailscale runs and is listened on).
    pub tailnet: Option<(String, u16)>,
}

impl Here {
    /// "Office", or "the network 10.10.30.x"; none without a LAN address.
    pub fn label(&self) -> Option<String> {
        self.range.as_deref().map(|r| label(r, self.name.as_deref()))
    }

    /// `network` in `state.json`, `GET /v1/network` and the event stream:
    /// `{"range", "name", "lan": {"host", "port"} | null}`, or null without a LAN address.
    pub fn view(&self) -> Value {
        match &self.range {
            None => Value::Null,
            Some(range) => json!({
                "range": range,
                "name": self.name,
                "lan": self.lan.as_ref().map(|(h, p)| json!({ "host": h, "port": p })),
            }),
        }
    }

    /// From `network` and the address list (`lan_hosts`, or `hosts`) as the state and `info` have them.
    pub fn of(network: &Value, hosts: &Value) -> Here {
        let addr = |v: &Value| Some((v["host"].as_str()?.to_string(), v["port"].as_u64()? as u16));
        Here {
            range: network["range"].as_str().map(String::from),
            name: network["name"].as_str().map(String::from),
            lan: addr(&network["lan"]),
            tailnet: hosts.as_array().and_then(|a| a.iter().find(|h| h["kind"] == "tailscale")).and_then(addr),
        }
    }
}

/// A network a phone was paired on or connected from: its range, its name
/// then, the tower's address there, and when it was first and last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Known {
    /// `10.10.30`, an IPv6 address, or `tailnet`.
    pub range: String,
    /// What the user called it when the phone was last there.
    #[serde(default)]
    pub name: Option<String>,
    pub host: String,
    pub port: u16,
    pub first_ms: u64,
    pub last_ms: u64,
}

impl Known {
    /// "Office", "the network 192.168.2.x", "Tailscale".
    pub fn label(&self) -> String {
        label(&self.range, self.name.as_deref())
    }
}

/// Notes that a phone knows the tower at `host:port` on `range`: the entry
/// for that range is brought up to date (or added), and the list kept short.
pub fn saw(known: &mut Vec<Known>, range: &str, name: Option<&str>, host: &str, port: u16, now_ms: u64) {
    match known.iter_mut().find(|k| k.range == range) {
        Some(k) => {
            k.host = host.to_string();
            k.port = port;
            if name.is_some() || range == TAILNET {
                k.name = name.map(String::from);
            }
            k.last_ms = k.last_ms.max(now_ms);
        }
        None => known.push(Known {
            range: range.into(),
            name: name.map(String::from),
            host: host.into(),
            port,
            first_ms: now_ms,
            last_ms: now_ms,
        }),
    }
    while known.len() > MAX_KNOWN {
        let oldest = known.iter().enumerate().min_by_key(|(_, k)| k.last_ms).map(|(i, _)| i).unwrap_or(0);
        known.remove(oldest);
    }
}

/// Whether a phone can reach the tower where the Mac is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// It knows the LAN address the Mac has now.
    Lan,
    /// Tailscale is listened on, and it knows the tailnet's address.
    Tailnet,
    /// It knows neither: paired on another network.
    Elsewhere,
    /// Nothing on file (paired by a tower that didn't keep networks, and not connected since).
    Unknown,
    /// The tower listens nowhere (the phone API is off).
    Off,
}

impl Reach {
    /// The verdict in a line, as Settings and `tower phone devices` say it.
    pub fn line(&self) -> &'static str {
        match self {
            Reach::Lan | Reach::Tailnet => "Can reach the tower here",
            Reach::Elsewhere => "Paired on another network: pair it again here, or use Tailscale",
            Reach::Unknown => "Not connected since the tower began keeping networks: it shows once it connects",
            Reach::Off => "The phone API is off",
        }
    }

    /// `here`, `elsewhere`, `unknown`, `off` (`tower phone devices --json`).
    pub fn word(&self) -> &'static str {
        match self {
            Reach::Lan | Reach::Tailnet => "here",
            Reach::Elsewhere => "elsewhere",
            Reach::Unknown => "unknown",
            Reach::Off => "off",
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, Reach::Lan | Reach::Tailnet)
    }
}

/// What a phone with these networks can do where the Mac is now.
pub fn reach(known: &[Known], here: &Here) -> Reach {
    if let Some((host, port)) = &here.lan
        && known.iter().any(|k| k.range != TAILNET && &k.host == host && k.port == *port)
    {
        return Reach::Lan;
    }
    // The tailnet's address rarely changes, and the MagicDNS name stands in when it does.
    if here.tailnet.is_some() && known.iter().any(|k| k.range == TAILNET) {
        return Reach::Tailnet;
    }
    if here.lan.is_none() && here.tailnet.is_none() {
        return Reach::Off;
    }
    if known.is_empty() { Reach::Unknown } else { Reach::Elsewhere }
}

/// Whether to say `HINT`: the tower listens on a LAN, Tailscale isn't
/// running, at least one phone was connected in the last day, and every
/// such phone is known to have been paired elsewhere (none knows the address
/// here; one with nothing on file keeps it unsaid). It goes once a phone
/// pairs or connects here, or Tailscale runs.
pub fn hint(devices: &[Device], here: &Here, tailscale_running: bool, now_ms: u64) -> bool {
    if tailscale_running || here.lan.is_none() {
        return false;
    }
    let recent: Vec<&Device> = devices.iter().filter(|d| now_ms.saturating_sub(d.last_seen_ms) <= RECENT_MS).collect();
    !recent.is_empty() && recent.iter().all(|d| reach(&d.networks, here) == Reach::Elsewhere)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here(host: &str, port: u16, tailnet: bool) -> Here {
        let ip: IpAddr = host.parse().unwrap();
        Here {
            range: Some(range_of(ip)),
            name: None,
            lan: Some((host.into(), port)),
            tailnet: tailnet.then(|| ("100.68.110.33".to_string(), port)),
        }
    }

    fn known(range: &str, host: &str) -> Known {
        Known { range: range.into(), name: None, host: host.into(), port: 7843, first_ms: 1, last_ms: 1 }
    }

    #[test]
    fn a_network_is_its_range_and_reads_by_its_name() {
        assert_eq!(range_of("10.10.30.116".parse().unwrap()), "10.10.30");
        assert_eq!(range_of("192.168.2.211".parse().unwrap()), "192.168.2");
        assert_eq!(range_of("fd00::1".parse().unwrap()), "fd00::1");
        assert_eq!(label("10.10.30", None), "the network 10.10.30.x");
        assert_eq!(label("10.10.30", Some("  ")), "the network 10.10.30.x", "a blank name is no name");
        assert_eq!(label("10.10.30", Some("Office")), "Office");
        assert_eq!(label("fd00::1", None), "the network fd00::1");
        assert_eq!(label(TAILNET, None), "Tailscale");
        assert_eq!(clean_name("  Home\u{7}  "), "Home");
        assert_eq!(clean_name(&"x".repeat(80)).len(), 40);
        let h = Here { name: Some("Office".into()), ..here("10.10.30.116", 7843, false) };
        assert_eq!(h.label().as_deref(), Some("Office"));
        assert_eq!(h.view(), json!({"range": "10.10.30", "name": "Office", "lan": {"host": "10.10.30.116", "port": 7843}}));
        assert_eq!(Here::default().view(), Value::Null, "no LAN address: no network");
        // Read back from the state as the windows see it.
        let hosts =
            json!([{"kind": "lan", "host": "10.10.30.116", "port": 7843}, {"kind": "tailscale", "host": "100.68.110.33", "port": 7843}]);
        let back = Here::of(&h.view(), &hosts);
        assert_eq!(back, Here { tailnet: Some(("100.68.110.33".into(), 7843)), ..h });
    }

    #[test]
    fn a_phone_keeps_each_network_once_and_up_to_date() {
        let mut k = vec![];
        saw(&mut k, "192.168.2", Some("Home"), "192.168.2.211", 7843, 10);
        saw(&mut k, "10.10.30", None, "10.10.30.116", 7843, 20);
        saw(&mut k, "192.168.2", None, "192.168.2.40", 7843, 30);
        assert_eq!(k.len(), 2);
        assert_eq!(
            (k[0].host.as_str(), k[0].name.as_deref(), k[0].first_ms, k[0].last_ms),
            ("192.168.2.40", Some("Home"), 10, 30),
            "a name is kept when none is given"
        );
        for i in 0..20u64 {
            saw(&mut k, &format!("10.0.{i}"), None, &format!("10.0.{i}.2"), 7843, 100 + i);
        }
        assert_eq!(k.len(), MAX_KNOWN);
        assert!(k.iter().all(|x| x.range != "192.168.2"), "the ones seen longest ago go first");
    }

    #[test]
    fn the_verdict_follows_what_the_phone_knows() {
        let office = here("10.10.30.116", 7843, false);
        assert_eq!(reach(&[known("10.10.30", "10.10.30.116")], &office), Reach::Lan);
        assert_eq!(reach(&[known("192.168.2", "192.168.2.211")], &office), Reach::Elsewhere);
        // The same network, another address (the lease changed): the phone doesn't know it yet.
        assert_eq!(reach(&[known("10.10.30", "10.10.30.90")], &office), Reach::Elsewhere);
        // The tailnet: only while it is listened on.
        let tail = [known("192.168.2", "192.168.2.211"), known(TAILNET, "100.68.110.33")];
        assert_eq!(reach(&tail, &office), Reach::Elsewhere);
        assert_eq!(reach(&tail, &here("10.10.30.116", 7843, true)), Reach::Tailnet);
        assert_eq!(reach(&[], &office), Reach::Unknown);
        assert_eq!(reach(&tail, &Here::default()), Reach::Off);
        assert!(Reach::Lan.ok() && Reach::Tailnet.ok() && !Reach::Elsewhere.ok());
        assert_eq!(Reach::Elsewhere.line(), "Paired on another network: pair it again here, or use Tailscale");
    }

    #[test]
    fn the_hint_shows_only_when_the_phones_in_use_cant_reach_the_tower_here() {
        let now = 100 * RECENT_MS;
        let dev = |known: Vec<Known>, seen: u64| Device {
            id: "d_1".into(),
            name: "Pixel".into(),
            token_sha256: String::new(),
            created_ms: 1,
            last_seen_ms: seen,
            audio_route: "phone".into(),
            install_id: None,
            networks: known,
        };
        let office = here("10.10.30.116", 7843, false);
        let home = || vec![known("192.168.2", "192.168.2.211")];
        // At the office, the phone used today knows only home: say it.
        assert!(hint(&[dev(home(), now - 3_600_000)], &office, false, now));
        // Tailscale runs: not said.
        assert!(!hint(&[dev(home(), now - 3_600_000)], &office, true, now));
        // A phone that knows this address (it paired or connected here): not said.
        assert!(!hint(&[dev(home(), now), dev(vec![known("10.10.30", "10.10.30.116")], now)], &office, false, now));
        // No phone used in the last day: nothing to say.
        assert!(!hint(&[dev(home(), now - 2 * RECENT_MS)], &office, false, now));
        assert!(!hint(&[], &office, false, now));
        // A phone with nothing on file can't be judged: not said.
        assert!(!hint(&[dev(vec![], now)], &office, false, now));
        // No LAN listener (the API off, or the tailnet only): not said.
        assert!(!hint(&[dev(home(), now)], &Here::default(), false, now));
    }
}
