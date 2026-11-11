//! What the tower opened for the user, kept so it can be shown with the
//! reply and undone (`docs/quiet-tower.md`, "A command gets a receipt").
//!
//! A command gets a receipt, never a question: asked to open a session or a
//! space, the tower takes the defaults, acts, and keeps an **act**: what it
//! opened, what it assumed and why, and the request it answered. `tower
//! start` and `tower dispatch` make one for the sessions they open, `tower
//! launch` one for its space. Sessions opened for the same request are one
//! act, with one Undo.
//!
//! `tower undo [<id>]` ends what an act opened: its sessions (and the
//! worktrees it made for them, when nothing in them would be lost), or the
//! whole space when the act made it and nothing else is in it. It refuses,
//! saying why, when a session was given more work than its first
//! instruction, or its worktree holds changes that are nowhere else, unless
//! forced.
//!
//! The daemon keeps the acts (`crate::daemon::acts`, `acts.json` in the
//! state folder); this is the model, and the undo itself over `Hands`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::history::Entry;
use crate::worktrees::{Place, Removal};

/// The acts kept, newest last.
pub const KEEP: usize = 50;
/// The acts the state carries, newest first.
pub const SHOWN: usize = 10;
/// `tower undo` with no id takes the latest act this recent.
pub const RECENT_MS: u64 = 10 * 60 * 1000;
/// A request older than this isn't what an act answers.
const REQUEST_MS: u64 = 30 * 60 * 1000;

/// One thing the tower settled by itself, or was told: what it is
/// (`account`, `model`, `folder`, `worktree`, `space`, `name`, `setup`),
/// its value as a chip shows it, and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Assumed {
    pub what: String,
    pub value: String,
    pub why: String,
}

/// A session an act opened.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Opened {
    pub address: String,
    /// Its name in Herdr (`fix-login-eyzflow`).
    pub name: String,
    /// When its terminal was made: the session is that terminal, not its
    /// address (an address can come back for another one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ms: Option<u64>,
    /// The worktree it works in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Place>,
    /// The act made that worktree (one that was there is left there).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fresh: bool,
    /// Its first instruction, as it was given.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub first: String,
    /// Its task in the agenda ledger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// The work it was given after its first instruction: undoing would lose it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub more: Option<String>,
}

/// The request an act answered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Asked {
    /// When it was asked (its history entry's `at`): which request it is.
    pub at: u64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Act {
    /// `r` and 12 hex digits.
    pub id: String,
    /// `start`, `launch` or `dispatch`.
    pub kind: String,
    pub at_ms: u64,
    /// Who ran it: a brain (`tower`, `tower-<space>`), `phone:<device>`,
    /// `cli` (a shell, the full window), or a session's address.
    pub by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// The act made the space: undoing closes it, when nothing else is in it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub space_made: bool,
    /// A launch's goal.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub goal: String,
    pub sessions: Vec<Opened>,
    pub assumed: Vec<Assumed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Asked>,
    /// `done` or `undone`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undone_ms: Option<u64>,
    /// What undoing it came to, in a line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closing: Option<String>,
}

/// An act's id: `r` and 12 hex digits.
pub fn is_id(s: &str) -> bool {
    s.len() == 13 && s.starts_with('r') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// A new id, from the time and a counter (unique in one daemon, and across restarts).
pub fn new_id(now_ms: u64) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mixed = (now_ms ^ (u64::from(std::process::id()) << 32)).wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(n.wrapping_mul(0xff51afd7ed558ccd));
    format!("r{:012x}", mixed & 0xffff_ffff_ffff)
}

/// "a", "a and b", "a, b and c".
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

impl Act {
    pub fn done(&self) -> bool {
        self.state != "undone"
    }

    /// What was opened, in a line: the reply to the request.
    pub fn line(&self) -> String {
        let space = self.space.as_deref().filter(|s| !s.is_empty());
        let names: Vec<String> = self.sessions.iter().map(|s| crate::names::label(space, &s.name)).map(|l| if l.is_empty() { "a session".into() } else { l }).collect();
        let placed = self.sessions.iter().filter(|s| s.worktree.is_some()).count();
        let n = self.sessions.len();
        let where_ = match (placed, n) {
            (0, _) => "",
            (1, 1) => " It works in its own worktree.",
            (p, n) if p == n => " Each works in its own worktree.",
            _ => " Those in a git repo work in their own worktrees.",
        };
        let sessions = |n: usize| if n == 1 { "1 session".to_string() } else { format!("{n} sessions") };
        match (self.kind.as_str(), space, n) {
            ("launch", Some(sp), 0) => format!("Launched {sp}, but none of its sessions opened."),
            ("launch", Some(sp), n) => format!("Launched {sp}: {}, {}.{where_}", sessions(n), listed(&names)),
            (_, _, 0) => "Opened nothing.".into(),
            (_, Some(sp), 1) => format!("Opened {} in {sp}.{where_}", names[0]),
            (_, None, 1) => format!("Opened {}.{where_}", names[0]),
            (_, Some(sp), n) => format!("Opened {} in {sp}: {}.{where_}", sessions(n), listed(&names)),
            (_, None, n) => format!("Opened {}: {}.{where_}", sessions(n), listed(&names)),
        }
    }

    /// Why undoing it would lose work a session was given since (none: nothing).
    pub fn given_more(&self) -> Option<String> {
        let held: Vec<String> = self.sessions.iter().filter_map(|s| s.more.as_ref().map(|m| format!("{} was given more work since ({})", s.name, quoted(m)))).collect();
        (!held.is_empty()).then(|| held.join("; "))
    }

