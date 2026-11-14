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
    let prompt = config::brain_dir().join("CLAUDE.md");
    let before = std::fs::read_to_string(&prompt).ok();
    // (Each space's own brain that has a folder gets its instructions and hooks written again too.)
    brain::write_files()?;
    println!("✓ {}", config::brain_dir().display());
    let own = crate::brains::cards();
    if !own.is_empty() {
        println!("✓ {} ({} space brain folder(s))", config::brains_dir().display(), own.len());
    }
    // A brain that runs has the old instructions in mind: it reads the new ones.
    if before.is_some() && before != std::fs::read_to_string(&prompt).ok() {
        let told: Vec<String> = crate::daemon::brains::reread("the tower was updated and your instructions changed", None).into_iter().filter_map(|h| h.join().ok().flatten()).collect();
        if !told.is_empty() {
            println!("✓ told to read its instructions again: {}", told.join(", "));
        }
    }

    // 3. The orb has the tower's keys now; take the old mod's out of Herdr.
    let _ = keys;
    tidy_keys()?;

    // 4. Every agent session's hooks, so the tower sees what each one does.
    agent_hooks("install")?;
    Ok(())
}

/// The agent events the tower follows, per program (Claude Code's and
/// Codex's hook names). Tool calls are left out: far too many.
const CLAUDE_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "Notification", "PermissionRequest", "Stop", "StopFailure", "SessionEnd", "PreToolUse"];
/// PreToolUse is hooked for browser tools only (`crate::browsers`): no other call pays for it.
const BROWSER_TOOLS: &str = "mcp__claude-in-chrome__.*|mcp__.*Claude_Browser.*|mcp__playwright.*|mcp__puppeteer.*|mcp__chrome-devtools.*|mcp__agent-browser.*|mcp__.*browser.*";
const CODEX_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "PermissionRequest", "Stop"];

/// Where each program keeps its hooks.
fn hook_files() -> Vec<(&'static str, PathBuf, &'static [&'static str])> {
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| config::home().join(".claude"));
    let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| config::home().join(".codex"));
    vec![("claude", claude.join("settings.json"), CLAUDE_EVENTS), ("codex", codex.join("hooks.json"), CODEX_EVENTS)]
}

/// `tower hooks install|remove|status`: the tower's hook in every agent
/// session's settings (Claude Code's `~/.claude/settings.json`, Codex's
/// `~/.codex/hooks.json`), beside whatever else is there. A file is backed up
/// before its first change.
pub fn agent_hooks(action: &str) -> Result<()> {
    let tower = config::tower_bin();
    for (app, path, events) in hook_files() {
        let command = hook_command(&tower.display().to_string(), app);
        // Codex not installed: nothing to hook.
        if app == "codex" && !path.parent().is_some_and(|d| d.exists()) {
            if action == "status" {
                println!("- codex: not installed");
            }
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let before: Value = if text.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?
        };
        let after = match action {
            "install" => with_hooks(&before, events, &command),
            "remove" | "uninstall" => without_hooks(&before),
            "status" => {
                let missing = missing_events(&before, events);
                if missing.is_empty() {
                    println!("✓ {app}: the tower follows every session ({})", path.display());
                } else {
                    println!("✗ {app}: not following {} ({}): run `tower hooks install`", missing.join(", "), path.display());
                }
                continue;
            }
            other => anyhow::bail!("tower hooks {other}: use install, remove or status"),
        };
        if after == before {
            println!("✓ {app}: hooks {} ({})", if action == "install" { "in place" } else { "already out" }, path.display());
            continue;
        }
        if path.exists() {
            let backup = path.with_extension("json.tower-backup");
            if !backup.exists() {
                std::fs::copy(&path, &backup).with_context(|| format!("backing up {}", path.display()))?;
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tower-tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&after)? + "\n")?;
        std::fs::rename(&tmp, &path)?;
        println!("✓ {app}: hooks {} ({})", if action == "install" { "installed" } else { "removed" }, path.display());
    }
    Ok(())
}

fn hook_command(tower: &str, app: &str) -> String {
    format!("'{tower}' hook agent {app}")
}

