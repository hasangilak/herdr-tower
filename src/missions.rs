//! A space is a mission with an end.
//!
//! Every Herdr space the tower knows has a goal (its agenda in
//! `spaces.json`), what counts as done, and a state:
//!
//! - **open**: it has live sessions, or was just made.
//! - **quiet**: no live session, from the moment its last one goes. Its held
//!   news is marked read, and what was open for the user there is over (not
//!   a PR that waits for a merge: see `decisions`). After a day quiet it is
//!   archived: kept in the file, out of the listings.
//! - **ready**: it has a goal, isn't standing, every request about it is
//!   finished and nothing there waits for the user. The tower asks the brain
//!   once for a closing report (`tower space report`). A new request or a
//!   new decision takes it back to open.
//! - **closed**: `tower space close` (the user's word), or by itself once it
//!   has been ready with every session idle for `[spaces] auto_close_hours`.
//!   Its sessions are ended, the clean worktrees the tower made for it are
//!   removed, its report goes into the record, and it is archived.
//!   `tower space resolve` is the user's word that its goal is achieved: the
//!   same close, with the goal marked achieved (`achieved_ms`) and the
//!   record saying so. `--dry-run` (`preview`) says first what either would
//!   end and remove, and changes nothing.
//!
//! A **standing** space is one the user keeps: never ready, never closed by
//! the tower, and its sessions work in the repos themselves (no worktrees).
//!
//! A space opened by a **chat** with the tower (`tower start --space` /
//! `--new-space`, not `tower launch`) gets its goal written from the user's
//! requests there (`crate::goals`: `Mission::chat`). Its sessions already
//! work in the checkout, so they keep doing so (no worktrees, for those
//! running and those opened later), and a goal the tower wrote is no end
//! the user agreed to: it is never ready on it, and so never closed by the
//! tower, until the user (or the brain, on the user's word) sets the goal
//! or what counts as done.
//!
//! `step` is the whole state machine (the daemon runs it every 20 seconds,
//! `daemon::missions`); `view` is what `state.json`, `tower missions` and
//! the phone carry.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hc::{self, SessionStatus};
use crate::history::{self, Entry};
use crate::spaces::{self, Spaces};
use crate::worktrees::{self, Place, Removal};

/// A quiet space is archived after this long.
pub const ARCHIVE_MS: u64 = 24 * 3_600_000;
/// A space just made (or just given a goal) may be without a session this
/// long before it counts as quiet.
pub const GRACE_MS: u64 = 10 * 60_000;
/// A session's last words in the missions are cut to this.
const SAID_CHARS: usize = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Open,
    Quiet,
    Ready,
    Closed,
}

impl State {
    pub fn word(&self) -> &'static str {
        match self {
            State::Open => "open",
            State::Quiet => "quiet",
            State::Ready => "ready",
            State::Closed => "closed",
        }
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn on() -> bool {
    true
}

fn is_on(b: &bool) -> bool {
    *b
}

/// A space as a mission, as `spaces.json` keeps it (its goal is the space's
/// agenda there; follow and mute are the spaces' own lists).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mission {
    /// What counts as done, in the user's words.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub done_when: String,
    /// The user keeps this space: never ready, never closed by the tower.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub standing: bool,
    pub state: State,
    /// Its closing report (`tower space report`), and when it was written.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub report: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub report_ms: u64,
    /// When the brain was asked for the report (once each time it is ready).
    #[serde(skip_serializing_if = "is_zero")]
    pub report_asked_ms: u64,
    /// When the tower first knew it (made, given a goal, or seen).
    pub born_ms: u64,
    /// When its goal was last set: what was finished before that doesn't make it ready.
    #[serde(skip_serializing_if = "is_zero")]
    pub goal_ms: u64,
    /// When a live session was first seen there (0: never).
    #[serde(skip_serializing_if = "is_zero")]
    pub seen_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_since_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_since_ms: Option<u64>,
    /// Ready, with every session idle since then (it closes by itself
    /// `auto_close_hours` later).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_since_ms: Option<u64>,
    /// The tower closes it by itself no sooner than this (`after_away`:
    /// what it didn't watch doesn't count as left alone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_until_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_ms: Option<u64>,
    /// Out of the listings since then: quiet for a day, or closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_ms: Option<u64>,
    /// The worktrees the tower made for it (`crate::worktrees`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub worktrees: Vec<Place>,
    /// The setup `tower launch` made it with.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub setup: String,
    /// The PRs its sessions open get a reviewer (`crate::reviews`): on
    /// unless the user turned it off (`tower space review <space> off`,
    /// `tower launch --no-review`). Left out of the file while on, so a
    /// mission from before it reads as on.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub review: bool,
    /// The user said its goal is achieved (`tower space resolve`), when.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub achieved_ms: Option<u64>,
    /// How its goal came to be what it is, oldest first: the last is the
    /// goal now (`goal_by`). Empty for a goal set before the tower kept this.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub goals: Vec<GoalChange>,
    /// Opened by a chat with the tower, its goal written from the user's
    /// requests (`crate::goals`): its sessions work in the checkout (no
    /// worktrees), and it is ready only on a goal the user set (`may_end`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub chat: bool,
    /// The user cleared its goal by hand: no request writes one again until
    /// a goal is set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub goal_cleared: bool,
}

/// One version of a space's goal: what it became, when, and what made it so.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalChange {
    pub goal: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub done_when: String,
    pub at_ms: u64,
    /// Who: `you` (by hand, in a window or on the phone), `request` (the
    /// tower, after a request of the user's added to what the space is
    /// for: see `crate::goals`), `launch` (the space was made with it),
    /// `tower` (`tower space goal` with no `--by`: the brain, on the user's
    /// word), "" (it was there before the tower kept how goals change).
    pub by: String,
    /// For `request`: the user's words that changed it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request: String,
    /// For `request`: what it added or changed, in a phrase.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// How many versions of a goal are kept: the first (what the space was
/// made for) and the latest.
pub const GOALS_KEPT: usize = 20;

/// Who changes a goal (`GoalChange::by`), and with what.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub by: String,
    pub request: String,
    pub why: String,
}

impl Change {
    pub fn by(who: &str) -> Change {
        Change { by: who.to_string(), ..Default::default() }
    }
}

impl Default for Mission {
    fn default() -> Mission {
        Mission {
            done_when: String::new(),
            standing: false,
            state: State::Open,
            report: String::new(),
            report_ms: 0,
            report_asked_ms: 0,
            born_ms: 0,
            goal_ms: 0,
            seen_ms: 0,
            quiet_since_ms: None,
            ready_since_ms: None,
            idle_since_ms: None,
            wait_until_ms: None,
            closed_ms: None,
            archived_ms: None,
            worktrees: vec![],
            setup: String::new(),
            review: true,
            achieved_ms: None,
            goals: vec![],
            chat: false,
            goal_cleared: false,
        }
    }
}

impl Mission {
    pub fn born(now: u64) -> Mission {
        Mission { born_ms: now, ..Default::default() }
    }

    /// A closed (or archived) space is used again: it starts over as open.
    pub fn reopen(&mut self, now: u64) {
        *self = Mission {
            done_when: std::mem::take(&mut self.done_when),
            standing: self.standing,
            worktrees: std::mem::take(&mut self.worktrees),
            setup: std::mem::take(&mut self.setup),
            review: self.review,
            goals: std::mem::take(&mut self.goals),
            chat: self.chat,
            goal_cleared: self.goal_cleared,
            ..Mission::born(now)
        };
    }

    /// `goal` and `done_when` are its goal now, as `change` made them (at
    /// `now`): kept in its history, after the goal it had before when the
    /// history doesn't have that one yet. Only the first and the latest
    /// `GOALS_KEPT` are kept.
    pub fn record_goal(&mut self, was: (&str, &str), goal: &str, done_when: &str, change: &Change, now: u64) {
        if self.goals.is_empty() && !was.0.trim().is_empty() && (was.0.trim() != goal.trim() || was.1.trim() != done_when.trim()) {
            let at_ms = if self.goal_ms > 0 { self.goal_ms } else { self.born_ms };
            self.goals.push(GoalChange { goal: was.0.trim().to_string(), done_when: was.1.trim().to_string(), at_ms, ..Default::default() });
        }
        self.goals.push(GoalChange {
            goal: goal.trim().to_string(),
            done_when: done_when.trim().to_string(),
            at_ms: now,
            by: change.by.clone(),
            request: change.request.trim().to_string(),
            why: change.why.trim().to_string(),
        });
        if self.goals.len() > GOALS_KEPT {
            let cut = self.goals.len() - GOALS_KEPT;
            self.goals.drain(1..=cut);
        }
    }

    /// The user (by hand, or the brain on their word) set its goal or what
    /// counts as done: not only the tower from their requests.
    pub fn goal_set_by_user(&self) -> bool {
        self.goals.iter().any(|c| c.by == "you" || c.by == "tower")
    }

    /// Whether it can be ready, and so closed by the tower: it has a goal,
    /// isn't standing, and isn't a chat's space on a goal the tower wrote.
    pub fn may_end(&self, has_goal: bool) -> bool {
        has_goal && !self.standing && (!self.chat || self.goal_set_by_user())
    }
}

/// Whether the PRs of `space` get a reviewer: its mission's switch (on when
/// it has none yet), off once it is closed or archived.
pub fn reviews_in(s: &Spaces, space: &str) -> bool {
    match of(s, space) {
        Some((_, m)) => m.review && m.state != State::Closed && m.archived_ms.is_none(),
        None => true,
    }
}

/// The mission of `space`, by its name (any case).
pub fn of<'a>(s: &'a Spaces, space: &str) -> Option<(&'a String, &'a Mission)> {
    let want = space.trim();
    s.missions.get_key_value(want).or_else(|| s.missions.iter().find(|(k, _)| k.eq_ignore_ascii_case(want)))
}

fn goal_of<'a>(s: &'a Spaces, space: &str) -> &'a str {
    s.agendas.get(space).or_else(|| s.agendas.iter().find(|(k, _)| k.eq_ignore_ascii_case(space)).map(|(_, v)| v)).map(|g| g.trim()).unwrap_or("")
}

/// Whether `space` is a mission the tower opens sessions in worktrees for:
/// it has a goal, or `tower launch` made it, and the user doesn't keep it.
/// A space with no goal is the user's own: its sessions work in the repos
/// themselves, as they always did.
/// A chat's space (`Mission::chat`) keeps working in the checkout.
pub fn works_in_worktrees(s: &Spaces, space: &str) -> bool {
    let m = of(s, space).map(|(_, m)| m);
    let launched = m.is_some_and(|m| !m.setup.trim().is_empty());
    (launched || !goal_of(s, space.trim()).is_empty()) && !m.is_some_and(|m| m.standing || (m.chat && !launched))
}

/// Whether `space` is one whose first goal the tower writes from the user's
/// requests (`crate::goals`), and its name in `s`: it has no goal, isn't
/// standing (under any spelling), closed or archived, `tower launch` didn't
/// make it, and the user didn't clear its goal by hand.
pub fn writes_first_goal(s: &Spaces, space: &str) -> Option<String> {
    let want = space.trim();
    if want.is_empty() || !goal_of(s, want).is_empty() {
        return None;
    }
    if s.missions.iter().any(|(k, m)| m.standing && k.trim().eq_ignore_ascii_case(want)) {
        return None;
    }
    match of(s, want) {
        Some((k, m)) => {
            let off = m.state == State::Closed || m.archived_ms.is_some() || !m.setup.trim().is_empty() || m.goal_cleared;
            (!off).then(|| k.clone())
        }
        None => Some(want.to_string()),
    }
}

/// The first goal of a chat's space (`writes_first_goal`), written by the
/// tower from the user's requests there (`change`, by `request`): set, kept
/// in its history, and the space marked as a chat's. `None` when it is no
/// longer one to write it for (a goal came meanwhile, by hand or otherwise).
pub fn first_goal_from_requests(s: &mut Spaces, space: &str, goal: &str, done_when: &str, change: &Change, now: u64) -> Option<GoalChange> {
    let key = writes_first_goal(s, space)?;
    if goal.trim().is_empty() {
        return None;
    }
    goal_by(s, &key, Some(goal), Some(done_when), change, now);
    let m = s.missions.get_mut(&key)?;
    m.chat = true;
    m.goals.last().cloned()
}

/// `works_in_worktrees`, of the spaces as they are on file now.
pub fn is_mission(space: &str) -> bool {
    works_in_worktrees(&spaces::load(), space)
}

// ---- the state machine ----

/// What the tower sees of one space at one look.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// What each of its live sessions is doing.
    pub sessions: Vec<SessionStatus>,
    /// Requests (agendas) about it that aren't finished.
    pub open_requests: usize,
    /// Requests about it that were finished since the mission began (and its goal was set).
    pub finished_requests: usize,
    /// Open decisions there.
    pub decisions: usize,
}

/// What one look changed.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Its last session went: its held news is read, its decisions close (not its merges).
    Quiet(String),
    /// Quiet for a day: out of the listings, and its merges close too.
    Archived(String),
    /// Everything asked for there is finished: the brain is asked for a closing report.
    Ready(String),
    /// Back to open: a session returned, or a new request or decision came.
    Reopened(String),
    /// Ready and left alone long enough: the tower closes it.
    AutoClose(String),
}

