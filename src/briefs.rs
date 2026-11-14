//! Where are we, in a few plain sentences: a living brief of every space and
//! every session, for the user who doesn't watch the screens.
//!
//! - **A session's**: what it was asked (its agenda's latest task), what it
//!   has done (tasks verified, with how; or that it says so, not checked
//!   yet), what it is doing now (its live progress line when the tower has
//!   one, else its task, or that it waits for the user, or is idle and what
//!   it last said), what's next (its queued tasks) and what it is stuck on
//!   (a task stalled or failed, a decision of the user's it waits on).
//! - **A space's**: its goal and stage (state, its tasks done of all), what
//!   is done (verified, with how), what is in progress and by whom (its
//!   sessions' "now"), what is next, what waits for the user (its
//!   decisions), and the risks found so far (tasks failed or stalled, PRs
//!   with conflicts, failing checks or breaking changes), and its release
//!   track in a line.
//! - **Composed, never generated**: from the data the tower keeps (the
//!   agenda, the decisions, the PRs, the release track, the sessions' status
//!   and last words); no model call, so it can be rewritten at every look
//!   (a turn's end, a task's change) for nothing.
//! - **Its history**: a version is kept each time what it says changes,
//!   apart from what is happening this moment (the "now" line moves all the
//!   time), the last `KEPT` of each (`History`).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agenda::{Task, TaskState};

/// Versions kept per brief.
pub const KEPT: usize = 8;
/// What is "done" is what was done lately: older work is in the record.
pub const DONE_WITHIN_MS: u64 = 24 * 60 * 60_000;

/// A session's brief.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionBrief {
    /// The whole of it, in a few sentences.
    pub text: String,
    pub asked: String,
    /// What came of what it was asked last, in its own words.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub result: String,
    /// What else it finished lately, each in its own words.
    pub done: Vec<String>,
    pub now: String,
    pub next: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stuck: Option<String>,
    /// When what it says last changed (its "now" aside).
    pub changed_ms: u64,
    /// Its earlier versions, newest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Version>,
}

/// A space's brief.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpaceBrief {
    pub text: String,
    pub goal: String,
    pub stage: String,
    pub done: Vec<String>,
    pub doing: Vec<String>,
    pub next: Vec<String>,
    pub waits: Vec<String>,
    pub risks: Vec<String>,
    pub changed_ms: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Version>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Version {
    pub at_ms: u64,
    pub text: String,
}

/// What a session's brief is made of.
pub struct SessionFacts<'a> {
    /// Herdr's word: working, blocked, done, idle …
    pub status: &'a str,
    /// Its last words, as its hooks said them.
    pub said: Option<&'a str>,
    /// Its agenda tasks, any order.
    pub tasks: Vec<&'a Task>,
    /// The titles of the user's decisions it waits on.
    pub waits_on: Vec<String>,
    /// Its live progress line (step N of M, …), when the tower has one.
    pub progress: Option<&'a str>,
    /// Its own task list's items still to do (the progress's pending items).
    pub upcoming: Vec<String>,
    pub now: u64,
}

/// What a space's brief is made of.
pub struct SpaceFacts<'a> {
    pub goal: &'a str,
    /// The mission's state: open, ready, quiet.
    pub state: &'a str,
    pub standing: bool,
    /// Its sessions' names and briefs, and whether each works now.
    pub sessions: Vec<(String, &'a SessionBrief, bool)>,
    /// Its agenda tasks.
    pub tasks: Vec<&'a Task>,
    /// The titles of the user's decisions in it.
    pub waits: Vec<String>,
    /// Its PRs' troubles, a line each ("PR #841 has conflicts with dev").
    pub pr_risks: Vec<String>,
    /// Its release track in a line ("prod 14:20 · 1 on dev"), when it has one.
    pub release: Option<String>,
    pub now: u64,
}

