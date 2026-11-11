//! Hooks. The brain's (it finishes a turn: its answer is read and handed to
//! the daemon to speak), and every agent session's (`tower hook agent`:
//! what it is doing, told to the daemon so the tower sees it at once). They
//! never fail the agent, never print, and never make it wait.

use std::io::Read;

use serde_json::Value;

use crate::protocol::{self, Request};

/// Whose hook this is: the main brain's, or a space's own (its hooks name
/// it: `tower hook claude tower-<space>`).
#[derive(Debug, Clone, Default, PartialEq)]
struct Who {
    /// A space's brain, by its terminal's name (None: the main brain).
    brain: Option<String>,
    /// Its space, as its folder says.
    space: Option<String>,
}

impl Who {
    fn of(brain: Option<&str>) -> Who {
        match brain.map(str::trim).filter(|b| crate::brains::is_space_brain(b)) {
            Some(b) => Who { brain: Some(b.to_string()), space: crate::brains::card_of(b).map(|c| c.space) },
            None => Who::default(),
        }
    }

    fn name(&self) -> Option<String> {
        self.brain.clone()
    }
}

/// Claude Code's Stop hook: JSON on stdin with the transcript's path.
/// `brain`: the space's brain whose hook it is (None: the main brain's).
pub fn claude_stop(brain: Option<&str>) {
    let who = Who::of(brain);
    // The turn is over: the orb's working line (the main brain's) gives way to the answer.
    if who.brain.is_none() {
        crate::activity::clear();
    }
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    // A turn that answered a `[question]` note (recommendations): nothing to say or keep.
    let prompt = v["transcript_path"].as_str().and_then(last_prompt);
    if prompt.as_deref().is_some_and(crate::brain::is_question_prompt) {
        let _ = protocol::send(&Request::Say { text: String::new(), interrupt: false, kind: "answer".into(), from: None, brain: who.name() });
        return;
    }
    let text = v["last_assistant_message"]
        .as_str()
        .map(String::from)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| v["transcript_path"].as_str().and_then(last_assistant_text));
    match text {
        Some(text) => finish(&text, prompt.as_deref(), &who),
        // Nothing to say, but the turn is over all the same.
        None => {
            let _ = protocol::send(&Request::Say { text: String::new(), interrupt: false, kind: "answer".into(), from: None, brain: who.name() });
        }
    }
}

/// What the brain's words at the end of a turn are.
#[derive(Debug, PartialEq)]
enum Said {
    /// Its answer to the user: a chat message, spoken.
    Answer,
    /// A note turn at a quiet level: kept for the record, about this
    /// session (None: no one session). Not a chat message, not spoken.
    Note(Option<String>),
}

/// How a turn that began with `prompt` ends at `level` (see `tell`): a
/// note from the tower itself is answered for the record unless the tower
/// tells everything; what the user asked is always answered to them.
fn said_as(prompt: Option<&str>, level: crate::tell::Level) -> Said {
    match prompt {
        Some(p) if level.quiet() && crate::brain::is_note_prompt(p) => Said::Note(crate::brain::note_session(p)),
        _ => Said::Answer,
    }
}

/// A brain's turn is over with `text`, after `prompt`. A space's own
/// brain's answer is kept as that space's, and said like the main one's.
fn finish(text: &str, prompt: Option<&str>, who: &Who) {
    match said_as(prompt, crate::tell::level()) {
        Said::Answer => say(text, "answer", who),
        Said::Note(session) => {
            crate::history::record_note(session.as_deref(), text, who.space.clone());
            // The turn is over all the same.
            let _ = protocol::send(&Request::Say { text: String::new(), interrupt: false, kind: "answer".into(), from: None, brain: who.name() });
        }
    }
}

