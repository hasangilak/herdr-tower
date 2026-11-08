//! Structured questions: what a blocked Claude Code or Codex session asks,
//! as data the panel and the phone can show, and answers given as data,
//! typed into the session's dialog for the user. The design is in
//! `docs/questions.md`.
//!
//! - **Reading** (`detect`): the session's screen (`hc read`) says which
//!   dialog is up, which tab is current and what is answered; for Claude
//!   Code's `AskUserQuestion` the session's transcript has the exact
//!   questions, options and multi-select flags, so every tab is known even
//!   before it is visited. A screen that can't be read gives no question set
//!   (the user answers as before: approve/refuse through the tower).
//! - **Answering** (`drive`): keys go in one step at a time, and after each
//!   one the screen is read again and checked; anything unexpected stops it
//!   and says what and where. Submitting happens only after the review tab
//!   shows every answer as asked.
//!
//! What the dialogs do with keys (measured on Claude Code 2.1.319, Codex
//! 0.164): on a single-choice tab a number moves the cursor and Enter picks
//! it and goes on; on a multi-choice tab a number ticks or unticks without
//! moving; Tab and Right go to the next tab; "Type something" takes pasted
//! text (Enter keeps it), and while the cursor is on typed text, numbers and
//! arrows go into the text: Up leaves it.

