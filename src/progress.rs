//! How a working session's turn is going, read from its transcript as it
//! is written: no question asked, nothing typed in.
//!
//! - **Its task list**: Claude Code's TaskCreate / TaskUpdate (or the older
//!   TodoWrite): step N of M and the item at work. Claude Code gives the
//!   current models these tools only with `CLAUDE_CODE_ENABLE_TODO_TOOLS=1`;
//!   without them there is no step, only what follows.
//! - **The call at work**: its tool, what it does, and for how long.
//! - **The turn**: when it started, its latest words, its last write.
//!
//! "step 3 of 7: running the test suite · 14 min in · last update 1 min ago".
//! (`tower progress` asks a session itself: `crate::daemon::progress`.)

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An item of a session's task list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    /// What it is ("Run the test suite").
    pub subject: String,
    /// What it says while at it ("Running the test suite"); may be empty.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub active: String,
    /// pending, in_progress, completed.
    pub status: String,
}

/// A tool call that has no result yet.
#[derive(Debug, Clone, Default, PartialEq)]
struct Open {
    id: String,
    what: String,
    since_ms: u64,
}

/// What a transcript has said so far, read on as it grows.
#[derive(Debug, Clone, Default)]
pub struct Reader {
    items: Vec<Item>,
    /// TaskCreate calls waiting for their result (which gives the id).
    creating: HashMap<String, Item>,
    /// Its calls without a result yet, the oldest first.
    open: Vec<Open>,
    /// When its turn started (the prompt it is working on).
    turn_ms: Option<u64>,
    /// When its turn ended (Claude Code's turn_duration), if it has.
    ended_ms: Option<u64>,
    /// Its latest words, and when.
    said: Option<(String, u64)>,
    /// Its last line of any kind.
    last_ms: u64,
}

/// How a session's turn is going: what `tower agents`, the radar, the panel
/// and the phone show (`progress`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// Step N of M of its task list (the item at work, else the next one).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub step: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub steps: Option<u32>,
    /// Items done.
    #[serde(default)]
    pub done: u32,
    /// Its whole task list, in order (what's done, at work, next).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub items: Vec<Item>,
    /// The item at work, in its own words ("Running the test suite").
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub item: Option<String>,
    /// The call at work ("Bash: cargo test"), and since when.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_ms: Option<u64>,
    /// When its turn started.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub turn_ms: Option<u64>,
    /// Its last write to its transcript.
    pub last_ms: u64,
    /// Its latest words this turn (cut to a line).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub said: Option<String>,
    /// Its background job's own progress, when it said some.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub job: Option<String>,
    /// All of it in one line.
    pub line: String,
}

/// Cut to `n` characters, on a word where it can.
pub fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n {
        return s;
    }
    let cut: String = s.chars().take(n).collect();
    let cut = match cut.rfind(' ') {
        Some(i) if i > n / 2 => cut[..i].to_string(),
        _ => cut,
    };
    format!("{}…", cut.trim_end_matches([',', ';', ':', '.', ' ']))
}

/// A call in a few words: "Bash: cargo test", "Edit: watch.rs", "Agent: review the diff".
fn call_of(name: &str, input: &Value) -> String {
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let detail = match name {
        "Bash" => input["description"].as_str().map(str::to_string).or_else(|| input["command"].as_str().map(|c| clip(c, 60))),
        "Read" | "Edit" | "Write" | "NotebookEdit" => input["file_path"].as_str().or(input["notebook_path"].as_str()).map(base),
        "Grep" | "Glob" => input["pattern"].as_str().map(|p| clip(p, 40)),
        "Agent" | "Task" => input["description"].as_str().map(str::to_string),
        "WebFetch" => input["url"].as_str().and_then(crate::browsers::host_of),
        "WebSearch" => input["query"].as_str().map(|q| clip(q, 40)),
        "Monitor" => input["description"].as_str().map(str::to_string),
        _ => None,
    };
    let name = name.strip_prefix("mcp__").map(|n| n.rsplit("__").next().unwrap_or(n)).unwrap_or(name);
    match detail.filter(|d| !d.trim().is_empty()) {
        Some(d) => format!("{name}: {}", clip(&d, 70)),
        None => name.to_string(),
    }
}