    /// The act as the state, the chat and the phone carry it.
    pub fn view(&self) -> Value {
        let space = self.space.as_deref().filter(|s| !s.is_empty());
        let why = if !self.done() { Some("it is undone already".to_string()) } else { self.given_more() };
        json!({
            "id": self.id, "kind": self.kind, "at_ms": self.at_ms, "by": self.by, "space": self.space,
            "line": self.line(),
            "sessions": self.sessions.iter().map(|s| json!({
                "address": s.address, "name": s.name, "label": crate::names::label(space, &s.name),
                "worktree": s.worktree.as_ref().map(|w| w.path.clone()), "branch": s.worktree.as_ref().map(|w| w.branch.clone()),
            })).collect::<Vec<_>>(),
            "assumed": self.assumed,
            "request": self.request.as_ref().map(|r| json!({ "at": r.at, "text": r.text })),
            "state": if self.done() { "done" } else { "undone" },
            "undo": { "possible": why.is_none(), "why": why },
            "closing": self.closing, "undone_ms": self.undone_ms,
        })
    }

    /// Its history entry (kind `act`): shown with the reply when it answered
    /// a request of the user's, for the record otherwise.
    pub fn entry(&self) -> Entry {
        Entry {
            kind: "act".into(),
            id: Some(self.id.clone()),
            status: Some(if self.done() { "done" } else { "undone" }.into()),
            text: match (&self.closing, self.done()) {
                (Some(c), false) => c.clone(),
                _ => self.line(),
            },
            space: self.space.clone(),
            act: Some(json!({
                "kind": self.kind, "by": self.by, "assumed": self.assumed,
                "sessions": self.sessions.iter().map(|s| json!({ "address": s.address, "name": s.name })).collect::<Vec<_>>(),
                "request_at": self.request.as_ref().map(|r| r.at),
                // The conversation it was asked in (a space's own tower's), for the chat.
                "request_space": self.request.as_ref().and_then(|r| r.space.clone()),
            })),
            record: self.request.is_none(),
            ..Default::default()
        }
    }
}

/// A prompt as a few words, for a sentence.
fn quoted(text: &str) -> String {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > 60 { format!("\"{}…\"", t.chars().take(60).collect::<String>().trim_end()) } else { format!("\"{t}\"") }
}

/// Whether `prompt` is work a session was given after its `first`
/// instruction. Not its first instruction arriving, not the tower's
/// introduction of the other sessions, not another session's message.
pub fn is_work(first: &str, prompt: &str) -> bool {
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    // Without what delivered it (`<hc from="t5">…</hc>`); an introduction is nobody's task.
    let Some(prompt) = crate::introduce::task_from_prompt(prompt).map(|p| flat(&p)).filter(|p| !p.is_empty()) else { return false };
    if prompt.starts_with("From ") {
        return false;
    }
    // The first instruction as its hook brings it: whole, or its task without where it works.
    let first = flat(first);
    let head: String = first.chars().take(200).collect();
    first.is_empty() || !(prompt.contains(&head) || first.starts_with(&prompt))
}

/// Whether the terminal `t` (as `hc get` has it now) is the session an act
/// opened: made when it was, when both are known; else called what it was,
/// when both have a name. An address alone says nothing: Herdr hands a free
/// one to the next terminal.
pub fn same_terminal(s: &Opened, t: &Value) -> bool {
    match (s.created_ms, crate::hc::created_ms(t)) {
        (Some(was), Some(now)) => was == now,
        _ => match (s.name.trim(), crate::hc::name(t)) {
            ("", _) | (_, None) => true,
            (was, Some(now)) => was == now,
        },
    }
}

/// The request an act made now answers: the latest one asked of that brain
/// (`space`: a space's own; None: the main one), unless it is old.
pub fn request_for(entries: &[Entry], space: Option<&str>, now_ms: u64) -> Option<Asked> {
    let same = |e: &Entry| match (e.space.as_deref().filter(|s| !s.is_empty()), space) {
        (None, None) => true,
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    let e = entries.iter().rev().find(|e| e.kind == "request" && !e.record && !e.held && same(e))?;
    (now_ms.saturating_sub(e.at) < REQUEST_MS).then(|| Asked { at: e.at, text: e.text.clone(), space: e.space.clone() })
}

/// What `tower start` assumed (`assumed` in its answer), as chips.
pub fn chips_of_start(a: &Value) -> Vec<Assumed> {
    chips(a, &["account", "model", "folder", "worktree", "space", "name"])
}

/// What `tower launch` assumed (`assumed` in its receipt), as chips.
pub fn chips_of_launch(a: &Value) -> Vec<Assumed> {
    let mut out = chips(a, &["setup", "account", "name"]);
    if let Some(why) = a["worktrees_why"].as_str() {
        out.push(Assumed { what: "worktrees".into(), value: if a["worktrees"] == true { "worktrees".into() } else { "no worktrees".into() }, why: why.into() });
    }
    out
}

/// `{"account": "orca", "account_why": "…"}` as `[{what, value, why}]`, in the order of `keys`.
fn chips(a: &Value, keys: &[&str]) -> Vec<Assumed> {
    keys.iter()
        .filter_map(|k| {
            let value = match &a[*k] {
                Value::String(s) if !s.trim().is_empty() => s.trim().to_string(),
                _ => return None,
            };
            // A folder and a worktree by their last part: a chip is short.
            let value = match *k {
                "folder" | "worktree" => value.trim_end_matches('/').rsplit('/').next().unwrap_or(&value).to_string(),
                _ => value,
            };
            Some(Assumed { what: k.to_string(), value, why: a[format!("{k}_why")].as_str().unwrap_or("").to_string() })
        })
        .collect()
}

/// The acts, newest last.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub acts: Vec<Act>,
}

