//! `tower phone`: turns the phone API on and off, pairs phones, lists and
//! revokes them. The API itself runs in the voice daemon (`daemon::lan`).

use anyhow::{Result, bail};
use serde_json::Value;

use crate::config;
use crate::daemon::lan::{auth::Devices, tls};
use crate::protocol::{self, Request};

pub fn cmd(action: &str, id: Option<&str>, json: bool) -> Result<()> {
    match action {
        "enable" | "on" => {
            set_enabled(true)?;
            match daemon("on", "") {
                Ok(v) => println!(
                    "Phone API on at {} (certificate {}).\nPair a phone: tower phone pair",
                    v["url"].as_str().unwrap_or("?"),
                    tls::grouped(&v["fingerprint"].as_str().unwrap_or("")[..16.min(v["fingerprint"].as_str().unwrap_or("").len())])
                ),
                Err(e) if e.to_string().contains("not running") => println!("Enabled: it starts with the voice (tower up)."),
                Err(e) => bail!("enabled in config.toml, but it didn't start: {e:#}"),
            }
            Ok(())
        }
        "disable" | "off" => {
            set_enabled(false)?;
            let _ = daemon("off", "");
            println!("Phone API off. Paired phones stay paired (tower phone revoke all to forget them).");
            Ok(())
        }
        "pair" => {
            let v = daemon("pair", "")?;
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
            println!("  address      {}:{}", v["host"].as_str().unwrap_or(""), v["port"]);
            println!("  code         {} {}", &code[..4.min(code.len())], &code[4.min(code.len())..]);
            println!("  certificate  {}", tls::grouped(v["fingerprint"].as_str().unwrap_or("")));
            println!("The code works once, for {} minutes.", v["expires_in_s"].as_u64().unwrap_or(300) / 60);
            Ok(())
        }
        "devices" | "status" | "ls" => {
            let info = daemon("status", "").ok();
            match info.as_ref().and_then(|v| v["url"].as_str()) {
                Some(url) => println!(
                    "Phone API on at {url} ({} stream(s), {} as the voice).",
                    info.as_ref().unwrap()["streams"],
                    info.as_ref().unwrap()["voice_streams"]
                ),
                None => println!(
                    "Phone API off{}.",
                    if config::Config::load().map(|c| c.lan.enabled).unwrap_or(false) {
                        " (enabled: it starts with the voice)"
                    } else {
                        ""
                    }
                ),
            }
            let d = Devices::load_from(&Devices::path());
            if d.devices.is_empty() {
                println!("No paired phones.");
            }
            for x in &d.devices {
                println!("  {:<12} {:<24} last seen {}", x.id, x.name, ago(x.last_seen_ms));
            }
            Ok(())
        }
        "revoke" => {
            let Some(id) = id else { bail!("tower phone revoke <device id|all> (ids: tower phone devices)") };
            let mut d = Devices::load_from(&Devices::path());
            let ids: Vec<String> = d.devices.iter().filter(|x| id == "all" || x.id == id).map(|x| x.id.clone()).collect();
            let n = d.revoke(id);
            if n == 0 {
                bail!("no paired device {id}");
            }
            d.save_to(&Devices::path())?;
            for i in ids {
                let _ = daemon("revoked", &i);
            }
            println!("Revoked {n} device(s): their tokens no longer work, and their open streams are closed.");
            Ok(())
        }
        other => bail!("tower phone {other}: use enable, disable, pair, devices or revoke"),
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

fn ago(ms: u64) -> String {
    let s = crate::history::now_ms().saturating_sub(ms) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
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
