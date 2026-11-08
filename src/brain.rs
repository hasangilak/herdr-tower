//! The brain: a Claude Code or Codex session in a Herdr terminal named
//! `tower`, with the tower's instructions, driving every other session
//! through `hc`.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;

use crate::config::{self, Config};
use crate::hc;

pub const NAME: &str = "tower";
pub const VOICE_NAME: &str = "tower-voice";
pub const PROMPT: &str = include_str!("../assets/tower-prompt.md");

/// The brain's terminal: `tower`, or `HERDER_TOWER_BRAIN` (a second tower
/// beside the first, for trying changes out).
pub fn name() -> String {
    std::env::var("HERDER_TOWER_BRAIN").ok().filter(|n| !n.is_empty()).unwrap_or_else(|| NAME.into())
}

/// Writes the brain's folder: its instructions (CLAUDE.md for Claude Code,
/// AGENTS.md for Codex) and Claude Code's hooks and permissions.
pub fn write_files() -> Result<()> {
    let dir = config::brain_dir();
    std::fs::create_dir_all(dir.join(".claude"))?;
    std::fs::write(dir.join("CLAUDE.md"), PROMPT)?;
    std::fs::write(dir.join("AGENTS.md"), PROMPT)?;
    let tower = config::tower_bin();
    let tower = tower.display();
    let settings = json!({
        "hooks": {
            "Stop": [{ "hooks": [{ "type": "command", "command": format!("'{tower}' hook claude") }] }],
            "Notification": [{ "hooks": [{ "type": "command", "command": format!("'{tower}' hook claude-notify") }] }],
            // What the brain is doing, for the orb's working line.
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": format!("'{tower}' hook activity-prompt") }] }],
            "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": format!("'{tower}' hook activity-pre") }] }],
            "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": format!("'{tower}' hook activity-post") }] }],
        },
        "permissions": {
            "allow": [
                "Bash(hc *)", "Bash(herdr-core *)", "Bash(tower *)", "Bash(herdr-app *)",
                format!("Bash({tower} *)"),
            ],
        },
    });
    std::fs::write(dir.join(".claude/settings.json"), serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

/// Starts the brain unless it runs. Returns its address.
pub fn launch(cfg: &Config, which: &str) -> Result<String> {
    if let Some(t) = hc::get(NAME).filter(|t| t["state"] == "live") {
        return Ok(t["address"].as_str().unwrap_or(NAME).to_string());
    }
    write_files()?;
    // A finished brain keeps its name until removed.
    let _ = hc::run(&["rm", NAME]);
    let dir = config::brain_dir();
    let dir_s = dir.display().to_string();
    let tower = config::tower_bin().display().to_string();
    let mut argv: Vec<String> = match which {
        "claude" => {
            let mut a = vec!["claude".into(), "-n".into(), "tower".into(), "--append-system-prompt".into(), PROMPT.into()];
            // Every step the brain takes is silence for the user: these
            // matter more for a voice than for a terminal.
            if !cfg.claude_model.is_empty() {
                a.extend(["--model".into(), cfg.claude_model.clone()]);
            }
            if !cfg.claude_effort.is_empty() {
                a.extend(["--effort".into(), cfg.claude_effort.clone()]);
            }
            a.extend(cfg.claude_args.iter().cloned());
            a
        }
        "codex" => {
            let mut a = vec![
                "codex".into(),
                "-C".into(),
                dir_s.clone(),
                "-c".into(),
                format!("notify=[\"{tower}\", \"hook\", \"codex\"]"),
                // hc talks to herdr-core over a Unix socket outside the workspace.
                "--sandbox".into(),
                "danger-full-access".into(),
                "--ask-for-approval".into(),
                "on-request".into(),
            ];
            if !cfg.codex_model.is_empty() {
                a.extend(["-m".into(), cfg.codex_model.clone()]);
            }
            if !cfg.codex_effort.is_empty() {
                a.extend(["-c".into(), format!("model_reasoning_effort=\"{}\"", cfg.codex_effort)]);
            }
            if !cfg.codex_service_tier.is_empty() {
                a.extend(["-c".into(), format!("service_tier=\"{}\"", cfg.codex_service_tier)]);
            }
            a.extend(cfg.codex_args.iter().cloned());
            a
        }
        other => bail!("unknown brain {other:?}: use claude or codex"),
    };
    let mut args: Vec<&str> = vec!["run", "--bg", "--json", "--name", NAME, "--no-notify", "--no-parent", "--cwd", &dir_s, "--"];
    let argv_refs: Vec<&str> = argv.iter_mut().map(|s| s.as_str()).collect();
    args.extend(argv_refs);
    let out = hc::run(&args)?;
    if out.code != 0 {
        bail!("starting the brain: {}{}", out.stderr.trim(), out.stdout.trim());
    }
    let v: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap_or_default();
    let address = v["address"].as_str().unwrap_or(NAME).to_string();
    settle(&address);
    Ok(address)
}

/// Gets past a first-run folder trust question, which is ours to answer:
/// the folder is the tower's own.
fn settle(address: &str) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(25) {
        thread::sleep(Duration::from_millis(700));
        let Some(t) = hc::get(address) else { return };
        if t["state"] != "live" {
            return;
        }
        let screen = hc::run(&["read", address, "--lines", "0"]).map(|o| o.stdout).unwrap_or_default();
        if let Some(keys) = trust_answer(&screen) {
            let mut args = vec!["keys", address];
            args.extend(keys);
            let _ = hc::run(&args);
            continue;
        }
        if matches!(hc::status_word(&t).as_deref(), Some("idle") | Some("working")) {
            return;
        }
    }
}