impl Book {
    /// Keeps an act. Sessions opened one by one for the same request are one
    /// act: a `start` for the request the latest `start` answered joins it.
    /// Returns the act as it is kept.
    pub fn record(&mut self, mut act: Act) -> Act {
        act.state = "done".into();
        let joins = self.acts.last_mut().filter(|last| {
            last.done() && last.kind == "start" && act.kind == "start" && last.request.is_some() && last.request.as_ref().map(|r| r.at) == act.request.as_ref().map(|r| r.at)
        });
        if let Some(last) = joins {
            last.sessions.extend(act.sessions);
            for a in act.assumed {
                if !last.assumed.iter().any(|b| b.what == a.what && b.value == a.value) {
                    last.assumed.push(a);
                }
            }
            // One space for them all, or none said.
            if last.space != act.space {
                last.space = None;
            }
            last.space_made |= act.space_made;
            return last.clone();
        }
        self.acts.push(act.clone());
        let over = self.acts.len().saturating_sub(KEEP);
        self.acts.drain(..over);
        act
    }

    pub fn get(&self, id: &str) -> Option<&Act> {
        self.acts.iter().find(|a| a.id == id)
    }

    /// What `tower undo` with no id means: the latest act, when it was made
    /// in the last ten minutes and isn't undone.
    pub fn latest(&self, now_ms: u64) -> Option<&Act> {
        self.acts.last().filter(|a| a.done() && now_ms.saturating_sub(a.at_ms) < RECENT_MS)
    }

    /// A session took a prompt, or was sent an instruction: work beyond its
    /// first one holds its act. True when that changed something.
    pub fn given(&mut self, address: &str, text: &str) -> bool {
        let mut changed = false;
        for s in self.acts.iter_mut().filter(|a| a.done()).flat_map(|a| a.sessions.iter_mut()).filter(|s| s.address == address && s.more.is_none()) {
            if is_work(&s.first, text) {
                s.more = Some(text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect());
                changed = true;
            }
        }
        changed
    }

    /// The acts as the state carries them: the latest, newest first.
    pub fn views(&self) -> Vec<Value> {
        self.acts.iter().rev().take(SHOWN).map(Act::view).collect()
    }
}

/// The note for the brain that opened an act (`act`: its view), when
/// somebody else undid it (a window's Undo, a phone, a shell): what was
/// undone and how it closed, so it doesn't go on about sessions that are
/// gone. `by`: the brain that ran the undo, if one did (it knows already).
/// None: no brain opened it, or that brain undid it itself. It is an
/// `[agenda]` note, so the brain's turn on it is a note turn: nothing it
/// writes then is shown or said.
pub fn undone_note(act: &Value, by: Option<&str>) -> Option<crate::agenda::Note> {
    let opener = act["by"].as_str().filter(|b| crate::brain::is_brain_name(b))?;
    if by == Some(opener) || act["state"] != "undone" {
        return None;
    }
    let id = act["id"].as_str().unwrap_or("");
    let sessions: Vec<String> = act["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some(format!("{} ({})", s["label"].as_str().filter(|l| !l.is_empty()).or(s["name"].as_str())?, s["address"].as_str()?)))
        .collect();
    let who = if sessions.is_empty() { String::new() } else { format!(" Its sessions: {}.", sessions.join(", ")) };
    let asked = act["request"]["text"].as_str().map(|t| format!(" (it answered \"{}\")", quoted(t).trim_matches('"'))).unwrap_or_default();
    // A space's own brain is told in its space; the main one in none.
    let space = act["space"].as_str().filter(|s| !s.is_empty() && crate::brains::is_space_brain(opener)).map(String::from);
    Some(crate::agenda::Note {
        space,
        msg: format!(
            "[agenda] The user undid {id}, which you opened{asked}: \"{}\" {}{who} Those sessions have ended and their tasks are dropped: don't wait for them, write to them or report on them, and don't open them again unless the user asks.",
            act["line"].as_str().unwrap_or("").trim(),
            act["closing"].as_str().unwrap_or("Closed it.").trim(),
        ),
        ..Default::default()
    })
}

/// A session that keeps an act from being undone, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub address: String,
    pub name: String,
    pub why: String,
}

/// Why an act wasn't undone.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    /// No act with that id (or none made lately, when none was named).
    Unknown(String),
    /// It was undone before.
    Undone(String),
    /// Work would be lost: which sessions, and why. `--force` undoes it anyway.
    Holds(Vec<Hold>),
    /// Something couldn't be ended.
    Failed(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Unknown(id) if id.is_empty() => f.write_str("nothing was opened in the last 10 minutes: name the act (tower acts lists them)"),
            Refused::Unknown(id) => write!(f, "no act {id} (tower acts lists them)"),
            Refused::Undone(id) => write!(f, "{id} is undone already"),
            Refused::Holds(list) => {
                let why: Vec<String> = list.iter().map(|h| format!("{} ({}) {}", h.name, h.address, h.why)).collect();
                write!(f, "not undone: {}. Undo it anyway with --force (the sessions are ended; a worktree with changes is kept).", why.join("; "))
            }
            Refused::Failed(why) => f.write_str(why),
        }
    }
}

/// What undoing an act came to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// The sessions ended (addresses).
    pub ended: Vec<String>,
    pub worktrees_removed: Vec<String>,
    /// Worktrees left where they are, and why.
    pub kept_worktrees: Vec<(String, String)>,
    /// The whole space was closed.
    pub space_closed: bool,
    /// In a line, for the reply.
    pub closing: String,
}

impl Outcome {
    pub fn value(&self) -> Value {
        json!({
            "sessions_ended": self.ended, "worktrees_removed": self.worktrees_removed,
            "kept_worktrees": self.kept_worktrees.iter().map(|(p, why)| json!({ "path": p, "why": why })).collect::<Vec<_>>(),
            "space_closed": self.space_closed, "closing": self.closing,
        })
    }
}