pub fn session(f: &SessionFacts) -> SessionBrief {
    let mut tasks = f.tasks.clone();
    tasks.sort_by_key(|t| t.created_at_ms);
    let recent = |t: &&&Task| f.now.saturating_sub(t.since_ms) <= DONE_WITHIN_MS;
    // The latest it took up (a queued one is "next"), else the latest.
    let latest = tasks.iter().rev().find(|t| t.state != TaskState::Queued).or(tasks.last()).copied();
    // What it was asked last, and where that stands.
    let asked = latest
        .map(|t| match state_word(t) {
            "" => title(t),
            w => format!("{} ({w})", title(t)),
        })
        .unwrap_or_default();
    // What came of it, in its own words (its turn's end), and how it was checked.
    // (Only when it says more than the task's own title.)
    let result = latest
        .filter(|t| matches!(t.state, TaskState::Verified | TaskState::DoneClaimed))
        .map(|t| outcome(t))
        .filter(|o| latest.is_none_or(|t| !o.starts_with(&title(t))))
        .unwrap_or_default();
    // What else it finished lately.
    let done: Vec<String> =
        tasks.iter().rev().filter(recent).filter(|t| t.state == TaskState::Verified && Some(**t) != latest).map(|t| outcome(t)).collect();
    let running = tasks.iter().rev().find(|t| t.state == TaskState::Running);
    let blocked = tasks.iter().rev().find(|t| t.state == TaskState::Blocked);
    let now = match (f.progress.filter(|p| !p.trim().is_empty()), f.status) {
        (Some(p), "working") => p.trim().to_string(),
        // On what it was asked (said just above): "working on it".
        (_, "working") => match running {
            Some(t) if latest.is_some_and(|l| std::ptr::eq(l, *t)) => "working on it".into(),
            Some(t) => format!("working on {}", lower_first(&title(t))),
            None => "working".into(),
        },
        (_, "blocked") => match (f.waits_on.first(), blocked) {
            (Some(d), _) => format!("waiting for you: {}", clip(d, 120)),
            (None, Some(t)) if !t.question.is_empty() => format!("waiting for you: {}", clip(&t.question, 120)),
            _ => "waiting on a prompt in its terminal".into(),
        },
        // Idle: what it last said, unless that is the result already told.
        _ => match f.said.map(first_sentence).filter(|s| !s.is_empty() && result.is_empty()) {
            Some(s) => format!("idle; it last said: {}", clip(&s, 140)),
            None => "idle".into(),
        },
    };
    let mut next: Vec<String> = tasks.iter().filter(|t| t.state == TaskState::Queued).map(|t| title(t)).collect();
    next.extend(f.upcoming.iter().map(|u| clip(u, 90)));
    let mut stuck: Vec<String> =
        tasks.iter().rev().filter(recent).filter(|t| matches!(t.state, TaskState::Stalled | TaskState::Failed)).map(|t| why_line(t)).collect();
    // A decision it waits on while it isn't blocked (a wait at the end of its turn).
    if f.status != "blocked" {
        stuck.extend(f.waits_on.iter().map(|d| format!("waits for your answer: {}", clip(d, 120))));
    }
    let stuck = stuck.into_iter().next();
    let mut s = SessionBrief { asked, result, done, now, next, stuck, ..Default::default() };
    s.text = session_text(&s);
    s
}

/// A task's state in a word or two for "Asked: … (done)"; nothing while it runs (its "now" says).
fn state_word(t: &Task) -> &'static str {
    match t.state {
        TaskState::Verified => "done",
        TaskState::DoneClaimed => "it says done; not checked yet",
        TaskState::Failed => "failed",
        TaskState::Stalled => "stalled",
        TaskState::Queued => "queued",
        TaskState::Blocked => "waiting for you",
        TaskState::Abandoned => "dropped",
        TaskState::Running => "",
    }
}