use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ---- the data ----

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Opt {
    pub label: String,
    pub description: String,
    /// The key that picks it in the dialog (`1`, `2`, ...).
    pub key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tab {
    /// The short header (`Database`), or what the prompt is (`Bash command`).
    pub title: String,
    pub question: String,
    /// The answers offered (without "Type something" / "Chat about this").
    pub options: Vec<Opt>,
    pub multi: bool,
    /// A typed answer is possible (Claude's "Type something").
    pub allows_other: bool,
    /// Already answered in the dialog.
    pub answered: bool,
}

/// One recommended answer for a tab.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rec {
    pub label: String,
    pub why: String,
    pub choice: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabRecs {
    pub tab: usize,
    pub options: Vec<Rec>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuestionSet {
    /// Stable while the same questions show (recommendations stay with it).
    pub id: String,
    /// The session's address (`t3`).
    pub session: String,
    pub name: String,
    /// `claude-ask`, `claude-plan`, `claude-permission`, `codex-menu`.
    pub kind: String,
    /// One line: what it asks (`Which database should the app use?`, `Run touch /tmp/x?`).
    pub title: String,
    /// More to read: the command, the plan.
    pub detail: String,
    /// What the session was asked to do (its open job), for context.
    pub context: String,
    pub tabs: Vec<Tab>,
    /// The tab the dialog shows (`tabs.len()`: the review/submit tab).
    pub current_tab: usize,
    /// `answering` (tabs to go), `review` (on the submit tab), `single` (no submit step).
    pub submit_state: String,
    /// Three per tab, from the tower's brain; `None` while it thinks.
    pub recommendations: Option<Vec<TabRecs>>,
    /// `open`, `answering` (keys going in), `answered`, `failed`.
    pub status: String,
    pub error: Option<String>,
    /// The questions came from the session's transcript (exact), not only its screen.
    pub exact: bool,
    pub at_ms: u64,
    /// `auto` (the tower answers it itself, with recommendation 1) or
    /// `escalate` (the user decides); `None` until the brain says.
    pub decision: Option<String>,
    /// Why: the brain's reason, or the rule that made it escalate.
    pub reason: String,
    /// What the tower answered for the user, when it did.
    pub answered_with: String,
}

// ---- reading the screen ----

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScreenOpt {
    pub n: usize,
    pub label: String,
    pub description: String,
    /// Multi-choice: ticked or not.
    pub checked: Option<bool>,
    pub cursor: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Screen {
    /// `ask`, `review`, `permission`, `plan`, `codex`.
    pub kind: String,
    /// The tab bar: (title, answered); empty without one.
    pub tabs: Vec<(String, bool)>,
    /// The tab bar has arrows and a Submit tab (several questions).
    pub has_submit: bool,
    pub question: String,
    pub options: Vec<ScreenOpt>,
    /// The review tab warns that not every question is answered.
    pub unanswered: bool,
    /// The review tab's lines (questions and answers).
    pub review: Vec<String>,
    pub detail: String,
}

impl Screen {
    pub fn cursor(&self) -> Option<usize> {
        self.options.iter().find(|o| o.cursor).map(|o| o.n)
    }
    pub fn option(&self, n: usize) -> Option<&ScreenOpt> {
        self.options.iter().find(|o| o.n == n)
    }
}

fn is_rule(l: &str) -> bool {
    let t = l.trim();
    t.chars().count() > 10 && t.chars().all(|c| matches!(c, '─' | '━' | '▔' | '╌' | '-'))
}

fn is_footer(l: &str) -> bool {
    let t = l.trim();
    t.starts_with("Enter to select")
        || t.starts_with("Esc to cancel")
        || t.starts_with("Press enter to confirm")
        || t.contains("Esc to cancel")
        || t.starts_with("ctrl+g to edit")
}

/// `❯ 3. [✔] Label` -> (cursor, 3, Some(true), "Label").
fn option_line(l: &str) -> Option<(bool, usize, Option<bool>, String)> {
    let t = l.trim_start();
    let (cursor, t) = match t.chars().next() {
        Some('❯') | Some('›') | Some('>') => (true, t[t.char_indices().nth(1).map(|(i, _)| i).unwrap_or(t.len())..].trim_start()),
        _ => (false, t),
    };
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = t[digits.len()..].strip_prefix(". ")?;
    let n: usize = digits.parse().ok()?;
    let (checked, label) = if let Some(r) = rest.strip_prefix("[ ] ") {
        (Some(false), r)
    } else if let Some(r) = rest.strip_prefix("[✔] ").or_else(|| rest.strip_prefix("[✓] ")).or_else(|| rest.strip_prefix("[x] ")) {
        (Some(true), r)
    } else {
        (None, rest)
    };
    Some((cursor, n, checked, label.trim().to_string()))
}

/// Options from line `from` on: until a rule (after one was found) or the footer.
fn options_from(lines: &[&str], from: usize) -> Vec<ScreenOpt> {
    let mut out: Vec<ScreenOpt> = vec![];
    for l in &lines[from..] {
        if is_footer(l) || (is_rule(l) && !out.is_empty()) {
            break;
        }
        if l.trim().is_empty() {
            continue;
        }
        match option_line(l) {
            Some((cursor, n, checked, label)) => out.push(ScreenOpt { n, label, description: String::new(), checked, cursor }),
            None => {
                let t = l.trim();
                // The multi tab's own buttons aren't descriptions.
                if matches!(t, "Next" | "Submit") {
                    continue;
                }
                if let Some(last) = out.last_mut()
                    && l.len() - l.trim_start().len() >= 3
                {
                    if !last.description.is_empty() {
                        last.description.push(' ');
                    }
                    last.description.push_str(t);
                }
            }
        }
    }
    out
}

fn last_index(lines: &[&str], f: impl Fn(&str) -> bool) -> Option<usize> {
    lines.iter().rposition(|l| f(l))
}

/// The tab bar: `←  ☐ Database  ☒ Features  ✔ Submit  →`, or ` ☐ Color` for one question.
fn tab_bar(l: &str) -> Option<(Vec<(String, bool)>, bool)> {
    let t = l.trim();
    if !(t.starts_with('←') || t.starts_with('☐') || t.starts_with('☒')) {
        return None;
    }
    let has_submit = t.contains("Submit") && t.starts_with('←');
    let tabs: Vec<(String, bool)> = t
        .split("  ")
        .map(str::trim)
        .filter_map(|tok| {
            if let Some(x) = tok.strip_prefix("☐ ") {
                Some((x.trim().to_string(), false))
            } else {
                tok.strip_prefix("☒ ").map(|x| (x.trim().to_string(), true))
            }
        })
        .collect();
    (!tabs.is_empty()).then_some((tabs, has_submit))
}

/// What dialog the screen shows, if any it knows.
pub fn parse_screen(screen: &str) -> Option<Screen> {
    let lines: Vec<&str> = screen.lines().map(|l| l.trim_end()).collect();
    let bar = last_index(&lines, |l| tab_bar(l).is_some());
    let review = last_index(&lines, |l| l.trim() == "Review your answers");
    let perm = last_index(&lines, |l| l.trim() == "Do you want to proceed?");
    let plan = last_index(&lines, |l| l.contains("Would you like to proceed?"));
    let codex = last_index(&lines, |l| l.trim() == "Would you like to run the following command?");
    // The dialog at the bottom is the one that's up.
    let best = [bar, perm, plan, codex].into_iter().flatten().max()?;
    if Some(best) == bar {
        let (tabs, has_submit) = tab_bar(lines[best])?;
        if let Some(r) = review.filter(|r| *r > best) {
            let end = last_index(&lines, |l| l.trim() == "Ready to submit your answers?").filter(|e| *e > r).unwrap_or(lines.len());
            let review: Vec<String> = lines[r + 1..end].iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            return Some(Screen {
                kind: "review".into(),
                unanswered: review.iter().any(|l| l.contains("not answered all questions")),
                review,
                tabs,
                has_submit,
                options: options_from(&lines, end),
                ..Default::default()
            });
        }
        let q = lines[best + 1..].iter().position(|l| !l.trim().is_empty()).map(|i| best + 1 + i)?;
        let options = options_from(&lines, q + 1);
        if options.is_empty() {
            return None;
        }
        return Some(Screen { kind: "ask".into(), tabs, has_submit, question: lines[q].trim().to_string(), options, ..Default::default() });
    }
    let block = |from: usize, to: usize| -> String {
        lines[from..to].iter().map(|l| l.trim()).filter(|l| !l.is_empty() && !is_rule(l)).collect::<Vec<_>>().join("\n")
    };
    if Some(best) == perm {
        let start = lines[..best].iter().rposition(|l| l.trim_start().starts_with("────")).map(|i| i + 1).unwrap_or(0);
        let detail = block(start, best);
        let options = options_from(&lines, best + 1);
        return (!options.is_empty()).then(|| Screen { kind: "permission".into(), question: "Do you want to proceed?".into(), detail, options, ..Default::default() });
    }
    if Some(best) == plan {
        let from = last_index(&lines[..best], |l| l.contains("Here is Claude's plan")).map(|i| i + 1).unwrap_or(best);
        let to = lines[from..best].iter().position(|l| l.trim_start().starts_with("────")).map(|i| from + i).unwrap_or(best);
        let options = options_from(&lines, best + 1);
        return (!options.is_empty()).then(|| Screen {
            kind: "plan".into(),
            question: lines[best].trim().to_string(),
            detail: block(from, to),
            options,
            ..Default::default()
        });
    }
    let first = lines[best + 1..].iter().position(|l| option_line(l).is_some()).map(|i| best + 1 + i)?;
    Some(Screen {
        kind: "codex".into(),
        question: "Would you like to run the following command?".into(),
        detail: block(best + 1, first),
        options: options_from(&lines, first),
        ..Default::default()
    })
}

// ---- the session's transcript ----

/// The questions of the last `AskUserQuestion` call that has no answer yet.
pub fn transcript_questions(jsonl: &str) -> Option<Vec<Tab>> {
    let mut pending: Option<(String, Value)> = None;
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some(content) = v["message"]["content"].as_array() else { continue };
        for c in content {
            match c["type"].as_str() {
                Some("tool_use") if c["name"] == "AskUserQuestion" => {
                    pending = Some((c["id"].as_str().unwrap_or("").to_string(), c["input"].clone()));
                }
                Some("tool_result") => {
                    if pending.as_ref().is_some_and(|(id, _)| c["tool_use_id"].as_str() == Some(id)) {
                        pending = None;
                    }
                }
                _ => {}
            }
        }
    }
    let (_, input) = pending?;
    let tabs: Vec<Tab> = input["questions"]
        .as_array()?
        .iter()
        .map(|q| Tab {
            title: q["header"].as_str().unwrap_or("").to_string(),
            question: q["question"].as_str().unwrap_or("").to_string(),
            options: q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(i, o)| Opt {
                    label: o["label"].as_str().unwrap_or("").to_string(),
                    description: o["description"].as_str().unwrap_or("").to_string(),
                    key: (i + 1).to_string(),
                })
                .collect(),
            multi: q["multiSelect"].as_bool().unwrap_or(false),
            allows_other: true,
            answered: false,
        })
        .collect();
    (!tabs.is_empty()).then_some(tabs)
}

/// The transcript of the Claude Code process `pid`: its session file (in
/// `~/.claude/sessions`, or its account's config folder) names it.
pub fn transcript_of(pid: u32) -> Option<String> {
    let home = dirs::home_dir()?;
    let mut roots = vec![home.join(".claude")];
    if let Some(dir) = config_dir_of(pid) {
        roots.insert(0, dir);
    }
    for root in roots {
        let Ok(text) = std::fs::read_to_string(root.join("sessions").join(format!("{pid}.json"))) else { continue };
        let v: Value = serde_json::from_str(&text).ok()?;
        let sid = v["sessionId"].as_str()?;
        let cwd = v["cwd"].as_str().unwrap_or("");
        let escaped: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let direct = root.join("projects").join(&escaped).join(format!("{sid}.jsonl"));
        let path = if direct.exists() {
            Some(direct)
        } else {
            std::fs::read_dir(root.join("projects")).ok()?.flatten().map(|d| d.path().join(format!("{sid}.jsonl"))).find(|p| p.exists())
        };
        if let Some(p) = path {
            // The tail is enough: the question is the latest call.
            let bytes = std::fs::read(&p).ok()?;
            let start = bytes.len().saturating_sub(2 * 1024 * 1024);
            let tail = String::from_utf8_lossy(&bytes[start..]).into_owned();
            return Some(tail.split_once('\n').map(|(_, rest)| rest.to_string()).filter(|_| start > 0).unwrap_or(tail));
        }
    }
    None
}

/// `CLAUDE_CONFIG_DIR` in a process's environment (its value may have spaces).
fn config_dir_of(pid: u32) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("ps").args(["-E", "-ww", "-p", &pid.to_string(), "-o", "command="]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let i = text.find(" CLAUDE_CONFIG_DIR=")? + " CLAUDE_CONFIG_DIR=".len();
    let rest = &text[i..];
    // Up to the next ` NAME=` (variables are upper-case words).
    let end = rest
        .match_indices(' ')
        .map(|(j, _)| j)
        .find(|&j| {
            let w: String = rest[j + 1..].chars().take_while(|c| *c != '=' && *c != ' ').collect();
            !w.is_empty() && w.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') && rest[j + 1 + w.len()..].starts_with('=')
        })
        .unwrap_or(rest.trim_end().len());
    let dir = rest[..end].trim();
    (!dir.is_empty()).then(|| std::path::PathBuf::from(dir))
}

// ---- the question set ----

fn id_of(session: &str, kind: &str, tabs: &[Tab], title: &str) -> String {
    let mut s = format!("{session}|{kind}|{title}");
    for t in tabs {
        s.push('|');
        s.push_str(&t.question);
        for o in &t.options {
            s.push('/');
            s.push_str(&o.label);
        }
    }
    format!("q{}", &crate::daemon::lan::auth::sha256_hex(s.as_bytes())[..12])
}

fn opts(screen: &Screen) -> Vec<Opt> {
    screen.options.iter().map(|o| Opt { label: o.label.clone(), description: o.description.clone(), key: o.n.to_string() }).collect()
}

/// The question set a screen (and, for Claude's questions, its transcript) shows.
/// `hc_msg`: herdr-core's one-line status (`approve Bash: touch /tmp/x`).
pub fn build(session: &str, name: &str, codex: bool, screen: &Screen, transcript: Option<Vec<Tab>>, hc_msg: &str, context: &str) -> QuestionSet {
    let mut q = QuestionSet { session: session.into(), name: name.into(), context: context.into(), status: "open".into(), ..Default::default() };
    match screen.kind.as_str() {
        "ask" | "review" => {
            q.kind = "claude-ask".into();
            let answered: Vec<bool> = screen.tabs.iter().map(|(_, a)| *a).collect();
            match transcript.filter(|t| t.len() == screen.tabs.len().max(1)) {
                Some(mut tabs) => {
                    q.exact = true;
                    for (i, t) in tabs.iter_mut().enumerate() {
                        t.answered = answered.get(i).copied().unwrap_or(false);
                    }
                    q.tabs = tabs;
                }
                None => {
                    // Only the current tab is known from the screen; the others by title.
                    q.tabs = screen
                        .tabs
                        .iter()
                        .map(|(title, a)| Tab { title: title.clone(), answered: *a, allows_other: true, ..Default::default() })
                        .collect();
                    if q.tabs.is_empty() {
                        q.tabs.push(Tab { allows_other: true, ..Default::default() });
                    }
                }
            }
            if screen.kind == "review" {
                q.current_tab = q.tabs.len();
                q.submit_state = "review".into();
            } else {
                q.current_tab = q.tabs.iter().position(|t| t.question == screen.question).unwrap_or(0);
                q.submit_state = if screen.has_submit { "answering".into() } else { "single".into() };
                let cur = &mut q.tabs[q.current_tab];
                if !q.exact {
                    let real: Vec<&ScreenOpt> = screen.options.iter().collect();
                    let n = real.len();
                    cur.question = screen.question.clone();
                    cur.multi = screen.options.iter().any(|o| o.checked.is_some());
                    // The last option is "Type something" (or what was typed there).
                    cur.options = real[..n.saturating_sub(1)]
                        .iter()
                        .map(|o| Opt { label: o.label.clone(), description: o.description.clone(), key: o.n.to_string() })
                        .collect();
                }
            }
            q.title = q.tabs.first().map(|t| t.question.clone()).unwrap_or_default();
            if q.tabs.len() > 1 {
                q.title = format!("{} questions: {}", q.tabs.len(), q.tabs.iter().map(|t| t.title.as_str()).collect::<Vec<_>>().join(", "));
            }
        }
        "permission" => {
            q.kind = "claude-permission".into();
            q.detail = screen.detail.clone();
            q.title = hc_msg.strip_prefix("approve ").map(|m| format!("Allow {m}?")).unwrap_or_else(|| "Allow this?".into());
            q.tabs = vec![Tab { title: "Permission".into(), question: screen.question.clone(), options: opts(screen), ..Default::default() }];
            q.submit_state = "single".into();
        }
        "plan" => {
            q.kind = "claude-plan".into();
            q.detail = screen.detail.clone();
            q.title = "Approve the plan?".into();
            q.tabs = vec![Tab { title: "Plan".into(), question: screen.question.clone(), options: opts(screen), ..Default::default() }];
            q.submit_state = "single".into();
        }
        _ => {
            q.kind = if codex { "codex-menu".into() } else { "unknown".into() };
            q.detail = screen.detail.clone();
            q.title = screen.detail.lines().find_map(|l| l.strip_prefix("$ ")).map(|c| format!("Run {c}?")).unwrap_or_else(|| "Approve?".into());
            q.tabs = vec![Tab { title: "Command".into(), question: screen.question.clone(), options: opts(screen), ..Default::default() }];
            q.submit_state = "single".into();
        }
    }
    q.id = id_of(session, &q.kind, &q.tabs, &q.title);
    q
}

// ---- answers ----

/// One tab's answer, as the API takes it: `choice` is an option's index
/// (0-based), `{"options": [0, 2]}` (multi), `{"other": "text"}`, both for
/// multi, or `{"spoken": "the second one"}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Answer {
    pub tab: usize,
    pub choice: Value,
    /// Files for a typed answer (paths under the attachments folder).
    pub attachments: Vec<String>,
}

