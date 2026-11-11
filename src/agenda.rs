//! The agenda ledger: everything the user asked for, kept until it is done
//! or someone says to drop it, so nothing falls through the cracks.
//!
//! An **agenda** is one request of the user's, in their own words: one
//! prompt that holds several tasks, work spread over several sessions or
//! Herdr spaces. Its **tasks** are what the tower handed out for it
//! (`tower dispatch`, `tower queue`, `tower start` register them by
//! themselves), each tied to the send queue's ids, so the ledger always
//! knows which session took which instruction, and when.
//!
//! Each task has one state machine (`TaskState`, `TaskState::next`):
//!
//! | From | Event | To |
//! |---|---|---|
//! | queued, stalled | delivered (its instruction went in) | running |
//! | queued, running, blocked, stalled | the session works on it | running |
//! | any open | the session asks something (dialog, or a turn that ends asking) | blocked |
//! | any open | its turn ends with a message | done-claimed |
//! | done-claimed (or any open, by hand) | the message matches what was asked, or `tower agenda verify`, `tower job done` | verified |
//! | any but verified, abandoned | it says it couldn't, its turn failed, `tower agenda fail`, `tower job failed` | failed |
//! | any open | parked, expired, the session exited, vanished, errored, never took it, no progress | stalled |
//! | any | `tower agenda requeue`, a re-prompt, a follow-up for a session that waits | queued |
//! | any but verified | cancelled, `tower agenda abandon`, `close --force` | abandoned |
//!
//! Open: queued, running, blocked, done-claimed, stalled. Failed is not open
//! (the session stopped) but unfinished. Verified and abandoned are settled.
//! An agenda is `done` once every task is settled; `closed` and `abandoned`
//! are said by hand.
//!
//! Kept in `~/.local/state/herder-tower/agendas.json`, written whole through
//! a synced temporary file and a rename after every change: a crash leaves
//! the old ledger or the new one. The daemon holds it in memory and its
//! reconciler (`daemon::agendas`) moves the tasks; with no daemon running the
//! CLI reads and changes the file itself.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::hc::SessionStatus;

/// A request the user made this long ago still collects what the tower hands out for it.
pub const REQUEST_MS: u64 = 30 * 60_000;
/// Delivered, the session free again, and it never took it: sent once more after this long
/// (longer than the send queue's retry window, so it is a new hand-over).
pub const REPROMPT_MS: u64 = 12 * 60_000;
/// Working, with nothing new on its screen, no turn, no hook event, for this long: stalled.
pub const STALL_MS: u64 = 25 * 60_000;
/// A queue id the queue doesn't know, on a task that never went in, after this long: unaccounted for.
pub const UNKNOWN_MS: u64 = 10 * 60_000;
/// Taken, finished, with no final message from its hooks after this long: claimed done.
pub const NO_WORD_MS: u64 = 2 * 60_000;
/// How long a state must hold before the brain hears about it.
pub const GRACE_TROUBLE_MS: u64 = 45_000;
/// A claim the reconciler couldn't verify: the brain usually judges it from the watch report first.
pub const GRACE_CLAIM_MS: u64 = 3 * 60_000;
/// Waiting on the user this long: a reminder (the question itself was told when it came).
pub const GRACE_BLOCKED_MS: u64 = 30 * 60_000;
/// Unfinished work already told is told again after this long, at most `MAX_REMINDERS` times.
pub const REMIND_MS: u64 = 6 * 3_600_000;
pub const MAX_REMINDERS: u32 = 3;
/// Settled agendas are kept this long, then dropped (the newest few hundred stay regardless).
const KEEP_SETTLED_MS: u64 = 30 * 24 * 3_600_000;
/// A task that failed is sent once more by the tower itself (a quiet level) only this soon after
/// it failed: one that failed long ago (before a restart, under an older tower) is left as it is.
/// For this long after that retry, the brain's word that it failed is about the try before.
pub const AUTO_RETRY_MS: u64 = 10 * 60_000;
/// A finished request's receipt is asked for (`tower job done` says so) this long after it finished.
pub const RECEIPT_DUE_MS: u64 = 30 * 60_000;
const KEEP_AGENDAS: usize = 300;
const KEEP_STEPS: usize = 60;

pub fn path() -> PathBuf {
    crate::config::state_dir().join("agendas.json")
}

/// Where a task stands. On disk and on the wire: its kebab-case word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    /// Handed out, not gone in yet (it waits in the send queue).
    #[default]
    Queued,
    /// Its session is on it.
    Running,
    /// Its session waits for a person (`question` says for what).
    Blocked,
    /// Its session ended its turn saying it's done; not verified yet.
    DoneClaimed,
    /// Done, checked: its last message matches what it was asked, or the brain or the user said so.
    Verified,
    /// It didn't work out (the session said so, its turn failed, or someone said so).
    Failed,
    /// Not moving and not finished: parked, its session gone, never taken, no progress.
    Stalled,
    /// Dropped on purpose (cancelled, abandoned, closed with its agenda).
    Abandoned,
}

impl TaskState {
    pub fn word(&self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Running => "running",
            TaskState::Blocked => "blocked",
            TaskState::DoneClaimed => "done-claimed",
            TaskState::Verified => "verified",
            TaskState::Failed => "failed",
            TaskState::Stalled => "stalled",
            TaskState::Abandoned => "abandoned",
        }
    }

    /// Nothing more will come of it, and nothing is owed.
    pub fn settled(&self) -> bool {
        matches!(self, TaskState::Verified | TaskState::Abandoned)
    }

    /// The reconciler still follows it.
    pub fn open(&self) -> bool {
        matches!(self, TaskState::Queued | TaskState::Running | TaskState::Blocked | TaskState::DoneClaimed | TaskState::Stalled)
    }

    /// Needs someone to look at it.
    pub fn trouble(&self) -> bool {
        matches!(self, TaskState::Blocked | TaskState::DoneClaimed | TaskState::Failed | TaskState::Stalled)
    }

    /// Where `ev` takes a task in this state (see the module's table).
    pub fn next(&self, ev: &TaskEv) -> Result<TaskState, Refused> {
        use TaskState::*;
        let s = *self;
        match ev {
            TaskEv::Requeue { .. } => Ok(Queued),
            TaskEv::Abandon { .. } => match s {
                Abandoned => Err(Refused::Same),
                Verified => Err(Refused::Settled(s)),
                _ => Ok(Abandoned),
            },
            TaskEv::Verify { .. } => match s {
                Verified => Err(Refused::Same),
                Abandoned => Err(Refused::Settled(s)),
                _ => Ok(Verified),
            },
            TaskEv::Fail { .. } => match s {
                Verified | Abandoned => Err(Refused::Settled(s)),
                _ => Ok(Failed),
            },
            // What the session does moves only an open task.
            _ if !s.open() => Err(Refused::Settled(s)),
            TaskEv::Delivered { .. } => Ok(match s {
                Queued | Stalled => Running,
                other => other,
            }),
            TaskEv::Working => match s {
                Queued | Blocked | Stalled => Ok(Running),
                Running => Err(Refused::Same),
                // Its turn is over; working again is the next instruction's (a requeue first).
                _ => Err(Refused::NotNow(s)),
            },
            TaskEv::Blocked { .. } => Ok(Blocked),
            TaskEv::Claimed { .. } => Ok(DoneClaimed),
            TaskEv::Stall { .. } => Ok(Stalled),
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.word())
    }
}

/// What happens to a task.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskEv {
    /// Its instruction went in to `session` (typed, or a session started with it).
    Delivered { session: String, name: String, created_ms: Option<u64>, space: Option<String>, at_ms: u64, fresh: bool },
    /// The session works on it.
    Working,
    /// The session waits for a person.
    Blocked { question: String },
    /// The session ended its turn with this message.
    Claimed { said: String },
    /// Checked and done (`how`: by whom, on what grounds).
    Verify { how: String },
    Fail { why: String },
    Stall { why: String },
    /// A new try: this queue id carries it (`text`: a follow-up, when it isn't the instruction again).
    Requeue { queue_id: u64, text: String },
    Abandon { why: String },
}

/// A move the table doesn't allow.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    /// It is there already (with the same details).
    Same,
    /// It is verified, abandoned or failed: what its session does is no longer its.
    Settled(TaskState),
    /// Not from this state.
    NotNow(TaskState),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Same => f.write_str("it is there already"),
            Refused::Settled(s) => write!(f, "it is {s} already"),
            Refused::NotNow(s) => write!(f, "not while it is {s}"),
        }
    }
}

/// One move in a task's life.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub at_ms: u64,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Task {
    /// `a3.2`: its agenda's id and its place in it.
    pub id: String,
    /// Its id in a dispatch plan (`after` names these).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub plan_id: String,
    pub title: String,
    /// What the session was asked, whole.
    pub instruction: String,
    /// What it was told after (a follow-up, an answer to its question).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub followups: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub project: String,
    /// Its session's address (`t5`, `vps/t2`), once it went in.
    pub session: String,
    pub name: String,
    /// When that terminal was made: another terminal at the same address is another session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// The send queue's ids for it, oldest first (the last is the current try).
    pub queue_ids: Vec<u64>,
    /// The history's job ids for it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
    /// Task ids in its agenda it waits for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    pub state: TaskState,
    pub since_ms: u64,
    pub created_at_ms: u64,
    /// When the current try went in (0: not yet).
    pub delivered_ms: u64,
    /// It went in as a new session's first task.
    pub fresh: bool,
    /// What it waits for (blocked).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub question: String,
    /// What it said at the end of its turn (done-claimed and after).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub said: String,
    /// Why it is where it is (stalled, failed, abandoned, how it was verified).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// Sent again by the reconciler.
    pub reprompts: u32,
    /// Tried again after it failed: by the tower itself (once, see
    /// `Ledger::failures`) or with `tower agenda requeue`.
    #[serde(skip_serializing_if = "is_zero")]
    pub retries: u32,
    /// When it was last tried again after a failure (0: never).
    #[serde(skip_serializing_if = "is_zero_ms")]
    pub retried_ms: u64,
    /// It failed again after a retry, and that is in front of the user as a decision.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub raised: bool,
    /// The last sign of life: a hook event, a turn, its screen changing.
    pub progress_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    /// Its screen's last look (digits left out: timers move on their own), and when.
    pub screen: u64,
    pub screen_ms: u64,
    /// What the brain was told about it last (`state:why`), and when.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub told: String,
    pub told_ms: u64,
    pub history: Vec<Step>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_zero_ms(n: &u64) -> bool {
    *n == 0
}

impl Task {
    /// Moves it by `ev` (the table, plus its details). Ok(false): only details changed.
    pub fn apply(&mut self, ev: TaskEv, now: u64) -> Result<bool, Refused> {
        let to = self.state.next(&ev)?;
        let why = match ev {
            TaskEv::Delivered { session, name, created_ms, space, at_ms, fresh } => {
                let rebound = self.session != session || self.delivered_ms != at_ms;
                if !rebound && to == self.state {
                    return Err(Refused::Same);
                }
                let why = format!("went in to {} ({name})", session);
                self.session = session;
                self.name = name;
                self.created_ms = created_ms;
                if space.is_some() {
                    self.space = space;
                }
                self.delivered_ms = at_ms;
                self.fresh = fresh;
                self.progress_ms = now;
                why
            }
            TaskEv::Working => {
                self.progress_ms = now;
                "working on it".into()
            }
            TaskEv::Blocked { question } => {
                if self.state == TaskState::Blocked && self.question == question {
                    return Err(Refused::Same);
                }
                self.question = question.clone();
                if question.is_empty() { "waits for someone".into() } else { format!("asks: {}", clip(&question, 200)) }
            }
            TaskEv::Claimed { said } => {
                if self.state == TaskState::DoneClaimed && self.said == said {
                    return Err(Refused::Same);
                }
                self.said = said;
                self.question.clear();
                "ended its turn".into()
            }
            TaskEv::Verify { how } | TaskEv::Fail { why: how } | TaskEv::Stall { why: how } | TaskEv::Abandon { why: how } => {
                if to == self.state && self.why == how {
                    return Err(Refused::Same);
                }
                self.why = how.clone();
                how
            }
            TaskEv::Requeue { queue_id, text } => {
                if !self.queue_ids.contains(&queue_id) {
                    self.queue_ids.push(queue_id);
                }
                let text = text.trim().to_string();
                if !text.is_empty() && text != self.instruction && !self.followups.contains(&text) {
                    self.followups.push(text.clone());
                }
                // A try after it failed: counted, and what the user was asked about the failure is over.
                if self.state == TaskState::Failed {
                    self.retries += 1;
                    self.retried_ms = now;
                }
                self.raised = false;
                // The next try binds afresh, from its queue id.
                self.delivered_ms = 0;
                self.question.clear();
                self.why.clear();
                self.told.clear();
                if text.is_empty() { format!("again, as #{queue_id}") } else { format!("told more, as #{queue_id}: {}", clip(&text, 160)) }
            }
        };
        if to != self.state {
            self.state = to;
            self.since_ms = now;
        }
        self.history.push(Step { at_ms: now, state: self.state, why });
        if self.history.len() > KEEP_STEPS {
            let n = self.history.len() - KEEP_STEPS;
            self.history.drain(..n);
        }
        Ok(true)
    }

    /// The current try's queue id.
    pub fn queue_id(&self) -> Option<u64> {
        self.queue_ids.last().copied()
    }

    /// Everything it was asked, for judging what it said.
    fn asked(&self) -> Vec<&str> {
        let mut v = vec![self.instruction.as_str()];
        v.extend(self.followups.iter().map(String::as_str));
        v
    }