/// What a brain's permission question is said as: the main brain's in its
/// own voice, a space's by its space.
fn go_ahead(msg: &str, who: &Who) -> String {
    let msg = msg.trim_end_matches('.');
    match (&who.brain, &who.space) {
        (None, _) => format!("I need your go-ahead: {}.", msg.replace("Claude needs", "I need")),
        (Some(_), Some(space)) => format!("The {space} tower needs your go-ahead: {}.", msg.replace("Claude needs", "it needs")),
        (Some(b), None) => format!("{} needs your go-ahead: {}.", b.replace('-', " "), msg.replace("Claude needs", "it needs")),
    }
}

/// Claude Code's Notification hook: say permission questions out loud.
pub fn claude_notify(brain: Option<&str>) {
    let who = Who::of(brain);
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    let msg = v["message"].as_str().unwrap_or("");
    let lower = msg.to_lowercase();
    if lower.contains("permission") || lower.contains("approve") || lower.contains("confirm") {
        say(&go_ahead(msg, &who), "alert", &who);
    }
}

/// Codex's `notify` program: the event is the last argument; a space's own
/// brain's name comes before it.
pub fn codex(args: &[String]) {
    let (brain, event) = match args {
        [event] => (None, Some(event)),
        [brain, event, ..] => (Some(brain.as_str()), Some(event)),
        [] => (None, None),
    };
    let who = Who::of(brain);
    if who.brain.is_none() {
        crate::activity::clear();
    }
    let v: Value = event.and_then(|a| serde_json::from_str(a).ok()).unwrap_or_default();
    if v["type"] == "agent-turn-complete"
        && let Some(text) = v["last-assistant-message"].as_str()
    {
        // What the turn was given, when Codex says: its latest input.
        let prompt = v["input-messages"].as_array().and_then(|a| a.last()).and_then(Value::as_str);
        finish(text, prompt, &who);
    }
}

fn say(text: &str, kind: &str, who: &Who) {
    if text.trim().is_empty() {
        return;
    }
    // The text without its screenshot markers; the screenshots beside it.
    crate::history::record_said(kind, text, who.space.clone());
    let _ = protocol::send(&Request::Say { text: text.to_string(), interrupt: false, kind: kind.into(), from: None, brain: who.name() });
}

/// Every agent session's hooks (Claude Code's and Codex's, installed by
/// `tower hooks install`): JSON on stdin, `app` is `claude` or `codex`. The
/// event goes to the daemon in a moment, or waits in a spool file for it.
pub fn agent(app: &str) {
    let mut input = String::new();
    let _ = std::io::stdin().take(4 << 20).read_to_string(&mut input);
    // Outside Herdr there's no terminal to tie it to; the brain has its own hooks.
    let terminal = std::env::var("HC_TERMINAL_ID").unwrap_or_default();
    if terminal.is_empty() {
        return;
    }
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    let Some(ev) = agent_event(app, &terminal, &v) else { return };
    if is_brain_dir(&ev.cwd) {
        return;
    }
    let req = Request::Agent { event: ev.clone() };
    // No daemon, or one too old to take it: kept for the next start.
    if !protocol::send_within(&req, std::time::Duration::from_millis(1500)).is_ok_and(|r| r.ok) {
        spool(&ev);
    }
}

