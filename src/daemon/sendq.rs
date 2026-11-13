//! The send queue (`tower queue <session> "<instruction>"`): an instruction
//! for a session that is busy waits here, and goes in the moment that
//! session stops working, so the brain never has to promise to "send it
//! later" and remember to.
//!
//! Kept in `~/.local/state/herder-tower/queue.json`, written before each
//! answer and before each hand-over, so a restarted voice still delivers,
//! and delivers once:
//!
//! - Each session's instructions go in the order they were queued, one per
//!   turn: the next waits until the session has taken the last one (Herdr's
//!   turn count moved, or its hook said so) and stopped again.
//! - An instruction is claimed before it's typed. If the tower stops in
//!   between, its next start looks at the session's turn count: moved means
//!   it went in (it isn't typed again); not moved means it's typed again.
//! - An instruction knows which terminal it's for (its address and when the
//!   terminal was made). A session that ends, or an address that now belongs
//!   to another terminal, parks what waits for it: it goes nowhere on its
//!   own until `tower queue retarget` or `cancel`. A machine out of reach is
//!   not "gone": its instructions wait for it.
//! - The same instruction queued twice for a session (a retry) is the same
//!   item, not two.
//! - `after`: an instruction can wait for others to be done first (`tower
//!   dispatch`), on any session.
//!
//! Each delivery is watched (the brain hears when the session stops), written
//! to the history, and told to the brain (`[queue] delivered to <t> ...`)
//! through the radar's outbox, so a space out of focus holds it like a watch.
//! At a quiet level (`tell`) a delivery or a start is only written down: the
//! brain is told when something is stuck, not when something went in.
//!
//! A message one session queues for another (`peers`) rides the same queue
//! and is no task: not watched, not told to any brain, kept in the history
//! as `peer`. One that can't be delivered is taken out and its sender told.
//! A Claude Code session that has been working longer than `[queue]
//! busy_deliver_minutes` (background agents keep one "working" for hours)
//! gets such a message typed in anyway, one at a time: Claude Code keeps
//! what is typed while it works and sends it as its next message when its
//! turn ends (`hc send` pastes and presses Enter, nothing that interrupts).
//! The user's and the brains' instructions always wait for the turn to end.
//!
//! A Claude Code or Codex session in `error` (its turn ended on an API
//! error) sits at its prompt: it takes an instruction as idle or done would.
//!
//! What the user types in a session's own terminal (`user_said`) is checked
//! against the tower's messages waiting for it (not a note, not a message
//! between sessions): one queued from ten minutes before those words on
//! waits until the check is done (`covered::judge`, `Sessions::check`), and
//! one the user's words already cover (they said it, gave the same
//! instruction, or made it moot) is not sent: it leaves the queue, the
//! record says so ("Not sent: you had told it yourself …"), and its task in
//! the agenda is dropped with that reason. A check that fails, or takes
//! longer than two minutes, lets the messages go. A session the user has
//! been typing in takes the tower's next message three seconds after its
//! turn ends, not at once: what the user typed while it worked goes in first.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hc::SessionStatus;
use crate::{config, hc};

/// After a hand-over, the session gets this long to show it took it (its
/// turn count moves) before the next may follow anyway.
const TAKE_MS: u64 = 90_000;
/// No two hand-overs to one session closer than this (the paste settles).
const GAP_MS: u64 = 3_000;
/// The brain hears about a machine out of reach after this long.
const UNREACHABLE_TELL_MS: u64 = 10 * 60_000;
/// The same instruction again within this long is a retry, not a new one.
const RETRY_MS: u64 = 10 * 60_000;
/// Deliveries remembered (for retries and `after`).
const KEEP_DELIVERED: usize = 300;
/// A parked instruction nobody retargets or cancels moves to `expired` after this long.
const PARK_EXPIRE_MS: u64 = 7 * 24 * 3_600_000;
/// What the user typed in a session is checked against the tower's messages
/// for it queued after it, or this long before it.
const SAID_BEFORE_MS: u64 = 10 * 60_000;
/// The user's own words in a session are kept this long (and at most `SAID_KEEP`).
const SAID_KEEP_MS: u64 = 30 * 60_000;
const SAID_KEEP: usize = 12;
/// A check that hasn't answered in this long is over: what it held goes.
const CHECK_LIMIT_MS: u64 = 120_000;
/// A session the user has been typing in lately takes the tower's next
/// message only this long after its turn ended: what the user typed while it
/// worked (Claude Code sends it the moment that turn ends) comes first, and
/// is checked against it.
const SETTLE_MS: u64 = 3_000;

/// Why an instruction won't go on its own. On disk (and on the wire) it is
/// its word, as before: `gone`, `replaced`, `after-cancelled`, `start-failed`;
/// a word this version doesn't know is kept as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ParkWhy {
    /// Its session ended.
    Gone,
    /// Its address is another terminal's now.
    Replaced,
    /// An instruction it waited for was cancelled.
    AfterCancelled,
    /// The session it was to start didn't start (three tries).
    StartFailed,
    Other(String),
}

impl ParkWhy {
    pub fn word(&self) -> &str {
        match self {
            ParkWhy::Gone => "gone",
            ParkWhy::Replaced => "replaced",
            ParkWhy::AfterCancelled => "after-cancelled",
            ParkWhy::StartFailed => "start-failed",
            ParkWhy::Other(s) => s,
        }
    }
}

impl From<String> for ParkWhy {
    fn from(s: String) -> ParkWhy {
        match s.as_str() {
            "gone" => ParkWhy::Gone,
            "replaced" => ParkWhy::Replaced,
            "after-cancelled" => ParkWhy::AfterCancelled,
            "start-failed" => ParkWhy::StartFailed,
            _ => ParkWhy::Other(s),
        }
    }
}

impl From<ParkWhy> for String {
    fn from(p: ParkWhy) -> String {
        p.word().to_string()
    }
}

impl std::fmt::Display for ParkWhy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.word())
    }
}

/// Where a queued instruction stands (derived from its fields: storage is
/// as it was). See docs/state-machines.md, section 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemState {
    /// A session to start once what it waits for is done (`session` empty).
    ToStart,
    /// Waits for these ids to be done first.
    WaitingFor(Vec<u64>),
    /// Waits for its session to be ready (idle or done, the gap and take rules).
    Waiting,
    /// Claimed on disk before typing; after a crash `turn_before` says whether it went in.
    Claimed { since_ms: u64, turn_before: Option<u64> },
    /// Won't go on its own; the brain was told once.
    Parked(ParkWhy),
}

/// What happens to a queued instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemEv {
    /// About to be typed (or started): on disk first. `busy`: typed in while
    /// its session works (its turn count then is that of the turn under way).
    Claim { now_ms: u64, turn: Option<u64>, busy: bool },
    /// The hand-over failed: it waits again, one try more.
    Failed,
    /// The claim is let go without a try (it couldn't be saved; a restart found it didn't go in).
    Release,
    Park { why: ParkWhy, now_ms: u64 },
    /// `tower queue retarget`: another session, a fresh start.
    Retarget { session: String, name: String, created_ms: Option<u64> },
}

/// A move the queue doesn't allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemRefused {
    /// It is being typed right now.
    GoingIn,
    /// It won't go on its own (claimed only after a retarget).
    Parked(ParkWhy),
    /// A hand-over's result for an instruction nobody claimed.
    NotClaimed,
}

impl std::fmt::Display for ItemRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItemRefused::GoingIn => f.write_str("it is going in right now"),
            ItemRefused::Parked(why) => write!(f, "it is parked ({why})"),
            ItemRefused::NotClaimed => f.write_str("it isn't going in"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Item {
    pub id: u64,
    /// The session's address (`t5`, `vps/t2`).
    pub session: String,
    /// Its spoken name when it was queued.
    pub name: String,
    pub text: String,
    pub at_ms: u64,
    /// Failed hand-overs so far (it is tried again).
    #[serde(default)]
    pub tries: u32,
    /// When the terminal was made: which terminal at that address it's for.
    #[serde(default)]
    pub created_ms: Option<u64>,
    /// Why it won't go on its own (`gone`, `replaced`, `after-cancelled`, `start-failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<ParkWhy>,
    /// Since when it is parked (it expires a week later). Parked by an
    /// older version: counted from when this one first saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_ms: Option<u64>,
    /// Ids that must be done first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<u64>,
    /// Claimed for a hand-over (being typed, or the tower stopped meanwhile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sending_ms: Option<u64>,
    /// The session's turn count when it was claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_before: Option<u64>,
    /// The brain was told about it (parked, stuck): once.
    #[serde(default)]
    pub gone_told: bool,
    /// A session to start (`tower start` arguments) once `after` is done,
    /// instead of a session to type into (`session` is empty until then).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub start: Vec<String>,
    /// A note for the session, not work (an introduction to the others in
    /// its space): delivered like any instruction, but not watched, not told
    /// to the brain, and dropped (not parked) if its session ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet: bool,
    /// A message from another session (not a brain, not the user): who sent
    /// it. Delivered like any instruction, with the sender's name on it, but
    /// it is no task: not watched, not in the agenda, not told to the brain;
    /// kept in the history as `peer`. If it can't be delivered it is taken
    /// out (not parked) and its sender is told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<crate::peers::Sender>,
    /// Claimed to be typed in while its session works (a message from
    /// another session, see `Book::busy_pick`): `turn_before` is then the
    /// count that session's turn under way ends on, so the turn its message
    /// starts is the one after. Still set after a restart let the claim go:
    /// it may be in that session's input already, so it is not typed in
    /// again while the session works, only once it is free.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
}

impl Item {
    /// Its state, as far as the item itself tells (`Book::state` also knows
    /// whether what it waits for is done).
    pub fn state(&self) -> ItemState {
        if let Some(why) = &self.parked {
            return ItemState::Parked(why.clone());
        }
        if let Some(since_ms) = self.sending_ms {
            return ItemState::Claimed { since_ms, turn_before: self.turn_before };
        }
        if !self.after.is_empty() {
            return ItemState::WaitingFor(self.after.clone());
        }
        if !self.start.is_empty() { ItemState::ToStart } else { ItemState::Waiting }
    }

    /// The whole table: moves the item by `ev`, or says why not.
    pub fn apply(&mut self, ev: ItemEv) -> Result<(), ItemRefused> {
        match (ev, self.state()) {
            (ItemEv::Claim { .. }, ItemState::Claimed { .. }) => Err(ItemRefused::GoingIn),
            (ItemEv::Claim { .. }, ItemState::Parked(why)) => Err(ItemRefused::Parked(why)),
            (ItemEv::Claim { now_ms, turn, busy }, _) => {
                self.sending_ms = Some(now_ms);
                if self.start.is_empty() {
                    // Typed in while it works: the turn under way ends on one more.
                    self.turn_before = if busy { turn.map(|n| n + 1) } else { turn };
                }
                self.busy = busy;
                Ok(())
            }
            (ItemEv::Failed, ItemState::Claimed { .. }) => {
                self.sending_ms = None;
                self.tries += 1;
                self.busy = false;
                Ok(())
            }
            (ItemEv::Release, ItemState::Claimed { .. }) => {
                self.sending_ms = None;
                Ok(())
            }
            (ItemEv::Failed | ItemEv::Release, _) => Err(ItemRefused::NotClaimed),
            // Never parked while its keys go in: it is in, or it waits again, first.
            (ItemEv::Park { .. } | ItemEv::Retarget { .. }, ItemState::Claimed { .. }) => Err(ItemRefused::GoingIn),
            (ItemEv::Park { why, now_ms }, _) => {
                if self.parked.as_ref() != Some(&why) {
                    self.parked = Some(why);
                    self.parked_ms = Some(now_ms);
                }
                Ok(())
            }
            (ItemEv::Retarget { session, name, created_ms }, _) => {
                self.session = session;
                self.name = name;
                self.created_ms = created_ms;
                self.parked = None;
                self.parked_ms = None;
                self.gone_told = false;
                self.tries = 0;
                Ok(())
            }
        }
    }
}

/// Where a delivery stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    /// Typed; the session hasn't shown it took it yet.
    Sent,
    /// Its turn count moved: it is working on it.
    Taken,
    /// ... and stopped since (or the session ended).
    Finished,
}

/// An instruction that went in.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Delivered {
    pub id: u64,
    pub session: String,
    pub text: String,
    pub at_ms: u64,
    /// The session's turn count before it went in.
    #[serde(default)]
    pub turn_before: Option<u64>,
    /// The session took it.
    #[serde(default)]
    pub taken: bool,
    /// ... and finished that turn.
    #[serde(default)]
    pub finished: bool,
    /// Typed in while its session worked (`Book::busy_pick`): its session
    /// sends it when that turn ends. `turn_before` is the count that turn
    /// ended on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
    /// When its session was first seen free after the turn it was typed into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_ms: Option<u64>,
}

impl Delivered {
    pub fn state(&self) -> DeliveryState {
        match (self.taken, self.finished) {
            (_, true) => DeliveryState::Finished,
            (true, false) => DeliveryState::Taken,
            (false, false) => DeliveryState::Sent,
        }
    }

    /// The session took it (Sent -> Taken). False: nothing changed.
    fn take(&mut self) -> bool {
        let moved = self.state() == DeliveryState::Sent;
        self.taken |= moved;
        moved
    }

    /// Its turn is over (Sent/Taken -> Finished). False: nothing changed.
    fn finish(&mut self) -> bool {
        let moved = self.state() != DeliveryState::Finished;
        self.finished = true;
        moved
    }
}

/// What the user typed in a session's own terminal, and when.
#[derive(Debug, Clone, PartialEq)]
pub struct Said {
    pub words: String,
    pub at_ms: u64,
}

/// A check of the tower's messages waiting for a session against what the
/// user typed there themselves: do their words already cover any of them?
#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub session: String,
    /// The session's spoken name.
    pub name: String,
    /// The user's words, oldest first.
    pub said: Vec<Said>,
    /// The waiting messages: id and text.
    pub items: Vec<(u64, String)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Book {
    next_id: u64,
    items: Vec<Item>,
    /// Notes an earlier version kept for the brain (handed to the outbox at start).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
    #[serde(default)]
    delivered: Vec<Delivered>,
    #[serde(default)]
    cancelled: BTreeSet<u64>,
    /// Sessions out of reach, since when (and whether the brain was told).
    #[serde(default)]
    unreachable: BTreeMap<String, (u64, bool)>,
    /// Sessions in a status nothing goes into (`error`, ...), told once.
    #[serde(default)]
    stuck: BTreeMap<String, String>,
    /// Parked for a week with nobody retargeting or cancelling them: out of
    /// the way, not deleted (newest last).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    expired: Vec<Item>,
    /// Sessions seen working, by address: at which turn count, and since when
    /// the queue first saw it so (Herdr's `status_ms` may say longer).
    #[serde(skip)]
    working: BTreeMap<String, (Option<u64>, u64)>,
    /// Not sent because the user had told the session themselves: id, and
    /// their words (newest last, at most `KEEP_DELIVERED`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    covered: BTreeMap<u64, String>,
    /// What the user typed in each session's own terminal lately (oldest first).
    #[serde(skip)]
    said: BTreeMap<String, Vec<Said>>,
    /// Each waiting message: the newest of the user's words it was checked against.
    #[serde(skip)]
    checked: BTreeMap<u64, u64>,
    /// Sessions whose waiting messages are being checked: since when, and which.
    #[serde(skip)]
    checking: BTreeMap<String, (u64, Vec<u64>)>,
}

