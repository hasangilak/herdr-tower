//! What the brain is doing right now, for the orb's working line ("Delegating
//! to orb-rust · 12s"), as Claude Code and Codex show while they work.
//!
//! The brain's own Claude Code hooks report it (`tower hook activity-prompt`,
//! `activity-pre`, `activity-post`; its Stop hook clears it), and the daemon
//! starts it when it hands the brain a request. It lives in
//! `~/.local/state/herder-tower/ui/work.json`, beside `state.json`, which the
//! orb watches.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;
use crate::history::now_ms;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Work {
    /// When this turn began (Unix ms).
    pub since_ms: u64,
    /// When the latest step began.
    pub at_ms: u64,
    /// `Thinking`, `Delegating`, `Reading`, `Waiting on`, ...
    pub word: String,
    /// What to: `to orb-rust`, `eyzflow`, `the sessions`.
    pub detail: String,
    /// When that step finished (0 while it runs). The orb keeps showing it a
    /// moment longer, so even a quick read is seen.
    pub done_ms: u64,
}

fn path() -> std::path::PathBuf {
    config::ui_dir().join("work.json")
}

pub fn read() -> Option<Work> {
    serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()
}

fn write(w: &Work) {
    let _ = std::fs::create_dir_all(config::ui_dir());
    // Written beside the watched folder, then moved in: the orb wakes once.
    let tmp = config::state_dir().join(format!(".work.json.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(w).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path());
    }
}

/// A new turn: the brain got a request.
pub fn start() {
    let now = now_ms();
    write(&Work { since_ms: now, at_ms: now, word: "Thinking".into(), detail: String::new(), done_ms: 0 });
}

/// The brain moved on to `word detail` (keeps the turn's start).
pub fn step(word: &str, detail: &str) {
    let now = now_ms();
    let since = read().filter(|w| w.since_ms > 0).map(|w| w.since_ms).unwrap_or(now);
    write(&Work { since_ms: since, at_ms: now, word: word.into(), detail: detail.into(), done_ms: 0 });
}

/// The current step finished; the brain is thinking again.
fn step_done() {
    if let Some(mut w) = read() {
        w.done_ms = now_ms();
        write(&w);
    }
}

/// The turn is over (the answer is in).
pub fn clear() {
    let _ = std::fs::remove_file(path());
}

/// A Claude Code hook, with its JSON on stdin: `prompt`, `pre` or `post`.
pub fn hook(kind: &str) {
    let mut input = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    match kind {
        "prompt" => start(),
        "pre" => {
            let (word, detail) = describe(v["tool_name"].as_str().unwrap_or(""), &v["tool_input"], &names());
            step(&word, &detail);
        }
        "post" => step_done(),
        _ => {}
    }
}

/// Sessions' names by address, from the radar the daemon publishes.
fn names() -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(config::ui_dir().join("state.json")) else { return vec![] };
    let v: Value = serde_json::from_str(&text).unwrap_or_default();
    v["agents"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| Some((t["address"].as_str()?.to_string(), t["name"].as_str().unwrap_or("").to_string()))).collect())
        .unwrap_or_default()
}

/// What a tool call is, in the tower's words: `("Delegating", "to orb-rust")`.
pub fn describe(tool: &str, input: &Value, names: &[(String, String)]) -> (String, String) {
    let name_of = |t: &str| -> String {
        let t = t.trim_matches(|c| c == '"' || c == '\'');
        names.iter().find(|(a, _)| a == t).map(|(_, n)| n.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| t.to_string())
    };
    let pair = |w: &str, d: String| (w.to_string(), d);
    match tool {
        "Bash" => {
            let cmd = input["command"].as_str().unwrap_or("");
            // The first hc / tower command in the line (it may follow a `cd` or sit in a pipe).
            let words: Vec<&str> = cmd.split_whitespace().collect();
            let at = words.iter().position(|w| matches!(*w, "hc" | "herdr-core" | "tower") || w.ends_with("/tower") || w.ends_with("/hc"));
            let Some(i) = at else { return pair("Running", "a command".into()) };
            let prog = words[i].rsplit('/').next().unwrap_or("");
            let sub = words.get(i + 1).copied().unwrap_or("");
            // The first argument that isn't a flag (the terminal, usually).
            let target = words[i + 2..].iter().find(|w| !w.starts_with('-')).copied().unwrap_or("");
            match (prog, sub) {
                ("tower", "watch") => pair("Waiting on", name_of(target)),
                ("tower", "agents") => pair("Checking", "the sessions".into()),
                ("tower", "show") => pair("Showing", name_of(target)),
                ("tower", "job" | "summary" | "log") => pair("Noting", "what happened".into()),
                ("tower", "history") => pair("Looking back", "at the history".into()),
                ("tower", _) => pair("Checking", "the tower".into()),
                (_, "send") => pair("Delegating", format!("to {}", name_of(target))),
                (_, "keys") => pair("Answering", name_of(target)),
                (_, "read" | "get") => pair("Reading", name_of(target)),
                (_, "wait") => pair("Waiting on", name_of(target)),
                (_, "interrupt") => pair("Stopping", name_of(target)),
                (_, "ls" | "events") => pair("Checking", "the sessions".into()),
                (_, "machine") => pair("Checking", "the machines".into()),
                (_, "exec") => {
                    let on = words.iter().position(|w| *w == "--on").and_then(|j| words.get(j + 1)).copied().unwrap_or("another machine");
                    pair("Checking", on.to_string())
                }
                (_, "run") => {
                    let named = words.iter().position(|w| *w == "--name").and_then(|j| words.get(j + 1)).copied();
                    pair("Starting", named.map(String::from).unwrap_or_else(|| "a session".into()))
                }
                _ => pair("Checking", "the sessions".into()),
            }
        }
        "Read" | "Grep" | "Glob" | "LS" => pair("Looking at", "files".into()),
        "Edit" | "Write" | "MultiEdit" => pair("Writing", "a file".into()),
        "WebFetch" | "WebSearch" => pair("Looking up", "the web".into()),
        "Task" | "Agent" => pair("Delegating", "to a helper".into()),
        "" => pair("Thinking", String::new()),
        other => pair("Working", other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn says_what_the_brain_does() {
        let names = vec![("t26".to_string(), "orb-rust".to_string()), ("t3".to_string(), "eyzflow".to_string())];
        let d = |cmd: &str| describe("Bash", &json!({ "command": cmd }), &names);
        assert_eq!(d("hc send t26 \"[from tower] fix it\""), ("Delegating".into(), "to orb-rust".into()));
        assert_eq!(d("hc read t3 --lines 40"), ("Reading".into(), "eyzflow".into()));
        assert_eq!(d("tower watch t26 \"fix it\""), ("Waiting on".into(), "orb-rust".into()));
        assert_eq!(d("/Users/x/.local/bin/tower watch t9 x"), ("Waiting on".into(), "t9".into()));
        assert_eq!(d("hc ls --json | jq ."), ("Checking".into(), "the sessions".into()));
        assert_eq!(d("hc run --bg --name b2c-dashboard --cwd ~/x claude \"go\""), ("Starting".into(), "b2c-dashboard".into()));
        assert_eq!(d("hc exec --on vps -- hc ls"), ("Checking".into(), "vps".into()));
        assert_eq!(d("git status"), ("Running".into(), "a command".into()));
        assert_eq!(describe("Read", &json!({}), &names), ("Looking at".into(), "files".into()));
    }
}