/// An agent hook's input as the tower's event (None: one it doesn't follow).
pub fn agent_event(app: &str, terminal: &str, v: &Value) -> Option<crate::daemon::sessions::AgentEvent> {
    let event = match v["hook_event_name"].as_str().unwrap_or("") {
        "SessionStart" => "start",
        "UserPromptSubmit" => "prompt",
        "PermissionRequest" => "permission",
        "Notification" => "notify",
        "Stop" => "stop",
        "StopFailure" => "stop-failed",
        "SessionEnd" => "end",
        _ => return None,
    };
    let text = match event {
        "prompt" => v["prompt"].as_str().map(String::from),
        "stop" | "stop-failed" => v["last_assistant_message"]
            .as_str()
            .map(String::from)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| v["error"].as_str().map(String::from))
            .or_else(|| v["transcript_path"].as_str().filter(|_| app == "claude").and_then(last_assistant_text)),
        "notify" => v["message"].as_str().map(String::from),
        "permission" => {
            let tool = v["tool_name"].as_str().unwrap_or("a tool");
            let what = v["tool_input"]["command"].as_str().or_else(|| v["tool_input"]["file_path"].as_str()).unwrap_or("");
            Some(if what.is_empty() { format!("permission to use {tool}") } else { format!("permission to use {tool}: {what}") })
        }
        "end" => v["reason"].as_str().map(String::from),
        _ => None,
    };
    Some(crate::daemon::sessions::AgentEvent {
        app: if app == "codex" { "codex".into() } else { "claude-code".into() },
        event: event.into(),
        terminal: terminal.into(),
        session_id: v["session_id"].as_str().unwrap_or("").into(),
        transcript: v["transcript_path"].as_str().unwrap_or("").into(),
        cwd: v["cwd"].as_str().unwrap_or("").into(),
        text: text.unwrap_or_default(),
        at_ms: crate::history::now_ms(),
        pid: agent_pid(),
    })
}

/// The agent that ran this hook: the parent process, past the shell it
/// runs hook commands in (`claude` ← `/bin/sh` ← this).
fn agent_pid() -> u32 {
    let parent = std::os::unix::process::parent_id();
    match parent_of(parent) {
        Some((grand, name)) if ["sh", "bash", "zsh", "dash"].contains(&name.as_str()) => grand,
        _ => parent,
    }
}

/// A process's parent and its name.
#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<(u32, String)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a plain C struct of exactly `size` bytes.
    let n = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    if n != size {
        return None;
    }
    let name: Vec<u8> = info.pbi_comm.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    Some((info.pbi_ppid, String::from_utf8_lossy(&name).into_owned()))
}

#[cfg(not(target_os = "macos"))]
fn parent_of(_pid: u32) -> Option<(u32, String)> {
    None
}

/// The brain's PreToolUse hook on Bash: an instruction for one session
/// (`tower queue` or `hc send`) that bundles tasks for different projects is
/// turned back, with how to split it. `--whole` says it really is one task.
pub fn brain_guard() {
    let mut input = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    let command = v["tool_input"]["command"].as_str().unwrap_or("");
    let Some(text) = handoff_text(command) else { return };
    let projects = crate::dispatch::known_projects();
    if let Some(reason) = guard_reason(&text, &projects) {
        let out = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        });
        println!("{out}");
    }
}