impl Book {
    fn load_from(path: &Path) -> Book {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                // Never lose what's in it: keep the unreadable file aside.
                let aside = path.with_extension(format!("bad-{}.json", crate::history::now_ms()));
                let _ = std::fs::copy(path, &aside);
                eprintln!("[queue] {} unreadable ({e}); kept as {}", path.display(), aside.display());
                Book::default()
            }),
            Err(_) => Book::default(),
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn push(&mut self, s: &Session, text: &str, after: &[u64], now: u64) -> Item {
        self.next_id = self
            .next_id
            .max(self.items.iter().map(|i| i.id).max().unwrap_or(0))
            .max(self.delivered.iter().map(|d| d.id).max().unwrap_or(0))
            + 1;
        let item = Item {
            id: self.next_id,
            session: s.address.clone(),
            name: s.name.clone(),
            text: text.into(),
            at_ms: now,
            created_ms: s.created_ms,
            after: after.to_vec(),
            ..Default::default()
        };
        self.items.push(item.clone());
        item
    }

    /// Instructions waiting for a session (quiet notes and messages from
    /// other sessions aren't work: not counted).
    fn waiting_for(&self, session: &str) -> usize {
        self.items.iter().filter(|i| i.session == session && !i.quiet && i.from.is_none() && !matches!(i.state(), ItemState::Parked(_))).count()
    }

    /// A message the user's own words may cover: the tower's (not a note,
    /// not a message between sessions, not a session to start), waiting.
    fn checkable(i: &Item) -> bool {
        !i.quiet && i.from.is_none() && i.start.is_empty() && matches!(i.state(), ItemState::Waiting | ItemState::WaitingFor(_))
    }

    /// The check `session`'s waiting messages need: each one the user typed
    /// something in that session for, since it was queued or a little
    /// before, that it wasn't checked against yet; with all the user's
    /// words of that time. None: none needs one.
    fn needs_check(&self, session: &str) -> Option<Check> {
        let said = self.said.get(session)?;
        let mut items = vec![];
        let mut words: Vec<Said> = vec![];
        for i in self.items.iter().filter(|i| i.session == session && Self::checkable(i)) {
            let upto = self.checked.get(&i.id).copied().unwrap_or(0);
            let about: Vec<&Said> = said.iter().filter(|s| s.at_ms + SAID_BEFORE_MS >= i.at_ms).collect();
            if !about.iter().any(|s| s.at_ms > upto) {
                continue;
            }
            items.push((i.id, i.text.clone()));
            for s in about {
                if !words.contains(s) {
                    words.push(s.clone());
                }
            }
        }
        if items.is_empty() {
            return None;
        }
        words.sort_by_key(|s| s.at_ms);
        let name = self.items.iter().find(|i| i.session == session).map(|i| i.name.clone()).unwrap_or_default();
        Some(Check { session: session.into(), name, said: words, items })
    }

    /// What waits for each session, in order (sessions to start aside).
    fn lines(&self) -> BTreeMap<String, Vec<super::state::WaitingLine>> {
        let mut out: BTreeMap<String, Vec<super::state::WaitingLine>> = BTreeMap::new();
        for i in self.items.iter().filter(|i| i.start.is_empty()) {
            let checking = self.checking.get(&i.session).is_some_and(|(_, ids)| ids.contains(&i.id));
            out.entry(i.session.clone()).or_default().push(super::state::WaitingLine {
                id: i.id,
                text: match &i.from {
                    Some(f) => crate::peers::unsigned(f, &i.text).to_string(),
                    None => i.text.clone(),
                },
                from: i.from.as_ref().map(|f| f.name.clone()),
                note: i.quiet,
                parked: i.parked.as_ref().map(|p| p.word().to_string()),
                checking,
            });
        }
        out
    }

    /// The check `session` needs, unless one is under way: marked under way.
    fn start_check(&mut self, session: &str, now: u64) -> Option<Check> {
        if self.checking.get(session).is_some_and(|(since, _)| now.saturating_sub(*since) < CHECK_LIMIT_MS) {
            return None;
        }
        let c = self.needs_check(session)?;
        self.checking.insert(session.into(), (now, c.items.iter().map(|(id, _)| *id).collect()));
        Some(c)
    }

    /// `id` waits for a check of the user's words under way. One that took
    /// too long is over: what it held counts as checked, and goes.
    fn held_by_check(&mut self, session: &str, id: u64, now: u64) -> bool {
        let Some((since, ids)) = self.checking.get(session).cloned() else { return false };
        if now.saturating_sub(since) < CHECK_LIMIT_MS {
            return ids.contains(&id);
        }
        eprintln!("[queue] the check of {session}'s waiting messages against the user's words took too long: they go");
        self.checking.remove(session);
        let upto = self.said.get(session).and_then(|s| s.iter().map(|s| s.at_ms).max()).unwrap_or(now);
        for id in ids {
            self.checked.insert(id, upto);
        }
        false
    }

    /// The sessions to look at: those with something waiting, then those
    /// whose last delivery isn't finished yet (something may wait on it).
    fn sessions(&self, now: u64) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for i in self.items.iter().filter(|i| !matches!(i.state(), ItemState::Parked(_)) && i.start.is_empty()) {
            if !out.contains(&i.session) {
                out.push(i.session.clone());
            }
        }
        for d in self.delivered.iter().filter(|d| d.state() != DeliveryState::Finished && now.saturating_sub(d.at_ms) < 24 * 3_600_000) {
            if !out.contains(&d.session) {
                out.push(d.session.clone());
            }
        }
        out
    }

    fn last_delivery(&self, session: &str) -> Option<&Delivered> {
        self.delivered.iter().rev().find(|d| d.session == session)
    }

    /// The session may take its next instruction now.
    fn may_send(&self, s: &Session, now: u64) -> bool {
        if !s.ready() {
            return false;
        }
        match self.last_delivery(&s.address) {
            None => true,
            // Typed in while it worked: its session starts on it when that turn
            // ends, so the next waits for that one, or for a while free without it.
            Some(d) if d.busy && !d.finished => d.taken || d.free_ms.is_some_and(|f| now.saturating_sub(f) >= TAKE_MS),
            Some(d) => {
                let since = now.saturating_sub(d.at_ms);
                since >= GAP_MS && (d.taken || since >= TAKE_MS)
            }
        }
    }

    /// Since when `s` has been working, at its turn count now: the queue's
    /// own first sight of it so, or Herdr's word (`status_ms`) when that is
    /// longer. None: it isn't working.
    fn working_since(&mut self, s: &Session, now: u64) -> Option<u64> {
        if s.status != SessionStatus::Working {
            self.working.remove(&s.address);
            return None;
        }
        let seen = match self.working.get(&s.address) {
            Some((turn, since)) if *turn == s.turn => *since,
            _ => {
                self.working.insert(s.address.clone(), (s.turn, now));
                now
            }
        };
        Some(s.status_ms.filter(|t| *t > 0 && *t <= now).map_or(seen, |t| t.min(seen)))
    }

    /// A message from another session that may be typed into `s` while it
    /// works (its place in `items`): `s` is a Claude Code session (not a
    /// brain) that has been working `minutes` or more (0: never), nothing of
    /// its own is being typed, and nothing typed in that way is still to be
    /// taken; the first such message waiting for it, in order, that wasn't
    /// let go half way before (`Item::busy`). The user's and the brains'
    /// instructions wait for the turn to end; a dialog takes nothing.
    fn busy_pick(&mut self, s: &Session, now: u64, minutes: u64) -> Option<usize> {
        if minutes == 0 || !s.claude() || s.brain || s.status != SessionStatus::Working {
            return None;
        }
        let since = self.working_since(s, now)?;
        if now.saturating_sub(since) < minutes.saturating_mul(60_000) {
            return None;
        }
        if self.last_delivery(&s.address).is_some_and(|d| d.busy && !d.finished) {
            return None;
        }
        if self.items.iter().any(|i| i.session == s.address && matches!(i.state(), ItemState::Claimed { .. })) {
            return None;
        }
        let pos = self.items.iter().position(|i| {
            i.session == s.address
                && i.from.is_some()
                && !i.busy
                && i.start.is_empty()
                && matches!(i.state(), ItemState::Waiting | ItemState::WaitingFor(_))
                && self.after_done(i) == Some(true)
        })?;
        Some(pos)
    }

    /// Whether the ids an item waits for are all done (None: one was cancelled).
    fn after_done(&self, item: &Item) -> Option<bool> {
        let mut all = true;
        for id in &item.after {
            // Cancelled, or parked so long it expired: it will never be done.
            if self.cancelled.contains(id) || self.expired.iter().any(|i| i.id == *id) {
                return None;
            }
            if self.items.iter().any(|i| i.id == *id) {
                all = false;
            } else if let Some(d) = self.delivered.iter().find(|d| d.id == *id) {
                all &= d.finished;
            }
            // Neither: long done and forgotten.
        }
        Some(all)
    }

    fn delivered(&mut self, d: Delivered) {
        self.delivered.push(d);
        if self.delivered.len() > KEEP_DELIVERED {
            let n = self.delivered.len() - KEEP_DELIVERED;
            self.delivered.drain(..n);
        }
    }

    /// What the session's facts say about the last delivery (and, when that
    /// one was typed in while the session worked, the one it worked on):
    /// taken (its turn count moved), finished (and it stopped since).
    fn follow(&mut self, s: &Session, now: u64) -> bool {
        let mine: Vec<usize> = self.delivered.iter().enumerate().rev().filter(|(_, d)| d.session == s.address).map(|(i, _)| i).take(2).collect();
        let Some(&last) = mine.first() else { return false };
        let mut changed = follow_one(&mut self.delivered[last], s, now);
        if self.delivered[last].busy
            && let Some(&before) = mine.get(1)
            && !self.delivered[before].finished
        {
            changed |= follow_one(&mut self.delivered[before], s, now);
        }
        changed
    }
}

/// `Book::follow` for one delivery.
fn follow_one(d: &mut Delivered, s: &Session, now: u64) -> bool {
    let moved = matches!((s.turn, d.turn_before), (Some(n), Some(before)) if n > before);
    let mut changed = moved && d.take();
    // Typed in while it worked: the turn under way is over, and it is free.
    if d.busy && !moved && d.free_ms.is_none() && s.ready() && matches!((s.turn, d.turn_before), (Some(n), Some(b)) if n >= b) {
        d.free_ms = Some(now);
        changed = true;
    }
    // Taken and ready again; or, with no turn count to go by, ready again.
    if (moved || d.turn_before.is_none()) && s.ready() {
        changed |= d.finish();
    }
    // Free a while since, with no turn of its own: it went into the turn it was typed into.
    if d.busy && s.ready() && d.free_ms.is_some_and(|f| now.saturating_sub(f) >= TAKE_MS) {
        changed |= d.finish();
    }
    changed
}


/// What `add` did.
#[derive(Debug, Clone, PartialEq)]
pub enum Added {
    /// The session was free: it has it now (its id, for `after`).
    Sent { id: u64 },
    /// It waits: its place in that session's queue (1 = next), and the session's status.
    Queued { id: u64, position: usize, status: String },
}

/// A note for the brain (through the radar's outbox), and what to say if it can't take it.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub session: String,
    pub name: String,
    pub msg: String,
    pub speech: String,
}

#[derive(Clone)]
pub struct SendQueue {
    book: Arc<Mutex<Book>>,
    path: PathBuf,
    /// Sessions being worked on right now (one hand-over per session at a time).
    busy: Arc<Mutex<HashSet<String>>>,
    /// Wakes the worker early (a hook said a session stopped).
    wake: Arc<(Mutex<bool>, Condvar)>,
}

impl SendQueue {
    pub fn open() -> SendQueue {
        Self::at(config::state_dir().join("queue.json"))
    }

