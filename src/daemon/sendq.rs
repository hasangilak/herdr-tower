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
    /// About to be typed (or started): on disk first.
    Claim { now_ms: u64, turn: Option<u64> },
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
            (ItemEv::Claim { now_ms, turn }, _) => {
                self.sending_ms = Some(now_ms);
                if self.start.is_empty() {
                    self.turn_before = turn;
                }
                Ok(())
            }
            (ItemEv::Failed, ItemState::Claimed { .. }) => {
                self.sending_ms = None;
                self.tries += 1;
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

    fn waiting_for(&self, session: &str) -> usize {
        self.items.iter().filter(|i| i.session == session && !matches!(i.state(), ItemState::Parked(_))).count()
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
        if !s.status.ready() {
            return false;
        }
        match self.last_delivery(&s.address) {
            None => true,
            Some(d) => {
                let since = now.saturating_sub(d.at_ms);
                since >= GAP_MS && (d.taken || since >= TAKE_MS)
            }
        }
    }

    /// Whether the ids an item waits for are all done (None: one was cancelled).
    fn after_done(&self, item: &Item) -> Option<bool> {
        let mut all = true;
        for id in &item.after {
            if self.cancelled.contains(id) {
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

    /// What the session's facts say about the last delivery: taken (its turn
    /// count moved), finished (and it stopped since).
    fn follow(&mut self, s: &Session) -> bool {
        let Some(d) = self.delivered.iter_mut().rev().find(|d| d.session == s.address) else { return false };
        let moved = matches!((s.turn, d.turn_before), (Some(now), Some(before)) if now > before);
        let mut changed = moved && d.take();
        // Taken and ready again; or, with no turn count to go by, ready again.
        if (moved || d.turn_before.is_none()) && s.status.ready() {
            changed |= d.finish();
        }
        changed
    }
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

    fn at(path: PathBuf) -> SendQueue {
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
        self.add_with(&Herdr, session, text, after)
    }

    fn add_with(&self, h: &dyn Sessions, session: &str, text: &str, after: &[u64]) -> Result<(Added, String, String)> {
        let text = hc::clean(text.trim());
        if text.is_empty() {
            bail!("nothing to queue: give the instruction after the session");
        }
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
            let item = b.push(&s, &text, after, now);
            // On disk before the answer: a queued instruction survives anything after this.
            if let Err(e) = self.save(&b) {
                b.items.retain(|i| i.id != item.id);
                return Err(e);
            }
            item
        };
        let (a, n, x) = (s.address.clone(), s.name.clone(), text.clone());
        h.record(&a, &format!("Queued for {n}: {x}"), "queued");
        // Free now: it goes at once.
        self.work(h, &s.address, Some(&s));
        let b = self.lock();
        if b.delivered.iter().any(|d| d.id == item.id) {
            return Ok((Added::Sent { id: item.id }, s.address, s.name));
        }
        let position = b.items.iter().filter(|j| j.session == s.address && j.parked.is_none() && j.id <= item.id).count();
        Ok((Added::Queued { id: item.id, position: position.max(1), status: s.status.word().to_string() }, s.address, s.name))
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
            taken: false,
            finished: false,
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
                        if b.items[pos].apply(ItemEv::Claim { now_ms: now, turn: None }).is_ok() {
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
                        taken: false,
                        finished: false,
                    });
                    h.record(&address, &format!("Started {} for: {}", item.name, item.text), "delivered");
                    h.tell(Note {
                        session: address.clone(),
                        name: item.name.clone(),
                        msg: format!(
                            "[queue] started {address} ({}) now that what it waited for is done: {}. The tower watches it.",
                            item.name, item.text
                        ),
                        speech: format!("Started {}.", item.name),
                    });
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

    /// Everything waiting, oldest first.
    pub fn items(&self) -> Vec<Item> {
        self.lock().items.clone()
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
                            taken: false,
                            finished: false,
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
        let now = crate::history::now_ms();
        match h.look(session) {
            Look::Live(s) => {
                {
                    let mut b = self.lock();
                    b.unreachable.remove(session);
                    let mut changed = b.follow(&s);
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
                    if s.status != SessionStatus::Starting && !s.status.ready() && !s.status.passing() && b.waiting_for(session) > 0 {
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
                // Its last delivery is over, one way or another.
                if let Some(d) = b.delivered.iter_mut().rev().find(|d| d.session == session) {
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

    /// The session's next instruction, if it may take one now.
    fn deliver(&self, h: &dyn Sessions, s: &Session) {
        let now = crate::history::now_ms();
        let item = {
            let mut b = self.lock();
            if !b.may_send(s, now) {
                return;
            }
            let Some(pos) = b.items.iter().position(|i| i.session == s.address && !matches!(i.state(), ItemState::Parked(_))) else { return };
            let item = b.items[pos].clone();
            match b.after_done(&item) {
                Some(true) => {}
                Some(false) => return,
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
            // Claimed, on disk, before it's typed.
            if let Err(e) = b.items[pos].apply(ItemEv::Claim { now_ms: now, turn: s.turn }) {
                eprintln!("[queue] not sending #{}: {e}", item.id);
                return;
            }
            if let Err(e) = self.save(&b) {
                let _ = b.items[pos].apply(ItemEv::Release);
                eprintln!("[queue] not sending #{}: {e:#}", item.id);
                return;
            }
            b.items[pos].clone()
        };
        let result = h.send(&s.address, &item.text);
        let mut b = self.lock();
        match result {
            Ok(()) => {
                self.went_in(h, &mut b, &item, s, now);
                eprintln!("[queue] delivered #{} to {}", item.id, s.address);
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
            taken: false,
            finished: false,
        });
        let left = b.waiting_for(&s.address);
        h.watch(&s.address, &item.text);
        h.record(&s.address, &format!("Delivered to {}: {}", s.name, item.text), "delivered");
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
}

/// A live session's facts, as the queue needs them.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub address: String,
    pub name: String,
    pub status: SessionStatus,
    pub created_ms: Option<u64>,
    pub turn: Option<u64>,
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
    fn send(&self, session: &str, text: &str) -> Result<()>;
    /// A note for the brain, through the radar's outbox.
    fn tell(&self, note: Note);
    /// Watch the session: the brain hears when it stops.
    fn watch(&self, session: &str, text: &str);
    fn record(&self, session: &str, text: &str, kind: &str);
    /// Starts a session (`tower start <args>`); its address.
    fn start(&self, args: &[String]) -> Result<String>;
}

/// How the daemon hears what the queue does: set once at its start.
pub struct Wiring {
    pub tell: Box<dyn Fn(Note) + Send + Sync>,
    pub watch: Box<dyn Fn(&str, &str) + Send + Sync>,
}

static WIRING: OnceLock<Wiring> = OnceLock::new();

/// The daemon's queue, for the radar's agent list.
static RUNNING: OnceLock<SendQueue> = OnceLock::new();

/// Everything waiting in the daemon's queue (for the phone API).
pub fn current_items() -> Vec<Item> {
    RUNNING.get().map(|q| q.items()).unwrap_or_default()
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
    fn send(&self, session: &str, text: &str) -> Result<()> {
        hc::send(session, text)
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
}

fn session_of(t: &Value, asked: &str) -> Session {
    Session {
        address: t["address"].as_str().unwrap_or(asked).to_string(),
        name: hc::spoken_name(t),
        status: hc::status(t),
        created_ms: hc::created_ms(t),
        turn: hc::turn(t),
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
            format!("#{} → {} ({}, {status}{after}), queued {ago}: {}", i.id, i.session, i.name, i.text)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        sessions: Mutex<BTreeMap<String, (String, Option<u64>, Option<u64>)>>,
        unreachable: Mutex<bool>,
        sent: Mutex<Vec<(String, String)>>,
        notes: Mutex<Vec<String>>,
        watched: Mutex<Vec<String>>,
        send_fails: Mutex<bool>,
    }
    impl Fake {
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
                Some((st, created, turn)) => {
                    Look::Live(Session { address: session.into(), name: format!("{session}-name"), status: SessionStatus::parse(&st), created_ms: created, turn })
                }
                None => Look::Gone,
            }
        }
        fn send(&self, session: &str, text: &str) -> Result<()> {
            if *self.send_fails.lock().unwrap() {
                bail!("herdr is down");
            }
            self.sent.lock().unwrap().push((session.into(), text.into()));
            Ok(())
        }
        fn tell(&self, note: Note) {
            self.notes.lock().unwrap().push(note.msg);
        }
        fn watch(&self, session: &str, _: &str) {
            self.watched.lock().unwrap().push(session.into());
        }
        fn record(&self, _: &str, _: &str, _: &str) {}
        fn start(&self, args: &[String]) -> Result<String> {
            let name = args.iter().skip_while(|a| *a != "--name").nth(1).cloned().unwrap_or_default();
            self.sent.lock().unwrap().push((format!("start:{name}"), args.last().cloned().unwrap_or_default()));
            self.sessions.lock().unwrap().insert(name.clone(), ("working".into(), Some(7), Some(1)));
            Ok(name)
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
    fn a_free_session_gets_it_at_once_and_is_watched() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "idle");
        let (added, _, _) = q.add_with(&f, "t5", "run the tests", &[]).unwrap();
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
        assert!(matches!(q.add_with(&f, "t5", "first", &[]).unwrap().0, Added::Queued { position: 1, .. }));
        assert!(matches!(q.add_with(&f, "t5", "second", &[]).unwrap().0, Added::Queued { position: 2, .. }));
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
        q.add_with(&f, "t5", "after the dialog", &[]).unwrap();
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
        i.apply(ItemEv::Claim { now_ms: 5, turn: Some(3) }).unwrap();
        assert_eq!(i.state(), ItemState::Claimed { since_ms: 5, turn_before: Some(3) });
        assert_eq!(i.apply(ItemEv::Claim { now_ms: 6, turn: Some(3) }), Err(ItemRefused::GoingIn), "claimed once");
        assert_eq!(i.apply(ItemEv::Park { why: ParkWhy::Gone, now_ms: 6 }), Err(ItemRefused::GoingIn), "never parked while typed");
        assert_eq!(i.apply(ItemEv::Retarget { session: "t6".into(), name: "n".into(), created_ms: None }), Err(ItemRefused::GoingIn));
        i.apply(ItemEv::Failed).unwrap();
        assert_eq!((i.state(), i.tries), (ItemState::Waiting, 1), "the hand-over failed: waits again, one try more");
        i.apply(ItemEv::Park { why: ParkWhy::Gone, now_ms: 7 }).unwrap();
        assert_eq!((i.state(), i.parked_ms), (ItemState::Parked(ParkWhy::Gone), Some(7)));
        assert_eq!(i.apply(ItemEv::Claim { now_ms: 8, turn: None }), Err(ItemRefused::Parked(ParkWhy::Gone)), "parked goes nowhere");
        i.apply(ItemEv::Park { why: ParkWhy::Gone, now_ms: 99 }).unwrap();
        assert_eq!(i.parked_ms, Some(7), "parked again for the same reason: since when it first was");
        i.apply(ItemEv::Retarget { session: "t6".into(), name: "t6-name".into(), created_ms: Some(4) }).unwrap();
        assert_eq!((i.state(), i.session.as_str(), i.tries, i.parked_ms), (ItemState::Waiting, "t6", 0, None));
        let start = Item { start: vec!["--name".into(), "x".into()], after: vec![1], ..Default::default() };
        assert_eq!(start.state(), ItemState::WaitingFor(vec![1]));
        assert_eq!(Item { after: vec![], ..start }.state(), ItemState::ToStart);

        let mut d = Delivered { id: 1, session: "t5".into(), text: "x".into(), at_ms: 0, turn_before: Some(1), taken: false, finished: false };
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
        q.add_with(&f, "t5", "deploy the billing fix", &[]).unwrap();
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
        q.add_with(&f, "t5", "deploy the billing fix", &[]).unwrap();
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
        q.add_with(&f, "t5", "go on", &[]).unwrap();
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
        q.add_with(&f, "vps/t2", "run the migrations check", &[]).unwrap();
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
        let a = q.add_with(&f, "t5", "add a test", &[]).unwrap().0;
        let b = q.add_with(&f, "t5", "add a test", &[]).unwrap().0;
        assert_eq!(a, b);
        assert_eq!(q.items().len(), 1);
        f.set("t5", "idle");
        q.tick(&f);
        // A retry just after it went in: "sent", not again.
        assert!(matches!(q.add_with(&f, "t5", "add a test", &[]).unwrap().0, Added::Sent { .. }));
        assert_eq!(f.sent().len(), 1);
    }

    #[test]
    fn a_restart_between_claim_and_typing_never_types_twice() {
        let (q, d) = queue();
        let f = Fake::default();
        f.set("t5", "working");
        q.add_with(&f, "t5", "migrate the db", &[]).unwrap();
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
        q2.add_with(&f, "t5", "second", &[]).unwrap();
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
        q.add_with(&f, "t5", "try again", &[]).unwrap();
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
        assert!(q.add_with(&f, "t5", "ls", &[]).is_err());
    }

    #[test]
    fn after_waits_for_another_session_to_finish() {
        let (q, _d) = queue();
        let f = Fake::default();
        f.set("t5", "idle");
        f.set("t6", "idle");
        let first = match q.add_with(&f, "t5", "fix the api", &[]).unwrap().0 {
            Added::Sent { id } => id,
            other => panic!("{other:?}"),
        };
        let (added, _, _) = q.add_with(&f, "t6", "update the client for the new api", &[first]).unwrap();
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
        let Added::Queued { id: first, .. } = q.add_with(&f, "t5", "a", &[]).unwrap().0 else { panic!() };
        q.add_with(&f, "t6", "b", &[first]).unwrap();
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
            fn send(&self, s: &str, t: &str) -> Result<()> {
                self.0.send(s, t)
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
            q.add_with(&f.0, "t1", "hello", &[]).unwrap();
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
        let Added::Sent { id: api } = q.add_with(&f, "t5", "change the api", &[]).unwrap().0 else { panic!() };
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

    #[test]
    fn notes_from_an_earlier_version_go_out() {
        let (q, _d) = queue();
        let f = Fake::default();
        q.lock().notes.push("[queue] delivered to t5 (x): y".into());
        q.recover(&f);
        assert_eq!(f.notes(), vec!["[queue] delivered to t5 (x): y"]);
    }
}