/// What came of a task: the first sentence of what its session said at the
/// end of it (its title when that says nothing), and "checked by the brain"
/// when the brain verified it (the evidence is then the brain's, not only its word).
fn outcome(t: &Task) -> String {
    // The tower's own note in its place ("(its turn ended before …)"): nothing it said.
    let said = if t.said.trim_start().starts_with('(') { String::new() } else { first_sentence(&t.said) };
    let what = if said.split_whitespace().count() >= 4 { clip(&said, 140) } else { title(t) };
    if t.state == TaskState::Verified && t.why.trim_start().starts_with("the brain checked it") { format!("{} (checked by the brain)", end(&what)) } else { what }
}

/// A session's brief as labelled lines ("Asked", "Done", "Now", "Next", "Stuck"): the panel's, and its text's.
pub fn session_lines(s: &SessionBrief) -> Vec<(&'static str, String)> {
    let mut parts = vec![];
    if !s.asked.is_empty() {
        parts.push(("Asked", end(&s.asked).to_string()));
    }
    if !s.result.is_empty() {
        parts.push(("Result", end(&s.result).to_string()));
    }
    if !s.done.is_empty() {
        parts.push(("Earlier today", if s.done.len() == 1 { end(&s.done[0]).to_string() } else { format!("{} more tasks done", s.done.len()) }));
    }
    if !s.now.is_empty() {
        parts.push(("Now", end(&s.now).to_string()));
    }
    if !s.next.is_empty() {
        parts.push(("Next", list(&s.next, 2)));
    }
    if let Some(x) = &s.stuck {
        parts.push(("Stuck", end(x).to_string()));
    }
    parts
}

fn session_text(s: &SessionBrief) -> String {
    session_lines(s).into_iter().map(|(l, t)| format!("{l}: {t}.")).collect::<Vec<_>>().join(" ")
}

pub fn space(f: &SpaceFacts) -> SpaceBrief {
    let recent = |t: &&&Task| f.now.saturating_sub(t.since_ms) <= DONE_WITHIN_MS;
    let mut tasks = f.tasks.clone();
    tasks.sort_by_key(|t| t.since_ms);
    // Its stage: where its open work stands (tasks of agendas still open).
    let live: Vec<&&Task> = tasks.iter().filter(|t| t.state != TaskState::Abandoned).filter(recent).collect();
    let verified = live.iter().filter(|t| t.state == TaskState::Verified).count();
    let mut stage = match f.state {
        "ready" => "ready to close".to_string(),
        "quiet" => "quiet: no session left".to_string(),
        _ if f.standing => "standing".to_string(),
        _ => "open".to_string(),
    };
    if !live.is_empty() {
        stage = format!("{stage}; {verified} of {} task{} verified today", live.len(), if live.len() == 1 { "" } else { "s" });
    }
    if let Some(r) = f.release.as_deref().filter(|r| !r.is_empty()) {
        stage = format!("{stage}; release {r}");
    }
    // What was done lately, each in its session's words, the newest first.
    let done: Vec<String> = tasks
        .iter()
        .rev()
        .filter(recent)
        .filter(|t| t.state == TaskState::Verified)
        .map(|t| if t.name.is_empty() { clip(&outcome(t), 110) } else { format!("{}: {}", t.name, clip(&outcome(t), 110)) })
        .collect();
    // Who works on what: its task when its "now" only says it works on it.
    let doing: Vec<String> = f
        .sessions
        .iter()
        .filter(|(_, _, w)| *w)
        .map(|(n, b, _)| {
            let what = if b.now == "working on it" { format!("working on {}", lower_first(&b.asked)) } else { b.now.clone() };
            format!("{n}: {}", clip(&what, 80))
        })
        .collect();
    let next: Vec<String> = tasks.iter().filter(|t| t.state == TaskState::Queued).map(|t| title(t)).collect();
    let mut waits = f.waits.clone();
    waits.extend(tasks.iter().rev().filter(|t| t.state == TaskState::Blocked && !t.question.is_empty()).map(|t| clip(&t.question, 120)));
    waits.dedup();
    let mut risks: Vec<String> = tasks.iter().rev().filter(recent).filter(|t| matches!(t.state, TaskState::Stalled | TaskState::Failed)).map(|t| why_line(t)).collect();
    risks.extend(f.pr_risks.iter().cloned());
    let goal = if f.goal.trim().is_empty() { String::new() } else { clip(f.goal.trim(), 200) };
    let mut s = SpaceBrief { goal, stage, done, doing, next, waits, risks, ..Default::default() };
    s.text = space_text(&s);
    s
}