    pub(super) fn at(path: PathBuf) -> SendQueue {
        SendQueue {
            book: Arc::new(Mutex::new(Book::load_from(&path))),
            path,
            busy: Arc::new(Mutex::new(HashSet::new())),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Delivers in the background, from now on (after settling what the last run left).
    pub fn start(&self) {
        let _ = RUNNING.set(self.clone());
        let q = self.clone();
        let n = q.lock().items.len();
        if n > 0 {
            eprintln!("[queue] {n} instruction(s) waiting from the last run");
        }
        thread::Builder::new()
            .name("send-queue".into())
            .spawn(move || {
                q.recover(&Herdr);
                loop {
                    q.tick(&Herdr);
                    let (m, cv) = &*q.wake;
                    let woken = m.lock().unwrap_or_else(|e| e.into_inner());
                    let mut woken = cv
                        .wait_timeout_while(woken, Duration::from_millis(1500), |w| !*w)
                        .map(|r| r.0)
                        .unwrap_or_else(|e| e.into_inner().0);
                    *woken = false;
                }
            })
            .ok();
    }

    /// Something changed for a session (its hook): look now, not in a moment.
    pub fn kick(&self) {
        let (m, cv) = &*self.wake;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    /// An instruction for `session`: sent now if it's free (and nothing is
    /// queued before it), else kept until it is. `after`: ids to wait for.
    pub fn add(&self, session: &str, text: &str, after: &[u64]) -> Result<(Added, String, String)> {
        self.add_with(&Herdr, session, text, after, None)
    }

    /// A message for `session` from another session (see `Item::from`): the
    /// same queue, the sender's name on it, and nothing for any brain.
    pub fn add_from(&self, session: &str, text: &str, after: &[u64], from: &crate::peers::Sender) -> Result<(Added, String, String)> {
        self.add_with(&Herdr, session, text, after, Some(from))
    }

    /// A quiet note for `session` (see `Item::quiet`): it waits its turn like
    /// an instruction, so a working session is never interrupted.
    pub fn add_note(&self, session: &str, text: &str) -> Result<(Added, String, String)> {
        self.add_note_with(&Herdr, session, text, false)
    }

    /// `add_note`, ahead of everything that waits for `session`: what a
    /// session moved to another account was doing goes back in first
    /// (`daemon::failover`), and what was queued for it follows.
    pub fn add_note_first(&self, session: &str, text: &str) -> Result<(Added, String, String)> {
        self.add_note_with(&Herdr, session, text, true)
    }

    fn add_note_with(&self, h: &dyn Sessions, session: &str, text: &str, first: bool) -> Result<(Added, String, String)> {
        let text = hc::clean(text.trim());
        let s = match h.look(session) {
            Look::Live(s) => s,
            Look::Finished => bail!("{session} has ended"),
            Look::Gone => bail!("no session {session}"),
            Look::Unreachable(e) => bail!("can't reach {session} right now ({e})"),
        };
        let now = crate::history::now_ms();
        let item = {
            let mut b = self.lock();
            if let Some(i) = b.items.iter().find(|i| i.session == s.address && i.text == text) {
                return Ok((Added::Queued { id: i.id, position: 1, status: s.status.word().to_string() }, s.address, s.name));
            }
            let pos = b.items.len();
            let mut item = b.push(&s, &text, &[], now);
            item.quiet = true;
            b.items[pos].quiet = true;
            if first && let Some(at) = b.items.iter().position(|i| i.session == s.address && i.id != item.id) {
                let it = b.items.remove(pos);
                b.items.insert(at, it);
            }
            if let Err(e) = self.save(&b) {
                b.items.retain(|i| i.id != item.id);
                return Err(e);
            }
            item
        };
        self.work(h, &s.address, Some(&s));
        let b = self.lock();
        if b.delivered.iter().any(|d| d.id == item.id) {
            return Ok((Added::Sent { id: item.id }, s.address, s.name));
        }
        Ok((Added::Queued { id: item.id, position: 1, status: s.status.word().to_string() }, s.address, s.name))
    }

    fn add_with(&self, h: &dyn Sessions, session: &str, text: &str, after: &[u64], from: Option<&crate::peers::Sender>) -> Result<(Added, String, String)> {
        let text = hc::clean(text.trim());
        if text.is_empty() {
            bail!("nothing to queue: give the instruction after the session");
        }
        // A message from another session says who it is from.
        let text = match from {
            Some(f) => crate::peers::signed(f, &text),
            None => text,
        };
        let s = match h.look(session) {
            Look::Live(s) => s,
            Look::Finished => bail!("{session} has ended: start a session, or queue it for another one"),
            Look::Gone => bail!("no session {session}"),
            Look::Unreachable(e) => bail!("can't reach {session} right now ({e}); try again in a moment"),
        };
        if s.status == SessionStatus::Starting {
            bail!("{} ({}) runs no agent the tower can follow: use hc send for a plain program", s.address, s.name);
        }
        let now = crate::history::now_ms();
        let item = {
            let mut b = self.lock();
            // A retry of what's already queued, or went in a moment ago: the same item.
            if let Some(i) = b.items.iter().find(|i| i.session == s.address && i.text == text) {
                let position = b.items.iter().filter(|j| j.session == s.address && j.parked.is_none() && j.id <= i.id).count();
                return Ok((Added::Queued { id: i.id, position, status: s.status.word().to_string() }, s.address, s.name));
            }
            if let Some(d) =
                b.delivered.iter().rev().find(|d| d.session == s.address && d.text == text && now.saturating_sub(d.at_ms) < RETRY_MS)
            {
                return Ok((Added::Sent { id: d.id }, s.address, s.name));
            }
            for id in after {
                if !b.items.iter().any(|i| i.id == *id) && !b.delivered.iter().any(|d| d.id == *id) {
                    bail!("nothing queued or delivered with id {id} to wait for");
                }
            }
            let pos = b.items.len();
            let mut item = b.push(&s, &text, after, now);
            item.from = from.cloned();
            b.items[pos].from = from.cloned();
            // On disk before the answer: a queued instruction survives anything after this.
            if let Err(e) = self.save(&b) {
                b.items.retain(|i| i.id != item.id);
                return Err(e);
            }
            item
        };
        // A message between sessions is recorded when it arrives, as their thread has it.
        if from.is_none() {
            let (a, n, x) = (s.address.clone(), s.name.clone(), text.clone());
            h.record(&a, &format!("Queued for {n}: {x}"), "queued");
        }
        // Free now: it goes at once.
        self.work(h, &s.address, Some(&s));
        let b = self.lock();
        if b.delivered.iter().any(|d| d.id == item.id) {
            return Ok((Added::Sent { id: item.id }, s.address, s.name));
        }
        let position = b.items.iter().filter(|j| j.session == s.address && j.parked.is_none() && j.id <= item.id).count();
        Ok((Added::Queued { id: item.id, position: position.max(1), status: s.status.word().to_string() }, s.address, s.name))
    }

    /// The user typed `words` in `session`'s own terminal (at `at_ms`): the
    /// tower's messages waiting for it are checked against them before they
    /// go in, starting now (the session works on the user's words meanwhile).
    pub fn user_said(&self, session: &str, words: &str, at_ms: u64) {
        self.user_said_with(&Herdr, session, words, at_ms)
    }

    fn user_said_with(&self, h: &dyn Sessions, session: &str, words: &str, at_ms: u64) {
        let words = words.trim();
        if words.is_empty() {
            return;
        }
        let now = crate::history::now_ms();
        let check = {
            let mut b = self.lock();
            b.said.retain(|_, l| l.iter().any(|s| now.saturating_sub(s.at_ms) < SAID_KEEP_MS));
            let list = b.said.entry(session.to_string()).or_default();
            list.retain(|s| now.saturating_sub(s.at_ms) < SAID_KEEP_MS);
            list.push(Said { words: words.to_string(), at_ms });
            if list.len() > SAID_KEEP {
                let n = list.len() - SAID_KEEP;
                list.drain(..n);
            }
            b.start_check(session, now)
        };
        if let Some(c) = check {
            eprintln!("[queue] {session}: checking {} waiting message(s) against what the user typed there", c.items.len());
            h.check(c);
        }
    }

    /// The check `c` is done: `covered`, each message the user's words cover
    /// (its id, and the words that cover it); or why it couldn't tell (then
    /// they all go). One covered leaves the queue, not sent.
    pub fn checked(&self, c: &Check, covered: Result<Vec<(u64, String)>>) {
        self.checked_with(&Herdr, c, covered)
    }

    fn checked_with(&self, h: &dyn Sessions, c: &Check, covered: Result<Vec<(u64, String)>>) {
        let upto = c.said.iter().map(|s| s.at_ms).max().unwrap_or(0);
        let covered = covered.unwrap_or_else(|e| {
            eprintln!("[queue] couldn't check {}'s waiting messages against the user's words ({e:#}): they go", c.session);
            vec![]
        });
        {
            let mut b = self.lock();
            if b.checking.get(&c.session).is_some_and(|(_, ids)| c.items.iter().all(|(id, _)| ids.contains(id))) {
                b.checking.remove(&c.session);
            }
            for (id, _) in &c.items {
                let e = b.checked.entry(*id).or_insert(0);
                *e = (*e).max(upto);
            }
            for (id, words) in covered {
                // Only one it asked about, still waiting (not going in this moment).
                if !c.items.iter().any(|(i, _)| *i == id) {
                    continue;
                }
                let Some(pos) = b.items.iter().position(|i| i.id == id && Book::checkable(i)) else { continue };
                let item = b.items.remove(pos);
                let quote = crate::agenda::clip(words.trim(), 160);
                b.checked.remove(&id);
                b.covered.insert(id, quote.clone());
                while b.covered.len() > KEEP_DELIVERED {
                    let Some(first) = b.covered.keys().next().copied() else { break };
                    b.covered.remove(&first);
                }
                eprintln!("[queue] #{id} for {} not sent: what the user typed there covers it", item.session);
                h.record(&item.session, &format!("Not sent: you had told it yourself (\"{quote}\"): {}", item.text), "not-sent");
                if !h.level().quiet() {
                    h.tell(Note {
                        session: item.session.clone(),
                        name: item.name.clone(),
                        msg: format!(
                            "[queue] #{id} for {} ({}) was not sent: the user had told it themselves in its terminal (\"{quote}\"), which covers it. It said: {}",
                            item.session, item.name, item.text
                        ),
                        speech: String::new(),
                    });
                }
            }
            let live: HashSet<u64> = b.items.iter().map(|i| i.id).collect();
            b.checked.retain(|id, _| live.contains(id));
            if let Err(e) = self.save(&b) {
                eprintln!("[queue] {e:#}");
            }
        }
        self.kick();
    }

    /// A session started with its first task (`tower start` from a
    /// dispatch): something later instructions can wait for. Returns its id.
    pub fn track(&self, session: &str, text: &str) -> Result<u64> {
        let mut b = self.lock();
        let id =
            b.next_id.max(b.items.iter().map(|i| i.id).max().unwrap_or(0)).max(b.delivered.iter().map(|d| d.id).max().unwrap_or(0)) + 1;
        b.next_id = id;
        let now = crate::history::now_ms();
        b.delivered(Delivered {
            id,
            session: session.into(),
            text: text.into(),
            at_ms: now,
            turn_before: Some(0),
            ..Default::default()
        });
        self.save(&b)?;
        Ok(id)
    }

    /// A session to start (`tower start <args>`) once the ids in `after`
    /// are done: what a dispatch does for a task that waits on another and
    /// has no session yet. Returns its id (later ones can wait for it too).
    pub fn add_start(&self, name: &str, text: &str, args: &[String], after: &[u64]) -> Result<u64> {
        if after.is_empty() {
            bail!("a start with nothing to wait for: run tower start");
        }
        let mut b = self.lock();
        for id in after {
            if !b.items.iter().any(|i| i.id == *id) && !b.delivered.iter().any(|d| d.id == *id) {
                bail!("nothing queued or delivered with id {id} to wait for");
            }
        }
        let s = Session { address: String::new(), name: name.into(), ..Default::default() };
        let now = crate::history::now_ms();
        let pos = b.items.len();
        let mut item = b.push(&s, text, after, now);
        item.start = args.to_vec();
        b.items[pos].start = args.to_vec();
        if let Err(e) = self.save(&b) {
            b.items.retain(|i| i.id != item.id);
            return Err(e);
        }
        Ok(item.id)
    }

    /// Deferred starts whose prerequisites are done: started now.
    fn start_round(&self, h: &dyn Sessions) {
        let ready: Vec<Item> = {
            let mut b = self.lock();
            let mut ready = vec![];
            let now = crate::history::now_ms();
            let ids: Vec<u64> =
                b.items.iter().filter(|i| !i.start.is_empty() && matches!(i.state(), ItemState::WaitingFor(_) | ItemState::ToStart)).map(|i| i.id).collect();
            for id in ids {
                let Some(pos) = b.items.iter().position(|i| i.id == id) else { continue };
                let item = b.items[pos].clone();
                match b.after_done(&item) {
                    Some(true) => {
                        if b.items[pos].apply(ItemEv::Claim { now_ms: now, turn: None, busy: false }).is_ok() {
                            ready.push(b.items[pos].clone());
                        }
                    }
                    Some(false) => {}
                    None => {
                        let _ = b.items[pos].apply(ItemEv::Park { why: ParkWhy::AfterCancelled, now_ms: now });
                        b.items[pos].gone_told = true;
                        h.tell(Note {
                            session: String::new(),
                            name: item.name.clone(),
                            msg: format!("[queue] #{} (start {}) waited for an instruction that was cancelled: it's parked. `tower queue cancel {}` drops it.", item.id, item.name, item.id),
                            speech: String::new(),
                        });
                    }
                }
            }
            if !ready.is_empty()
                && let Err(e) = self.save(&b)
            {
                eprintln!("[queue] {e:#}");
                for i in b.items.iter_mut().filter(|i| ready.iter().any(|r| r.id == i.id)) {
                    let _ = i.apply(ItemEv::Release);
                }
                return;
            }
            ready
        };
        for item in ready {
            let result = h.start(&item.start);
            let mut b = self.lock();
            let now = crate::history::now_ms();
            match result {
                Ok(address) => {
                    b.items.retain(|i| i.id != item.id);
                    b.delivered(Delivered {
                        id: item.id,
                        session: address.clone(),
                        text: item.text.clone(),
                        at_ms: now,
                        turn_before: Some(0),
                        ..Default::default()
                    });
                    h.record(&address, &format!("Started {} for: {}", item.name, item.text), "delivered");
                    // A start is for the record unless the tower tells everything.
                    if !h.level().quiet() {
                        h.tell(Note {
                            session: address.clone(),
                            name: item.name.clone(),
                            msg: format!(
                                "[queue] started {address} ({}) now that what it waited for is done: {}. The tower watches it.",
                                item.name, item.text
                            ),
                            speech: format!("Started {}.", item.name),
                        });
                    }
                    eprintln!("[queue] started #{} as {address}", item.id);
                }
                Err(e) => {
                    if let Some(i) = b.items.iter_mut().find(|i| i.id == item.id) {
                        let _ = i.apply(ItemEv::Failed);
                        if i.tries >= 3 {
                            let _ = i.apply(ItemEv::Park { why: ParkWhy::StartFailed, now_ms: now });
                            i.gone_told = true;
                            h.tell(Note {
                                session: String::new(),
                                name: item.name.clone(),
                                msg: format!(
                                    "[queue] #{} couldn't start {} ({e:#}): it's parked. `tower queue cancel {}` drops it.",
                                    item.id, item.name, item.id
                                ),
                                speech: format!("I couldn't start {}.", item.name),
                            });
                        }
                    }
                }
            }
            if let Err(e) = self.save(&b) {
                eprintln!("[queue] {e:#}");
            }
        }
    }

    /// What the queue knows about one of its ids (for the agenda ledger).
    pub fn fact(&self, id: u64) -> crate::agenda::QueueFact {
        use crate::agenda::QueueFact;
        let b = self.lock();
        if let Some(i) = b.items.iter().find(|i| i.id == id) {
            return match i.state() {
                ItemState::Claimed { .. } => QueueFact::Claimed,
                ItemState::Parked(why) => QueueFact::Waiting { session: i.session.clone(), parked: Some(why.word().to_string()), start: !i.start.is_empty() },
                _ => QueueFact::Waiting { session: i.session.clone(), parked: None, start: !i.start.is_empty() },
            };
        }
        if let Some(d) = b.delivered.iter().find(|d| d.id == id) {
            // The session's next hand-over after this one: its turn was over by
            // then (not one typed in while it still worked on this one).
            let next_ms = b.delivered.iter().filter(|o| o.session == d.session && o.id != d.id && o.at_ms > d.at_ms && !o.busy).map(|o| o.at_ms).min();
            return QueueFact::Delivered {
                session: d.session.clone(),
                at_ms: d.at_ms,
                turn_before: d.turn_before,
                taken: d.taken,
                finished: d.finished,
                next_ms,
            };
        }
        if let Some(said) = b.covered.get(&id) {
            return QueueFact::Covered { said: said.clone() };
        }
        if b.cancelled.contains(&id) {
            return QueueFact::Cancelled;
        }
        if let Some(i) = b.expired.iter().find(|i| i.id == id) {
            return QueueFact::Expired { why: i.parked.as_ref().map(|p| p.word().to_string()).unwrap_or_default() };
        }
        QueueFact::Unknown
    }

    /// Everything waiting, oldest first.
    pub fn items(&self) -> Vec<Item> {
        self.lock().items.clone()
    }

    /// The delivery `id` was typed in while its session worked (it reads it when that turn ends).
    pub fn typed_while_working(&self, id: u64) -> bool {
        self.lock().delivered.iter().any(|d| d.id == id && d.busy)
    }

    /// Takes one out by hand (the only way one leaves undelivered).
    pub fn cancel(&self, id: u64) -> Result<Item> {
        let mut b = self.lock();
        let Some(pos) = b.items.iter().position(|i| i.id == id) else {
            if b.expired.iter().any(|i| i.id == id) {
                bail!("#{id} expired already (parked for a week); it went nowhere")
            }
            bail!("nothing queued with id {id}")
        };
        if let ItemState::Claimed { .. } = b.items[pos].state() {
            bail!("#{id} is going in right now");
        }
        let item = b.items.remove(pos);
        b.cancelled.insert(id);
        if let Err(e) = self.save(&b) {
            b.items.insert(pos, item);
            return Err(e);
        }
        Ok(item)
    }

    /// Sends a waiting (or parked) instruction to another session instead.
    pub fn retarget(&self, id: u64, session: &str) -> Result<Item> {
        self.retarget_with(&Herdr, id, session)
    }

    fn retarget_with(&self, h: &dyn Sessions, id: u64, session: &str) -> Result<Item> {
        let Look::Live(s) = h.look(session) else { bail!("no live session {session}") };
        let item = {
            let mut b = self.lock();
            // An expired one can be sent after all: back in the queue.
            if let Some(pos) = b.expired.iter().position(|i| i.id == id) {
                let i = b.expired.remove(pos);
                b.items.push(i);
            }
            let Some(i) = b.items.iter_mut().find(|i| i.id == id) else { bail!("nothing queued with id {id}") };
            if let Err(e) = i.apply(ItemEv::Retarget { session: s.address.clone(), name: s.name.clone(), created_ms: s.created_ms }) {
                bail!("#{id}: {e}");
            }
            let item = i.clone();
            // Behind what already waits for that session.
            b.items.retain(|i| i.id != id);
            b.items.push(item.clone());
            self.save(&b)?;
            item
        };
        self.kick();
        Ok(item)
    }

    fn save(&self, b: &Book) -> Result<()> {
        b.save_to(&self.path).map_err(|e| anyhow::anyhow!("saving {}: {e:#}", self.path.display()))
    }

    /// At start: what the last run claimed but may not have typed, and the
    /// notes it still owed the brain.
    fn recover(&self, h: &dyn Sessions) {
        let (claimed, notes): (Vec<Item>, Vec<String>) = {
            let mut b = self.lock();
            (b.items.iter().filter(|i| matches!(i.state(), ItemState::Claimed { .. })).cloned().collect(), std::mem::take(&mut b.notes))
        };
        for n in notes {
            // What went in or started is for the record unless the tower tells everything.
            if h.level().quiet() && (n.starts_with("[queue] delivered") || n.starts_with("[queue] started")) {
                continue;
            }
            h.tell(Note { session: String::new(), name: String::new(), msg: n, speech: String::new() });
        }
        for item in claimed {
            if !item.start.is_empty() {
                // Started before the restart? Its name tells.
                let started = match h.look(&item.name) {
                    Look::Live(s) => Some(s.address),
                    _ => None,
                };
                let mut b = self.lock();
                match started {
                    Some(address) => {
                        b.items.retain(|i| i.id != item.id);
                        b.delivered(Delivered {
                            id: item.id,
                            session: address,
                            text: item.text.clone(),
                            at_ms: crate::history::now_ms(),
                            turn_before: Some(0),
                            ..Default::default()
                        });
                    }
                    None => {
                        if let Some(i) = b.items.iter_mut().find(|i| i.id == item.id) {
                            let _ = i.apply(ItemEv::Release);
                        }
                    }
                }
                let _ = self.save(&b);
                continue;
            }
            let look = h.look(&item.session);
            let moved = match (&look, item.turn_before) {
                (Look::Live(s), Some(before)) => s.turn.is_some_and(|t| t > before),
                _ => false,
            };
            let mut b = self.lock();
            if moved {
                eprintln!("[queue] #{} went in before the restart: not typed again", item.id);
                if let Look::Live(s) = &look {
                    self.went_in(h, &mut b, &item, s, crate::history::now_ms());
                }
            } else if let Some(i) = b.items.iter_mut().find(|i| i.id == item.id) {
                let _ = i.apply(ItemEv::Release);
            }
            if let Err(e) = self.save(&b) {
                eprintln!("[queue] {e:#}");
            }
        }
        self.kick();
    }

    /// One round: each session with something to do, each in its own thread
    /// (a slow machine never holds up the others).
    fn tick(&self, h: &dyn Sessions) {
        self.expire_round(h, crate::history::now_ms());
        self.start_round(h);
        let sessions = self.lock().sessions(crate::history::now_ms());
        thread::scope(|scope| {
            for session in sessions {
                if !self.busy.lock().unwrap_or_else(|e| e.into_inner()).insert(session.clone()) {
                    continue;
                }
                scope.spawn(move || {
                    self.session_round(h, &session);
                    self.busy.lock().unwrap_or_else(|e| e.into_inner()).remove(&session);
                });
            }
        });
    }

    /// Parked a week with nobody retargeting or cancelling them: out of the
    /// queue into `expired` (kept, not deleted), and the brain reminded once.
    /// One parked by an older version (no `parked_ms`) counts from now.
    fn expire_round(&self, h: &dyn Sessions, now: u64) {
        let mut b = self.lock();
        let mut changed = false;
        for i in b.items.iter_mut().filter(|i| i.parked.is_some() && i.parked_ms.is_none()) {
            i.parked_ms = Some(now);
            changed = true;
        }
        let old = |i: &Item| matches!(i.state(), ItemState::Parked(_)) && i.parked_ms.is_some_and(|t| now.saturating_sub(t) >= PARK_EXPIRE_MS);
        if b.items.iter().any(old) {
            let (gone, keep): (Vec<Item>, Vec<Item>) = std::mem::take(&mut b.items).into_iter().partition(old);
            b.items = keep;
            let list: Vec<String> = gone.iter().map(|i| format!("#{} for {} ({}): {}", i.id, i.session, i.parked.as_ref().map(ParkWhy::word).unwrap_or(""), i.text)).collect();
            h.tell(Note {
                session: String::new(),
                name: String::new(),
                msg: format!(
                    "[queue] parked for a week, so out of the queue now (kept as expired): {}. `tower queue retarget <id> <session>` still sends one.",
                    list.join("; ")
                ),
                speech: String::new(),
            });
            eprintln!("[queue] expired {} parked instruction(s)", gone.len());
            b.expired.extend(gone);
            if b.expired.len() > KEEP_DELIVERED {
                let n = b.expired.len() - KEEP_DELIVERED;
                b.expired.drain(..n);
            }
            changed = true;
        }
        if changed && let Err(e) = self.save(&b) {
            eprintln!("[queue] {e:#}");
        }
    }

    /// `add`'s immediate try: the same round, for one session, now.
    fn work(&self, h: &dyn Sessions, session: &str, known: Option<&Session>) {
        // Moving to another Claude account, or waiting for one: it waits (see `session_round`).
        if super::failover::holds(known.map(|s| s.address.as_str()).unwrap_or(session)) {
            return;
        }
        if !self.busy.lock().unwrap_or_else(|e| e.into_inner()).insert(session.to_string()) {
            // The worker is on it this very moment.
            return;
        }
        match known {
            Some(s) => self.deliver(h, s),
            None => self.session_round(h, session),
        }
        self.busy.lock().unwrap_or_else(|e| e.into_inner()).remove(session);
    }

    fn session_round(&self, h: &dyn Sessions, session: &str) {
        // Moving to another Claude account, or waiting for one: what waits for it stays until it runs again.
        if super::failover::holds(session) {
            return;
        }
        let now = crate::history::now_ms();
        match h.look(session) {
            Look::Live(s) => {
                {
                    let mut b = self.lock();
                    b.unreachable.remove(session);
                    let mut changed = b.follow(&s, now);
                    b.working_since(&s, now);
                    // A different terminal at this address: what waited for the old one stays.
                    let replaced: Vec<u64> = b
                        .items
                        .iter()
                        .filter(|i| {
                            i.session == session
                                && !matches!(i.state(), ItemState::Parked(_))
                                && i.created_ms.is_some()
                                && s.created_ms.is_some()
                                && i.created_ms != s.created_ms
                        })
                        .map(|i| i.id)
                        .collect();
                    if !replaced.is_empty() {
                        self.park(h, &mut b, session, ParkWhy::Replaced, &s.name);
                        changed = true;
                    }
                    // Nothing goes into this status; the brain hears it once.
                    if s.status != SessionStatus::Starting && !s.ready() && !s.status.passing() && b.waiting_for(session) > 0 {
                        if b.stuck.get(session).map(String::as_str) != Some(s.status.word()) {
                            b.stuck.insert(session.to_string(), s.status.word().to_string());
                            let n = b.waiting_for(session);
                            h.tell(Note {
                                session: session.into(),
                                name: s.name.clone(),
                                msg: format!(
                                    "[queue] {session} ({}) is {}, with {n} instruction(s) waiting for it: they go in once it's idle or done. Read it (hc read {session}) and tell me if it needs help.",
                                    s.name, s.status
                                ),
                                speech: format!("{} is {}, with instructions waiting.", s.name, s.status),
                            });
                            changed = true;
                        }
                    } else if b.stuck.remove(session).is_some() {
                        changed = true;
                    }
                    if changed && let Err(e) = self.save(&b) {
                        eprintln!("[queue] {e:#}");
                    }
                }
                self.deliver(h, &s);
            }
            Look::Finished | Look::Gone => {
                let mut b = self.lock();
                let name = b.items.iter().find(|i| i.session == session).map(|i| i.name.clone()).unwrap_or_else(|| session.to_string());
                // What went in to it is over, one way or another.
                b.working.remove(session);
                for d in b.delivered.iter_mut().filter(|d| d.session == session && !d.finished) {
                    d.finish();
                }
                self.park(h, &mut b, session, ParkWhy::Gone, &name);
                let _ = self.save(&b);
            }
            Look::Unreachable(e) => {
                let mut b = self.lock();
                let waiting = b.waiting_for(session);
                let entry = b.unreachable.entry(session.to_string()).or_insert((now, false));
                if waiting > 0 && !entry.1 && now.saturating_sub(entry.0) >= UNREACHABLE_TELL_MS {
                    entry.1 = true;
                    h.tell(Note {
                        session: session.into(),
                        name: session.into(),
                        msg: format!(
                            "[queue] can't reach {session} for ten minutes ({e}); its {waiting} instruction(s) keep waiting for it."
                        ),
                        speech: format!("I can't reach {session}; its instructions keep waiting."),
                    });
                    let _ = self.save(&b);
                }
            }
        }
    }

    /// The session's next instruction, if it may take one now; or, while it
    /// works, a message from another session that may be typed in anyway
    /// (`Book::busy_pick`).
    fn deliver(&self, h: &dyn Sessions, s: &Session) {
        let now = crate::history::now_ms();
        let (item, busy) = {
            let mut b = self.lock();
            let (pos, busy) = if b.may_send(s, now) {
                let Some(pos) = b.items.iter().position(|i| i.session == s.address && !matches!(i.state(), ItemState::Parked(_))) else { return };
                (pos, false)
            } else {
                // Only a Claude Code session, working, is worth the setting's read.
                let minutes = if s.claude() && s.status == SessionStatus::Working && b.items.iter().any(|i| i.session == s.address && i.from.is_some()) {
                    h.busy_minutes()
                } else {
                    0
                };
                match b.busy_pick(s, now, minutes) {
                    Some(pos) => (pos, true),
                    None => return,
                }
            };
            let item = b.items[pos].clone();
            match b.after_done(&item) {
                Some(true) => {}
                Some(false) => return,
                None if item.from.is_some() => {
                    // A message between sessions: back to its sender, nobody else.
                    self.bounce(h, &mut b, &|i| i.id == item.id, "what it waited for was cancelled");
                    let _ = self.save(&b);
                    return;
                }
                None => {
                    let _ = b.items[pos].apply(ItemEv::Park { why: ParkWhy::AfterCancelled, now_ms: now });
                    b.items[pos].gone_told = true;
                    h.tell(Note {
                        session: s.address.clone(),
                        name: s.name.clone(),
                        msg: format!(
                            "[queue] #{} for {} ({}) waited for an instruction that was cancelled: it's parked. `tower queue retarget {} {}` sends it anyway, `tower queue cancel {}` drops it.",
                            item.id, s.address, s.name, item.id, s.address, item.id
                        ),
                        speech: String::new(),
                    });
                    let _ = self.save(&b);
                    return;
                }
            }
            // Claimed: being typed this moment (or a restart's recover has it).
            if let ItemState::Claimed { .. } = item.state() {
                return;
            }
            if let (Some(before), Some(turn)) = (item.turn_before, s.turn)
                && turn > before
            {
                eprintln!("[queue] #{} went in on an earlier try", item.id);
                self.went_in(h, &mut b, &item, s, now);
                let _ = self.save(&b);
                return;
            }
            // The user typed in this session themselves lately: it waits until
            // their words are checked against it (they may say it already).
            if !busy && Book::checkable(&item) {
                if b.held_by_check(&s.address, item.id, now) {
                    return;
                }
                if let Some(c) = b.start_check(&s.address, now) {
                    drop(b);
                    eprintln!("[queue] {}: checking {} waiting message(s) against what the user typed there", s.address, c.items.len());
                    h.check(c);
                    return;
                }
                if b.said.contains_key(&s.address) && s.status_ms.is_some_and(|t| t <= now && now - t < SETTLE_MS) {
                    return;
                }
            }
            // Claimed, on disk, before it's typed.
            if let Err(e) = b.items[pos].apply(ItemEv::Claim { now_ms: now, turn: s.turn, busy }) {
                eprintln!("[queue] not sending #{}: {e}", item.id);
                return;
            }
            if let Err(e) = self.save(&b) {
                let _ = b.items[pos].apply(ItemEv::Release);
                // Not typed: it may be typed in while it works another time.
                b.items[pos].busy = false;
                eprintln!("[queue] not sending #{}: {e:#}", item.id);
                return;
            }
            (b.items[pos].clone(), busy)
        };
        let result = h.send(&s.address, &item.text, busy);
        let mut b = self.lock();
        match result {
            Ok(()) => {
                self.went_in(h, &mut b, &item, s, now);
                if busy {
                    let mins = s.status_ms.map(|t| now.saturating_sub(t) / 60_000).unwrap_or(0);
                    eprintln!("[queue] typed #{} into {} while it works ({mins} min so far): it reads it when that turn ends", item.id, s.address);
                } else {
                    eprintln!("[queue] delivered #{} to {}", item.id, s.address);
                }
            }
            Err(e) => {
                if let Some(i) = b.items.iter_mut().find(|i| i.id == item.id) {
                    let _ = i.apply(ItemEv::Failed);
                    if i.tries == 1 || i.tries.is_multiple_of(20) {
                        eprintln!("[queue] #{} to {} didn't go in ({e:#}); trying again", item.id, s.address);
                    }
                }
            }
        }
        if let Err(e) = self.save(&b) {
            // It went in; the next start sees the turn count moved and won't type it again.
            eprintln!("[queue] {e:#}");
        }
    }

    /// It went in: out of the queue, remembered, watched, told.
    fn went_in(&self, h: &dyn Sessions, b: &mut Book, item: &Item, s: &Session, now: u64) {
        b.items.retain(|i| i.id != item.id);
        b.delivered(Delivered {
            id: item.id,
            session: s.address.clone(),
            text: item.text.clone(),
            at_ms: now,
            turn_before: item.turn_before,
            busy: item.busy,
            ..Default::default()
        });
        if item.quiet {
            // A note, not work: nothing to watch or tell.
            h.record(&s.address, &format!("Told {}: {}", s.name, item.text), "note");
            return;
        }
        if let Some(from) = &item.from {
            // A message between sessions, not work: into their thread, and
            // nothing to watch, no task, no word to any brain.
            h.peer(crate::peers::Message {
                at_ms: now,
                from: from.clone(),
                to: s.address.clone(),
                to_name: s.name.clone(),
                space: None,
                text: crate::peers::unsigned(from, &item.text).to_string(),
                delivered: true,
            });
            return;
        }
        let left = b.waiting_for(&s.address);
        h.watch(&s.address, &item.text);
        h.record(&s.address, &format!("Delivered to {}: {}", s.name, item.text), "delivered");
        // A delivery is for the record unless the tower tells everything.
        if h.level().quiet() {
            return;
        }
        h.tell(Note {
            session: s.address.clone(),
            name: s.name.clone(),
            msg: format!(
                "[queue] delivered to {} ({}): {}{}. The tower watches it: you'll get a [watch] message when it stops.",
                s.address,
                s.name,
                item.text.trim_end_matches('.'),
                if left > 0 { format!(" ({left} more queued for it)") } else { String::new() }
            ),
            speech: format!("{} has the queued instruction.", s.name),
        });
    }

    /// What waits for a session that ended (or whose address is another
    /// terminal's now) goes nowhere by itself; the brain hears it once.
    fn park(&self, h: &dyn Sessions, b: &mut Book, session: &str, why: ParkWhy, name: &str) {
        // A note for a session that's gone is moot: dropped, not parked.
        b.items.retain(|i| !(i.session == session && i.quiet && matches!(i.state(), ItemState::Waiting | ItemState::WaitingFor(_))));
        // A message from another session: its sender hears it didn't arrive, nobody else.
        let gone = if why == ParkWhy::Replaced { "its terminal is another session's now" } else { "it has ended" };
        self.bounce(h, b, &|i| i.session == session && matches!(i.state(), ItemState::Waiting | ItemState::WaitingFor(_)), gone);
        let mut ids = vec![];
        let mut untold = false;
        let now = crate::history::now_ms();
        for i in b.items.iter_mut().filter(|i| i.session == session && matches!(i.state(), ItemState::Waiting | ItemState::WaitingFor(_))) {
            // A claimed one isn't parked: it went in, or waits again first.
            if i.apply(ItemEv::Park { why: why.clone(), now_ms: now }).is_err() {
                continue;
            }
            untold |= !i.gone_told;
            i.gone_told = true;
            ids.push(format!("#{}", i.id));
        }
        if ids.is_empty() || !untold {
            return;
        }
        let what = if why == ParkWhy::Replaced { "is a different terminal now" } else { "is gone" };
        h.tell(Note {
            session: session.into(),
            name: name.into(),
            msg: format!(
                "[queue] {session} ({name}) {what}, with {} instruction(s) queued for it ({}): they won't go anywhere on their own. `tower queue retarget <id> <session>` sends one to another session; `tower queue cancel <id>` drops it.",
                ids.len(),
                ids.join(", ")
            ),
            speech: format!("{name} {what}; its queued instructions wait for you."),
        });
    }

    /// Messages between sessions (`which` of them) that can't be delivered:
    /// out of the queue, kept in the record as not delivered, and each
    /// sender told once, with one queued note. The user and the brain are
    /// not told. True when there were any.
    fn bounce(&self, h: &dyn Sessions, b: &mut Book, which: &dyn Fn(&Item) -> bool, why: &str) -> bool {
        let (lost, keep): (Vec<Item>, Vec<Item>) = std::mem::take(&mut b.items).into_iter().partition(|i| i.from.is_some() && which(i));
        b.items = keep;
        let now = crate::history::now_ms();
        // One note per sender and receiver, however many messages it lost.
        let mut told: Vec<(crate::peers::Sender, String, String, Vec<String>)> = vec![];
        for item in &lost {
            let Some(from) = item.from.clone() else { continue };
            let text = crate::peers::unsigned(&from, &item.text).to_string();
            h.peer(crate::peers::Message {
                at_ms: now,
                from: from.clone(),
                to: item.session.clone(),
                to_name: item.name.clone(),
                space: None,
                text: text.clone(),
                delivered: false,
            });
            eprintln!("[queue] #{} from {} to {} wasn't delivered ({why}): its sender is told", item.id, from.address, item.session);
            match told.iter_mut().find(|(f, to, ..)| f.address == from.address && *to == item.session) {
                Some((.., texts)) => texts.push(text),
                None => told.push((from, item.session.clone(), item.name.clone(), vec![text])),
            }
        }
        for (from, to, to_name, texts) in told {
            let note = hc::clean(&undelivered_note(super::brains::name_at(&from.address).is_some(), &to_name, &to, why, &texts));
            if b.items.iter().any(|i| i.session == from.address && i.text == note) {
                continue;
            }
            let s = Session { address: from.address.clone(), name: from.name.clone(), ..Default::default() };
            let pos = b.items.len();
            b.push(&s, &note, &[], now);
            b.items[pos].quiet = true;
        }
        !lost.is_empty()
    }
}

/// What a sender is told when its messages to `to` weren't delivered. To a
/// brain (one tower writing to another) it is a `[queue]` note, so its turn
/// on it is a note turn: the user isn't told of a lost message between
/// sessions, by a brain's reply no more than by the tower.
fn undelivered_note(to_brain: bool, to_name: &str, to: &str, why: &str, texts: &[String]) -> String {
    let tag = if to_brain { "[queue]" } else { "[tower]" };
    let said: String = texts.first().map(|t| t.chars().take(200).collect()).unwrap_or_default();
    match texts.len() {
        0 | 1 => format!("{tag} Your message to {to_name} ({to}) was not delivered: {why}. It said: \"{said}\". Nothing was sent in its place."),
        n => format!("{tag} Your {n} messages to {to_name} ({to}) were not delivered: {why}. The first said: \"{said}\". Nothing was sent in their place."),
    }
}

/// A live session's facts, as the queue needs them.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub address: String,
    pub name: String,
    pub status: SessionStatus,
    pub created_ms: Option<u64>,
    pub turn: Option<u64>,
    /// The agent program Herdr reports for it (`claude-code`, `codex`).
    pub app: Option<String>,
    /// When Herdr set its status.
    pub status_ms: Option<u64>,
    /// It is a brain (the main one, a space's own).
    pub brain: bool,
}

impl Session {
    /// It takes an instruction now: idle or done; or, a Claude Code or
    /// Codex session, `error` (its turn ended on an API error, and it sits
    /// at its prompt; the radar says it "hit an error").
    pub fn ready(&self) -> bool {
        self.status.ready() || (self.status == SessionStatus::Error && matches!(self.app.as_deref(), Some("claude-code" | "codex")))
    }

    /// A Claude Code session: what is typed while it works, it keeps for its next message.
    pub fn claude(&self) -> bool {
        self.app.as_deref() == Some("claude-code")
    }
}

/// What Herdr says about a session.
#[derive(Debug, Clone)]
pub enum Look {
    Live(Session),
    /// The terminal is there, but its program ended.
    Finished,
    Gone,
    Unreachable(String),
}

/// Herdr, the brain and the radar, behind a seam the tests replace.
trait Sessions: Sync {
    fn look(&self, session: &str) -> Look;
    /// Types `text` in. `busy`: while the session works (only if it still does).
    fn send(&self, session: &str, text: &str, busy: bool) -> Result<()>;
    /// A note for the brain, through the radar's outbox.
    fn tell(&self, note: Note);
    /// Watch the session: the brain hears when it stops.
    fn watch(&self, session: &str, text: &str);
    fn record(&self, session: &str, text: &str, kind: &str);
    /// Starts a session (`tower start <args>`); its address.
    fn start(&self, args: &[String]) -> Result<String>;
    /// A message between sessions arrived, or couldn't: into the history (kind `peer`).
    fn peer(&self, msg: crate::peers::Message);
    /// How much the tower tells the user now (see `tell`).
    fn level(&self) -> crate::tell::Level;
    /// `[queue] busy_deliver_minutes`, as config.toml says now (0: never).
    fn busy_minutes(&self) -> u64;
    /// Checks what the user typed in a session against what waits for it
    /// (`covered::judge`), and hands the answer to `SendQueue::checked`.
    fn check(&self, c: Check);
}

/// How the daemon hears what the queue does: set once at its start.
pub struct Wiring {
    pub tell: Box<dyn Fn(Note) + Send + Sync>,
    pub watch: Box<dyn Fn(&str, &str) + Send + Sync>,
    /// A message between sessions was recorded (its history entry): for the state's threads.
    pub peer: Box<dyn Fn(&crate::history::Entry) + Send + Sync>,
}

static WIRING: OnceLock<Wiring> = OnceLock::new();

/// The daemon's queue, for the radar's agent list.
static RUNNING: OnceLock<SendQueue> = OnceLock::new();

/// Everything waiting in the daemon's queue (for the phone API).
pub fn current_items() -> Vec<Item> {
    RUNNING.get().map(|q| q.items()).unwrap_or_default()
}

/// The daemon's queue typed a message into `session` while it worked, and
/// it hasn't taken it yet: its next prompt (when that turn ends) is that one.
pub fn typed_in_while_working(session: &str) -> bool {
    RUNNING.get().is_some_and(|q| q.lock().last_delivery(session).is_some_and(|d| d.busy && !d.finished))
}

/// How many instructions wait for each session (not parked).
pub fn waiting_counts() -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    if let Some(q) = RUNNING.get() {
        for i in q.lock().items.iter().filter(|i| !matches!(i.state(), ItemState::Parked(_)) && i.start.is_empty()) {
            *out.entry(i.session.clone()).or_insert(0) += 1;
        }
    }
    out
}

/// What waits for each session, in order (sessions to start aside).
pub fn waiting_lines() -> BTreeMap<String, Vec<super::state::WaitingLine>> {
    RUNNING.get().map(|q| q.lock().lines()).unwrap_or_default()
}

pub fn wire(w: Wiring) {
    let _ = WIRING.set(w);
}

struct Herdr;

impl Sessions for Herdr {
    fn look(&self, session: &str) -> Look {
        match hc::lookup(session) {
            hc::Lookup::Found(t) if t["state"] == "live" => Look::Live(session_of(&t, session)),
            hc::Lookup::Found(_) => Look::Finished,
            hc::Lookup::Gone => Look::Gone,
            hc::Lookup::Unreachable(e) => Look::Unreachable(e),
        }
    }
    fn send(&self, session: &str, text: &str, busy: bool) -> Result<()> {
        // Written down before it is typed: the prompt its hook reports is the
        // tower's, not the user's (typed in while it works: when that turn ends).
        let id = super::typed::note_as(session, text, "queue", busy);
        // While it works: only if it still does as hc types (never into a dialog: hc refuses one).
        let sent = hc::send_if(session, text, busy.then_some("working"));
        super::typed::done(id, sent.is_ok());
        sent
    }
    fn tell(&self, note: Note) {
        match WIRING.get() {
            Some(w) => (w.tell)(note),
            None => {
                let _ = crate::brain::note(&note.msg);
            }
        }
    }
    fn watch(&self, session: &str, text: &str) {
        // Setting a watch asks hc about the session: not while the queue's book is held.
        let (s, t) = (session.to_string(), text.to_string());
        thread::spawn(move || {
            if let Some(w) = WIRING.get() {
                (w.watch)(&s, &t);
            }
        });
    }
    fn record(&self, session: &str, text: &str, kind: &str) {
        let (s, x, k) = (session.to_string(), text.to_string(), kind.to_string());
        crate::history::later(move || crate::history::record_session("queue", &s, &x, Some(&k)));
    }
    fn start(&self, args: &[String]) -> Result<String> {
        let mut cmd = std::process::Command::new(config::tower_bin());
        cmd.arg("start").args(args);
        let out = hc::output_within(cmd, "tower start", Duration::from_secs(180))?;
        if out.code != 0 {
            bail!("{}{}", out.stderr.trim(), out.stdout.trim());
        }
        // Piped, it prints JSON with the address.
        let v: Value = serde_json::from_str(out.stdout.trim()).unwrap_or_default();
        match v["address"].as_str() {
            Some(a) => Ok(a.to_string()),
            None => bail!("tower start said: {}", out.stdout.trim()),
        }
    }
    fn peer(&self, mut msg: crate::peers::Message) {
        // Herdr is asked for the receiver's space: on the history's thread, not while the book is held.
        crate::history::later(move || {
            if msg.space.is_none() {
                msg.space = hc::get(&msg.to).as_ref().and_then(crate::spaces::space_of);
            }
            let e = msg.entry();
            crate::history::append(e.clone());
            if let Some(w) = WIRING.get() {
                (w.peer)(&e);
            }
        });
    }
    fn level(&self) -> crate::tell::Level {
        crate::tell::level()
    }
    fn busy_minutes(&self) -> u64 {
        config::Config::live().queue.busy_deliver_minutes
    }
    fn check(&self, c: Check) {
        // A model reads it: never on the queue's own thread.
        thread::spawn(move || {
            let covered = crate::covered::judge(&c);
            if let Some(q) = RUNNING.get() {
                q.checked(&c, covered);
            }
        });
    }
}

fn session_of(t: &Value, asked: &str) -> Session {
    Session {
        address: t["address"].as_str().unwrap_or(asked).to_string(),
        name: hc::spoken_name(t),
        status: hc::status(t),
        created_ms: hc::created_ms(t),
        turn: hc::turn(t),
        app: hc::agent_app(t),
        status_ms: t["status_ms"].as_u64(),
        brain: crate::brain::brain_of(t).is_some(),
    }
}

/// One line per instruction waiting, for `tower queue list`.
pub fn describe(items: &[Item], status_of: impl Fn(&str) -> Option<String>) -> Vec<String> {
    items
        .iter()
        .map(|i| {
            let status = match i.state() {
                ItemState::Parked(why) => format!("parked: {why}"),
                ItemState::Claimed { .. } => "going in now".into(),
                ItemState::ToStart => "starts when it may".into(),
                ItemState::WaitingFor(_) if i.session.is_empty() => "starts when it may".into(),
                ItemState::WaitingFor(_) | ItemState::Waiting => status_of(&i.session).unwrap_or_else(|| "gone".into()),
            };
            let ago = crate::phone::ago(i.at_ms);
            let after = if i.after.is_empty() {
                String::new()
            } else {
                format!(", after {}", i.after.iter().map(|a| format!("#{a}")).collect::<Vec<_>>().join(" "))
            };
            let from = i.from.as_ref().map(|f| format!(", from {}", f.address)).unwrap_or_default();
            format!("#{} → {} ({}, {status}{after}{from}), queued {ago}: {}", i.id, i.session, i.name, i.text)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        sessions: Mutex<BTreeMap<String, (String, Option<u64>, Option<u64>)>>,
        unreachable: Mutex<bool>,
        sent: Mutex<Vec<(String, String)>>,
        notes: Mutex<Vec<String>>,
        watched: Mutex<Vec<String>>,
        send_fails: Mutex<bool>,
        /// What went into the history: (session, text, kind).
        recorded: Mutex<Vec<(String, String, String)>>,
        peers: Mutex<Vec<crate::peers::Message>>,
        level: Mutex<crate::tell::Level>,
        /// Each session's agent program (none: a session of no program the queue tells apart).
        apps: Mutex<BTreeMap<String, String>>,
        /// Herdr's `status_ms` for each session.
        since: Mutex<BTreeMap<String, u64>>,
        /// `[queue] busy_deliver_minutes`.
        minutes: Mutex<u64>,
        /// What was typed in while its session worked.
        busy_sent: Mutex<Vec<String>>,
        /// The checks of the user's words asked for.
        checks: Mutex<Vec<Check>>,
    }
    /// A tower that tells everything, as it always did: what these tests
    /// were written for. The quiet levels are set by the tests about them.
    impl Default for Fake {
        fn default() -> Fake {
            Fake::at(crate::tell::Level::Everything)
        }
    }
    impl Fake {
        fn at(level: crate::tell::Level) -> Fake {
            Fake {
                sessions: Default::default(),
                unreachable: Default::default(),
                sent: Default::default(),
                notes: Default::default(),
                watched: Default::default(),
                send_fails: Default::default(),
                recorded: Default::default(),
                peers: Default::default(),
                level: Mutex::new(level),
                apps: Default::default(),
                since: Default::default(),
                minutes: Mutex::new(10),
                busy_sent: Default::default(),
                checks: Default::default(),
            }
        }
        /// `s` runs `app` (`claude-code`, `codex`), its status set at `since_ms`.
        fn runs(&self, s: &str, app: &str, since_ms: u64) {
            self.apps.lock().unwrap().insert(s.into(), app.into());
            self.since.lock().unwrap().insert(s.into(), since_ms);
        }
        fn recorded(&self, kind: &str) -> Vec<String> {
            self.recorded.lock().unwrap().iter().filter(|(_, _, k)| k == kind).map(|(_, t, _)| t.clone()).collect()
        }
        fn peers(&self) -> Vec<crate::peers::Message> {
            self.peers.lock().unwrap().clone()
        }
        fn set(&self, s: &str, st: &str) {
            let mut all = self.sessions.lock().unwrap();
            let e = all.entry(s.into()).or_insert((String::new(), Some(100), Some(0)));
            e.0 = st.into();
        }
        /// The session took a turn (Herdr counts it).
        fn turn(&self, s: &str) {
            let mut all = self.sessions.lock().unwrap();
            if let Some(e) = all.get_mut(s) {
                e.2 = e.2.map(|t| t + 1);
            }
        }
        fn replace(&self, s: &str, created: u64) {
            self.sessions.lock().unwrap().insert(s.into(), ("idle".into(), Some(created), Some(0)));
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().iter().map(|(_, t)| t.clone()).collect()
        }
        fn notes(&self) -> Vec<String> {
            self.notes.lock().unwrap().clone()
        }
    }
    impl Sessions for Fake {
        fn look(&self, session: &str) -> Look {
            if *self.unreachable.lock().unwrap() {
                return Look::Unreachable("ssh: timed out".into());
            }
            match self.sessions.lock().unwrap().get(session).cloned() {
                Some((st, created, turn)) => Look::Live(Session {
                    address: session.into(),
                    name: format!("{session}-name"),
                    status: SessionStatus::parse(&st),
                    created_ms: created,
                    turn,
                    app: self.apps.lock().unwrap().get(session).cloned(),
                    status_ms: self.since.lock().unwrap().get(session).copied(),
                    brain: false,
                }),
                None => Look::Gone,
            }
        }
        fn send(&self, session: &str, text: &str, busy: bool) -> Result<()> {
            if *self.send_fails.lock().unwrap() {
                bail!("herdr is down");
            }
            self.sent.lock().unwrap().push((session.into(), text.into()));
            if busy {
                self.busy_sent.lock().unwrap().push(text.into());
            }
            Ok(())
        }
        fn tell(&self, note: Note) {
            self.notes.lock().unwrap().push(note.msg);
        }
        fn watch(&self, session: &str, _: &str) {
            self.watched.lock().unwrap().push(session.into());
        }
        fn record(&self, session: &str, text: &str, kind: &str) {
            self.recorded.lock().unwrap().push((session.into(), text.into(), kind.into()));
        }
        fn start(&self, args: &[String]) -> Result<String> {
            let name = args.iter().skip_while(|a| *a != "--name").nth(1).cloned().unwrap_or_default();
            self.sent.lock().unwrap().push((format!("start:{name}"), args.last().cloned().unwrap_or_default()));
            self.sessions.lock().unwrap().insert(name.clone(), ("working".into(), Some(7), Some(1)));
            Ok(name)
        }
        fn peer(&self, msg: crate::peers::Message) {
            self.peers.lock().unwrap().push(msg);
        }
        fn level(&self) -> crate::tell::Level {
            *self.level.lock().unwrap()
        }
        fn busy_minutes(&self) -> u64 {
            *self.minutes.lock().unwrap()
        }
        fn check(&self, c: Check) {
            self.checks.lock().unwrap().push(c);
        }
    }

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn queue() -> (SendQueue, Dir) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("tower-sendq-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (SendQueue::at(dir.join("queue.json")), Dir(dir))
    }

    /// Lets the gap after a hand-over pass (not the wait for it to be taken).
    fn settle(q: &SendQueue) {
        let then = crate::history::now_ms() - GAP_MS - 1;
        for d in &mut q.lock().delivered {
            d.at_ms = d.at_ms.min(then);
        }
    }

    #[test]
    fn a_lost_message_between_towers_is_a_note_not_something_to_tell_the_user() {
        let texts = vec!["take the release notes from here".to_string()];
        // A session is told in the tower's own name, as before.
        let to_session = undelivered_note(false, "api", "t5", "its session has ended", &texts);
        assert_eq!(to_session, "[tower] Your message to api (t5) was not delivered: its session has ended. It said: \"take the release notes from here\". Nothing was sent in its place.");
        // A brain's turn on it must be a note turn: what it writes then is neither shown nor said.
        let to_brain = undelivered_note(true, "tower sandbox", "t12", "its session has ended", &texts);
        assert!(crate::brain::is_note_prompt(&to_brain) && !crate::brain::is_note_prompt(&to_session), "{to_brain}");
        let two = undelivered_note(true, "tower sandbox", "t12", "it is gone", &[texts[0].clone(), "and the changelog".into()]);
        assert!(two.starts_with("[queue] Your 2 messages to tower sandbox (t12) were not delivered: it is gone. The first said:"), "{two}");
    }

    /// What the user typed in a session themselves is checked against what
    /// the tower has waiting for it; what their words cover is not sent.
    #[test]
    fn what_the_user_already_told_a_session_is_not_sent_again() {
        use crate::agenda::QueueFact;
        let (q, _d) = queue();
        let h = Fake::default();
        h.set("t8w", "working");
        let (Added::Queued { id: note, .. }, ..) = q.add_with(&h, "t8w", "PR #787 (eyzhub/eyzflow) into dev was merged on GitHub.", &[], None).unwrap() else { panic!("it works") };
        let (Added::Queued { id: other, .. }, ..) = q.add_with(&h, "t8w", "Write the changelog for #787", &[], None).unwrap() else { panic!("it works") };
        // A message from another session, and a quiet note: never held for the user's words.
        let peer = crate::peers::Sender { address: "t91".into(), name: "reviewer".into(), space: None };
        q.add_with(&h, "t8w", "Reviewed #787: nothing critical.", &[], Some(&peer)).unwrap();
        let now = crate::history::now_ms();
        q.user_said_with(&h, "t8w", "merged on dev", now);
        // The check starts at once, while the session works on the user's words.
        let checks = h.checks.lock().unwrap().clone();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].items.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![note, other]);
        assert_eq!(checks[0].said, vec![Said { words: "merged on dev".into(), at_ms: now }]);
        // The turn ends before the check answers: nothing it holds goes in.
        h.turn("t8w");
        h.set("t8w", "idle");
        q.tick(&h);
        assert!(h.sent().is_empty(), "held for the check: {:?}", h.sent());
        assert_eq!(h.checks.lock().unwrap().len(), 1, "one check at a time");
        // It answers: the merge note is covered, the changelog isn't.
        q.checked_with(&h, &checks[0], Ok(vec![(note, "merged on dev".into())]));
        assert_eq!(q.fact(note), QueueFact::Covered { said: "merged on dev".into() });
        assert_eq!(h.recorded("not-sent"), vec!["Not sent: you had told it yourself (\"merged on dev\"): PR #787 (eyzhub/eyzflow) into dev was merged on GitHub.".to_string()]);
        assert!(h.notes().iter().any(|n| n.starts_with(&format!("[queue] #{note} for t8w (t8w-name) was not sent"))), "{:?}", h.notes());
        q.tick(&h);
        assert_eq!(h.sent(), vec!["Write the changelog for #787".to_string()]);
        // Checked once: the same words don't hold what is left again.
        assert_eq!(h.checks.lock().unwrap().len(), 1);
    }