    fn who(&self) -> String {
        match (self.name.is_empty(), self.session.is_empty()) {
            (_, true) => "no session yet".into(),
            (true, false) => self.session.clone(),
            (false, false) => format!("{} ({})", self.name, self.session),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgendaState {
    #[default]
    Open,
    /// Every task settled, by itself.
    Done,
    /// Closed by hand.
    Closed,
    Abandoned,
}

impl AgendaState {
    pub fn word(&self) -> &'static str {
        match self {
            AgendaState::Open => "open",
            AgendaState::Done => "done",
            AgendaState::Closed => "closed",
            AgendaState::Abandoned => "abandoned",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Agenda {
    /// `a3`.
    pub id: String,
    /// The user's words, as they asked (or the instruction, for work the tower started itself).
    pub words: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// `user` (a request), `tower` (the brain handed it out on its own) or
    /// `terminal` (the user typed it in a session's own terminal).
    pub origin: String,
    /// When the request it came from was heard (0: none).
    pub request_ms: u64,
    pub spaces: Vec<String>,
    pub created_ms: u64,
    pub state: AgendaState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_ms: Option<u64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub tasks: Vec<Task>,
    /// The brain was told it's all done.
    pub told_done: bool,
    pub reminded: u32,
    pub reminded_ms: u64,
    /// When its receipt was given (`tower receipt`): one per request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_ms: Option<u64>,
}

impl Agenda {
    pub fn unfinished(&self) -> Vec<&Task> {
        self.tasks.iter().filter(|t| !t.state.settled()).collect()
    }

    pub fn live(&self) -> bool {
        matches!(self.state, AgendaState::Open)
    }

    /// Open ↔ done, as its tasks are: done once every one is settled.
    fn settle(&mut self, now: u64) {
        let all = !self.tasks.is_empty() && self.tasks.iter().all(|t| t.state.settled());
        match (self.state, all) {
            (AgendaState::Open, true) => {
                self.state = AgendaState::Done;
                self.closed_ms = Some(now);
            }
            (AgendaState::Done, false) => {
                self.state = AgendaState::Open;
                self.closed_ms = None;
                self.told_done = false;
            }
            _ => {}
        }
        let mut spaces: Vec<String> = self.spaces.clone();
        for t in &self.tasks {
            if let Some(s) = &t.space
                && !spaces.contains(s)
            {
                spaces.push(s.clone());
            }
        }
        self.spaces = spaces;
    }

    /// `2/3 verified`.
    pub fn tally(&self) -> String {
        let v = self.tasks.iter().filter(|t| t.state == TaskState::Verified).count();
        let a = self.tasks.iter().filter(|t| t.state == TaskState::Abandoned).count();
        let mut s = format!("{v}/{} verified", self.tasks.len());
        if a > 0 {
            s += &format!(", {a} abandoned");
        }
        s
    }

    /// The one space its tasks share, if they do (for the focus).
    fn space(&self) -> Option<String> {
        let set: BTreeSet<&String> = self.tasks.iter().filter_map(|t| t.space.as_ref()).collect();
        if set.len() == 1 { set.into_iter().next().cloned() } else { None }
    }
}

/// How a task registers: what the send queue did, and what the caller knows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tag {
    /// Join this agenda (`a3`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agenda: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub plan_id: String,
    /// Plan ids (in the same agenda) it waits for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// Not for the ledger (a start whose queue item already stands for it).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skip: bool,
}

/// A registration: a queue id, its session (empty: one still to start) and text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reg {
    pub queue_id: u64,
    pub session: String,
    pub name: String,
    pub text: String,
    pub tag: Tag,
}

/// The origin of the user's own work typed in a session's terminal (see `Ledger::typed`).
pub const TERMINAL: &str = "terminal";

/// A request the user typed in a session's own terminal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Typed {
    pub session: String,
    pub name: String,
    /// When that terminal was made.
    pub created_ms: Option<u64>,
    pub space: Option<String>,
    pub words: String,
    /// When its prompt hook said it went in.
    pub at_ms: u64,
}

/// The user's latest request, as the daemon heard it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Heard {
    pub text: String,
    pub at_ms: u64,
}

/// Where a registration went.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Registered {
    pub agenda: String,
    pub task: String,
    /// `new` (a task), `followup` (more for a task that waited), `same` (a retry).
    pub how: &'static str,
}

/// What the send queue knows about one of its ids.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum QueueFact {
    /// Still waiting (for its session, for what it waits for, or to start).
    Waiting { session: String, parked: Option<String>, start: bool },
    /// Being typed this moment.
    Claimed,
    /// It went in. `next_ms`: when the next instruction went in to that session (its turn was over by then).
    Delivered { session: String, at_ms: u64, turn_before: Option<u64>, taken: bool, finished: bool, next_ms: Option<u64> },
    Cancelled,
    /// Parked a week, then put away.
    Expired { why: String },
    /// Not known (never, or long forgotten).
    #[default]
    Unknown,
}

/// What Herdr says about a session now.
#[derive(Debug, Clone, PartialEq)]
pub enum Live {
    Here { status: SessionStatus, created_ms: Option<u64>, turn: Option<u64>, name: String, space: Option<String> },
    /// The terminal is there; its program ended.
    Exited,
    /// Herdr doesn't know it.
    Gone,
    Unreachable,
}

/// What a session's own hooks said (`daemon::sessions`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HookFact {
    pub state: SessionStatus,
    pub event: String,
    pub event_ms: u64,
    pub prompt_ms: u64,
    pub said: String,
    pub said_ms: u64,
    pub notice: String,
    pub started_ms: u64,
}

/// Everything the reconciler knows about one task now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Facts {
    pub queue: QueueFact,
    /// None: not looked at.
    pub live: Option<Live>,
    pub hook: Option<HookFact>,
    /// The question card open for its session.
    pub question: Option<String>,
    /// A fresh look at its screen (digits left out), when one was taken.
    pub screen: Option<u64>,
}

/// What the reconciler should do beyond moving a task.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send its instruction again (the session never took it).
    Reprompt { task: String, session: String, text: String },
}

/// What is done about a task that failed (`Ledger::failures`).
#[derive(Debug, Clone, PartialEq)]
pub enum FailedTask {
    /// The first time: its instruction goes to its session once more.
    Retry { task: String, session: String, text: String, why: String },
    /// Again, after a retry: the user's to decide (try once more, or drop it).
    Decide { task: String, title: String, session: String, name: String, space: Option<String>, why: String },
}

/// Why a task's instruction doesn't go to the terminal that has its
/// session's address now: it isn't the one the task was handed to (`given`:
/// when that one was made; `there`: when the one there now was). An address
/// comes back for the next terminal, and what the tower sends by itself must
/// never land in a stranger. None: the same one, or nothing says otherwise.
pub fn another_terminal(session: &str, given: Option<u64>, there: Option<u64>) -> Option<String> {
    match (given, there) {
        (Some(was), Some(now)) if was != now => Some(format!("{session} is another terminal now than the one the task was given to")),
        _ => None,
    }
}

/// A note for the brain.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Note {
    pub session: String,
    pub name: String,
    pub space: Option<String>,
    pub msg: String,
    pub speech: String,
}

/// A task the reconciler needs facts for.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub task: String,
    pub queue_id: Option<u64>,
    pub session: String,
    pub state: TaskState,
    /// Its screen is due a look (working a while with no other sign of life).
    pub look: bool,
}

/// What `judge` makes of a session's last message, against what it was asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// It says it's done, and talks about what it was asked.
    Done(String),
    /// It says it couldn't.
    Failed(String),
    /// It waits for the user.
    Asks(String),
    /// Can't tell: someone should look.
    Unclear(String),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ledger {
    pub next_id: u64,
    pub agendas: Vec<Agenda>,
}

static TMP: AtomicU64 = AtomicU64::new(0);

