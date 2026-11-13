//! Moving work off a Claude account at its usage limit, in the daemon (see
//! `crate::failover`).
//!
//! - **What tells of a limit.** An agent session's StopFailure hook (space
//!   sessions, reviewers, the planner: `Request::Agent`, read as the event
//!   `limited`), and a tower's own (the main tower's, each space tower's:
//!   `Request::Limited` from `tower hook claude-failed`).
//! - **The move** (`limited` → `move_one`, in a thread): the account it ran
//!   on is marked out, the next one with usage is picked, and the session
//!   is resumed on it in its own terminal (`Hands`: Herdr keeps the
//!   terminal, the tower ends its `claude`, and types the resume into the
//!   shell Herdr left). A session then gets what it was doing once more
//!   through the queue (a quiet note: it waits its turn); a tower gets its
//!   unfinished prompt again, as it was. The user is told in one line (a
//!   chat message of kind `account`).
//! - **Meanwhile** (`holds`) the agenda leaves its task alone: the limit is
//!   no failure, and the session's next turn says how it went.
//! - **Every account out.** The session waits in the book, and one card
//!   (`Decisions::offer_accounts`) says when the first account resets.
//!   Every `TICK` the tower looks again: a session whose own account is
//!   back is told to go on; one that can move is moved. The card follows
//!   what still waits, and stops when nothing does. "Leave them stopped"
//!   lets them go.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::state::lock;
use crate::accounts::{Account, Usage};
use crate::failover::{self, Book, Limit, Moved, Pick, Waiting};
use crate::hc;

/// How often the sessions that wait for an account are looked at.
const TICK: Duration = Duration::from_secs(30);
/// How long the shell Herdr leaves may take to come up, and the resumed session to start.
const SHELL_WAIT_MS: u64 = 15_000;
const START_WAIT_MS: u64 = 45_000;

/// A session at its limit, as a hook tells of it.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Hit {
    /// Its terminal (`HC_TERMINAL_ID`).
    pub terminal: String,
    pub session_id: String,
    pub transcript: String,
    pub cwd: String,
    /// StopFailure's `error` and the message Claude Code showed.
    pub error: String,
    pub text: String,
    /// The tower it is (`tower`, `tower-<space>`), when it is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tower: Option<String>,
    /// What it was doing: its last prompt.
    pub prompt: String,
}

/// What failing over needs of Herdr, the mod and the rest of the tower.
pub trait Hands: Send + Sync {
    fn get(&self, address: &str) -> Option<Value>;
    /// Herdr keeps the terminal as a shell when its program dies (or stops keeping it).
    fn keep(&self, address: &str, on: bool) -> Result<()>;
    /// Ends the session's program at once (its turn is over: nothing is lost).
    fn end(&self, pid: u32) -> Result<()>;
    fn type_line(&self, address: &str, line: &str) -> Result<()>;
    /// Past a first-run question in a folder the session already worked in, until it is ready.
    fn settle(&self, address: &str);
    fn account_of(&self, t: &Value) -> Option<String>;
    fn accounts(&self) -> Result<(Vec<Account>, std::collections::BTreeMap<String, Usage>)>;
    fn cca(&self) -> Option<String>;
    /// What a session took last (its hooks).
    fn last_prompt(&self, address: &str) -> String;
    /// A quiet note for a session, through the queue, ahead of what waits for it there.
    fn note_session(&self, address: &str, text: &str) -> Result<()>;
    /// The session is the queue's again (it held what waited for it meanwhile).
    fn released(&self, address: &str);
    /// A tower's prompt, typed as the tower types its notes.
    fn note_tower(&self, name: &str, address: &str, space: Option<&str>, text: &str) -> Result<()>;
    /// The line the user is told.
    fn tell(&self, entry: crate::history::Entry);
    fn sleep(&self, ms: u64);
    fn now(&self) -> u64;
    /// Whether the tower leaves this terminal alone (a test session for the real tower, the rest for a test one).
    fn left_alone(&self, t: &Value) -> bool;
}

#[derive(Clone)]
pub struct Failover {
    book: Arc<Mutex<Book>>,
    path: PathBuf,
    decisions: super::decisions::Decisions,
    hands: Arc<dyn Hands>,
    /// The terminals being moved now.
    moving: Arc<Mutex<BTreeSet<String>>>,
    /// One look at the waiting at a time.
    busy: Arc<Mutex<()>>,
}

static RUNNING: OnceLock<Failover> = OnceLock::new();

/// The daemon's, once it runs (None in a command, and in tests).
pub fn get() -> Option<&'static Failover> {
    RUNNING.get()
}

/// The session is moving to another account, or waits for one: what it
/// was doing isn't judged meanwhile (its next turn says how it went).
pub fn holds(address: &str) -> bool {
    get().is_some_and(|f| f.holding(address))
}

fn program(t: &Value) -> String {
    t["foreground"]["argv"][0].as_str().unwrap_or("").rsplit('/').next().unwrap_or("").to_string()
}