/// What to do on one tab.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    /// Option indexes (0-based) to choose (one for a single-choice tab).
    pub options: Vec<usize>,
    pub other: Option<String>,
}

fn ordinal(w: &str) -> Option<usize> {
    Some(match w {
        "1" | "one" | "first" | "eins" | "erste" | "ersten" | "erstes" => 0,
        "2" | "two" | "second" | "zwei" | "zweite" | "zweiten" | "zweites" => 1,
        "3" | "three" | "third" | "drei" | "dritte" | "dritten" | "drittes" => 2,
        "4" | "four" | "fourth" | "vier" | "vierte" | "vierten" => 3,
        "5" | "five" | "fifth" | "fünf" | "fünfte" | "fünften" => 4,
        "6" | "six" | "sixth" | "sechs" | "sechste" => 5,
        _ => return None,
    })
}

fn norm(s: &str) -> String {
    s.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A spoken answer for a tab: "the second one", "first and third",
/// "Postgres", "zweite", or else the words themselves as a typed answer.
pub fn interpret(spoken: &str, tab: &Tab) -> Pick {
    let n = norm(spoken);
    let words: Vec<&str> = n.split(' ').collect();
    let filler = ["the", "one", "option", "number", "choice", "and", "die", "der", "das", "nummer", "und", "pick", "take", "nimm", "please", "bitte"];
    let mut picked: Vec<usize> = vec![];
    let mut only_picks = !words.is_empty();
    for (k, w) in words.iter().enumerate() {
        // "the second one": "one" says nothing after an ordinal or "the".
        let after = k.checked_sub(1).map(|j| words[j]);
        let one_as_filler = *w == "one" && after.is_some_and(|a| a == "the" || ordinal(a).is_some() || a == "last");
        if let Some(i) = ordinal(w).filter(|i| *i < tab.options.len() && !one_as_filler) {
            if !picked.contains(&i) {
                picked.push(i);
            }
        } else if *w == "last" || *w == "letzte" || *w == "letzten" {
            if let Some(i) = tab.options.len().checked_sub(1) {
                picked.push(i);
            }
        } else if !filler.contains(w) {
            only_picks = false;
        }
    }
    if !picked.is_empty() && only_picks && (tab.multi || picked.len() == 1) {
        return Pick { options: picked, other: None };
    }
    // An option's own name.
    let named: Vec<usize> = tab
        .options
        .iter()
        .enumerate()
        .filter(|(_, o)| {
            let l = norm(&o.label);
            let head = l.split(' ').next().unwrap_or("").to_string();
            !l.is_empty() && (n == l || n == head || (head.len() > 2 && words.contains(&head.as_str()) && words.len() <= 3))
        })
        .map(|(i, _)| i)
        .collect();
    if named.len() == 1 || (tab.multi && !named.is_empty()) {
        return Pick { options: named, other: None };
    }
    Pick { options: vec![], other: Some(spoken.trim().to_string()) }
}

/// An answer's choice for its tab, checked against it.
pub fn pick_of(a: &Answer, tab: &Tab) -> Result<Pick> {
    let n = tab.options.len();
    let mut p = match &a.choice {
        Value::Number(i) => Pick { options: vec![i.as_u64().unwrap_or(u64::MAX) as usize], other: None },
        Value::String(s) => interpret(s, tab),
        Value::Object(o) => {
            if let Some(s) = o.get("spoken").and_then(Value::as_str) {
                interpret(s, tab)
            } else {
                let mut options: Vec<usize> = o.get("options").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).map(|i| i as usize).collect();
                if let Some(i) = o.get("option").and_then(Value::as_u64) {
                    options.push(i as usize);
                }
                Pick { options, other: o.get("other").and_then(Value::as_str).map(String::from).filter(|t| !t.trim().is_empty()) }
            }
        }
        _ => bail!("tab {}: choice is an option index, {{\"options\": [...]}}, {{\"other\": \"text\"}} or {{\"spoken\": \"...\"}}", a.tab),
    };
    if let Some(bad) = p.options.iter().find(|i| **i >= n) {
        bail!("tab {}: there is no option {bad} (it has {n})", a.tab);
    }
    if !tab.multi && p.options.len() + usize::from(p.other.is_some()) > 1 {
        bail!("tab {} takes one answer", a.tab);
    }
    if p.other.is_some() && !tab.allows_other {
        bail!("tab {} takes one of its options, not typed text", a.tab);
    }
    if p.options.is_empty() && p.other.is_none() {
        bail!("tab {}: no answer", a.tab);
    }
    // Files go with the typed text, as the panel sends them.
    if !a.attachments.is_empty() {
        let text = p.other.take().unwrap_or_default();
        let markers: Vec<String> = a
            .attachments
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let kind = crate::daemon::lan::file_kind(&name, None, &[]);
                crate::daemon::lan::marker(kind, i + 1, &name)
            })
            .collect();
        if !tab.allows_other {
            bail!("tab {} takes one of its options, so it can't take files", a.tab);
        }
        p.other = Some(crate::history::compose(&crate::daemon::lan::with_markers(&text, &markers), &a.attachments));
    }
    Ok(p)
}

// ---- driving the dialog ----

/// The session's terminal, as the driver uses it.
pub trait Term {
    fn read(&mut self) -> String;
    fn keys(&mut self, keys: &[&str]) -> Result<()>;
    /// Pastes text (it may have several lines) and presses Enter.
    fn paste(&mut self, text: &str) -> Result<()>;
    /// Time for the dialog to redraw.
    fn settle(&mut self) {}
}

/// The real terminal, through `hc`.
pub struct Hc {
    pub address: String,
}

impl Term for Hc {
    fn read(&mut self) -> String {
        crate::hc::run(&["read", &self.address]).map(|o| o.stdout).unwrap_or_default()
    }
    fn keys(&mut self, keys: &[&str]) -> Result<()> {
        let mut args = vec!["keys", self.address.as_str()];
        args.extend_from_slice(keys);
        let out = crate::hc::run(&args)?;
        if out.code != 0 {
            bail!("hc keys: {}", out.stderr.trim());
        }
        Ok(())
    }
    fn paste(&mut self, text: &str) -> Result<()> {
        // Without the caller's terminal: hc then types the text as it is
        // (from a terminal it would wrap it in a note saying who sent it).
        let out = std::process::Command::new(crate::config::hc_bin())
            .args(["send", "--force", &self.address, text])
            .env_remove("HC_TERMINAL_ID")
            .env_remove("HC_SCOPE")
            .stdin(std::process::Stdio::null())
            .output()?;
        if !out.status.success() {
            bail!("hc send: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
    fn settle(&mut self) {
        std::thread::sleep(Duration::from_millis(350));
    }
}

/// Where the driver stopped, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Stopped {
    pub step: String,
    pub message: String,
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.message)
    }
}

/// Reads until `ok` holds (a few tries: the dialog redraws), else stops.
fn expect(term: &mut dyn Term, step: &str, what: &str, ok: impl Fn(&Screen) -> bool) -> Result<Screen, Stopped> {
    for _ in 0..8 {
        term.settle();
        let text = term.read();
        if let Some(s) = parse_screen(&text)
            && ok(&s)
        {
            return Ok(s);
        }
    }
    Err(Stopped { step: step.into(), message: format!("expected {what}, but the screen shows something else; stopped there") })
}

fn now(term: &mut dyn Term, step: &str) -> Result<Screen, Stopped> {
    expect(term, step, "the question dialog", |_| true)
}

/// The tab a screen shows (`tabs.len()`: the review).
fn tab_index(s: &Screen, set: &QuestionSet) -> Option<usize> {
    if s.kind == "review" {
        return Some(set.tabs.len());
    }
    set.tabs.iter().position(|t| t.question == s.question)
}

/// The cursor is on typed text (keys would go into it): Up leaves it.
fn leave_text(term: &mut dyn Term, s: &Screen, step: &str) -> Result<Screen, Stopped> {
    let last = s.options.iter().map(|o| o.n).max().unwrap_or(0);
    if s.cursor() == Some(last) && s.option(last).is_some_and(|o| !o.label.starts_with("Type something")) {
        term.keys(&["up"]).map_err(|e| Stopped { step: step.into(), message: e.to_string() })?;
        return expect(term, step, "the cursor off the typed answer", |x| x.cursor() != Some(last));
    }
    Ok(s.clone())
}

fn go_to(term: &mut dyn Term, set: &QuestionSet, target: usize) -> Result<Screen, Stopped> {
    let step = if target == set.tabs.len() { "the review tab".to_string() } else { format!("tab {} ({})", target + 1, set.tabs[target].title) };
    let mut s = now(term, &step)?;
    for _ in 0..(2 * set.tabs.len() + 4) {
        s = leave_text(term, &s, &step)?;
        let Some(cur) = tab_index(&s, set) else {
            return Err(Stopped { step, message: "the dialog shows a question that isn't in this set".into() });
        };
        if cur == target {
            return Ok(s);
        }
        let key = if cur < target { "right" } else { "left" };
        term.keys(&[key]).map_err(|e| Stopped { step: step.clone(), message: e.to_string() })?;
        s = expect(term, &step, "the next tab", |x| tab_index(x, set) != Some(cur))?;
    }
    Err(Stopped { step, message: "couldn't reach it with the arrow keys".into() })
}