impl Ledger {
    /// The ledger in `path`. A file that doesn't parse is kept aside (never lost) and the ledger starts empty.
    pub fn load_from(path: &Path) -> Ledger {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                let aside = path.with_extension(format!("bad-{}.json", crate::history::now_ms()));
                let _ = std::fs::copy(path, &aside);
                eprintln!("[agenda] {} unreadable ({e}); kept as {}", path.display(), aside.display());
                Ledger::default()
            }),
            Err(_) => Ledger::default(),
        }
    }

    /// Written whole: a synced temporary file, then a rename. A crash leaves the old ledger or the new one.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), TMP.fetch_add(1, Ordering::Relaxed)));
        let written = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
            f.sync_all()
        })();
        if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, path)) {
            let _ = std::fs::remove_file(&tmp);
            bail!("saving {}: {e}", path.display());
        }
        Ok(())
    }

    pub fn agenda(&self, id: &str) -> Option<&Agenda> {
        self.agendas.iter().find(|a| a.id == id)
    }

    fn agenda_mut(&mut self, id: &str) -> Option<&mut Agenda> {
        self.agendas.iter_mut().find(|a| a.id == id)
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.agendas.iter().flat_map(|a| a.tasks.iter()).find(|t| t.id == id)
    }

    fn task_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.agendas.iter_mut().flat_map(|a| a.tasks.iter_mut()).find(|t| t.id == id)
    }

    fn agenda_of(&self, task: &str) -> Option<&Agenda> {
        self.agendas.iter().find(|a| a.tasks.iter().any(|t| t.id == task))
    }

    /// `a3`, `3`, `#3` (an agenda) or `a3.2` (a task): which it is.
    pub fn resolve(&self, target: &str) -> Result<Target> {
        let t = target.trim().trim_start_matches('#');
        let t = if t.chars().next().is_some_and(|c| c.is_ascii_digit()) { format!("a{t}") } else { t.to_string() };
        if self.task(&t).is_some() {
            return Ok(Target::Task(t));
        }
        if self.agenda(&t).is_some() {
            return Ok(Target::Agenda(t));
        }
        bail!("no agenda or task {target} (`tower agenda` lists them)")
    }

    /// The one receipt for a finished request (`target`: its agenda, or one
    /// of its tasks): refused while tasks are open, and a second time. Its
    /// agenda's id, the spaces it was about, and whether the user asked for
    /// it (not work the tower started on its own).
    pub fn receipt(&mut self, target: &str, now: u64) -> Result<(String, Vec<String>, bool)> {
        let id = match self.resolve(target)? {
            Target::Agenda(a) => a,
            Target::Task(t) => self.agenda_of(&t).map(|a| a.id.clone()).unwrap_or_default(),
        };
        let Some(a) = self.agenda_mut(&id) else { bail!("no agenda {target} (`tower agenda` lists them)") };
        let open: Vec<String> = a.unfinished().iter().map(|t| format!("{} \"{}\" is {}", t.id, t.title, t.state)).collect();
        if !open.is_empty() {
            bail!("{id} isn't finished: {}. Verify or drop what is open first; its receipt comes once, at the end.", open.join("; "));
        }
        if a.receipt_ms.is_some() {
            bail!("{id} has its receipt already: one per request");
        }
        a.receipt_ms = Some(now);
        let mut spaces = a.spaces.clone();
        for s in a.tasks.iter().filter_map(|t| t.space.clone()) {
            if !spaces.contains(&s) {
                spaces.push(s);
            }
        }
        Ok((id, spaces, a.origin != "tower"))
    }

    /// The request of the user's that `session` worked on, finished within
    /// the last `within` ms, whose receipt hasn't been given: the latest.
    pub fn receipt_due(&self, session: &str, now: u64, within: u64) -> Option<&Agenda> {
        self.agendas.iter().rev().find(|a| {
            a.state == AgendaState::Done
                && a.receipt_ms.is_none()
                && a.origin != "tower"
                && a.origin != TERMINAL
                && a.closed_ms.is_some_and(|c| now.saturating_sub(c) < within)
                && a.tasks.iter().any(|t| t.session == session)
        })
    }

    /// A new agenda for `words`. A request that has one already gets that one.
    pub fn open(&mut self, words: &str, title: &str, origin: &str, request_ms: u64, spaces: &[String], now: u64) -> String {
        if request_ms > 0
            && let Some(a) = self.agendas.iter().find(|a| a.request_ms == request_ms && a.live())
        {
            return a.id.clone();
        }
        self.next_id = self.next_id.max(self.agendas.iter().filter_map(|a| a.id.trim_start_matches('a').parse::<u64>().ok()).max().unwrap_or(0)) + 1;
        let id = format!("a{}", self.next_id);
        self.agendas.push(Agenda {
            id: id.clone(),
            words: words.trim().to_string(),
            title: title.trim().to_string(),
            origin: origin.into(),
            request_ms,
            spaces: spaces.to_vec(),
            created_ms: now,
            ..Default::default()
        });
        id
    }

    /// The task the send queue's id stands for.
    pub fn by_queue_id(&self, id: u64) -> Option<&Task> {
        self.agendas.iter().flat_map(|a| a.tasks.iter()).find(|t| t.queue_ids.contains(&id))
    }

    /// The task `session` is on, or was until `within` ms ago: one handed
    /// out and not through its turn yet (queued, running, blocked), or one
    /// whose turn just ended. None: nothing was asked of it lately (what
    /// the user typed in its terminal is their own conversation there).
    pub fn at_work_on(&self, session: &str, now: u64, within: u64) -> Option<&Task> {
        self.agendas
            .iter()
            .filter(|a| a.origin != TERMINAL)
            .flat_map(|a| a.tasks.iter())
            .filter(|t| !session.is_empty() && t.session == session)
            .filter(|t| matches!(t.state, TaskState::Queued | TaskState::Running | TaskState::Blocked) || now.saturating_sub(t.since_ms) < within)
            .max_by_key(|t| (t.delivered_ms, t.created_at_ms))
    }

    /// The tasks that stand failed and aren't dealt with. One that just
    /// failed for the first time is sent once more by the tower itself
    /// (`retry`: at a quiet level; while the tower tells everything the
    /// brain hears of it and asks the user, as before). One that failed
    /// again after a retry (the tower's, or `tower agenda requeue`) is the
    /// user's to decide, once.
    pub fn failures(&self, retry: bool, now: u64) -> Vec<FailedTask> {
        let mut out = vec![];
        for t in self.agendas.iter().filter(|a| a.live() && a.origin != TERMINAL).flat_map(|a| a.tasks.iter()).filter(|t| t.state == TaskState::Failed) {
            if t.retries == 0 {
                if retry && !t.session.is_empty() && now.saturating_sub(t.since_ms) <= AUTO_RETRY_MS {
                    out.push(FailedTask::Retry {
                        task: t.id.clone(),
                        session: t.session.clone(),
                        text: t.followups.last().cloned().unwrap_or_else(|| t.instruction.clone()),
                        why: t.why.clone(),
                    });
                }
            } else if !t.raised {
                out.push(FailedTask::Decide {
                    task: t.id.clone(),
                    title: t.title.clone(),
                    session: t.session.clone(),
                    name: t.name.clone(),
                    space: t.space.clone(),
                    why: t.why.clone(),
                });
            }
        }
        out
    }

    /// The task on `session` that failed a moment ago and is on its way
    /// again (tried once more, its new turn not over): word that "it failed"
    /// now is about the try before, which is dealt with.
    pub fn retrying(&self, session: &str, now: u64) -> Option<String> {
        self.unfinished_on(session)
            .filter(|t| matches!(t.state, TaskState::Queued | TaskState::Running) && t.retries > 0 && now.saturating_sub(t.retried_ms) <= AUTO_RETRY_MS)
            .map(|t| t.id.clone())
    }

    /// The tower's own retry of a failed task couldn't be sent (`why`): it
    /// counts as tried, so the failure goes to the user.
    pub fn retry_unsent(&mut self, task: &str, why: &str) {
        if let Some(t) = self.task_mut(task) {
            t.retries += 1;
            t.why = format!("{}; and it couldn't be sent again: {why}", t.why.trim_end_matches('.'));
        }
    }

    /// A task's second failure is in front of the user as a decision.
    /// `told`: the brain hears of it from that (it isn't told the failure again).
    pub fn decided(&mut self, task: &str, told: bool, now: u64) {
        if let Some(t) = self.task_mut(task) {
            t.raised = true;
            if told {
                t.told = format!("{}:{}", t.state.word(), t.why);
                t.told_ms = now;
            }
        }
    }

    /// The tasks whose second failure is in front of the user now.
    pub fn failed_twice(&self) -> Vec<String> {
        self.agendas.iter().filter(|a| a.live()).flat_map(|a| a.tasks.iter()).filter(|t| t.state == TaskState::Failed && t.raised).map(|t| t.id.clone()).collect()
    }

    /// The latest task on `session` that isn't settled, in an agenda still open.
    pub fn unfinished_on(&self, session: &str) -> Option<&Task> {
        self.agendas
            .iter()
            .filter(|a| a.live())
            .flat_map(|a| a.tasks.iter())
            .filter(|t| !session.is_empty() && t.session == session && !t.state.settled())
            .max_by_key(|t| (t.delivered_ms, t.created_at_ms))
    }

    /// `unfinished_on`, of what the tower handed out: the user's own work in
    /// a session's terminal (origin `terminal`) is theirs, and what the
    /// tower sends that session is never more of it.
    fn unfinished_handed(&self, session: &str) -> Option<&Task> {
        self.agendas
            .iter()
            .filter(|a| a.live() && a.origin != TERMINAL)
            .flat_map(|a| a.tasks.iter())
            .filter(|t| !session.is_empty() && t.session == session && !t.state.settled())
            .max_by_key(|t| (t.delivered_ms, t.created_at_ms))
    }

    /// The user typed `words` into `session`'s own terminal (its prompt hook
    /// said so, and the tower typed nothing of it): their request, worked
    /// there. A new agenda of origin `terminal` with one task, already
    /// running (it went in as the user typed it); while the last one typed
    /// in that terminal is unfinished, more of the same task instead (the
    /// user carries on there). The brain is never told of it (`news`), the
    /// tower never sends it again (`failures`), and its turn's end settles it
    /// (`step`): the user was there to see it.
    pub fn typed(&mut self, t: &Typed, now: u64) -> Registered {
        let words = t.words.trim().to_string();
        let open = self
            .agendas
            .iter()
            .filter(|a| a.live() && a.origin == TERMINAL)
            .flat_map(|a| a.tasks.iter())
            .filter(|k| k.session == t.session && !k.state.settled() && !matches!((k.created_ms, t.created_ms), (Some(a), Some(b)) if a != b))
            .max_by_key(|k| k.created_at_ms)
            .map(|k| k.id.clone());
        if let Some(id) = open {
            let agenda = self.agenda_of(&id).map(|a| a.id.clone()).unwrap_or_default();
            if let Some(k) = self.task_mut(&id) {
                if !words.is_empty() && words != k.instruction && !k.followups.contains(&words) {
                    k.followups.push(words.clone());
                }
                k.state = TaskState::Running;
                k.since_ms = now;
                k.delivered_ms = t.at_ms;
                k.progress_ms = now;
                k.said.clear();
                k.question.clear();
                k.why.clear();
                k.history.push(Step { at_ms: now, state: TaskState::Running, why: format!("told more in its terminal: {}", clip(&words, 160)) });
                if k.history.len() > KEEP_STEPS {
                    let n = k.history.len() - KEEP_STEPS;
                    k.history.drain(..n);
                }
            }
            if let Some(a) = self.agendas.iter_mut().find(|a| a.id == agenda) {
                a.settle(now);
            }
            return Registered { agenda, task: id, how: "followup" };
        }
        let spaces: Vec<String> = t.space.iter().cloned().collect();
        let agenda = self.open(&words, &title_of(&words), TERMINAL, t.at_ms, &spaces, now);
        let id = format!("{agenda}.1");
        if let Some(a) = self.agenda_mut(&agenda) {
            a.tasks.push(Task {
                id: id.clone(),
                title: title_of(&words),
                instruction: words.clone(),
                session: t.session.clone(),
                name: t.name.clone(),
                created_ms: t.created_ms,
                space: t.space.clone(),
                state: TaskState::Running,
                since_ms: now,
                created_at_ms: now,
                delivered_ms: t.at_ms,
                progress_ms: now,
                history: vec![Step { at_ms: now, state: TaskState::Running, why: format!("typed in its terminal ({})", t.session) }],
                ..Default::default()
            });
            a.settle(now);
        }
        Registered { agenda, task: id, how: "new" }
    }

    /// A task for what the send queue took on. Where it goes:
    /// 1. the same queue id again (a retry): the task it is already;
    /// 2. `tag.agenda`: that agenda (a plan's task with the same plan id there gets the id);
    /// 3. a session whose task waits (asked something; or claimed, stalled, failed with no
    ///    newer request from the user): more for that task;
    /// 4. the user's request of the last half hour: its agenda (opened now if it has none);
    /// 5. the session's open agenda;
    /// 6. a new agenda of the tower's own.
    pub fn register(&mut self, reg: &Reg, heard: &Heard, now: u64) -> Option<Registered> {
        if reg.tag.skip {
            return None;
        }
        if let Some(t) = self.by_queue_id(reg.queue_id) {
            let agenda = self.agenda_of(&t.id).map(|a| a.id.clone()).unwrap_or_default();
            return Some(Registered { agenda, task: t.id.clone(), how: "same" });
        }
        let recent = (heard.at_ms > 0 && now.saturating_sub(heard.at_ms) < REQUEST_MS && !heard.text.trim().is_empty()).then_some(heard);
        let named = reg.tag.agenda.as_deref().and_then(|a| self.resolve(a).ok()).and_then(|t| match t {
            Target::Agenda(a) => Some(a),
            Target::Task(t) => self.agenda_of(&t).map(|a| a.id.clone()),
        });
        if let Some(a) = &named
            && !reg.tag.plan_id.is_empty()
            && let Some(t) = self.agenda(a).and_then(|ag| ag.tasks.iter().find(|t| t.plan_id == reg.tag.plan_id)).map(|t| t.id.clone())
        {
            self.requeue(&t, reg.queue_id, &reg.text, now);
            return Some(Registered { agenda: a.clone(), task: t, how: "followup" });
        }
        if named.is_none()
            && let Some(t) = self.unfinished_handed(&reg.session)
        {
            let newer_request = recent.is_some_and(|h| h.at_ms > t.since_ms);
            let waits = t.state == TaskState::Blocked
                || (matches!(t.state, TaskState::DoneClaimed | TaskState::Stalled | TaskState::Failed) && !newer_request);
            if waits {
                let (id, agenda) = (t.id.clone(), self.agenda_of(&t.id).map(|a| a.id.clone()).unwrap_or_default());
                self.requeue(&id, reg.queue_id, &reg.text, now);
                return Some(Registered { agenda, task: id, how: "followup" });
            }
        }
        let agenda = match named {
            Some(a) => a,
            None => match recent {
                Some(h) => self.open(&h.text, "", "user", h.at_ms, &[], now),
                None => match self.unfinished_handed(&reg.session).and_then(|t| self.agenda_of(&t.id)).map(|a| a.id.clone()) {
                    Some(a) => a,
                    None => self.open(&reg.text, &title_of(&reg.text), "tower", 0, &[], now),
                },
            },
        };
        let a = self.agenda_mut(&agenda)?;
        let n = a.tasks.iter().filter_map(|t| t.id.rsplit('.').next()?.parse::<u64>().ok()).max().unwrap_or(0) + 1;
        let id = format!("{agenda}.{n}");
        let after: Vec<String> =
            reg.tag.after.iter().filter_map(|p| a.tasks.iter().find(|t| &t.plan_id == p || &t.id == p).map(|t| t.id.clone())).collect();
        let title = if reg.tag.title.trim().is_empty() { title_of(&reg.text) } else { reg.tag.title.trim().to_string() };
        a.tasks.push(Task {
            id: id.clone(),
            plan_id: reg.tag.plan_id.clone(),
            title,
            instruction: reg.text.trim().to_string(),
            project: reg.tag.project.clone(),
            session: reg.session.clone(),
            name: reg.name.clone(),
            space: reg.tag.space.clone(),
            queue_ids: vec![reg.queue_id],
            after,
            state: TaskState::Queued,
            since_ms: now,
            created_at_ms: now,
            progress_ms: now,
            history: vec![Step { at_ms: now, state: TaskState::Queued, why: format!("handed out as #{}", reg.queue_id) }],
            ..Default::default()
        });
        a.settle(now);
        Some(Registered { agenda, task: id, how: "new" })
    }

    /// A new try for a task, on queue id `queue_id` (`text`: what it was told this time).
    pub fn requeue(&mut self, task: &str, queue_id: u64, text: &str, now: u64) -> bool {
        let Some(t) = self.task_mut(task) else { return false };
        let ok = t.apply(TaskEv::Requeue { queue_id, text: text.into() }, now).is_ok();
        let a = self.agendas.iter_mut().find(|a| a.tasks.iter().any(|t| t.id == task));
        if let Some(a) = a {
            if a.state == AgendaState::Done || (ok && matches!(a.state, AgendaState::Closed | AgendaState::Abandoned)) {
                a.state = AgendaState::Open;
                a.closed_ms = None;
                a.told_done = false;
            }
            a.settle(now);
        }
        ok
    }

    /// Moves a task (by hand or by the reconciler) and settles its agenda.
    pub fn apply(&mut self, task: &str, ev: TaskEv, now: u64) -> Result<bool, Refused> {
        let a = self.agendas.iter_mut().find(|a| a.tasks.iter().any(|t| t.id == task)).ok_or(Refused::NotNow(TaskState::Queued))?;
        let t = a.tasks.iter_mut().find(|t| t.id == task).ok_or(Refused::NotNow(TaskState::Queued))?;
        let r = t.apply(ev, now);
        a.settle(now);
        r
    }

    /// `tower job done|failed <session>`: the brain's word on the session's latest unfinished task.
    pub fn judged(&mut self, session: &str, ok: bool, text: &str, now: u64) -> Option<String> {
        let id = self.unfinished_on(session)?.id.clone();
        let ev = if ok {
            TaskEv::Verify { how: if text.trim().is_empty() { "the brain checked it".into() } else { format!("the brain checked it: {}", clip(text, 200)) } }
        } else {
            TaskEv::Fail { why: if text.trim().is_empty() { "the brain says it didn't work out".into() } else { clip(text, 300) } }
        };
        self.apply(&id, ev, now).ok()?;
        Some(id)
    }

    /// Abandons a task, or every unsettled task of an agenda (and the agenda). Returns the
    /// tasks it dropped and the queue ids still waiting that should be cancelled.
    pub fn abandon(&mut self, target: &str, why: &str, now: u64) -> Result<(Vec<String>, Vec<u64>)> {
        let why = if why.trim().is_empty() { "abandoned by hand".to_string() } else { why.trim().to_string() };
        let (agenda, only) = match self.resolve(target)? {
            Target::Agenda(a) => (a, None),
            Target::Task(t) => (self.agenda_of(&t).map(|a| a.id.clone()).unwrap_or_default(), Some(t)),
        };
        let a = self.agenda_mut(&agenda).expect("resolved");
        let mut dropped = vec![];
        let mut cancel = vec![];
        for t in a.tasks.iter_mut().filter(|t| only.as_ref().is_none_or(|o| &t.id == o)) {
            if t.state == TaskState::Queued && t.delivered_ms == 0 {
                cancel.extend(t.queue_id());
            }
            if t.apply(TaskEv::Abandon { why: why.clone() }, now).is_ok() {
                dropped.push(t.id.clone());
            }
        }
        if only.is_none() {
            a.state = AgendaState::Abandoned;
            a.closed_ms = Some(now);
            a.note = why;
        } else {
            a.settle(now);
        }
        Ok((dropped, cancel))
    }

    /// Closes an agenda. With tasks unfinished it refuses (listing them) unless `force`:
    /// then they are abandoned, said so, and their waiting queue ids returned for cancelling.
    pub fn close(&mut self, target: &str, force: bool, note: &str, now: u64) -> Result<(Vec<String>, Vec<u64>)> {
        let agenda = match self.resolve(target)? {
            Target::Agenda(a) => a,
            Target::Task(t) => bail!("{t} is a task: `tower agenda verify {t}` or `abandon {t}`; close takes an agenda"),
        };
        let a = self.agenda_mut(&agenda).expect("resolved");
        let open: Vec<String> = a.unfinished().iter().map(|t| format!("{} {} ({})", t.id, t.title, t.state)).collect();
        if !open.is_empty() && !force {
            bail!("{agenda} has unfinished tasks: {}. Verify or abandon them, or close it with --force (they are abandoned)", open.join("; "));
        }
        let mut dropped = vec![];
        let mut cancel = vec![];
        for t in a.tasks.iter_mut().filter(|t| !t.state.settled()) {
            if t.state == TaskState::Queued && t.delivered_ms == 0 {
                cancel.extend(t.queue_id());
            }
            if t.apply(TaskEv::Abandon { why: "closed with its agenda, unfinished".into() }, now).is_ok() {
                dropped.push(t.id.clone());
            }
        }
        a.state = AgendaState::Closed;
        a.closed_ms = Some(now);
        a.told_done = true;
        if !note.trim().is_empty() {
            a.note = note.trim().into();
        }
        Ok((dropped, cancel))
    }

    /// The open tasks the reconciler needs facts for (`now`: to see whose screen is due a look).
    pub fn probes(&self, now: u64) -> Vec<Probe> {
        self.agendas
            .iter()
            .filter(|a| a.live())
            .flat_map(|a| a.tasks.iter())
            .filter(|t| t.state.open())
            .map(|t| Probe {
                task: t.id.clone(),
                queue_id: t.queue_id(),
                session: t.session.clone(),
                state: t.state,
                look: t.state == TaskState::Running
                    && now.saturating_sub(t.progress_ms) >= 5 * 60_000
                    && now.saturating_sub(t.screen_ms) >= 2 * 60_000,
            })
            .collect()
    }

    /// One round: each probed task's facts move it; returns what else to do.
    pub fn step(&mut self, facts: &BTreeMap<String, Facts>, now: u64) -> Vec<Action> {
        let mut actions = vec![];
        for a in self.agendas.iter_mut().filter(|a| a.live()) {
            let typed = a.origin == TERMINAL;
            for t in a.tasks.iter_mut().filter(|t| t.state.open()) {
                let Some(f) = facts.get(&t.id) else { continue };
                notice_progress(t, f, now);
                for act in assess(t, f, now) {
                    match act {
                        Assessed::Ev(ev) => {
                            let _ = t.apply(ev, now);
                        }
                        // What the user typed there is theirs: the tower never types it again.
                        Assessed::Reprompt if typed => {}
                        Assessed::Reprompt => actions.push(Action::Reprompt {
                            task: t.id.clone(),
                            session: t.session.clone(),
                            text: t.followups.last().cloned().unwrap_or_else(|| t.instruction.clone()),
                        }),
                    }
                }
                // Its turn ended, with the user there to see it: done.
                if typed && t.state == TaskState::DoneClaimed {
                    let _ = t.apply(TaskEv::Verify { how: "its turn ended; the user worked it in its terminal".into() }, now);
                }
            }
            a.settle(now);
        }
        actions
    }

    /// What the brain should hear now: tasks that went wrong (once each, after a grace),
    /// agendas all done (two tasks or more: one finished session is its watch's news),
    /// and reminders of what is still unfinished. Marks them told.
    pub fn news(&mut self, now: u64) -> Vec<Note> {
        let mut out = vec![];
        for a in self.agendas.iter_mut() {
            // The user's own work in a terminal: the brain hears nothing of it.
            if a.origin == TERMINAL {
                a.told_done |= a.state == AgendaState::Done;
                continue;
            }
            if a.state == AgendaState::Done && !a.told_done {
                a.told_done = true;
                if a.tasks.len() >= 2 {
                    let titles: Vec<String> = a.tasks.iter().map(|t| format!("{} ({})", t.title, t.state)).collect();
                    out.push(Note {
                        space: a.space(),
                        msg: format!(
                            "[agenda] {} is done: every task is settled ({}): {}. The user asked: \"{}\". Tell them in one line that it's all done.",
                            a.id,
                            a.tally(),
                            titles.join("; "),
                            clip(&a.words, 200)
                        ),
                        speech: format!("Everything for {} is done.", clip(&a.title_or_words(), 60)),
                        ..Default::default()
                    });
                }
                continue;
            }
            if !a.live() {
                continue;
            }
            let mut lines = vec![];
            let mut first: Option<&Task> = None;
            for t in a.tasks.iter_mut().filter(|t| t.state.trouble()) {
                let key = format!("{}:{}", t.state.word(), if t.state == TaskState::Blocked { &t.question } else { &t.why });
                let grace = match t.state {
                    TaskState::Blocked => GRACE_BLOCKED_MS,
                    TaskState::DoneClaimed => GRACE_CLAIM_MS,
                    _ => GRACE_TROUBLE_MS,
                };
                if t.told == key || now.saturating_sub(t.since_ms) < grace {
                    continue;
                }
                t.told = key;
                t.told_ms = now;
                lines.push(trouble_line(t));
            }
            for t in a.tasks.iter().filter(|t| t.state.trouble()) {
                if first.is_none() {
                    first = Some(t);
                }
            }
            if !lines.is_empty() {
                let f = first.cloned().unwrap_or_default();
                out.push(Note {
                    session: f.session.clone(),
                    name: f.name.clone(),
                    space: if lines.len() == 1 { f.space.clone() } else { a.space() },
                    msg: format!(
                        "[agenda] {} (the user asked: \"{}\"; {}): {} Tell the user in a line or two, unless you are handling it.",
                        a.id,
                        clip(&a.words, 160),
                        a.tally(),
                        lines.join(" ")
                    ),
                    speech: format!("Something from your request {} needs a look.", clip(&a.title_or_words(), 60)),
                });
                a.reminded_ms = now;
                continue;
            }
            // Told already and still unfinished: a reminder now and then.
            let stuck: Vec<&Task> = a.tasks.iter().filter(|t| t.state.trouble() && !t.told.is_empty()).collect();
            let last = a.reminded_ms.max(stuck.iter().map(|t| t.told_ms).max().unwrap_or(0));
            if !stuck.is_empty() && a.reminded < MAX_REMINDERS && now.saturating_sub(last) >= REMIND_MS {
                let list: Vec<String> = stuck.iter().map(|t| format!("{} \"{}\" on {} is {}", t.id, t.title, t.who(), t.state)).collect();
                let s = stuck[0];
                out.push(Note {
                    session: s.session.clone(),
                    name: s.name.clone(),
                    space: a.space(),
                    msg: format!(
                        "[agenda] still unfinished from earlier: {} (the user asked: \"{}\"): {}. `tower agenda show {}` has the details. Remind the user in one line and ask whether to retry or drop it.",
                        a.id,
                        clip(&a.words, 160),
                        list.join("; "),
                        a.id
                    ),
                    speech: format!("Still unfinished: {}.", clip(&a.title_or_words(), 60)),
                });
                a.reminded += 1;
                a.reminded_ms = now;
            }
        }
        out
    }

    /// Settled agendas older than a month go, keeping the newest few hundred overall.
    pub fn prune(&mut self, now: u64) -> usize {
        let before = self.agendas.len();
        let old = |a: &Agenda| !a.live() && a.closed_ms.is_some_and(|c| now.saturating_sub(c) > KEEP_SETTLED_MS);
        self.agendas.retain(|a| !old(a));
        while self.agendas.len() > KEEP_AGENDAS {
            match self.agendas.iter().position(|a| !a.live()) {
                Some(i) => {
                    self.agendas.remove(i);
                }
                None => break,
            }
        }
        before - self.agendas.len()
    }

    /// Open agendas, oldest first.
    pub fn open_agendas(&self) -> Vec<&Agenda> {
        self.agendas.iter().filter(|a| a.live()).collect()
    }

    /// The requests about one space, for its mission: those still open and
    /// those finished since `since_ms`, each with its tasks settled out of
    /// all of them; and how many of each.
    pub fn requests_of(&self, space: &str, since_ms: u64) -> Requests {
        let about = |a: &Agenda| {
            a.spaces.iter().chain(a.tasks.iter().filter_map(|t| t.space.as_ref())).any(|s| s.eq_ignore_ascii_case(space.trim()))
        };
        let mut out = Requests::default();
        let row = |a: &Agenda| {
            serde_json::json!({
                "id": a.id,
                "title": clip(&a.title_or_words(), 80),
                "state": a.state.word(),
                "done": a.tasks.iter().filter(|t| t.state.settled()).count(),
                "total": a.tasks.len(),
            })
        };
        for a in self.agendas.iter().filter(|a| about(a) && a.live()) {
            out.open += 1;
            out.rows.push(row(a));
        }
        for a in self.agendas.iter().rev().filter(|a| about(a) && !a.live() && a.closed_ms.is_some_and(|c| c >= since_ms)) {
            out.finished += 1;
            out.rows.push(row(a));
        }
        out
    }

    /// The open requests that are about `space` and no other: closed with it.
    pub fn only_in(&self, space: &str) -> Vec<String> {
        self.agendas
            .iter()
            .filter(|a| a.live())
            .filter(|a| {
                let mut spaces = a.spaces.iter().chain(a.tasks.iter().filter_map(|t| t.space.as_ref())).peekable();
                spaces.peek().is_some() && spaces.all(|s| s.eq_ignore_ascii_case(space.trim()))
            })
            .map(|a| a.id.clone())
            .collect()
    }

    /// One short line for the brain with each request: what is still open.
    pub fn radar_note(&self) -> Option<String> {
        let open = self.open_agendas();
        if open.is_empty() {
            return None;
        }
        let mut parts = vec![];
        for a in open.iter().rev().take(3) {
            let trouble: Vec<String> = a.tasks.iter().filter(|t| t.state.trouble()).map(|t| format!("{} {}", t.id, t.state)).collect();
            let running = a.tasks.iter().filter(|t| matches!(t.state, TaskState::Queued | TaskState::Running)).count();
            let mut s = format!("{} \"{}\" {}", a.id, clip(&a.title_or_words(), 50), a.tally());
            if running > 0 {
                s += &format!(", {running} in progress");
            }
            if !trouble.is_empty() {
                s += &format!(" ({})", trouble.join(", "));
            }
            parts.push(s);
        }
        let more = if open.len() > 3 { format!("; +{} more", open.len() - 3) } else { String::new() };
        Some(format!("[agendas] unfinished: {}{more}. `tower agenda` has them.", parts.join("; ")))
    }
}