/// One look at every space: `facts` for those with live sessions (a space
/// that isn't in it has none). Moves each mission, and says what changed.
pub fn step(s: &mut Spaces, facts: &BTreeMap<String, Facts>, now: u64, auto_close_hours: u64) -> Vec<Effect> {
    let mut names: Vec<String> = facts.keys().cloned().collect();
    for n in s.missions.keys().chain(s.agendas.keys()) {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    let mut out = vec![];
    for name in names {
        let f = facts.get(&name).cloned().unwrap_or_default();
        let has_goal = !goal_of(s, &name).is_empty();
        // Kept under another spelling of its name ("sandbox", before "Sandbox" was there): kept still.
        let kept = !s.missions.contains_key(&name) && s.missions.iter().any(|(k, m)| m.standing && k.trim().eq_ignore_ascii_case(name.trim()));
        let m = s.missions.entry(name.clone()).or_insert_with(|| Mission { standing: kept, ..Mission::born(now) });
        if m.state == State::Closed {
            continue;
        }
        if f.sessions.is_empty() {
            match m.state {
                State::Open | State::Ready if m.seen_ms > 0 || now.saturating_sub(m.born_ms) >= GRACE_MS => {
                    m.state = State::Quiet;
                    m.quiet_since_ms = Some(now);
                    m.idle_since_ms = None;
                    // Its held news is nobody's to catch up on now.
                    s.read_ms.insert(name.clone(), now);
                    out.push(Effect::Quiet(name));
                }
                State::Quiet if m.archived_ms.is_none() && now.saturating_sub(m.quiet_since_ms.unwrap_or(now)) >= ARCHIVE_MS => {
                    // Ready when its sessions left, and left alone since: closed as a ready one is.
                    if m.ready_since_ms.is_some() && m.may_end(has_goal) && auto_close_hours > 0 {
                        if m.wait_until_ms.is_none_or(|w| now >= w) {
                            out.push(Effect::AutoClose(name));
                        }
                    } else {
                        m.archived_ms = Some(now);
                        out.push(Effect::Archived(name));
                    }
                }
                _ => {}
            }
            continue;
        }
        if m.seen_ms == 0 {
            m.seen_ms = now;
        }
        if m.state == State::Quiet {
            m.state = State::Open;
            (m.quiet_since_ms, m.archived_ms, m.ready_since_ms, m.idle_since_ms, m.wait_until_ms) = (None, None, None, None, None);
            out.push(Effect::Reopened(name.clone()));
        }
        let settled = f.open_requests == 0 && f.decisions == 0;
        let may_end = m.may_end(has_goal);
        match m.state {
            State::Open if may_end && settled && f.finished_requests > 0 => {
                m.state = State::Ready;
                m.ready_since_ms = Some(now);
                out.push(Effect::Ready(name.clone()));
            }
            State::Ready if !(may_end && settled) => {
                m.state = State::Open;
                (m.ready_since_ms, m.idle_since_ms, m.wait_until_ms) = (None, None, None);
                // Its report was for what is no longer finished.
                m.report.clear();
                (m.report_ms, m.report_asked_ms) = (0, 0);
                out.push(Effect::Reopened(name.clone()));
            }
            _ => {}
        }
        if m.state == State::Ready {
            if f.sessions.iter().all(SessionStatus::ready) {
                let since = *m.idle_since_ms.get_or_insert(now);
                if auto_close_hours > 0 && now.saturating_sub(since) >= auto_close_hours * 3_600_000 && m.wait_until_ms.is_none_or(|w| now >= w) {
                    out.push(Effect::AutoClose(name));
                }
            } else {
                m.idle_since_ms = None;
            }
        }
    }
    out
}

/// Looks this far apart (or more) are a time the tower wasn't watching: the
/// Mac slept, the voice was down, Herdr couldn't be asked, the clock moved.
pub const AWAY_MS: u64 = 5 * 60_000;
/// After such a time a space is closed by itself no sooner than this.
pub const AWAY_GRACE_MS: u64 = 3_600_000;

/// Whether the time since the last look (`last`: None before the first) is
/// one the tower wasn't watching.
pub fn was_away(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|l| now.saturating_sub(l) >= AWAY_MS)
}

/// The tower wasn't watching for a while (`was_away`): what it didn't see
/// doesn't count as a space left alone. A ready space, and one that was
/// ready when it went quiet, close by themselves no sooner than
/// `AWAY_GRACE_MS` from now (`wait_until_ms`), and only if they are still
/// left alone then: a Mac opened after a weekend doesn't close a space
/// before anyone looked.
pub fn after_away(s: &mut Spaces, now: u64, auto_close_hours: u64) {
    if auto_close_hours == 0 {
        return;
    }
    for m in s.missions.values_mut() {
        let may_close = match m.state {
            State::Ready => true,
            State::Quiet => m.ready_since_ms.is_some() && m.archived_ms.is_none(),
            _ => false,
        };
        if may_close {
            m.wait_until_ms = Some(now + AWAY_GRACE_MS);
        }
    }
}

/// The note that asks the brain for a ready space's closing report.
pub fn report_note(space: &str, goal: &str, done_when: &str) -> String {
    let done = if done_when.trim().is_empty() { String::new() } else { format!(" Done when: \"{}\".", done_when.trim()) };
    format!(
        "[space] \"{space}\" looks finished: every request there is done and nothing waits for the user. Its goal was: \"{}\".{done} Write its closing report now, two or three plain sentences (what was done, what is left open, what the user should know): tower space report \"{space}\" \"<the report>\". Don't close it: only the user closes a space (or it closes by itself when left alone).",
        goal.trim()
    )
}

// ---- what the state carries ----

/// A live session of a space.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Seen {
    #[serde(skip)]
    pub space: String,
    pub address: String,
    pub name: String,
    pub status: String,
    /// The Claude account it runs on, when known.
    pub account: Option<String>,
    /// Its worktree and branch (`crate::worktrees`), when the tower gave it one.
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// Its dev port.
    pub port: Option<u16>,
    /// Its last words, when the voice heard them from its hooks.
    pub said: Option<String>,
    /// A job it left running (`crate::running`): its status may say `done`
    /// while its work goes on; how it shows (`line`: "running a job: …").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<crate::running::Shown>,
    /// A browser it uses now (`crate::browsers`): "using Chrome: sooner.de, since 04:00".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser: Option<crate::browsers::Use>,
    /// How its turn goes, while it works (`crate::progress`): "step 3 of 7:
    /// running the test suite · 14 min in · last update 1 min ago".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub progress: Option<crate::progress::Progress>,
}

/// A terminal as a session of a space, if it is one: live, an agent, in a
/// space, and not the tower's own. `row`: its line in `hc ls`; `full`: its
/// `hc get` (its labels), when asked.
pub fn seen_of(row: &Value, full: Option<&Value>, said: Option<String>) -> Option<Seen> {
    let t = full.unwrap_or(row);
    if row["state"].as_str().is_some_and(|s| s != "live") {
        return None;
    }
    // A plain program reports no status.
    let status = hc::status_word(row).or_else(|| hc::status_word(t))?;
    let place = row["where"].as_str().or_else(|| row["labels"]["where"].as_str()).or_else(|| t["labels"]["where"].as_str())?;
    let space = place.split(" / ").next().map(str::trim).filter(|s| !s.is_empty())?.to_string();
    let name = hc::name(t).or_else(|| hc::name(row)).unwrap_or("");
    // A brain (the main one, or a space's own) and the voice are the tower's.
    // (By its own name: a session called `tower-…` in a tab is a session.)
    if crate::brain::brain_of(t).is_some() || crate::brain::brain_of(row).is_some() || name == crate::brain::VOICE_NAME {
        return None;
    }
    // Whose it is shows in its labels: a row of `hc ls` has none, so a test
    // daemon (whose sessions are the labelled ones) tells only by the closer look.
    if (full.is_some() || !crate::daemon::watch::test_daemon()) && crate::daemon::watch::left_alone(t) {
        return None;
    }
    let given = worktrees::given(t);
    let said = said.map(|s| clip_said(&s)).filter(|s| !s.is_empty());
    let working = status == "working";
    Some(Seen {
        space,
        address: row["address"].as_str()?.to_string(),
        name: hc::spoken_name(t),
        status,
        account: full.and_then(crate::accounts::of_terminal),
        worktree: given.worktree,
        branch: given.branch,
        port: given.port,
        said,
        job: row["address"].as_str().and_then(crate::daemon::running::shown_of),
        browser: row["address"].as_str().and_then(|a| crate::daemon::running::browsers_of(a).into_iter().next()),
        progress: row["address"].as_str().and_then(crate::daemon::running::progress_of).filter(|_| working),
    })
}

/// A session's last words as a space shows them: trimmed, cut short.
pub fn clip_said(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > SAID_CHARS { format!("{}…", s.chars().take(SAID_CHARS).collect::<String>().trim_end()) } else { s.to_string() }
}

/// Every agent session in a space, here and on the other machines, as
/// Herdr has them now. None: Herdr (or a machine) couldn't be asked, so
/// nothing can be said about who is gone. `get`: a terminal's `hc get` (its
/// labels); `said`: its last words, when known.
pub fn scan(get: &dyn Fn(&str) -> Option<Value>, said: &dyn Fn(&str) -> Option<String>) -> Option<Vec<Seen>> {
    let mut out = vec![];
    for row in hc::list_on(None).ok()? {
        // Only those that may be sessions get the closer look.
        if seen_of(&row, None, None).is_none() {
            continue;
        }
        let address = row["address"].as_str().unwrap_or("");
        let full = get(address);
        out.extend(seen_of(&row, full.as_ref(), said(address)));
    }
    // (Another machine's rows carry no labels: a test daemon can't tell its own there.)
    for m in hc::machines().into_iter().filter(|_| !crate::daemon::watch::test_daemon()) {
        for row in hc::list_on(Some(&m)).ok()? {
            let address = row["address"].as_str().unwrap_or("").to_string();
            out.extend(seen_of(&row, None, said(&address)));
        }
    }
    Some(out)
}

/// The start of the local day `now` is in (Unix ms).
pub fn day_start(now: u64) -> u64 {
    let secs = (now / 1000) as libc::time_t;
    // SAFETY: localtime_r fills the struct we own; mktime reads it.
    let start = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        (tm.tm_hour, tm.tm_min, tm.tm_sec) = (0, 0, 0);
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    if start > 0 { start as u64 * 1000 } else { now.saturating_sub(now % 86_400_000) }
}

/// What the missions are built from.
pub struct Sources<'a> {
    /// Every live session in a space (None: Herdr couldn't be asked).
    pub sessions: Option<&'a [Seen]>,
    /// The open decisions, as the state carries them (their `space`).
    pub decisions: &'a [Value],
    pub ledger: &'a crate::agenda::Ledger,
    /// How many messages each space's thread holds.
    pub threads: &'a BTreeMap<String, usize>,
    /// Each space's own brain, by the space's name (`crate::daemon::brains::towers_of`).
    pub towers: &'a BTreeMap<String, Value>,
    pub now: u64,
    pub auto_close_hours: u64,
    /// Each space's active time, by its name and when it opened (`crate::active`; the daemon's).
    pub active: Option<&'a dyn Fn(&str, u64) -> crate::active::Active>,
}

impl Sources<'_> {
    fn sessions_of(&self, space: &str) -> Vec<&Seen> {
        self.sessions.unwrap_or_default().iter().filter(|x| x.space == space).collect()
    }

    /// The decisions in `space` that wait for the user: one on hold doesn't
    /// (the daemon's state lists none; `tower missions` reads them all).
    fn needs(&self, space: &str) -> usize {
        self.decisions.iter().filter(|d| d["hold"].is_null() && d["space"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(space))).count()
    }

    /// What `step` needs: a line for every space with live sessions.
    pub fn facts(&self, s: &Spaces) -> BTreeMap<String, Facts> {
        let mut out: BTreeMap<String, Facts> = BTreeMap::new();
        for x in self.sessions.unwrap_or_default() {
            out.entry(x.space.clone()).or_default().sessions.push(SessionStatus::parse(&x.status));
        }
        for (space, f) in out.iter_mut() {
            // Only what was finished for this mission counts toward its end.
            let since = s.missions.get(space).map(|m| m.born_ms.max(m.goal_ms)).unwrap_or(self.now);
            let r = self.ledger.requests_of(space, since);
            (f.open_requests, f.finished_requests, f.decisions) = (r.open, r.finished, self.needs(space));
        }
        out
    }
}

/// The missions as `state.json`, `tower missions` and the phone carry them:
/// `{"spaces": [Mission], "quiet": [name], "closed_today": [name]}`. The
/// open and ready ones first (those that need the user, then those at work,
/// then the rest; then by name), the quiet ones after them. Archived and
/// closed spaces are left out.
pub fn view(s: &Spaces, src: &Sources) -> Value {
    let today = day_start(src.now);
    let mut rows: Vec<(bool, bool, bool, String, Value)> = vec![];
    for (name, m) in s.missions.iter().filter(|(_, m)| m.state != State::Closed && m.archived_ms.is_none()) {
        let sessions = src.sessions_of(name);
        let needs = src.needs(name);
        let working = sessions.iter().any(|x| x.status == "working");
        let goal = goal_of(s, name);
        let closes = match (m.state, m.idle_since_ms) {
            (State::Ready, Some(since)) if src.auto_close_hours > 0 => Some((since + src.auto_close_hours * 3_600_000).max(m.wait_until_ms.unwrap_or(0))),
            _ => None,
        };
        let v = json!({
            "name": name,
            "goal": goal,
            "done_when": m.done_when,
            // How the goal came to be what it is, oldest first (the last is the goal now).
            "goal_history": m.goals,
            "standing": m.standing,
            // Opened by a chat: its goal is written from the user's requests (`Mission::chat`).
            "chat": m.chat,
            "review": m.review,
            "state": m.state.word(),
            "follow": spaces::followed_in(s, name),
            "mute": spaces::muted_in(s, name),
            "needs": needs,
            "report": m.report,
            "sessions": sessions,
            "requests": src.ledger.requests_of(name, today).rows,
            "thread_count": src.threads.get(name).copied().unwrap_or(0),
            // Its own brain, when it has one (never one of its sessions).
            "tower": src.towers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()),
            "quiet_since_ms": m.quiet_since_ms,
            "ready_since_ms": m.ready_since_ms,
            "auto_close_ms": closes,
        });
        // How long its sessions have been at work: now, today, since it opened (`crate::active`).
        let mut v = v;
        if let Some(f) = src.active {
            v["active"] = json!(f(name, m.born_ms));
        }
        rows.push((m.state == State::Quiet, needs == 0, !working, name.to_lowercase(), v));
    }
    rows.sort_by(|a, b| (a.0, a.1, a.2, &a.3).cmp(&(b.0, b.1, b.2, &b.3)).then_with(|| a.4["name"].as_str().cmp(&b.4["name"].as_str())));
    let quiet: Vec<&Value> = rows.iter().filter(|r| r.0).map(|r| &r.4["name"]).collect();
    let mut closed: Vec<(u64, &String)> =
        s.missions.iter().filter(|(_, m)| m.state == State::Closed).filter_map(|(n, m)| m.closed_ms.filter(|c| *c >= today).map(|c| (c, n))).collect();
    closed.sort();
    json!({
        "spaces": rows.iter().map(|r| &r.4).collect::<Vec<_>>(),
        "quiet": quiet,
        "closed_today": closed.iter().map(|(_, n)| n).collect::<Vec<_>>(),
    })
}