/// What undoing touches: Herdr's terminals, git worktrees, the tower's own books.
pub trait Hands {
    /// The session the act opened still runs: a live terminal at its
    /// address that is the one it opened (`same_terminal`).
    fn runs(&self, session: &Opened) -> bool;
    /// Why taking a worktree away would lose something (None: nothing would).
    fn dirty(&self, place: &Place) -> Option<String>;
    /// The agent sessions in a space now (addresses), brains aside.
    fn in_space(&self, space: &str) -> Vec<String>;
    /// A live session other than `mine` works in that worktree.
    fn shared(&self, place: &Place, mine: &[String]) -> bool;
    /// Drops what the tower holds for a session: its task, what waits for it in the queue, its watch.
    fn forget(&self, session: &Opened);
    /// Ends a session's terminal.
    fn end(&self, address: &str) -> Result<(), String>;
    /// Takes a worktree away, when nothing in it would be lost.
    fn remove(&self, space: Option<&str>, place: &Place) -> Removal;
    /// Closes a whole space as `tower space close --force` does: its receipt.
    fn close_space(&self, space: &str) -> Result<Value, String>;
}

/// The sessions that keep an act from being undone without `--force`: one
/// that was given more work, one whose worktree holds changes.
pub fn holds(act: &Act, h: &dyn Hands) -> Vec<Hold> {
    let mut out = vec![];
    for s in &act.sessions {
        if let Some(more) = &s.more {
            out.push(Hold { address: s.address.clone(), name: s.name.clone(), why: format!("was given more work since ({})", quoted(more)) });
        } else if let Some(why) = s.worktree.as_ref().and_then(|w| h.dirty(w)) {
            out.push(Hold { address: s.address.clone(), name: s.name.clone(), why: format!("has work in its worktree that is nowhere else: {why}") });
        }
    }
    out
}