impl Agenda {
    fn title_or_words(&self) -> String {
        if self.title.is_empty() { self.words.clone() } else { self.title.clone() }
    }
}

impl Ledger {
    /// The reconciler sent a task's instruction again, as `queue_id`.
    pub fn reprompted(&mut self, task: &str, queue_id: u64, now: u64) {
        self.requeue(task, queue_id, "", now);
        if let Some(t) = self.task_mut(task) {
            t.reprompts += 1;
        }
    }

    /// The history's jobs (`session`, id, started) that belong to tasks that went in
    /// lately and have none yet. True when one was added.
    pub fn attach_jobs(&mut self, jobs: &[(String, String, u64)]) -> bool {
        let mut changed = false;
        for t in self.agendas.iter_mut().filter(|a| a.live()).flat_map(|a| a.tasks.iter_mut()) {
            if t.session.is_empty() || t.delivered_ms == 0 {
                continue;
            }
            for (s, id, started) in jobs {
                if *s == t.session && *started + 10_000 >= t.delivered_ms && !t.jobs.contains(id) {
                    t.jobs.push(id.clone());
                    changed = true;
                }
            }
        }
        changed
    }

    /// Tasks that went in in the last `within` ms and have no job yet: the history is worth a look.
    pub fn wants_jobs(&self, now: u64, within: u64) -> bool {
        self.agendas
            .iter()
            .filter(|a| a.live())
            .flat_map(|a| a.tasks.iter())
            .any(|t| t.jobs.is_empty() && t.delivered_ms > 0 && now.saturating_sub(t.delivered_ms) < within)
    }
}

/// A `tower agenda` command, done to a ledger (the daemon's, or the file's when no daemon runs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cmd {
    pub action: String,
    pub target: String,
    pub text: String,
    pub session: String,
    pub title: String,
    pub spaces: Vec<String>,
    pub force: bool,
    pub all: bool,
    pub ok: bool,
}

/// What a command did: its answer, the queue ids to cancel, whether the ledger changed.
pub struct Acted {
    pub value: serde_json::Value,
    pub cancel: Vec<u64>,
    pub changed: bool,
}

/// Every command but `requeue` (which needs the send queue).
pub fn act(l: &mut Ledger, c: &Cmd, heard: &Heard, now: u64) -> Result<Acted> {
    use serde_json::json;
    let read = |value| Ok(Acted { value, cancel: vec![], changed: false });
    match c.action.as_str() {
        "list" | "" => {
            let list: Vec<&Agenda> = l.agendas.iter().filter(|a| c.all || a.live()).collect();
            read(json!({ "agendas": list }))
        }
        "tasks" => {
            let rows: Vec<serde_json::Value> = l
                .open_agendas()
                .iter()
                .flat_map(|a| a.unfinished().into_iter().map(move |t| json!({ "agenda": a.id, "words": a.words, "task": t })))
                .collect();
            read(json!({ "tasks": rows }))
        }
        "show" => {
            let (agenda, task) = match l.resolve(&c.target)? {
                Target::Agenda(a) => (a, None),
                Target::Task(t) => (l.agenda_of(&t).map(|a| a.id.clone()).unwrap_or_default(), Some(t)),
            };
            read(json!({ "agenda": l.agenda(&agenda), "task": task }))
        }
        "open" | "new" => {
            let recent = heard.at_ms > 0 && now.saturating_sub(heard.at_ms) < REQUEST_MS && !heard.text.trim().is_empty();
            let (words, request_ms, origin) = match (c.text.trim(), recent) {
                ("", true) => (heard.text.clone(), heard.at_ms, "user"),
                ("", false) => (c.title.clone(), 0, "tower"),
                (w, true) => (w.to_string(), heard.at_ms, "user"),
                (w, false) => (w.to_string(), 0, "user"),
            };
            if words.trim().is_empty() {
                bail!("an agenda needs the user's words (or a title)");
            }
            let id = l.open(&words, &c.title, origin, request_ms, &c.spaces, now);
            Ok(Acted { value: json!({ "id": id, "words": l.agenda(&id).map(|a| a.words.clone()) }), cancel: vec![], changed: true })
        }
        "close" => {
            let (abandoned, cancel) = l.close(&c.target, c.force, &c.text, now)?;
            let id = l.resolve(&c.target).map(|t| match t {
                Target::Agenda(a) | Target::Task(a) => a,
            })?;
            Ok(Acted { value: json!({ "closed": id, "abandoned": abandoned, "cancelled": cancel }), cancel, changed: true })
        }
        "abandon" => {
            let (abandoned, cancel) = l.abandon(&c.target, &c.text, now)?;
            Ok(Acted { value: json!({ "abandoned": abandoned, "cancelled": cancel }), cancel, changed: true })
        }
        "verify" | "fail" => {
            let tasks: Vec<String> = match l.resolve(&c.target)? {
                Target::Task(t) => vec![t],
                Target::Agenda(a) => l.agenda(&a).map(|a| a.unfinished().iter().map(|t| t.id.clone()).collect()).unwrap_or_default(),
            };
            let words = c.text.trim();
            let mut moved = vec![];
            for t in &tasks {
                let ev = if c.action == "verify" {
                    TaskEv::Verify { how: if words.is_empty() { "checked by hand".into() } else { format!("checked by hand: {}", clip(words, 300)) } }
                } else {
                    TaskEv::Fail { why: if words.is_empty() { "failed, by hand".into() } else { clip(words, 300) } }
                };
                match l.apply(t, ev, now) {
                    Ok(_) => moved.push(t.clone()),
                    Err(e) if tasks.len() == 1 => bail!("{t}: {e}"),
                    Err(_) => {}
                }
            }
            Ok(Acted { value: json!({ "action": c.action, "moved": moved }), cancel: vec![], changed: !moved.is_empty() })
        }
        "judge" => {
            // "It failed", about a task the tower just sent once more: that is the try before.
            let retried = (!c.ok).then(|| l.retrying(&c.session, now)).flatten();
            if retried.is_some() {
                return Ok(Acted { value: json!({ "task": null, "retried": retried, "agenda_done": null }), cancel: vec![], changed: false });
            }
            let task = l.judged(&c.session, c.ok, &c.text, now);
            // The user's request this session worked on, when it is finished
            // now (by this, or checked by the tower itself a moment ago) and
            // its receipt is still to come.
            let done = l.receipt_due(&c.session, now, RECEIPT_DUE_MS).map(|a| a.id.clone());
            Ok(Acted { value: json!({ "task": task, "agenda_done": done }), cancel: vec![], changed: task.is_some() })
        }
        // `tower receipt --agenda <id>`: marks the request as receipted.
        "receipt" => {
            let (id, spaces, asked) = l.receipt(&c.target, now)?;
            Ok(Acted { value: json!({ "agenda": id, "spaces": spaces, "asked": asked }), cancel: vec![], changed: true })
        }
        other => bail!("tower agenda {other}: use list, tasks, show, new, close, abandon, verify, fail or requeue"),
    }
}

/// A space's requests (`Ledger::requests_of`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Requests {
    /// `{id, title, state, done, total}`: the open ones, oldest first, then those finished lately, newest first.
    pub rows: Vec<serde_json::Value>,
    /// Still open.
    pub open: usize,
    /// Done, closed or dropped since then.
    pub finished: usize,
}

/// What `resolve` found.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Agenda(String),
    Task(String),
}

/// Signs of life: a hook event, a new turn, its screen changing.
fn notice_progress(t: &mut Task, f: &Facts, now: u64) {
    if let Some(h) = &f.hook
        && h.event_ms > t.progress_ms
        && h.event_ms >= t.delivered_ms
    {
        t.progress_ms = h.event_ms;
    }
    if let Some(Live::Here { turn: Some(n), .. }) = &f.live {
        if t.turn.is_some_and(|was| was != *n) {
            t.progress_ms = now;
        }
        t.turn = Some(*n);
    }
    if let Some(h) = f.screen {
        if t.screen != 0 && t.screen != h {
            t.progress_ms = now;
        }
        t.screen = h;
        t.screen_ms = now;
    }
}

enum Assessed {
    Ev(TaskEv),
    Reprompt,
}