/// The instruction a hand-off command types, when it is one (and isn't marked `--whole`).
fn handoff_text(command: &str) -> Option<String> {
    for part in command.split(['\n', ';']).flat_map(|p| p.split("&&")) {
        let words = shell_words(part.trim());
        let w: Vec<&str> = words.iter().map(String::as_str).collect();
        let rest = match w.as_slice() {
            ["tower", "queue", s, rest @ ..] if !["list", "cancel", "retarget"].contains(s) => rest,
            ["hc", "send", rest @ ..] => {
                // Options, then the terminal, then the text.
                let mut r = rest;
                while let [first, more @ ..] = r {
                    if first.starts_with("--") {
                        r = if *first == "--if" || *first == "--timeout" { more.get(1..).unwrap_or(&[]) } else { more };
                    } else {
                        break;
                    }
                }
                r.get(1..).unwrap_or(&[])
            }
            _ => continue,
        };
        if rest.contains(&"--whole") {
            return None;
        }
        let mut text: Vec<&str> = vec![];
        let mut i = 0;
        while i < rest.len() {
            match rest[i] {
                "--after" => i += 1,
                "--" => {}
                w => text.push(w),
            }
            i += 1;
        }
        let text = text.join(" ");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

/// Why the hand-off should be split, if it should.
fn guard_reason(text: &str, projects: &[crate::split::Project]) -> Option<String> {
    let parts = crate::split::separate(text, projects)?;
    let names: Vec<String> = parts
        .iter()
        .map(|(folder, part)| {
            let name = projects.iter().find(|p| &p.folder == folder).map(|p| p.name().to_string()).unwrap_or_else(|| folder.clone());
            format!("{name}: \"{part}\"")
        })
        .collect();
    Some(format!(
        "This instruction holds separate tasks for different projects ({}). Give each project its own session: `tower dispatch --json '{{\"tasks\":[...]}}'` (one task per project; `after` for one that must wait), or queue each part to the session in that project's folder. If it really is one task for this session, run it again with --whole after the session.",
        names.join("; ")
    ))
}

/// A command line's words, with simple quoting ('…', "…", \).
fn shell_words(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (Some(q), c) if c == q => quote = None,
            (Some('"') | None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (_, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A brain's own folder (the main one's, or a space's): it has its own hooks.
fn is_brain_dir(cwd: &str) -> bool {
    crate::brains::is_brain_folder(cwd)
}

/// No daemon to take it: kept for its next start (a bounded file).
fn spool(ev: &crate::daemon::sessions::AgentEvent) {
    use std::io::Write;
    let path = crate::daemon::sessions::spool_path();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 << 20) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let (Ok(mut f), Ok(line)) = (std::fs::OpenOptions::new().create(true).append(true).open(&path), serde_json::to_string(ev)) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

/// The prompt that started the last turn in a Claude Code transcript.
fn last_prompt(path: &str) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    let mut last = None;
    for line in data.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["type"] != "user" || v["isMeta"] == true {
            continue;
        }
        let c = &v["message"]["content"];
        let text = c.as_str().map(String::from).or_else(|| {
            c.as_array().and_then(|a| a.iter().find(|b| b["type"] == "text")).and_then(|b| b["text"].as_str().map(String::from))
        });
        if text.is_some() {
            last = text;
        }
    }
    last
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
    use serde_json::json;

    #[test]
    fn the_agent_is_found_past_the_shell() {
        // This test's parent is cargo (no shell between): the parent itself.
        assert_eq!(agent_pid(), std::os::unix::process::parent_id());
        let (ppid, name) = parent_of(std::process::id()).unwrap();
        assert_eq!(ppid, std::os::unix::process::parent_id());
        assert!(!name.is_empty());
    }

    #[test]
    fn the_guard_reads_hand_offs() {
        assert_eq!(handoff_text(r#"tower queue t5 "fix the login""#).as_deref(), Some("fix the login"));
        assert_eq!(handoff_text(r#"tower say "Sending."; hc send --if idle t5 'run the tests'"#).as_deref(), Some("run the tests"));
        assert_eq!(handoff_text(r#"tower queue t5 --after 3 "then deploy""#).as_deref(), Some("then deploy"));
        assert_eq!(handoff_text(r#"tower queue t5 --whole "a. also b.""#), None);
        assert_eq!(handoff_text("tower queue list"), None);
        assert_eq!(handoff_text("hc read t5 --lines 40"), None);
    }

    #[test]
    fn the_guard_turns_back_two_projects_in_one() {
        let projects: Vec<crate::split::Project> =
            ["/w/eyzflow", "/w/herder-tower"].iter().filter_map(|f| crate::split::Project::of_folder(f)).collect();
        let r = guard_reason("Fix the login in eyzflow. Also add a mute button to herder tower.", &projects).unwrap();
        assert!(r.contains("eyzflow: ") && r.contains("herder tower: ") && r.contains("tower dispatch"), "{r}");
        assert!(guard_reason("Fix the login in eyzflow and add a test for it.", &projects).is_none());
    }

    #[test]
    fn a_note_turn_ends_in_the_record_and_the_users_turn_in_an_answer() {
        use crate::tell::Level::*;
        let watch = "[watch] t3 (api) is now done. You had asked it: fix the login. Read its screen and tell me what happened, briefly.";
        for level in [Decisions, Results] {
            assert_eq!(said_as(Some(watch), level), Said::Note(Some("t3".into())), "kept as a note about its session");
            assert_eq!(said_as(Some("<hc from=\"tower-voice\">[agenda] a3 is done: every task is settled"), level), Said::Note(None));
            assert_eq!(said_as(Some("[queue] t5 (web) is gone, with 1 instruction(s) queued for it (#4)"), level), Said::Note(Some("t5".into())));
            // A session that wrote to the brain (`tower queue tower "…"`): a note from it, not the user asking.
            let from = crate::peers::Sender { address: "t7h".into(), name: "orb rust".into(), space: Some("Shop".into()) };
            assert_eq!(said_as(Some(&crate::peers::for_brain(&from, "the build is green")), level), Said::Note(Some("t7h".into())));
            assert_eq!(said_as(Some(&crate::peers::signed(&from, "the build is green")), level), Said::Answer, "untagged, it read as the user's");
            // What the user asked is answered to them at every level.
            assert_eq!(said_as(Some("[voice] what did the [watch] say?"), level), Said::Answer);
            assert_eq!(said_as(Some("typed at the keyboard"), level), Said::Answer);
            assert_eq!(said_as(None, level), Said::Answer, "no transcript to go by: an answer, as before");
        }
        // Telling everything: a note's news is an answer for the chat and the voice, as before.
        assert_eq!(said_as(Some(watch), Everything), Said::Answer);
        assert_eq!(said_as(Some("[agenda] a3 is done"), Everything), Said::Answer);
    }

    #[test]
    fn a_hook_says_whose_it_is() {
        // The main brain's hooks name nobody; a name that is no space's brain is the main brain too.
        assert_eq!(Who::of(None), Who::default());
        assert_eq!(Who::of(Some("tower")), Who::default());
        assert_eq!(Who::of(Some("tower-voice")), Who::default());
        // A space's own: its name goes with what it says (its space comes from its folder).
        let own = Who::of(Some(" tower-no-such-space-here "));
        assert_eq!((own.name().as_deref(), own.space.clone()), (Some("tower-no-such-space-here"), None));
        // A brain's folder has its own hooks: the agent hooks leave it alone.
        assert!(is_brain_dir(&crate::config::brain_dir().to_string_lossy()));
        assert!(is_brain_dir(&crate::brains::dir_for("Sandbox").to_string_lossy()));
        assert!(!is_brain_dir("/w/app") && !is_brain_dir(""));
        // A permission question on a brain's own screen, said by whose it is.
        let asks = "Claude needs your permission to use Bash.";
        assert_eq!(go_ahead(asks, &Who::default()), "I need your go-ahead: I need your permission to use Bash.");
        let sandbox = Who { brain: Some("tower-sandbox".into()), space: Some("Sandbox".into()) };
        assert_eq!(go_ahead(asks, &sandbox), "The Sandbox tower needs your go-ahead: it needs your permission to use Bash.");
        assert_eq!(go_ahead(asks, &Who { space: None, ..sandbox }), "tower sandbox needs your go-ahead: it needs your permission to use Bash.");
    }

    #[test]
    fn agent_hooks_become_events() {
        let e = agent_event("claude", "t5", &json!({"hook_event_name":"UserPromptSubmit","prompt":"fix it","session_id":"s1","cwd":"/w"}))
            .unwrap();
        assert_eq!((e.event.as_str(), e.text.as_str(), e.app.as_str(), e.terminal.as_str()), ("prompt", "fix it", "claude-code", "t5"));
        let e = agent_event("codex", "t6", &json!({"hook_event_name":"Stop","last_assistant_message":"Done."})).unwrap();
        assert_eq!((e.event.as_str(), e.text.as_str(), e.app.as_str()), ("stop", "Done.", "codex"));
        let e = agent_event("claude", "t5", &json!({"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"npm test"}}))
            .unwrap();
        assert_eq!(e.text, "permission to use Bash: npm test");
        assert!(agent_event("claude", "t5", &json!({"hook_event_name":"PreToolUse"})).is_none());
    }

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