impl Failover {
    pub fn open(decisions: super::decisions::Decisions, sendq: super::sendq::SendQueue) -> Failover {
        Failover::with(failover::path(), decisions, Arc::new(Real { sendq }))
    }

    pub fn with(path: PathBuf, decisions: super::decisions::Decisions, hands: Arc<dyn Hands>) -> Failover {
        let book = failover::load_from(&path);
        Failover { book: Arc::new(Mutex::new(book)), path, decisions, hands, moving: Arc::default(), busy: Arc::default() }
    }

    /// Runs it: the tick that looks at what waits for an account.
    pub fn start(&self) {
        let _ = RUNNING.set(self.clone());
        let me = self.clone();
        let _ = thread::Builder::new().name("failover".into()).spawn(move || {
            loop {
                thread::sleep(TICK);
                me.tick();
            }
        });
    }

    fn save(&self, b: &Book) {
        if let Err(e) = failover::save_to(&self.path, b) {
            eprintln!("[failover] {}: {e:#}", self.path.display());
        }
    }

    pub fn holding(&self, address: &str) -> bool {
        lock(&self.moving).contains(address) || lock(&self.book).waiting.iter().any(|w| w.address == address)
    }

    /// A session (or a tower) hit its usage limit. Whether the tower takes
    /// it on: it moves (in a thread), or waits for an account. False: it
    /// isn't a limit, nor a Claude session here the tower runs.
    pub fn limited(&self, hit: Hit) -> Value {
        let now = self.hands.now();
        let Some(limit) = failover::limit_of(&hit.error, &hit.text, now) else {
            return json!({ "moving": false, "why": "not a usage limit" });
        };
        let address = hit.terminal.trim().to_string();
        let t = match self.hands.get(&address).filter(|t| t["state"] == "live") {
            Some(t) if !address.contains('/') && t["machine"].is_null() => t,
            _ => return json!({ "moving": false, "why": "no live terminal of this machine" }),
        };
        if self.hands.left_alone(&t) {
            return json!({ "moving": false, "why": "a session this tower leaves alone" });
        }
        if program(&t) != "claude" && hc::agent_app(&t).as_deref() != Some("claude-code") {
            return json!({ "moving": false, "why": "no Claude Code session" });
        }
        if lock(&self.moving).contains(&address) {
            return json!({ "moving": true, "why": "already moving" });
        }
        let (list, usage) = match self.hands.accounts() {
            Ok(l) => l,
            Err(e) => return json!({ "moving": false, "why": format!("the accounts mod didn't answer: {e:#}") }),
        };
        if list.is_empty() {
            eprintln!("[failover] {address} hit its {}, and there is no other account: the accounts mod (cca) isn't installed", limit.what);
            return json!({ "moving": false, "why": "no accounts" });
        }
        let from = self.hands.account_of(&t).unwrap_or_else(|| list[0].name.clone());
        let until = limit.resets_ms.or_else(|| usage.get(&from).and_then(|u| u.spent_until(now)));
        let pick = {
            let mut b = lock(&self.book);
            b.mark_out(&from, &limit, until, now);
            let pick = failover::pick(&list, &usage, &b, Some(&from), now);
            self.save(&b);
            pick
        };
        // A tower's space: as the brains know it, else as its own folder's card says.
        let space = crate::spaces::space_of(&t)
            .or_else(|| hit.tower.as_deref().and_then(super::brains::space_of))
            .or_else(|| hit.tower.as_deref().and_then(crate::brains::card_of).map(|c| c.space));
        let name = hc::name(&t).unwrap_or(&address).to_string();
        let prompt = if hit.prompt.trim().is_empty() { self.hands.last_prompt(&address) } else { hit.prompt.clone() };
        eprintln!(
            "[failover] {address} ({name}) hit the {from} account's {}{}: {pick:?}",
            limit.what,
            until.map(|u| format!(", out until {}", failover::when(u, now))).unwrap_or_default()
        );
        match pick {
            Pick::To(to) => {
                lock(&self.moving).insert(address.clone());
                let me = self.clone();
                let answer = json!({ "moving": true, "from": from, "to": to });
                let _ = thread::Builder::new().name("failover-move".into()).spawn(move || {
                    me.carry(&address, &hit, &name, space, &from, &to, &limit, until, &prompt);
                    lock(&me.moving).remove(&address);
                    me.hands.released(&address);
                });
                answer
            }
            Pick::AllOut { first } => {
                self.wait(
                    Waiting {
                        address: address.clone(),
                        name,
                        space,
                        tower: hit.tower.clone(),
                        account: from.clone(),
                        since_ms: now,
                        limit,
                        prompt,
                    },
                    first,
                );
                json!({ "moving": false, "waiting": true, "from": from })
            }
            Pick::NoAccounts => json!({ "moving": false, "why": "no accounts" }),
        }
    }