/// What the facts say happened to an open task (see the module's table).
fn assess(t: &Task, f: &Facts, now: u64) -> Vec<Assessed> {
    let stall = |why: String| -> Vec<Assessed> {
        if t.state == TaskState::Stalled && t.why == why { vec![] } else { vec![Assessed::Ev(TaskEv::Stall { why })] }
    };
    let mut out = vec![];
    // Where its current try is, as the send queue says.
    let (session, since, next_ms, taken_q, finished_q, turn_known) = match &f.queue {
        QueueFact::Cancelled => return vec![Assessed::Ev(TaskEv::Abandon { why: "its queued instruction was cancelled".into() })],
        QueueFact::Expired { why } => return stall(format!("its instruction sat parked a week ({why}) and was put away; it never went in")),
        QueueFact::Waiting { parked: Some(why), session, .. } => return stall(parked_why(why, session)),
        QueueFact::Waiting { .. } | QueueFact::Claimed => {
            if t.state == TaskState::Stalled {
                // Retargeted, or its session is back: waiting again.
                return vec![Assessed::Ev(TaskEv::Requeue { queue_id: t.queue_id().unwrap_or(0), text: String::new() })];
            }
            if let Some(Live::Here { status: SessionStatus::Error, .. }) = &f.live {
                return stall("its session is in error; the instruction waits for it to be idle".into());
            }
            return vec![];
        }
        QueueFact::Delivered { session, at_ms, turn_before, taken, finished, next_ms } => {
            if t.session != *session || t.delivered_ms != *at_ms {
                let (name, created_ms, space) = match &f.live {
                    Some(Live::Here { name, created_ms, space, .. }) => (name.clone(), *created_ms, space.clone()),
                    _ => (t.name.clone(), t.created_ms, t.space.clone()),
                };
                let fresh = t.fresh || *turn_before == Some(0);
                out.push(Assessed::Ev(TaskEv::Delivered { session: session.clone(), name, created_ms, space, at_ms: *at_ms, fresh }));
            }
            (session.clone(), *at_ms, *next_ms, *taken, *finished, turn_before.is_some())
        }
        QueueFact::Unknown => {
            if t.delivered_ms == 0 {
                if now.saturating_sub(t.created_at_ms) >= UNKNOWN_MS && t.state == TaskState::Queued {
                    return stall("the send queue has no trace of its instruction: unaccounted for".into());
                }
                return vec![];
            }
            (t.session.clone(), t.delivered_ms, None, false, false, false)
        }
    };
    let fresh = t.fresh || matches!(&f.queue, QueueFact::Delivered { turn_before: Some(0), .. });
    // A new session's first turn may end before its start was written down.
    let from = if fresh { since.saturating_sub(60_000) } else { since };
    let until = next_ms.unwrap_or(u64::MAX);
    let hook = f.hook.as_ref();
    let claim = hook.filter(|h| !h.said.trim().is_empty() && h.said_ms >= from && h.said_ms < until).map(|h| h.said.clone());
    let failed_turn = hook.filter(|h| h.event == "stop-failed" && h.event_ms >= from && h.event_ms < until);
    let taken = taken_q || hook.is_some_and(|h| h.prompt_ms >= from || h.said_ms >= from);
    let replaced = matches!(&f.live, Some(Live::Here { created_ms: Some(c), .. }) if t.created_ms.is_some_and(|was| was != *c));
    // Its turn ended with a message: what that says decides.
    let judged = |out: &mut Vec<Assessed>, said: String| {
        if t.said == said && matches!(t.state, TaskState::DoneClaimed | TaskState::Blocked) {
            return;
        }
        if failed_turn.is_some() {
            out.push(Assessed::Ev(TaskEv::Fail { why: format!("its turn failed: {}", clip(&said, 300)) }));
            return;
        }
        match judge(&t.asked(), &said) {
            Verdict::Asks(q) => out.push(Assessed::Ev(TaskEv::Blocked { question: q })),
            Verdict::Failed(why) => out.push(Assessed::Ev(TaskEv::Fail { why: format!("it says it couldn't: {why}") })),
            Verdict::Done(how) => {
                out.push(Assessed::Ev(TaskEv::Claimed { said }));
                out.push(Assessed::Ev(TaskEv::Verify { how }));
            }
            Verdict::Unclear(_) => out.push(Assessed::Ev(TaskEv::Claimed { said })),
        }
    };
    // Another instruction went in to its session after it: its turn was over by then.
    if next_ms.is_some() && claim.is_none() && t.state.open() && t.state != TaskState::DoneClaimed {
        out.push(Assessed::Ev(TaskEv::Claimed {
            said: "(its turn ended before the next instruction went in; its last words weren't kept)".into(),
        }));
        return out;
    }
    match &f.live {
        Some(Live::Here { .. }) if replaced => match claim {
            Some(said) => judged(&mut out, said),
            None => out.extend(stall(format!("its session vanished: {session} is another terminal now"))),
        },
        Some(Live::Here { status, .. }) => match status {
            SessionStatus::Blocked => {
                let q = f.question.clone().or_else(|| hook.map(|h| h.notice.clone()).filter(|n| !n.is_empty())).unwrap_or_default();
                if t.state != TaskState::Blocked || (!q.is_empty() && q != t.question) {
                    out.push(Assessed::Ev(TaskEv::Blocked { question: q }));
                }
            }
            SessionStatus::Working => {
                if let Some(said) = claim.filter(|_| hook.is_some_and(|h| h.state.ready() && h.said_ms >= h.prompt_ms)) {
                    // The hook says the turn is over before Herdr does.
                    judged(&mut out, said);
                } else if matches!(t.state, TaskState::Queued | TaskState::Blocked | TaskState::Stalled)
                    && !(t.state == TaskState::Stalled && t.why.starts_with("no sign of progress") && now.saturating_sub(t.progress_ms) >= STALL_MS)
                {
                    out.push(Assessed::Ev(TaskEv::Working));
                } else if t.state == TaskState::Running && now.saturating_sub(t.progress_ms.max(since)) >= STALL_MS {
                    out.extend(stall(format!("no sign of progress for {} min while it says it's working", STALL_MS / 60_000)));
                }
            }
            SessionStatus::Idle | SessionStatus::Done => match claim {
                Some(said) => judged(&mut out, said),
                None if !taken && (turn_known || hook.is_some()) && now.saturating_sub(since) >= REPROMPT_MS => {
                    if t.reprompts == 0 {
                        out.push(Assessed::Reprompt);
                    } else {
                        out.extend(stall("it never took the instruction, even when sent again".into()));
                    }
                }
                None if (taken || finished_q) && now.saturating_sub(t.progress_ms.max(since)) >= NO_WORD_MS && t.state != TaskState::DoneClaimed => {
                    out.push(Assessed::Ev(TaskEv::Claimed { said: "(it finished its turn without a final message the tower could read)".into() }));
                }
                None => {}
            },
            SessionStatus::Error => match claim {
                Some(said) => judged(&mut out, said),
                None => out.extend(stall("its session hit an error".into())),
            },
            _ => {}
        },
        Some(Live::Exited) | Some(Live::Gone) => match claim {
            // Its last words settle it if they can; a claim nobody could check, from a session
            // that is gone now, will never be checked: stalled.
            Some(said) => match judge(&t.asked(), &said) {
                Verdict::Unclear(why) if failed_turn.is_none() => {
                    out.extend(stall(format!("its session ended, and its last words didn't show it was done ({why})")))
                }
                _ => judged(&mut out, said),
            },
            None => out.extend(stall(if f.live == Some(Live::Exited) {
                "its session exited before it finished".into()
            } else {
                "its session vanished before it finished".into()
            })),
        },
        Some(Live::Unreachable) | None => {
            if let Some(said) = claim {
                judged(&mut out, said);
            }
        }
    }
    out
}

fn parked_why(why: &str, session: &str) -> String {
    match why {
        "gone" => format!("its session {session} ended before the instruction went in (it's parked in the queue)"),
        "replaced" => format!("{session} is another terminal now; the instruction is parked in the queue"),
        "after-cancelled" => "what it waited for was cancelled; it's parked in the queue".into(),
        "start-failed" => "its session couldn't be started; it's parked in the queue".into(),
        other => format!("parked in the queue ({other})"),
    }
}

/// One sentence for a task in trouble, with what to do about it.
fn trouble_line(t: &Task) -> String {
    let what = format!("{} \"{}\" on {}", t.id, t.title, t.who());
    let again = format!(
        "`tower agenda requeue {} [<session>]` sends it again (to that session or another), `tower agenda abandon {}` drops it; ask the user which unless it's clear.",
        t.id, t.id
    );
    match t.state {
        TaskState::Stalled => format!("{what} is stalled: {}. It was asked: \"{}\". {again}", t.why, clip(&t.instruction, 200)),
        TaskState::Failed => format!("{what} failed: {}. {again}", clip(&t.why, 300)),
        TaskState::DoneClaimed => format!(
            "{what} ended its turn saying \"{}\", which doesn't clearly show it did what it was asked (\"{}\"). Check it (hc read {}), then `tower agenda verify {}` or `tower agenda fail {} \"<why>\"`; tell the user only if it didn't do it.",
            clip(&t.said, 300),
            clip(&t.instruction, 160),
            t.session,
            t.id,
            t.id
        ),
        TaskState::Blocked => format!(
            "{what} has waited for the user for {} min: \"{}\". Remind them in one line.",
            GRACE_BLOCKED_MS / 60_000,
            clip(if t.question.is_empty() { "it needs someone" } else { &t.question }, 240)
        ),
        _ => what,
    }
}

/// A few words for a task with no title.
pub fn title_of(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().take(8).collect();
    let mut s = words.join(" ");
    if text.split_whitespace().count() > 8 {
        s.push('…');
    }
    s
}

/// At most `n` characters, on one line.
pub fn clip(s: &str, n: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ").replace('"', "'");
    if flat.chars().count() <= n { flat } else { format!("{}…", flat.chars().take(n).collect::<String>().trim_end()) }
}

/// A screen's fingerprint, without the digits (timers and token counts move by themselves).
pub fn screen_hash(screen: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    screen.chars().filter(|c| !c.is_ascii_digit()).collect::<String>().hash(&mut h);
    h.finish().max(1)
}

const FAIL: &[&str] = &[
    "i couldn't",
    "i could not",
    "i can't",
    "i cannot",
    "i was unable",
    "i'm unable",
    "i am unable",
    "unable to",
    "failed to",
    "wasn't able to",
    "was not able to",
    "didn't work",
    "did not work",
    "not possible to",
    "gave up",
    "giving up",
    "i'm stuck",
    "i am stuck",
    "permission denied",
    "no access to",
];

const DONE: &[&str] = &[
    "done", "fixed", "finished", "complete", "completed", "implemented", "added", "created", "updated", "committed", "merged", "passes",
    "passing", "passed", "wrote", "written", "removed", "renamed", "refactored", "deployed", "pushed", "opened", "built", "resolved",
    "changed", "ready", "verified", "succeeded", "successful", "successfully", "shipped", "landed", "applied", "installed", "configured",
    "migrated", "exists", "summary", "✅", "replaced", "moved", "cleaned", "upgraded", "bumped", "released", "tested",
];

/// It says the work goes on after its turn: someone has to check later.
const PENDING: &[&str] = &[
    "will create",
    "will be created",
    "will finish",
    "will run",
    "will confirm",
    "will let you know",
    "i'll confirm",
    "i'll let you know",
    "i'll check",
    "i'll report",
    "is running",
    "is still running",
    "still running",
    "in progress",
    "in the background",
    "once it finishes",
    "once the",
    "once its",
    "when it finishes",
    "when the completion",
    "waiting for the",
    "not yet",
    "hasn't finished",
    "haven't finished",
    "about to",
];

const NEG: &[&str] = &["not", "nothing", "no", "never", "isn't", "wasn't", "aren't", "haven't", "hasn't", "didn't", "hadn't", "nor", "without"];

const STOP: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "then", "than", "please", "make", "sure", "also", "can", "you", "your", "should", "would",
    "could", "need", "want", "into", "from", "have", "has", "had", "all", "any", "some", "each", "when", "what", "them", "its", "are",
    "was", "were", "will", "just", "now", "new", "fix", "add", "run", "use", "get", "set", "let", "see", "look", "check", "update",
    "change", "write", "make", "sure", "there", "here", "out", "our", "but", "not", "one", "two", "via", "per", "it's", "don't", "does",
    "did", "done", "only", "same", "other", "more", "most", "very", "much", "into", "onto", "about", "over", "under", "after", "before",
    "they", "their", "it", "is", "be", "to", "of", "in", "on", "at", "by", "an", "a", "or", "if", "so", "as", "we", "i", "me", "my",
];

/// The words that carry meaning, roughly stemmed (`tests`/`testing` → `test`).
fn keywords(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-' || c == '\''))
        .map(|w| w.trim_matches(|c: char| c == '-' || c == '\''))
        .filter(|w| w.chars().count() >= 3 && !STOP.contains(w) && !w.chars().all(|c| c.is_ascii_digit()))
        .map(stem)
        .collect()
}

fn stem(w: &str) -> String {
    for suf in ["ing", "ed", "es", "s"] {
        if let Some(base) = w.strip_suffix(suf)
            && base.chars().count() >= 3
            // "fixes" → "fix", but "notes" → "note".
            && (suf != "es" || ["s", "x", "z", "ch", "sh"].iter().any(|e| base.ends_with(e)))
        {
            return base.to_string();
        }
    }
    w.to_string()
}

/// Whether a session's last message shows it did what it was asked: it says it's done
/// (and not that it couldn't), and it talks about the same things. A message that
/// waits for the user is a question; one that only says it couldn't is a failure;
/// anything else is for someone to read.
pub fn judge(asked: &[&str], said: &str) -> Verdict {
    let said = said.trim();
    if said.is_empty() {
        return Verdict::Unclear("it said nothing at the end of its turn".into());
    }
    if let Some(r) = crate::questions::waits_for_user(said) {
        // Told to wait for instructions, and it says it does: that is what
        // it was asked, not a question for the user.
        let as_asked = crate::questions::ready_only(said) && asked.iter().any(|a| crate::questions::ready_only(a));
        if !as_asked {
            return Verdict::Asks(r);
        }
    }
    let low = said.to_lowercase().replace(['’', '‘'], "'");
    let fail = FAIL.iter().find(|p| low.contains(*p));
    // Still going ("the job is running and will create it once …"): not a claim it's done.
    if let Some(p) = PENDING.iter().find(|p| low.contains(*p)) {
        return Verdict::Unclear(format!("it says it's still going: \"{}\"", sentence_with(said, p)));
    }
    // Words as written: a file name (`DONE-web.txt`, `fix_done.rs`) is one word, not "done".
    let words: Vec<&str> = low
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '(' | ')' | '"' | '`' | '*' | '!' | '?' | '[' | ']'))
        .map(|w| w.trim_matches(|c: char| matches!(c, '.' | '\'' | '-' | '_')))
        .filter(|w| !w.is_empty())
        .collect();
    // "nothing changed", "not done": no claim.
    let negated = |i: usize| i > 0 && NEG.contains(&words[i - 1]);
    let done = words
        .iter()
        .enumerate()
        .find(|(i, w)| DONE.contains(*w) && !negated(*i))
        .map(|(_, w)| *w)
        .or_else(|| low.contains('✅').then_some("✅"));
    let want: BTreeSet<String> = asked.iter().flat_map(|a| keywords(a)).collect();
    let got = keywords(said);
    let shared: Vec<&String> = want.iter().filter(|w| got.contains(*w)).collect();
    let need = (want.len() as f64 * 0.2).ceil().clamp(1.0, 3.0) as usize;
    let relevant = want.is_empty() || shared.len() >= need;
    match (fail, done) {
        (Some(f), None) => Verdict::Failed(sentence_with(said, f)),
        (None, Some(d)) if relevant => {
            let named: Vec<&str> = shared.iter().take(4).map(|s| s.as_str()).collect();
            Verdict::Done(if named.is_empty() {
                format!("its last message says \"{d}\"")
            } else {
                format!("its last message says \"{d}\" and names {}", named.join(", "))
            })
        }
        (Some(f), Some(_)) => Verdict::Unclear(format!("it says it's done, but also \"{}\"", sentence_with(said, f))),
        (None, Some(_)) => Verdict::Unclear("it says it's done, but doesn't talk about what it was asked".into()),
        (None, None) => Verdict::Unclear("its last message doesn't say it's done".into()),
    }
}