/// Calls that are bookkeeping, not work: never "the call at work".
fn bookkeeping(name: &str) -> bool {
    matches!(name, "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet" | "TodoWrite" | "ToolSearch")
}

impl Reader {
    /// Reads on: the transcript's new lines.
    pub fn read(&mut self, lines: &str) {
        for line in lines.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let at = v["timestamp"].as_str().and_then(crate::releases::ms_of_iso).unwrap_or(0);
            if v["isSidechain"].as_bool() == Some(true) {
                continue;
            }
            match v["type"].as_str() {
                Some("system") if v["subtype"] == "turn_duration" => {
                    self.ended_ms = Some(at);
                    self.open.clear();
                }
                Some("user") => {
                    self.last_ms = self.last_ms.max(at);
                    self.user(&v, at);
                }
                Some("assistant") => {
                    self.last_ms = self.last_ms.max(at);
                    self.assistant(&v, at);
                }
                _ => {}
            }
        }
        if self.open.len() > 50 {
            self.open.drain(..self.open.len() - 50);
        }
    }

    fn user(&mut self, v: &Value, at: u64) {
        let content = &v["message"]["content"];
        let blocks: Vec<Value> = match content {
            Value::String(s) => vec![serde_json::json!({ "type": "text", "text": s })],
            Value::Array(a) => a.clone(),
            _ => return,
        };
        let mut prompt = false;
        for b in &blocks {
            match b["type"].as_str() {
                Some("tool_result") => {
                    let id = b["tool_use_id"].as_str().unwrap_or("");
                    self.open.retain(|o| o.id != id);
                    if let Some(mut item) = self.creating.remove(id) {
                        // "Task #3 created successfully: …" (its id in toolUseResult too).
                        let text = match &b["content"] {
                            Value::String(s) => s.clone(),
                            c => c.to_string(),
                        };
                        let from_result = v["toolUseResult"]["task"]["id"].as_str().map(str::to_string);
                        let from_text = text.split_once("Task #").map(|(_, r)| r.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()).filter(|s| !s.is_empty());
                        if let Some(id) = from_result.or(from_text) {
                            item.id = id;
                            self.items.retain(|i| i.id != item.id);
                            self.items.push(item);
                        }
                    }
                }
                Some("text") => {
                    // A prompt starts a turn (not Claude Code's own notes to itself).
                    if v["isMeta"].as_bool() != Some(true) && !b["text"].as_str().unwrap_or("").starts_with("<local-command") {
                        prompt = true;
                    }
                }
                _ => {}
            }
        }
        if prompt {
            self.turn_ms = Some(at);
            self.ended_ms = None;
            self.said = None;
            self.open.clear();
        }
    }

    fn assistant(&mut self, v: &Value, at: u64) {
        let Some(blocks) = v["message"]["content"].as_array() else { return };
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => {
                    let text = b["text"].as_str().unwrap_or("").trim();
                    if let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
                        let first = first.trim_start_matches(['#', '*', '-', ' ']);
                        self.said = Some((clip(first, 140), at));
                    }
                }
                Some("tool_use") => {
                    let name = b["name"].as_str().unwrap_or("");
                    let id = b["id"].as_str().unwrap_or("").to_string();
                    let input = &b["input"];
                    match name {
                        "TaskCreate" => {
                            let item = Item {
                                id: String::new(),
                                subject: input["subject"].as_str().unwrap_or("").to_string(),
                                active: input["activeForm"].as_str().unwrap_or("").to_string(),
                                status: "pending".into(),
                            };
                            self.creating.insert(id.clone(), item);
                        }
                        "TaskUpdate" => {
                            let tid = input["taskId"].as_str().map(str::to_string).or_else(|| input["taskId"].as_u64().map(|n| n.to_string()));
                            if let Some(i) = tid.and_then(|t| self.items.iter_mut().find(|i| i.id == t)) {
                                if let Some(s) = input["status"].as_str() {
                                    i.status = s.to_string();
                                }
                                if let Some(s) = input["subject"].as_str() {
                                    i.subject = s.to_string();
                                }
                                if let Some(s) = input["activeForm"].as_str() {
                                    i.active = s.to_string();
                                }
                            }
                            self.items.retain(|i| i.status != "deleted");
                        }
                        "TodoWrite" => {
                            // The whole list each time.
                            self.items = input["todos"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .enumerate()
                                .map(|(n, t)| Item {
                                    id: (n + 1).to_string(),
                                    subject: t["content"].as_str().unwrap_or("").to_string(),
                                    active: t["activeForm"].as_str().unwrap_or("").to_string(),
                                    status: t["status"].as_str().unwrap_or("pending").to_string(),
                                })
                                .collect();
                        }
                        _ => {}
                    }
                    if !bookkeeping(name) && !id.is_empty() {
                        self.open.push(Open { id, what: call_of(name, input), since_ms: at });
                    }
                }
                _ => {}
            }
        }
    }

    /// Its task list now.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// How its turn goes at `now`; `job`: its counted background job's own
    /// words. None: no turn read yet.
    pub fn progress(&self, job: Option<String>, now: u64) -> Option<Progress> {
        let turn_ms = self.turn_ms;
        if turn_ms.is_none() && self.items.is_empty() && self.open.is_empty() {
            return None;
        }
        let steps = self.items.len() as u32;
        let done = self.items.iter().filter(|i| i.status == "completed").count() as u32;
        let (step, item) = match self.items.iter().position(|i| i.status == "in_progress") {
            Some(n) => {
                let i = &self.items[n];
                (Some(n as u32 + 1), Some(if i.active.is_empty() { i.subject.clone() } else { i.active.clone() }))
            }
            // None at work: the next one waiting (all done: none).
            None => match self.items.iter().position(|i| i.status == "pending") {
                Some(n) if done > 0 => (Some(n as u32 + 1), Some(format!("next: {}", self.items[n].subject))),
                _ => (None, None),
            },
        };
        // The oldest open call is the one at work: calls made together run in turn.
        let open = self.open.first();
        let mut p = Progress {
            step,
            steps: (steps > 0).then_some(steps),
            done,
            items: self.items.clone(),
            item,
            tool: open.map(|o| o.what.clone()),
            tool_ms: open.map(|o| o.since_ms),
            turn_ms,
            last_ms: self.last_ms,
            said: self.said.as_ref().map(|(s, _)| s.clone()),
            job,
            line: String::new(),
        };
        p.line = line(&p, now);
        Some(p)
    }
}