fn first_line(t: &str) -> String {
    t.lines().next().unwrap_or("").trim().to_string()
}

/// Answers one tab of a Claude question dialog.
fn answer_tab(term: &mut dyn Term, set: &QuestionSet, t: usize, pick: &Pick) -> Result<(), Stopped> {
    let tab = &set.tabs[t];
    let step = format!("tab {} ({})", t + 1, tab.title);
    let err = |m: String| Stopped { step: step.clone(), message: m };
    let s = go_to(term, set, t)?;
    let other_n = tab.options.len() + 1;
    if !tab.multi {
        let n = match &pick.other {
            Some(_) => other_n,
            None => pick.options[0] + 1,
        };
        let single = set.submit_state == "single";
        // Picked: the tab is ticked and the dialog moved on (or, for one question, went away).
        let moved_on = |x: &Screen| tab_index(x, set) != Some(t) || x.tabs.get(t).is_some_and(|(_, a)| *a) && x.question != tab.question;
        term.keys(&[&n.to_string()]).map_err(|e| err(e.to_string()))?;
        match &pick.other {
            Some(text) => {
                // The number moves to "Type something"; the pasted text (and its Enter) answers.
                expect(term, &step, &format!("the cursor on option {n}"), |x| x.cursor() == Some(n) && x.question == tab.question)?;
                term.paste(text).map_err(|e| err(e.to_string()))?;
            }
            None => {
                // An option's number picks it at once on Claude's dialog; if it only
                // moved the cursor there, Enter picks it.
                if !single {
                    let s = expect(term, &step, &format!("option {n} picked, or the cursor on it"), |x| moved_on(x) || x.cursor() == Some(n))?;
                    if !moved_on(&s) {
                        term.keys(&["enter"]).map_err(|e| err(e.to_string()))?;
                    }
                } else {
                    term.settle();
                    let still = parse_screen(&term.read()).filter(|x| x.kind == "ask" && x.question == tab.question);
                    if still.as_ref().is_some_and(|x| x.cursor() == Some(n)) {
                        term.keys(&["enter"]).map_err(|e| err(e.to_string()))?;
                    }
                }
            }
        }
        if single {
            // One question: picking it sends it; the dialog goes away.
            for _ in 0..8 {
                term.settle();
                let text = term.read();
                if parse_screen(&text).is_none_or(|x| x.kind != "ask" || x.question != tab.question) {
                    return Ok(());
                }
            }
            return Err(err("the answer didn't go through: the question is still there".into()));
        }
        expect(term, &step, "the tab answered", moved_on)?;
        return Ok(());
    }
    // Several choices: tick what's wanted, untick the rest, then the typed answer.
    let mut s = s;
    for i in 0..tab.options.len() {
        let n = i + 1;
        let want = pick.options.contains(&i);
        if s.option(n).and_then(|o| o.checked) != Some(want) {
            term.keys(&[&n.to_string()]).map_err(|e| err(e.to_string()))?;
            s = expect(term, &step, &format!("option {n} {}", if want { "ticked" } else { "unticked" }), |x| {
                x.option(n).and_then(|o| o.checked) == Some(want)
            })?;
        }
    }
    let other_on = s.option(other_n).and_then(|o| o.checked).unwrap_or(false);
    match &pick.other {
        Some(text) => {
            // To the typed answer's row, type it, step off it, tick it.
            let mut guard = 0;
            while s.cursor() != Some(other_n) {
                term.keys(&["down"]).map_err(|e| err(e.to_string()))?;
                s = now(term, &step)?;
                guard += 1;
                if guard > tab.options.len() + 2 {
                    return Err(err("couldn't reach the typed answer's row".into()));
                }
            }
            term.paste(text).map_err(|e| err(e.to_string()))?;
            let head = first_line(text);
            s = expect(term, &step, "the typed answer in its row", |x| x.option(other_n).is_some_and(|o| o.label.starts_with(&head)))?;
            s = leave_text(term, &s, &step)?;
            if s.option(other_n).and_then(|o| o.checked) != Some(true) {
                term.keys(&[&other_n.to_string()]).map_err(|e| err(e.to_string()))?;
                expect(term, &step, "the typed answer ticked", |x| x.option(other_n).and_then(|o| o.checked) == Some(true))?;
            }
        }
        None if other_on => {
            term.keys(&[&other_n.to_string()]).map_err(|e| err(e.to_string()))?;
            expect(term, &step, "the typed answer unticked", |x| x.option(other_n).and_then(|o| o.checked) == Some(false))?;
        }
        None => {}
    }
    Ok(())
}

/// What a pick looks like on the review tab.
fn review_text(tab: &Tab, pick: &Pick) -> Vec<String> {
    let mut v: Vec<String> = pick.options.iter().map(|i| tab.options[*i].label.clone()).collect();
    if let Some(t) = &pick.other {
        v.push(first_line(t));
    }
    v
}

/// Types the answers into the dialog and, with `submit`, sends them, all
/// checked on the screen as it goes. The set must be current (just detected).
pub fn drive(term: &mut dyn Term, set: &QuestionSet, answers: &[Answer], submit: bool) -> Result<(), Stopped> {
    let bad = |m: String| Stopped { step: "the answers".into(), message: m };
    let mut picks: Vec<(usize, Pick)> = vec![];
    for a in answers {
        let tab = set.tabs.get(a.tab).ok_or_else(|| bad(format!("there is no tab {}", a.tab)))?;
        if tab.options.is_empty() && set.kind == "claude-ask" {
            return Err(bad(format!("tab {} isn't known yet (open it in the session once)", a.tab)));
        }
        picks.push((a.tab, pick_of(a, tab).map_err(|e| bad(e.to_string()))?));
    }
    picks.sort_by_key(|(t, _)| *t);
    if set.kind != "claude-ask" {
        // A menu: one choice, Enter confirms.
        let Some((_, p)) = picks.first() else { return Err(bad("no answer".into())) };
        if p.other.is_some() || p.options.len() != 1 {
            return Err(bad("this prompt takes one of its options".into()));
        }
        let n = set.tabs[0].options[p.options[0]].key.clone();
        let step = "the prompt".to_string();
        let same = |x: &Screen| x.question == set.tabs[0].question && x.detail == set.detail;
        let before = parse_screen(&term.read());
        if !before.as_ref().is_some_and(same) {
            return Err(Stopped { step, message: "the prompt on screen isn't this one; nothing pressed".into() });
        }
        term.keys(&[&n]).map_err(|e| Stopped { step: step.clone(), message: e.to_string() })?;
        term.settle();
        // Some menus act on the number at once; others move the cursor and wait for Enter.
        match parse_screen(&term.read()).filter(same) {
            Some(x) if x.cursor() == n.parse().ok() => {
                term.keys(&["enter"]).map_err(|e| Stopped { step: step.clone(), message: e.to_string() })?;
            }
            Some(_) => return Err(Stopped { step, message: format!("the cursor isn't on option {n}; nothing confirmed") }),
            None => return Ok(()),
        }
        for _ in 0..8 {
            term.settle();
            let text = term.read();
            if parse_screen(&text).is_none_or(|x| x.question != set.tabs[0].question || x.detail != set.detail) {
                return Ok(());
            }
        }
        return Err(Stopped { step, message: "the prompt is still there after Enter".into() });
    }
    for (t, p) in &picks {
        answer_tab(term, set, *t, p)?;
    }
    if set.submit_state == "single" || !submit {
        return Ok(());
    }
    let s = go_to(term, set, set.tabs.len())?;
    let step = "the review tab".to_string();
    let answered_all = picks.len() == set.tabs.len() || set.tabs.iter().enumerate().all(|(i, t)| t.answered || picks.iter().any(|(j, _)| *j == i));
    if s.unanswered && answered_all {
        return Err(Stopped { step, message: "it says not every question is answered; nothing sent".into() });
    }
    if s.unanswered {
        return Err(Stopped { step, message: "not every question has an answer yet; nothing sent".into() });
    }
    let review = s.review.join("\n");
    for (t, p) in &picks {
        for want in review_text(&set.tabs[*t], p) {
            if !review.contains(&want) {
                return Err(Stopped { step: step.clone(), message: format!("the review doesn't show \"{want}\" for tab {}; nothing sent", t + 1) });
            }
        }
    }
    if s.cursor() != Some(1) {
        term.keys(&["1"]).map_err(|e| Stopped { step: step.clone(), message: e.to_string() })?;
        expect(term, &step, "the cursor on Submit answers", |x| x.kind == "review" && x.cursor() == Some(1))?;
    }
    term.keys(&["enter"]).map_err(|e| Stopped { step: step.clone(), message: e.to_string() })?;
    for _ in 0..8 {
        term.settle();
        if parse_screen(&term.read()).is_none_or(|x| x.kind != "review" && tab_index(&x, set).is_none()) {
            return Ok(());
        }
    }
    Err(Stopped { step, message: "the dialog is still up after Submit".into() })
}

// ---- recommendations ----

/// Recommendations as `tower recommend` gives them: `[{tab, options: [{label, why, choice}]}]`
/// (or `{"tabs": [...]}`), checked against the set: up to three per tab,
/// each a valid choice.
pub fn parse_recs(v: &Value, set: &QuestionSet) -> Result<Vec<TabRecs>> {
    let list = v.get("tabs").unwrap_or(v);
    let recs: Vec<TabRecs> = serde_json::from_value(list.clone())?;
    let mut out = vec![];
    for r in recs {
        let Some(tab) = set.tabs.get(r.tab) else { bail!("there is no tab {}", r.tab) };
        let mut kept = vec![];
        for o in r.options.into_iter().take(3) {
            if o.label.trim().is_empty() {
                bail!("tab {}: a recommendation needs a label", r.tab);
            }
            if !tab.options.is_empty() || tab.allows_other {
                pick_of(&Answer { tab: r.tab, choice: o.choice.clone(), attachments: vec![] }, tab)
                    .map_err(|e| anyhow::anyhow!("tab {}: \"{}\": {e}", r.tab, o.label))?;
            }
            kept.push(o);
        }
        out.push(TabRecs { tab: r.tab, options: kept });
    }
    Ok(out)
}

