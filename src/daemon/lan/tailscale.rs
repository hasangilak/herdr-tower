//! Tailscale on this Mac, as the phone API needs it: is it there, is it
//! running, and this Mac's tailnet IPv4 (100.64.0.0/10) and MagicDNS name.
//!
//! Asked through the `tailscale` CLI: the one on the PATH (Homebrew, or the
//! `/usr/local/bin/tailscale` shim the app installs), else the standalone
//! app's own binary (`/Applications/Tailscale.app/Contents/MacOS/Tailscale`,
//! which answers the same commands). `HERDER_TOWER_TAILSCALE` names another.
//! `tailscale status --json` says it all; `tailscale ip -4` stands in for the
//! address when the status has none.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

/// The standalone app's binary (it is the CLI too).
pub const APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
/// How long one CLI call may take.
const CALL: Duration = Duration::from_secs(4);

/// What Tailscale is doing on this Mac.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Tailscale {
    /// The CLI that answered (None: not installed, as far as the tower can tell).
    pub cli: Option<String>,
    /// Its `BackendState`: `Running`, `Stopped`, `NeedsLogin`, `Starting`, `NoState`, …
    /// ("" when it couldn't be asked).
    pub backend: String,
    /// Connected to the tailnet (`BackendState` is `Running`).
    pub running: bool,
    /// This Mac's tailnet IPv4 (in 100.64.0.0/10). Kept by Tailscale while
    /// stopped too, but only usable while it runs.
    pub ipv4: Option<Ipv4Addr>,
    /// Its MagicDNS name (`mac.tailnet-name.ts.net`, no trailing dot).
    pub dns_name: Option<String>,
    /// Why it couldn't be asked, when it couldn't.
    pub error: Option<String>,
}

impl Tailscale {
    pub fn installed(&self) -> bool {
        self.cli.is_some()
    }

    /// One word for people: `not installed`, `running`, `stopped`, `needs login`, …
    pub fn word(&self) -> String {
        if !self.installed() {
            return "not installed".into();
        }
        if self.running {
            return "running".into();
        }
        match self.backend.as_str() {
            "" => "not answering".into(),
            "Stopped" => "stopped".into(),
            "NeedsLogin" => "needs login".into(),
            "NeedsMachineAuth" => "waiting for approval".into(),
            "Starting" => "starting".into(),
            "NoState" => "not started".into(),
            other => other.to_lowercase(),
        }
    }

    /// As `tower phone status`, the panel, `state.json` and the pairing payload show it.
    pub fn view(&self) -> Value {
        json!({
            "installed": self.installed(),
            "running": self.running,
            "state": self.word(),
            "ip": self.ipv4.map(|i| i.to_string()),
            "dns_name": self.dns_name,
        })
    }
}

/// An address on a tailnet: 100.64.0.0/10 (carrier-grade NAT space, which Tailscale uses).
pub fn is_tailnet(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

/// Where to look for the CLI, in order: `HERDER_TOWER_TAILSCALE`, each PATH
/// folder, Homebrew's and the app's shim, the app's own binary.
pub fn candidates(env: Option<&str>, path: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    if let Some(e) = env.filter(|e| !e.is_empty()) {
        out.push(PathBuf::from(e));
    }
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        out.push(Path::new(dir).join("tailscale"));
    }
    for p in ["/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale", APP_CLI] {
        let p = PathBuf::from(p);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The first CLI that is there.
pub fn find_cli() -> Option<PathBuf> {
    let env = std::env::var("HERDER_TOWER_TAILSCALE").ok();
    let path = std::env::var("PATH").unwrap_or_default();
    candidates(env.as_deref(), &path).into_iter().find(|p| executable(p))
}

/// What `tailscale status --json` says (the `cli` is the caller's to fill in).
pub fn parse_status(text: &str) -> Result<Tailscale, String> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|e| format!("tailscale status --json: {e}"))?;
    if !v.is_object() {
        return Err("tailscale status --json: not an object".into());
    }
    let backend = v["BackendState"].as_str().unwrap_or("").to_string();
    let v4 = |list: &Value| -> Option<Ipv4Addr> {
        list.as_array()?.iter().filter_map(|x| x.as_str()?.parse::<Ipv4Addr>().ok()).find(|ip| is_tailnet(*ip))
    };
    let ipv4 = v4(&v["TailscaleIPs"]).or_else(|| v4(&v["Self"]["TailscaleIPs"]));
    let dns_name = v["Self"]["DNSName"].as_str().map(|n| n.trim_end_matches('.').to_string()).filter(|n| !n.is_empty());
    Ok(Tailscale { cli: None, running: backend == "Running", backend, ipv4, dns_name, error: None })
}

/// What `tailscale ip -4` printed: its first tailnet IPv4.
pub fn parse_ip4(text: &str) -> Option<Ipv4Addr> {
    text.lines().filter_map(|l| l.trim().parse::<Ipv4Addr>().ok()).find(|ip| is_tailnet(*ip))
}

fn call(cli: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(cli);
    cmd.args(args);
    let out = crate::hc::output_within(cmd, "tailscale", CALL).map_err(|e| format!("{e:#}"))?;
    if out.code != 0 && out.stdout.trim().is_empty() {
        return Err(format!("tailscale {}: {}", args.join(" "), out.stderr.trim()));
    }
    Ok(out.stdout)
}