/// The keys that answer "yes" to a folder-trust question on screen, if one
/// is showing: Claude Code's ("Yes, I trust this folder", sometimes below a
/// highlighted "No, exit") and Codex's.
fn trust_answer(screen: &str) -> Option<Vec<&'static str>> {
    let lower = screen.to_lowercase();
    if !lower.contains("trust") {
        return None;
    }
    let lines: Vec<&str> = screen.lines().collect();
    let yes = lines.iter().position(|l| {
        let l = l.to_lowercase();
        l.contains("yes") && (l.contains("trust") || l.contains("proceed") || l.contains("allow"))
    })?;
    let cursor =
        lines.iter().position(|l| l.trim_start().starts_with('❯') || l.trim_start().starts_with('›') || l.trim_start().starts_with('>'));
    match cursor {
        Some(c) if c < yes => Some(std::iter::repeat_n("down", yes - c).chain(["enter"]).collect()),
        Some(c) if c > yes => Some(std::iter::repeat_n("up", c - yes).chain(["enter"]).collect()),
        _ => Some(vec!["enter"]),
    }
}

/// Hands what the user said to the brain, with the radar's view of every
/// agent (`radar`, may be empty), so a status question needs no lookup.
/// `source`: `mac`, `voice` or `phone:<device id>`, for the chat.
pub fn ask(text: &str, radar: &str, source: &str) -> Result<()> {
    let msg = if radar.is_empty() { format!("[voice] {text}") } else { format!("[voice] {text} {radar}") };
    note(&msg)?;
    set_question_turn(false);
    crate::activity::start();
    let (text, source) = (text.to_string(), source.to_string());
    crate::history::later(move || crate::history::record_request(&text, &source));
    Ok(())
}