    /// The tower's message queued a little after the user typed the same
    /// thing (a merge seen on GitHub after the user said "merged"): checked too.
    #[test]
    fn a_message_queued_just_after_the_users_words_is_checked_before_it_goes() {
        let (q, _d) = queue();
        let h = Fake::default();
        h.set("t8w", "idle");
        let now = crate::history::now_ms();
        q.user_said_with(&h, "t8w", "merged on dev", now - 12_000);
        assert!(h.checks.lock().unwrap().is_empty(), "nothing waits yet");
        let (Added::Queued { id, .. }, ..) = q.add_with(&h, "t8w", "PR #787 (eyzhub/eyzflow) into dev was merged on GitHub.", &[], None).unwrap() else { panic!("held for the check") };
        assert!(h.sent().is_empty());
        let c = h.checks.lock().unwrap()[0].clone();
        // Not covered after all (or the check failed): it goes.
        q.checked_with(&h, &c, Err(anyhow::anyhow!("claude: timed out")));
        q.tick(&h);
        assert_eq!(h.sent().len(), 1);
        assert!(matches!(q.fact(id), crate::agenda::QueueFact::Delivered { .. }));
        // Words from long before a message are not about it.
        let (q, _d) = queue();
        let h = Fake::default();
        h.set("t5", "idle");
        q.user_said_with(&h, "t5", "merged on dev", crate::history::now_ms() - SAID_BEFORE_MS - 60_000);
        q.add_with(&h, "t5", "PR #9 (a/b) into dev was merged on GitHub.", &[], None).unwrap();
        assert_eq!(h.sent().len(), 1);
        assert!(h.checks.lock().unwrap().is_empty());
    }