/// The missions with no voice running: from Herdr and the tower's own files.
fn from_files() -> Value {
    let sessions = scan(&hc::get, &|_| None);
    let book = crate::decisions::Book::load(&crate::daemon::decisions::Decisions::path());
    let decisions: Vec<Value> = book.open.iter().map(|d| d.view(false)).collect();
    let ledger = crate::agenda::Ledger::load_from(&crate::agenda::path());
    let threads: BTreeMap<String, usize> = crate::peers::threads(&history::load()).into_iter().map(|(k, v)| (k, v.len())).collect();
    let cfg = crate::config::Config::load().unwrap_or_default();
    let auto_close_hours = cfg.spaces.auto_close_hours;
    // The spaces' own brains as Herdr has them: those due one that doesn't run aren't known here.
    let towers = match cfg.brains.per_space {
        true => crate::daemon::brains::towers_of(&crate::brains::known_from_herdr(), &Default::default()),
        false => BTreeMap::new(),
    };
    view(&spaces::load(), &Sources { sessions: sessions.as_deref(), decisions: &decisions, ledger: &ledger, threads: &threads, towers: &towers, now: history::now_ms(), auto_close_hours, active: None })
}

/// The missions now: the voice's (it looks again first), or from the files.
pub fn current() -> Value {
    if crate::protocol::is_running()
        && let Ok(r) = crate::protocol::send(&crate::protocol::Request::Missions)
        && r.ok
        && let Some(v) = r.state
    {
        return v;
    }
    from_files()
}

/// Has the voice look at the missions now (after a change to `spaces.json`).
pub fn poke() {
    if crate::protocol::is_running() {
        let _ = crate::protocol::send(&crate::protocol::Request::Missions);
    }
}

// ---- `tower space goal|standing|report|close`, `tower missions` ----

fn brief(s: &Spaces, name: &str) -> Value {
    let m = s.missions.get(name).cloned().unwrap_or_default();
    json!({ "space": name, "goal": goal_of(s, name), "done_when": m.done_when, "standing": m.standing, "review": m.review, "state": m.state.word(), "report": m.report, "goal_history": m.goals, "chat": m.chat })
}

/// A space's goal and what counts as done, set in `s` (each when given;
/// a goal of "" clears it, and its history with it), as `change` says who
/// set it (`you`: by hand). A change is kept in the goal's history
/// (`Mission::goals`). A closed space that gets a goal is open again.
/// Returns what `tower space goal` prints.
pub fn goal_by(s: &mut Spaces, name: &str, goal: Option<&str>, done_when: Option<&str>, change: &Change, now: u64) -> Value {
    let was = goal_of(s, name).to_string();
    match goal.map(str::trim) {
        Some("") => {
            s.agendas.remove(name);
        }
        Some(g) => {
            s.agendas.insert(name.to_string(), g.to_string());
        }
        None => {}
    }
    let set = goal.map(str::trim).filter(|g| !g.is_empty());
    let cleared = goal.is_some_and(|g| g.trim().is_empty());
    let m = s.missions.entry(name.to_string()).or_insert_with(|| Mission::born(now));
    let was_done = m.done_when.clone();
    if let Some(d) = done_when {
        m.done_when = d.trim().to_string();
    }
    // Kept in its history (the goal before with the time it was set), then dated.
    if cleared {
        m.goals.clear();
        // Cleared by hand: no request writes one again until a goal is set.
        m.goal_cleared = change.by == "you";
    } else {
        if set.is_some() {
            m.goal_cleared = false;
        }
        let goal_now = set.map(String::from).unwrap_or_else(|| was.clone());
        if !goal_now.is_empty() && (goal_now != was || m.done_when != was_done.trim()) {
            let done_now = m.done_when.clone();
            m.record_goal((&was, &was_done), &goal_now, &done_now, change, now);
        }
    }
    if set.is_some_and(|g| g != was) {
        m.goal_ms = now;
    }
    if set.is_some() && (m.state == State::Closed || m.archived_ms.is_some()) {
        m.reopen(now);
    }
    brief(s, name)
}

/// What a request of the user's made of a space's goal (`crate::goals`):
/// `goal` and `done_when` take the place of `seen` (the goal and done-when
/// the judgement was made on), unless the goal changed since (by hand, or
/// by another request): then nothing changes and `None` says so. A space
/// that was ready to close is open again (what it is for grew, and what
/// was finished before doesn't count toward it: `goal_ms`), and its report,
/// which was of the goal before, goes.
pub fn goal_from_request(s: &mut Spaces, name: &str, seen: (&str, &str), goal: &str, done_when: &str, change: &Change, now: u64) -> Option<GoalChange> {
    let (key, m) = of(s, name).map(|(k, m)| (k.clone(), m.clone()))?;
    if m.standing || m.state == State::Closed || m.archived_ms.is_some() {
        return None;
    }
    if goal_of(s, &key) != seen.0.trim() || m.done_when.trim() != seen.1.trim() {
        return None;
    }
    goal_by(s, &key, Some(goal), Some(done_when), change, now);
    let m = s.missions.get_mut(&key)?;
    if m.state == State::Ready {
        m.state = State::Open;
        (m.ready_since_ms, m.idle_since_ms, m.wait_until_ms) = (None, None, None);
        m.report.clear();
        (m.report_ms, m.report_asked_ms) = (0, 0);
    }
    m.goals.last().cloned()
}

/// Keeps a space (`on`), or makes it a mission with an end again, in `s`.
pub fn standing_in(s: &mut Spaces, name: &str, on: bool, now: u64) -> Value {
    s.missions.entry(name.to_string()).or_insert_with(|| Mission::born(now)).standing = on;
    brief(s, name)
}

/// Turns the review of a space's PRs on or off, in `s`.
pub fn review_in(s: &mut Spaces, name: &str, on: bool, now: u64) -> Value {
    let key = of(s, name).map(|(k, _)| k.clone()).unwrap_or_else(|| name.to_string());
    s.missions.entry(key.clone()).or_insert_with(|| Mission::born(now)).review = on;
    brief(s, &key)
}

/// Sets a space's goal ("" clears it) and, when given, what counts as done,
/// as `by` says who set it (`you`: by hand; else the tower's brain).
/// A closed space that gets a goal is open again.
pub fn set_goal(space: &str, goal: &str, done_when: Option<&str>, by: Option<&str>) -> Result<Value> {
    let name = spaces::pick(space)?;
    let now = history::now_ms();
    let change = Change::by(by.map(str::trim).filter(|b| !b.is_empty()).unwrap_or("tower"));
    let v = spaces::update(|s| goal_by(s, &name, Some(goal), done_when, &change, now));
    poke();
    Ok(v)
}

/// `tower space goal <space> "<goal>" [--done-when "<…>"] [--by you]` (and `tower space agenda`).
pub fn cmd_goal(space: String, goal: String, done_when: Option<String>, by: Option<String>) -> Result<()> {
    let v = set_goal(&space, &goal, done_when.as_deref(), by.as_deref())?;
    let name = v["space"].as_str().unwrap_or("");
    if !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if goal.trim().is_empty() {
        println!("{name}: goal cleared.");
    } else {
        let done = v["done_when"].as_str().filter(|d| !d.is_empty()).map(|d| format!(" Done when: {d}.")).unwrap_or_default();
        println!("{name}: {}.{done}", goal.trim().trim_end_matches('.'));
    }
    Ok(())
}

/// Keeps a space (`on`), or makes it a mission with an end again. A name no
/// space has yet is kept as given, for the space that will have it.
pub fn set_standing(space: &str, on: bool) -> Result<Value> {
    let name = match spaces::pick(space) {
        Ok(n) => n,
        Err(_) if on && !space.trim().is_empty() => space.trim().to_string(),
        Err(e) => return Err(e),
    };
    let now = history::now_ms();
    let v = spaces::update(|s| standing_in(s, &name, on, now));
    poke();
    Ok(v)
}

/// `tower space standing <space> on|off`.
pub fn cmd_standing(mut words: Vec<String>) -> Result<()> {
    let on = match words.pop().map(|w| w.to_lowercase()).as_deref() {
        Some("on" | "yes" | "true") => true,
        Some("off" | "no" | "false") => false,
        _ => bail!("tower space standing <space> on|off"),
    };
    let v = set_standing(&words.join(" "), on)?;
    let name = v["space"].as_str().unwrap_or("");
    if !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if on {
        println!("{name} is a standing space: it is never ready to close and never closed by the tower, and sessions opened there work in the repos themselves.");
    } else {
        println!("{name} is a mission with an end again: sessions opened there get worktrees, and it can be ready to close.");
    }
    Ok(())
}

/// Turns the review of a space's PRs on or off. Off ends a reviewer that
/// is open there (the daemon looks at once). A name no space has yet is
/// kept as given, for the space that will have it.
pub fn set_review(space: &str, on: bool) -> Result<Value> {
    let name = match spaces::pick(space) {
        Ok(n) => n,
        Err(_) if !space.trim().is_empty() => space.trim().to_string(),
        Err(e) => return Err(e),
    };
    let now = history::now_ms();
    let v = spaces::update(|s| review_in(s, &name, on, now));
    poke();
    if crate::protocol::is_running() {
        let _ = crate::protocol::send(&crate::protocol::Request::Review { action: "sweep".into(), pr: String::new(), critical: None, text: String::new(), from: None });
    }
    Ok(v)
}

/// `tower space review <space> on|off`.
pub fn cmd_review(mut words: Vec<String>) -> Result<()> {
    let on = match words.pop().map(|w| w.to_lowercase()).as_deref() {
        Some("on" | "yes" | "true") => true,
        Some("off" | "no" | "false") => false,
        _ => bail!("tower space review <space> on|off"),
    };
    let v = set_review(&words.join(" "), on)?;
    let name = v["space"].as_str().unwrap_or("");
    if !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if on {
        println!("{name}: each PR a session opens there gets a reviewer session, which looks for critical issues only and agrees with the PR's session before you merge.");
    } else {
        println!("{name}: no reviewer for its PRs (one that is open there now ends).");
    }
    Ok(())
}

pub fn set_report(space: &str, text: &str) -> Result<Value> {
    let name = spaces::pick(space)?;
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        bail!("tower space report <space> \"<the closing report>\"");
    }
    let now = history::now_ms();
    let v = spaces::update(|s| {
        let m = s.missions.entry(name.clone()).or_insert_with(|| Mission::born(now));
        (m.report, m.report_ms) = (text.clone(), now);
        brief(s, &name)
    });
    poke();
    Ok(v)
}

/// `tower space report <space> "<text>"`.
pub fn cmd_report(space: String, text: String) -> Result<()> {
    let v = set_report(&space, &text)?;
    if std::io::stdout().is_terminal() {
        println!(
            "{}: closing report kept. It is shown with the space; the user closes it (tower space close), or it closes by itself when left alone.",
            v["space"].as_str().unwrap_or("")
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&v)?);
    }
    Ok(())
}

/// A terminal of a space, for closing it.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    address: String,
    name: String,
    status: SessionStatus,
    /// An agent session (not a plain program).
    agent: bool,
    /// The tower's own (its brain, its voice): never ended with a space.
    own: bool,
    /// A brain: at work on the user's word when it closes a space.
    brain: bool,
    /// The worktree the tower gave it, from its labels.
    given: Option<Place>,
}

/// What a space holds, and whether Herdr's window shows it.
fn targets(name: &str) -> (Option<spaces::Window>, Vec<Target>) {
    let target = |address: &str, shown: &str, status: &str, agent: bool| {
        // A tab's terminal carries its real name, and what the tower gave it, in Herdr's core.
        let full = hc::get(address);
        let named = full.as_ref().and_then(|t| hc::name(t).map(String::from)).unwrap_or_else(|| shown.to_string());
        let given = full.as_ref().map(worktrees::given).and_then(|g| Some(Place { path: g.worktree?, repo: g.repo?, branch: g.branch.unwrap_or_default() }));
        Target {
            address: address.to_string(),
            own: crate::introduce::is_tower(&named),
            // By its own name in Herdr's core; one that can't be read counts as a session (a close then waits for it).
            brain: full.as_ref().is_some_and(|t| crate::brain::brain_of(t).is_some()),
            name: if named.is_empty() { address.to_string() } else { named },
            status: SessionStatus::parse(status),
            agent,
            given,
        }
    };
    if let Some(t) = spaces::tree().into_iter().find(|t| t.window.name.eq_ignore_ascii_case(name)) {
        let list = t.terminals.iter().map(|x| target(&x.address, &x.name, &x.status, !x.app.is_empty())).collect();
        return (Some(t.window), list);
    }
    // Herdr's window isn't open (or the space is gone from it): its terminals by their label.
    let list = hc::list_on(None)
        .unwrap_or_default()
        .iter()
        .filter(|t| t["state"].as_str().is_none_or(|s| s == "live"))
        .filter(|t| t["where"].as_str().and_then(|w| w.split(" / ").next()).is_some_and(|sp| sp.trim().eq_ignore_ascii_case(name)))
        .filter_map(|t| Some(target(t["address"].as_str()?, hc::name(t).unwrap_or(""), &hc::status_word(t).unwrap_or_default(), hc::status_word(t).is_some())))
        .collect();
    (None, list)
}

