//! herdr-core (`hc`), run as a program.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config;

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn run(args: &[&str]) -> Result<Output> {
    let out = Command::new(config::hc_bin())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running hc {}", args.join(" ")))?;
    Ok(Output {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Runs hc with `--json` and parses what it prints.
pub fn json(args: &[&str]) -> Result<Value> {
    let mut all: Vec<&str> = args.to_vec();
    all.push("--json");
    let out = run(&all)?;
    let value: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
    if out.code != 0 && value.is_null() {
        bail!("hc {} failed: {}{}", args.join(" "), out.stderr.trim(), out.stdout.trim());
    }
    Ok(value)
}

/// One terminal's facts, or None when there is no such live terminal.
pub fn get(terminal: &str) -> Option<Value> {
    let v = json(&["get", terminal]).ok()?;
    if v.get("address").is_some() { Some(v) } else { None }
}

pub fn is_live(terminal: &str) -> bool {
    get(terminal).is_some_and(|t| t["state"] == "live")
}

/// The agent status word a terminal reports (`idle`, `working`, `blocked`,
/// `done`, `error`), or None for a plain program.
pub fn status_word(t: &Value) -> Option<String> {
    match &t["status"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("state").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

/// The program that reported the status (`claude-code`, `codex`, ...).
pub fn agent_app(t: &Value) -> Option<String> {
    t["status"].get("app").and_then(Value::as_str).map(String::from)
}

/// What to call a terminal out loud: its name, else its folder.
pub fn spoken_name(t: &Value) -> String {
    if let Some(n) = t["name"].as_str().filter(|n| !n.is_empty()) {
        return n.replace(['-', '_'], " ");
    }
    let cwd = t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str()).unwrap_or("");
    let folder = cwd.rsplit('/').find(|s| !s.is_empty()).unwrap_or("a terminal");
    let app = match agent_app(t).as_deref() {
        Some("claude-code") => "Claude",
        Some("codex") => "Codex",
        Some(other) if !other.is_empty() => return format!("{} in {}", other, folder.replace(['-', '_'], " ")),
        _ => "the agent",
    };
    format!("{app} in {}", folder.replace(['-', '_'], " "))
}

/// The machines added with `hc machine add`.
pub fn machines() -> Vec<String> {
    let Ok(v) = json(&["machine", "ls"]) else { return vec![] };
    v["machines"].as_array().map(|a| a.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect()).unwrap_or_default()
}

/// The live terminals on one machine (`None`: this one), each with its full
/// address (`vps/t3` for another machine).
pub fn list_on(machine: Option<&str>) -> Result<Vec<Value>> {
    let v = match machine {
        None => json(&["ls"])?,
        Some(m) => {
            let out = run(&["exec", "--on", m, "--", "hc", "ls", "--json"])?;
            if out.code != 0 {
                bail!("{m}: {}", out.stderr.trim());
            }
            serde_json::from_str(out.stdout.trim()).with_context(|| format!("{m}: hc ls printed {}", out.stdout.trim()))?
        }
    };
    let mut list = v["terminals"].as_array().cloned().unwrap_or_default();
    if let Some(m) = machine {
        for t in &mut list {
            if let Some(a) = t["address"].as_str() {
                t["address"] = Value::String(format!("{m}/{a}"));
            }
            t["machine"] = Value::String(m.to_string());
        }
    }
    Ok(list)
}

/// Pastes text into a terminal and presses Enter.
pub fn send(terminal: &str, text: &str) -> Result<()> {
    let out = run(&["send", "--json", terminal, text])?;
    if out.code != 0 {
        let msg = if out.stderr.trim().is_empty() { out.stdout.trim() } else { out.stderr.trim() };
        bail!("{msg}");
    }
    Ok(())
}