    /// What the user typed while the session worked goes in the moment its
    /// turn ends: the tower's message waits a moment for it.
    #[test]
    fn a_session_the_user_types_in_settles_before_the_towers_message() {
        let (q, _d) = queue();
        let h = Fake::default();
        h.set("t5", "working");
        let now = crate::history::now_ms();
        q.user_said_with(&h, "t5", "run the tests", now - 60_000);
        q.add_with(&h, "t5", "deploy the docs", &[], None).unwrap();
        // Its turn ends: checked against the user's words first.
        h.set("t5", "idle");
        h.since.lock().unwrap().insert("t5".into(), now - 500);
        q.tick(&h);
        let c = h.checks.lock().unwrap()[0].clone();
        q.checked_with(&h, &c, Ok(vec![]));
        // Not covered, but its turn ended a moment ago: not yet.
        q.tick(&h);
        assert!(h.sent().is_empty());
        h.since.lock().unwrap().insert("t5".into(), now - SETTLE_MS - 10);
        q.tick(&h);
        assert_eq!(h.sent(), vec!["deploy the docs".to_string()]);
    }

    #[test]
    fn a_check_that_never_answers_lets_the_messages_go() {
        let (q, _d) = queue();
        let h = Fake::default();
        h.set("t5", "working");
        q.add_with(&h, "t5", "deploy the docs", &[], None).unwrap();
        q.user_said_with(&h, "t5", "also fix the footer", crate::history::now_ms());
        h.set("t5", "idle");
        q.tick(&h);
        assert!(h.sent().is_empty());
        // Two minutes on, no answer: it goes, and isn't checked against those words again.
        q.lock().checking.get_mut("t5").unwrap().0 -= CHECK_LIMIT_MS;
        q.tick(&h);
        assert_eq!(h.sent(), vec!["deploy the docs".to_string()]);
        assert_eq!(h.checks.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_ledger_reads_each_id_as_the_queue_knows_it() {
        use crate::agenda::QueueFact;
        let (q, _d) = queue();
        let h = Fake::default();
        h.set("t1", "idle");
        h.set("t2", "working");
        let (Added::Sent { id: sent }, ..) = q.add_with(&h, "t1", "first", &[], None).unwrap() else { panic!("t1 was free") };
        let (Added::Queued { id: waiting, .. }, ..) = q.add_with(&h, "t2", "second", &[], None).unwrap() else { panic!("t2 works") };
        let (Added::Queued { id: dropped, .. }, ..) = q.add_with(&h, "t2", "third", &[], None).unwrap() else { panic!("t2 works") };
        assert!(matches!(q.fact(sent), QueueFact::Delivered { ref session, next_ms: None, .. } if session == "t1"));
        assert_eq!(q.fact(waiting), QueueFact::Waiting { session: "t2".into(), parked: None, start: false });
        q.cancel(dropped).unwrap();
        assert_eq!(q.fact(dropped), QueueFact::Cancelled);
        assert_eq!(q.fact(9999), QueueFact::Unknown);
        // A second hand-over to t1: the first one's turn was over by then.
        h.turn("t1");
        settle(&q);
        q.session_round(&h, "t1");
        let (Added::Sent { .. }, ..) = q.add_with(&h, "t1", "fourth", &[], None).unwrap() else { panic!("t1 is free again") };
        assert!(matches!(q.fact(sent), QueueFact::Delivered { next_ms: Some(_), .. }));
        // t2 ends: what waits for it is parked, and the ledger hears why.
        h.sessions.lock().unwrap().remove("t2");
        q.session_round(&h, "t2");
        assert_eq!(q.fact(waiting), QueueFact::Waiting { session: "t2".into(), parked: Some("gone".into()), start: false });
    }

    #[test]
    fn a_note_waits_for_a_working_session_and_is_neither_watched_nor_told() {
        let (q, _d) = queue();
        let h = Fake::default();
        h.set("t1", "working");
        let (Added::Queued { id, .. }, ..) = q.add_note_with(&h, "t1", "the others in this space", false).unwrap() else { panic!("t1 works") };
        assert!(h.sent().is_empty(), "never typed into a working session");
        // The same note again: the same item.
        let (Added::Queued { id: again, .. }, ..) = q.add_note_with(&h, "t1", "the others in this space", false).unwrap() else { panic!() };
        assert_eq!(id, again);
        assert!(q.items()[0].quiet);
        h.set("t1", "idle");
        q.session_round(&h, "t1");
        assert_eq!(h.sent(), vec!["the others in this space".to_string()]);
        assert!(h.watched.lock().unwrap().is_empty(), "a note isn't watched");
        assert!(h.notes().is_empty(), "nor told to the brain");
        // One for a session that ends is dropped, not parked.
        h.set("t2", "working");
        q.add_note_with(&h, "t2", "hello", false).unwrap();
        h.sessions.lock().unwrap().remove("t2");
        q.session_round(&h, "t2");
        assert!(q.items().is_empty());
        assert!(h.notes().is_empty());
    }

    #[test]
    fn a_note_put_first_goes_in_before_what_waited() {
        let (q, _d) = queue();
        let h = Fake::default();
        h.set("t1", "working");
        q.add_with(&h, "t1", "then write the docs", &[], None).unwrap();
        q.add_note_with(&h, "t2", "elsewhere", false).ok();
        q.add_note_with(&h, "t1", "carry on with the login fix", true).unwrap();
        let mine: Vec<String> = q.items().iter().filter(|i| i.session == "t1").map(|i| i.text.clone()).collect();
        assert_eq!(mine, ["carry on with the login fix", "then write the docs"]);
        h.set("t1", "idle");
        q.session_round(&h, "t1");
        assert_eq!(h.sent(), vec!["carry on with the login fix".to_string()], "the note first; the instruction after its turn");
    }

    #[test]
    fn a_free_session_gets_it_at_once_and_is_watched() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "idle");
        let (added, _, _) = q.add_with(&f, "t5", "run the tests", &[], None).unwrap();
        assert!(matches!(added, Added::Sent { .. }));
        assert_eq!(f.sent(), vec!["run the tests"]);
        assert_eq!(f.watched.lock().unwrap().as_slice(), &["t5".to_string()]);
        assert!(f.notes()[0].starts_with("[queue] delivered to t5"));
        assert!(q.items().is_empty());
    }