/// One of the tower's own hook entries.
fn is_ours(entry: &Value) -> bool {
    entry["hooks"].as_array().is_some_and(|hs| hs.iter().any(|h| h["command"].as_str().is_some_and(|c| c.contains(" hook agent "))))
}

/// The settings with the tower's hook on each event (once; an older one,
/// say from another install path, is replaced).
fn with_hooks(settings: &Value, events: &[&str], command: &str) -> Value {
    let mut v = settings.clone();
    if !v.is_object() {
        v = serde_json::json!({});
    }
    if !v["hooks"].is_object() {
        v["hooks"] = serde_json::json!({});
    }
    for event in events {
        let list = v["hooks"][*event].as_array().cloned().unwrap_or_default();
        let mut list: Vec<Value> = list.into_iter().filter(|e| !is_ours(e)).collect();
        let mut entry = serde_json::json!({ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] });
        if *event == "PreToolUse" {
            entry["matcher"] = serde_json::json!(BROWSER_TOOLS);
        }
        list.push(entry);
        v["hooks"][*event] = Value::Array(list);
    }
    v
}

/// The settings without any of the tower's agent hooks.
fn without_hooks(settings: &Value) -> Value {
    let mut v = settings.clone();
    if let Some(hooks) = v.get_mut("hooks").and_then(Value::as_object_mut) {
        for (_, list) in hooks.iter_mut() {
            if let Some(a) = list.as_array_mut() {
                a.retain(|e| !is_ours(e));
            }
        }
        hooks.retain(|_, list| list.as_array().is_none_or(|a| !a.is_empty()));
    }
    v
}

fn missing_events(settings: &Value, events: &[&str]) -> Vec<String> {
    events
        .iter()
        .filter(|e| !settings["hooks"][**e].as_array().is_some_and(|a| a.iter().any(is_ours)))
        .map(|e| e.to_string())
        .collect()
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
    // What the brains run on, and each space's own.
    let known = crate::brains::known_from_herdr();
    let model = crate::brains::report(&cfg, &crate::daemon::brains::running_of(&known));
    println!("  model: {}", model["says"].as_str().unwrap_or(""));
    let own: Vec<String> = known.iter().filter(|k| k.live).filter_map(|k| k.space.as_ref().map(|s| format!("{} for {s} ({})", k.name, k.word()))).collect();
    println!(
        "  spaces' own brains: {}",
        match (cfg.brains.per_space, own.is_empty()) {
            (false, _) => "off ([brains] per_space = false): one brain for everything".to_string(),
            (true, true) => "none running (each open space gets one with its first session)".to_string(),
            (true, false) => own.join(", "),
        }
    );
    let machines = hc::machines();
    println!("  machines: {}", if machines.is_empty() { "none added (hc machine add <ssh host>)".into() } else { machines.join(", ") });
    // Whether the tower sees every agent session (their hooks).
    agent_hooks("status")?;
    Ok(())
}

fn which(name: &str) -> bool {
    Command::new("sh").args(["-lc", &format!("command -v {name}")]).output().is_ok_and(|o| o.status.success())
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_go_in_beside_others_once_and_come_out_clean() {
        let others = serde_json::json!({
            "model": "opus",
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "rtk hook claude" }] }] }
        });
        let cmd = hook_command("/Users/me/.local/bin/tower", "claude");
        let once = with_hooks(&others, CLAUDE_EVENTS, &cmd);
        let twice = with_hooks(&once, CLAUDE_EVENTS, &cmd);
        assert_eq!(once, twice, "installing again changes nothing");
        assert_eq!(once["hooks"]["Stop"].as_array().unwrap().len(), 2, "the other Stop hook stays");
        assert_eq!(once["model"], "opus");
        assert!(missing_events(&once, CLAUDE_EVENTS).is_empty());
        assert_eq!(missing_events(&others, &["Stop"]), vec!["Stop"]);
        // Another install path replaces the old entry.
        let moved = with_hooks(&once, CLAUDE_EVENTS, &hook_command("/opt/tower", "claude"));
        assert_eq!(moved["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        let out = without_hooks(&moved);
        assert_eq!(out, others, "removing leaves exactly what was there");
    }
}
