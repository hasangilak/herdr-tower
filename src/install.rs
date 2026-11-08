//! `tower install` and `tower doctor`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::brain;
use crate::config::{self, Config};
use crate::hc;

/// Keys earlier versions added to Herdr's keys.json (section "global"), for
/// the old Herdr mod. The orb (`tower orb`) has them now, from any app:
/// ⌘⇧K shows or hides it, ⌘⇧Space talks, ⌘⇧Esc hushes.
const OLD_KEYS: &[(&str, &str)] =
    &[("cmd-shift-k", "mod.open:tower"), ("cmd-shift-space", "tower.talk"), ("cmd-shift-escape", "tower.hush")];

/// Herdr's settings folder.
fn herdr_app_dir() -> PathBuf {
    std::env::var_os("HERDR_APP_HOME").map(PathBuf::from).unwrap_or_else(|| config::home().join(".config/herdr-app"))
}

pub fn install(keys: bool) -> Result<()> {
    // 1. The binary, at a stable path for hooks and the orb.
    let bin = config::home().join(".local/bin/tower");
    let me = std::env::current_exe()?;
    if me.canonicalize().ok() != bin.canonicalize().ok() {
        std::fs::create_dir_all(bin.parent().unwrap())?;
        let tmp = bin.with_extension("new");
        std::fs::copy(&me, &tmp).with_context(|| format!("copying to {}", tmp.display()))?;
        std::fs::rename(&tmp, &bin)?;
        // A copied binary keeps its ad-hoc signature only if re-signed on macOS.
        let _ = Command::new("codesign").args(["--force", "--sign", "-"]).arg(&bin).output();
    }
    println!("✓ {}", bin.display());

    // 2. Settings and the brain's folder.
    if Config::write_default_if_missing()? {
        println!("✓ {} (defaults)", config::config_file().display());
    } else {
        println!("✓ {} (kept)", config::config_file().display());
    }
    brain::write_files()?;
    println!("✓ {}", config::brain_dir().display());

    // 3. The orb has the tower's keys now; take the old mod's out of Herdr.
    let _ = keys;
    tidy_keys()?;
    Ok(())
}

/// Takes the keys earlier versions added for the old mod out of Herdr's
/// keys.json (only where they still say what the tower put there).
fn tidy_keys() -> Result<()> {
    let path = herdr_app_dir().join("keys.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(()) };
    let mut v: Value = serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    let Some(global) = v.get_mut("global").and_then(Value::as_object_mut) else { return Ok(()) };
    let mut removed = vec![];
    for (key, action) in OLD_KEYS {
        if global.get(*key).and_then(Value::as_str) == Some(*action) {
            global.remove(*key);
            removed.push(*key);
        }
    }
    if !removed.is_empty() {
        std::fs::write(&path, serde_json::to_string_pretty(&v)? + "\n")?;
        println!("✓ keys.json: took out {} (the orb has them now)", removed.join(", "));
    }
    Ok(())
}

pub fn doctor() -> Result<()> {
    let cfg = Config::load()?;
    let check = |ok: bool, what: String| println!("{} {what}", if ok { "✓" } else { "✗" });
    let hc_bin = config::hc_bin();
    check(hc_bin.exists(), format!("hc at {}", hc_bin.display()));
    let server = hc::json(&["server", "status"]);
    check(server.is_ok(), format!("herdr-core {}", server.as_ref().map(|v| v["version"].to_string()).unwrap_or_else(|e| e.to_string())));
    check(cfg.whisper_model.exists(), format!("Whisper model {}", cfg.whisper_model.display()));
    let claude = which("claude");
    let codex = which("codex");
    check(claude || codex, format!("agents: claude {}, codex {}", yes(claude), yes(codex)));
    check(config::home().join(".local/bin/tower").exists(), "tower installed in ~/.local/bin".into());
    let orb = Command::new("pgrep").args(["-f", "tower orb"]).output().is_ok_and(|o| o.status.success());
    check(orb, format!("orb {}", if orb { "running" } else { "not running (tower up, or tower orb)" }));
    check(brain::brain_dir_exists(), format!("brain folder {}", config::brain_dir().display()));
    let voice = crate::protocol::is_running();
    check(voice, format!("voice daemon {}", if voice { "running" } else { "not running (tower up)" }));
    let b = hc::get(brain::NAME).filter(|t| t["state"] == "live");
    check(b.is_some(), format!("brain {}", b.as_ref().and_then(hc::status_word).unwrap_or_else(|| "not running".into())));
    let machines = hc::machines();
    println!("  machines: {}", if machines.is_empty() { "none added (hc machine add <ssh host>)".into() } else { machines.join(", ") });
    Ok(())
}

fn which(name: &str) -> bool {
    Command::new("sh").args(["-lc", &format!("command -v {name}")]).output().is_ok_and(|o| o.status.success())
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}