/// Asks Tailscale now (a few milliseconds when it's there; nothing runs when it isn't).
pub fn detect() -> Tailscale {
    let Some(cli) = find_cli() else { return Tailscale::default() };
    let shown = cli.to_string_lossy().into_owned();
    let mut t = match call(&cli, &["status", "--json"]).and_then(|s| parse_status(&s)) {
        Ok(t) => t,
        Err(e) => Tailscale { error: Some(e), ..Default::default() },
    };
    t.cli = Some(shown);
    if t.ipv4.is_none() && t.error.is_none() {
        t.ipv4 = call(&cli, &["ip", "-4"]).ok().as_deref().and_then(parse_ip4);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = r#"{
        "Version": "1.104.5", "BackendState": "Running",
        "TailscaleIPs": ["100.68.110.33", "fd7a:115c:a1e0::804d:6e22"],
        "Self": {"DNSName": "hassans-macbook-pro.tailaff7aa.ts.net.", "HostName": "Hassan’s MacBook Pro", "Online": true,
                 "TailscaleIPs": ["100.68.110.33", "fd7a:115c:a1e0::804d:6e22"]},
        "MagicDNSSuffix": "tailaff7aa.ts.net", "Peer": {}
    }"#;

    #[test]
    fn a_running_tailnet_gives_its_address_and_name() {
        let t = parse_status(RUNNING).unwrap();
        assert!(t.running);
        assert_eq!(t.backend, "Running");
        assert_eq!(t.ipv4, Some(Ipv4Addr::new(100, 68, 110, 33)));
        assert_eq!(t.dns_name.as_deref(), Some("hassans-macbook-pro.tailaff7aa.ts.net"));
    }

    #[test]
    fn stopped_and_logged_out_are_not_running() {
        // As this Mac's Tailscale 1.104.5 says it while stopped: the address is kept.
        let stopped = RUNNING.replace("\"Running\"", "\"Stopped\"").replace("\"Online\": true", "\"Online\": false");
        let t = Tailscale { cli: Some("tailscale".into()), ..parse_status(&stopped).unwrap() };
        assert!(!t.running);
        assert_eq!(t.ipv4, Some(Ipv4Addr::new(100, 68, 110, 33)));
        assert_eq!(t.word(), "stopped");
        let t = Tailscale { cli: Some("x".into()), ..parse_status(r#"{"BackendState":"NeedsLogin","TailscaleIPs":null,"Self":{}}"#).unwrap() };
        assert_eq!((t.running, t.ipv4, t.dns_name.clone(), t.word().as_str()), (false, None, None, "needs login"));
        assert_eq!(Tailscale::default().word(), "not installed");
        assert!(parse_status("not json").is_err());
        assert!(parse_status("[1,2]").is_err());
    }

    #[test]
    fn only_tailnet_addresses_count() {
        // The top-level list missing: Self's; addresses outside 100.64/10 are not the tailnet's.
        let t = parse_status(r#"{"BackendState":"Running","Self":{"TailscaleIPs":["192.168.1.4","100.101.2.3"]}}"#).unwrap();
        assert_eq!(t.ipv4, Some(Ipv4Addr::new(100, 101, 2, 3)));
        assert_eq!(parse_ip4("100.68.110.33\n"), Some(Ipv4Addr::new(100, 68, 110, 33)));
        assert_eq!(parse_ip4("fd7a::1\n100.200.1.1\n"), None, "100.200 is outside 100.64/10");
        assert_eq!(parse_ip4(""), None);
        assert!(is_tailnet(Ipv4Addr::new(100, 64, 0, 1)) && is_tailnet(Ipv4Addr::new(100, 127, 255, 254)));
        assert!(!is_tailnet(Ipv4Addr::new(100, 63, 0, 1)) && !is_tailnet(Ipv4Addr::new(100, 128, 0, 1)));
    }

    #[test]
    fn the_cli_is_looked_for_on_the_path_then_in_the_app() {
        let c = candidates(Some("/x/ts"), "/usr/bin:/opt/homebrew/bin");
        assert_eq!(c[0], PathBuf::from("/x/ts"));
        assert_eq!(c[1], PathBuf::from("/usr/bin/tailscale"));
        assert_eq!(c[2], PathBuf::from("/opt/homebrew/bin/tailscale"));
        assert_eq!(c.last().unwrap(), &PathBuf::from(APP_CLI), "the standalone app's binary is the last resort");
        assert_eq!(c.iter().filter(|p| p.as_path() == Path::new("/opt/homebrew/bin/tailscale")).count(), 1);
        assert_eq!(candidates(None, "")[0], PathBuf::from("/opt/homebrew/bin/tailscale"));
    }

    #[test]
    fn the_view_is_what_the_app_reads() {
        let t = Tailscale { cli: Some("tailscale".into()), ..parse_status(RUNNING).unwrap() };
        assert_eq!(
            t.view(),
            json!({ "installed": true, "running": true, "state": "running", "ip": "100.68.110.33", "dns_name": "hassans-macbook-pro.tailaff7aa.ts.net" })
        );
    }
}