/// The one-line message the tower's brain gets for a new question set.
pub fn brain_note(set: &QuestionSet) -> String {
    let tabs: Vec<Value> = set
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "tab": i, "title": t.title, "question": t.question, "multi": t.multi, "allows_other": t.allows_other,
                "options": t.options.iter().enumerate().map(|(j, o)| json!({ "i": j, "label": o.label, "description": o.description })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let body = json!({ "id": set.id, "session": set.session, "name": set.name, "kind": set.kind, "asked": set.context, "detail": set.detail, "tabs": tabs });
    format!("[question] {} {} {}", set.session, set.id, serde_json::to_string(&body).unwrap_or_default())
}

// ---- questions asked in plain text, at the end of a turn ----

/// The last thing the assistant wrote in a Claude Code transcript (JSONL).
pub fn last_assistant_text(jsonl: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v["type"].as_str() {
            Some("assistant") => {
                for b in v["message"]["content"].as_array().into_iter().flatten() {
                    if b["type"] == "text"
                        && let Some(t) = b["text"].as_str().filter(|t| !t.trim().is_empty())
                    {
                        last = Some(t.to_string());
                    }
                }
            }
            Some("user") => {
                let c = &v["message"]["content"];
                if c.is_string() || c.as_array().is_some_and(|a| a.iter().any(|b| b["type"] == "text")) {
                    last = None;
                }
            }
            _ => {}
        }
    }
    last
}

/// The last message on a Claude Code or Codex screen, without the
/// interface around it (the prompt box, the status lines, tool calls).
pub fn message_from_screen(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().map(|l| l.trim_end()).collect();
    // Where the input box begins: the last rule, or Codex's prompt line.
    let end = lines.iter().rposition(|l| is_rule(l) || l.trim_start().starts_with("› ")).unwrap_or(lines.len());
    // The last message bullet above it (Claude ⏺, Codex •), skipping tool calls.
    let start = (0..end).rev().find(|&i| {
        let t = lines[i].trim_start();
        (t.starts_with("⏺ ") || t.starts_with("• "))
            && !t.contains('(')
            && !["Running", "Ran ", "Read", "Edited", "Explored", "Called", "Bash", "Exited", "Worked"].iter().any(|w| t[t.char_indices().nth(2).map(|(k, _)| k).unwrap_or(0)..].starts_with(w))
    })?;
    let text: Vec<String> = lines[start..end]
        .iter()
        .map(|l| l.trim())
        .take_while(|l| !l.starts_with('✻') && !l.starts_with("■") && !l.starts_with("─ Worked"))
        .map(|l| l.trim_start_matches("⏺ ").trim_start_matches("• ").to_string())
        .collect();
    let t = text.join("\n").trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Worth asking the brain: the message's end asks the user something.
/// Cheap and generous; the brain decides.
pub fn looks_like_question(message: &str) -> bool {
    // A `?` in code or a link isn't a question.
    let mut plain = String::new();
    let mut in_code = false;
    for part in message.split('`') {
        if !in_code {
            plain.push_str(part);
        }
        in_code = !in_code;
    }
    let plain: String = plain.split_whitespace().filter(|w| !w.contains("://") && !w.starts_with("](")).collect::<Vec<_>>().join(" ");
    let plain = plain.replace("](", " ");
    let chars: Vec<char> = plain.trim().chars().collect();
    let tail: String = chars[chars.len().saturating_sub(600)..].iter().collect();
    let end: String = chars[chars.len().saturating_sub(300)..].iter().collect();
    let lower = tail.to_lowercase();
    let phrases = [
        "want me to", "should i ", "shall i ", "would you like", "do you want", "which do you prefer", "which one", "which would you",
        "let me know", "your call", "or should i", "do you prefer", "ok to ", "okay to ", "go ahead and", "should we", "any preference",
        "möchtest du", "soll ich", "willst du", "sollen wir", "lass mich wissen", "welche", "wie möchtest",
    ];
    if end.contains('?') || phrases.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // Options laid out for a choice: "1. …", "2. …" or "a) …", "b) …" near the end, with a question word.
    let listed = tail.lines().filter(|l| {
        let t = l.trim_start();
        let b = t.as_bytes();
        b.len() > 3 && ((b[0].is_ascii_digit() && (b[1] == b'.' || b[1] == b')')) || (b[0].is_ascii_lowercase() && b[1] == b')'))
    });
    listed.count() >= 2 && ["option", "choose", "pick", "prefer", "which"].iter().any(|w| lower.contains(w))
}

/// The question it seems to ask, until the brain says: the last sentence with a `?`.
fn last_question(message: &str) -> String {
    let flat = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(q) = flat.rfind('?') else { return flat.chars().rev().take(160).collect::<Vec<_>>().into_iter().rev().collect() };
    let before = &flat[..q];
    let start = before.rfind(['.', '!', '\n', ':']).map(|i| i + 1).unwrap_or(0);
    flat[start..=q].trim().to_string()
}

/// A question set for a message that ends asking something (kind `text`).
pub fn text_set(session: &str, name: &str, message: &str, context: &str) -> QuestionSet {
    let tail: String = {
        let c: Vec<char> = message.trim().chars().collect();
        c[c.len().saturating_sub(1500)..].iter().collect()
    };
    let tab = Tab { title: "Reply".into(), question: last_question(&tail), allows_other: true, ..Default::default() };
    let mut q = QuestionSet {
        session: session.into(),
        name: name.into(),
        kind: "text".into(),
        title: tab.question.clone(),
        detail: tail,
        context: context.into(),
        tabs: vec![tab],
        submit_state: "single".into(),
        status: "open".into(),
        ..Default::default()
    };
    q.id = id_of(session, "text", &[], &q.detail);
    q
}

/// The one-line message the brain gets for a message that may ask the user something.
pub fn text_note(set: &QuestionSet) -> String {
    let body = json!({ "id": set.id, "session": set.session, "name": set.name, "asked": set.context, "message": set.detail });
    format!("[question-text] {} {} {}", set.session, set.id, serde_json::to_string(&body).unwrap_or_default())
}

/// The brain's reading of a text question set: the questions it asks (each
/// with three recommended replies). `[{"question": "...", "options": [{label, why, choice}]}]`
/// (or `{"tabs": [...]}`); `choice` is `{"other": "<the reply>"}`.
pub fn text_recs(v: &Value) -> Result<(Vec<Tab>, Vec<TabRecs>)> {
    let list = v.get("tabs").or_else(|| v.get("questions")).unwrap_or(v).as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        bail!("give the questions it asks (or --none when it asks nothing)");
    }
    let mut tabs = vec![];
    let mut recs = vec![];
    for (i, q) in list.iter().enumerate() {
        let question = q["question"].as_str().unwrap_or("").trim().to_string();
        if question.is_empty() {
            bail!("question {}: its text is missing", i + 1);
        }
        let tab = Tab { title: q["title"].as_str().map(String::from).unwrap_or_else(|| format!("Question {}", i + 1)), question, allows_other: true, ..Default::default() };
        let mut kept = vec![];
        for o in q["options"].as_array().into_iter().flatten().take(3) {
            let r: Rec = serde_json::from_value(o.clone())?;
            let reply = r.choice.get("other").and_then(Value::as_str).or_else(|| r.choice.as_str()).unwrap_or("").trim().to_string();
            if r.label.trim().is_empty() || reply.is_empty() {
                bail!("question {}: each recommendation needs a label and the reply as {{\"other\": \"text\"}}", i + 1);
            }
            kept.push(Rec { choice: json!({ "other": reply }), ..r });
        }
        recs.push(TabRecs { tab: i, options: kept });
        tabs.push(tab);
    }
    Ok((tabs, recs))
}

/// One reply to a text question set: each question's answer (numbered when
/// there are several), files attached as the panel does.
pub fn compose_reply(set: &QuestionSet, answers: &[Answer]) -> Result<String> {
    if answers.is_empty() {
        bail!("no answer");
    }
    let mut parts: Vec<(usize, String)> = vec![];
    let mut files: Vec<String> = vec![];
    for a in answers {
        let Some(tab) = set.tabs.get(a.tab) else { bail!("there is no question {}", a.tab) };
        let text = match &a.choice {
            Value::String(s) => s.clone(),
            Value::Object(o) => {
                let mut v: Vec<String> = o.get("options").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64).filter_map(|i| tab.options.get(i as usize).map(|x| x.label.clone())).collect();
                if let Some(t) = o.get("other").or_else(|| o.get("spoken")).and_then(Value::as_str) {
                    v.push(t.to_string());
                }
                v.join("; ")
            }
            Value::Number(i) => tab.options.get(i.as_u64().unwrap_or(u64::MAX) as usize).map(|o| o.label.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        if text.trim().is_empty() && a.attachments.is_empty() {
            bail!("question {}: no answer", a.tab + 1);
        }
        parts.push((a.tab, text.trim().to_string()));
        files.extend(a.attachments.iter().cloned());
    }
    parts.sort_by_key(|(t, _)| *t);
    let body = if set.tabs.len() > 1 || parts.len() > 1 {
        parts.iter().map(|(t, text)| format!("{}. {text}", t + 1)).collect::<Vec<_>>().join("\n")
    } else {
        parts[0].1.clone()
    };
    if files.is_empty() {
        return Ok(body);
    }
    let markers: Vec<String> = files
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            crate::daemon::lan::marker(crate::daemon::lan::file_kind(&name, None, &[]), i + 1, &name)
        })
        .collect();
    // As the panel attaches them, keeping the reply's lines (it is typed into the session as it is).
    Ok(format!("{} [attached: {}]", crate::daemon::lan::with_markers(&body, &markers), files.join(" | ")))
}