/// "14 min", "40 s", "2 h 5 min".
pub fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        _ if s % 3600 < 60 => format!("{} h", s / 3600),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// Lower-case its first letter: "Running the tests" → "running the tests".
fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // An acronym or a name stays ("CI", "PR #42", "API").
        Some(f) if c.clone().next().is_some_and(|n| n.is_lowercase()) => f.to_lowercase().chain(c).collect(),
        Some(f) => std::iter::once(f).chain(c).collect(),
        None => String::new(),
    }
}

/// One line of it: "step 3 of 7: running the test suite · Bash: cargo
/// test for 4 min · 14 min in · last update 1 min ago".
pub fn line(p: &Progress, now: u64) -> String {
    let mut parts: Vec<String> = vec![];
    match (p.step, p.steps, &p.item) {
        (Some(n), Some(m), Some(item)) => parts.push(format!("step {n} of {m}: {}", lower_first(&clip(item, 70)))),
        (None, Some(m), _) if p.done == m => parts.push(format!("all {m} steps done")),
        _ => {}
    }
    // The call at work, when it has run a while (a short one is noise).
    if let (Some(tool), Some(since)) = (&p.tool, p.tool_ms) {
        let ran = now.saturating_sub(since);
        if parts.is_empty() {
            parts.push(if ran >= 60_000 { format!("{tool} for {}", span(ran)) } else { tool.clone() });
        } else if ran >= 60_000 {
            parts.push(format!("{tool} for {}", span(ran)));
        }
    }
    if parts.is_empty()
        && let Some(said) = &p.said
    {
        parts.push(format!("\"{}\"", clip(said, 80)));
    }
    if let Some(job) = &p.job {
        parts.push(format!("job: {}", clip(job, 60)));
    }
    if let Some(t) = p.turn_ms.filter(|t| *t > 0 && now > *t) {
        parts.push(format!("{} in", span(now - t)));
    }
    if p.last_ms > 0 {
        let quiet = now.saturating_sub(p.last_ms);
        parts.push(if quiet < 60_000 { "last update just now".to_string() } else { format!("last update {} ago", span(quiet)) });
    }
    if parts.is_empty() { "working".to_string() } else { parts.join(" · ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-14T05:00:00Z + s seconds (within the day).
    fn at(s: u64) -> String {
        format!("2026-11-14T{:02}:{:02}:{:02}.000Z", 5 + s / 3600, (s / 60) % 60, s % 60)
    }
    fn ms(s: u64) -> u64 {
        crate::releases::ms_of_iso("2026-11-14T05:00:00Z").unwrap() + s * 1000
    }
    fn prompt(s: u64, text: &str) -> String {
        serde_json::json!({ "type": "user", "timestamp": at(s), "message": { "role": "user", "content": text } }).to_string()
    }
    fn uses(s: u64, id: &str, name: &str, input: Value) -> String {
        serde_json::json!({ "type": "assistant", "timestamp": at(s), "message": { "content": [{ "type": "tool_use", "id": id, "name": name, "input": input }] } }).to_string()
    }
    fn result(s: u64, id: &str, text: &str, tur: Value) -> String {
        serde_json::json!({ "type": "user", "timestamp": at(s), "toolUseResult": tur, "message": { "content": [{ "type": "tool_result", "tool_use_id": id, "content": text }] } }).to_string()
    }
    fn says(s: u64, text: &str) -> String {
        serde_json::json!({ "type": "assistant", "timestamp": at(s), "message": { "content": [{ "type": "text", "text": text }] } }).to_string()
    }

    /// The task list of a real turn (Claude Code 2.1.325, TaskCreate and
    /// TaskUpdate), step by step.
    #[test]
    fn a_task_list_is_step_n_of_m_and_its_item() {
        let mut r = Reader::default();
        let mut lines = vec![prompt(0, "count the files, then run the tests")];
        for (n, (subject, active)) in [("Count the Rust files in src", "Counting the Rust files"), ("Run the test suite", "Running the test suite"), ("Write a one-line summary", "")].iter().enumerate() {
            let id = format!("c{n}");
            lines.push(uses(2, &id, "TaskCreate", serde_json::json!({ "subject": subject, "description": "…", "activeForm": active })));
            let tid = (n + 1).to_string();
            lines.push(result(2, &id, &format!("Task #{tid} created successfully: {subject}"), serde_json::json!({ "task": { "id": tid, "subject": subject } })));
        }
        lines.push(uses(3, "u1", "TaskUpdate", serde_json::json!({ "taskId": "1", "status": "in_progress" })));
        r.read(&lines.join("\n"));
        let p = r.progress(None, ms(30)).unwrap();
        assert_eq!((p.step, p.steps, p.item.as_deref()), (Some(1), Some(3), Some("Counting the Rust files")));
        assert_eq!(p.line, "step 1 of 3: counting the Rust files · 30 s in · last update just now");

        // Item 1 done, item 2 at work with a long call.
        r.read(
            &[
                uses(40, "u2", "TaskUpdate", serde_json::json!({ "taskId": "1", "status": "completed" })),
                uses(41, "u3", "TaskUpdate", serde_json::json!({ "taskId": "2", "status": "in_progress" })),
                uses(42, "b1", "Bash", serde_json::json!({ "command": "cargo test --release", "description": "Run the tests" })),
            ]
            .join("\n"),
        );
        let p = r.progress(None, ms(42 + 4 * 60)).unwrap();
        assert_eq!((p.step, p.done, p.tool.as_deref()), (Some(2), 1, Some("Bash: Run the tests")));
        assert_eq!(p.line, "step 2 of 3: running the test suite · Bash: Run the tests for 4 min · 4 min in · last update 4 min ago");

        // The call ends; between items it says what is next.
        // Two calls made together: the first runs, the second waits its turn.
        r.read(&uses(43, "b2", "Bash", serde_json::json!({ "command": "wc -l src/x.rs", "description": "Count the lines" })));
        assert_eq!(r.progress(None, ms(44)).unwrap().tool.as_deref(), Some("Bash: Run the tests"));
        r.read(&[result(300, "b1", "ok", Value::Null), result(300, "b2", "4", Value::Null), uses(301, "u4", "TaskUpdate", serde_json::json!({ "taskId": "2", "status": "completed" }))].join("\n"));
        let p = r.progress(None, ms(310)).unwrap();
        assert_eq!((p.step, p.item.as_deref(), p.tool.as_deref()), (Some(3), Some("next: Write a one-line summary"), None));
        // A deleted item leaves the count.
        r.read(&uses(311, "u5", "TaskUpdate", serde_json::json!({ "taskId": "3", "status": "deleted" })));
        let p = r.progress(None, ms(312)).unwrap();
        assert_eq!((p.steps, p.done), (Some(2), 2));
        assert!(p.line.starts_with("all 2 steps done"), "{}", p.line);
    }

    /// The older TodoWrite: the whole list in each call.
    #[test]
    fn a_todo_write_list_is_read_too() {
        let mut r = Reader::default();
        let todos = serde_json::json!({ "todos": [
            { "content": "Read the spec", "status": "completed", "activeForm": "Reading the spec" },
            { "content": "Fix the parser", "status": "in_progress", "activeForm": "Fixing the parser" },
            { "content": "Run the tests", "status": "pending", "activeForm": "Running the tests" },
        ] });
        r.read(&[prompt(0, "fix it"), uses(5, "t1", "TodoWrite", todos)].join("\n"));
        let p = r.progress(None, ms(65)).unwrap();
        assert_eq!(p.line, "step 2 of 3: fixing the parser · 1 min in · last update 1 min ago");
    }

    /// No task list (the default for current models): the call at work, else
    /// its latest words, and a job's own progress.
    #[test]
    fn without_a_task_list_it_is_the_call_or_its_words() {
        let mut r = Reader::default();
        r.read(&[prompt(0, "migrate the db"), says(3, "I'll start with the **schema**.\nThen the data."), uses(4, "e1", "Edit", serde_json::json!({ "file_path": "/repo/src/db/schema.rs" }))].join("\n"));
        assert_eq!(r.progress(None, ms(10)).unwrap().line, "Edit: schema.rs · 10 s in · last update just now");
        r.read(&result(6, "e1", "ok", Value::Null));
        let p = r.progress(Some("epoch 3 of 10".into()), ms(200)).unwrap();
        assert_eq!(p.said.as_deref(), Some("I'll start with the **schema**."));
        assert_eq!(p.line, "\"I'll start with the **schema**.\" · job: epoch 3 of 10 · 3 min in · last update 3 min ago");
        // A new prompt starts a new turn: its words and calls are gone.
        r.read(&prompt(300, "now the docs"));
        let p = r.progress(None, ms(310)).unwrap();
        assert_eq!((p.said.as_deref(), p.tool.as_deref(), p.turn_ms), (None, None, Some(ms(300))));
        // Tool results don't start a turn; bookkeeping is never the call at work.
        r.read(&[uses(311, "s1", "ToolSearch", serde_json::json!({})), result(312, "s1", "…", Value::Null)].join("\n"));
        assert_eq!(r.progress(None, ms(313)).unwrap().turn_ms, Some(ms(300)));
        assert_eq!(r.progress(None, ms(313)).unwrap().tool, None);
    }

    #[test]
    fn spans_and_clips_read_short() {
        assert_eq!((span(40_000), span(14 * 60_000), span(2 * 3_600_000), span(2 * 3_600_000 + 5 * 60_000)), ("40 s".into(), "14 min".into(), "2 h".into(), "2 h 5 min".into()));
        assert_eq!(clip("one two three four", 12), "one two…");
        assert_eq!(call_of("mcp__claude-in-chrome__navigate", &Value::Null), "navigate");
        assert_eq!(lower_first("CI checks"), "CI checks");
        assert_eq!(lower_first("Running"), "running");
    }
}