/// The sessions that hold a space open: at work, or waiting for the user.
fn busy(list: &[Target]) -> Vec<&Target> {
    list.iter().filter(|t| t.agent && !t.own && !t.brain && t.status.passing()).collect()
}

/// Herdr may close the whole space: nothing of the tower's own is in it.
fn whole(list: &[Target]) -> bool {
    !list.iter().any(|t| t.own)
}

/// `herdr-app do <command> '<json>'`.
fn herdr_do(command: &str, args: Value) -> Result<Value> {
    let mut cmd = std::process::Command::new(crate::config::herdr_app_bin());
    cmd.args(["do", command, &args.to_string()]);
    let out = hc::output_within(cmd, &format!("herdr-app do {command}"), Duration::from_secs(30))?;
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
    if out.code != 0 || !v["error"].is_null() {
        bail!("herdr-app {command}: {}{}", out.stdout.trim(), out.stderr.trim());
    }
    Ok(v)
}

/// Ends a space's terminals. Herdr closes a whole space in one command
/// (`space.close`), ending every terminal in it; a space that holds the
/// tower's own terminals is left standing and its other terminals are ended
/// one by one (`hc kill`, then `hc rm`). Returns the terminals ended, those
/// that couldn't be, and whether Herdr's space itself is closed.
fn end_sessions(window: Option<&spaces::Window>, list: &[Target]) -> (Vec<Target>, Vec<(Target, String)>, &'static str) {
    if let Some(w) = window
        && whole(list)
    {
        match herdr_do("space.close", json!({ "id": w.id, "confirmed": true })) {
            Ok(_) => return (list.to_vec(), vec![], "closed"),
            Err(e) => eprintln!("tower: {e:#}; ending its terminals one by one"),
        }
    }
    let (mut ended, mut left) = (vec![], vec![]);
    for t in list.iter().filter(|t| !t.own) {
        let killed = hc::run(&["kill", &t.address]);
        let _ = hc::run(&["rm", &t.address]);
        match killed {
            Ok(o) if o.code == 0 || !hc::is_live(&t.address) => ended.push(t.clone()),
            Ok(o) => left.push((t.clone(), o.stderr.trim().to_string())),
            Err(e) => left.push((t.clone(), format!("{e:#}"))),
        }
    }
    (ended, left, if window.is_some() { "kept" } else { "not shown" })
}

/// The sessions that hold a space open now (at work, or waiting for the
/// user): what `close` without `force` refuses over. Each as `{"address",
/// "name", "status"}`.
pub fn holding(space: &str) -> Result<Vec<Value>> {
    let name = named(space)?;
    let (_, list) = targets(&name);
    Ok(busy(&list).iter().map(|t| json!({ "address": t.address, "name": t.name, "status": t.status.to_string() })).collect())
}

/// The space a close was asked for by what someone called it (`tower space
/// close <words>`): its whole name, else the one space still open that the
/// words are a part of. Words that fit several are refused, naming them: a
/// close ends sessions, and is never a guess. `closed`: the spaces closed
/// already (a part of a name never means one of those).
pub fn to_close(want: &str, known: &[String], closed: &[String]) -> Result<String> {
    if let Some(name) = spaces::exactly(want, known) {
        return Ok(name);
    }
    let open: Vec<String> = known.iter().filter(|k| !closed.contains(k)).cloned().collect();
    match spaces::resolve_only(want, &open) {
        Ok(name) => Ok(name),
        Err(many) if !many.is_empty() => {
            bail!("{want:?} fits several spaces: {}. Nothing is closed: say which one, by its whole name.", many.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", "))
        }
        Err(_) => bail!("no space called {want:?} (spaces: {})", open.join(", ")),
    }
}

/// The space of exactly that name (any case), among those the tower knows:
/// what a close that nobody typed (by itself, an undo, a phone's row) names.
fn named(space: &str) -> Result<String> {
    let known = spaces::known(&spaces::live(), &history::load(), &spaces::load());
    match spaces::exactly(space, &known) {
        Some(name) => Ok(name),
        None => bail!("no space called {space:?}"),
    }
}

/// The worktrees made for a space: those it remembers, and those its
/// sessions (`list`) say they were given in its own folder.
fn places_of(name: &str, mission: &Mission, list: &[Target]) -> Vec<Place> {
    let mut places = mission.worktrees.clone();
    let folder = worktrees::slug(name);
    for p in list.iter().filter_map(|t| t.given.as_ref()) {
        let own = std::path::Path::new(&p.path).parent().and_then(|d| d.file_name()).is_some_and(|d| d.to_string_lossy() == folder.as_str());
        if own && !places.iter().any(|w| w.path == p.path) {
            places.push(p.clone());
        }
    }
    places
}

/// The space's own brain (`tower-<space>`), when it runs: its terminal's
/// address and name (a close ends it first, `end_brain`).
fn own_brain(space: &str) -> Option<(String, String)> {
    let name = crate::brains::name_for(space);
    let t = hc::get(&name)?;
    let live = t["state"].as_str().is_none_or(|s| s == "live");
    (live && crate::brains::is_brain_folder(t["cwd"].as_str().unwrap_or(""))).then(|| (t["address"].as_str().unwrap_or(&name).to_string(), name))
}

/// What closing (or resolving) the space of exactly that name would do, as
/// things stand; nothing is changed. `{"space", "goal", "done_when",
/// "standing", "report", "tower": {"address", "name"} or null, "ends":
/// [{"address", "name", "status", "agent", "busy"}], "stays": [{"address",
/// "name"}], "busy": [...], "worktrees":
/// [{"path", "branch", "repo", "removed", "why"?}], "herdr_space"}`:
/// `tower`, its own brain, which it ends first; `ends`, every terminal of
/// the space it ends (agents and plain shells; `busy`: an
/// agent at work or waiting for the user, which a close without force
/// refuses over); `stays`, the tower's own terminals; a worktree with work
/// not committed or pushed is kept (`removed` false, and why); `herdr_space`
/// is `closed` (Herdr's space goes), `kept`, or `not shown`.
pub fn preview(space: &str) -> Result<Value> {
    let name = named(space)?;
    let (window, list) = targets(&name);
    let s = spaces::load();
    let mission = of(&s, &name).map(|(_, m)| m.clone()).unwrap_or_default();
    let worktrees: Vec<Value> = places_of(&name, &mission, &list)
        .iter()
        .map(|p| {
            let (removed, why) = match worktrees::dirty(p) {
                Ok(None) => (true, None),
                Ok(Some(why)) => (false, Some(why)),
                Err(e) => (false, Some(format!("{e:#}"))),
            };
            json!({ "path": p.path, "branch": p.branch, "repo": p.repo, "removed": removed, "why": why })
        })
        .collect();
    let held = busy(&list);
    let ends: Vec<Value> = list
        .iter()
        .filter(|t| !t.own)
        .map(|t| json!({ "address": t.address, "name": t.name, "status": t.status.to_string(), "agent": t.agent, "busy": held.iter().any(|h| h.address == t.address) }))
        .collect();
    let herdr_space = match &window {
        None => "not shown",
        Some(_) if whole(&list) => "closed",
        Some(_) => "kept",
    };
    Ok(json!({
        "space": name,
        "goal": goal_of(&s, &name),
        "done_when": mission.done_when,
        "standing": mission.standing,
        "report": mission.report,
        "tower": own_brain(&name).map(|(address, name)| json!({ "address": address, "name": name })),
        "ends": ends,
        "stays": list.iter().filter(|t| t.own).map(|t| json!({ "address": t.address, "name": t.name })).collect::<Vec<_>>(),
        "busy": held.iter().map(|t| json!({ "address": t.address, "name": t.name, "status": t.status.to_string() })).collect::<Vec<_>>(),
        "worktrees": worktrees,
        "herdr_space": herdr_space,
    }))
}

/// `close_named`, for a space by what someone called it (`to_close`).
pub fn close(space: &str, force: bool, tell_voice: bool) -> Result<Value> {
    let s = spaces::load();
    let known = spaces::known(&spaces::live(), &history::load(), &s);
    let closed: Vec<String> = s.missions.iter().filter(|(_, m)| m.state == State::Closed).map(|(k, _)| k.clone()).collect();
    close_named(&to_close(space, &known, &closed)?, force, tell_voice)
}

/// Closes the space of exactly that name: ends its sessions, removes the
/// clean worktrees the tower made for it, puts its closing report into the
/// record, and archives it. Refused while a session there works or waits
/// for the user, unless `force`. `tell_voice`: have the running voice look
/// at once (not when the voice itself closes it).
pub fn close_named(space: &str, force: bool, tell_voice: bool) -> Result<Value> {
    close_as(space, force, tell_voice, false)
}

/// `resolve_named`, for a space by what someone called it (`to_close`).
pub fn resolve(space: &str, force: bool, tell_voice: bool) -> Result<Value> {
    let s = spaces::load();
    let known = spaces::known(&spaces::live(), &history::load(), &s);
    let closed: Vec<String> = s.missions.iter().filter(|(_, m)| m.state == State::Closed).map(|(k, _)| k.clone()).collect();
    resolve_named(&to_close(space, &known, &closed)?, force, tell_voice)
}

/// The user's word that the goal of the space of exactly that name is
/// achieved: it is marked so, and the space is closed as `close_named`
/// closes it (refused the same way while a session there works or waits
/// for the user, unless `force`). Only a space with a goal: one without has
/// nothing to achieve (close it instead).
pub fn resolve_named(space: &str, force: bool, tell_voice: bool) -> Result<Value> {
    let name = named(space)?;
    let s = spaces::load();
    if goal_of(&s, &name).trim().is_empty() {
        bail!("\"{name}\" has no goal to resolve: close it instead (tower space close)");
    }
    close_as(&name, force, tell_voice, true)
}

/// `close_named`; `resolved`: the user said its goal is achieved.
fn close_as(space: &str, force: bool, tell_voice: bool, resolved: bool) -> Result<Value> {
    let name = named(space)?;
    let (window, list) = targets(&name);
    let held = busy(&list);
    if !held.is_empty() && !force {
        let who: Vec<String> = held.iter().map(|t| format!("{} ({}) is {}", t.name, t.address, t.status)).collect();
        bail!("\"{name}\" isn't closed: {}. Wait for them, or close it anyway with --force (their work in progress is cut off).", who.join(", "));
    }
    // Its own brain goes first: it has nothing to say about sessions that end with their space.
    let brain_ended = end_brain(&name, tell_voice);
    // What was still asked for there is dropped with it (before its sessions end: none is told as stalled).
    let ledger = crate::agenda::Ledger::load_from(&crate::agenda::path());
    let mut dropped = vec![];
    for id in ledger.only_in(&name) {
        let cmd = crate::agenda::Cmd { action: "close".into(), target: id.clone(), force: true, text: "its space was closed".into(), ..Default::default() };
        match crate::agenda_do(&cmd) {
            Ok(_) => dropped.push(id),
            Err(e) => eprintln!("tower: {id} stays open: {e:#}"),
        }
    }
    let goal = goal_of(&spaces::load(), &name).trim().to_string();
    let (ended, left, herdr_space) = end_sessions(window.as_ref(), &list);
    // Their programs let go of the worktrees first.
    let until = std::time::Instant::now() + Duration::from_secs(8);
    while ended.iter().any(|t| hc::is_live(&t.address)) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(300));
    }
    let kept_own: Vec<&Target> = list.iter().filter(|t| t.own).collect();
    let now = history::now_ms();
    let before = spaces::load();
    let mission = of(&before, &name).map(|(_, m)| m.clone()).unwrap_or_default();
    // The worktrees made for it: those it remembers, and those its sessions say they were given.
    let places = places_of(&name, &mission, &list);
    let (mut removed, mut kept) = (vec![], vec![]);
    // Who still runs here, now that its own sessions are over: a session of another space
    // (one renamed in Herdr, one whose name makes the same folder) may work in one of them.
    let still: Vec<Value> = match places.is_empty() {
        true => vec![],
        false => hc::list_on(None)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t["state"].as_str().is_none_or(|s| s == "live"))
            .map(|t| t["address"].as_str().and_then(hc::get).unwrap_or(t))
            .collect(),
    };
    for p in &places {
        if !left.is_empty() {
            kept.push((p.clone(), "a session there couldn't be ended".to_string()));
            continue;
        }
        if let Some(who) = worktrees::used_by(p, &still) {
            kept.push((p.clone(), format!("{who} still works there")));
            continue;
        }
        match worktrees::remove(p) {
            Removal::Removed | Removal::Gone => removed.push(p.clone()),
            Removal::Kept(why) => kept.push((p.clone(), why)),
        }
    }
    let report = mission.report.clone();
    let text = closed_text(&name, resolved.then_some(goal.as_str()), &report);
    history::append(Entry { kind: "space".into(), space: Some(name.clone()), status: Some("closed".into()), text, record: true, ..Default::default() });
    spaces::update(|s| {
        let m = s.missions.entry(name.clone()).or_insert_with(|| Mission::born(now));
        m.state = State::Closed;
        if resolved {
            m.achieved_ms = Some(now);
        }
        (m.closed_ms, m.archived_ms) = (Some(now), Some(now));
        (m.quiet_since_ms, m.idle_since_ms, m.wait_until_ms) = (None, None, None);
        m.worktrees = kept.iter().map(|(p, _)| p.clone()).collect();
        s.read_ms.insert(name.clone(), now);
        if s.focus.as_ref().is_some_and(|f| f.eq_ignore_ascii_case(&name)) {
            (s.focus, s.focus_ms) = (None, None);
        }
    });
    if tell_voice {
        poke();
    }
    let who = |t: &Target| json!({ "address": t.address, "name": t.name });
    Ok(json!({
        "closed": name,
        "resolved": resolved,
        "goal": goal,
        "sessions_ended": ended.iter().map(who).collect::<Vec<_>>(),
        "sessions_left": left.iter().map(|(t, why)| json!({ "address": t.address, "name": t.name, "why": why })).chain(kept_own.iter().map(|t| json!({ "address": t.address, "name": t.name, "why": "the tower's own terminal" }))).collect::<Vec<_>>(),
        "herdr_space": herdr_space,
        "worktrees_removed": removed.iter().map(|p| &p.path).collect::<Vec<_>>(),
        "kept_worktrees": kept.iter().map(|(p, why)| json!({ "path": p.path, "branch": p.branch, "repo": p.repo, "why": why })).collect::<Vec<_>>(),
        "requests_closed": dropped,
        "tower_ended": brain_ended,
        "report": report,
    }))
}