/// The brain's turn is a watch reporting back (not a reply to the user):
/// set as a `[watch]` note goes in, and by the transcript as it starts.
static WATCH_TURN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_watch_turn(on: bool) {
    WATCH_TURN.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn watch_turn() -> bool {
    WATCH_TURN.load(std::sync::atomic::Ordering::SeqCst)
}

/// The brain's turn is a `[question]` note (it recommends answers with
/// `tower recommend`): nothing it writes is spoken or kept in the chat.
static QUESTION_TURN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_question_turn(on: bool) {
    QUESTION_TURN.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn question_turn() -> bool {
    QUESTION_TURN.load(std::sync::atomic::Ordering::SeqCst)
}

/// A turn's prompt is a `[question]` note (not something the user said).
pub fn is_question_prompt(prompt: &str) -> bool {
    prompt.contains("[question]") && !prompt.contains("[voice]")
}

/// A message from the tower itself (a watch firing).
pub fn note(text: &str) -> Result<()> {
    let name = name();
    let Some(t) = hc::get(&name).filter(|t| t["state"] == "live") else {
        bail!("brain-absent");
    };
    let status = hc::status_word(&t);
    if status.as_deref() == Some("blocked") {
        bail!("brain-blocked");
    }
    hc::send(&name, text)?;
    if let Some(report) = text.strip_prefix("[watch] ") {
        let report = report.to_string();
        crate::history::later(move || crate::history::record("watch", &report));
    }
    // A busy brain takes it after its turn; the transcript says when.
    if status.as_deref() != Some("working") {
        set_watch_turn(text.starts_with("[watch]"));
        set_question_turn(text.starts_with("[question]"));
    }
    Ok(())
}

pub fn spoken_error(msg: &str) -> String {
    if msg.contains("brain-absent") {
        "The tower isn't running. Start it with tower up.".into()
    } else if msg.contains("brain-blocked") {
        "I'm waiting on a permission question on my own screen. Have a look at the tower terminal.".into()
    } else {
        "I couldn't pass that on. The error is on screen.".into()
    }
}

/// Starts the voice daemon in a Herdr terminal (so it lives on after this
/// command, and macOS asks once for the microphone on Herdr Core's behalf).
pub fn launch_voice() -> Result<()> {
    if crate::protocol::is_running() {
        return Ok(());
    }
    if hc::is_live(VOICE_NAME) {
        let _ = hc::run(&["kill", VOICE_NAME]);
        let _ = hc::run(&["rm", VOICE_NAME]);
    }
    let tower = config::tower_bin().display().to_string();
    let home = config::home().display().to_string();
    let out =
        hc::run(&["run", "--bg", "--json", "--name", VOICE_NAME, "--no-notify", "--no-parent", "--cwd", &home, "--", &tower, "daemon"])?;
    if out.code != 0 {
        bail!("starting the voice: {}{}", out.stderr.trim(), out.stdout.trim());
    }
    // Models load before the socket opens: Whisper and Kokoro take a few seconds.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(600) {
        if crate::protocol::is_running() {
            return Ok(());
        }
        if !hc::is_live(VOICE_NAME) {
            let screen = hc::run(&["read", VOICE_NAME, "--lines", "40"]).map(|o| o.stdout).unwrap_or_default();
            bail!("the voice stopped while starting:\n{}", screen.trim());
        }
        thread::sleep(Duration::from_millis(300));
    }
    bail!("the voice took more than ten minutes to start (downloading models?); see `hc read {VOICE_NAME}`")
}

pub fn stop(name: &str) -> bool {
    if hc::get(name).is_some() {
        let _ = hc::run(&["kill", name]);
        let _ = hc::run(&["rm", name]);
        true
    } else {
        false
    }
}

pub fn brain_dir_exists() -> bool {
    Path::new(&config::brain_dir()).join("CLAUDE.md").exists()
}

#[cfg(test)]
mod tests {
    use super::trust_answer;

    #[test]
    fn answers_claude_trust() {
        let screen = " Quick safety check: Is this a project you created or one you trust?\n ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm";
        assert_eq!(trust_answer(screen), Some(vec!["down", "enter"]));
        let screen = " Do you trust the files in this folder?\n ❯ 1. Yes, proceed\n   2. No, exit";
        assert_eq!(trust_answer(screen), Some(vec!["enter"]));
        assert_eq!(trust_answer("Welcome to Claude Code"), None);
    }
}