/// A space's brief as labelled lines ("Goal", "Done", "In progress", "Next", "Waiting for you", "Risks").
pub fn space_lines(s: &SpaceBrief) -> Vec<(&'static str, String)> {
    let mut parts = vec![if s.goal.is_empty() { ("No goal", s.stage.clone()) } else { ("Goal", format!("{} ({})", end(&s.goal), s.stage)) }];
    if !s.done.is_empty() {
        let more = s.done.len().saturating_sub(2);
        let shown = s.done.iter().take(2).map(|d| end(d).to_string()).collect::<Vec<_>>().join("; ");
        parts.push(("Done", if more > 0 { format!("{shown} (and {more} more today)") } else { shown }));
    }
    parts.push(if s.doing.is_empty() { ("In progress", "nobody is working now".to_string()) } else { ("In progress", list(&s.doing, 3)) });
    if !s.next.is_empty() {
        parts.push(("Next", list(&s.next, 2)));
    }
    if !s.waits.is_empty() {
        parts.push(("Waiting for you", list(&s.waits, 2)));
    }
    if !s.risks.is_empty() {
        parts.push(("Risks", list(&s.risks, 2)));
    }
    parts
}

fn space_text(s: &SpaceBrief) -> String {
    space_lines(s)
        .into_iter()
        .map(|(l, t)| match l {
            "No goal" => format!("No goal ({t})."),
            "In progress" if s.doing.is_empty() => "Nobody is working now.".to_string(),
            _ => format!("{l}: {t}."),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A task in a few words: its title, else its instruction's start.
fn title(t: &Task) -> String {
    let words = if t.title.trim().is_empty() { t.instruction.lines().next().unwrap_or("") } else { t.title.as_str() };
    clip(words.trim(), 120)
}


fn why_line(t: &Task) -> String {
    let state = if t.state == TaskState::Failed { "failed" } else { "stalled" };
    match t.why.trim() {
        "" => format!("{} {state}", title(t)),
        why => format!("{} {state}: {}", title(t), clip(why, 100)),
    }
}

/// "a; b (and 2 more)".
fn list(v: &[String], n: usize) -> String {
    let shown: Vec<String> = v.iter().take(n).map(|x| end(x).to_string()).collect();
    let more = v.len().saturating_sub(n);
    if more > 0 { format!("{} (and {more} more)", shown.join("; ")) } else { shown.join("; ") }
}

/// Without its own full stop, so a sentence adds one.
fn end(s: &str) -> &str {
    s.trim().trim_end_matches(['.', ' '])
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        // "PR …", "API …": an acronym keeps its capital.
        Some(_) if c.clone().next().is_some_and(|n| n.is_uppercase()) => s.to_string(),
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

/// The first sentence of a message, plain (markdown marks out).
pub fn first_sentence(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ").replace("**", "").replace('`', "");
    let flat = flat.trim_start_matches(['#', '-', '*', ' ']);
    let end = flat.char_indices().find(|(i, c)| ['.', '!', '?'].contains(c) && flat[i + c.len_utf8()..].starts_with(' ')).map(|(i, c)| i + c.len_utf8());
    clip(&flat[..end.unwrap_or(flat.len())], 200)
}

pub fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>().trim_end()) }
}

/// Each brief's versions, kept in the state folder (`briefs.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    /// By key (`space:<name>`, `session:<address>`): (what it says but its "now", when it changed, its versions newest first).
    briefs: HashMap<String, Kept>,
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Kept {
    stable: String,
    changed_ms: u64,
    versions: Vec<Version>,
}

impl History {
    pub fn load(path: PathBuf) -> History {
        let mut h: History = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        h.path = Some(path);
        h
    }

    /// Notes a brief as it reads now: a new version when what it says (its
    /// `stable` part) changed. Its versions, newest first, and when it changed.
    pub fn note(&mut self, key: &str, stable: &str, text: &str, now: u64) -> (Vec<Version>, u64) {
        let k = self.briefs.entry(key.to_string()).or_default();
        if k.stable != stable {
            k.stable = stable.to_string();
            k.changed_ms = now;
            k.versions.insert(0, Version { at_ms: now, text: text.to_string() });
            k.versions.truncate(KEPT);
            self.dirty = true;
        }
        (k.versions.clone(), k.changed_ms)
    }

    /// Forgets the briefs whose space or session isn't there any more.
    pub fn keep_only(&mut self, keys: &std::collections::HashSet<String>) {
        let before = self.briefs.len();
        self.briefs.retain(|k, _| keys.contains(k));
        self.dirty |= self.briefs.len() != before;
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(p) = &self.path
            && let Ok(text) = serde_json::to_string(&self)
        {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, p).is_ok() {
                self.dirty = false;
            }
        }
    }
}