/// The record's line for a space that closed: resolved (`goal`: the user
/// said it is achieved) or closed, with its report.
fn closed_text(name: &str, goal: Option<&str>, report: &str) -> String {
    let head = match goal {
        Some(g) if !g.trim().is_empty() => format!("{name} is resolved: its goal is achieved ({}), and it is closed.", g.trim()),
        Some(_) => format!("{name} is resolved, and it is closed."),
        None => format!("{name} is closed."),
    };
    if report.trim().is_empty() { format!("{head} No closing report.") } else { format!("{head} {}", report.trim()) }
}

/// Ends a space's own brain (`tower-<space>`), if it has one: by the voice
/// when it runs (it also holds the brain off while the space closes; not
/// when the voice itself is closing the space: `ask_voice` false), else
/// here. Returns the names of the terminals ended.
fn end_brain(space: &str, ask_voice: bool) -> Vec<String> {
    if let Some(b) = crate::daemon::brains::get() {
        return b.close(space);
    }
    if ask_voice
        && crate::protocol::is_running()
        && let Ok(r) = crate::protocol::send(&crate::protocol::Request::Brains { action: "close".into(), space: space.to_string() })
        && r.ok
    {
        return r.state.unwrap_or_default()["ended"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
    }
    let name = crate::brains::name_for(space);
    match hc::get(&name) {
        Some(t) if crate::brains::is_brain_folder(t["cwd"].as_str().unwrap_or("")) && crate::brain::stop(&name) => vec![name],
        _ => vec![],
    }
}

/// `tower space close <space> [--force]`.
pub fn cmd_close(space: String, force: bool) -> Result<()> {
    print_closed(&close(&space, force, true)?)
}

/// `tower space resolve <space> [--force] [--dry-run]`: with `--dry-run`,
/// what it would end and remove (`preview`), nothing changed.
pub fn cmd_resolve(space: String, force: bool, dry_run: bool) -> Result<()> {
    if dry_run {
        let s = spaces::load();
        let known = spaces::known(&spaces::live(), &history::load(), &s);
        let closed: Vec<String> = s.missions.iter().filter(|(_, m)| m.state == State::Closed).map(|(k, _)| k.clone()).collect();
        let v = preview(&to_close(&space, &known, &closed)?)?;
        if std::io::stdout().is_terminal() {
            println!("{}", preview_text(&v));
        } else {
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        return Ok(());
    }
    print_closed(&resolve(&space, force, true)?)
}

/// A preview (`preview`) in a few lines: what ends, what stays, which
/// worktrees go and which are kept, and what holds it open.
pub fn preview_text(v: &Value) -> String {
    let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    let name = v["space"].as_str().unwrap_or("");
    let mut out = vec![];
    let ends = list("ends");
    if let Some(t) = v["tower"].as_object() {
        out.push(format!("Ends its own tower first: {} ({}).", t["name"].as_str().unwrap_or(""), t["address"].as_str().unwrap_or("")));
    }
    if ends.is_empty() {
        out.push(format!("Resolving \"{name}\" ends no terminal of its own: none is open there."));
    } else {
        let each: Vec<String> = ends
            .iter()
            .map(|t| {
                let kind = if t["agent"] == true { t["status"].as_str().filter(|s| !s.is_empty()).unwrap_or("idle").to_string() } else { "a terminal".to_string() };
                format!("{} ({}, {kind})", t["name"].as_str().unwrap_or(""), t["address"].as_str().unwrap_or(""))
            })
            .collect();
        out.push(format!("Resolving \"{name}\" ends: {}.", each.join(", ")));
    }
    for t in list("stays") {
        out.push(format!("{} ({}) stays: it is the tower's own.", t["name"].as_str().unwrap_or(""), t["address"].as_str().unwrap_or("")));
    }
    for w in list("worktrees") {
        let what = w["branch"].as_str().filter(|b| !b.is_empty()).or(w["path"].as_str()).unwrap_or("");
        if w["removed"] == true {
            out.push(format!("Removes the worktree {what} (its branch stays)."));
        } else {
            out.push(format!("Keeps the worktree {what}: {}.", w["why"].as_str().unwrap_or("it has work in it").trim_end_matches('.')));
        }
    }
    let busy = list("busy");
    if !busy.is_empty() {
        let who: Vec<String> = busy.iter().map(|t| format!("{} is {}", t["name"].as_str().unwrap_or(""), t["status"].as_str().unwrap_or(""))).collect();
        out.push(format!("Not without --force: {}.", who.join(", ")));
    }
    out.join("\n")
}

/// What a close or a resolve did, for a person (or its JSON, piped).
fn print_closed(v: &Value) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    let names = |k: &str| list(k).iter().map(|t| format!("{} ({})", t["name"].as_str().unwrap_or(""), t["address"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ");
    if v["resolved"] == true {
        println!("Resolved \"{}\": its goal is achieved, and it is closed.", v["closed"].as_str().unwrap_or(""));
    } else {
        println!("Closed \"{}\".", v["closed"].as_str().unwrap_or(""));
    }
    if !list("sessions_ended").is_empty() {
        println!("Ended: {}.", names("sessions_ended"));
    }
    for t in list("sessions_left") {
        println!("Still running: {} ({}): {}.", t["name"].as_str().unwrap_or(""), t["address"].as_str().unwrap_or(""), t["why"].as_str().unwrap_or(""));
    }
    match v["herdr_space"].as_str() {
        Some("kept") => println!("Herdr's space itself stays (it holds the tower's own terminal, or Herdr wouldn't close it)."),
        Some("not shown") => println!("Herdr's window doesn't show that space: nothing to close there."),
        _ => {}
    }
    if !list("worktrees_removed").is_empty() {
        let paths: Vec<String> = list("worktrees_removed").iter().filter_map(|p| p.as_str().map(String::from)).collect();
        println!("Removed its worktrees (their branches stay): {}.", paths.join(", "));
    }
    for w in list("kept_worktrees") {
        println!("Kept the worktree {} (branch {}): {}.", w["path"].as_str().unwrap_or(""), w["branch"].as_str().unwrap_or(""), w["why"].as_str().unwrap_or(""));
    }
    if !list("requests_closed").is_empty() {
        let ids: Vec<String> = list("requests_closed").iter().filter_map(|p| p.as_str().map(String::from)).collect();
        println!("Closed its open requests: {}.", ids.join(", "));
    }
    match v["report"].as_str().filter(|r| !r.is_empty()) {
        Some(r) => println!("Its closing report is in the record: {r}"),
        None => println!("It had no closing report."),
    }
    Ok(())
}

/// A session was opened in `space` (`tower start`, `tower launch`): the
/// space is a mission the tower knows, open again if it was closed, and it
/// remembers the worktree made for it.
pub fn opened(space: &str, worktree: Option<&Place>) {
    let now = history::now_ms();
    spaces::update(|s| {
        let name = of(s, space).map(|(k, _)| k.clone()).unwrap_or_else(|| space.trim().to_string());
        let m = s.missions.entry(name).or_insert_with(|| Mission::born(now));
        if m.state == State::Closed || m.archived_ms.is_some() {
            m.reopen(now);
        }
        if let Some(p) = worktree
            && !m.worktrees.iter().any(|w| w.path == p.path)
        {
            m.worktrees.push(p.clone());
        }
    });
}

/// `tower missions [--json]`.
pub fn cmd_missions(as_json: bool) -> Result<()> {
    let v = current();
    if as_json || !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let rows = v["spaces"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("No open spaces.");
    }
    for m in &rows {
        let sessions = m["sessions"].as_array().cloned().unwrap_or_default();
        let working = sessions.iter().filter(|x| x["status"] == "working").count();
        let needs = m["needs"].as_u64().unwrap_or(0);
        println!(
            "{:<20} {:<6} {}{} session(s), {working} working{}  {}",
            m["name"].as_str().unwrap_or(""),
            m["state"].as_str().unwrap_or(""),
            if needs > 0 { format!("{needs} need you, ") } else { String::new() },
            sessions.len(),
            if m["standing"] == true { ", standing" } else { "" },
            m["goal"].as_str().unwrap_or("")
        );
        for x in &sessions {
            let place = match (x["worktree"].as_str(), x["port"].as_u64()) {
                (Some(w), Some(p)) => format!("  {w} (port {p})"),
                (Some(w), None) => format!("  {w}"),
                _ => String::new(),
            };
            println!("    {:<8} {:<24} {:<8}{place}", x["address"].as_str().unwrap_or(""), x["name"].as_str().unwrap_or(""), x["status"].as_str().unwrap_or(""));
        }
        for r in m["requests"].as_array().into_iter().flatten() {
            println!("    {} {} ({}, {}/{})", r["id"].as_str().unwrap_or(""), r["title"].as_str().unwrap_or(""), r["state"].as_str().unwrap_or(""), r["done"], r["total"]);
        }
        if let Some(r) = m["report"].as_str().filter(|r| !r.is_empty()) {
            println!("    report: {r}");
        }
    }
    let closed: Vec<&str> = v["closed_today"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !closed.is_empty() {
        println!("Closed today: {}.", closed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_goals_history_keeps_who_changed_it_and_the_goal_it_had_before() {
        let mut s = Spaces::default();
        // A goal from before the tower kept its history.
        s.agendas.insert("Shop".into(), "Ship the checkout".into());
        s.missions.insert("Shop".into(), Mission { goal_ms: 50, done_when: "it is live".into(), ..Mission::born(10) });
        goal_by(&mut s, "Shop", Some("Ship the checkout, with Apple Pay"), None, &Change::by("you"), 100);
        let g = &s.missions["Shop"].goals;
        assert_eq!(g.iter().map(|c| (c.goal.as_str(), c.by.as_str(), c.at_ms)).collect::<Vec<_>>(), [("Ship the checkout", "", 50), ("Ship the checkout, with Apple Pay", "you", 100)]);
        assert_eq!(g[1].done_when, "it is live", "a version keeps the done-when it had");
        // The same again is no new version; a done-when alone is one.
        goal_by(&mut s, "Shop", Some("Ship the checkout, with Apple Pay"), None, &Change::by("you"), 110);
        assert_eq!(s.missions["Shop"].goals.len(), 2);
        goal_by(&mut s, "Shop", None, Some("it is live; Apple Pay works"), &Change::by("tower"), 120);
        assert_eq!(s.missions["Shop"].goals.last().map(|c| (c.by.as_str(), c.done_when.as_str())), Some(("tower", "it is live; Apple Pay works")));
        assert_eq!(brief(&s, "Shop")["goal_history"].as_array().map(Vec::len), Some(3), "tower space goal prints it");
        // The first version and the latest are kept.
        for i in 0..30 {
            goal_by(&mut s, "Shop", Some(&format!("Ship the checkout, step {i}")), None, &Change::by("request"), 200 + i);
        }
        let g = &s.missions["Shop"].goals;
        assert_eq!((g.len(), g[0].goal.as_str(), g.last().unwrap().goal.as_str()), (GOALS_KEPT, "Ship the checkout", "Ship the checkout, step 29"));
        // Cleared: its history goes with it.
        goal_by(&mut s, "Shop", Some(""), None, &Change::by("you"), 300);
        assert!(s.missions["Shop"].goals.is_empty());
        // A request's change, when the goal is still what it was weighed on.
        goal_by(&mut s, "Shop", Some("Ship the checkout"), Some(""), &Change::by("launch"), 400);
        let req = Change { by: "request".into(), request: "add Apple Pay too".into(), why: "adds Apple Pay".into() };
        assert!(goal_from_request(&mut s, "Shop", ("Ship the checkout", "other"), "Ship it with Apple Pay", "", &req, 410).is_none(), "the done-when changed meanwhile");
        let c = goal_from_request(&mut s, "shop", ("Ship the checkout", ""), "Ship it with Apple Pay", "Apple Pay works", &req, 420).unwrap();
        assert_eq!((c.by.as_str(), c.request.as_str(), s.agendas["Shop"].as_str(), s.missions["Shop"].goal_ms), ("request", "add Apple Pay too", "Ship it with Apple Pay", 420));
        // The view carries it for the windows and the phone.
        let v = view(&s, &Sources { sessions: None, decisions: &[], ledger: &crate::agenda::Ledger::default(), threads: &BTreeMap::new(), towers: &BTreeMap::new(), now: 500, auto_close_hours: 24, active: None });
        let row = v["spaces"].as_array().unwrap().iter().find(|r| r["name"] == "Shop").unwrap();
        assert_eq!(row["goal_history"].as_array().unwrap().last().unwrap()["by"], "request");
    }

    /// `tower missions` reads every open decision, those on hold too: one on
    /// hold needs nobody, so a space whose only decision is held needs 0.
    #[test]
    fn a_decision_on_hold_needs_nobody_in_its_space() {
        let mut s = Spaces::default();
        s.missions.insert("Docs".into(), Mission::born(100));
        let held = json!({ "id": "d1", "space": "docs", "hold": { "reason": "after the launch", "by": "mac", "at_ms": 200 } });
        let waits = json!({ "id": "d2", "space": "Docs" });
        let needs = |decisions: &[Value]| {
            let v = view(&s, &Sources { sessions: None, decisions, ledger: &crate::agenda::Ledger::default(), threads: &BTreeMap::new(), towers: &BTreeMap::new(), now: 500, auto_close_hours: 24, active: None });
            v["spaces"][0]["needs"].as_u64()
        };
        assert_eq!(needs(std::slice::from_ref(&held)), Some(0));
        assert_eq!(needs(&[held, waits]), Some(1));
    }

    #[test]
    fn a_resolved_space_says_its_goal_is_achieved_in_the_record() {
        assert_eq!(closed_text("isa", Some("Ship the clips"), "Clips are live."), "isa is resolved: its goal is achieved (Ship the clips), and it is closed. Clips are live.");
        assert_eq!(closed_text("isa", None, ""), "isa is closed. No closing report.");
        assert_eq!(closed_text("isa", Some(" "), ""), "isa is resolved, and it is closed. No closing report.");
    }

    #[test]
    fn a_preview_reads_as_what_it_would_end() {
        let v = json!({
            "space": "isa",
            "ends": [{ "address": "t9", "name": "clips", "status": "working", "agent": true }, { "address": "t10", "name": "shell", "status": "", "agent": false }],
            "stays": [],
            "busy": [{ "address": "t9", "name": "clips", "status": "working" }],
            "worktrees": [{ "path": "/w/a", "branch": "isa/app", "removed": false, "why": "1 commit not pushed" }],
        });
        assert_eq!(preview_text(&v), "Resolving \"isa\" ends: clips (t9, working), shell (t10, a terminal).\nKeeps the worktree isa/app: 1 commit not pushed.\nNot without --force: clips is working.");
    }
    use crate::agenda::{Ledger, Reg, Tag, TaskEv};

    const MIN: u64 = 60_000;
    const HOUR: u64 = 3_600_000;

    fn with_goal(name: &str, goal: &str) -> Spaces {
        let mut s = Spaces::default();
        s.agendas.insert(name.into(), goal.into());
        s
    }

    fn live(statuses: &[&str]) -> Facts {
        Facts { sessions: statuses.iter().map(|w| SessionStatus::parse(w)).collect(), ..Default::default() }
    }

    fn at(space: &str, f: Facts) -> BTreeMap<String, Facts> {
        BTreeMap::from([(space.to_string(), f)])
    }

    #[test]
    fn only_a_mission_gets_worktrees() {
        // A space of the user's own, with no goal: its sessions work in the repos themselves.
        let mut s = Spaces::default();
        s.missions.insert("M5 Pro".into(), Mission::born(1));
        assert!(!works_in_worktrees(&s, "M5 Pro"));
        assert!(!works_in_worktrees(&s, "never heard of"), "nor one the tower doesn't know");
        // A goal makes it a mission (its name in any case); clearing the goal undoes that.
        s.agendas.insert("M5 Pro".into(), "Ship checkout v2".into());
        assert!(works_in_worktrees(&s, "M5 Pro") && works_in_worktrees(&s, " m5 pro "));
        s.agendas.insert("M5 Pro".into(), "  ".into());
        assert!(!works_in_worktrees(&s, "M5 Pro"), "a blank goal is no goal");
        // One `tower launch` made is a mission from its first session on.
        s.missions.insert("fix-login".into(), Mission { setup: "eyzflow on orca".into(), ..Mission::born(1) });
        assert!(works_in_worktrees(&s, "fix-login"));
        // A standing space never, goal or not.
        s.agendas.insert("support".into(), "Answer the tickets".into());
        s.missions.insert("support".into(), Mission { standing: true, ..Mission::born(1) });
        assert!(!works_in_worktrees(&s, "support"));
        s.missions.get_mut("fix-login").unwrap().standing = true;
        assert!(!works_in_worktrees(&s, "fix-login"));
        // A goal set ahead of the space's first session counts.
        assert!(works_in_worktrees(&with_goal("billing", "Fix the invoice bug"), "billing"));
    }

    #[test]
    fn a_spaces_prs_are_reviewed_unless_the_user_turned_it_off() {
        // A new mission, and one kept before there was the switch: on.
        assert!(Mission::born(1).review && Mission::default().review);
        let old: Spaces = serde_json::from_str(r#"{"missions":{"shop":{"state":"open","born_ms":1}}}"#).unwrap();
        assert!(old.missions["shop"].review && reviews_in(&old, "Shop"), "absent is on");
        assert!(reviews_in(&Spaces::default(), "never-seen"), "a space the tower doesn't know yet: on");
        // On is left out of the file; off is kept.
        assert!(!serde_json::to_string(&Mission::born(1)).unwrap().contains("review"));
        let mut s = old.clone();
        let v = review_in(&mut s, "SHOP", false, 2);
        assert_eq!((v["space"].as_str(), v["review"].as_bool()), (Some("shop"), Some(false)), "its own spelling");
        assert!(!reviews_in(&s, "shop"));
        assert!(serde_json::to_string(&s).unwrap().contains("\"review\":false"));
        // Kept when the space is used again.
        let m = s.missions.get_mut("shop").unwrap();
        m.reopen(3);
        assert!(!m.review);
        review_in(&mut s, "shop", true, 4);
        assert!(reviews_in(&s, "shop"));
        // A closed or archived space's PRs get none.
        s.missions.get_mut("shop").unwrap().state = State::Closed;
        assert!(!reviews_in(&s, "shop"));
        let mut s = old;
        s.missions.get_mut("shop").unwrap().archived_ms = Some(5);
        assert!(!reviews_in(&s, "shop"));
    }

    #[test]
    fn a_space_goes_quiet_with_its_last_session_and_is_archived_a_day_later() {
        let mut s = Spaces::default();
        // First sight of a space with sessions: a mission, open.
        assert!(step(&mut s, &at("Shop", live(&["working", "idle"])), HOUR, 24).is_empty());
        assert_eq!((s.missions["Shop"].state, s.missions["Shop"].seen_ms, s.missions["Shop"].born_ms), (State::Open, HOUR, HOUR));
        // Its last session goes: quiet from that moment, and its held news is read.
        let t = HOUR + 5 * MIN;
        assert_eq!(step(&mut s, &BTreeMap::new(), t, 24), [Effect::Quiet("Shop".into())]);
        let m = &s.missions["Shop"];
        assert_eq!((m.state, m.quiet_since_ms, m.archived_ms), (State::Quiet, Some(t), None));
        assert_eq!(s.read_ms.get("Shop"), Some(&t));
        // Still quiet a while later: nothing more.
        assert!(step(&mut s, &BTreeMap::new(), t + 23 * HOUR, 24).is_empty());
        // A day quiet: archived, kept in the file.
        assert_eq!(step(&mut s, &BTreeMap::new(), t + 24 * HOUR, 24), [Effect::Archived("Shop".into())]);
        assert_eq!((s.missions["Shop"].state, s.missions["Shop"].archived_ms), (State::Quiet, Some(t + 24 * HOUR)));
        assert!(step(&mut s, &BTreeMap::new(), t + 30 * HOUR, 24).is_empty(), "once");
        // A session there again: open, and back in the listings.
        assert_eq!(step(&mut s, &at("Shop", live(&["idle"])), t + 31 * HOUR, 24), [Effect::Reopened("Shop".into())]);
        let m = &s.missions["Shop"];
        assert_eq!((m.state, m.quiet_since_ms, m.archived_ms), (State::Open, None, None));
    }

    #[test]
    fn a_space_just_made_waits_for_its_first_session() {
        let mut s = with_goal("fix-login", "Fix the login");
        s.missions.insert("fix-login".into(), Mission::born(HOUR));
        assert!(step(&mut s, &BTreeMap::new(), HOUR + MIN, 24).is_empty());
        assert_eq!(s.missions["fix-login"].state, State::Open, "its sessions are still opening");
        assert_eq!(step(&mut s, &BTreeMap::new(), HOUR + GRACE_MS, 24), [Effect::Quiet("fix-login".into())]);
        // A goal for a space nobody has seen makes it a mission too.
        let mut s = with_goal("Docs", "Write the guide");
        assert!(step(&mut s, &BTreeMap::new(), HOUR, 24).is_empty());
        assert_eq!(s.missions["Docs"].born_ms, HOUR);
    }

    #[test]
    fn a_space_is_ready_when_all_asked_for_is_finished_and_open_again_with_more() {
        let mut s = with_goal("Shop", "Ship the new checkout");
        let done = |sessions: &[&str]| Facts { finished_requests: 1, ..live(sessions) };
        // Work in progress: open.
        assert!(step(&mut s, &at("Shop", Facts { open_requests: 1, ..live(&["working"]) }), HOUR, 24).is_empty());
        // No request yet finished there: a goal alone doesn't make it ready.
        assert!(step(&mut s, &at("Shop", live(&["idle"])), HOUR + MIN, 24).is_empty());
        // Something still waits for the user: not ready.
        assert!(step(&mut s, &at("Shop", Facts { decisions: 1, ..done(&["idle"]) }), HOUR + 2 * MIN, 24).is_empty());
        assert_eq!(s.missions["Shop"].state, State::Open);
        // Every request finished, nothing waiting: ready.
        let t = HOUR + 3 * MIN;
        assert_eq!(step(&mut s, &at("Shop", done(&["working", "idle"])), t, 24), [Effect::Ready("Shop".into())]);
        let m = &s.missions["Shop"];
        assert_eq!((m.state, m.ready_since_ms, m.idle_since_ms), (State::Ready, Some(t), None), "one still works: not idle yet");
        assert!(step(&mut s, &at("Shop", done(&["idle", "done"])), t + MIN, 24).is_empty(), "ready once");
        assert_eq!(s.missions["Shop"].idle_since_ms, Some(t + MIN));
        // Its report is written.
        s.missions.get_mut("Shop").unwrap().report = "The checkout shipped.".into();
        s.missions.get_mut("Shop").unwrap().report_asked_ms = t;
        // A new decision there: open again, and the report no longer stands.
        assert_eq!(step(&mut s, &at("Shop", Facts { decisions: 1, ..done(&["idle"]) }), t + 2 * MIN, 24), [Effect::Reopened("Shop".into())]);
        let m = &s.missions["Shop"];
        assert_eq!((m.state, m.ready_since_ms, m.idle_since_ms, m.report.as_str(), m.report_asked_ms), (State::Open, None, None, "", 0));
        // Settled: ready again; a new request: open again.
        assert_eq!(step(&mut s, &at("Shop", done(&["idle"])), t + 3 * MIN, 24), [Effect::Ready("Shop".into())]);
        assert_eq!(step(&mut s, &at("Shop", Facts { open_requests: 1, ..done(&["working"]) }), t + 4 * MIN, 24), [Effect::Reopened("Shop".into())]);
        // No goal, or a standing space: never ready.
        let mut plain = Spaces::default();
        assert!(step(&mut plain, &at("M5 Pro", done(&["idle"])), HOUR, 24).is_empty());
        let mut kept = with_goal("M5 Pro", "The tower itself");
        kept.missions.insert("M5 Pro".into(), Mission { standing: true, ..Mission::born(HOUR) });
        assert!(step(&mut kept, &at("M5 Pro", done(&["idle"])), 2 * HOUR, 24).is_empty());
        assert_eq!(kept.missions["M5 Pro"].state, State::Open);
        // Ready, then made standing: open again.
        let mut s = with_goal("Shop", "Ship it");
        step(&mut s, &at("Shop", done(&["idle"])), HOUR, 24);
        s.missions.get_mut("Shop").unwrap().standing = true;
        assert_eq!(step(&mut s, &at("Shop", done(&["idle"])), HOUR + MIN, 24), [Effect::Reopened("Shop".into())]);
    }

    #[test]
    fn a_chats_space_keeps_its_checkout_and_is_ready_only_on_a_goal_the_user_set() {
        let done = |sessions: &[&str]| Facts { finished_requests: 1, ..live(sessions) };
        // Its first goal, written by the tower from a request: a chat's space.
        let mut s = Spaces::default();
        s.missions.insert("search-llm".into(), Mission { seen_ms: 1, ..Mission::born(1) });
        assert_eq!(writes_first_goal(&s, " Search-LLM ").as_deref(), Some("search-llm"));
        let req = Change { by: "request".into(), request: "fine tune the search for the mediaroom api".into(), why: "the work".into() };
        let c = first_goal_from_requests(&mut s, "search-llm", "Fine-tune the search model for the MediaRoom API", "p95 under 800 ms", &req, HOUR).unwrap();
        assert_eq!((c.by.as_str(), c.goal.as_str()), ("request", "Fine-tune the search model for the MediaRoom API"));
        let m = &s.missions["search-llm"];
        assert!(m.chat && m.goals.len() == 1 && m.goal_ms == HOUR);
        assert!(writes_first_goal(&s, "search-llm").is_none(), "once");
        assert!(first_goal_from_requests(&mut s, "search-llm", "Another", "", &req, HOUR).is_none());
        // Its sessions work in the checkout, those running and those opened later.
        assert!(!works_in_worktrees(&s, "search-llm"));
        // Everything finished: not ready on a goal the tower wrote, so never closed by it.
        assert!(step(&mut s, &at("search-llm", done(&["idle"])), 2 * HOUR, 24).is_empty());
        assert_eq!(s.missions["search-llm"].state, State::Open);
        // A request that grows it: still the tower's goal.
        goal_from_request(&mut s, "search-llm", ("Fine-tune the search model for the MediaRoom API", "p95 under 800 ms"), "Fine-tune and serve it", "served", &req, 3 * HOUR);
        assert!(step(&mut s, &at("search-llm", done(&["idle"])), 4 * HOUR, 24).is_empty());
        // The user sets the goal: a mission with an end like any (its checkout stays).
        goal_by(&mut s, "search-llm", None, Some("served on the GPU host"), &Change::by("you"), 5 * HOUR);
        assert_eq!(step(&mut s, &at("search-llm", done(&["idle"])), 6 * HOUR, 24), [Effect::Ready("search-llm".into())]);
        assert!(!works_in_worktrees(&s, "search-llm"));
    }

    #[test]
    fn a_first_goal_is_for_a_bare_space_only() {
        let mut s = Spaces::default();
        assert_eq!(writes_first_goal(&s, "new-one").as_deref(), Some("new-one"), "one not known yet: made a moment ago");
        s.missions.insert("M5 Pro".into(), Mission { standing: true, ..Mission::born(1) });
        assert!(writes_first_goal(&s, "m5 pro").is_none(), "standing, under any spelling");
        s.missions.insert("Shop".into(), Mission { setup: "launch".into(), ..Mission::born(1) });
        assert!(writes_first_goal(&s, "Shop").is_none(), "launched");
        s.missions.insert("old".into(), Mission { state: State::Closed, ..Mission::born(1) });
        s.missions.insert("gone".into(), Mission { archived_ms: Some(2), ..Mission::born(1) });
        assert!(writes_first_goal(&s, "old").is_none() && writes_first_goal(&s, "gone").is_none());
        s.agendas.insert("b2c".into(), "Answer tickets".into());
        assert!(writes_first_goal(&s, "b2c").is_none(), "it has one");
        // Cleared by hand: none again until a goal is set.
        goal_by(&mut s, "b2c", Some(""), None, &Change::by("you"), 3);
        assert!(writes_first_goal(&s, "b2c").is_none());
        goal_by(&mut s, "b2c", Some("Answer tickets"), None, &Change::by("you"), 4);
        goal_by(&mut s, "b2c", Some(""), None, &Change::by("tower"), 5);
        assert_eq!(writes_first_goal(&s, "b2c").as_deref(), Some("b2c"), "cleared by the brain: written again");
    }

    #[test]
    fn a_ready_space_left_alone_closes_by_itself() {
        let mut s = with_goal("Shop", "Ship the new checkout");
        let done = |sessions: &[&str]| at("Shop", Facts { finished_requests: 2, ..live(sessions) });
        assert_eq!(step(&mut s, &done(&["idle"]), HOUR, 24), [Effect::Ready("Shop".into())]);
        assert_eq!(s.missions["Shop"].idle_since_ms, Some(HOUR));
        // A session works again for a while: the wait starts over when it stops.
        assert!(step(&mut s, &done(&["working"]), 10 * HOUR, 24).is_empty());
        assert_eq!(s.missions["Shop"].idle_since_ms, None);
        assert!(step(&mut s, &done(&["done"]), 11 * HOUR, 24).is_empty());
        assert!(step(&mut s, &done(&["idle"]), 34 * HOUR, 24).is_empty(), "23 hours idle");
        assert_eq!(step(&mut s, &done(&["idle"]), 35 * HOUR, 24), [Effect::AutoClose("Shop".into())]);
        // The hours are the user's; 0 is never.
        assert_eq!(step(&mut s.clone(), &done(&["idle"]), 13 * HOUR, 2), [Effect::AutoClose("Shop".into())]);
        assert!(step(&mut s, &done(&["idle"]), 1000 * HOUR, 0).is_empty());
        // Blocked is not idle.
        let mut s = with_goal("Shop", "Ship it");
        step(&mut s, &done(&["blocked"]), HOUR, 1);
        assert_eq!((s.missions["Shop"].state, s.missions["Shop"].idle_since_ms), (State::Ready, None));
        assert!(step(&mut s, &done(&["blocked"]), 50 * HOUR, 1).is_empty());
        // Ready when its sessions left: after its day quiet it is closed, not only archived.
        let mut s = with_goal("Shop", "Ship it");
        step(&mut s, &done(&["idle"]), HOUR, 24);
        assert_eq!(step(&mut s, &BTreeMap::new(), 2 * HOUR, 24), [Effect::Quiet("Shop".into())]);
        assert_eq!(step(&mut s, &BTreeMap::new(), 26 * HOUR, 24), [Effect::AutoClose("Shop".into())]);
        assert_eq!(step(&mut s.clone(), &BTreeMap::new(), 26 * HOUR, 0), [Effect::Archived("Shop".into())], "never closed by itself");
        // A closed space stays closed, whatever is seen there.
        let m = s.missions.get_mut("Shop").unwrap();
        (m.state, m.closed_ms, m.archived_ms) = (State::Closed, Some(26 * HOUR), Some(26 * HOUR));
        assert!(step(&mut s, &done(&["working"]), 27 * HOUR, 24).is_empty());
        assert_eq!(s.missions["Shop"].state, State::Closed);
        // Used again (a session opened there, a new goal): it starts over.
        s.missions.get_mut("Shop").unwrap().reopen(28 * HOUR);
        let m = &s.missions["Shop"];
        assert_eq!((m.state, m.closed_ms, m.archived_ms, m.born_ms), (State::Open, None, None, 28 * HOUR));
    }

    #[test]
    fn a_space_kept_under_another_spelling_of_its_name_is_never_ready() {
        // "Keep sandbox for good", said before Herdr had the space; it is made as "Sandbox".
        let mut s = with_goal("Sandbox", "The playground");
        s.missions.insert("sandbox".into(), Mission { standing: true, ..Mission::born(HOUR) });
        let done = Facts { finished_requests: 1, ..live(&["idle"]) };
        // (The name as it was first said has no session of its own: that one only goes quiet.)
        assert_eq!(step(&mut s, &at("Sandbox", done.clone()), 2 * HOUR, 24), [Effect::Quiet("sandbox".into())], "never ready");
        assert!(s.missions["Sandbox"].standing, "kept, as it was said");
        assert!(step(&mut s, &at("Sandbox", done), 40 * HOUR, 24).iter().all(|e| *e == Effect::Archived("sandbox".into())), "and never closed by the tower");
        assert_eq!(s.missions["Sandbox"].state, State::Open);
        // A space nobody keeps, beside one that is kept: on its own terms.
        let mut s = with_goal("Shop", "Ship it");
        s.missions.insert("sandbox".into(), Mission { standing: true, ..Mission::born(HOUR) });
        assert!(step(&mut s, &at("Shop", Facts { finished_requests: 1, ..live(&["idle"]) }), 2 * HOUR, 24).contains(&Effect::Ready("Shop".into())));
        assert!(!s.missions["Shop"].standing);
    }

    #[test]
    fn what_the_tower_didnt_watch_doesnt_count_as_left_alone() {
        let mut s = with_goal("Shop", "Ship the new checkout");
        let done = |sessions: &[&str]| Facts { finished_requests: 1, ..live(sessions) };
        let t = 100 * HOUR;
        assert_eq!(step(&mut s, &at("Shop", done(&["idle"])), t, 24), [Effect::Ready("Shop".into())]);
        // Looks 20 seconds apart are no time away; the first of a run, or one after five minutes, is.
        assert!(!was_away(Some(t), t + 20_000) && was_away(Some(t), t + AWAY_MS) && was_away(None, t));
        // The Mac slept for 60 hours. The first look after it closes nothing: nobody has seen the space since.
        let woke = t + 60 * HOUR;
        after_away(&mut s, woke, 24);
        assert!(step(&mut s, &at("Shop", done(&["idle"])), woke, 24).is_empty(), "not closed the moment the Mac wakes");
        assert_eq!(s.missions["Shop"].wait_until_ms, Some(woke + AWAY_GRACE_MS));
        assert!(step(&mut s, &at("Shop", done(&["idle"])), woke + AWAY_GRACE_MS - MIN, 24).is_empty());
        // Still left alone an hour on: closed.
        assert_eq!(step(&mut s, &at("Shop", done(&["idle"])), woke + AWAY_GRACE_MS, 24), [Effect::AutoClose("Shop".into())]);
        // The same for one that was ready when its sessions left, a day quiet.
        let mut s = with_goal("Docs", "Write the guide");
        assert_eq!(step(&mut s, &at("Docs", done(&["idle"])), t, 24), [Effect::Ready("Docs".into())]);
        assert_eq!(step(&mut s, &BTreeMap::new(), t + MIN, 24), [Effect::Quiet("Docs".into())]);
        let woke = t + 3 * ARCHIVE_MS;
        after_away(&mut s, woke, 24);
        assert!(step(&mut s, &BTreeMap::new(), woke, 24).is_empty());
        assert_eq!(step(&mut s, &BTreeMap::new(), woke + AWAY_GRACE_MS, 24), [Effect::AutoClose("Docs".into())]);
        // A session back before then: open again, and nothing waits any more.
        let mut s = with_goal("Api", "Ship it");
        assert_eq!(step(&mut s, &at("Api", done(&["idle"])), t, 24), [Effect::Ready("Api".into())]);
        after_away(&mut s, woke, 24);
        assert_eq!(step(&mut s, &at("Api", Facts { open_requests: 1, ..done(&["working"]) }), woke + MIN, 24), [Effect::Reopened("Api".into())]);
        assert_eq!(s.missions["Api"].wait_until_ms, None);
        // With auto-close off nothing is marked.
        let mut s = with_goal("Web", "Ship it");
        assert_eq!(step(&mut s, &at("Web", done(&["idle"])), t, 0), [Effect::Ready("Web".into())]);
        after_away(&mut s, woke, 0);
        assert_eq!(s.missions["Web"].wait_until_ms, None);
    }

    #[test]
    fn a_close_never_guesses_which_space_was_meant() {
        let known: Vec<String> = ["fix-login", "fix-login-2", "fix-signup", "M5 Pro"].map(String::from).to_vec();
        // Its whole name (any case), though another begins with it.
        assert_eq!(to_close("Fix-Login", &known, &[]).unwrap(), "fix-login");
        // A part of one name only.
        assert_eq!(to_close("m5", &known, &[]).unwrap(), "M5 Pro");
        assert_eq!(to_close("signup", &known, &[]).unwrap(), "fix-signup");
        // A part of several: refused, naming them. Nothing is closed.
        let e = to_close("fix", &known, &[]).unwrap_err().to_string();
        assert!(e.contains("fits several spaces") && e.contains("\"fix-login\"") && e.contains("\"fix-login-2\"") && e.contains("\"fix-signup\"") && e.contains("Nothing is closed"), "{e}");
        // One that is closed already isn't what a part of a name means; by its whole name it still answers.
        let closed = vec!["fix-login".to_string(), "fix-login-2".to_string()];
        assert_eq!(to_close("fix", &known, &closed).unwrap(), "fix-signup");
        assert_eq!(to_close("fix-login-2", &known, &closed).unwrap(), "fix-login-2");
        assert!(to_close("nothing", &known, &[]).unwrap_err().to_string().starts_with("no space called"));
    }

    fn seen(space: &str, address: &str, status: &str) -> Seen {
        Seen { space: space.into(), address: address.into(), name: format!("s-{address}"), status: status.into(), ..Default::default() }
    }

    /// Shop: a request with two tasks (one verified), one finished just now, one finished long ago. Docs: one finished just now.
    fn ledger(now: u64) -> Ledger {
        let mut l = Ledger::default();
        let reg = |l: &mut Ledger, agenda: &str, queue_id: u64, session: &str, space: &str| {
            let tag = Tag { agenda: Some(agenda.into()), title: format!("task {queue_id}"), space: Some(space.into()), ..Default::default() };
            l.register(&Reg { queue_id, session: session.into(), name: session.into(), text: format!("do {queue_id}"), tag }, &Default::default(), now - HOUR).unwrap().task
        };
        let a = l.open("Ship the new checkout with the redesigned payment step", "", "user", 0, &["Shop".to_string()], now - HOUR);
        let t1 = reg(&mut l, &a, 1, "t1", "Shop");
        reg(&mut l, &a, 2, "t2", "Shop");
        l.apply(&t1, TaskEv::Verify { how: "checked".into() }, now).unwrap();
        let b = l.open("Fix the cart badge", "The cart badge", "user", 0, &["Shop".to_string()], now - HOUR);
        let t = reg(&mut l, &b, 3, "t1", "Shop");
        l.apply(&t, TaskEv::Verify { how: "checked".into() }, now).unwrap();
        let c = l.open("An old one", "", "user", 0, &["Shop".to_string()], now - HOUR);
        let t = reg(&mut l, &c, 4, "t1", "Shop");
        l.apply(&t, TaskEv::Verify { how: "checked".into() }, now).unwrap();
        l.agendas.iter_mut().find(|x| x.id == c).unwrap().closed_ms = Some(1);
        let d = l.open("Write the guide", "", "user", 0, &[], now - HOUR);
        let t = reg(&mut l, &d, 5, "t7", "Docs");
        l.apply(&t, TaskEv::Verify { how: "checked".into() }, now).unwrap();
        l
    }

    #[test]
    fn the_missions_are_listed_in_a_stable_order_with_all_the_ui_needs() {
        let now = 1_800_000_000_000;
        let mut s = Spaces::default();
        for (name, goal) in [("Zeta", "Needs you"), ("alpha", "At work"), ("Beta", "At work too"), ("Docs", "Write the guide"), ("Shop", "Ship the new checkout"), ("Old", "Long gone"), ("Asleep", "No sessions")] {
            s.agendas.insert(name.into(), goal.into());
            s.missions.insert(name.into(), Mission::born(now - HOUR));
        }
        s.follow.insert("Shop".into());
        s.mute.insert("docs".into());
        let m = s.missions.get_mut("Shop").unwrap();
        (m.done_when, m.state, m.report, m.ready_since_ms, m.idle_since_ms) =
            ("the checkout is live".into(), State::Ready, "It shipped.".into(), Some(now - 2 * HOUR), Some(now - HOUR));
        let m = s.missions.get_mut("Asleep").unwrap();
        (m.state, m.quiet_since_ms) = (State::Quiet, Some(now - HOUR));
        let m = s.missions.get_mut("Old").unwrap();
        (m.state, m.quiet_since_ms, m.archived_ms) = (State::Quiet, Some(now - 30 * HOUR), Some(now - 6 * HOUR));
        s.missions.insert("Done one".into(), Mission { state: State::Closed, closed_ms: Some(now - MIN), archived_ms: Some(now - MIN), ..Mission::born(now - HOUR) });
        s.missions.insert("Done before".into(), Mission { state: State::Closed, closed_ms: Some(now - 40 * 24 * HOUR), ..Mission::born(now - HOUR) });
        s.missions.get_mut("Docs").unwrap().standing = true;
        let mut shop = seen("Shop", "t1", "idle");
        (shop.account, shop.worktree, shop.branch, shop.port, shop.said) =
            (Some("orca".into()), Some("/w/.tower-worktrees/shop/web".into()), Some("shop/web".into()), Some(3004), Some("Done: the checkout is live.".into()));
        let sessions = vec![seen("Zeta", "t9", "blocked"), seen("alpha", "t3", "working"), seen("Beta", "t4", "working"), seen("Docs", "t7", "idle"), shop, seen("Shop", "t2", "done")];
        let decisions = vec![json!({ "id": "d1", "space": "zeta" }), json!({ "id": "d2", "space": "Zeta" }), json!({ "id": "d3", "space": null })];
        let l = ledger(now);
        let threads = BTreeMap::from([("Shop".to_string(), 7usize)]);
        // Shop has a brain of its own, and Zeta is due one that doesn't run.
        let towers = BTreeMap::from([
            ("Shop".to_string(), json!({ "name": "tower-shop", "status": "ready", "address": "t20" })),
            ("zeta".to_string(), json!({ "name": "tower-zeta", "status": "off", "address": null })),
        ]);
        let src = Sources { sessions: Some(&sessions), decisions: &decisions, ledger: &l, threads: &threads, towers: &towers, now, auto_close_hours: 24, active: None };
        let v = view(&s, &src);
        let names: Vec<&str> = v["spaces"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap()).collect();
        // Needs you, then at work (by name, any case), then the rest, then the quiet ones.
        assert_eq!(names, ["Zeta", "alpha", "Beta", "Docs", "Shop", "Asleep"]);
        assert_eq!(v["quiet"], json!(["Asleep"]));
        assert_eq!(v["closed_today"], json!(["Done one"]));
        let shop = &v["spaces"][4];
        assert_eq!(
            shop,
            &json!({
                "name": "Shop",
                "goal": "Ship the new checkout",
                "done_when": "the checkout is live",
                "goal_history": [],
                "standing": false,
                "chat": false,
                "review": true,
                "state": "ready",
                "follow": true,
                "mute": false,
                "needs": 0,
                "report": "It shipped.",
                "sessions": [
                    { "address": "t1", "name": "s-t1", "status": "idle", "account": "orca", "worktree": "/w/.tower-worktrees/shop/web", "branch": "shop/web", "port": 3004, "said": "Done: the checkout is live." },
                    { "address": "t2", "name": "s-t2", "status": "done", "account": null, "worktree": null, "branch": null, "port": null, "said": null },
                ],
                "requests": [
                    { "id": "a1", "title": "Ship the new checkout with the redesigned payment step", "state": "open", "done": 1, "total": 2 },
                    { "id": "a2", "title": "The cart badge", "state": "done", "done": 1, "total": 1 },
                ],
                "thread_count": 7,
                "tower": { "name": "tower-shop", "status": "ready", "address": "t20" },
                "quiet_since_ms": null,
                "ready_since_ms": now - 2 * HOUR,
                "auto_close_ms": now + 23 * HOUR,
            })
        );
        let zeta = &v["spaces"][0];
        assert_eq!((zeta["needs"].as_u64(), zeta["state"].as_str(), zeta["sessions"][0]["status"].as_str()), (Some(2), Some("open"), Some("blocked")));
        assert_eq!(zeta["tower"], json!({ "name": "tower-zeta", "status": "off", "address": null }), "by the space's name, in any case");
        assert_eq!(zeta["sessions"].as_array().map(Vec::len), Some(1), "a space's brain is its `tower`, never one of its sessions");
        let docs = &v["spaces"][3];
        assert!(docs["tower"].is_null(), "no brain of its own: the main one has it");
        assert_eq!((docs["standing"].as_bool(), docs["mute"].as_bool(), docs["requests"][0]["id"].as_str()), (Some(true), Some(true), Some("a4")));
        let asleep = &v["spaces"][5];
        assert_eq!((asleep["state"].as_str(), asleep["sessions"].as_array().map(Vec::len), asleep["quiet_since_ms"].as_u64()), (Some("quiet"), Some(0), Some(now - HOUR)));
        // What `step` reads from the same sources.
        let facts = src.facts(&s);
        assert_eq!(
            facts["Shop"],
            Facts { sessions: vec![SessionStatus::Idle, SessionStatus::Done], open_requests: 1, finished_requests: 1, decisions: 0 },
            "what was finished before the mission began doesn't count"
        );
        assert_eq!((facts["Zeta"].decisions, facts["Docs"].finished_requests, facts.contains_key("Asleep")), (2, 1, false));
        // A goal set just now: nothing finished before it makes the space ready.
        let mut later = s.clone();
        later.missions.get_mut("Docs").unwrap().goal_ms = now + 1;
        assert_eq!(src.facts(&later)["Docs"].finished_requests, 0);
        // Herdr couldn't be asked: the missions are still there, without sessions.
        let blind = view(&s, &Sources { sessions: None, ..src });
        assert_eq!(blind["spaces"].as_array().map(Vec::len), Some(6));
    }

    #[test]
    fn a_terminal_is_a_session_of_its_space_unless_it_is_the_towers() {
        let row = json!({ "address": "t5", "agent": "claude-code", "cwd": "/w/.tower-worktrees/shop/web", "state": "live", "status": "working", "turn": 3, "where": "Shop / web" });
        let full = json!({
            "address": "t5", "state": "live", "status": { "app": "claude-code", "state": "working" },
            "labels": { "where": "Shop / web", "tower.name": "shop-web", "tower.worktree": "/w/.tower-worktrees/shop/web", "tower.branch": "shop/web", "tower.port": "3004" },
        });
        let s = seen_of(&row, Some(&full), Some(format!("  {}  ", "x".repeat(300)))).unwrap();
        assert_eq!((s.space.as_str(), s.address.as_str(), s.name.as_str(), s.status.as_str()), ("Shop", "t5", "web", "working"), "its name within its space");
        assert_eq!((s.worktree.as_deref(), s.branch.as_deref(), s.port), (Some("/w/.tower-worktrees/shop/web"), Some("shop/web"), Some(3004)));
        assert_eq!(s.said.as_ref().map(|x| x.chars().count()), Some(SAID_CHARS + 1), "its last words, cut short");
        // Before its labels are read it is one already (they only add to it).
        assert_eq!(seen_of(&row, None, None).map(|s| (s.space, s.worktree)), Some(("Shop".into(), None)));
        // A plain program, a finished terminal, one in no space: not sessions.
        assert!(seen_of(&json!({ "address": "t1", "agent": "zsh", "state": "live", "where": "Shop / sh" }), None, None).is_none());
        assert!(seen_of(&json!({ "address": "t1", "state": "finished", "status": "idle", "where": "Shop / x" }), None, None).is_none());
        assert!(seen_of(&json!({ "address": "t1", "state": "live", "status": "idle" }), None, None).is_none());
        // The tower's own: its brain, a space's brain, its voice.
        for name in ["tower", "tower-shop", "tower-voice"] {
            let t = json!({ "address": "t2", "name": name, "state": "live", "status": "idle", "where": "Shop / tower" });
            assert!(seen_of(&t, None, None).is_none(), "{name}");
        }
        // A session in a tab that is only called like one (its name is a label) is a session: its
        // space isn't quiet while it runs, and a close waits for it.
        let row = json!({ "address": "t8", "agent": "claude-code", "state": "live", "status": "working", "where": "tower-pr / tower-pr-herder-tower" });
        let full = json!({ "address": "t8", "name": null, "state": "live", "status": { "app": "claude-code", "state": "working" }, "labels": { "where": "tower-pr / tower-pr-herder-tower", "tower.name": "tower-pr-herder-tower" } });
        let s = seen_of(&row, Some(&full), None).unwrap();
        assert_eq!((s.space.as_str(), s.name.as_str(), s.status.as_str()), ("tower-pr", "herder tower", "working"));
        // A test session is left alone.
        let test = json!({ "address": "t3", "state": "live", "status": "idle", "labels": { "where": "Shop / x", "tower.test": "1" } });
        assert!(seen_of(&row, Some(&test), None).is_none());
    }

    #[test]
    fn only_sessions_at_work_hold_a_space_open() {
        let t = |name: &str, status: &str, agent: bool| Target {
            address: format!("t-{name}"),
            name: name.into(),
            status: SessionStatus::parse(status),
            agent,
            own: crate::introduce::is_tower(name),
            brain: crate::brain::is_brain_name(name),
            given: None,
        };
        let list = [t("web", "working", true), t("api", "blocked", true), t("docs", "idle", true), t("sh", "", false), t("tower", "working", true), t("tower-shop", "working", true)];
        assert_eq!(busy(&list).iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["web", "api"], "not the brain that closes it, nor a plain program");
        assert!(busy(&list[2..]).is_empty());
        // Herdr closes a space whole (ending all in it) only when nothing of the tower's own is there.
        assert!(whole(&list[..4]) && !whole(&list), "the tower's brain would end with it");
        assert!(whole(&[t("tower-shop", "idle", true)]), "a space's own brain ends with its space");
        assert!(!whole(&[t("tower-voice", "", false)]));
    }

    #[test]
    fn the_brain_is_asked_for_the_closing_report_in_a_note() {
        let note = report_note("fix-login", "Fix the login redirect", "the PR is merged");
        assert!(crate::brain::is_note_prompt(&note), "a note turn: nothing it writes is shown or spoken");
        assert!(note.contains("tower space report \"fix-login\"") && note.contains("Fix the login redirect") && note.contains("Done when: \"the PR is merged\""), "{note}");
        assert!(note.contains("only the user closes a space"));
        assert!(!report_note("x", "y", " ").contains("Done when"));
        assert_eq!(crate::brain::note_session(&note), None, "about no one session");
    }

    #[test]
    fn missions_are_kept_with_the_spaces_and_an_older_file_still_loads() {
        let dir = worktrees::tests::temp("missions-file");
        let ui = dir.join("ui");
        // A spaces file from before missions.
        std::fs::create_dir_all(&ui).unwrap();
        std::fs::write(spaces::path_in(&ui), r#"{"focus":"Shop","agendas":{"Shop":"Ship it"},"read_ms":{"Shop":5},"follow":["Shop"]}"#).unwrap();
        let old = spaces::load_from(&ui);
        assert!(old.missions.is_empty() && !serde_json::to_string(&old).unwrap().contains("missions"));
        spaces::update_in(&ui, |s| {
            let m = s.missions.entry("Shop".into()).or_insert_with(|| Mission::born(7));
            (m.done_when, m.standing) = ("it is live".into(), true);
            m.worktrees.push(Place { repo: "/w/web".into(), path: "/w/.tower-worktrees/shop/web".into(), branch: "shop/web".into() });
        });
        let back = spaces::load_from(&ui);
        assert_eq!((back.focus.as_deref(), back.agendas["Shop"].as_str(), back.read_ms["Shop"]), (Some("Shop"), "Ship it", 5));
        let (name, m) = of(&back, "shop").unwrap();
        assert_eq!((name.as_str(), m.done_when.as_str(), m.standing, m.state, m.born_ms, m.worktrees.len()), ("Shop", "it is live", true, State::Open, 7, 1));
        assert_eq!(goal_of(&back, "Shop"), "Ship it");
        // What it writes for a mission that is only open: nothing it doesn't need.
        let text = std::fs::read_to_string(spaces::path_in(&ui)).unwrap();
        assert!(text.contains("\"state\": \"open\"") && !text.contains("report") && !text.contains("closed_ms"), "{text}");
        // A change that changes nothing writes nothing.
        let before = std::fs::metadata(spaces::path_in(&ui)).unwrap().modified().unwrap();
        spaces::update_in(&ui, |s| s.missions.get_mut("Shop").unwrap().standing = true);
        assert_eq!(std::fs::metadata(spaces::path_in(&ui)).unwrap().modified().unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_day_starts_at_local_midnight() {
        let now = 1_800_000_000_000u64;
        let start = day_start(now);
        assert!(start <= now && now - start < 25 * HOUR);
        assert_eq!(day_start(start), start);
        assert_eq!(day_start(start + HOUR), start);
    }
}
