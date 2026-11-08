//! Hooks the brain calls when it finishes a turn: they read its answer and
//! hand it to the daemon to speak. They never fail the agent.

use std::io::Read;

use serde_json::Value;

use crate::protocol::{self, Request};

/// Claude Code's Stop hook: JSON on stdin with the transcript's path.
pub fn claude_stop() {
    // The turn is over: the orb's working line gives way to the answer.
    crate::activity::clear();
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    let text = v["last_assistant_message"]
        .as_str()
        .map(String::from)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| v["transcript_path"].as_str().and_then(last_assistant_text));
    match text {
        Some(text) => say(&text, "answer"),
        // Nothing to say, but the turn is over all the same.
        None => {
            let _ = protocol::send(&Request::Say { text: String::new(), interrupt: false, kind: "answer".into() });
        }
    }
}

/// Claude Code's Notification hook: say permission questions out loud.
pub fn claude_notify() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    let msg = v["message"].as_str().unwrap_or("");
    let lower = msg.to_lowercase();
    if lower.contains("permission") || lower.contains("approve") || lower.contains("confirm") {
        say(&format!("I need your go-ahead: {}.", msg.trim_end_matches('.').replace("Claude needs", "I need")), "alert");
    }
}

/// Codex's `notify` program: the event is the last argument.
pub fn codex(arg: Option<String>) {
    crate::activity::clear();
    let v: Value = arg.as_deref().and_then(|a| serde_json::from_str(a).ok()).unwrap_or_default();
    if v["type"] == "agent-turn-complete"
        && let Some(text) = v["last-assistant-message"].as_str()
    {
        say(text, "answer");
    }
}

fn say(text: &str, kind: &str) {
    if text.trim().is_empty() {
        return;
    }
    crate::history::record(kind, text);
    let _ = protocol::send(&Request::Say { text: text.to_string(), interrupt: false, kind: kind.into() });
}

/// The last text the assistant wrote in a Claude Code transcript (JSONL).
fn last_assistant_text(path: &str) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    let mut last = None;
    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v["type"].as_str() {
            Some("assistant") => {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        if b["type"] == "text"
                            && let Some(t) = b["text"].as_str().filter(|t| !t.trim().is_empty())
                        {
                            last = Some(t.to_string());
                        }
                    }
                }
            }
            Some("user") => {
                // A new prompt (not a tool result) starts a new answer.
                let c = &v["message"]["content"];
                let is_prompt = c.is_string() || c.as_array().is_some_and(|a| a.iter().any(|b| b["type"] == "text"));
                if is_prompt {
                    last = None;
                }
            }
            _ => {}
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_last_answer() {
        let dir = std::env::temp_dir().join(format!("tower-hook-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"status?"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Checking."},{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Two sessions are working."}]}}"#,
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        assert_eq!(last_assistant_text(p.to_str().unwrap()).as_deref(), Some("Two sessions are working."));
    }
}