/// A session brief's stable part: all but its "now".
pub fn session_stable(s: &SessionBrief) -> String {
    session_text(&SessionBrief { now: String::new(), ..s.clone() })
}

/// A space brief's stable part: all but who is working this moment.
pub fn space_stable(s: &SpaceBrief) -> String {
    space_text(&SpaceBrief { doing: vec![], ..s.clone() })
}

/// `tower brief [<what>] [--history]`: from the state the daemon publishes.
pub fn cmd_brief(what: &str, history: bool, json: bool) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let state: serde_json::Value = std::fs::read_to_string(crate::config::ui_dir().join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let spaces: Vec<serde_json::Value> = state["missions"]["spaces"].as_array().cloned().unwrap_or_default();
    if spaces.iter().all(|m| m["brief"].is_null()) {
        anyhow::bail!("no briefs: the tower isn't running, or is older than briefs (`tower up`)");
    }
    let want = what.trim().to_lowercase();
    let at = |ms: u64| crate::phone::ago(ms);
    let versions = |b: &serde_json::Value| -> Vec<String> {
        b["history"].as_array().into_iter().flatten().skip(1).take(3).map(|v| format!("    before, {}: {}", at(v["at_ms"].as_u64().unwrap_or(0)), v["text"].as_str().unwrap_or(""))).collect()
    };
    let mut out: Vec<String> = vec![];
    let mut picked: Vec<serde_json::Value> = vec![];
    // A session by its address or name, else a space by its name (or part of it), else every space.
    let session = spaces.iter().flat_map(|m| m["sessions"].as_array().cloned().unwrap_or_default().into_iter().map(move |x| (m.clone(), x))).find(|(_, x)| {
        !want.is_empty() && (x["address"].as_str() == Some(want.as_str()) || x["name"].as_str().is_some_and(|n| n.to_lowercase() == want))
    });
    if let Some((m, x)) = session {
        out.push(format!("{} ({}, in {}): {}", x["name"].as_str().unwrap_or(""), x["address"].as_str().unwrap_or(""), m["name"].as_str().unwrap_or(""), x["brief"]["text"].as_str().unwrap_or("no brief yet")));
        if history {
            out.extend(versions(&x["brief"]));
        }
        picked.push(x);
    } else {
        let chosen: Vec<&serde_json::Value> = spaces
            .iter()
            .filter(|m| want.is_empty() || m["name"].as_str().is_some_and(|n| n.to_lowercase() == want))
            .chain(spaces.iter().filter(|m| !want.is_empty() && m["name"].as_str().is_some_and(|n| n.to_lowercase() != want && n.to_lowercase().contains(&want))))
            .take(if want.is_empty() { usize::MAX } else { 1 })
            .collect();
        if chosen.is_empty() {
            anyhow::bail!("no open space or session called \"{what}\" (`tower brief` lists every space)");
        }
        for m in chosen {
            out.push(format!("{}: {}", m["name"].as_str().unwrap_or(""), m["brief"]["text"].as_str().unwrap_or("no brief yet")));
            if history {
                out.extend(versions(&m["brief"]));
            }
            // One space asked for: its sessions too.
            if !want.is_empty() {
                for x in m["sessions"].as_array().into_iter().flatten() {
                    out.push(format!("  {} ({}): {}", x["name"].as_str().unwrap_or(""), x["address"].as_str().unwrap_or(""), x["brief"]["text"].as_str().unwrap_or("no brief yet")));
                    if history {
                        out.extend(versions(&x["brief"]).into_iter().map(|l| format!("  {l}")));
                    }
                }
            }
            picked.push(m.clone());
        }
    }
    if json || !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "briefs": picked })).unwrap_or_default());
    } else {
        println!("{}", out.join("\n"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn task(id: &str, title: &str, state: TaskState, at: u64) -> Task {
        Task { id: id.into(), title: title.into(), state, since_ms: at, created_at_ms: at, session: "t1".into(), ..Default::default() }
    }

    #[test]
    fn a_sessions_brief_says_what_it_was_asked_did_does_and_waits_on() {
        let mut verified = task("a1.1", "Rename Unanswered to Knowledge gaps", TaskState::Verified, 10 * MIN);
        verified.why = "its last message matches: tests pass, screenshot sent".into();
        let running = task("a2.1", "Count open gaps in the studio column", TaskState::Running, 20 * MIN);
        let queued = task("a3.1", "Draft help page", TaskState::Queued, 30 * MIN);
        let f = SessionFacts { status: "working", said: None, tasks: vec![&verified, &running, &queued], waits_on: vec![], progress: None, upcoming: vec![], now: 40 * MIN };
        let b = session(&f);
        assert_eq!(b.asked, "Count open gaps in the studio column", "the latest it took up");
        assert_eq!(b.now, "working on it", "on what it was asked, said just above");
        assert_eq!(
            b.text,
            "Asked: Count open gaps in the studio column. Earlier today: Rename Unanswered to Knowledge gaps. Now: working on it. Next: Draft help page."
        );
        // Done: what came of it, in its own words; checked by the brain when it was.
        let mut finished = task("a5.1", "ok go", TaskState::Verified, 39 * MIN);
        finished.said = "Main is pushed and equals origin/main. 16 commits went up.".into();
        finished.why = "the brain checked it: pushed, tests pass".into();
        let d = session(&SessionFacts { status: "done", said: Some("Main is pushed and equals origin/main."), tasks: vec![&verified, &finished], waits_on: vec![], progress: None, upcoming: vec![], now: 40 * MIN });
        assert_eq!(d.text, "Asked: ok go (done). Result: Main is pushed and equals origin/main (checked by the brain). Earlier today: Rename Unanswered to Knowledge gaps. Now: idle.");
        // Its live progress line, when the tower has one, is its "now" as it is.
        let p = session(&SessionFacts { progress: Some("step 3 of 5: running the tests · 12m in"), ..f });
        assert_eq!(p.now, "step 3 of 5: running the tests · 12m in");
        // Blocked on the user: what for. Idle: what it last said.
        let blocked = session(&SessionFacts { status: "blocked", said: None, tasks: vec![], waits_on: vec!["Merge PR #839 into dev".into()], progress: None, upcoming: vec![], now: 40 * MIN });
        assert_eq!(blocked.text, "Now: waiting for you: Merge PR #839 into dev.");
        let idle = session(&SessionFacts { status: "done", said: Some("**Done.** The PR is open. Want me to merge it?"), tasks: vec![], waits_on: vec![], progress: None, upcoming: vec![], now: 40 * MIN });
        assert_eq!(idle.now, "idle; it last said: Done.");
        // A task stalled, and a wait at the end of its turn: what it's stuck on.
        let mut stalled = task("a4.1", "Train the LoRA", TaskState::Stalled, 35 * MIN);
        stalled.why = "no progress for 2 hours".into();
        let s = session(&SessionFacts { status: "done", said: None, tasks: vec![&stalled], waits_on: vec![], progress: None, upcoming: vec![], now: 40 * MIN });
        assert_eq!(s.stuck.as_deref(), Some("Train the LoRA stalled: no progress for 2 hours"));
        let w = session(&SessionFacts { status: "done", said: None, tasks: vec![], waits_on: vec!["Push to main?".into()], progress: None, upcoming: vec![], now: 40 * MIN });
        assert_eq!(w.stuck.as_deref(), Some("waits for your answer: Push to main?"));
    }

    #[test]
    fn a_spaces_brief_says_its_goal_stage_work_waits_and_risks() {
        let mut v = task("a1.1", "Rename the page", TaskState::Verified, 10 * MIN);
        v.why = "tests pass".into();
        let mut f = task("a1.2", "Deploy to dev", TaskState::Failed, 20 * MIN);
        f.why = "the build broke".into();
        let q = task("a1.3", "Draft help page", TaskState::Queued, 30 * MIN);
        let working = SessionBrief { now: "step 2 of 4: writing the migration".into(), ..Default::default() };
        let idle = SessionBrief { now: "idle".into(), ..Default::default() };
        let facts = SpaceFacts {
            goal: "Answer payment questions from the knowledge base.",
            state: "open",
            standing: false,
            sessions: vec![("mediaroom".into(), &working, true), ("kb".into(), &idle, false)],
            tasks: vec![&v, &f, &q],
            waits: vec!["Ship #1408 to prod?".into()],
            pr_risks: vec!["PR #841 has conflicts with dev".into()],
            release: Some("prod 14:20 · 1 on dev".into()),
            now: 40 * MIN,
        };
        let b = space(&facts);
        assert_eq!(
            b.text,
            "Goal: Answer payment questions from the knowledge base (open; 1 of 3 tasks verified today; release prod 14:20 · 1 on dev). Done: Rename the page. In progress: mediaroom: step 2 of 4: writing the migration. Next: Draft help page. Waiting for you: Ship #1408 to prod?. Risks: Deploy to dev failed: the build broke; PR #841 has conflicts with dev."
        );
        // Who works this moment isn't a change worth a version.
        let calm = space(&SpaceFacts { sessions: vec![("mediaroom".into(), &idle, false)], ..facts });
        assert_eq!(space_stable(&b), space_stable(&calm));
        assert!(calm.text.contains("Nobody is working now."));
    }

    #[test]
    fn a_new_version_is_kept_only_when_what_it_says_changes() {
        let mut h = History::default();
        assert_eq!(h.note("space:Shop", "Goal: x. Done: a.", "Goal: x. Done: a. In progress: web.", 1).0.len(), 1);
        // Only "now" moved: no new version.
        let (v, changed) = h.note("space:Shop", "Goal: x. Done: a.", "Goal: x. Done: a. Nobody is working now.", 2);
        assert_eq!((v.len(), changed), (1, 1));
        let (v, changed) = h.note("space:Shop", "Goal: x. Done: a; b.", "Goal: x. Done: a; b.", 3);
        assert_eq!((v.len(), changed, v[0].text.as_str()), (2, 3, "Goal: x. Done: a; b."), "newest first");
        for i in 0..20 {
            h.note("space:Shop", &format!("v{i}"), &format!("v{i}"), 10 + i);
        }
        assert_eq!(h.note("space:Shop", "v19", "v19", 99).0.len(), KEPT);
        h.keep_only(&std::collections::HashSet::new());
        assert!(h.briefs.is_empty());
    }

    #[test]
    fn a_messages_first_sentence_is_plain() {
        assert_eq!(first_sentence("**Done.** The PR is open."), "Done.");
        assert_eq!(first_sentence("## Summary\nIt works. Next I'll test."), "Summary It works.");
        assert_eq!(first_sentence("v2.1 is out and notes.md updated. More."), "v2.1 is out and notes.md updated.");
        assert_eq!(lower_first("PR #841 is open"), "PR #841 is open");
        assert_eq!(lower_first("Working on it"), "working on it");
    }
}