    #[test]
    fn a_busy_session_gets_them_in_order_one_per_turn() {
        let (q, d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        assert!(matches!(q.add_with(&f, "t5", "first", &[], None).unwrap().0, Added::Queued { position: 1, .. }));
        assert!(matches!(q.add_with(&f, "t5", "second", &[], None).unwrap().0, Added::Queued { position: 2, .. }));
        q.tick(&f);
        assert!(f.sent().is_empty(), "nothing goes while it works");

        // It survives a restart.
        let q = SendQueue::at(d.0.join("queue.json"));
        assert_eq!(q.items().len(), 2);

        f.set("t5", "idle");
        q.tick(&f);
        assert_eq!(f.sent(), vec!["first"]);
        // Still idle, and it hasn't taken "first" yet: the next one waits.
        settle(&q);
        q.tick(&f);
        assert_eq!(f.sent().len(), 1);
        // It took it (its turn count moved), worked, and stopped.
        f.turn("t5");
        f.set("t5", "working");
        q.tick(&f);
        f.set("t5", "done");
        settle(&q);
        q.tick(&f);
        assert_eq!(f.sent(), vec!["first", "second"]);
        assert!(q.items().is_empty());
    }

    #[test]
    fn blocked_waits_and_nothing_is_dropped() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "blocked");
        q.add_with(&f, "t5", "after the dialog", &[], None).unwrap();
        q.tick(&f);
        assert!(f.sent().is_empty());

        // Herdr refuses: it stays.
        f.set("t5", "done");
        *f.send_fails.lock().unwrap() = true;
        q.tick(&f);
        assert_eq!(q.items().len(), 1);
        assert_eq!(q.items()[0].tries, 1);
        assert!(q.items()[0].sending_ms.is_none());

        // The session ends: told once, parked, kept.
        f.sessions.lock().unwrap().remove("t5");
        q.tick(&f);
        q.tick(&f);
        assert_eq!(f.notes().iter().filter(|n| n.contains("is gone")).count(), 1);
        assert_eq!(q.items().len(), 1);
        assert_eq!(q.items()[0].parked, Some(ParkWhy::Gone));