// ---- the 90/10 rule: what the user must decide ----

/// Why a question set must go to the user whatever the brain says: it
/// deploys or releases, pushes or merges to a shared branch, deletes or
/// overwrites, touches infrastructure, secrets or money. None: the brain may
/// answer it itself if it's routine.
pub fn must_escalate(set: &QuestionSet) -> Option<&'static str> {
    let mut text = format!("{}\n{}\n{}", set.title, set.detail, set.context);
    for t in &set.tabs {
        text.push('\n');
        text.push_str(&t.question);
        for o in &t.options {
            text.push('\n');
            text.push_str(&o.label);
            text.push(' ');
            text.push_str(&o.description);
        }
    }
    let raw = text.to_lowercase();
    let words = format!(" {} ", norm(&text));
    let word = |w: &str| words.contains(&format!(" {w} "));
    let any = |ws: &[&str]| ws.iter().any(|w| word(w));
    let raw_any = |ps: &[&str]| ps.iter().any(|p| raw.contains(p));
    // A Yes/No about one of these is the same as doing it.
    if any(&["deploy", "deploys", "deploying", "deployment", "redeploy", "production", "prod", "go live", "rollout", "publish", "publishing"])
        || (any(&["release", "releasing"]) && !raw.contains("release notes"))
    {
        return Some("it deploys or releases");
    }
    if raw_any(&["git push", "push to", "push the", "push origin", "force push", "--force", "merge into main", "merge to main", "into main", "into master", "merge into prod", "into prod"]) {
        return Some("it pushes or merges to a shared branch");
    }
    if any(&["delete", "deletes", "deleting", "remove the branch", "drop", "truncate", "overwrite", "overwrites", "wipe", "destroy", "purge", "irreversible"])
        || raw_any(&["rm -rf", "rm -r ", "reset --hard", "drop table", "drop database", "branch -d", "branch -d "])
    {
        return Some("it deletes or overwrites");
    }
    if any(&["aws", "s3", "ec2", "terraform", "kubernetes", "kubectl", "dns", "cloudflare", "infrastructure", "servers", "firewall", "iam"]) {
        return Some("it touches infrastructure");
    }
    if any(&["secret", "secrets", "credential", "credentials", "password", "passwords", "api key", "private key", "token", "tokens"]) || raw.contains(".env") {
        return Some("it involves secrets or credentials");
    }
    if any(&["billing", "payment", "payments", "subscription", "pricing", "purchase", "paid", "credit card", "invoice", "charge"]) {
        return Some("it spends money");
    }
    None
}

/// The decision that holds: the user's policy first, then the rule, then the brain.
/// `policy`: `normal`, `keep-moving` or `ask-everything`.
pub fn decide(set: &QuestionSet, brain: Option<&str>, brain_reason: &str, policy: &str) -> (String, String) {
    if policy == "ask-everything" {
        return ("escalate".into(), "you asked to decide everything yourself".into());
    }
    if let Some(why) = must_escalate(set) {
        return ("escalate".into(), why.into());
    }
    match brain {
        Some("auto") => ("auto".into(), brain_reason.to_string()),
        _ => ("escalate".into(), if brain_reason.is_empty() { "it needs your call".into() } else { brain_reason.to_string() }),
    }
}

/// The answers the tower gives by itself: recommendation 1 for every question.
pub fn first_picks(set: &QuestionSet) -> Option<Vec<Answer>> {
    let recs = set.recommendations.as_ref()?;
    (0..set.tabs.len())
        .map(|t| recs.iter().find(|r| r.tab == t).and_then(|r| r.options.first()).map(|o| Answer { tab: t, choice: o.choice.clone(), attachments: vec![] }))
        .collect()
}