/// The sentence of `text` that holds `phrase` (lowercase), clipped.
fn sentence_with(text: &str, phrase: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let low = flat.to_lowercase();
    let Some(at) = low.find(phrase) else { return clip(&flat, 200) };
    let start = low[..at].rfind(['.', '!', '?', '\n']).map(|i| i + 1).unwrap_or(0);
    let end = low[at..].find(['.', '!', '?', '\n']).map(|i| at + i + 1).unwrap_or(flat.len());
    clip(flat.get(start..end).unwrap_or(&flat).trim(), 200)
}

// ---------------------------------------------------------------- printing

fn ago(ms: u64) -> String {
    crate::phone::ago(ms)
}

/// One agenda, its tasks under it, for `tower agenda`.
pub fn render(a: &Agenda, detail: bool) -> String {
    let spaces = if a.spaces.is_empty() { String::new() } else { format!(" [{}]", a.spaces.join(", ")) };
    let mut out = format!("{} {} · {} · {}{} · {}\n   \"{}\"\n", a.id, a.state.word(), a.tally(), a.origin, spaces, ago(a.created_ms), clip(&a.words, 300));
    if !a.note.is_empty() {
        out += &format!("   note: {}\n", a.note);
    }
    for t in &a.tasks {
        let why = match t.state {
            TaskState::Blocked => t.question.clone(),
            TaskState::DoneClaimed => format!("said: {}", t.said),
            _ => t.why.clone(),
        };
        let why = if why.is_empty() { String::new() } else { format!(": {}", clip(&why, if detail { 400 } else { 110 })) };
        let ids: Vec<String> = t.queue_ids.iter().map(|i| format!("#{i}")).collect();
        out += &format!("   {} {:<12} {} on {} {}{why}\n", t.id, t.state.word(), t.title, t.who(), ids.join(" "));
        if detail {
            out += &format!("      asked: {}\n", clip(&t.instruction, 600));
            for f in &t.followups {
                out += &format!("      then: {}\n", clip(f, 300));
            }
            if !t.after.is_empty() {
                out += &format!("      after: {}\n", t.after.join(", "));
            }
            if !t.jobs.is_empty() {
                out += &format!("      jobs: {}\n", t.jobs.join(", "));
            }
            for s in &t.history {
                out += &format!("      {} {:<12} {}\n", ago(s.at_ms), s.state.word(), clip(&s.why, 200));
            }
        }
    }
    out
}

