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
    /// The set this one replaces: the same dialog, read again (its question
    /// on a tab is known now, say). Its card carries on as this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    /// Where it is in its life (`daemon::card`): `status`, `decision` and
    /// `error` are its words on the wire, written from it.
    #[serde(skip)]
    pub state: crate::daemon::card::SetState,
    /// When the session's terminal was made: the set is for that terminal
    /// (an address can come back for another one).
    #[serde(skip)]
    pub created_ms: Option<u64>,
    /// The Herdr space its session is in: what it asks is one decision per
    /// space (`decisions`).
    #[serde(skip)]
    pub space: Option<String>,
}

/// The same question on both: its title, wording, kind and options (a
/// question not read yet is never the same).
pub fn same_tab(a: &Tab, b: &Tab) -> bool {
    !a.question.trim().is_empty()
        && a.title == b.title
        && same_question(&a.question, &b.question)
        && a.multi == b.multi
        && a.allows_other == b.allows_other
        && a.options.iter().map(|o| &o.label).eq(b.options.iter().map(|o| &o.label))
}

/// `new` is `old`'s dialog read again: the same session, kind and tabs (by
/// title), whatever more is known of their questions now.
pub fn redrawn(old: &QuestionSet, new: &QuestionSet) -> bool {
    old.session == new.session && old.kind == new.kind && old.tabs.iter().map(|t| &t.title).eq(new.tabs.iter().map(|t| &t.title))
}

/// Answers given to `old` are good for `new`: it is the same dialog and every
/// tab they answer asks just what it asked (so nobody answers a question
/// they didn't see).
pub fn answers_still_fit(old: &QuestionSet, new: &QuestionSet, answers: &[Answer]) -> bool {
    redrawn(old, new) && answers.iter().all(|a| matches!((old.tabs.get(a.tab), new.tabs.get(a.tab)), (Some(o), Some(n)) if same_tab(o, n)))
}