    /// Moves it, sends what it was doing again and tells the user (or
    /// why it couldn't).
    #[allow(clippy::too_many_arguments)]
    fn carry(
        &self,
        address: &str,
        hit: &Hit,
        name: &str,
        space: Option<String>,
        from: &str,
        to: &str,
        limit: &Limit,
        until: Option<u64>,
        prompt: &str,
    ) {
        let now = self.hands.now();
        let who = failover::called(name, space.as_deref(), hit.tower.as_deref());
        let done = self.move_one(address, hit, to);
        let mut m = Moved {
            at_ms: now,
            address: address.to_string(),
            name: name.to_string(),
            space: space.clone(),
            tower: hit.tower.clone(),
            from: from.to_string(),
            to: to.to_string(),
            session_id: hit.session_id.clone(),
            how: "moved".into(),
            why: String::new(),
        };
        let (line, status) = match &done {
            Ok(()) => (failover::told(&who, from, to, limit, until, now), "moved"),
            Err(e) => {
                (m.how, m.why) = ("failed".into(), format!("{e:#}"));
                (format!("{} hit {from}'s {} and couldn't move to {to}: {e:#}.", failover::opening(&who), limit.what), "failed")
            }
        };
        eprintln!("[failover] {line}");
        {
            let mut b = lock(&self.book);
            b.moved(m);
            self.save(&b);
        }
        self.hands.tell(crate::history::Entry {
            kind: "account".into(),
            status: Some(status.into()),
            text: line,
            session: Some(address.to_string()),
            name: Some(name.to_string()),
            space: space.clone(),
            account: Some(if done.is_ok() { to } else { from }.to_string()),
            ..Default::default()
        });
        if done.is_ok() {
            self.go_on(address, name, space.as_deref(), hit.tower.as_deref(), from, to, limit, prompt);
        }
    }

    /// What it was doing, once more: a session's in a note that says why,
    /// a tower's own prompt as it was (its answer then pairs with the request).
    #[allow(clippy::too_many_arguments)]
    fn go_on(
        &self,
        address: &str,
        name: &str,
        space: Option<&str>,
        tower: Option<&str>,
        from: &str,
        to: &str,
        limit: &Limit,
        prompt: &str,
    ) {
        let sent = match tower {
            Some(t) if prompt.trim().is_empty() => {
                eprintln!("[failover] {t}: no unfinished prompt to send again");
                return;
            }
            Some(t) => self.hands.note_tower(t, address, space, prompt),
            None => self.hands.note_session(address, &failover::carry_on(from, to, limit, prompt)),
        };
        if let Err(e) = sent {
            eprintln!("[failover] {address} ({name}): what it was doing couldn't be sent again: {e:#}");
        }
    }