/// Unfinished agendas touching `space` (all with None), a line each, for `tower digest`.
pub fn digest_lines(l: &Ledger, space: Option<&str>) -> Vec<String> {
    l.open_agendas()
        .into_iter()
        .filter(|a| space.is_none_or(|s| a.spaces.iter().any(|x| x == s) || a.spaces.is_empty()))
        .map(|a| {
            let open: Vec<String> = a.unfinished().iter().map(|t| format!("{} {}", t.title, t.state)).collect();
            format!("- {} \"{}\" ({}): still open: {}", a.id, clip(&a.words, 120), a.tally(), open.join("; "))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger_with(n: usize, now: u64) -> (Ledger, String, Vec<String>) {
        let mut l = Ledger::default();
        let heard = Heard { text: "fix the login bug in the api and update the docs site".into(), at_ms: now - 1000 };
        let mut ids = vec![];
        let mut agenda = String::new();
        for i in 0..n {
            let r = l
                .register(
                    &Reg { queue_id: 10 + i as u64, session: format!("t{}", i + 1), name: format!("s{}", i + 1), text: format!("task number {i}: fix the login bug"), tag: Tag::default() },
                    &heard,
                    now,
                )
                .unwrap();
            agenda = r.agenda.clone();
            ids.push(r.task);
        }
        (l, agenda, ids)
    }

    fn delivered(session: &str, at_ms: u64) -> QueueFact {
        QueueFact::Delivered { session: session.into(), at_ms, turn_before: Some(3), taken: true, finished: false, next_ms: None }
    }

    fn here(status: SessionStatus) -> Option<Live> {
        Some(Live::Here { status, created_ms: Some(1), turn: Some(4), name: "api".into(), space: Some("B2C".into()) })
    }

    fn hook_said(said: &str, at: u64) -> Option<HookFact> {
        Some(HookFact { state: SessionStatus::Done, event: "stop".into(), event_ms: at, prompt_ms: at - 5000, said: said.into(), said_ms: at, ..Default::default() })
    }

    #[test]
    fn the_table_allows_what_it_says_and_refuses_the_rest() {
        use TaskState::*;
        let stall = TaskEv::Stall { why: "x".into() };
        let delivered = TaskEv::Delivered { session: "t1".into(), name: "a".into(), created_ms: None, space: None, at_ms: 1, fresh: false };
        let rows: Vec<(TaskState, TaskEv, Result<TaskState, Refused>)> = vec![
            (Queued, delivered.clone(), Ok(Running)),
            (Stalled, delivered.clone(), Ok(Running)),
            (Blocked, delivered.clone(), Ok(Blocked)),
            (Queued, TaskEv::Working, Ok(Running)),
            (Blocked, TaskEv::Working, Ok(Running)),
            (Running, TaskEv::Working, Err(Refused::Same)),
            (DoneClaimed, TaskEv::Working, Err(Refused::NotNow(DoneClaimed))),
            (Running, TaskEv::Blocked { question: "q".into() }, Ok(Blocked)),
            (Running, TaskEv::Claimed { said: "s".into() }, Ok(DoneClaimed)),
            (DoneClaimed, TaskEv::Verify { how: "h".into() }, Ok(Verified)),
            (Running, TaskEv::Verify { how: "by hand".into() }, Ok(Verified)),
            (Verified, TaskEv::Verify { how: "h".into() }, Err(Refused::Same)),
            (Running, TaskEv::Fail { why: "w".into() }, Ok(Failed)),
            (Verified, TaskEv::Fail { why: "w".into() }, Err(Refused::Settled(Verified))),
            (Running, stall.clone(), Ok(Stalled)),
            (Failed, stall.clone(), Err(Refused::Settled(Failed))),
            (Verified, stall.clone(), Err(Refused::Settled(Verified))),
            (Abandoned, TaskEv::Working, Err(Refused::Settled(Abandoned))),
            (Failed, TaskEv::Requeue { queue_id: 9, text: String::new() }, Ok(Queued)),
            (Verified, TaskEv::Requeue { queue_id: 9, text: String::new() }, Ok(Queued)),
            (Running, TaskEv::Abandon { why: "w".into() }, Ok(Abandoned)),
            (Verified, TaskEv::Abandon { why: "w".into() }, Err(Refused::Settled(Verified))),
            (Abandoned, TaskEv::Abandon { why: "w".into() }, Err(Refused::Same)),
            (Failed, TaskEv::Verify { how: "the user says so".into() }, Ok(Verified)),
        ];
        for (from, ev, want) in rows {
            assert_eq!(from.next(&ev), want, "{from} + {ev:?}");
        }
    }

    #[test]
    fn apply_records_history_and_details() {
        let mut t = Task { id: "a1.1".into(), instruction: "do x".into(), queue_ids: vec![1], ..Default::default() };
        t.apply(TaskEv::Delivered { session: "t5".into(), name: "api".into(), created_ms: Some(7), space: Some("B2C".into()), at_ms: 100, fresh: false }, 100).unwrap();
        assert_eq!((t.state, t.session.as_str(), t.delivered_ms), (TaskState::Running, "t5", 100));
        t.apply(TaskEv::Blocked { question: "Which db?".into() }, 200).unwrap();
        assert_eq!(t.apply(TaskEv::Blocked { question: "Which db?".into() }, 210), Err(Refused::Same));
        t.apply(TaskEv::Working, 300).unwrap();
        t.apply(TaskEv::Stall { why: "gone".into() }, 400).unwrap();
        assert_eq!(t.apply(TaskEv::Stall { why: "gone".into() }, 410), Err(Refused::Same));
        t.apply(TaskEv::Requeue { queue_id: 2, text: "try again on t6".into() }, 500).unwrap();
        assert_eq!((t.state, t.delivered_ms, t.queue_ids.clone()), (TaskState::Queued, 0, vec![1, 2]));
        assert_eq!(t.followups, vec!["try again on t6".to_string()]);
        let states: Vec<TaskState> = t.history.iter().map(|s| s.state).collect();
        assert_eq!(states, vec![TaskState::Running, TaskState::Blocked, TaskState::Running, TaskState::Stalled, TaskState::Queued]);
    }

    #[test]
    fn a_multi_task_request_is_one_agenda_and_survives_a_restart() {
        let now = 1_000_000_000;
        let (l, agenda, ids) = ledger_with(3, now);
        assert_eq!(l.agendas.len(), 1);
        assert_eq!(ids, vec![format!("{agenda}.1"), format!("{agenda}.2"), format!("{agenda}.3")]);
        assert_eq!(l.agendas[0].origin, "user");
        let dir = std::env::temp_dir().join(format!("tower-agenda-test-{}", std::process::id()));
        let path = dir.join("agendas.json");
        l.save_to(&path).unwrap();
        // No temporary file left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let back = Ledger::load_from(&path);
        assert_eq!(back, l);
        // A garbled file is kept aside, never lost.
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Ledger::load_from(&path), Ledger::default());
        assert!(std::fs::read_dir(&dir).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("bad-")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registering_groups_by_request_follows_up_and_retries() {
        let now = 5_000_000_000;
        let mut l = Ledger::default();
        let heard = Heard { text: "do A and B".into(), at_ms: now - 60_000 };
        let reg = |q: u64, s: &str, text: &str| Reg { queue_id: q, session: s.into(), name: s.into(), text: text.into(), tag: Tag::default() };
        let a = l.register(&reg(1, "t1", "do A"), &heard, now).unwrap();
        let b = l.register(&reg(2, "t2", "do B"), &heard, now).unwrap();
        assert_eq!((a.agenda.as_str(), b.agenda.as_str(), a.how, b.how), ("a1", "a1", "new", "new"));
        assert_eq!(l.agendas[0].words, "do A and B");
        // The same queue id again: the same task.
        assert_eq!(l.register(&reg(2, "t2", "do B"), &heard, now).unwrap().how, "same");
        // A blocked task's session gets more: the same task, a follow-up.
        l.apply(&a.task, TaskEv::Delivered { session: "t1".into(), name: "t1".into(), created_ms: None, space: None, at_ms: now, fresh: false }, now).unwrap();
        l.apply(&a.task, TaskEv::Blocked { question: "Which way?".into() }, now + 1).unwrap();
        let later = Heard { text: "use the second way".into(), at_ms: now + 2 };
        let f = l.register(&reg(3, "t1", "use the second way"), &later, now + 3).unwrap();
        assert_eq!((f.task.as_str(), f.how), (a.task.as_str(), "followup"));
        assert_eq!(l.task(&a.task).unwrap().state, TaskState::Queued);
        // An old request and no task on the session: an agenda of the tower's own.
        let t = l.register(&reg(4, "t9", "tidy the logs"), &heard, now + 2 * REQUEST_MS).unwrap();
        assert_eq!(l.agenda(&t.agenda).unwrap().origin, "tower");
        // Named: it joins that agenda, and a plan id's task gets the new id.
        let mut tag = Tag { agenda: Some("a1".into()), plan_id: "web".into(), title: "the web part".into(), ..Default::default() };
        let w = l.register(&Reg { tag: tag.clone(), ..reg(5, "", "start web") }, &Heard::default(), now).unwrap();
        assert_eq!((w.agenda.as_str(), w.how), ("a1", "new"));
        tag.after = vec!["web".into()];
        let again = l.register(&Reg { tag: tag.clone(), ..reg(6, "t7", "start web") }, &Heard::default(), now).unwrap();
        assert_eq!((again.task.as_str(), again.how), (w.task.as_str(), "followup"));
        // skip: not registered.
        assert!(l.register(&Reg { tag: Tag { skip: true, ..Default::default() }, ..reg(7, "t1", "x") }, &heard, now).is_none());
    }

    #[test]
    fn a_session_that_finishes_and_says_so_is_verified() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(1, now);
        let t0 = now + 1000;
        let mut facts = BTreeMap::new();
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", t0), live: here(SessionStatus::Working), ..Default::default() });
        l.step(&facts, t0 + 10);
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::Running);
        assert_eq!(l.task(&ids[0]).unwrap().space.as_deref(), Some("B2C"));
        facts.insert(
            ids[0].clone(),
            Facts { queue: delivered("t1", t0), live: here(SessionStatus::Idle), hook: hook_said("Fixed the login bug: the token check was inverted. Tests pass.", t0 + 60_000), ..Default::default() },
        );
        l.step(&facts, t0 + 61_000);
        let t = l.task(&ids[0]).unwrap();
        assert_eq!(t.state, TaskState::Verified, "{:?}", t.history);
        assert!(t.why.contains("login"), "{}", t.why);
        assert_eq!(l.agendas[0].state, AgendaState::Done);
        // One task: the watch already told it; no agenda note.
        assert!(l.news(t0 + 200_000).is_empty());
    }

    #[test]
    fn a_killed_session_is_stalled_and_told_once_then_reminded() {
        let now = 9_000_000_000;
        let (mut l, agenda, ids) = ledger_with(2, now);
        let t0 = now + 1000;
        let mut facts = BTreeMap::new();
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", t0), live: here(SessionStatus::Working), ..Default::default() });
        facts.insert(ids[1].clone(), Facts { queue: delivered("t2", t0), live: here(SessionStatus::Working), ..Default::default() });
        l.step(&facts, t0);
        // t2 is killed.
        facts.insert(ids[1].clone(), Facts { queue: delivered("t2", t0), live: Some(Live::Gone), ..Default::default() });
        l.step(&facts, t0 + 5_000);
        let t = l.task(&ids[1]).unwrap();
        assert_eq!(t.state, TaskState::Stalled);
        assert!(t.why.contains("vanished"), "{}", t.why);
        // Not told within the grace, told after it, once.
        assert!(l.news(t0 + 10_000).is_empty());
        let n = l.news(t0 + 5_000 + GRACE_TROUBLE_MS);
        assert_eq!(n.len(), 1);
        assert!(n[0].msg.starts_with(&format!("[agenda] {agenda}")), "{}", n[0].msg);
        assert!(n[0].msg.contains("stalled") && n[0].msg.contains("tower agenda requeue"), "{}", n[0].msg);
        assert!(l.news(t0 + 5_000 + GRACE_TROUBLE_MS + 1).is_empty());
        // The same facts again: no new move, no new note.
        l.step(&facts, t0 + 100_000);
        assert!(l.news(t0 + 100_000 + GRACE_TROUBLE_MS).is_empty());
        // Hours later, still unfinished: a reminder, at most MAX_REMINDERS times.
        let mut at = t0 + 100_000;
        let mut reminders = 0;
        for _ in 0..6 {
            at += REMIND_MS;
            reminders += l.news(at).len();
        }
        assert_eq!(reminders, MAX_REMINDERS as usize);
        // Survives a restart as it is.
        let path = std::env::temp_dir().join(format!("tower-agenda-stall-{}.json", std::process::id()));
        l.save_to(&path).unwrap();
        let back = Ledger::load_from(&path);
        assert_eq!(back.task(&ids[1]).unwrap().state, TaskState::Stalled);
        assert!(back.radar_note().unwrap().contains(&format!("{} stalled", ids[1])));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exited_parked_cancelled_and_unknown_are_never_silent() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(4, now);
        let mut facts = BTreeMap::new();
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: Some(Live::Exited), ..Default::default() });
        facts.insert(ids[1].clone(), Facts { queue: QueueFact::Waiting { session: "t2".into(), parked: Some("gone".into()), start: false }, ..Default::default() });
        facts.insert(ids[2].clone(), Facts { queue: QueueFact::Cancelled, ..Default::default() });
        facts.insert(ids[3].clone(), Facts { queue: QueueFact::Unknown, ..Default::default() });
        l.step(&facts, now + 1);
        let st = |i: usize, l: &Ledger| l.task(&ids[i]).unwrap().state;
        assert_eq!((st(0, &l), st(1, &l), st(2, &l), st(3, &l)), (TaskState::Stalled, TaskState::Stalled, TaskState::Abandoned, TaskState::Queued));
        assert!(l.task(&ids[0]).unwrap().why.contains("exited"));
        l.step(&facts, now + UNKNOWN_MS);
        assert_eq!(st(3, &l), TaskState::Stalled);
        assert!(l.task(&ids[3]).unwrap().why.contains("unaccounted"));
        // The parked one is retargeted: it waits again.
        facts.insert(ids[1].clone(), Facts { queue: QueueFact::Waiting { session: "t5".into(), parked: None, start: false }, ..Default::default() });
        l.step(&facts, now + UNKNOWN_MS + 1);
        assert_eq!(st(1, &l), TaskState::Queued);
    }

    #[test]
    fn a_session_that_never_takes_it_is_sent_again_once_then_stalled() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(1, now);
        let not_taken = QueueFact::Delivered { session: "t1".into(), at_ms: now, turn_before: Some(3), taken: false, finished: false, next_ms: None };
        let mut facts = BTreeMap::new();
        facts.insert(ids[0].clone(), Facts { queue: not_taken.clone(), live: here(SessionStatus::Idle), ..Default::default() });
        assert!(l.step(&facts, now + 60_000).is_empty());
        let acts = l.step(&facts, now + REPROMPT_MS);
        assert_eq!(acts.len(), 1);
        let Action::Reprompt { session, .. } = &acts[0];
        assert_eq!(session, "t1");
        // The daemon queues it again.
        l.requeue(&ids[0], 99, "", now + REPROMPT_MS);
        l.task_mut(&ids[0]).unwrap().reprompts = 1;
        let again = QueueFact::Delivered { session: "t1".into(), at_ms: now + REPROMPT_MS, turn_before: Some(3), taken: false, finished: false, next_ms: None };
        facts.insert(ids[0].clone(), Facts { queue: again, live: here(SessionStatus::Idle), ..Default::default() });
        assert!(l.step(&facts, now + 2 * REPROMPT_MS + 1).is_empty());
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::Stalled);
    }

    #[test]
    fn working_without_progress_stalls_and_a_moving_screen_counts() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(1, now);
        let mut facts = BTreeMap::new();
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Working), screen: Some(1), ..Default::default() });
        l.step(&facts, now);
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Working), screen: Some(2), ..Default::default() });
        l.step(&facts, now + STALL_MS - 1000);
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::Running);
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Working), screen: Some(2), ..Default::default() });
        l.step(&facts, now + 2 * STALL_MS);
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::Stalled);
        // Signs of life again: running.
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Working), screen: Some(3), ..Default::default() });
        l.step(&facts, now + 2 * STALL_MS + 1);
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::Running);
    }

    #[test]
    fn blocked_failed_and_unclear_claims() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(3, now);
        let mut facts = BTreeMap::new();
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Blocked), question: Some("Which database?".into()), ..Default::default() });
        facts.insert(ids[1].clone(), Facts { queue: delivered("t2", now), live: here(SessionStatus::Idle), hook: hook_said("I couldn't reach the staging server, so nothing changed.", now + 9000), ..Default::default() });
        facts.insert(ids[2].clone(), Facts { queue: delivered("t3", now), live: here(SessionStatus::Idle), hook: hook_said("Here is what I found about the weather.", now + 9000), ..Default::default() });
        l.step(&facts, now + 10_000);
        let t = |i: usize| l.task(&ids[i]).unwrap().clone();
        assert_eq!((t(0).state, t(0).question.as_str()), (TaskState::Blocked, "Which database?"));
        assert_eq!(t(1).state, TaskState::Failed);
        assert_eq!(t(2).state, TaskState::DoneClaimed);
        // The brain checks the claim: `tower job done t3`.
        assert_eq!(l.judged("t3", true, "it did", now + 20_000), Some(ids[2].clone()));
        assert_eq!(l.task(&ids[2]).unwrap().state, TaskState::Verified);
        // A failed turn (StopFailure) fails it whatever it says.
        let (mut l2, _, ids2) = ledger_with(1, now);
        let mut h = hook_said("Fixed the login bug.", now + 5000).unwrap();
        h.event = "stop-failed".into();
        let mut f2 = BTreeMap::new();
        f2.insert(ids2[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Idle), hook: Some(h), ..Default::default() });
        l2.step(&f2, now + 6000);
        assert_eq!(l2.task(&ids2[0]).unwrap().state, TaskState::Failed);
    }

    #[test]
    fn close_refuses_unfinished_unless_forced_and_abandon_cancels_waiting() {
        let now = 9_000_000_000;
        let (mut l, agenda, ids) = ledger_with(2, now);
        let err = l.close(&agenda, false, "", now).unwrap_err().to_string();
        assert!(err.contains(&ids[0]) && err.contains("--force"), "{err}");
        let (dropped, cancel) = l.close(&agenda, true, "the user is happy", now).unwrap();
        assert_eq!(dropped.len(), 2);
        assert_eq!(cancel, vec![10, 11]);
        assert_eq!(l.agendas[0].state, AgendaState::Closed);
        let (mut l, agenda, ids) = ledger_with(2, now);
        let (dropped, _) = l.abandon(&ids[1], "not needed", now).unwrap();
        assert_eq!(dropped, vec![ids[1].clone()]);
        assert_eq!(l.agenda(&agenda).unwrap().state, AgendaState::Open);
        l.abandon(&agenda, "", now).unwrap();
        assert_eq!(l.agenda(&agenda).unwrap().state, AgendaState::Abandoned);
        assert!(l.radar_note().is_none());
        assert!(l.resolve("1").is_ok() && l.resolve("#1").is_ok() && l.resolve("a9").is_err());
    }

    #[test]
    fn an_agenda_of_several_tasks_says_when_it_is_all_done() {
        let now = 9_000_000_000;
        let (mut l, agenda, ids) = ledger_with(2, now);
        for id in &ids {
            l.apply(id, TaskEv::Verify { how: "by hand".into() }, now).unwrap();
        }
        assert_eq!(l.agenda(&agenda).unwrap().state, AgendaState::Done);
        let n = l.news(now + 1);
        assert_eq!(n.len(), 1);
        assert!(n[0].msg.contains("is done"), "{}", n[0].msg);
        assert!(l.news(now + 2).is_empty());
        // A follow-up reopens it.
        l.requeue(&ids[0], 77, "one more thing", now + 3);
        assert_eq!(l.agenda(&agenda).unwrap().state, AgendaState::Open);
    }

    #[test]
    fn nothing_is_sent_again_into_a_terminal_that_only_has_the_address() {
        // The task went to the terminal made at 7; the one at its address now was made at 9.
        let why = another_terminal("t5", Some(7), Some(9)).unwrap();
        assert_eq!(why, "t5 is another terminal now than the one the task was given to");
        assert_eq!(another_terminal("t5", Some(7), Some(7)), None, "the same terminal");
        // Nothing to tell them apart by (an older task, a look that didn't say): as before.
        assert_eq!((another_terminal("t5", None, Some(9)), another_terminal("t5", Some(7), None)), (None, None));
        // A retry that isn't sent counts as tried: the failure is the user's to decide, not sent anywhere.
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(1, now);
        l.apply(&ids[0], TaskEv::Fail { why: "the tests fail".into() }, now).unwrap();
        assert!(matches!(&l.failures(true, now + 1)[..], [FailedTask::Retry { .. }]));
        l.retry_unsent(&ids[0], &why);
        assert!(matches!(&l.failures(true, now + 2)[..], [FailedTask::Decide { why, .. }] if why.contains("another terminal now")));
    }

    #[test]
    fn a_task_that_fails_is_tried_once_more_and_then_is_the_users() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(1, now);
        l.task_mut(&ids[0]).unwrap().space = Some("Shop".into());
        assert!(l.failures(true, now).is_empty(), "nothing failed");
        l.apply(&ids[0], TaskEv::Fail { why: "it says it couldn't: the tests fail".into() }, now).unwrap();
        // While the tower tells everything, the brain hears of it and asks the user, as before.
        assert!(l.failures(false, now).is_empty());
        // One that failed long ago (before a restart, under an older tower) is left as it is.
        assert!(l.failures(true, now + AUTO_RETRY_MS + 1).is_empty());
        // At a quiet level the tower sends it once more by itself.
        let f = l.failures(true, now + 5);
        assert!(matches!(&f[..], [FailedTask::Retry { task, session, why, .. }] if task == &ids[0] && session == "t1" && why.contains("the tests fail")), "{f:?}");
        l.requeue(&ids[0], 50, "", now + 1);
        let t = l.task(&ids[0]).unwrap();
        assert_eq!((t.state, t.retries, t.retried_ms, t.raised), (TaskState::Queued, 1, now + 1, false));
        assert!(l.failures(true, now + 1).is_empty(), "it is on its way again");
        // The brain's word on the try before comes late (`tower job failed`): it doesn't fail the new try.
        let late = act(&mut l, &Cmd { action: "judge".into(), session: "t1".into(), ok: false, text: "the tests fail".into(), ..Default::default() }, &Heard::default(), now + 30_000).unwrap();
        assert_eq!((late.value["retried"].as_str(), late.value["task"].is_null(), late.changed), (Some(ids[0].as_str()), true, false));
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::Queued);
        // It fails again: the user's to decide, at every level, once.
        l.apply(&ids[0], TaskEv::Fail { why: "its turn failed: rate limit".into() }, now + 2).unwrap();
        assert!(l.retrying("t1", now + 3).is_none());
        for retry in [true, false] {
            let f = l.failures(retry, now + 2);
            assert!(matches!(&f[..], [FailedTask::Decide { task, space, why, .. }] if task == &ids[0] && space.as_deref() == Some("Shop") && why.contains("rate limit")), "{f:?}");
        }
        assert!(l.failed_twice().is_empty());
        l.decided(&ids[0], true, now + 3);
        assert!(l.failures(true, now + 3).is_empty() && l.failed_twice() == ids);
        assert!(l.news(now + 3 + GRACE_TROUBLE_MS).is_empty(), "the brain isn't told the failure again: the decision says it");
        // The user says try again: a new try, and the decision about the old failure is over.
        l.requeue(&ids[0], 51, "", now + 4);
        let t = l.task(&ids[0]).unwrap();
        assert_eq!((t.state, t.retries, t.raised), (TaskState::Queued, 2, false));
        assert!(l.failed_twice().is_empty());
        // A retry that can't be sent (its session is gone) counts as tried.
        let (mut l, _, ids) = ledger_with(1, now);
        l.apply(&ids[0], TaskEv::Fail { why: "it says it couldn't".into() }, now).unwrap();
        l.retry_unsent(&ids[0], "t1 has ended");
        assert!(matches!(&l.failures(true, now)[..], [FailedTask::Decide { why, .. }] if why.contains("couldn't be sent again: t1 has ended")));
        // Kept across a restart, and absent from a task that never failed.
        l.decided(&ids[0], false, now);
        let back: Ledger = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!((back.task(&ids[0]).unwrap().retries, back.task(&ids[0]).unwrap().raised), (1, true));
        let fresh = serde_json::to_string(&Task::default()).unwrap();
        assert!(!fresh.contains("retries") && !fresh.contains("retried_ms") && !fresh.contains("raised"), "{fresh}");
    }

    #[test]
    fn a_session_is_at_work_while_its_task_is_open_or_just_ended() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(2, now);
        assert_eq!(l.at_work_on("t1", now, 60_000).map(|t| t.id.as_str()), Some(ids[0].as_str()), "handed out, not through its turn");
        assert!(l.at_work_on("t9", now, 60_000).is_none() && l.at_work_on("", now, 60_000).is_none());
        l.apply(&ids[0], TaskEv::Verify { how: "checked".into() }, now + 1_000).unwrap();
        assert!(l.at_work_on("t1", now + 2_000, 60_000).is_some(), "its turn ended a moment ago");
        assert!(l.at_work_on("t1", now + 120_000, 60_000).is_none(), "long done: nothing was asked of it lately");
        assert!(l.at_work_on("t2", now + 120_000, 60_000).is_some());
    }

    #[test]
    fn judging_last_messages() {
        let asked = ["Fix the failing login test in the api and make sure the suite passes"];
        assert!(matches!(judge(&asked, "Fixed the login test: the fixture used an expired token. The suite passes now."), Verdict::Done(_)));
        assert!(matches!(judge(&asked, "I couldn't run the suite: the database container won't start."), Verdict::Failed(_)));
        assert!(matches!(judge(&asked, "Done."), Verdict::Unclear(_)));
        assert!(matches!(judge(&asked, "I looked around the repository."), Verdict::Unclear(_)));
        assert!(matches!(judge(&asked, ""), Verdict::Unclear(_)));
        assert!(matches!(judge(&["update the docs"], "Updated the docs ✅"), Verdict::Done(_)));
        // Waiting for instructions is a question, unless that is what it was told to do.
        assert!(matches!(judge(&asked, "I'm waiting for your instructions."), Verdict::Asks(_)));
        let wait = ["First task: read CLAUDE.md and README.md, then wait for your instructions"];
        assert!(!matches!(judge(&wait, "I've read CLAUDE.md and README.md. I'm on dev and waiting for your instructions."), Verdict::Asks(_)));
        assert!(matches!(judge(&wait, "Read both. Please merge PR #12 first."), Verdict::Asks(_)), "a real wait still is one");
        // Seen live: a background job "will create" the file; and a file name holding "DONE".
        let web = ["Run the shell command: sleep 240 && echo ok > DONE-web.txt . When finished, reply with one sentence saying you created DONE-web.txt."];
        assert!(matches!(
            judge(&web, "The background job is running and will create `DONE-web.txt` in the working directory once its 240-second sleep finishes. I'll confirm it when the completion notice arrives."),
            Verdict::Unclear(_)
        ));
        assert!(matches!(judge(&web, "Wrote DONE-web.txt after the sleep finished."), Verdict::Done(_)));
        // Too terse to tell against a long instruction: someone checks.
        assert!(matches!(judge(&web, "Wrote DONE-web.txt"), Verdict::Unclear(_)));
        assert!(matches!(judge(&["write the report"], "See DONE-report.md for the outline."), Verdict::Unclear(_)));
        assert!(matches!(judge(&web, "I created DONE-web.txt containing ok."), Verdict::Done(_)));
        // Seen live: "working directory" is no claim, so a message that says it couldn't is a failure.
        assert!(matches!(
            judge(&web, "I couldn't create DONE-web.txt in the foreground. The shell blocks sleep 200 in the working directory."),
            Verdict::Failed(_)
        ));
        assert!(matches!(judge(&web, "DONE-web.txt exists in the working directory and contains ok; the sleep finished."), Verdict::Done(_)));
    }

    #[test]
    fn a_turn_superseded_by_the_next_instruction_is_claimed_not_lost() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(1, now);
        let mut facts = BTreeMap::new();
        let q = QueueFact::Delivered { session: "t1".into(), at_ms: now, turn_before: Some(3), taken: true, finished: true, next_ms: Some(now + 50_000) };
        // The hook's last words are the next turn's.
        facts.insert(ids[0].clone(), Facts { queue: q, live: here(SessionStatus::Working), hook: hook_said("Next thing done.", now + 90_000), ..Default::default() });
        l.step(&facts, now + 100_000);
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::DoneClaimed);
    }

    #[test]
    fn commands_work_on_a_ledger_and_the_wire_keeps_them() {
        let now = 9_000_000_000;
        let (mut l, agenda, ids) = ledger_with(2, now);
        let heard = Heard::default();
        let cmd = |action: &str, target: &str| Cmd { action: action.into(), target: target.into(), ..Default::default() };
        let v = act(&mut l, &cmd("list", ""), &heard, now).unwrap().value;
        assert_eq!(v["agendas"].as_array().unwrap().len(), 1);
        let v = act(&mut l, &cmd("tasks", ""), &heard, now).unwrap().value;
        assert_eq!(v["tasks"].as_array().unwrap().len(), 2);
        let v = act(&mut l, &cmd("show", &ids[1]), &heard, now).unwrap().value;
        assert_eq!((v["agenda"]["id"].as_str(), v["task"].as_str()), (Some(agenda.as_str()), Some(ids[1].as_str())));
        let a = act(&mut l, &Cmd { text: "it works, I tried it".into(), ..cmd("verify", &ids[0]) }, &heard, now).unwrap();
        assert!(a.changed);
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::Verified);
        assert!(act(&mut l, &cmd("verify", &ids[0]), &heard, now).is_err());
        let a = act(&mut l, &Cmd { text: "not needed".into(), ..cmd("abandon", &ids[1]) }, &heard, now).unwrap();
        assert_eq!(a.cancel, vec![11]);
        assert_eq!(l.agenda(&agenda).unwrap().state, AgendaState::Done);
        // A new agenda from the user's latest request, or from words given.
        let recent = Heard { text: "ship the release".into(), at_ms: now - 1000 };
        let v = act(&mut l, &cmd("open", ""), &recent, now).unwrap().value;
        assert_eq!(v["words"], "ship the release");
        let again = act(&mut l, &cmd("open", ""), &recent, now).unwrap().value;
        assert_eq!(again["id"], v["id"]);
        assert!(act(&mut l, &cmd("open", ""), &Heard::default(), now).is_err());
        assert!(act(&mut l, &cmd("nonsense", ""), &heard, now).is_err());
        // On the socket: the request's tag and the command's fields side by side.
        let req = crate::protocol::Request::Agenda { ledger: Cmd { force: true, ..cmd("close", "a1") } };
        let wire = serde_json::to_string(&req).unwrap();
        assert!(wire.contains("\"cmd\":\"agenda\"") && wire.contains("\"action\":\"close\""), "{wire}");
        let back: crate::protocol::Request = serde_json::from_str(&wire).unwrap();
        assert!(matches!(back, crate::protocol::Request::Agenda { ledger } if ledger.force && ledger.target == "a1"));
        // A queue request from an older client has no tag; one with a tag keeps it.
        let old: crate::protocol::Request = serde_json::from_str(r#"{"cmd":"queue","action":"add","session":"t1","text":"x"}"#).unwrap();
        assert!(matches!(old, crate::protocol::Request::Queue { tag: None, .. }));
        let tagged: crate::protocol::Request =
            serde_json::from_str(r#"{"cmd":"queue","action":"add","session":"t1","text":"x","tag":{"agenda":"a2","title":"t"}}"#).unwrap();
        assert!(matches!(tagged, crate::protocol::Request::Queue { tag: Some(Tag { agenda: Some(ref a), .. }), .. } if a == "a2"));
    }

    #[test]
    fn a_finished_request_gets_one_receipt() {
        let now = 1_000_000;
        let (mut l, id, tasks) = ledger_with(2, now);
        l.task_mut(&tasks[0]).unwrap().space = Some("Shop".into());
        let e = l.receipt(&id, now).unwrap_err().to_string();
        assert!(e.contains("isn't finished") && e.contains(&tasks[0]), "{e}");
        for t in &tasks {
            l.apply(t, TaskEv::Verify { how: "checked".into() }, now).unwrap();
        }
        let cmd = Cmd { action: "receipt".into(), target: tasks[1].clone(), ..Default::default() };
        let acted = act(&mut l, &cmd, &Heard::default(), now + 5).unwrap();
        assert_eq!((acted.value["agenda"].as_str(), acted.changed), (Some(id.as_str()), true), "by one of its tasks, too");
        assert_eq!(acted.value["spaces"], serde_json::json!(["Shop"]));
        assert_eq!(acted.value["asked"], true, "a request of the user's");
        assert_eq!(l.agenda(&id).unwrap().receipt_ms, Some(now + 5));
        assert!(l.receipt(&id, now + 9).unwrap_err().to_string().contains("has its receipt already"));
        // `tower job done` on a request's last open task says the request is done, until it has its receipt.
        let (mut j, jid, jtasks) = ledger_with(2, now);
        let judge = |l: &mut Ledger, session: &str| act(l, &Cmd { action: "judge".into(), session: session.into(), ok: true, ..Default::default() }, &Heard::default(), now).unwrap().value;
        let first = judge(&mut j, "t1");
        assert_eq!((first["task"].as_str(), first["agenda_done"].is_null()), (Some(jtasks[0].as_str()), true), "one task is still open");
        assert_eq!(judge(&mut j, "t2")["agenda_done"].as_str(), Some(jid.as_str()));
        // The tower had checked it by itself already: the brain's `tower job done` still hears the request is done.
        let again = judge(&mut j, "t2");
        assert_eq!((again["task"].is_null(), again["agenda_done"].as_str()), (true, Some(jid.as_str())));
        assert!(j.receipt_due("t2", now + RECEIPT_DUE_MS + 1, RECEIPT_DUE_MS).is_none(), "not for a request finished long ago");
        j.receipt(&jid, now).unwrap();
        assert!(judge(&mut j, "t2")["agenda_done"].is_null(), "it has its receipt");
        // Work the tower started on its own is no request of the user's.
        j.agenda_mut(&jid).unwrap().receipt_ms = None;
        j.agenda_mut(&jid).unwrap().origin = "tower".into();
        assert!(j.receipt_due("t2", now, RECEIPT_DUE_MS).is_none());
        assert!(!j.receipt(&jid, now).unwrap().2);
        assert!(l.receipt("a99", now).is_err());
        // Kept across a restart, and absent from an agenda that has none.
        let back: Ledger = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back.agenda(&id).unwrap().receipt_ms, Some(now + 5));
        assert!(!serde_json::to_string(&Agenda::default()).unwrap().contains("receipt_ms"));
    }

    #[test]
    fn digest_and_radar_lines_name_what_is_open() {
        let now = 9_000_000_000;
        let (mut l, agenda, ids) = ledger_with(2, now);
        l.apply(&ids[0], TaskEv::Delivered { session: "t1".into(), name: "api".into(), created_ms: None, space: Some("B2C".into()), at_ms: now, fresh: false }, now).unwrap();
        l.apply(&ids[0], TaskEv::Stall { why: "its session exited before it finished".into() }, now).unwrap();
        let lines = digest_lines(&l, Some("B2C"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(&agenda) && lines[0].contains("stalled"), "{}", lines[0]);
        assert!(digest_lines(&l, Some("Elsewhere")).is_empty());
        let r = l.radar_note().unwrap();
        assert!(r.starts_with("[agendas] unfinished: ") && r.contains(&format!("{} stalled", ids[0])) && r.contains("1 in progress"), "{r}");
        let text = render(l.agenda(&agenda).unwrap(), true);
        assert!(text.contains("asked: task number 0") && text.contains("stalled"), "{text}");
    }

    #[test]
    fn a_session_that_ends_on_an_unchecked_claim_is_stalled() {
        let now = 9_000_000_000;
        let (mut l, _, ids) = ledger_with(2, now);
        let mut facts = BTreeMap::new();
        let going = "The job is running in the background and will create the file once it finishes.";
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Idle), hook: hook_said(going, now + 9000), ..Default::default() });
        facts.insert(ids[1].clone(), Facts { queue: delivered("t2", now), live: here(SessionStatus::Idle), hook: hook_said("Fixed the login bug.", now + 9000), ..Default::default() });
        l.step(&facts, now + 10_000);
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::DoneClaimed);
        // Both sessions are killed: the unchecked claim stalls, the verified task stays verified.
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: Some(Live::Gone), hook: hook_said(going, now + 9000), ..Default::default() });
        facts.insert(ids[1].clone(), Facts { queue: delivered("t2", now), live: Some(Live::Gone), ..Default::default() });
        l.step(&facts, now + 20_000);
        let t = l.task(&ids[0]).unwrap();
        assert_eq!(t.state, TaskState::Stalled);
        assert!(t.why.contains("still going"), "{}", t.why);
        assert_eq!(l.task(&ids[1]).unwrap().state, TaskState::Verified);
        // Its background job reports later, in a new turn: claimed again, and judged again.
        let (mut l, _, ids) = ledger_with(1, now);
        facts.clear();
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Idle), hook: hook_said(going, now + 9000), ..Default::default() });
        l.step(&facts, now + 10_000);
        facts.insert(ids[0].clone(), Facts { queue: delivered("t1", now), live: here(SessionStatus::Idle), hook: hook_said("The sleep finished and I created the login bug fix file.", now + 300_000), ..Default::default() });
        l.step(&facts, now + 301_000);
        assert_eq!(l.task(&ids[0]).unwrap().state, TaskState::Verified);
    }

    #[test]
    fn stems_are_rough_but_not_wrong() {
        assert_eq!(stem("notes"), "note");
        assert_eq!(stem("fixes"), "fix");
        assert_eq!(stem("tests"), "test");
        assert_eq!(stem("testing"), "test");
    }

    #[test]
    fn screens_hash_without_digits() {
        assert_eq!(screen_hash("Working (12s)"), screen_hash("Working (13s)"));
        assert_ne!(screen_hash("Working"), screen_hash("Reading file"));
    }

    /// What the user typed in a session's own terminal is their request
    /// there: an agenda of origin `terminal`, running at once; more typed
    /// while it is unfinished is more of it; what the tower hands that
    /// session is never part of it; its turn's end settles it; and no brain
    /// hears a word of it, whatever happens to it.
    #[test]
    fn work_typed_in_a_terminal_is_the_users_own_request() {
        let now = 10_000_000;
        let mut l = Ledger::default();
        let typed = |words: &str, at: u64| Typed {
            session: "t5".into(),
            name: "api".into(),
            created_ms: Some(1),
            space: Some("Shop".into()),
            words: words.into(),
            at_ms: at,
        };
        let r = l.typed(&typed("add rate limiting to the login endpoint", now), now);
        assert_eq!(r.how, "new");
        let a = l.agenda(&r.agenda).unwrap().clone();
        assert_eq!((a.origin.as_str(), a.words.as_str(), a.spaces.as_slice()), (TERMINAL, "add rate limiting to the login endpoint", &["Shop".to_string()][..]));
        let t = l.task(&r.task).unwrap().clone();
        assert_eq!((t.state, t.session.as_str(), t.space.as_deref(), t.queue_ids.len()), (TaskState::Running, "t5", Some("Shop"), 0));
        // "What's left?" and the space's mission have it.
        assert_eq!(l.requests_of("Shop", 0).open, 1);
        assert!(digest_lines(&l, None).iter().any(|line| line.contains("rate limiting")), "{:?}", digest_lines(&l, None));
        // It is no work the tower handed out (the idle check), and no receipt is owed for it.
        assert!(l.at_work_on("t5", now, 60_000).is_none());
        // More typed there while it is open: the same task.
        let more = l.typed(&typed("also log the rejected ones", now + 60_000), now + 60_000);
        assert_eq!((more.task.as_str(), more.how), (r.task.as_str(), "followup"));
        assert_eq!(l.task(&r.task).unwrap().followups, ["also log the rejected ones"]);
        // The brain hands the same session something: a task of its own, not more of the user's.
        let reg = Reg { queue_id: 7, session: "t5".into(), name: "api".into(), text: "update the API docs".into(), tag: Tag::default() };
        let theirs = l.register(&reg, &Heard::default(), now + 70_000).unwrap();
        assert_ne!(theirs.agenda, r.agenda);
        assert_eq!(l.agenda(&theirs.agenda).unwrap().origin, "tower");
        // Its turn ends with a message: done, with what it said (an unclear claim too: the user saw it).
        let facts = |said: &str, at: u64| {
            let mut m = BTreeMap::new();
            m.insert(r.task.clone(), Facts { live: here(SessionStatus::Done), hook: hook_said(said, at), ..Default::default() });
            m
        };
        l.step(&facts("Here is what I changed.", now + 120_000), now + 120_000);
        let t = l.task(&r.task).unwrap();
        assert_eq!((t.state, t.said.as_str()), (TaskState::Verified, "Here is what I changed."));
        assert_eq!(l.agenda(&r.agenda).unwrap().state, AgendaState::Done);
        assert!(l.receipt_due("t5", now + 121_000, RECEIPT_DUE_MS).is_none(), "the user saw it end there: nothing for the brain to report");
        // Done, failed, stalled or blocked: no note to any brain, ever; never sent again, never a decision.
        let next = l.typed(&typed("now deploy it", now + 200_000), now + 200_000);
        assert_eq!(next.how, "new", "the last one is settled: a new request");
        l.apply(&next.task, TaskEv::Fail { why: "the deploy key is missing".into() }, now + 210_000).unwrap();
        let later = now + 210_000 + REMIND_MS + GRACE_BLOCKED_MS;
        assert!(l.failures(true, now + 211_000).is_empty(), "not sent again by the tower");
        let notes = l.news(later);
        assert!(notes.iter().all(|n| !n.msg.contains("rate limiting") && !n.msg.contains("deploy it")), "{notes:?}");
        // The tower's own task there is still followed as before.
        assert!(l.unfinished_handed("t5").is_some_and(|t| t.id == theirs.task));
    }
}