        // Only a cancel takes it out.
        let id = q.items()[0].id;
        q.cancel(id).unwrap();
        assert!(q.items().is_empty());
    }

    #[test]
    fn the_items_table() {
        let mut i = Item { id: 1, session: "t5".into(), text: "x".into(), ..Default::default() };
        assert_eq!(i.state(), ItemState::Waiting);
        assert_eq!(i.apply(ItemEv::Failed), Err(ItemRefused::NotClaimed));
        assert_eq!(i.apply(ItemEv::Release), Err(ItemRefused::NotClaimed));
        i.apply(ItemEv::Claim { now_ms: 5, turn: Some(3), busy: false }).unwrap();
        assert_eq!(i.state(), ItemState::Claimed { since_ms: 5, turn_before: Some(3) });
        assert_eq!(i.apply(ItemEv::Claim { now_ms: 6, turn: Some(3), busy: false }), Err(ItemRefused::GoingIn), "claimed once");
        assert_eq!(i.apply(ItemEv::Park { why: ParkWhy::Gone, now_ms: 6 }), Err(ItemRefused::GoingIn), "never parked while typed");
        assert_eq!(i.apply(ItemEv::Retarget { session: "t6".into(), name: "n".into(), created_ms: None }), Err(ItemRefused::GoingIn));
        i.apply(ItemEv::Failed).unwrap();
        assert_eq!((i.state(), i.tries), (ItemState::Waiting, 1), "the hand-over failed: waits again, one try more");
        i.apply(ItemEv::Park { why: ParkWhy::Gone, now_ms: 7 }).unwrap();
        assert_eq!((i.state(), i.parked_ms), (ItemState::Parked(ParkWhy::Gone), Some(7)));
        assert_eq!(i.apply(ItemEv::Claim { now_ms: 8, turn: None, busy: false }), Err(ItemRefused::Parked(ParkWhy::Gone)), "parked goes nowhere");
        i.apply(ItemEv::Park { why: ParkWhy::Gone, now_ms: 99 }).unwrap();
        assert_eq!(i.parked_ms, Some(7), "parked again for the same reason: since when it first was");
        i.apply(ItemEv::Retarget { session: "t6".into(), name: "t6-name".into(), created_ms: Some(4) }).unwrap();
        assert_eq!((i.state(), i.session.as_str(), i.tries, i.parked_ms), (ItemState::Waiting, "t6", 0, None));
        let start = Item { start: vec!["--name".into(), "x".into()], after: vec![1], ..Default::default() };
        assert_eq!(start.state(), ItemState::WaitingFor(vec![1]));
        assert_eq!(Item { after: vec![], ..start }.state(), ItemState::ToStart);

        let mut d = Delivered { id: 1, session: "t5".into(), text: "x".into(), at_ms: 0, turn_before: Some(1), ..Default::default() };
        assert_eq!(d.state(), DeliveryState::Sent);
        assert!(d.take() && !d.take(), "taken once");
        assert_eq!(d.state(), DeliveryState::Taken);
        assert!(d.finish() && !d.finish());
        assert_eq!(d.state(), DeliveryState::Finished);
        assert!(!d.take(), "finished stays finished");
    }

    #[test]
    fn old_queue_files_load_and_save_the_same_words() {
        let old = r#"{"next_id": 3, "items": [
            {"id": 1, "session": "t5", "name": "a", "text": "x", "at_ms": 1, "parked": "gone"},
            {"id": 2, "session": "t6", "name": "b", "text": "y", "at_ms": 1, "parked": "something-newer"},
            {"id": 3, "session": "t7", "name": "c", "text": "z", "at_ms": 1, "sending_ms": 5, "turn_before": 2}
        ], "delivered": [{"id": 0, "session": "t5", "text": "w", "at_ms": 1, "taken": true}]}"#;
        let b: Book = serde_json::from_str(old).unwrap();
        assert_eq!(b.items[0].state(), ItemState::Parked(ParkWhy::Gone));
        assert_eq!(b.items[1].state(), ItemState::Parked(ParkWhy::Other("something-newer".into())));
        assert_eq!(b.items[2].state(), ItemState::Claimed { since_ms: 5, turn_before: Some(2) });
        assert_eq!(b.delivered[0].state(), DeliveryState::Taken);
        let again = serde_json::to_value(&b).unwrap();
        assert_eq!(again["items"][0]["parked"], "gone");
        assert_eq!(again["items"][1]["parked"], "something-newer", "a word it doesn't know is kept");
        assert!(again.get("expired").is_none(), "nothing new on disk until something expires");
        // The CLI reads the daemon's items the same way.
        let items: Vec<Item> = serde_json::from_value(again["items"].clone()).unwrap();
        assert_eq!(items, b.items);
    }

    #[test]
    fn a_parked_instruction_expires_after_a_week_and_is_kept() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        q.add_with(&f, "t5", "deploy the billing fix", &[], None).unwrap();
        f.sessions.lock().unwrap().remove("t5");
        q.tick(&f);
        let parked_at = q.items()[0].parked_ms.unwrap();
        let notes = f.notes().len();
        q.expire_round(&f, parked_at + PARK_EXPIRE_MS - 1);
        assert_eq!(q.items().len(), 1, "not yet");
        q.expire_round(&f, parked_at + PARK_EXPIRE_MS);
        assert!(q.items().is_empty());
        assert_eq!(f.notes().len(), notes + 1, "the brain is reminded once");
        assert!(f.notes().last().unwrap().contains("#1 for t5 (gone): deploy the billing fix"), "{:?}", f.notes());
        q.expire_round(&f, parked_at + 2 * PARK_EXPIRE_MS);
        assert_eq!(f.notes().len(), notes + 1, "and only once");
        // Kept: on disk after a restart, and it can still be sent.
        let q2 = SendQueue::at(q.path.clone());
        assert_eq!(q2.lock().expired.len(), 1);
        assert!(format!("{:#}", q2.cancel(1).unwrap_err()).contains("expired already"));
        f.set("t9", "idle");
        q2.retarget_with(&f, 1, "t9").unwrap();
        q2.tick(&f);
        assert_eq!(f.sent(), vec!["deploy the billing fix"]);
        assert!(q2.lock().expired.is_empty());
    }

    #[test]
    fn what_waits_for_an_expired_one_never_goes() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        f.set("t6", "idle");
        let (Added::Queued { id: a, .. }, _, _) = q.add_with(&f, "t5", "build it", &[], None).unwrap() else { panic!() };
        q.add_with(&f, "t6", "deploy what t5 built", &[a], None).unwrap();
        f.sessions.lock().unwrap().remove("t5");
        q.tick(&f);
        let parked_at = q.items().iter().find(|i| i.id == a).unwrap().parked_ms.unwrap();
        q.expire_round(&f, parked_at + PARK_EXPIRE_MS);
        q.tick(&f);
        assert!(f.sent().is_empty(), "never as if what it waited for were done: {:?}", f.sent());
        assert_eq!(q.items().iter().map(|i| i.parked.clone()).collect::<Vec<_>>(), [Some(ParkWhy::AfterCancelled)]);
    }

    #[test]
    fn parked_by_an_older_version_expires_a_week_after_this_one_sees_it() {
        let (q, _d) = queue();
        let f = Fake::default();
        q.lock().items.push(Item { id: 4, session: "t5".into(), text: "x".into(), parked: Some(ParkWhy::Gone), ..Default::default() });
        q.expire_round(&f, 1_000_000_000_000);
        assert_eq!(q.items()[0].parked_ms, Some(1_000_000_000_000), "counted from now, not dropped");
        assert!(f.notes().is_empty());
    }

    #[test]
    fn the_list_says_where_each_stands() {
        let items = vec![
            Item { id: 1, session: "t5".into(), name: "a".into(), text: "x".into(), ..Default::default() },
            Item { id: 2, session: "t5".into(), name: "a".into(), text: "y".into(), sending_ms: Some(1), ..Default::default() },
            Item { id: 3, session: "t6".into(), name: "b".into(), text: "z".into(), parked: Some(ParkWhy::Replaced), ..Default::default() },
            Item { id: 4, session: String::new(), name: "c".into(), text: "w".into(), after: vec![1], start: vec!["c".into()], ..Default::default() },
        ];
        let lines = describe(&items, |s| (s == "t5").then(|| "working".to_string()));
        assert!(lines[0].contains("(a, working)"), "{lines:?}");
        assert!(lines[1].contains("going in now"), "{lines:?}");
        assert!(lines[2].contains("parked: replaced"), "{lines:?}");
        assert!(lines[3].contains("starts when it may, after #1"), "a session to start isn't \"gone\": {lines:?}");
    }

    #[test]
    fn a_parked_instruction_never_goes_to_whoever_comes_back() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        q.add_with(&f, "t5", "deploy the billing fix", &[], None).unwrap();
        f.sessions.lock().unwrap().remove("t5");
        q.tick(&f);
        // A new, unrelated session at t5 (and the old one, back, all the same):
        f.replace("t5", 999);
        settle(&q);
        q.tick(&f);
        q.tick(&f);
        assert!(f.sent().is_empty(), "parked instructions wait for a retarget");
        // Retargeted, it goes.
        let id = q.items()[0].id;
        q.retarget_with(&f, id, "t5").unwrap();
        q.tick(&f);
        assert_eq!(f.sent(), vec!["deploy the billing fix"]);
    }

    #[test]
    fn another_terminal_at_the_address_parks_them() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        q.add_with(&f, "t5", "go on", &[], None).unwrap();
        f.replace("t5", 555);
        q.tick(&f);
        assert!(f.sent().is_empty());
        assert_eq!(q.items()[0].parked, Some(ParkWhy::Replaced));
        assert!(f.notes().iter().any(|n| n.contains("a different terminal now")));
    }

    #[test]
    fn out_of_reach_is_not_gone() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("vps/t2", "working");
        q.add_with(&f, "vps/t2", "run the migrations check", &[], None).unwrap();
        *f.unreachable.lock().unwrap() = true;
        q.tick(&f);
        q.tick(&f);
        assert!(f.notes().iter().all(|n| !n.contains("gone")));
        assert_eq!(q.items()[0].parked, None);
        // Back, and free: it goes.
        *f.unreachable.lock().unwrap() = false;
        f.set("vps/t2", "idle");
        q.tick(&f);
        assert_eq!(f.sent(), vec!["run the migrations check"]);
    }

    #[test]
    fn a_retry_is_the_same_instruction() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        let a = q.add_with(&f, "t5", "add a test", &[], None).unwrap().0;
        let b = q.add_with(&f, "t5", "add a test", &[], None).unwrap().0;
        assert_eq!(a, b);
        assert_eq!(q.items().len(), 1);
        f.set("t5", "idle");
        q.tick(&f);
        // A retry just after it went in: "sent", not again.
        assert!(matches!(q.add_with(&f, "t5", "add a test", &[], None).unwrap().0, Added::Sent { .. }));
        assert_eq!(f.sent().len(), 1);
    }

    #[test]
    fn a_restart_between_claim_and_typing_never_types_twice() {
        let (q, d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        q.add_with(&f, "t5", "migrate the db", &[], None).unwrap();
        // Claimed and typed; the tower stops before it writes that down.
        {
            let mut b = q.lock();
            b.items[0].sending_ms = Some(1);
            b.items[0].turn_before = Some(0);
            q.save(&b).unwrap();
        }
        f.turn("t5");
        let q2 = SendQueue::at(d.0.join("queue.json"));
        q2.recover(&f);
        f.set("t5", "idle");
        q2.tick(&f);
        assert!(f.sent().is_empty(), "the turn count moved: it went in already");
        assert!(q2.items().is_empty());

        // Claimed but never typed (the turn count didn't move): typed after the restart.
        q2.add_with(&f, "t5", "second", &[], None).unwrap();
        let _ = f.sent.lock().unwrap().drain(..);
        {
            let mut b = q2.lock();
            if let Some(i) = b.items.first_mut() {
                i.sending_ms = Some(1);
            }
        }
    }

    #[test]
    fn a_status_nothing_goes_into_is_told_once() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        q.add_with(&f, "t5", "try again", &[], None).unwrap();
        f.set("t5", "error");
        q.tick(&f);
        q.tick(&f);
        assert_eq!(f.notes().iter().filter(|n| n.contains("is error")).count(), 1);
        assert!(f.sent().is_empty());
        f.set("t5", "idle");
        q.tick(&f);
        assert_eq!(f.sent(), vec!["try again"]);
    }

    #[test]
    fn a_plain_program_takes_no_queue() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "");
        assert!(q.add_with(&f, "t5", "ls", &[], None).is_err());
    }

    #[test]
    fn after_waits_for_another_session_to_finish() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "idle");
        f.set("t6", "idle");
        let first = match q.add_with(&f, "t5", "fix the api", &[], None).unwrap().0 {
            Added::Sent { id } => id,
            other => panic!("{other:?}"),
        };
        let (added, _, _) = q.add_with(&f, "t6", "update the client for the new api", &[first], None).unwrap();
        assert!(matches!(added, Added::Queued { .. }), "t6 waits for t5's turn to finish");
        q.tick(&f);
        assert_eq!(f.sent(), vec!["fix the api"]);
        // t5 takes it and finishes.
        f.turn("t5");
        f.set("t5", "working");
        q.tick(&f);
        f.set("t5", "done");
        q.tick(&f);
        q.tick(&f);
        assert_eq!(f.sent(), vec!["fix the api", "update the client for the new api"]);
    }

    #[test]
    fn waiting_on_a_cancelled_one_parks() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        f.set("t6", "idle");
        let Added::Queued { id: first, .. } = q.add_with(&f, "t5", "a", &[], None).unwrap().0 else { panic!() };
        q.add_with(&f, "t6", "b", &[first], None).unwrap();
        q.cancel(first).unwrap();
        q.tick(&f);
        assert!(f.sent().is_empty());
        assert_eq!(q.items()[0].parked, Some(ParkWhy::AfterCancelled));
    }

    #[test]
    fn a_slow_machine_never_holds_up_the_others() {
        struct Slow(Fake);
        impl Sessions for Slow {
            fn look(&self, s: &str) -> Look {
                if s.starts_with("vps/") {
                    thread::sleep(Duration::from_millis(1500));
                }
                self.0.look(s)
            }
            fn send(&self, s: &str, t: &str, busy: bool) -> Result<()> {
                self.0.send(s, t, busy)
            }
            fn tell(&self, n: Note) {
                self.0.tell(n)
            }
            fn watch(&self, s: &str, t: &str) {
                self.0.watch(s, t)
            }
            fn record(&self, _: &str, _: &str, _: &str) {}
            fn start(&self, args: &[String]) -> Result<String> {
                self.0.start(args)
            }
            fn peer(&self, m: crate::peers::Message) {
                self.0.peer(m)
            }
            fn level(&self) -> crate::tell::Level {
                self.0.level()
            }
            fn busy_minutes(&self) -> u64 {
                self.0.busy_minutes()
            }
            fn check(&self, c: Check) {
                self.0.check(c)
            }
        }
        let (q, _d) = queue();
        let f = Slow(Fake::default());
        for i in 0..5 {
            f.0.set(&format!("vps/t{i}"), "working");
        }
        f.0.set("t1", "working");
        for i in 0..5 {
            q.lock().push(
                &Session { address: format!("vps/t{i}"), name: "n".into(), status: SessionStatus::Working, ..Default::default() },
                "x",
                &[],
                0,
            );
        }
        // A tick and an add at once: the add doesn't wait for the slow machine.
        let q2 = q.clone();
        let t0 = std::time::Instant::now();
        thread::scope(|s| {
            s.spawn(|| q2.tick(&f));
            thread::sleep(Duration::from_millis(50));
            q.add_with(&f.0, "t1", "hello", &[], None).unwrap();
            assert!(t0.elapsed() < Duration::from_millis(1000), "add waited {:?}", t0.elapsed());
        });
        // The tick looked at the five machines' sessions at once, not one by one.
        assert!(t0.elapsed() < Duration::from_millis(4000), "tick took {:?}", t0.elapsed());
    }

    #[test]
    fn a_session_starts_once_what_it_waits_for_is_done() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "idle");
        let Added::Sent { id: api } = q.add_with(&f, "t5", "change the api", &[], None).unwrap().0 else { panic!() };
        let args: Vec<String> =
            ["--name", "client-update", "--cwd", "/w/client", "update the client"].iter().map(|s| s.to_string()).collect();
        let start = q.add_start("client-update", "update the client", &args, &[api]).unwrap();
        q.tick(&f);
        assert_eq!(f.sent(), vec!["change the api"], "not started yet: t5 hasn't finished");
        f.turn("t5");
        f.set("t5", "working");
        q.tick(&f);
        f.set("t5", "done");
        q.tick(&f);
        q.tick(&f);
        assert_eq!(f.sent(), vec!["change the api", "update the client"]);
        assert!(q.items().is_empty());
        // And something can wait for the started session in turn.
        assert!(q.lock().delivered.iter().any(|d| d.id == start && d.session == "client-update"));
        assert!(q.add_start("x", "y", &args, &[]).is_err(), "nothing to wait for: tower start it");
    }

    fn orb() -> crate::peers::Sender {
        crate::peers::Sender { address: "t1".into(), name: "orb rust".into(), space: Some("Shop".into()) }
    }

    #[test]
    fn a_message_between_sessions_has_its_senders_name_and_is_nobodys_task() {
        let (q, _d) = queue();
        let h = Fake::at(crate::tell::Level::Decisions);
        h.set("t1", "working");
        h.set("t2", "working");
        let (Added::Queued { id, .. }, address, _) = q.add_with(&h, "t2", "which port does the api use?", &[], Some(&orb())).unwrap() else {
            panic!("t2 works: it waits, like any instruction")
        };
        assert_eq!(address, "t2");
        let item = q.items()[0].clone();
        assert_eq!((item.id, item.from.as_ref()), (id, Some(&orb())));
        assert_eq!(item.text, "From Shop › orb rust (t1): which port does the api use?", "the tower puts the sender's name on it");
        assert_eq!(q.lock().waiting_for("t2"), 0, "not work waiting for it");
        assert!(h.sent().is_empty(), "never typed into a working session");
        // The same message again is the same item, as with any instruction.
        let (Added::Queued { id: again, .. }, ..) = q.add_with(&h, "t2", "which port does the api use?", &[], Some(&orb())).unwrap() else { panic!() };
        assert_eq!(id, again);
        // It goes in when the receiver stops, in its turn.
        h.set("t2", "idle");
        q.session_round(&h, "t2");
        assert_eq!(h.sent(), vec!["From Shop › orb rust (t1): which port does the api use?".to_string()]);
        assert!(q.items().is_empty());
        // No watch (so no job and no report), no word to any brain, no queue entry: only the thread.
        assert!(h.watched.lock().unwrap().is_empty(), "not watched");
        assert!(h.notes().is_empty(), "the brain hears nothing");
        assert!(h.recorded.lock().unwrap().is_empty(), "no `queue` entry: {:?}", h.recorded.lock().unwrap());
        let said = h.peers();
        assert_eq!(said.len(), 1);
        assert_eq!((said[0].from.address.as_str(), said[0].to.as_str(), said[0].text.as_str(), said[0].delivered), ("t1", "t2", "which port does the api use?", true));
        let e = said[0].entry();
        assert_eq!((e.kind.as_str(), e.session.as_deref(), e.from.as_deref(), e.space.as_deref()), ("peer", Some("t2"), Some("t1"), Some("Shop")));
        // A free receiver has it at once; one that says who it is from keeps its own words.
        h.set("t3", "idle");
        let (added, ..) = q.add_with(&h, "t3", "From the orb session: ready when you are", &[], Some(&orb())).unwrap();
        assert!(matches!(added, Added::Sent { .. }));
        assert_eq!(h.sent().last().map(String::as_str), Some("From the orb session: ready when you are"));
        assert!(h.watched.lock().unwrap().is_empty() && h.notes().is_empty());
        // Kept across a restart with who sent it.
        h.set("t4", "working");
        q.add_with(&h, "t4", "later", &[], Some(&orb())).unwrap();
        let again = SendQueue::at(q.path.clone());
        assert_eq!(again.items()[0].from.as_ref(), Some(&orb()));
    }

    #[test]
    fn a_message_that_cant_be_delivered_tells_its_sender_once_and_nobody_else() {
        let (q, _d) = queue();
        let h = Fake::at(crate::tell::Level::Decisions);
        h.set("t1", "working");
        h.set("t2", "working");
        q.add_with(&h, "t2", "which port does the api use?", &[], Some(&orb())).unwrap();
        q.add_with(&h, "t2", "and which branch?", &[], Some(&orb())).unwrap();
        // The receiver ends before it could take them.
        h.sessions.lock().unwrap().remove("t2");
        q.session_round(&h, "t2");
        assert!(h.notes().is_empty(), "the brain (and so the user) is not told");
        let lost = h.peers();
        assert_eq!(lost.len(), 2, "both are in the record, as not delivered");
        assert!(lost.iter().all(|m| !m.delivered && m.to == "t2" && m.from.address == "t1"));
        assert_eq!(lost[0].text, "which port does the api use?");
        // Out of the queue (not parked), and one note waits for the sender.
        let items = q.items();
        assert_eq!(items.len(), 1);
        assert!(items[0].quiet && items[0].session == "t1" && items[0].parked.is_none(), "{:?}", items[0]);
        assert!(items[0].text.starts_with("[tower] Your 2 messages to t2-name (t2) were not delivered: it has ended."), "{}", items[0].text);
        // Looking again changes nothing: told once.
        q.session_round(&h, "t2");
        assert_eq!(q.items().len(), 1);
        assert_eq!(h.peers().len(), 2);
        // The sender gets it when it is free, as a note: not watched, not told to the brain.
        h.set("t1", "idle");
        q.session_round(&h, "t1");
        assert_eq!(h.sent().len(), 1);
        assert!(h.sent()[0].contains("were not delivered") && h.sent()[0].contains("which port does the api use?"));
        assert!(q.items().is_empty() && h.notes().is_empty() && h.watched.lock().unwrap().is_empty());
        settle(&q);
        h.turn("t1");
        q.session_round(&h, "t1");
        assert_eq!(h.sent().len(), 1, "once");
    }

    #[test]
    fn the_brains_own_instruction_beside_a_message_is_still_an_instruction() {
        let (q, _d) = queue();
        let h = Fake::at(crate::tell::Level::Decisions);
        h.set("t1", "idle");
        h.set("t2", "working");
        // A message from a session, then work from the brain (no sender), for the same receiver.
        q.add_with(&h, "t2", "which port?", &[], Some(&orb())).unwrap();
        let (Added::Queued { id: work, position, .. }, ..) = q.add_with(&h, "t2", "fix the login", &[], None).unwrap() else { panic!() };
        assert_eq!(position, 2, "in order, behind the message");
        assert_eq!(q.lock().waiting_for("t2"), 1, "one instruction waits; the message isn't work");
        assert_eq!(h.recorded("queued"), vec!["Queued for t2-name: fix the login".to_string()]);
        // The receiver ends: the instruction is parked and the brain hears it, as before; the message goes back to its sender.
        h.sessions.lock().unwrap().remove("t2");
        q.session_round(&h, "t2");
        let notes = h.notes();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("is gone, with 1 instruction(s) queued for it") && notes[0].contains(&format!("#{work}")), "{}", notes[0]);
        let items = q.items();
        assert!(items.iter().any(|i| i.id == work && i.parked == Some(ParkWhy::Gone)));
        assert!(items.iter().any(|i| i.quiet && i.session == "t1" && i.text.contains("Your message to t2-name (t2) was not delivered")));
        assert_eq!(h.peers().len(), 1);
    }

    #[test]
    fn a_delivery_is_written_down_not_told_at_a_quiet_level() {
        for level in [crate::tell::Level::Decisions, crate::tell::Level::Results] {
            let (q, _d) = queue();
            let h = Fake::at(level);
            h.set("t5", "idle");
            let (Added::Sent { id: api }, ..) = q.add_with(&h, "t5", "change the api", &[], None).unwrap() else { panic!("t5 was free") };
            assert_eq!(h.sent(), vec!["change the api"]);
            assert_eq!(h.watched.lock().unwrap().as_slice(), &["t5".to_string()], "still watched: the brain hears when it stops");
            assert_eq!(h.recorded("delivered"), vec!["Delivered to t5-name: change the api".to_string()], "in the record");
            assert!(h.notes().is_empty(), "`[queue] delivered` is not sent to the brain");
            // A session a dispatch held back starts: written down too.
            let args: Vec<String> = ["--name", "client-update", "--cwd", "/w/client", "update the client"].iter().map(|s| s.to_string()).collect();
            q.add_start("client-update", "update the client", &args, &[api]).unwrap();
            h.turn("t5");
            h.set("t5", "done");
            q.tick(&h);
            q.tick(&h);
            assert_eq!(h.sent(), vec!["change the api", "update the client"]);
            assert!(h.notes().is_empty(), "nor `[queue] started`");
            assert_eq!(h.recorded("delivered").len(), 2);
            // What is stuck still reaches the brain: a session that ends with work queued for it.
            h.set("t6", "working");
            q.add_with(&h, "t6", "deploy it", &[], None).unwrap();
            h.sessions.lock().unwrap().remove("t6");
            q.session_round(&h, "t6");
            assert_eq!(h.notes().len(), 1);
            assert!(h.notes()[0].starts_with("[queue] t6 (t6-name) is gone"), "{}", h.notes()[0]);
            // Notes an earlier version left: only what is stuck goes out.
            q.lock().notes.extend(["[queue] delivered to t5 (x): y".to_string(), "[queue] t9 (z) is gone, with 1 instruction(s) queued for it".to_string()]);
            q.recover(&h);
            assert_eq!(h.notes().len(), 2);
            assert!(h.notes()[1].starts_with("[queue] t9 (z) is gone"));
        }
    }

    /// A Claude Code or Codex session whose turn ended on an API error sits
    /// at its prompt: it gets what waits for it at once. A program the
    /// queue can't tell is at its prompt still waits, and the brain hears once.
    #[test]
    fn an_agent_in_error_takes_its_instruction_at_once() {
        let (q, _d) = queue();
        let h = Fake::at(crate::tell::Level::Decisions);
        let now = crate::history::now_ms();
        for (s, app) in [("t5", "claude-code"), ("t6", "codex")] {
            h.set(s, "error");
            h.runs(s, app, now);
            let (added, ..) = q.add_with(&h, s, &format!("try again in {s}"), &[], None).unwrap();
            assert!(matches!(added, Added::Sent { .. }), "{s} ({app}) is at its prompt");
        }
        // A peer message too, as to a session that is done.
        let (added, ..) = q.add_with(&h, "t5", "the api is back up", &[], Some(&orb())).unwrap();
        assert!(matches!(added, Added::Queued { .. }), "one per turn: the instruction before it isn't taken yet");
        h.turn("t5");
        settle(&q);
        q.session_round(&h, "t5");
        assert_eq!(h.sent(), ["try again in t5", "try again in t6", "From Shop › orb rust (t1): the api is back up"]);
        assert!(h.busy_sent.lock().unwrap().is_empty(), "nothing typed into a working session");
        // An error from a program the queue can't tell: it waits, told once.
        h.set("t7", "working");
        q.add_with(&h, "t7", "rerun it", &[], None).unwrap();
        h.set("t7", "error");
        q.tick(&h);
        q.tick(&h);
        assert_eq!(h.notes().iter().filter(|n| n.contains("t7") && n.contains("is error")).count(), 1);
        assert!(!h.sent().contains(&"rerun it".to_string()));
    }

    /// A Claude Code session that has worked longer than `busy_deliver_minutes`
    /// (background agents keep it "working" for hours) gets a message from
    /// another session typed in: it keeps it and reads it when its turn ends.
    /// One at a time, each exactly once; the user's and the brains'
    /// instructions wait for the turn's end; a session that worked a short
    /// while, a Codex session, a brain or a setting of 0 get nothing typed in.
    #[test]
    fn a_message_to_a_session_busy_for_long_goes_in_once_and_is_read_when_its_turn_ends() {
        let (q, _d) = queue();
        let h = Fake::at(crate::tell::Level::Decisions);
        let now = crate::history::now_ms();
        h.set("t2", "working");
        h.runs("t2", "claude-code", now - 11 * 60_000);
        h.sessions.lock().unwrap().get_mut("t2").unwrap().2 = Some(7);
        let sent_ = |n: usize| h.sent().len() == n;
        // The message goes in at once, while it works.
        let (Added::Sent { id: first }, ..) = q.add_with(&h, "t2", "which port does the api use?", &[], Some(&orb())).unwrap() else {
            panic!("typed in while it works")
        };
        assert_eq!(*h.busy_sent.lock().unwrap(), ["From Shop › orb rust (t1): which port does the api use?"]);
        assert!(q.typed_while_working(first));
        assert_eq!(h.peers().len(), 1, "in the thread, as delivered");
        {
            let b = q.lock();
            let d = b.delivered.iter().find(|d| d.id == first).unwrap();
            assert_eq!((d.busy, d.turn_before, d.taken, d.finished), (true, Some(8), false, false), "its turn is the one after this");
        }
        // The user's (or a brain's) instruction waits for the turn to end; so does the next message.
        let (Added::Queued { id: work, .. }, ..) = q.add_with(&h, "t2", "fix the login", &[], None).unwrap() else { panic!("waits") };
        let (Added::Queued { id: second, .. }, ..) = q.add_with(&h, "t2", "and which branch?", &[], Some(&orb())).unwrap() else { panic!("one at a time") };
        for _ in 0..3 {
            q.tick(&h);
        }
        assert!(sent_(1), "{:?}", h.sent());
        // The long turn ends: Claude Code sends the message it kept as its next one. Nothing else goes in meanwhile.
        h.turn("t2");
        h.set("t2", "done");
        settle(&q);
        q.tick(&h);
        assert!(sent_(1), "free a moment, but it starts on the message: {:?}", h.sent());
        h.set("t2", "working");
        q.tick(&h);
        // It worked on the message and stopped: then the instruction, then the second message, one per turn.
        h.turn("t2");
        h.set("t2", "done");
        q.tick(&h);
        assert_eq!(h.sent().last().map(String::as_str), Some("fix the login"));
        assert!(q.lock().delivered.iter().any(|d| d.id == first && d.finished), "the first is taken and done");
        h.turn("t2");
        settle(&q);
        q.tick(&h);
        assert_eq!(h.sent().last().map(String::as_str), Some("From Shop › orb rust (t1): and which branch?"));
        for text in ["From Shop › orb rust (t1): which port does the api use?", "fix the login", "From Shop › orb rust (t1): and which branch?"] {
            assert_eq!(h.sent().iter().filter(|s| *s == text).count(), 1, "{text} went in once");
        }
        assert!(q.items().is_empty() && !q.typed_while_working(work) && !q.typed_while_working(second));

        // Working only five minutes: it waits.
        h.set("t3", "working");
        h.runs("t3", "claude-code", now - 5 * 60_000);
        assert!(matches!(q.add_with(&h, "t3", "ping", &[], Some(&orb())).unwrap().0, Added::Queued { .. }));
        // Codex keeps no typed input for later: it waits however long it works.
        h.set("t4", "working");
        h.runs("t4", "codex", now - 60 * 60_000);
        assert!(matches!(q.add_with(&h, "t4", "ping", &[], Some(&orb())).unwrap().0, Added::Queued { .. }));
        // 0 is never.
        *h.minutes.lock().unwrap() = 0;
        h.set("t8", "working");
        h.runs("t8", "claude-code", now - 60 * 60_000);
        assert!(matches!(q.add_with(&h, "t8", "ping", &[], Some(&orb())).unwrap().0, Added::Queued { .. }));
        q.tick(&h);
        let typed_in = || h.busy_sent.lock().unwrap().clone();
        assert_eq!(typed_in().len(), 1, "only the first message was typed in while working: {:?}", typed_in());
        // Blocked on a dialog: never.
        *h.minutes.lock().unwrap() = 10;
        h.set("t8", "blocked");
        q.tick(&h);
        assert_eq!(typed_in().len(), 1, "{:?}", typed_in());
        // The setting back, working again: it goes in.
        h.set("t8", "working");
        q.tick(&h);
        assert_eq!(typed_in().len(), 2, "{:?}", typed_in());
        assert_eq!(typed_in()[1], "From Shop › orb rust (t1): ping");
    }

    /// A message claimed to be typed in while its session works, and the
    /// tower stops before it writes down that it went in: after the restart
    /// it is never typed in again while that turn lasts (it may be in the
    /// session's input already). It goes in when the session is free, unless
    /// the turn count says the session took it.
    #[test]
    fn a_restart_around_a_busy_hand_over_types_it_once() {
        let (q, d) = queue();
        let h = Fake::at(crate::tell::Level::Decisions);
        let now = crate::history::now_ms();
        h.set("t2", "working");
        h.runs("t2", "claude-code", now - 30 * 60_000);
        h.sessions.lock().unwrap().get_mut("t2").unwrap().2 = Some(3);
        *h.minutes.lock().unwrap() = 0;
        q.add_with(&h, "t2", "the docs moved", &[], Some(&orb())).unwrap();
        // Claimed (as a busy hand-over at turn 3), typed, and the tower stopped.
        {
            let mut b = q.lock();
            assert!(b.items[0].apply(ItemEv::Claim { now_ms: now, turn: Some(3), busy: true }).is_ok());
            assert_eq!((b.items[0].turn_before, b.items[0].busy), (Some(4), true));
            q.save(&b).unwrap();
        }
        *h.minutes.lock().unwrap() = 10;
        let q2 = SendQueue::at(d.0.join("queue.json"));
        q2.recover(&h);
        for _ in 0..3 {
            q2.tick(&h);
        }
        assert!(h.sent().is_empty(), "not typed in again while it works");
        // That turn ends and the session works on the message it kept, then stops: it went in.
        h.turn("t2");
        h.set("t2", "working");
        q2.tick(&h);
        h.turn("t2");
        h.set("t2", "done");
        q2.tick(&h);
        assert!(h.sent().is_empty(), "the turn count says it took it: never typed twice");
        assert!(q2.items().is_empty());

        // Claimed, never typed (the session's turn ended with nothing more): typed once it is free.
        let (q, d) = queue();
        h.set("t9", "working");
        h.runs("t9", "claude-code", now - 30 * 60_000);
        h.sessions.lock().unwrap().get_mut("t9").unwrap().2 = Some(1);
        *h.minutes.lock().unwrap() = 0;
        q.add_with(&h, "t9", "the build is green", &[], Some(&orb())).unwrap();
        {
            let mut b = q.lock();
            assert!(b.items[0].apply(ItemEv::Claim { now_ms: now, turn: Some(1), busy: true }).is_ok());
            q.save(&b).unwrap();
        }
        *h.minutes.lock().unwrap() = 10;
        let q2 = SendQueue::at(d.0.join("queue.json"));
        q2.recover(&h);
        q2.tick(&h);
        assert!(h.sent().is_empty());
        h.turn("t9");
        h.set("t9", "idle");
        q2.tick(&h);
        assert_eq!(h.sent(), ["From Shop › orb rust (t1): the build is green"]);
        q2.tick(&h);
        assert_eq!(h.sent().len(), 1, "once");
    }

    /// A message typed in while its session still works on a task is not
    /// "the next hand-over" that says the task's turn was over.
    #[test]
    fn a_busy_hand_over_says_nothing_about_the_turn_under_way() {
        use crate::agenda::QueueFact;
        let (q, _d) = queue();
        let h = Fake::at(crate::tell::Level::Decisions);
        let now = crate::history::now_ms();
        h.set("t2", "idle");
        h.runs("t2", "claude-code", now - 30 * 60_000);
        let (Added::Sent { id: task }, ..) = q.add_with(&h, "t2", "port the billing page", &[], None).unwrap() else { panic!() };
        h.set("t2", "working");
        q.session_round(&h, "t2");
        settle(&q);
        let (Added::Sent { id: msg }, ..) = q.add_with(&h, "t2", "the api is on 3001", &[], Some(&orb())).unwrap() else { panic!("typed in") };
        assert!(q.typed_while_working(msg));
        assert!(matches!(q.fact(task), QueueFact::Delivered { next_ms: None, taken: false, finished: false, .. }), "{:?}", q.fact(task));
        // The turn ends: the task's turn is over (and the message's is next).
        h.turn("t2");
        h.set("t2", "done");
        q.session_round(&h, "t2");
        assert!(matches!(q.fact(task), QueueFact::Delivered { taken: true, finished: true, .. }), "{:?}", q.fact(task));
    }

    #[test]
    fn notes_from_an_earlier_version_go_out() {
        let (q, _d) = queue();
        let f = Fake::default();
        q.lock().notes.push("[queue] delivered to t5 (x): y".into());
        q.recover(&f);
        assert_eq!(f.notes(), vec!["[queue] delivered to t5 (x): y"]);
    }
}