    /// The move itself: in the session's own terminal, its conversation resumed on `to`.
    fn move_one(&self, address: &str, hit: &Hit, to: &str) -> Result<()> {
        let h = &self.hands;
        let t = h.get(address).filter(|t| t["state"] == "live").context("its terminal ended")?;
        let session_id = Some(hit.session_id.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| t["labels"][crate::reopen::LABEL_SESSION].as_str().map(String::from))
            .context("its conversation isn't known, so it can't be resumed")?;
        let argv: Vec<String> =
            t["foreground"]["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let flags = crate::reopen::flags_of(&argv);
        let cwd = [hit.cwd.as_str(), t["foreground"]["cwd"].as_str().unwrap_or(""), t["cwd"].as_str().unwrap_or("")]
            .into_iter()
            .find(|c| !c.is_empty())
            .unwrap_or("")
            .to_string();
        let name = hc::name(&t).unwrap_or("").to_string();
        let cca = h.cca().context("the accounts mod's cca isn't installed")?;
        let pid = crate::accounts::claude_pid(&t)
            .or_else(|| t["foreground"]["pid"].as_u64().map(|p| p as u32))
            .context("no claude runs there")?;
        let was_kept = t["keep"].as_bool() == Some(true);
        h.keep(address, true)?;
        h.end(pid)?;
        // The shell Herdr leaves in its place.
        let mut waited = 0;
        loop {
            match h.get(address) {
                Some(t) if crate::reopen::kept_shell(&t) => break,
                Some(t) if t["state"] != "live" => bail!("its terminal ended instead of keeping a shell"),
                None => bail!("its terminal is gone"),
                _ if waited >= SHELL_WAIT_MS => bail!("no shell came up in its terminal"),
                _ => {}
            }
            h.sleep(300);
            waited += 300;
        }
        // The tower's own folders when they aren't the usual ones, and the planner's plan (it runs with it in its environment).
        let mut env = crate::plans::tower_env();
        if let Some(plan) = t["labels"][crate::plans::LABEL_PLAN].as_str().filter(|p| !p.is_empty()) {
            env.push((crate::plans::ENV_PLAN.to_string(), plan.to_string()));
        }
        let line = failover::resume_line(&cwd, &failover::resume_argv(&cca, to, &name, &flags, &session_id, &env));
        eprintln!("[failover] {address}: {line}");
        h.type_line(address, &line)?;
        let mut waited = 0;
        loop {
            let t = h.get(address);
            if t.as_ref().is_some_and(|t| program(t) == "claude") {
                break;
            }
            if t.as_ref().is_none_or(|t| t["state"] != "live") {
                bail!("its terminal ended as the session started on {to}");
            }
            if waited >= START_WAIT_MS {
                bail!("the session didn't start again on {to}");
            }
            h.sleep(500);
            waited += 500;
        }
        h.settle(address);
        if !was_kept {
            let _ = h.keep(address, false);
        }
        match h.get(address).and_then(|t| h.account_of(&t)) {
            Some(a) if a != to => eprintln!("[failover] {address} runs on {a}, not {to}"),
            _ => {}
        }
        Ok(())
    }

    /// Every account is out: it waits, and the card says when the first is back.
    fn wait(&self, w: Waiting, first: Option<(String, u64)>) {
        let now = self.hands.now();
        let mut b = lock(&self.book);
        b.hold = false;
        match b.waiting.iter_mut().find(|x| x.address == w.address) {
            Some(x) => *x = w,
            None => {
                // One more waits: the card is the user's again, even if they put the last one away.
                b.waiting.push(w);
                b.card_closed = false;
            }
        }
        self.offer(&mut b, first, now);
        self.save(&b);
    }

    /// The card, as what waits stands now. One the user answered or put
    /// away isn't raised again for the same sessions (`card_closed`).
    fn offer(&self, b: &mut Book, first: Option<(String, u64)>, now: u64) {
        if b.waiting.is_empty() {
            b.card_closed = false;
        }
        let out: Vec<(String, Option<u64>)> = b.out.iter().map(|(a, o)| (a.clone(), failover::out_until(o, now))).collect();
        let card = failover::card(&b.waiting, first.as_ref(), &out, now);
        if (card.is_none() || b.card_closed) && b.decision.is_none() {
            return;
        }
        match self.decisions.offer_accounts(card) {
            Some((id, told)) => {
                if b.decision.as_deref() != Some(id.as_str()) {
                    eprintln!("[failover] every account is out: {id} for the user{}", if told { " (told)" } else { "" });
                }
                b.decision = Some(id);
            }
            None => b.decision = None,
        }
    }

    /// What waits for an account: on, as soon as one has usage again.
    pub fn tick(&self) {
        let _one = lock(&self.busy);
        let now = self.hands.now();
        let (waiting, hold) = {
            let mut b = lock(&self.book);
            // The card answered or dismissed elsewhere: not raised again for these.
            if b.decision.as_deref().is_some_and(|id| self.decisions.get(id).is_none_or(|d| d.closed_ms.is_some())) {
                b.decision = None;
                b.card_closed = true;
            }
            let changed = b.tidy(now);
            if changed {
                self.save(&b);
            }
            (b.waiting.clone(), b.hold)
        };
        if waiting.is_empty() || hold {
            return;
        }
        let Ok((list, usage)) = self.hands.accounts() else { return };
        let mut left: Vec<Waiting> = vec![];
        let mut went: Vec<String> = vec![];
        let mut first: Option<(String, u64)> = None;
        for w in waiting {
            let Some(t) = self.hands.get(&w.address).filter(|t| t["state"] == "live" && program(t) == "claude") else {
                eprintln!("[failover] {} ({}) no longer runs: it waits no more", w.address, w.name);
                continue;
            };
            let own = self.hands.account_of(&t).unwrap_or_else(|| w.account.clone());
            let book = lock(&self.book).clone();
            if failover::account_out(&book, usage.get(&own), &own, now).is_none() {
                // Its own account is back: it goes on where it is.
                let line = failover::told_back(&failover::called(&w.name, w.space.as_deref(), w.tower.as_deref()), &own);
                eprintln!("[failover] {line}");
                self.hands.tell(crate::history::Entry {
                    kind: "account".into(),
                    status: Some("back".into()),
                    text: line,
                    session: Some(w.address.clone()),
                    name: Some(w.name.clone()),
                    space: w.space.clone(),
                    account: Some(own.clone()),
                    ..Default::default()
                });
                let note = match &w.tower {
                    Some(_) => w.prompt.clone(),
                    None => failover::carry_back(&own, &w.limit, &w.prompt),
                };
                if !note.trim().is_empty() {
                    let sent = match &w.tower {
                        Some(name) => self.hands.note_tower(name, &w.address, w.space.as_deref(), &note),
                        None => self.hands.note_session(&w.address, &note),
                    };
                    if let Err(e) = sent {
                        eprintln!("[failover] {}: couldn't be told to go on: {e:#}", w.address);
                    }
                }
                went.push(w.address.clone());
                continue;
            }
            match failover::pick(&list, &usage, &book, Some(&own), now) {
                Pick::To(to) => {
                    lock(&self.moving).insert(w.address.clone());
                    let hit = Hit { terminal: w.address.clone(), tower: w.tower.clone(), prompt: w.prompt.clone(), ..Default::default() };
                    let until = book.out.get(&own).and_then(|o| failover::out_until(o, now));
                    self.carry(&w.address, &hit, &w.name, w.space.clone(), &own, &to, &w.limit, until, &w.prompt);
                    lock(&self.moving).remove(&w.address);
                    went.push(w.address.clone());
                }
                Pick::AllOut { first: f } => {
                    first = match (first, f) {
                        (Some(a), Some(b)) => Some(if b.1 < a.1 { b } else { a }),
                        (a, b) => a.or(b),
                    };
                    left.push(w);
                }
                Pick::NoAccounts => left.push(w),
            }
        }
        {
            let mut b = lock(&self.book);
            b.waiting.retain(|w| left.iter().any(|l| l.address == w.address));
            self.offer(&mut b, first, now);
            self.save(&b);
        }
        for a in went {
            self.hands.released(&a);
        }
    }

    /// The card's answer: wait for the reset (the tower goes on by itself),
    /// or leave them stopped (they wait no more).
    pub fn answer(&self, wait: bool) -> Result<Value> {
        let (n, let_go) = {
            let mut b = lock(&self.book);
            let n = b.waiting.len();
            let mut let_go = vec![];
            if wait {
                b.hold = false;
            } else {
                let_go = b.waiting.drain(..).map(|w| w.address).collect();
                b.hold = true;
            }
            b.decision = None;
            b.card_closed = true;
            self.save(&b);
            (n, let_go)
        };
        for a in let_go {
            self.hands.released(&a);
        }
        eprintln!("[failover] the user says: {}", if wait { "go on at the reset" } else { "leave them stopped" });
        if wait {
            let me = self.clone();
            thread::spawn(move || me.tick());
        }
        Ok(json!({ "waiting": if wait { n } else { 0 }, "left": if wait { 0 } else { n } }))
    }

    /// `tower failover`: what is out, what moved, what waits.
    pub fn status(&self) -> Value {
        let now = self.hands.now();
        let b = lock(&self.book).clone();
        let (list, usage) = self.hands.accounts().unwrap_or_default();
        let accounts: Vec<Value> = list
            .iter()
            .map(|a| {
                let out = failover::account_out(&b, usage.get(&a.name), &a.name, now);
                json!({ "name": a.name, "email": a.email, "out_until_ms": out, "out_until": out.map(|u| failover::when(u, now)), "usage": usage.get(&a.name) })
            })
            .collect();
        json!({ "accounts": accounts, "out": b.out, "moves": b.moves, "waiting": b.waiting, "decision": b.decision, "moving": lock(&self.moving).iter().cloned().collect::<Vec<_>>() })
    }

    /// `tower failover move <session> [--to <account>]`: the user moves one by hand.
    pub fn move_by_hand(&self, address: &str, to: Option<&str>) -> Result<Value> {
        let t = self.hands.get(address).filter(|t| t["state"] == "live").with_context(|| format!("no live terminal {address}"))?;
        let address = t["address"].as_str().unwrap_or(address).to_string();
        if program(&t) != "claude" {
            bail!("{address} runs no Claude Code session");
        }
        if lock(&self.moving).contains(&address) {
            bail!("{address} is moving already");
        }
        let (list, usage) = self.hands.accounts()?;
        let from = self.hands.account_of(&t).or_else(|| list.first().map(|a| a.name.clone())).unwrap_or_default();
        let to = match to {
            Some(w) => crate::accounts::resolve(Some(w), &list)?.context("no accounts")?,
            None => match failover::pick(&list, &usage, &lock(&self.book), Some(&from), self.hands.now()) {
                Pick::To(a) => a,
                _ => bail!("no other account has usage left"),
            },
        };
        if to == from {
            bail!("{address} runs on {to} already");
        }
        let registry = super::sessions::registry().get(&address);
        let tower = hc::name(&t).filter(|n| crate::brains::is_space_brain(n) || *n == crate::brain::name()).map(String::from);
        let hit = Hit {
            terminal: address.clone(),
            session_id: registry.as_ref().map(|s| s.session_id.clone()).unwrap_or_default(),
            cwd: registry.as_ref().map(|s| s.cwd.clone()).unwrap_or_default(),
            tower: tower.clone(),
            ..Default::default()
        };
        let name = hc::name(&t).unwrap_or(&address).to_string();
        let space = crate::spaces::space_of(&t);
        lock(&self.moving).insert(address.clone());
        let me = self.clone();
        let (a, f, to2) = (address.clone(), from.clone(), to.clone());
        thread::spawn(move || {
            let done = me.move_one(&a, &hit, &to2);
            let now = me.hands.now();
            let line = match &done {
                Ok(()) => format!(
                    "{} moved from the {f} account to {to2}, as you asked.",
                    failover::opening(&failover::called(&name, space.as_deref(), hit.tower.as_deref()))
                ),
                Err(e) => format!(
                    "{} couldn't move to {to2}: {e:#}.",
                    failover::opening(&failover::called(&name, space.as_deref(), hit.tower.as_deref()))
                ),
            };
            eprintln!("[failover] {line}");
            let mut b = lock(&me.book);
            b.moved(Moved {
                at_ms: now,
                address: a.clone(),
                name: name.clone(),
                space: space.clone(),
                tower: hit.tower.clone(),
                from: f.clone(),
                to: to2.clone(),
                session_id: hit.session_id.clone(),
                how: if done.is_ok() { "moved".into() } else { "failed".into() },
                why: done.err().map(|e| format!("{e:#}")).unwrap_or_default(),
            });
            me.save(&b);
            drop(b);
            lock(&me.moving).remove(&a);
            me.hands.released(&a);
        });
        Ok(json!({ "moving": address, "from": from, "to": to }))
    }

    /// `tower failover clear`: forget what is out and what waits.
    pub fn clear(&self) -> Value {
        let mut b = lock(&self.book);
        let n = (b.out.len(), b.waiting.len());
        b.out.clear();
        for w in b.waiting.drain(..) {
            self.hands.released(&w.address);
        }
        b.hold = false;
        let mut card = None;
        std::mem::swap(&mut card, &mut b.decision);
        if card.is_some() {
            let _ = self.decisions.offer_accounts(None);
        }
        self.save(&b);
        json!({ "forgot_out": n.0, "forgot_waiting": n.1 })
    }
}

/// Herdr, the mod and the tower, for real.
struct Real {
    sendq: super::sendq::SendQueue,
}

impl Hands for Real {
    fn get(&self, address: &str) -> Option<Value> {
        hc::get(address)
    }