/// Answers put together on the card of `shown` mean the same in `other`
/// (another session that asks the same): every tab they answer offers there
/// the options the card showed, in that order. An option is chosen by its
/// place, so the same place under other options is another answer; words
/// that are only typed mean the same anywhere.
pub fn answers_mean_the_same(shown: &QuestionSet, other: &QuestionSet, answers: &[Answer]) -> bool {
    answers.iter().all(|a| {
        let typed = a.choice.as_object().is_some_and(|o| o.get("other").is_some_and(Value::is_string) && !o.contains_key("option") && !o.contains_key("options") && !o.contains_key("spoken"));
        match (shown.tabs.get(a.tab), other.tabs.get(a.tab)) {
            (Some(x), Some(y)) if typed => x.allows_other == y.allows_other,
            (Some(x), Some(y)) => x.multi == y.multi && x.options.iter().map(|o| o.label.trim()).eq(y.options.iter().map(|o| o.label.trim())),
            _ => false,
        }
    })
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

/// A line of the question without the `│ ` gutter Claude Code draws it behind.
fn ungutter(l: &str) -> &str {
    let t = l.trim();
    t.strip_prefix('│').map(str::trim).unwrap_or(t)
}

/// Whether the dialog's question (as read off the screen: wrapped to the
/// terminal's width, maybe cut with `…`) is this question. Spaces don't
/// count: a wrap may fall anywhere.
pub fn same_question(on_screen: &str, question: &str) -> bool {
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let (s, q) = (squash(on_screen), squash(question));
    if s.is_empty() || q.is_empty() {
        return s == q;
    }
    s == q || s.strip_suffix('…').is_some_and(|head| head.chars().count() >= 12 && q.starts_with(head))
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
            let review: Vec<String> = lines[r + 1..end].iter().map(|l| ungutter(l).to_string()).filter(|l| !l.is_empty()).collect();
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
        // The question runs until the first blank line or option: it may wrap
        // over several lines, behind a `│` gutter (newer Claude Code).
        let end = lines[q..].iter().position(|l| l.trim().is_empty() || option_line(l).is_some()).map(|i| q + i).unwrap_or(lines.len());
        let question = lines[q..end].iter().map(|l| ungutter(l)).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
        let options = options_from(&lines, end);
        if options.is_empty() {
            return None;
        }
        return Some(Screen { kind: "ask".into(), tabs, has_submit, question, options, ..Default::default() });
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

/// The set's id: its session, kind, title, detail (the command, the plan)
/// and questions, so a different command or plan is a different set.
fn id_of(session: &str, kind: &str, tabs: &[Tab], title: &str, detail: &str) -> String {
    let mut s = format!("{session}|{kind}|{title}|{detail}");
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
                q.current_tab = q.tabs.iter().position(|t| same_question(&screen.question, &t.question)).unwrap_or(0);
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
    q.id = id_of(session, &q.kind, &q.tabs, &q.title, &q.detail);
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
                Pick { options, other: o.get("other").and_then(Value::as_str).map(clean_text).filter(|t| !t.trim().is_empty()) }
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
    // Typed into a terminal: no control characters (a spoken answer's text included).
    p.other = p.other.map(|t| clean_text(&t)).filter(|t| !t.trim().is_empty());
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

/// Text as it may be typed into a terminal: without control characters
/// (C0, DEL, C1), which a terminal would take as keys or escapes; line
/// breaks and tabs stay (`\r\n` and a lone `\r` become `\n`).
pub fn clean_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").chars().filter(|c| *c == '\n' || *c == '\t' || !c.is_control()).collect()
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
        // `--`: a text that starts with `-` isn't taken for a flag.
        let text = clean_text(text);
        let out = std::process::Command::new(crate::config::hc_bin())
            .args(["send", "--force", "--", &self.address, &text])
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
    set.tabs.iter().position(|t| same_question(&s.question, &t.question))
}

/// `tab_index`, where the dialog should be at `guess`: a question the set
/// doesn't know yet (read from the screen only, before the transcript had it)
/// is taken for that tab, so moving across it works.
fn tab_index_at(s: &Screen, set: &QuestionSet, guess: Option<usize>) -> Option<usize> {
    tab_index(s, set).or_else(|| {
        let g = guess?;
        (s.kind == "ask" && set.tabs.get(g).is_some_and(|t| t.question.trim().is_empty())).then_some(g)
    })
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

/// Moves the dialog to tab `target`; `at`: the tab it should be on now.
fn go_to(term: &mut dyn Term, set: &QuestionSet, target: usize, at: usize) -> Result<Screen, Stopped> {
    let step = if target == set.tabs.len() { "the review tab".to_string() } else { format!("tab {target} ({})", set.tabs[target].title) };
    let mut s = now(term, &step)?;
    // Where the dialog should be, then one step at a time.
    let mut guess = Some(at);
    for _ in 0..(2 * set.tabs.len() + 4) {
        s = leave_text(term, &s, &step)?;
        let Some(cur) = tab_index_at(&s, set, guess) else {
            return Err(Stopped { step, message: "the dialog shows a question that isn't in this set".into() });
        };
        if cur == target {
            return Ok(s);
        }
        let key = if cur < target { "right" } else { "left" };
        let next = if cur < target { cur + 1 } else { cur - 1 };
        term.keys(&[key]).map_err(|e| Stopped { step: step.clone(), message: e.to_string() })?;
        s = expect(term, &step, "the next tab", |x| tab_index_at(x, set, Some(next)) != Some(cur))?;
        guess = Some(next);
    }
    Err(Stopped { step, message: "couldn't reach it with the arrow keys".into() })
}

fn first_line(t: &str) -> String {
    t.lines().next().unwrap_or("").trim().to_string()
}

/// Answers one tab of a Claude question dialog.
fn answer_tab(term: &mut dyn Term, set: &QuestionSet, t: usize, pick: &Pick, at: usize) -> Result<(), Stopped> {
    let tab = &set.tabs[t];
    let step = format!("tab {t} ({})", tab.title);
    let err = |m: String| Stopped { step: step.clone(), message: m };
    let s = go_to(term, set, t, at)?;
    let other_n = tab.options.len() + 1;
    if !tab.multi {
        let n = match &pick.other {
            Some(_) => other_n,
            None => pick.options[0] + 1,
        };
        let single = set.submit_state == "single";
        // Picked: the tab is ticked and the dialog moved on (or, for one question, went away).
        let moved_on = |x: &Screen| tab_index(x, set) != Some(t) || x.tabs.get(t).is_some_and(|(_, a)| *a) && !same_question(&x.question, &tab.question);
        term.keys(&[&n.to_string()]).map_err(|e| err(e.to_string()))?;
        match &pick.other {
            Some(text) => {
                // The number moves to "Type something"; the pasted text (and its Enter) answers.
                expect(term, &step, &format!("the cursor on option {n}"), |x| x.cursor() == Some(n) && same_question(&x.question, &tab.question))?;
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
                    let still = parse_screen(&term.read()).filter(|x| x.kind == "ask" && same_question(&x.question, &tab.question));
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
                if parse_screen(&text).is_none_or(|x| x.kind != "ask" || !same_question(&x.question, &tab.question)) {
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
        if (tab.options.is_empty() || tab.question.trim().is_empty()) && set.kind == "claude-ask" {
            return Err(bad(format!("tab {} ({}) hasn't been read yet: look again in a moment, or answer it in the session", a.tab, tab.title)));
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
    // The tab the dialog is on: where the set was read; a single-choice answer moves it on.
    let mut at = set.current_tab;
    for (t, p) in &picks {
        answer_tab(term, set, *t, p, at)?;
        at = if set.tabs[*t].multi { *t } else { *t + 1 };
    }
    if set.submit_state == "single" || !submit {
        return Ok(());
    }
    let s = go_to(term, set, set.tabs.len(), at)?;
    let step = "the review tab".to_string();
    let answered_all = picks.len() == set.tabs.len() || set.tabs.iter().enumerate().all(|(i, t)| t.answered || picks.iter().any(|(j, _)| *j == i));
    if s.unanswered && answered_all {
        return Err(Stopped { step, message: "it says not every question is answered; nothing sent".into() });
    }
    if s.unanswered {
        return Err(Stopped { step, message: "not every question has an answer yet; nothing sent".into() });
    }
    // Wrapped to the terminal's width, maybe behind a gutter: compared without spaces.
    let squash = |s: &str| s.chars().filter(|c| !c.is_whitespace() && *c != '│').collect::<String>();
    let review = squash(&s.review.join(" "));
    for (t, p) in &picks {
        for want in review_text(&set.tabs[*t], p) {
            if !review.contains(&squash(&want)) {
                return Err(Stopped { step: step.clone(), message: format!("the review doesn't show \"{want}\" for tab {t}; nothing sent") });
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

/// What a message's end waits on the user to do (merge, approve, review,
/// test, decide, log in …), as the sentence that asks it, in plain words.
/// None when it ends without handing anything to the user. A sentence
/// counts when it is addressed to the user ("you", "the user"), names
/// something to do, and asks or waits for it ("please", "must", "once you",
/// "waiting for"); or says it waits on the user outright.
pub fn waits_for_user(message: &str) -> Option<String> {
    let plain = plain_words(message);
    let chars: Vec<char> = plain.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(700)..].iter().collect();
    let sentences: Vec<String> = split_sentences(&tail);
    // The last few sentences: a request in the middle of a long report isn't where it stopped.
    for (i, s) in sentences.iter().enumerate().rev().take(4) {
        if asks_the_user(s) {
            // "… so you need to merge it": what "it" is, from the sentence before.
            let s = match i.checked_sub(1).map(|p| &sentences[p]) {
                Some(prev) if refers_back(s) => format!("{prev} {s}"),
                _ => s.clone(),
            };
            // Links aren't read out: "merge it: https://…/pull/1250" says "merge it".
            let words: Vec<&str> = s.split_whitespace().filter(|w| !w.contains("://")).collect();
            let s = words.join(" ");
            let s = s.trim().trim_start_matches(['-', '*', ' ']).trim_end_matches([':', ',', ' ']).trim();
            let s: String = s.chars().take(300).collect();
            return (!s.is_empty()).then_some(s);
        }
    }
    None
}

/// A turn's end that only says the session is ready: it waits for
/// instructions, has no task yet, stands by. Nothing in it is for the user
/// to do or decide, so on its own it is no wait (`waits_for_user` takes
/// "waiting for your instructions" for one). True when, among the last few
/// sentences, one says so and none hands the user anything else (a merge, a
/// review, a question) or says something is wrong (it is blocked, it failed).
pub fn ready_only(message: &str) -> bool {
    let plain = plain_words(message);
    let chars: Vec<char> = plain.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(700)..].iter().collect();
    let trouble = [
        "blocked", "stuck", "error", "failed", "fails", "failing", "can't", "cannot", "couldn't", "could not", "unable", "missing", "denied", "not allowed",
        "i need", "requires your", "fehlgeschlagen", "fehler",
        // Something didn't go as it should, in other words: the user is told, not left with "is ready".
        "n't", " not ", "rejected", "refused", "timed out", "conflict", "broke", "crash", "wrong", "problem", "abgelehnt", " nicht ",
    ];
    let mut ready = false;
    for s in split_sentences(&tail).iter().rev().take(4) {
        if says_ready(s) {
            ready = true;
            continue;
        }
        let l = s.to_lowercase().replace(['’', '‘'], "'");
        if asks_the_user(s) || s.contains('?') || trouble.iter().any(|w| l.contains(w)) {
            return false;
        }
    }
    ready
}

/// One sentence says the session is ready for instructions, and names
/// nothing for the user to do.
fn says_ready(sentence: &str) -> bool {
    let l = format!(" {} ", sentence.to_lowercase().replace(['’', '‘'], "'").replace([',', ';', ':', '(', ')', '"', '.', '!', '?'], " "));
    let l = l.split_whitespace().collect::<Vec<_>>().join(" ");
    let l = format!(" {l} ");
    // It waits (or is ready) for instructions or a task, in so many words.
    let verbs = ["waiting for", "wait for", "waits for", "waiting on", "wait on", "await", "awaiting", "ready for", "standing by for"];
    let objects = [
        "your instruction", "instruction", "my instruction", "further instruction", "your next instruction", "the next instruction", "more instruction",
        "new instruction", "your next task", "a task", "your task", "the next task", "my next task", "the first task", "your first task", "a new task",
    ];
    let outright = [
        "no task yet", "have no task", "no tasks yet", "don't have a task", "do not have a task", "no instructions yet", "ready when you are", "standing by",
        "tell me what to do", "what would you like me to do", "what would you like me to work on", "what should i work on", "how can i help",
        "auf deine anweisung", "auf anweisungen", "auf weitere anweisungen", "noch keine aufgabe",
    ];
    let ready = outright.iter().any(|p| l.contains(p)) || verbs.iter().any(|v| objects.iter().any(|o| l.contains(&format!(" {v} {o}"))));
    if !ready {
        return false;
    }
    // Something concrete beside it: not only ready.
    let concrete = [
        "merg", " pr ", "pull request", "approv", "review", "test", "deploy", "releas", "decid", "decision", "confirm", "sign off", "sign-off", "log in",
        "login", "sign in", "click", "verify", "check", "look at", "restart", "install", "permission", "access", "credential", "token", "password", "secret",
        "blocked", "stuck", "error", "fail", "can't", "cannot", "unable", "conflict", " which ", "whether", "before i ", " how to ", "freigeb", "freigabe",
        "prüf", "entscheid", "bestätig",
    ];
    let numbered = sentence.split('#').skip(1).any(|r| r.starts_with(|c: char| c.is_ascii_digit()));
    !numbered && !concrete.iter().any(|w| l.contains(w))
}

/// A sentence that names what it is about only by "it", "this", "them":
/// no PR or issue number, link or file of its own.
fn refers_back(sentence: &str) -> bool {
    // A link isn't said, so it doesn't name anything.
    let sentence: String = sentence.split_whitespace().filter(|w| !w.contains("://")).collect::<Vec<_>>().join(" ");
    let l = format!(" {} ", sentence.to_lowercase().replace([',', ';', ':', '.', '!', '?'], " "));
    let pronoun = [" it ", " this ", " that ", " them ", " these ", " es ", " ihn "].iter().any(|w| l.contains(w));
    let concrete = sentence.contains('#') || sentence.contains('/') || l.contains(" pr ") || l.contains(" apk ");
    pronoun && !concrete
}

/// Markdown and code out of the way, links as their text, one line.
fn plain_words(message: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in message.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    // [text](url) → text; **bold** and `code` → their words.
    let mut s = String::new();
    let mut rest = out.as_str();
    while let Some(i) = rest.find("](") {
        let Some(j) = rest[i..].find(')') else { break };
        s.push_str(&rest[..i]);
        rest = &rest[i + j + 1..];
    }
    s.push_str(rest);
    s.replace(['[', '*', '`', '_'], "").replace("  ", " ")
}

/// Sentences: ends at `.`, `!`, `?` before a space, and at line breaks.
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            if !cur.trim().is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.clear();
            continue;
        }
        cur.push(c);
        let next = chars.get(i + 1).copied();
        if matches!(c, '.' | '!' | '?') && next.is_none_or(char::is_whitespace) {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| s.chars().any(char::is_alphanumeric)).collect()
}

/// One sentence hands something to the user.
fn asks_the_user(sentence: &str) -> bool {
    let l = format!(" {} ", sentence.to_lowercase().replace([',', ';', ':', '(', ')', '"', '\''], " "));
    // Outright: it's the user's move.
    let outright = [
        "waiting for you", "waiting on you", "waiting for your", "waiting on your", "over to you", "your turn", "in your court",
        "needs your", "need your", "pending your", "up to you", "ich warte auf dich", "warte auf deine", "du bist dran",
    ];
    // Nothing for them to do.
    let nothing = ["don't need", "do not need", "no need", "nothing for you", "nothing to do", "no action", "you can ignore", "not needed"];
    if nothing.iter().any(|n| l.contains(n)) {
        return false;
    }
    if outright.iter().any(|p| l.contains(p)) {
        return true;
    }
    let to_user = [" you ", " your ", " the user ", " du ", " dich ", " dein ", " deine "].iter().any(|w| l.contains(w));
    let act = [
        "merge", "approv", "review", "test", "decide", "decision", "confirm", "sign off", "sign-off", "log in", "login", "click", "verify",
        "check", "look at", "restart", "install", "freigeb", "freigabe", "prüf", "entscheid", "bestätig", "teste",
    ]
    .iter()
    .any(|w| l.contains(w));
    let ask = [
        " need to ", " needs to ", " must ", " please ", " once you", " when you", " after you", " as soon as you", "ready for you", "for you to ",
        " have to ", " has to ", "can you ", "could you ", " wait for ", " waiting ", " i'll wait", " bitte ", " musst ", " sobald du", " warte ",
    ]
    .iter()
    .any(|w| l.contains(w));
    // "Please merge PR #1250.", "Bitte prüfe …": asked without a "you".
    let imperative = [" please ", " bitte "].iter().any(|w| l.contains(w));
    (to_user && act && ask) || (imperative && act)
}

/// A text set for a turn that waits on the user (`waits_for_user`): its
/// question is the request, and it is the user's from the start (escalated,
/// with why), so the brain isn't asked to answer it.
pub fn wait_set(session: &str, name: &str, message: &str, request: &str, context: &str) -> QuestionSet {
    let mut q = text_set(session, name, message, context);
    q.title = request.to_string();
    if let Some(t) = q.tabs.first_mut() {
        t.question = request.to_string();
    }
    q.state.decision = crate::daemon::card::Decision::Escalated;
    q.reason = WAITS.to_string();
    q
}

/// A wait set's `reason`: what the card says about why it's the user's.
pub const WAITS: &str = "it waits for you to do this";

/// What is said of a turn that waits on the user.
pub fn wait_line(name: &str, request: &str) -> String {
    format!("{name} waits for you: {}", request.trim_end_matches(['.', '!']))
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
    q.id = id_of(session, "text", &[], &q.detail, "");
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
/// overwrites, rewrites git history, runs as root, kills processes, skips
/// the checks, touches infrastructure, secrets or money. None: the brain may
/// answer it itself if it's routine. Words are matched whole ("prod" isn't
/// "prompt", "push" isn't "pushd"); a permission prompt's command is in its
/// title and detail, so the command itself is checked.
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
    let flat = norm(&text);
    let ws: Vec<&str> = flat.split(' ').collect();
    let words = format!(" {flat} ");
    // A word, or words in a row ("go live").
    let word = |w: &str| words.contains(&format!(" {w} "));
    let any = |ws: &[&str]| ws.iter().any(|w| word(w));
    let raw_any = |ps: &[&str]| ps.iter().any(|p| raw.contains(p));
    // Word `a` with the next word as `f` wants it.
    let then = |a: &str, f: &dyn Fn(Option<&str>) -> bool| ws.iter().enumerate().any(|(i, w)| *w == a && f(ws.get(i + 1).copied()));
    // A Yes/No about one of these is the same as doing it.
    // A release build (`--release`) or release notes aren't a release.
    let released = {
        let r = ["--release", "release notes", "release build", "release mode", "release profile"]
            .iter()
            .fold(raw.clone(), |r, p| r.replace(p, " "));
        let r = format!(" {} ", norm(&r));
        r.contains(" release ") || r.contains(" releasing ")
    };
    if any(&[
        "deploy",
        "deploys",
        "deploying",
        "deployment",
        "redeploy",
        "production",
        "prod",
        "go live",
        "rollout",
        "publish",
        "publishing",
    ]) || released
    {
        return Some("it deploys or releases");
    }
    // "push", but not `git stash push` or push notifications.
    let pushes = ws.iter().enumerate().any(|(i, w)| {
        matches!(*w, "push" | "pushes" | "pushing")
            && !(i > 0 && ws[i - 1] == "stash")
            && !ws.get(i + 1).is_some_and(|n| n.starts_with("notif"))
    });
    let merges = any(&["merge", "merges", "merging"])
        && any(&[
            "pr",
            "prs",
            "pull",
            "mr",
            "git",
            "branch",
            "main",
            "master",
            "prod",
            "production",
            "develop",
            "dev",
            "release",
            "into",
            "upstream",
            "origin",
            "remote",
        ]);
    if pushes
        || merges
        || raw_any(&[
            "git push",
            "push to",
            "push the",
            "push origin",
            "force push",
            "--force",
            "merge into main",
            "merge to main",
            "into main",
            "into master",
            "merge into prod",
            "into prod",
        ])
    {
        return Some("it pushes or merges to a shared branch");
    }
    // "remove" alone is often routine (an import); with files or data it isn't.
    let removes = any(&["remove", "removes", "removing"])
        && any(&[
            "file",
            "files",
            "folder",
            "folders",
            "directory",
            "directories",
            "dir",
            "dirs",
            "data",
            "database",
            "db",
            "table",
            "tables",
            "branch",
            "branches",
            "tag",
            "tags",
            "repo",
            "repository",
            "old",
            "everything",
            "backup",
            "backups",
            "logs",
            "records",
            "rows",
            "users",
            "accounts",
            "volume",
            "volumes",
            "container",
            "containers",
            "images",
        ]);
    if removes
        || any(&[
            "delete",
            "deletes",
            "deleting",
            "remove the branch",
            "drop",
            "truncate",
            "overwrite",
            "overwrites",
            "wipe",
            "destroy",
            "purge",
            "prune",
            "irreversible",
            "rm",
            "rmdir",
            "rmi",
            "unlink",
            "shred",
        ])
        || raw_any(&[
            "rm -rf",
            "rm -r ",
            "reset --hard",
            "drop table",
            "drop database",
            "branch -d",
            "branch -d ",
            "git clean",
            "git checkout -- ",
            "git checkout .",
            "git checkout -f",
            "git restore",
            "git reset",
            "git stash drop",
            "git stash clear",
            "delete from",
            "down -v",
        ])
    {
        return Some("it deletes or overwrites");
    }
    // A rebase (not one going on: --continue, --abort) or a history filter.
    if then("rebase", &|n| !matches!(n, Some("continue" | "abort" | "skip" | "quit"))) || raw_any(&["filter-branch", "filter-repo"]) {
        return Some("it rewrites git history");
    }
    if any(&["sudo", "doas", "chown", "chgrp"])
        || raw_any(&["chmod -r", "chmod --recursive", "chmod 777", "chmod 666", "chmod a+w", "chmod o+w"])
    {
        return Some("it runs as root or changes permissions");
    }
    if any(&["pkill", "killall", "taskkill"])
        || then("kill", &|n| n.is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()) || n.starts_with("sig")))
        || raw_any(&["kill -", "kill $(", "docker kill"])
    {
        return Some("it kills processes");
    }
    if raw_any(&["--no-verify"]) {
        return Some("it skips the checks");
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

/// A menu option that answers more than this once: it turns on auto mode,
/// always allows, stops asking, or holds for the rest of the session.
pub fn widens(o: &Opt) -> bool {
    let t = format!(" {} ", norm(&format!("{} {}", o.label, o.description).replace('’', "'")));
    [
        "auto mode",
        "automode",
        "auto accept",
        "accept edits",
        "accept all",
        "allow all",
        "always",
        "don t ask",
        "dont ask",
        "do not ask",
        "never ask",
        "this session",
        "the session",
        "bypass",
        "skip permissions",
        "yolo",
    ]
    .iter()
    .any(|p| t.contains(&format!(" {p} ")))
}

/// A plain one-time yes: "Yes", "Yes, proceed (y)", "Yes, manually approve edits".
fn plain_yes(o: &Opt) -> bool {
    let l = norm(&o.label);
    (l == "yes" || l.starts_with("yes ")) && !widens(o)
}

/// The tower swapped the brain's pick for the plain yes.
#[derive(Debug, Clone, PartialEq)]
pub struct Swap {
    pub from: String,
    pub to: String,
}

/// What the tower may answer by itself on a menu (permission, plan, Codex):
/// only a plain one-time yes. A pick that would stop future prompts (auto
/// mode, always allow, don't ask again) becomes the plain yes when there is
/// one (the swap is returned), else the answer goes to the user (Err: why).
/// Claude's questions and text replies are taken as they are.
pub fn safe_auto(set: &QuestionSet, answers: &[Answer]) -> Result<(Vec<Answer>, Option<Swap>), String> {
    if matches!(set.kind.as_str(), "claude-ask" | "text") {
        return Ok((answers.to_vec(), None));
    }
    let Some(tab) = set.tabs.first() else { return Err("the prompt has no options".into()) };
    let Some(a) = answers.iter().find(|a| a.tab == 0) else { return Err("no answer for the prompt".into()) };
    let p = pick_of(a, tab).map_err(|e| e.to_string())?;
    let (Some(&i), None, 1) = (p.options.first(), &p.other, p.options.len()) else {
        return Err("this prompt takes one of its options".into());
    };
    let o = &tab.options[i];
    if plain_yes(o) {
        return Ok((answers.to_vec(), None));
    }
    if !widens(o) {
        return Err(format!("only a plain yes is answered for you on a prompt like this, and it would pick \"{}\"", o.label));
    }
    match tab.options.iter().position(plain_yes) {
        Some(j) => {
            let swap = Swap { from: o.label.clone(), to: tab.options[j].label.clone() };
            Ok((vec![Answer { tab: 0, choice: json!(j), attachments: vec![] }], Some(swap)))
        }
        None => Err(format!("\"{}\" would stop future prompts, and there is no plain yes", o.label)),
    }
}

/// The decision that holds: the user's policy first, then the rule, then the brain.
/// `policy`: `normal`, `keep-moving` or `ask-everything`. On a menu the
/// brain's first recommendation must be one the tower may give (`safe_auto`),
/// whatever the policy.
pub fn decide(set: &QuestionSet, brain: Option<&str>, brain_reason: &str, policy: &str) -> (String, String) {
    if policy == "ask-everything" {
        return ("escalate".into(), "you asked to decide everything yourself".into());
    }
    if let Some(why) = must_escalate(set) {
        return ("escalate".into(), why.into());
    }
    match brain {
        Some("auto") => {
            if let Some(answers) = first_picks(set)
                && let Err(why) = safe_auto(set, &answers)
            {
                return ("escalate".into(), why);
            }
            ("auto".into(), brain_reason.to_string())
        }
        _ => ("escalate".into(), if brain_reason.is_empty() { "it needs your call".into() } else { brain_reason.to_string() }),
    }
}

/// The answers the tower gives by itself: recommendation 1 for every question
/// (on a menu, checked with `safe_auto` before it is typed).
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
        /// Questions wrapped to this width behind a `│ ` gutter, as newer Claude Code draws them.
        wrap: Option<usize>,
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
                wrap: None,
            }
        }
        /// Two single-choice tabs with long questions, drawn wrapped behind a gutter
        /// (the shape of the dialog that once stopped with "a question that isn't in this set").
        fn two_long() -> Sim {
            let t = |title, q, opts: Vec<&'static str>| SimTab { title, q, checked: vec![false; opts.len()], opts, multi: false, picked: None, other: None, other_on: false };
            Sim {
                tabs: vec![
                    t(
                        "Commit",
                        "All the tower fixes, the new Herdr/Claude/Codex hooks and the queue/dispatch work are uncommitted on `main` (36 changed files, 4 new ones). Should I commit them?",
                        vec!["Commit on a new branch (Recommended)", "Commit on main", "Don't commit"],
                    ),
                    t(
                        "Unmute test",
                        "Your tower is muted. To hear the \"While you were muted: N updates\" line on the real tower, I'd unmute it after an alert arrives while muted, so it speaks aloud on the Mac or your phone. It has already been verified silently on a test daemon. Do you want it run on your real tower?",
                        vec!["Yes, unmute and test", "No, leave it muted"],
                    ),
                ],
                cur: 0,
                cursor: 1,
                editing: false,
                submitted: false,
                log: vec![],
                wrap: Some(60),
            }
        }
        /// Text as the dialog draws it: as is, or wrapped behind the gutter.
        fn draw(&self, text: &str, indent: &str) -> String {
            let Some(w) = self.wrap else { return format!("{indent}{text}\n") };
            let mut out = String::new();
            let mut line = String::new();
            for word in text.split(' ') {
                if !line.is_empty() && line.len() + 1 + word.len() > w {
                    out += &format!("│ {line}\n");
                    line.clear();
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
            out + &format!("│ {line}\n")
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
                    s += &self.draw(t.q, " ● ");
                    s += &self.draw(&format!("→ {}", a.join(", ")), "   ");
                }
                s += &format!("\nReady to submit your answers?\n\n{} 1. Submit answers\n{} 2. Cancel\n", c(self.cursor == 1), c(self.cursor == 2));
                return s;
            }
            let t = &self.tabs[self.cur];
            s += &self.draw(t.q, "");
            s += "\n";
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
        assert!(e.step.starts_with("tab 1 (Features)"), "counted from 0, as answers are: {e}");
        assert!(!stuck.0.submitted);
        // A set that doesn't match the dialog at all.
        let mut other = set.clone();
        other.tabs[0].question = "Something else?".into();
        let mut sim = Sim::new();
        assert!(drive(&mut sim, &other, &[Answer { tab: 0, choice: json!(0), attachments: vec![] }], true).is_err());
    }

    fn two_long_tabs() -> Vec<Tab> {
        Sim::two_long()
            .tabs
            .iter()
            .map(|t| Tab {
                title: t.title.into(),
                question: t.q.into(),
                options: t.opts.iter().enumerate().map(|(i, o)| Opt { label: o.to_string(), description: String::new(), key: (i + 1).to_string() }).collect(),
                allows_other: true,
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn reads_a_question_wrapped_behind_a_gutter() {
        // A real screen (Claude Code 2.1.319): the question wraps over two lines behind `│ `.
        let s = parse_screen(&fixture("claude-ask-gutter-wrapped.txt")).unwrap();
        assert_eq!(s.kind, "ask");
        assert_eq!(s.tabs, vec![("Commit".into(), false), ("Unmute test".into(), false)]);
        assert_eq!(s.question, "All the tower fixes, the new Herdr/Claude/Codex hooks and the queue/dispatch work are uncommitted on `main` (36 changed files, 4 new ones). Should I commit them?");
        assert_eq!(s.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Commit on a new branch (Recommended)", "Commit on main", "Don't commit", "Type something."]);
        assert!(s.options[0].description.starts_with("Create a branch"));
        let set = build("t41", "review", false, &s, Some(two_long_tabs()), "", "");
        assert!(set.exact);
        assert_eq!((set.current_tab, tab_index(&s, &set)), (0, Some(0)), "the dialog's question is tab 0 of the set");
    }

    #[test]
    fn questions_match_however_the_screen_wraps_them() {
        let q = "Should I commit the 36 changed files on `main`?";
        assert!(same_question("Should I commit the 36 changed\nfiles on `main`?", q));
        assert!(same_question("Should I commit the 36 changed fi les on `main`?", q), "a wrap inside a word");
        assert!(same_question("Should I commit the 36…", q), "cut short");
        assert!(!same_question("Should I…", "Should I push?"), "too short to tell");
        assert!(!same_question("Should I commit the 36 changed files on `dev`?", q));
        assert!(!same_question("", q));
    }

    #[test]
    fn answers_two_long_questions_drawn_behind_a_gutter_and_submits() {
        let mut sim = Sim::two_long();
        let set = build("t41", "review", false, &parse_screen(&sim.render()).unwrap(), Some(two_long_tabs()), "", "");
        assert_eq!(set.current_tab, 0);
        let answers = vec![Answer { tab: 0, choice: json!(0), attachments: vec![] }, Answer { tab: 1, choice: json!(0), attachments: vec![] }];
        drive(&mut sim, &set, &answers, true).unwrap_or_else(|e| panic!("{e} ({:?})", sim.log));
        assert!(sim.submitted, "{:?}", sim.log);
        assert_eq!((sim.tabs[0].picked, sim.tabs[1].picked), (Some(0), Some(0)));

        // A typed answer, long enough to wrap on the review tab, is still found there.
        let mut sim = Sim::two_long();
        let long = "Commit on a branch named tower-hooks-and-fixes, then show me the diff stat before anything else";
        let answers = vec![Answer { tab: 0, choice: json!({"other": long}), attachments: vec![] }, Answer { tab: 1, choice: json!(1), attachments: vec![] }];
        drive(&mut sim, &set, &answers, true).unwrap_or_else(|e| panic!("{e} ({:?})", sim.log));
        assert!(sim.submitted);
        assert_eq!(sim.tabs[0].other.as_deref(), Some(long));
    }

    #[test]
    fn crosses_a_tab_whose_question_isnt_read_yet() {
        // The set as read from the screen alone: tab 1 has no question yet.
        let mut sim = Sim::two_long();
        let set = build("t41", "review", false, &parse_screen(&sim.render()).unwrap(), None, "", "");
        assert!(!set.exact && set.tabs[1].question.is_empty());
        let a0 = Answer { tab: 0, choice: json!(0), attachments: vec![] };
        // Tab 0 goes in; on the way to the review it passes tab 1, then stops there
        // because it has no answer: not "a question that isn't in this set".
        let e = drive(&mut sim, &set, std::slice::from_ref(&a0), true).unwrap_err();
        assert_eq!(sim.tabs[0].picked, Some(0));
        assert!(e.message.contains("not every question"), "{e}");
        assert!(!sim.submitted);
        // Answering the unread tab is refused before any key, and says why.
        let mut sim = Sim::two_long();
        let e = drive(&mut sim, &set, &[a0, Answer { tab: 1, choice: json!({"other": "Skip"}), attachments: vec![] }], true).unwrap_err();
        assert!(e.message.contains("hasn't been read yet"), "{e}");
        assert!(sim.log.is_empty(), "nothing typed: {:?}", sim.log);
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
            (q("", "push?", &[]), "it pushes or merges to a shared branch"),
            (q("", "Commit and push these?", &[]), "it pushes or merges to a shared branch"),
            (q("", "Merge the PR?", &[]), "it pushes or merges to a shared branch"),
            (q("Allow Bash: gh pr merge 12?", "Do you want to proceed?", &["Yes", "No"]), "it pushes or merges to a shared branch"),
            (q("", "Should I remove the old files?", &[]), "it deletes or overwrites"),
            (q("Allow Bash: rm db.sqlite?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: rm -fr build?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: git clean -fd?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: git checkout -- .?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: git reset HEAD~3?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: git restore src/app.rs?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: docker system prune -a?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: docker rm web?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: psql -c 'DROP SCHEMA app CASCADE'?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: sqlite3 app.db 'DELETE FROM users'?", "Do you want to proceed?", &["Yes", "No"]), "it deletes or overwrites"),
            (q("Allow Bash: git rebase -i HEAD~5?", "Do you want to proceed?", &["Yes", "No"]), "it rewrites git history"),
            (q("", "Should I rebase onto main?", &[]), "it rewrites git history"),
            (q("Allow Bash: sudo make install?", "Do you want to proceed?", &["Yes", "No"]), "it runs as root or changes permissions"),
            (q("Allow Bash: chmod -R 755 .?", "Do you want to proceed?", &["Yes", "No"]), "it runs as root or changes permissions"),
            (q("Allow Bash: chown me:staff app?", "Do you want to proceed?", &["Yes", "No"]), "it runs as root or changes permissions"),
            (q("Allow Bash: kill -9 4242?", "Do you want to proceed?", &["Yes", "No"]), "it kills processes"),
            (q("Allow Bash: pkill node?", "Do you want to proceed?", &["Yes", "No"]), "it kills processes"),
            (q("Allow Bash: npm publish?", "Do you want to proceed?", &["Yes", "No"]), "it deploys or releases"),
            (q("Allow Bash: cargo publish?", "Do you want to proceed?", &["Yes", "No"]), "it deploys or releases"),
            (q("Allow Bash: git commit --no-verify -m wip?", "Do you want to proceed?", &["Yes", "No"]), "it skips the checks"),
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
            q("Allow Bash: cargo build?", "Do you want to proceed?", &["Yes", "No"]),
            q("Allow Bash: cargo build --release?", "Do you want to proceed?", &["Yes", "No"]),
            q("Allow Bash: git status?", "Do you want to proceed?", &["Yes", "No"]),
            q("Allow Bash: git diff?", "Do you want to proceed?", &["Yes", "No"]),
            q("Allow Bash: git rebase --continue?", "Do you want to proceed?", &["Yes", "No"]),
            q("Allow Bash: ls -la?", "Do you want to proceed?", &["Yes", "No"]),
            q("Allow Bash: pushd src && cargo test?", "Do you want to proceed?", &["Yes", "No"]),
            q("Allow Bash: chmod +x scripts/run.sh?", "Do you want to proceed?", &["Yes", "No"]),
            q("", "Should I tighten the prompt for the summary?", &[]),
            q("", "Want me to remove the unused import?", &[]),
            q("", "Should I add push notifications to the settings page?", &[]),
        ] {
            assert_eq!(must_escalate(&set), None, "{}", set.tabs[0].question);
            assert_eq!(decide(&set, Some("auto"), "routine choice", "normal"), ("auto".into(), "routine choice".into()));
            assert_eq!(decide(&set, Some("escalate"), "", "normal").0, "escalate", "the brain may still ask");
            assert_eq!(decide(&set, None, "", "normal").0, "escalate", "no word from the brain: ask");
            assert_eq!(decide(&set, Some("auto"), "routine", "ask-everything").0, "escalate", "the user's toggle wins");
        }
    }

    fn screen_set(name: &str, msg: &str) -> QuestionSet {
        build("t1", "x", name.starts_with("codex"), &parse_screen(&fixture(name)).unwrap(), None, msg, "")
    }

    fn rec(set: &mut QuestionSet, choice: usize) {
        let label = set.tabs[0].options[choice].label.clone();
        set.recommendations = Some(vec![TabRecs { tab: 0, options: vec![Rec { label, why: "".into(), choice: json!(choice) }] }]);
    }

    #[test]
    fn real_prompts_are_routine() {
        for (name, msg) in [
            ("claude-permission.txt", "approve Bash: touch /tmp/qfix-perm-test"),
            ("claude-plan.txt", "approve plan"),
            ("codex-approval.txt", ""),
        ] {
            assert_eq!(must_escalate(&screen_set(name, msg)), None, "{name}");
        }
    }

    #[test]
    fn the_tower_never_turns_prompts_off() {
        let a = |i: usize| vec![Answer { tab: 0, choice: json!(i), attachments: vec![] }];
        let swap = |from: &str, to: &str| Some(Swap { from: from.into(), to: to.into() });
        // Claude's permission prompt: 1 Yes, 2 always allow /tmp, 3 auto mode, 4 No.
        let p = screen_set("claude-permission.txt", "approve Bash: touch /tmp/qfix-perm-test");
        assert_eq!(safe_auto(&p, &a(0)), Ok((a(0), None)));
        let always = p.tabs[0].options[1].label.clone();
        assert_eq!(safe_auto(&p, &a(1)), Ok((a(0), swap(&always, "Yes"))));
        assert_eq!(safe_auto(&p, &a(2)).unwrap().0, a(0), "auto mode: the plain yes instead");
        assert!(safe_auto(&p, &a(3)).is_err(), "only a yes is given for the user");
        // The plan: 1 auto mode, 2 approve edits by hand, 3 tell Claude.
        let plan = screen_set("claude-plan.txt", "approve plan");
        assert_eq!(safe_auto(&plan, &a(0)), Ok((a(1), swap("Yes, and use auto mode", "Yes, manually approve edits"))));
        assert!(safe_auto(&plan, &a(2)).is_err());
        // Codex: 1 yes, 2 don't ask again, 3 no.
        let c = screen_set("codex-approval.txt", "");
        assert_eq!(safe_auto(&c, &a(1)).unwrap().0, a(0));
        // No plain yes to fall back on: it goes to the user.
        let only = Opt { label: "Yes, and don't ask again for this command".into(), key: "1".into(), ..Default::default() };
        let mut menu = p.clone();
        menu.tabs[0].options = vec![only, Opt { label: "No".into(), key: "2".into(), ..Default::default() }];
        assert!(safe_auto(&menu, &a(0)).is_err());
        for l in [
            "Yes, allow all edits during this session (shift+tab)",
            "Yes, and auto-accept edits",
            "Yes, and bypass permissions",
            "Yes, don’t ask again",
        ] {
            assert!(widens(&Opt { label: l.into(), ..Default::default() }), "{l}");
        }
        assert!(!widens(&Opt { label: "Yes, proceed (y)".into(), ..Default::default() }));
        // Claude's own questions are taken as they are.
        let ask = q("", "Always use tabs?", &["Always", "Never"]);
        assert_eq!(safe_auto(&QuestionSet { kind: "claude-ask".into(), ..ask }, &a(0)).unwrap().0, a(0));

        // The brain's decision on a menu, whatever the policy.
        for policy in ["normal", "keep-moving"] {
            let mut p = p.clone();
            rec(&mut p, 2);
            assert_eq!(decide(&p, Some("auto"), "routine", policy).0, "auto", "swapped for the plain yes when typed");
            rec(&mut p, 3);
            assert_eq!(decide(&p, Some("auto"), "routine", policy).0, "escalate", "No: the user's");
            rec(&mut menu, 0);
            assert_eq!(decide(&menu, Some("auto"), "routine", policy).0, "escalate", "nothing safe to pick");
        }
    }

    #[test]
    fn typed_text_has_no_control_characters() {
        assert_eq!(clean_text("ok\x1b[2J\x07 then\r\nmore\tx\u{9b}31m\u{7f}"), "ok[2J then\nmore\tx31m");
        let tab = Tab { allows_other: true, ..Default::default() };
        let p = pick_of(&Answer { tab: 0, choice: json!({"other": "fine\x1b[A"}), attachments: vec![] }, &tab).unwrap();
        assert_eq!(p.other.as_deref(), Some("fine[A"));
        assert!(
            pick_of(&Answer { tab: 0, choice: json!({"other": "\x1b\x03"}), attachments: vec![] }, &tab).is_err(),
            "nothing left: no answer"
        );
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

    /// The missed release: t58 ended a turn waiting for the user to merge
    /// the release PR, with no question mark, and nobody was told.
    #[test]
    fn a_turn_that_waits_for_the_user_says_what_they_must_do() {
        let pr = "Dev is healthy: `GET /health` 200, and the old widget path still gets 409 `case_exists` with its `case` field.\n\n\
                  PR #1250 (dev to main) is open and its checks passed. The merge command is blocked for me, so you need to merge it: \
                  https://github.com/eyzhub/eyz-mediaroom-serverless/pull/1250. I'll wait, then verify prod.";
        assert!(!looks_like_question(pr), "not a question: the old path missed it");
        assert_eq!(
            waits_for_user(pr).as_deref(),
            Some("PR #1250 (dev to main) is open and its checks passed. The merge command is blocked for me, so you need to merge it")
        );
        for (msg, want) in [
            ("Plan is ready. Waiting for your approval.", "Waiting for your approval."),
            ("Once you merge #1250 into main, I'll verify the production deployment.", "Once you merge #1250 into main, I'll verify the production deployment."),
            ("All green on dev.\n\nPlease merge [PR #1250](https://github.com/x/y/pull/1250) when you're ready.", "Please merge PR #1250 when you're ready."),
            ("Fertig auf dev. Bitte merge den PR #1250, dann prüfe ich prod.", "Bitte merge den PR #1250, dann prüfe ich prod."),
            ("The new APK is at /tmp/app.apk. You need to install it on the phone and test the header.", "The new APK is at /tmp/app.apk. You need to install it on the phone and test the header."),
            ("Fix is in. Please review PR #77 before I deploy.", "Please review PR #77 before I deploy."),
        ] {
            assert_eq!(waits_for_user(msg).as_deref(), Some(want), "{msg}");
        }
    }

    #[test]
    fn a_turn_that_only_reports_doesnt_wait_for_the_user() {
        for msg in [
            // t58's earlier turn: it waits on a deploy, not on the user.
            "The dev deploy of #1248 is running (run 42645505320), and I'll get a notice when it finishes. Nothing is checked yet.",
            "I merged PR #1248 and verified dev; prod is healthy.",
            "Done. You don't need to do anything: the release is live and verified.",
            "I'll wait for the deploy to finish, then check the logs.",
            "If you want, I can also check the SDK later.",
            "The user asked me to verify prod; it's verified and the smoke request passed.",
            "Please note: the build takes 4 minutes.\nIt finished and all tests pass.\nThe APK is in the shots.\nNothing else changed.\nAll good.",
        ] {
            assert_eq!(waits_for_user(msg), None, "{msg}");
        }
    }

    /// Idle is not blocked: a session that only says it is ready waits for
    /// nothing the user has to do. (The first four are from the tower's own
    /// log, where each became a "waits for you" alert.)
    #[test]
    fn a_turn_that_only_says_it_is_ready_is_not_a_wait() {
        for msg in [
            "I'm waiting for your instructions",
            "I have no task yet and I'm waiting for your instructions",
            "I'm on dev and waiting for your instructions",
            "First task: read CLAUDE.md and README.md, then wait for your instructions",
            "I've read CLAUDE.md and README.md.\n\nI'm on `dev`, the tree is clean. Waiting for your instructions.",
            "Understood: the web session is t5 and the api session is t7. I have no task yet.",
            "Ready when you are.",
            "Standing by for your next task.",
            "Got it. Awaiting further instructions.",
            "I know who the others are now. What would you like me to work on?",
            "Alles klar. Ich warte auf deine Anweisungen.",
        ] {
            assert!(ready_only(msg), "{msg}");
        }
        // Real waits are never swallowed: something there is the user's to do.
        for msg in [
            "waits for you to merge PR #1250",
            "Still waiting on you: merging #1273 into dev",
            "PR #1250 (dev to main) is open and its checks passed. The merge command is blocked for me, so you need to merge it. I'll wait.",
            "Plan is ready. Waiting for your approval.",
            "I'm waiting for your instructions on whether to merge #1273 into dev.",
            "I'm blocked on the migration and waiting for your instructions.",
            "Waiting for your instructions before I deploy.",
            "I'm waiting for your instructions. Also, please review PR #77.",
            "Should I open the PR now? I'm waiting for your instructions.",
            "The new APK is at /tmp/app.apk. You need to install it on the phone and test the header.",
            "Waiting for your review.",
            "Ready for you to test.",
            "Blocked: the AWS credentials are missing. Waiting for your instructions.",
            "The build failed on the lint step. I'm waiting for your instructions.",
            "The staging deploy was rejected with a 403. What would you like me to do?",
            "The migration didn't apply. I'm waiting for your instructions.",
            "The push was refused by the remote. Standing by.",
            "Two files are in conflict after the rebase. Ready when you are.",
            "The request timed out twice. Awaiting further instructions.",
        ] {
            assert!(!ready_only(msg), "{msg}");
        }
        // What merely happened is neither.
        for msg in ["I merged PR #1248 and verified dev; prod is healthy.", "Done. All tests pass.", "I created a branch for the task and pushed it.", ""] {
            assert!(!ready_only(msg), "{msg}");
        }
    }
}