/// "Which database? -> SQLite; Which features? -> Auth": what was picked, in a line.
pub fn picked_line(set: &QuestionSet) -> String {
    let Some(recs) = &set.recommendations else { return String::new() };
    set.tabs
        .iter()
        .enumerate()
        .filter_map(|(i, t)| recs.iter().find(|r| r.tab == i).and_then(|r| r.options.first()).map(|o| format!("{} -> {}", t.question, o.label)))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/questions/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn reads_claude_question_tabs() {
        let s = parse_screen(&fixture("claude-ask-tab1.txt")).unwrap();
        assert_eq!(s.kind, "ask");
        assert!(s.has_submit);
        assert_eq!(s.tabs, vec![("Database".into(), false), ("Features".into(), false), ("Deploy".into(), false)]);
        assert_eq!(s.question, "Which database should the app use?");
        let labels: Vec<&str> = s.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Postgres (managed, scales)", "SQLite (single file)", "MySQL (classic)", "Type something."], "Chat about this is left out");
        assert_eq!(s.options[0].description, "Managed Postgres that scales with load.");
        assert_eq!(s.cursor(), Some(1));

        let m = parse_screen(&fixture("claude-ask-tab2-checked.txt")).unwrap();
        assert_eq!(m.question, "Which features do you want in the first release?");
        assert_eq!(m.options.iter().map(|o| o.checked).collect::<Vec<_>>(), [Some(false), Some(true), Some(false), Some(false)]);
        assert_eq!(m.tabs[1], ("Features".into(), true));

        let single = parse_screen(&fixture("claude-ask-single.txt")).unwrap();
        assert!(!single.has_submit);
        assert_eq!(single.tabs, vec![("Color".into(), false)]);

        let edit = parse_screen(&fixture("claude-multi-other-typed.txt")).unwrap();
        assert_eq!(edit.option(3).map(|o| (o.label.as_str(), o.checked)), Some(("A parrot", Some(false))));
        assert_eq!(edit.cursor(), Some(3));
    }

    #[test]
    fn reads_the_review_tab() {
        let r = parse_screen(&fixture("claude-ask-review-tagged.txt")).unwrap();
        assert_eq!(r.kind, "review");
        assert!(r.unanswered);
        assert!(r.review.iter().any(|l| l.contains("Search (full text)")));
        assert_eq!(r.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Submit answers", "Cancel"]);
    }

    #[test]
    fn reads_permission_plan_and_codex_prompts() {
        let p = parse_screen(&fixture("claude-permission.txt")).unwrap();
        assert_eq!(p.kind, "permission");
        assert_eq!(p.options.len(), 4);
        assert_eq!(p.options[0].label, "Yes");
        assert_eq!(p.options[3].label, "No");
        assert!(p.detail.contains("touch /tmp/qfix-perm-test"));
        let set = build("t37", "qfix perm", false, &p, None, "approve Bash: touch /tmp/qfix-perm-test", "");
        assert_eq!((set.kind.as_str(), set.title.as_str()), ("claude-permission", "Allow Bash: touch /tmp/qfix-perm-test?"));

        let plan = parse_screen(&fixture("claude-plan.txt")).unwrap();
        assert_eq!(plan.kind, "plan");
        assert_eq!(plan.options[0].label, "Yes, and use auto mode");
        assert!(plan.detail.contains("Plan: create /tmp/qfix-plan.txt"));
        assert_eq!(plan.options[2].description, "shift+tab to approve with this feedback");

        let c = parse_screen(&fixture("codex-approval.txt")).unwrap();
        assert_eq!(c.kind, "codex");
        assert_eq!(c.options.len(), 3);
        assert!(c.options[0].label.starts_with("Yes, proceed"));
        assert_eq!(c.cursor(), Some(1));
        let set = build("t3b", "codex", true, &c, None, "", "");
        assert_eq!((set.kind.as_str(), set.title.as_str()), ("codex-menu", "Run touch /tmp/qfix-codex-test?"));
    }

    #[test]
    fn plain_screens_are_no_question() {
        assert!(parse_screen(&fixture("claude-ask-answered.txt")).is_none(), "answered and done");
        assert!(parse_screen("$ ls\nCargo.toml  src\n$ ").is_none());
        assert!(parse_screen("").is_none());
    }

    #[test]
    fn the_transcript_has_the_whole_set() {
        let tabs = transcript_questions(&fixture("claude-ask-transcript.jsonl")).unwrap();
        assert_eq!(tabs.len(), 3);
        assert_eq!((tabs[1].title.as_str(), tabs[1].multi, tabs[1].options.len()), ("Features", true, 3));
        assert_eq!(tabs[2].options[1].label, "Fly (containers)");
        // Answered: no question pending.
        let answered = format!(
            "{}\n{}",
            fixture("claude-ask-transcript.jsonl").trim(),
            r#"{"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_01EuRMRZkZ5d9fjsKbAvUKkg"}]}}"#
        );
        assert!(transcript_questions(&answered).is_none());

        let screen = parse_screen(&fixture("claude-ask-tab3.txt")).unwrap();
        let set = build("t34", "qfix ask", false, &screen, Some(tabs.clone()), "", "set up the app");
        assert!(set.exact);
        assert_eq!((set.current_tab, set.submit_state.as_str()), (2, "answering"));
        assert_eq!(set.tabs[1].answered, true, "Features was ticked on screen");
        assert_eq!(set.id, build("t34", "x", false, &parse_screen(&fixture("claude-ask-tab1.txt")).unwrap(), Some(tabs), "", "").id, "same questions, same id");
        assert!(brain_note(&set).starts_with(&format!("[question] t34 {} {{", set.id)));
    }

    #[test]
    fn spoken_answers() {
        let tab = Tab {
            options: ["Postgres (managed)", "SQLite (single file)", "MySQL"].iter().map(|l| Opt { label: l.to_string(), ..Default::default() }).collect(),
            allows_other: true,
            ..Default::default()
        };
        let p = |s: &str| interpret(s, &tab);
        assert_eq!(p("the second one").options, [1]);
        assert_eq!(p("Zweite").options, [1]);
        assert_eq!(p("number 3").options, [2]);
        assert_eq!(p("the last one").options, [2]);
        assert_eq!(p("SQLite").options, [1]);
        assert_eq!(p("Use Redis for caching and keep the rest").other.as_deref(), Some("Use Redis for caching and keep the rest"));
        assert_eq!(p("first and third").other.is_some(), true, "a single-choice tab takes one");
        let multi = Tab { multi: true, ..tab.clone() };
        assert_eq!(interpret("first and third", &multi).options, [0, 2]);
    }

    #[test]
    fn answers_are_checked_and_files_go_with_typed_text() {
        let tab = Tab { options: vec![Opt::default(), Opt::default()], allows_other: true, ..Default::default() };
        let a = |c: Value| Answer { tab: 0, choice: c, attachments: vec![] };
        assert_eq!(pick_of(&a(json!(1)), &tab).unwrap().options, [1]);
        assert!(pick_of(&a(json!(5)), &tab).is_err());
        assert!(pick_of(&a(json!({"options": [0, 1]})), &tab).is_err(), "single choice");
        let with_file = Answer { tab: 0, choice: json!({"other": "like this"}), attachments: vec!["/x/attachments/1-phone/1-a.png".into()] };
        assert_eq!(pick_of(&with_file, &tab).unwrap().other.as_deref(), Some("[Image #1] like this [attached: /x/attachments/1-phone/1-a.png]"));
        let menu = Tab { options: vec![Opt::default()], ..Default::default() };
        assert!(pick_of(&a(json!({"other": "x"})), &menu).is_err());
    }

    // ---- a simulated Claude question dialog, to drive ----

    #[derive(Clone)]
    struct SimTab {
        title: &'static str,
        q: &'static str,
        opts: Vec<&'static str>,
        multi: bool,
        checked: Vec<bool>,
        picked: Option<usize>,
        other: Option<String>,
        other_on: bool,
    }

    struct Sim {
        tabs: Vec<SimTab>,
        cur: usize,
        cursor: usize,
        editing: bool,
        submitted: bool,
        log: Vec<String>,
    }

    impl Sim {
        fn new() -> Sim {
            let t = |title, q, opts: Vec<&'static str>, multi| SimTab {
                title,
                q,
                checked: vec![false; opts.len()],
                opts,
                multi,
                picked: None,
                other: None,
                other_on: false,
            };
            Sim {
                tabs: vec![
                    t("Database", "Which database?", vec!["Postgres", "SQLite"], false),
                    t("Features", "Which features?", vec!["Auth", "Search", "Billing"], true),
                    t("Deploy", "Where?", vec!["Vercel", "Fly"], false),
                ],
                cur: 0,
                cursor: 1,
                editing: false,
                submitted: false,
                log: vec![],
            }
        }
        fn answered(t: &SimTab) -> bool {
            t.picked.is_some() || t.checked.iter().any(|c| *c) || t.other_on || (!t.multi && t.other.is_some())
        }
        fn render(&self) -> String {
            if self.submitted {
                return "⏺ User answered Claude's questions:\n".into();
            }
            let bar: Vec<String> = self.tabs.iter().map(|t| format!("{} {}", if Self::answered(t) { "☒" } else { "☐" }, t.title)).collect();
            let mut s = format!("────────────────\n←  {}  ✔ Submit  →\n\n", bar.join("  "));
            if self.cur == self.tabs.len() {
                s += "Review your answers\n\n";
                if !self.tabs.iter().all(Self::answered) {
                    s += "⚠ You have not answered all questions\n";
                }
                for t in self.tabs.iter().filter(|t| Self::answered(t)) {
                    let mut a: Vec<String> = t.opts.iter().zip(&t.checked).filter(|(_, c)| **c).map(|(o, _)| o.to_string()).collect();
                    if let Some(p) = t.picked {
                        a.push(t.opts[p].to_string());
                    }
                    if t.other_on || (!t.multi && t.other.is_some()) {
                        a.push(t.other.clone().unwrap_or_default());
                    }
                    s += &format!(" ● {}\n   → {}\n", t.q, a.join(", "));
                }
                s += &format!("\nReady to submit your answers?\n\n{} 1. Submit answers\n{} 2. Cancel\n", c(self.cursor == 1), c(self.cursor == 2));
                return s;
            }
            let t = &self.tabs[self.cur];
            s += &format!("{}\n\n", t.q);
            for (i, o) in t.opts.iter().enumerate() {
                let n = i + 1;
                let box_ = if t.multi { format!("[{}] ", if t.checked[i] { "✔" } else { " " }) } else { String::new() };
                s += &format!("{} {n}. {box_}{o}\n     about {o}\n", c(self.cursor == n));
            }
            let n = t.opts.len() + 1;
            let label = t.other.clone().unwrap_or_else(|| if t.multi { "Type something".into() } else { "Type something.".into() });
            let box_ = if t.multi { format!("[{}] ", if t.other_on { "✔" } else { " " }) } else { String::new() };
            s += &format!("{} {n}. {box_}{label}\n", c(self.cursor == n));
            s += "────────────────\n  9. Chat about this\nEnter to select · Tab/Arrow keys to navigate · Esc to cancel\n";
            s
        }
        fn next(&mut self) {
            self.cur = (self.cur + 1).min(self.tabs.len());
            self.cursor = 1;
            self.editing = false;
        }
    }

    fn c(on: bool) -> &'static str {
        if on { "❯" } else { " " }
    }

    impl Term for Sim {
        fn read(&mut self) -> String {
            self.render()
        }
        fn keys(&mut self, keys: &[&str]) -> Result<()> {
            for k in keys {
                self.log.push(k.to_string());
                if self.editing && *k != "up" && *k != "down" && *k != "enter" {
                    // Into the typed text.
                    let t = &mut self.tabs[self.cur];
                    t.other.get_or_insert_with(String::new).push_str(k);
                    continue;
                }
                match *k {
                    "right" | "tab" => self.next(),
                    "left" => {
                        self.cur = self.cur.saturating_sub(1);
                        self.cursor = 1;
                    }
                    "up" => {
                        self.cursor = self.cursor.saturating_sub(1).max(1);
                        self.editing = false;
                    }
                    "down" => self.cursor += 1,
                    "enter" => {
                        if self.cur == self.tabs.len() {
                            if self.cursor == 1 {
                                self.submitted = true;
                            }
                            continue;
                        }
                        let t = &mut self.tabs[self.cur];
                        if t.multi {
                            if self.cursor <= t.opts.len() {
                                t.checked[self.cursor - 1] ^= true;
                            }
                        } else if self.cursor <= t.opts.len() {
                            t.picked = Some(self.cursor - 1);
                            self.next();
                        }
                    }
                    d => {
                        let n: usize = d.parse()?;
                        if self.cur == self.tabs.len() {
                            self.cursor = n;
                            continue;
                        }
                        let t = &mut self.tabs[self.cur];
                        if t.multi {
                            if n <= t.opts.len() {
                                t.checked[n - 1] ^= true;
                            } else {
                                t.other_on ^= true;
                            }
                        } else if n <= t.opts.len() {
                            // Claude's dialog: a real option's number picks it and moves on.
                            t.picked = Some(n - 1);
                            self.next();
                        } else {
                            self.cursor = n;
                        }
                    }
                }
            }
            Ok(())
        }
        fn paste(&mut self, text: &str) -> Result<()> {
            self.log.push(format!("paste:{text}"));
            let n = self.tabs[self.cur].opts.len() + 1;
            if self.cursor != n {
                bail!("pasted off the typed row");
            }
            let t = &mut self.tabs[self.cur];
            t.other = Some(text.to_string());
            if t.multi {
                // Enter after the paste leaves it unticked, cursor still in the text.
                t.other_on = false;
                self.editing = true;
            } else {
                self.next();
            }
            Ok(())
        }
    }

    fn sim_set(sim: &mut Sim) -> QuestionSet {
        let tabs: Vec<Tab> = sim
            .tabs
            .iter()
            .map(|t| Tab {
                title: t.title.into(),
                question: t.q.into(),
                options: t.opts.iter().enumerate().map(|(i, o)| Opt { label: o.to_string(), description: String::new(), key: (i + 1).to_string() }).collect(),
                multi: t.multi,
                allows_other: true,
                answered: false,
            })
            .collect();
        let s = parse_screen(&sim.render()).unwrap();
        build("t9", "web", false, &s, Some(tabs), "", "")
    }

    #[test]
    fn drives_every_kind_of_tab_and_submits_after_checking() {
        let mut sim = Sim::new();
        let set = sim_set(&mut sim);
        let answers = vec![
            Answer { tab: 0, choice: json!(1), attachments: vec![] },
            Answer { tab: 1, choice: json!({"options": [0, 2], "other": "Exports, as CSV"}), attachments: vec![] },
            Answer { tab: 2, choice: json!({"other": "Railway\nwith a staging app"}), attachments: vec![] },
        ];
        drive(&mut sim, &set, &answers, true).unwrap();
        assert!(sim.submitted, "{:?}", sim.log);
        assert_eq!(sim.tabs[0].picked, Some(1));
        assert_eq!(sim.tabs[1].checked, [true, false, true]);
        assert!(sim.tabs[1].other_on);
        assert_eq!(sim.tabs[1].other.as_deref(), Some("Exports, as CSV"), "no stray keys typed into it");
        assert_eq!(sim.tabs[2].other.as_deref(), Some("Railway\nwith a staging app"));
    }

    #[test]
    fn never_submits_with_questions_left() {
        let mut sim = Sim::new();
        let set = sim_set(&mut sim);
        let e = drive(&mut sim, &set, &[Answer { tab: 0, choice: json!(0), attachments: vec![] }], true).unwrap_err();
        assert_eq!(e.step, "the review tab");
        assert!(!sim.submitted);
        // Without submit it just fills in.
        let mut sim = Sim::new();
        drive(&mut sim, &set, &[Answer { tab: 2, choice: json!(1), attachments: vec![] }], false).unwrap();
        assert_eq!(sim.tabs[2].picked, Some(1));
        assert!(!sim.submitted);
    }

    #[test]
    fn stops_when_the_screen_isnt_what_it_should_be() {
        struct Stuck(Sim);
        impl Term for Stuck {
            fn read(&mut self) -> String {
                self.0.render()
            }
            // Keys go nowhere.
            fn keys(&mut self, _: &[&str]) -> Result<()> {
                Ok(())
            }
            fn paste(&mut self, _: &str) -> Result<()> {
                Ok(())
            }
        }
        let mut sim = Sim::new();
        let set = sim_set(&mut sim);
        let mut stuck = Stuck(sim);
        let e = drive(&mut stuck, &set, &[Answer { tab: 1, choice: json!({"options": [0]}), attachments: vec![] }], true).unwrap_err();
        assert!(e.step.starts_with("tab 2"), "{e}");
        assert!(!stuck.0.submitted);
        // A set that doesn't match the dialog at all.
        let mut other = set.clone();
        other.tabs[0].question = "Something else?".into();
        let mut sim = Sim::new();
        assert!(drive(&mut sim, &other, &[Answer { tab: 0, choice: json!(0), attachments: vec![] }], true).is_err());
    }

    #[test]
    fn plain_text_questions() {
        let real = [
            "I added the filter and the tests pass. Want me to also open the PR into dev, or wait until you've looked at it?",
            "Done. The import works for both formats.\n\nTwo ways to handle the old files:\n1. Convert them on read\n2. Migrate them once with a script\n\nWhich do you prefer?",
            "Ich habe die Seite umgebaut. Soll ich das gleich deployen?",
            "The build is green. Let me know if you want the changelog updated too.",
        ];
        for m in real {
            assert!(looks_like_question(m), "{m}");
        }
        let not = [
            "All 40 tests pass and the PR is merged into dev. The deploy finished in 3 minutes.",
            "Fixed the redirect loop: the cookie domain was wrong. Verified in the browser.",
            "Q: why did it fail? A: the token expired. Renewed it, everything works now and the job is done. Nothing else is pending on my side, and the release notes are written in the docs folder as usual for this project.",
        ];
        for m in &not[..2] {
            assert!(!looks_like_question(m), "{m}");
        }
        let set = text_set("t22", "eyzflow", real[0], "add the filter");
        assert_eq!(set.kind, "text");
        assert_eq!(set.tabs[0].question, "Want me to also open the PR into dev, or wait until you've looked at it?");
        assert!(set.tabs[0].allows_other);
        assert!(text_note(&set).starts_with(&format!("[question-text] t22 {} {{", set.id)));
    }

    #[test]
    fn real_finished_messages() {
        let list: Vec<Value> = serde_json::from_str(&fixture("finished-messages.json")).unwrap();
        for m in list {
            let text = m["text"].as_str().unwrap();
            assert_eq!(looks_like_question(text), m["asks"].as_bool().unwrap(), "{text}");
        }
    }

    #[test]
    fn messages_from_screens_and_transcripts() {
        let screen = "❯ do it\n⏺ Bash(npm test)\n  ⎿  ok\n⏺ The tests pass.\n  Want me to open the PR?\n✻ Worked for 9s\n──────────\n❯ \n──────────\n";
        assert_eq!(message_from_screen(screen).as_deref(), Some("The tests pass.\nWant me to open the PR?"));
        let codex = "› fix it\n• Ran cargo test\n• Fixed. Should I also update the docs?\n\n› Ask Codex to do anything\n";
        assert_eq!(message_from_screen(codex).as_deref(), Some("Fixed. Should I also update the docs?"));
        let jsonl = r#"{"type":"user","message":{"content":"go"}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Working on it."}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Done. Deploy now?"}]}}"#;
        assert_eq!(last_assistant_text(jsonl).as_deref(), Some("Done. Deploy now?"));
    }

    #[test]
    fn the_brain_reads_text_questions_and_one_reply_goes_back() {
        let set = text_set("t22", "eyzflow", "Done. Want me to open the PR? And should I tag a release?", "");
        let v = json!([
            {"question": "Open the PR?", "options": [
                {"label": "Yes, into dev", "why": "the tests pass", "choice": {"other": "Yes, open the PR into dev."}},
                {"label": "Not yet", "why": "I want to look first", "choice": {"other": "Not yet, I'll review first."}},
                {"label": "Into main", "why": "it's a hotfix", "choice": {"other": "Open it into main."}}]},
            {"question": "Tag a release?", "options": [{"label": "No", "why": "nothing user-facing", "choice": {"other": "No release."}}]}
        ]);
        let (tabs, recs) = text_recs(&v).unwrap();
        assert_eq!((tabs.len(), recs[0].options.len(), tabs[1].question.as_str()), (2, 3, "Tag a release?"));
        assert!(text_recs(&json!([{"question": "x", "options": [{"label": "a", "why": "", "choice": 1}]}])).is_err(), "text replies are text");
        let mut set = set;
        set.tabs = tabs;
        let reply = compose_reply(&set, &[
            Answer { tab: 1, choice: json!({"other": "No release."}), attachments: vec![] },
            Answer { tab: 0, choice: json!({"other": "Yes, open the PR into dev."}), attachments: vec!["/a/attachments/1-phone/1-shot.png".into()] },
        ]).unwrap();
        assert_eq!(reply, "[Image #1] 1. Yes, open the PR into dev.\n2. No release. [attached: /a/attachments/1-phone/1-shot.png]");
        let one = text_set("t3", "x", "Deploy now?", "");
        assert_eq!(compose_reply(&one, &[Answer { tab: 0, choice: json!("Yes, deploy."), attachments: vec![] }]).unwrap(), "Yes, deploy.");
        assert!(compose_reply(&one, &[]).is_err());
    }

    fn q(title: &str, question: &str, options: &[&str]) -> QuestionSet {
        QuestionSet {
            title: title.into(),
            tabs: vec![Tab { question: question.into(), options: options.iter().map(|o| Opt { label: o.to_string(), ..Default::default() }).collect(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn the_90_10_rule() {
        // Must go to the user.
        for (set, why) in [
            (q("", "Should I deploy the dashboard to production now?", &["Yes", "No"]), "it deploys or releases"),
            (q("Allow Bash: git push origin main?", "Do you want to proceed?", &["Yes", "No"]), "it pushes or merges to a shared branch"),
            (q("", "Want me to merge this into main?", &[]), "it pushes or merges to a shared branch"),
            (q("Allow Bash: rm -rf build/ dist/?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("", "Should I delete the old migration files?", &[]), "it deletes or overwrites"),
            (q("", "Which AWS region for the new bucket?", &["eu-central-1", "us-east-1"]), "it touches infrastructure"),
            (q("", "Should I put the API key in .env and commit it?", &[]), "it involves secrets or credentials"),
            (q("", "Enable the paid plan for more build minutes?", &[]), "it spends money"),
        ] {
            assert_eq!(must_escalate(&set), Some(why), "{}", set.tabs[0].question);
            assert_eq!(decide(&set, Some("auto"), "routine", "normal").0, "escalate", "the rule wins over the brain");
        }
        // Routine: the brain may answer.
        for set in [
            q("", "Which test runner should I use?", &["Vitest", "Jest"]),
            q("Allow Bash: npm test?", "Do you want to proceed?", &["Yes", "No"]),
            q("", "Want me to run the linter and fix the warnings?", &[]),
            q("", "Should I name it useCaseFilters or useFilters?", &[]),
            q("", "Want me to add the release notes to CHANGELOG.md?", &[]),
            q("", "Should I commit this locally?", &[]),
        ] {
            assert_eq!(must_escalate(&set), None, "{}", set.tabs[0].question);
            assert_eq!(decide(&set, Some("auto"), "routine choice", "normal"), ("auto".into(), "routine choice".into()));
            assert_eq!(decide(&set, Some("escalate"), "", "normal").0, "escalate", "the brain may still ask");
            assert_eq!(decide(&set, None, "", "normal").0, "escalate", "no word from the brain: ask");
            assert_eq!(decide(&set, Some("auto"), "routine", "ask-everything").0, "escalate", "the user's toggle wins");
        }
    }

    #[test]
    fn recommendation_one_is_the_default_pick() {
        let mut set = q("", "Which test runner?", &["Vitest", "Jest"]);
        assert!(first_picks(&set).is_none(), "nothing recommended yet");
        set.recommendations = Some(vec![TabRecs {
            tab: 0,
            options: vec![Rec { label: "Vitest".into(), why: "fast".into(), choice: json!(0) }, Rec { label: "Jest".into(), why: "".into(), choice: json!(1) }],
        }]);
        let picks = first_picks(&set).unwrap();
        assert_eq!((picks.len(), picks[0].choice.clone()), (1, json!(0)));
        assert_eq!(picked_line(&set), "Which test runner? -> Vitest");
    }
}