    fn keep(&self, address: &str, on: bool) -> Result<()> {
        let out = if on { hc::run(&["keep", address])? } else { hc::run(&["keep", address, "--off"])? };
        if out.code != 0 {
            bail!("hc keep: {}{}", out.stderr.trim(), out.stdout.trim());
        }
        Ok(())
    }

    fn end(&self, pid: u32) -> Result<()> {
        // SIGKILL: a signal's death is what Herdr keeps a shell for (Claude Code
        // ends cleanly on SIGTERM, and Herdr takes that as the terminal's end).
        // Its turn is over and its conversation is on disk.
        // SAFETY: plain kill(2) on a pid Herdr says runs in the terminal.
        if pid == 0 || unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } != 0 {
            bail!("couldn't end its claude (pid {pid}): {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn type_line(&self, address: &str, line: &str) -> Result<()> {
        let out = hc::run(&["send", address, line])?;
        if out.code != 0 {
            bail!("hc send: {}{}", out.stderr.trim(), out.stdout.trim());
        }
        Ok(())
    }

    fn settle(&self, address: &str) {
        crate::brain::settle(address);
    }

    fn account_of(&self, t: &Value) -> Option<String> {
        crate::accounts::of_terminal(t)
    }

    fn accounts(&self) -> Result<(Vec<Account>, std::collections::BTreeMap<String, Usage>)> {
        crate::accounts::list_with_usage()
    }

    fn cca(&self) -> Option<String> {
        crate::accounts::cca_bin().map(|p| p.to_string_lossy().into_owned())
    }

    fn last_prompt(&self, address: &str) -> String {
        super::sessions::registry().get(address).map(|s| s.prompt).unwrap_or_default()
    }

    fn note_session(&self, address: &str, text: &str) -> Result<()> {
        self.sendq.add_note_first(address, text).map(|_| ())
    }

    fn released(&self, _address: &str) {
        self.sendq.kick();
    }

    fn note_tower(&self, name: &str, address: &str, space: Option<&str>, text: &str) -> Result<()> {
        let to = crate::brain::Target { name: name.to_string(), terminal: address.to_string(), space: space.map(String::from) };
        // Ready first: it was only just started again.
        for _ in 0..40 {
            if hc::get(address).and_then(|t| hc::status_word(&t)).is_some_and(|w| w == "idle" || w == "done") {
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        crate::brain::note_to(&to, text, false)
    }

    fn tell(&self, entry: crate::history::Entry) {
        crate::history::later(move || crate::history::append(entry));
    }

    fn sleep(&self, ms: u64) {
        thread::sleep(Duration::from_millis(ms));
    }

    fn now(&self) -> u64 {
        crate::history::now_ms()
    }

    fn left_alone(&self, t: &Value) -> bool {
        super::watch::left_alone(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::{Shared, State};
    use std::collections::BTreeMap;

    /// Herdr and the mod, as a test has them: one terminal, whose program a
    /// signal ends into a shell, and that a typed resume starts again.
    #[derive(Default)]
    struct Fake {
        t: Mutex<Value>,
        accounts: Vec<Account>,
        usage: BTreeMap<String, Usage>,
        calls: Mutex<Vec<String>>,
        now: u64,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            lock(&self.calls).clone()
        }
        fn call(&self, c: String) {
            lock(&self.calls).push(c);
        }
    }

    impl Hands for Fake {
        fn get(&self, address: &str) -> Option<Value> {
            let t = lock(&self.t).clone();
            (t["address"] == address).then_some(t)
        }
        fn keep(&self, address: &str, on: bool) -> Result<()> {
            self.call(format!("keep {address} {on}"));
            lock(&self.t)["keep"] = on.into();
            Ok(())
        }
        fn end(&self, pid: u32) -> Result<()> {
            self.call(format!("end {pid}"));
            let mut t = lock(&self.t);
            t["foreground"] = json!({ "argv": ["/bin/zsh", "-l"], "pid": 900, "cwd": "/w/app" });
            t["pid"] = 900.into();
            t["status"] = Value::Null;
            Ok(())
        }
        fn type_line(&self, address: &str, line: &str) -> Result<()> {
            self.call(format!("type {address} {line}"));
            let account = line.split(" exec ").nth(1).and_then(|c| c.split(' ').nth(1)).unwrap_or("").to_string();
            let mut t = lock(&self.t);
            t["foreground"] = json!({ "argv": ["claude", "-n", "fix-login", "--resume", "s-1"], "pid": 901, "cwd": "/w/app" });
            t["status"] = json!({ "state": "idle", "app": "claude-code" });
            t["account"] = account.into();
            Ok(())
        }
        fn settle(&self, address: &str) {
            self.call(format!("settle {address}"));
        }
        fn account_of(&self, t: &Value) -> Option<String> {
            t["account"].as_str().map(String::from)
        }
        fn accounts(&self) -> Result<(Vec<Account>, BTreeMap<String, Usage>)> {
            Ok((self.accounts.clone(), self.usage.clone()))
        }
        fn cca(&self) -> Option<String> {
            Some("/u/cca".into())
        }
        fn last_prompt(&self, _: &str) -> String {
            "fix the login bug".into()
        }
        fn note_session(&self, address: &str, text: &str) -> Result<()> {
            self.call(format!("note {address} {text}"));
            Ok(())
        }
        fn released(&self, _: &str) {}
        fn note_tower(&self, name: &str, address: &str, _: Option<&str>, text: &str) -> Result<()> {
            self.call(format!("tower {name} {address} {text}"));
            Ok(())
        }
        fn tell(&self, e: crate::history::Entry) {
            self.call(format!("tell {} {}", e.status.unwrap_or_default(), e.text));
        }
        fn sleep(&self, _: u64) {}
        fn now(&self) -> u64 {
            self.now
        }
        fn left_alone(&self, _: &Value) -> bool {
            false
        }
    }

    fn acc(name: &str) -> Account {
        Account { name: name.into(), email: format!("{name}@example.com") }
    }

    fn session(address: &str, name: &str) -> Value {
        json!({
            "address": address, "state": "live", "name": name, "keep": false, "pid": 77, "cwd": "/w/app",
            "foreground": { "argv": ["claude", "-n", name, "--model", "claude-opus-5-5"], "pid": 77, "cwd": "/w/app" },
            "status": { "state": "idle", "app": "claude-code" }, "account": "default",
            "labels": { "tower.session": "s-1" }
        })
    }

    fn setup(fake: Fake) -> (Failover, Arc<Fake>, super::super::decisions::Decisions, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tower-failover-{}-{}", std::process::id(), fake.now));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let shared = Shared::new(State::new("ptt", "test", "test"));
        let decisions = super::super::decisions::Decisions::at(shared, dir.join("decisions.json"));
        let fake = Arc::new(fake);
        let f = Failover::with(dir.join("failover.json"), decisions.clone(), fake.clone());
        (f, fake, decisions, dir)
    }

    fn hit(address: &str) -> Hit {
        Hit {
            terminal: address.into(),
            session_id: "s-1".into(),
            cwd: "/w/app".into(),
            error: "rate_limit".into(),
            text: "You've hit your 5-hour limit · resets 3pm".into(),
            ..Default::default()
        }
    }

    fn settled(f: &Failover, address: &str) {
        for _ in 0..200 {
            if !lock(&f.moving).contains(address) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("{address} never finished moving");
    }

    #[test]
    fn a_session_at_its_limit_resumes_on_the_next_account_in_its_own_terminal() {
        let now = 1_794_580_000_000;
        let (f, fake, _, dir) = setup(Fake {
            t: Mutex::new(session("t5", "fix-login")),
            accounts: vec![acc("default"), acc("orca")],
            now,
            ..Default::default()
        });
        let v = f.limited(hit("t5"));
        assert_eq!((v["moving"].as_bool(), v["from"].as_str(), v["to"].as_str()), (Some(true), Some("default"), Some("orca")), "{v}");
        assert!(f.holding("t5"), "the agenda leaves its task alone meanwhile");
        settled(&f, "t5");
        let calls = fake.calls();
        assert_eq!(
            calls[..4],
            [
                "keep t5 true".to_string(),
                "end 77".into(),
                "type t5 cd /w/app && exec /u/cca orca -n fix-login --model claude-opus-5-5 --resume s-1".into(),
                "settle t5".into()
            ]
        );
        assert_eq!(calls[4], "keep t5 false", "kept as it was");
        assert!(
            calls[5].starts_with("tell moved fix-login moved from the default account to orca: default hit its 5-hour limit (back at "),
            "{}",
            calls[5]
        );
        assert!(
            calls[6].starts_with("note t5 The Claude account default hit its 5-hour limit, so the tower moved you to the orca account")
                && calls[6].ends_with("fix the login bug"),
            "{}",
            calls[6]
        );
        assert_eq!(calls.len(), 7, "told once: {calls:?}");
        assert!(!f.holding("t5"));
        let b = failover::load_from(&dir.join("failover.json"));
        assert!(b.out.contains_key("default") && b.moves.len() == 1 && b.moves[0].to == "orca" && b.waiting.is_empty());
        // Not a limit: nothing happens.
        let mut plain = hit("t5");
        plain.text = "API Error: overloaded".into();
        assert_eq!(f.limited(plain)["moving"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tower_gets_its_unfinished_prompt_again() {
        let now = 1_794_580_100_000;
        let mut t = session("t9", "tower-sandbox");
        t["cwd"] = "/brains/sandbox".into();
        let (f, fake, _, dir) = setup(Fake { t: Mutex::new(t), accounts: vec![acc("default"), acc("orca")], now, ..Default::default() });
        let mut h = hit("t9");
        (h.tower, h.prompt) = (Some("tower-sandbox".into()), "[voice] how is the sandbox doing?".into());
        assert_eq!(f.limited(h)["to"], "orca");
        settled(&f, "t9");
        let calls = fake.calls();
        assert!(
            calls.iter().any(|c| c.starts_with("tell moved The tower moved from the default account to orca")
                || c.starts_with("tell moved The sandbox tower moved")),
            "{calls:?}"
        );
        assert_eq!(calls.last().unwrap(), "tower tower-sandbox t9 [voice] how is the sandbox doing?", "its own prompt, as it was");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_account_out_is_one_decision_and_the_session_goes_on_at_the_reset() {
        let now = 1_794_580_200_000;
        let mut usage = BTreeMap::new();
        // orca is spent too, back in ten minutes.
        usage.insert(
            "orca".to_string(),
            Usage { five_hour: Some(crate::accounts::Window { used: 100.0, resets_ms: Some(now + 600_000) }), seven_day: None },
        );
        let (f, fake, decisions, dir) = setup(Fake {
            t: Mutex::new(session("t5", "fix-login")),
            accounts: vec![acc("default"), acc("orca")],
            usage,
            now,
            ..Default::default()
        });
        let v = f.limited(hit("t5"));
        assert_eq!((v["moving"].as_bool(), v["waiting"].as_bool()), (Some(false), Some(true)), "{v}");
        let list = decisions.list(None, false);
        assert_eq!(list["count"], 1, "{list}");
        let d = &list["decisions"][0];
        assert!(
            d["title"].as_str().unwrap().starts_with("Every Claude account is out of usage: fix-login waits. The first back is orca, at "),
            "{d}"
        );
        assert!(f.holding("t5"));
        // A second hit of the same session: still one decision.
        f.limited(hit("t5"));
        assert_eq!(decisions.list(None, false)["count"], 1);
        assert!(fake.calls().is_empty(), "nothing moved, nothing typed: {:?}", fake.calls());
        // Ten minutes later orca is back: the session moves there, and the card stops.
        let later = Fake {
            t: Mutex::new(session("t5", "fix-login")),
            accounts: vec![acc("default"), acc("orca")],
            now: now + 601_000,
            ..Default::default()
        };
        let f2 = Failover::with(dir.join("failover.json"), decisions.clone(), Arc::new(later));
        f2.tick();
        assert!(!f2.holding("t5"));
        assert_eq!(decisions.list(None, false)["count"], 0, "the card stops once nothing waits");
        let b = failover::load_from(&dir.join("failover.json"));
        assert!(b.waiting.is_empty() && b.moves.last().is_some_and(|m| m.to == "orca" && m.how == "moved"), "{b:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_card_put_away_isnt_raised_again_until_another_session_waits() {
        let now = 1_794_580_400_000;
        let (f, _, decisions, dir) =
            setup(Fake { t: Mutex::new(session("t5", "fix-login")), accounts: vec![acc("default")], now, ..Default::default() });
        f.limited(hit("t5"));
        let id = decisions.list(None, false)["decisions"][0]["id"].as_str().unwrap().to_string();
        decisions.dismiss(&id).unwrap();
        f.tick();
        assert_eq!(decisions.list(None, false)["count"], 0, "dismissed: not raised again for the same session");
        assert!(f.holding("t5"), "it still waits, and goes on at the reset");
        // Another session at its limit: the card is back, with both.
        *lock(&f.book) = {
            let mut b = lock(&f.book).clone();
            b.waiting[0].address = "t4".into();
            b
        };
        f.limited(hit("t5"));
        let list = decisions.list(None, false);
        assert_eq!(list["count"], 1, "{list}");
        assert!(list["decisions"][0]["title"].as_str().unwrap().contains("2 sessions wait"), "{list}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leave_them_stopped_lets_them_go() {
        let now = 1_794_580_300_000;
        let (f, _, decisions, dir) =
            setup(Fake { t: Mutex::new(session("t5", "fix-login")), accounts: vec![acc("default")], now, ..Default::default() });
        assert_eq!(f.limited(hit("t5"))["waiting"], true, "one account, and it is out");
        assert_eq!(decisions.list(None, false)["count"], 1);
        f.answer(false).unwrap();
        assert!(!f.holding("t5"));
        f.tick();
        let b = failover::load_from(&dir.join("failover.json"));
        assert!(b.waiting.is_empty() && b.hold);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