/// Undoes an act: ends the sessions it opened and takes away the worktrees
/// it made for them (each only when nothing in it would be lost), or closes
/// the space it made when nothing else is in it. Refused while work would be
/// lost, unless `force` (then the sessions are ended, and a worktree with
/// changes is kept).
pub fn undo(act: &Act, force: bool, h: &dyn Hands) -> Result<Outcome, Refused> {
    if !act.done() {
        return Err(Refused::Undone(act.id.clone()));
    }
    let held = holds(act, h);
    if !held.is_empty() && !force {
        return Err(Refused::Holds(held));
    }
    // What it opened and still runs. A terminal that has its address now but isn't
    // the one it opened is somebody else's: never ended, never forgotten.
    let running: Vec<&Opened> = act.sessions.iter().filter(|s| h.runs(s)).collect();
    let mine: Vec<String> = running.iter().map(|s| s.address.clone()).collect();
    let mut out = Outcome::default();
    // The space it made, with nothing in it but what it opened: closed whole.
    let whole = act.space.as_deref().filter(|sp| act.space_made && !sp.is_empty() && h.in_space(sp).iter().all(|a| mine.contains(a)));
    if let Some(space) = whole {
        for s in &running {
            h.forget(s);
        }
        let r = h.close_space(space).map_err(Refused::Failed)?;
        let texts = |k: &str| r[k].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>();
        let left: Vec<String> = r["sessions_left"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| mine.iter().any(|a| s["address"] == a.as_str()))
            .map(|s| format!("{} ({}): {}", s["name"].as_str().unwrap_or(""), s["address"].as_str().unwrap_or(""), s["why"].as_str().unwrap_or("")))
            .collect();
        if !left.is_empty() {
            return Err(Refused::Failed(format!("couldn't end {}", left.join("; "))));
        }
        out.ended = r["sessions_ended"].as_array().into_iter().flatten().filter_map(|s| s["address"].as_str().map(String::from)).collect();
        out.worktrees_removed = texts("worktrees_removed");
        out.kept_worktrees = r["kept_worktrees"].as_array().into_iter().flatten().map(|w| (w["path"].as_str().unwrap_or("").to_string(), w["why"].as_str().unwrap_or("").to_string())).collect();
        out.space_closed = true;
    } else {
        let mut failed = vec![];
        for s in &running {
            h.forget(s);
            match h.end(&s.address) {
                Ok(()) => out.ended.push(s.address.clone()),
                Err(why) => failed.push(format!("{} ({}): {why}", s.name, s.address)),
            }
        }
        if !failed.is_empty() {
            return Err(Refused::Failed(format!("couldn't end {}", failed.join("; "))));
        }
        // The worktrees it made, each once: gone unless something in it would be lost, or someone else works there.
        let mut seen: Vec<&str> = vec![];
        for w in act.sessions.iter().filter(|s| s.fresh).filter_map(|s| s.worktree.as_ref()) {
            if seen.contains(&w.path.as_str()) {
                continue;
            }
            seen.push(&w.path);
            if h.shared(w, &mine) {
                out.kept_worktrees.push((w.path.clone(), "another session works there".into()));
                continue;
            }
            match h.remove(act.space.as_deref(), w) {
                Removal::Removed | Removal::Gone => out.worktrees_removed.push(w.path.clone()),
                Removal::Kept(why) => out.kept_worktrees.push((w.path.clone(), why)),
            }
        }
    }
    out.closing = match out.kept_worktrees.as_slice() {
        [] if out.ended.is_empty() && !out.space_closed => "It had ended already. Nothing else changed.".to_string(),
        [] => "Closed it. Nothing else changed.".to_string(),
        kept => {
            let list: Vec<String> = kept.iter().map(|(p, why)| format!("{p} ({why})")).collect();
            format!("Closed it. Kept {}: {}.", if kept.len() == 1 { "its worktree" } else { "these worktrees" }, list.join("; "))
        }
    };
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::cell::RefCell;

    pub fn opened(address: &str, name: &str, first: &str) -> Opened {
        Opened { address: address.into(), name: name.into(), first: first.into(), ..Default::default() }
    }

    fn place(name: &str) -> Place {
        Place { repo: format!("/w/{name}"), path: format!("/w/.tower-worktrees/sandbox/{name}"), branch: format!("sandbox/{name}") }
    }

    pub fn act(kind: &str, space: Option<&str>, sessions: Vec<Opened>) -> Act {
        Act { id: new_id(1), kind: kind.into(), at_ms: 1_000, by: "tower".into(), space: space.map(String::from), sessions, state: "done".into(), ..Default::default() }
    }

    /// Hands that touch nothing and say what they were asked to do.
    #[derive(Default)]
    struct Fake {
        dead: Vec<&'static str>,
        /// Addresses another terminal has now.
        replaced: Vec<&'static str>,
        dirty: Vec<(&'static str, &'static str)>,
        others: Vec<&'static str>,
        shared: Vec<&'static str>,
        wont_end: Vec<&'static str>,
        did: RefCell<Vec<String>>,
    }

    impl Hands for Fake {
        fn runs(&self, s: &Opened) -> bool {
            !self.dead.contains(&s.address.as_str()) && !self.replaced.contains(&s.address.as_str())
        }
        fn dirty(&self, place: &Place) -> Option<String> {
            self.dirty.iter().find(|(p, _)| place.path.ends_with(p)).map(|(_, why)| why.to_string())
        }
        fn in_space(&self, _space: &str) -> Vec<String> {
            self.others.iter().map(|s| s.to_string()).collect()
        }
        fn shared(&self, place: &Place, _mine: &[String]) -> bool {
            self.shared.iter().any(|p| place.path.ends_with(p))
        }
        fn forget(&self, s: &Opened) {
            self.did.borrow_mut().push(format!("forget {}", s.address));
        }
        fn end(&self, address: &str) -> Result<(), String> {
            if self.wont_end.contains(&address) {
                return Err("it won't die".into());
            }
            self.did.borrow_mut().push(format!("end {address}"));
            Ok(())
        }
        fn remove(&self, _space: Option<&str>, place: &Place) -> Removal {
            match self.dirty(place) {
                Some(why) => Removal::Kept(why),
                None => {
                    self.did.borrow_mut().push(format!("remove {}", place.path));
                    Removal::Removed
                }
            }
        }
        fn close_space(&self, space: &str) -> Result<Value, String> {
            self.did.borrow_mut().push(format!("close {space}"));
            Ok(json!({
                "closed": space, "sessions_ended": [{ "address": "t5", "name": "fix-login-eyzflow" }, { "address": "t6", "name": "fix-login-mediaroom" }],
                "sessions_left": [], "worktrees_removed": ["/w/.tower-worktrees/fix-login/eyzflow"], "kept_worktrees": [],
            }))
        }
    }

    #[test]
    fn an_act_says_what_was_opened_and_what_was_assumed() {
        // One session, in the space the window shows.
        let mut a = act("start", Some("M5 Pro"), vec![Opened { worktree: Some(place("herder-tower")), fresh: true, ..opened("t9", "fix-login", "look at the login bug") }]);
        a.assumed = chips_of_start(&json!({
            "account": "orca", "account_why": "as asked",
            "model": "the account's default", "model_why": "none was asked for",
            "folder": "/Users/me/Work/herder-tower", "folder_why": "the best match for \"herder tower\"",
            "worktree": "/Users/me/Work/.tower-worktrees/m5-pro/herder-tower", "worktree_why": "made for this space, on the branch m5-pro/herder-tower",
            "space": "M5 Pro", "space_why": "the one named",
            "name": "fix-login", "name_why": "as asked",
            "port": 3004,
        }));
        assert_eq!(a.line(), "Opened fix login in M5 Pro. It works in its own worktree.");
        assert_eq!(
            a.assumed.iter().map(|c| (c.what.as_str(), c.value.as_str())).collect::<Vec<_>>(),
            [("account", "orca"), ("model", "the account's default"), ("folder", "herder-tower"), ("worktree", "herder-tower"), ("space", "M5 Pro"), ("name", "fix-login")]
        );
        assert_eq!(a.assumed[2].why, "the best match for \"herder tower\"");
        let v = a.view();
        assert!(is_id(v["id"].as_str().unwrap()), "{v}");
        assert_eq!((v["state"].as_str(), v["undo"]["possible"].as_bool(), v["undo"]["why"].is_null(), v["request"].is_null()), (Some("done"), Some(true), true, true));
        assert_eq!(v["sessions"][0], json!({ "address": "t9", "name": "fix-login", "label": "fix login", "worktree": "/w/.tower-worktrees/sandbox/herder-tower", "branch": "sandbox/herder-tower" }));
        assert_eq!(v["assumed"][0], json!({ "what": "account", "value": "orca", "why": "as asked" }));
        // Answering no request, it is for the record; answering one, it is shown with the reply.
        assert!(a.entry().record && a.entry().kind == "act" && a.entry().status.as_deref() == Some("done"));
        a.request = Some(Asked { at: 900, text: "open a session in M5 Pro on my second account".into(), space: None });
        let e = a.entry();
        assert_eq!((e.record, e.id.as_deref() == Some(a.id.as_str()), e.text.as_str(), e.space.as_deref()), (false, true, "Opened fix login in M5 Pro. It works in its own worktree.", Some("M5 Pro")));
        assert_eq!((e.act.as_ref().unwrap()["request_at"].as_u64(), e.act.as_ref().unwrap()["kind"].as_str()), (Some(900), Some("start")));
        assert_eq!(a.view()["request"], json!({ "at": 900, "text": "open a session in M5 Pro on my second account" }));
        // A launch names its sessions by their repos; no space: the sessions alone.
        let mut l = act("launch", Some("fix-login"), vec![opened("t5", "fix-login-eyzflow", "x"), opened("t6", "fix-login-mediaroom", "x")]);
        assert_eq!(l.line(), "Launched fix-login: 2 sessions, eyzflow and mediaroom.");
        for s in &mut l.sessions {
            s.worktree = Some(place("x"));
        }
        assert_eq!(l.line(), "Launched fix-login: 2 sessions, eyzflow and mediaroom. Each works in its own worktree.");
        l.assumed = chips_of_launch(&json!({ "setup": "eyzflow + mediaroom on orca", "setup_why": "the first of 2", "account": "orca", "account_why": "the setup's", "name": "fix-login", "name_why": "the goal's first two words", "worktrees": true, "worktrees_why": "one per repo, on a branch of its own" }));
        assert_eq!(l.assumed.iter().map(|c| c.value.as_str()).collect::<Vec<_>>(), ["eyzflow + mediaroom on orca", "orca", "fix-login", "worktrees"]);
        assert_eq!(act("start", None, vec![opened("t1", "api", ""), opened("t2", "web", ""), opened("t3", "docs", "")]).line(), "Opened 3 sessions: api, web and docs.");
        assert_eq!(act("launch", Some("x"), vec![]).line(), "Launched x, but none of its sessions opened.");
    }

    #[test]
    fn sessions_opened_for_one_request_are_one_act() {
        let asked = |at: u64| Some(Asked { at, text: "open three sessions".into(), space: None });
        let mut b = Book::default();
        let first = b.record(Act { request: asked(900), assumed: vec![Assumed { what: "account".into(), value: "orca".into(), why: "as asked".into() }], ..act("start", Some("M5 Pro"), vec![opened("t1", "api", "a")]) });
        let second = b.record(Act { request: asked(900), assumed: vec![Assumed { what: "account".into(), value: "orca".into(), why: "as asked".into() }, Assumed { what: "folder".into(), value: "web".into(), why: "as asked".into() }], ..act("start", Some("M5 Pro"), vec![opened("t2", "web", "b")]) });
        assert_eq!((b.acts.len(), second.id == first.id, second.sessions.len(), second.assumed.len()), (1, true, 2, 2));
        assert_eq!(second.line(), "Opened 2 sessions in M5 Pro: api and web.");
        // Another request, a launch, or one that answers no request: an act of its own.
        b.record(Act { request: asked(2_000), ..act("start", Some("M5 Pro"), vec![opened("t3", "docs", "c")]) });
        b.record(Act { request: asked(2_000), ..act("launch", Some("fix-login"), vec![opened("t4", "fix-login-eyzflow", "d")]) });
        b.record(act("start", None, vec![opened("t5", "x", "")]));
        b.record(act("start", None, vec![opened("t6", "y", "")]));
        assert_eq!(b.acts.len(), 5);
        // Sessions in two spaces: the act names none.
        let mut two = Book::default();
        two.record(Act { request: asked(1), ..act("start", Some("a"), vec![opened("t1", "api", "")]) });
        assert_eq!(two.record(Act { request: asked(1), ..act("start", Some("b"), vec![opened("t2", "web", "")]) }).space, None);
        // An undone act takes no more sessions; only the last 50 are kept.
        let mut u = Book::default();
        u.record(Act { request: asked(1), ..act("start", None, vec![opened("t1", "api", "")]) });
        u.acts[0].state = "undone".into();
        u.record(Act { request: asked(1), ..act("start", None, vec![opened("t2", "web", "")]) });
        assert_eq!(u.acts.len(), 2);
        for i in 0..60 {
            u.record(act("launch", Some(&format!("s{i}")), vec![]));
        }
        assert_eq!(u.acts.len(), KEEP);
        // `tower undo` with no id: the latest, while it is recent and not undone.
        let mut r = Book::default();
        r.record(Act { at_ms: 1_000, ..act("start", None, vec![opened("t1", "api", "")]) });
        assert!(r.latest(1_000 + RECENT_MS - 1).is_some() && r.latest(1_000 + RECENT_MS).is_none());
        r.acts[0].state = "undone".into();
        assert!(r.latest(2_000).is_none());
        assert!(is_id(&new_id(5)) && !is_id("d0123456789ab") && !is_id("act_12345678") && new_id(5) != new_id(5));
    }

    #[test]
    fn an_act_answers_the_request_in_progress() {
        let e = |kind: &str, at: u64, text: &str, space: Option<&str>| Entry { kind: kind.into(), at, text: text.into(), space: space.map(String::from), ..Default::default() };
        let min = 60_000;
        let entries = vec![
            e("request", min, "open a session in M5 Pro", None),
            e("answer", 2 * min, "Opened.", None),
            e("request", 3 * min, "open one more here", Some("sandbox")),
            Entry { record: true, ..e("request", 4 * min, "kept", None) },
        ];
        assert_eq!(request_for(&entries, None, 5 * min), Some(Asked { at: min, text: "open a session in M5 Pro".into(), space: None }));
        assert_eq!(request_for(&entries, Some("Sandbox"), 5 * min).map(|a| a.at), Some(3 * min), "a space's own tower answers what was asked in its view");
        assert_eq!(request_for(&entries, Some("isa"), 5 * min), None);
        assert_eq!(request_for(&entries, None, 40 * min), None, "an old request isn't what it answers");
        assert_eq!(request_for(&[], None, min), None);
    }

    #[test]
    fn more_work_than_the_first_instruction_holds_an_act() {
        let first = "look at the login bug\n\nYou work in a fresh worktree of eyzflow at /w/.tower-worktrees/sandbox/eyzflow on branch sandbox/eyzflow.";
        // Its first instruction arriving (as typed, wrapped by Herdr, or only its start) is not more work.
        assert!(!is_work(first, first));
        assert!(!is_work(first, &format!("<hc from=\"tower\">{first}")));
        assert!(!is_work(first, "look at the login bug"));
        // Neither is the tower's introduction, nor another session's message, nor nothing.
        assert!(crate::introduce::is_introduction(&crate::introduce::compose("sandbox", &member("t1"), &[member("t1"), member("t2")])));
        assert!(!is_work(first, &crate::introduce::compose("sandbox", &member("t1"), &[member("t1"), member("t2")])));
        assert!(!is_work(first, "From sandbox › mediaroom (t6): which port do you use?"));
        assert!(!is_work(first, "  "));
        // Anything else is: the user typing into it, the tower queueing an instruction.
        assert!(is_work(first, "also fix the logout redirect"));
        assert!(is_work("", "look at the login bug"), "opened with no task: its first prompt is work already");
        let mut b = Book::default();
        b.record(act("start", Some("sandbox"), vec![opened("t5", "eyzflow", first), opened("t6", "mediaroom", "x")]));
        assert!(!b.given("t5", first) && !b.given("t9", "anything"));
        assert!(b.acts[0].view()["undo"]["possible"] == true);
        assert!(b.given("t5", "also fix the   logout redirect"));
        assert!(!b.given("t5", "and one more thing"), "said once");
        let v = b.acts[0].view();
        assert_eq!((v["undo"]["possible"].as_bool(), v["undo"]["why"].as_str()), (Some(false), Some("eyzflow was given more work since (\"also fix the logout redirect\")")));
    }

    fn member(address: &str) -> crate::introduce::Member {
        crate::introduce::Member { address: address.into(), name: format!("s-{address}"), space: "sandbox".into(), folder: "/w/app".into(), ..Default::default() }
    }

    #[test]
    fn undo_ends_what_the_act_opened_and_nothing_else() {
        // One session with a worktree the act made: ended, its worktree gone.
        let a = act("start", Some("sandbox"), vec![Opened { worktree: Some(place("eyzflow")), fresh: true, ..opened("t5", "eyzflow", "x") }]);
        let h = Fake { others: vec!["t5", "t2"], ..Default::default() };
        let out = undo(&a, false, &h).unwrap();
        assert_eq!(*h.did.borrow(), ["forget t5", "end t5", "remove /w/.tower-worktrees/sandbox/eyzflow"]);
        assert_eq!((out.ended.clone(), out.worktrees_removed.clone(), out.space_closed, out.closing.as_str()), (vec!["t5".to_string()], vec!["/w/.tower-worktrees/sandbox/eyzflow".to_string()], false, "Closed it. Nothing else changed."));
        assert_eq!(out.value()["closing"], "Closed it. Nothing else changed.");
        // A worktree that was there before, or that another session works in, stays.
        let was_there = act("start", Some("sandbox"), vec![Opened { worktree: Some(place("eyzflow")), fresh: false, ..opened("t5", "eyzflow", "x") }]);
        let h = Fake::default();
        assert!(undo(&was_there, false, &h).unwrap().worktrees_removed.is_empty());
        assert_eq!(*h.did.borrow(), ["forget t5", "end t5"]);
        let h = Fake { shared: vec!["eyzflow"], ..Default::default() };
        let out = undo(&a, false, &h).unwrap();
        assert_eq!(out.kept_worktrees, [("/w/.tower-worktrees/sandbox/eyzflow".to_string(), "another session works there".to_string())]);
        assert!(out.closing.starts_with("Closed it. Kept its worktree: /w/.tower-worktrees/sandbox/eyzflow (another session works there)."), "{}", out.closing);
        // Refused while a session was given more work, naming it; forced, it is ended.
        let mut more = a.clone();
        more.sessions[0].more = Some("also fix the logout redirect".into());
        let h = Fake::default();
        let refused = undo(&more, false, &h).unwrap_err();
        assert_eq!(refused, Refused::Holds(vec![Hold { address: "t5".into(), name: "eyzflow".into(), why: "was given more work since (\"also fix the logout redirect\")".into() }]));
        assert!(refused.to_string().starts_with("not undone: eyzflow (t5) was given more work since") && refused.to_string().contains("--force"), "{refused}");
        assert!(h.did.borrow().is_empty(), "nothing is touched by a refusal");
        assert_eq!(undo(&more, true, &h).unwrap().ended, ["t5"]);
        // Refused while its worktree holds changes; forced, the session is ended and the worktree is kept.
        let h = Fake { dirty: vec![("eyzflow", "2 uncommitted changes (src/a.rs, src/b.rs)")], ..Default::default() };
        match undo(&a, false, &h).unwrap_err() {
            Refused::Holds(list) => assert_eq!(list[0].why, "has work in its worktree that is nowhere else: 2 uncommitted changes (src/a.rs, src/b.rs)"),
            other => panic!("{other:?}"),
        }
        let out = undo(&a, true, &h).unwrap();
        assert_eq!((out.ended.len(), out.worktrees_removed.len(), out.kept_worktrees.len()), (1, 0, 1));
        assert_eq!(*h.did.borrow(), ["forget t5", "end t5"], "a worktree with changes is never removed");
        // A session that has ended already, one that won't end, and an act undone before.
        let h = Fake { dead: vec!["t5"], ..Default::default() };
        assert_eq!(undo(&was_there, false, &h).unwrap().closing, "It had ended already. Nothing else changed.");
        let h = Fake { wont_end: vec!["t5"], ..Default::default() };
        assert_eq!(undo(&a, false, &h).unwrap_err(), Refused::Failed("couldn't end eyzflow (t5): it won't die".into()));
        let undone = Act { state: "undone".into(), ..a.clone() };
        assert_eq!(undo(&undone, true, &Fake::default()).unwrap_err().to_string(), format!("{} is undone already", undone.id));
        assert!(undone.view()["undo"]["possible"] == false);
        assert!(Refused::Unknown(String::new()).to_string().contains("last 10 minutes") && Refused::Unknown("r1".into()).to_string().starts_with("no act r1"));
    }

    #[test]
    fn the_brain_that_opened_it_is_told_when_somebody_else_undid_it() {
        let mut a = Act { by: "tower".into(), request: Some(Asked { at: 900, text: "open a session for the notes fix".into(), space: None }), ..act("start", Some("sandbox"), vec![opened("t5", "notes-fix", "x")]) };
        // Not undone yet: nothing to tell.
        assert!(undone_note(&a.view(), None).is_none());
        (a.state, a.closing) = ("undone".into(), Some("Closed it. Nothing else changed.".into()));
        // Undone from a window or a phone: the main brain gets one note, and its turn on it is a note turn.
        let note = undone_note(&a.view(), None).unwrap();
        assert!(note.msg.starts_with(&format!("[agenda] The user undid {}, which you opened (it answered \"open a session for the notes fix\"): \"Opened notes fix in sandbox.\" Closed it. Nothing else changed. Its sessions: notes fix (t5).", a.id)), "{}", note.msg);
        assert!(note.msg.contains("don't wait for them") && crate::brain::is_note_prompt(&note.msg));
        assert_eq!((note.space.clone(), note.speech.as_str()), (None, ""), "the main brain's, and nothing is said");
        // The brain that ran the undo itself knows; another brain's undo is told to the one that opened it.
        assert!(undone_note(&a.view(), Some("tower")).is_none());
        assert!(undone_note(&a.view(), Some("tower-sandbox")).is_some());
        // A space's own brain is told in its space.
        a.by = "tower-sandbox".into();
        assert_eq!(undone_note(&a.view(), None).unwrap().space.as_deref(), Some("sandbox"));
        // What a shell, a phone or a session opened has no brain to tell.
        for by in ["cli", "phone:d_1", "t7"] {
            a.by = by.into();
            assert!(undone_note(&a.view(), None).is_none(), "{by}");
        }
    }

    #[test]
    fn undo_never_ends_a_terminal_that_only_has_the_address_now() {
        use serde_json::json;
        // The session is its terminal: made when it was, or (an act kept without that) called what it was.
        let s = Opened { created_ms: Some(5_000), ..opened("t5", "eyzflow", "x") };
        assert!(same_terminal(&s, &json!({ "address": "t5", "created_ms": 5_000, "state": "live" })));
        assert!(!same_terminal(&s, &json!({ "address": "t5", "created_ms": 9_000, "name": "eyzflow" })), "the address came back for another terminal");
        let unnamed = opened("t5", "eyzflow", "x");
        assert!(same_terminal(&unnamed, &json!({ "address": "t5", "labels": { "tower.name": "eyzflow" } })));
        assert!(!same_terminal(&unnamed, &json!({ "address": "t5", "created_ms": 9_000, "name": "notes" })));
        assert!(same_terminal(&unnamed, &json!({ "address": "t5" })), "nothing to tell them apart by");
        // t5 ended long ago and the user's own session has its address now; t6 is still the act's.
        let a = act("start", Some("sandbox"), vec![Opened { worktree: Some(place("eyzflow")), fresh: true, ..opened("t5", "eyzflow", "x") }, opened("t6", "web", "y")]);
        let h = Fake { replaced: vec!["t5"], others: vec!["t5", "t6"], ..Default::default() };
        let out = undo(&a, false, &h).unwrap();
        assert_eq!(out.ended, ["t6"]);
        assert_eq!(*h.did.borrow(), ["forget t6", "end t6", "remove /w/.tower-worktrees/sandbox/eyzflow"], "the stranger keeps its queue, its watch and its life");
        // Nothing of the act runs any more: nothing is ended.
        let h = Fake { replaced: vec!["t5", "t6"], shared: vec!["eyzflow"], ..Default::default() };
        let out = undo(&a, true, &h).unwrap();
        assert!(out.ended.is_empty() && h.did.borrow().is_empty(), "{:?}", h.did.borrow());
        // A space the act made isn't closed whole while a terminal that isn't the act's sits at one of its addresses.
        let launch = Act { space_made: true, ..act("launch", Some("fix-login"), vec![opened("t5", "fix-login-eyzflow", "x"), opened("t6", "fix-login-mediaroom", "x")]) };
        let h = Fake { replaced: vec!["t6"], others: vec!["t5", "t6"], ..Default::default() };
        let out = undo(&launch, false, &h).unwrap();
        assert!(!out.space_closed);
        assert_eq!(*h.did.borrow(), ["forget t5", "end t5"]);
    }

    #[test]
    fn undoing_a_launch_closes_the_space_it_made() {
        let launch = Act { space_made: true, ..act("launch", Some("fix-login"), vec![opened("t5", "fix-login-eyzflow", "x"), opened("t6", "fix-login-mediaroom", "x")]) };
        // Nothing in the space but what it opened: closed whole, as `tower space close` does.
        let h = Fake { others: vec!["t5", "t6"], ..Default::default() };
        let out = undo(&launch, false, &h).unwrap();
        assert_eq!(*h.did.borrow(), ["forget t5", "forget t6", "close fix-login"]);
        assert_eq!((out.space_closed, out.ended.clone(), out.worktrees_removed.len(), out.closing.as_str()), (true, vec!["t5".to_string(), "t6".to_string()], 1, "Closed it. Nothing else changed."));
        // Someone else works there now: only its own sessions are ended, the space stays.
        let h = Fake { others: vec!["t5", "t6", "t9"], ..Default::default() };
        let out = undo(&launch, false, &h).unwrap();
        assert_eq!(*h.did.borrow(), ["forget t5", "end t5", "forget t6", "end t6"]);
        assert!(!out.space_closed);
        // A space it joined is never closed.
        let joined = Act { space_made: false, ..launch.clone() };
        let h = Fake { others: vec!["t5", "t6"], ..Default::default() };
        assert!(!undo(&joined, false, &h).unwrap().space_closed);
    }
}
