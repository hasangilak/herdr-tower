//! The radar: follows every terminal through `hc events` (this machine) and
//! `hc ls` over `hc exec --on` (other machines). It keeps the brain's status
//! and the agent list current, says out loud when an agent needs the user,
//! and fires the brain's watches.

use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::book::{self, Book, Now, OpenJob, Owed, Pending, Watch};
use super::state::{AgentLine, Shared};
use super::tts::Mouth;
use crate::brain;
use crate::config::{self, Alerts};
use crate::hc::{self, SessionStatus};

#[derive(Clone, Debug)]
struct Seen {
    name: String,
    status: SessionStatus,
    /// When its terminal was made (another one at the same address is another session).
    created_ms: Option<u64>,
    /// Its turn count.
    turn: Option<u64>,
    /// The folder it works in.
    cwd: String,
    app: String,
    /// The Claude account it runs under (see `accounts`), when known.
    account: Option<String>,
    live: bool,
    /// The Herdr space it's in, for focus (see `spaces`).
    space: Option<String>,
    /// A brain's terminal (the main one, or a space's own), by its name:
    /// the tower itself, never an agent to watch, ask about or report on.
    brain: Option<String>,
}

impl Seen {
    /// A terminal's facts from one look at it, on what the radar knew
    /// (`before`). Herdr describes a terminal in two shapes: whole (`hc get`,
    /// the snapshot `hc events` begins with: its status with the program
    /// that reported it, its labels, its foreground program, when it was
    /// made), and as a row of `hc ls` (its status a word, and no labels, no
    /// pids, no dates). What this look can't say is kept as it was known,
    /// never blanked.
    fn of(t: &Value, machine: Option<&str>, before: Option<&Seen>) -> Seen {
        let status = hc::status(t);
        let app = match &t["status"] {
            Value::Object(_) => hc::agent_app(t).unwrap_or_default(),
            // A row: an agent reports, and the row doesn't say which program it is.
            // (Its `agent` does for a terminal here that was never read whole.)
            Value::String(_) => before
                .map(|b| b.app.clone())
                .filter(|a| !a.is_empty())
                .or_else(|| t["agent"].as_str().filter(|_| machine.is_none()).map(String::from))
                .unwrap_or_default(),
            // No status: a plain program now.
            _ => String::new(),
        };
        let labelled = t["labels"].is_object();
        let name = match (hc::name(t), before) {
            // A row has no labels, so not the name `tower start --space` gave (`tower.name`).
            (None, Some(b)) if !labelled => b.name.clone(),
            _ => match machine {
                Some(m) => format!("{} on {m}", hc::spoken_name(t)),
                None => hc::spoken_name(t),
            },
        };
        let space = crate::spaces::space_of(t)
            .or_else(|| t["where"].as_str().and_then(|w| w.split(" / ").next()).map(str::trim).filter(|s| !s.is_empty()).map(String::from))
            .or_else(|| before.filter(|_| !labelled).and_then(|b| b.space.clone()));
        let account = match (machine, t["foreground"].is_object()) {
            (None, true) => crate::accounts::of_terminal(t),
            _ => before.and_then(|b| b.account.clone()),
        };
        let cwd = t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str()).map(String::from).or_else(|| before.map(|b| b.cwd.clone())).unwrap_or_default();
        // A brain is known by its name, here only (a look that doesn't give the name keeps what was known).
        let brain = match (brain::brain_of(t), machine) {
            (Some(n), None) => Some(n.to_string()),
            (None, None) if !labelled || t["name"].is_null() => before.and_then(|b| b.brain.clone()),
            _ => None,
        };
        Seen {
            brain,
            name,
            status,
            created_ms: hc::created_ms(t).or_else(|| before.and_then(|b| b.created_ms)),
            turn: hc::turn(t).or_else(|| before.and_then(|b| b.turn)),
            cwd,
            app,
            account,
            live: t["state"].as_str().is_none_or(|s| s == "live"),
            space,
        }
    }
}

/// Whether a row of `hc ls` says something the radar doesn't know: a
/// terminal it hasn't seen (or has never read whole), or one whose status,
/// turn or liveness moved without an event saying so.
fn row_news(known: Option<&Seen>, row: &Value) -> bool {
    let Some(s) = known else { return true };
    let live = row["state"].as_str().is_none_or(|x| x == "live");
    s.created_ms.is_none() || s.live != live || s.status != hc::status(row) || hc::turn(row).is_some_and(|n| s.turn != Some(n))
}

/// Terminals the brain asked to hear about, what each was last seen doing,
/// and reports not yet taken: the radar's book (see `book`), kept in
/// `watches.json` so a restarted voice still reports back, once.
#[derive(Clone)]
pub struct Watches {
    book: Arc<Mutex<Book>>,
    radar: Arc<OnceLock<Radar>>,
    /// One delivery at a time, so no report goes out twice.
    sending: Arc<Mutex<()>>,
    /// Wakes the outbox thread (a report is owed now).
    kick: Arc<(Mutex<bool>, Condvar)>,
}

impl Default for Watches {
    fn default() -> Self {
        Self {
            book: Arc::new(Mutex::new(Book::load_from(&Book::path()))),
            radar: Arc::new(OnceLock::new()),
            sending: Arc::new(Mutex::new(())),
            kick: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Watches {
    fn save(book: &Book) {
        book.save_to(&Book::path());
    }

    /// Watches a session (by its address or name): the brain hears when it
    /// stops. Returns its address. A session hc says isn't there is an error;
    /// one on a machine out of reach is watched as asked.
    pub fn add(&self, terminal: &str, note: &str, account: Option<String>) -> anyhow::Result<String> {
        let (address, created_ms, turn, status) = match hc::lookup(terminal) {
            hc::Lookup::Found(t) if t["state"] == "live" => (
                t["address"].as_str().unwrap_or(terminal).to_string(),
                hc::created_ms(&t),
                hc::turn(&t),
                hc::status(&t),
            ),
            hc::Lookup::Found(_) => anyhow::bail!("{terminal} has ended: nothing to watch"),
            hc::Lookup::Gone => anyhow::bail!("no session {terminal}"),
            hc::Lookup::Unreachable(e) => {
                eprintln!("[radar] watching {terminal}, out of reach right now: {e}");
                (terminal.to_string(), None, None, SessionStatus::Starting)
            }
        };
        let (t, n, acc) = (address.clone(), note.to_string(), account.clone());
        crate::history::later(move || {
            crate::history::job_start_on(&t, &n, acc);
        });
        {
            let mut b = lock(&self.book);
            let last = if status == SessionStatus::Starting { b.seen.get(&address).cloned().unwrap_or_default() } else { status };
            b.watches.insert(address.clone(), Watch { note: note.to_string(), since_ms: crate::history::now_ms(), last, account, created_ms, turn });
            Self::save(&b);
        }
        if let Some(r) = self.radar.get() {
            r.recheck(&address);
        }
        Ok(address)
    }

    /// A session is being ended on the user's word (`tower undo`): its watch
    /// goes with it, and nothing is reported about its end.
    pub fn forget(&self, address: &str) {
        let mut b = lock(&self.book);
        if b.watches.remove(address).is_some() {
            Self::save(&b);
        }
    }

    /// A note from the send queue for the brain: through the outbox, so it
    /// waits for a brain that's away and a space out of focus holds it.
    pub fn tell(&self, note: super::sendq::Note) {
        let space = self.radar.get().and_then(|r| lock(&r.seen).get(&note.session).and_then(|s| s.space.clone()));
        let p = Pending {
            address: note.session,
            name: note.name,
            space,
            status: "news".into(),
            msg: note.msg,
            speech: note.speech,
            kind: "queue".into(),
            at_ms: crate::history::now_ms(),
            spoken: false,
        };
        if self.owe(p) {
            self.kick();
        }
    }

    /// News for the brain about work it handed out (a PR the user merged):
    /// through the outbox as a report of kind `pr`, so a space out of focus
    /// doesn't hold it.
    pub fn news(&self, address: &str, name: &str, space: Option<String>, msg: String, speech: String) {
        let p = Pending {
            address: address.into(),
            name: name.into(),
            space,
            status: "news".into(),
            msg,
            speech,
            kind: "pr".into(),
            at_ms: crate::history::now_ms(),
            spoken: false,
        };
        if self.owe(p) {
            self.kick();
        }
    }

    /// A note from the agenda ledger for the brain: through the outbox, as
    /// work the user asked for (a space out of focus marks it, and lets it through).
    pub fn tell_agenda(&self, note: crate::agenda::Note) {
        let space = note.space.or_else(|| self.radar.get().and_then(|r| lock(&r.seen).get(&note.session).and_then(|s| s.space.clone())));
        let p = Pending {
            address: note.session,
            name: note.name,
            space,
            status: "news".into(),
            msg: note.msg,
            speech: note.speech,
            kind: "agenda".into(),
            at_ms: crate::history::now_ms(),
            spoken: false,
        };
        if self.owe(p) {
            self.kick();
        }
    }

    /// A decision the brain raised for the user (`tower raise`): told as a
    /// session that needs them is (the alert, and said once), held for
    /// another space in focus as usual. False: the radar isn't up yet.
    pub fn raise(&self, address: &str, text: &str) -> bool {
        match self.radar.get() {
            Some(r) => {
                r.announce(address, text);
                true
            }
            None => false,
        }
    }

    /// What an agent session's hook said (see `sessions`).
    pub fn agent(&self, ev: &super::sessions::AgentEvent, change: &super::sessions::Change) {
        if let Some(r) = self.radar.get() {
            r.agent(ev, change);
        }
    }

    /// The outbox thread: deliver now.
    fn kick(&self) {
        let (m, cv) = &*self.kick;
        *lock(m) = true;
        cv.notify_all();
    }

    /// The watch fires: out of the watches, its report into the outbox, in
    /// one write. None when it had fired already (each finish is told once).
    fn fire(&self, address: &str, report: impl FnOnce(&Watch) -> Pending) -> Option<Pending> {
        let mut b = lock(&self.book);
        let p = b.fire(address, report)?;
        Self::save(&b);
        Some(p)
    }

    /// A report owed without a watch.
    fn owe(&self, p: Pending) -> bool {
        let mut b = lock(&self.book);
        let added = b.owe(p);
        if added {
            Self::save(&b);
        }
        added
    }

    /// Remembers a session's status (for the next start's reconcile).
    fn seen(&self, address: &str, status: &SessionStatus) {
        let mut b = lock(&self.book);
        let mut changed = b.note_seen(address, status);
        if let Some(w) = b.watches.get_mut(address)
            && w.last != *status
        {
            w.last = status.clone();
            changed = true;
        }
        if changed {
            Self::save(&b);
        }
    }

    fn watched(&self, terminal: &str) -> bool {
        lock(&self.book).watches.contains_key(terminal)
    }
    fn watch(&self, terminal: &str) -> Option<Watch> {
        lock(&self.book).watches.get(terminal).cloned()
    }
    fn snapshot(&self) -> Book {
        lock(&self.book).clone()
    }
    fn remote(&self) -> Vec<String> {
        lock(&self.book).watches.keys().filter(|k| k.contains('/')).cloned().collect()
    }
    fn all(&self) -> Vec<String> {
        lock(&self.book).watches.keys().cloned().collect()
    }

    /// Hands the outbox to `send`, outside the lock (it talks to Herdr);
    /// what it couldn't hand over stays for the next try.
    fn deliver(&self, send: impl FnMut(&mut Pending) -> bool) {
        let _one = lock(&self.sending);
        let mut pending = {
            let b = lock(&self.book);
            if b.outbox.is_empty() {
                return;
            }
            Book { outbox: b.outbox.clone(), ..Default::default() }
        };
        let tried = pending.outbox.clone();
        if !pending.deliver(crate::history::now_ms(), send) {
            return;
        }
        let mut b = lock(&self.book);
        // Keep reports added meanwhile; drop the ones taken (or gone stale).
        b.outbox.retain(|p| !tried.contains(p));
        let mut rest = pending.outbox;
        rest.append(&mut b.outbox);
        b.outbox = rest;
        Self::save(&b);
    }
}

#[derive(Clone)]
struct Radar {
    shared: Shared,
    mouth: Mouth,
    alerts: Alerts,
    watches: Watches,
    asks: super::asks::Asks,
    /// What needs the user, one decision each: told when a session works again or is gone.
    decisions: super::decisions::Decisions,
    seen: Arc<Mutex<HashMap<String, Seen>>>,
    /// Sessions that stopped to wait for the user (a watch's report said so):
    /// their job stays blocked until they work again or end.
    waiting: Arc<Mutex<std::collections::HashSet<String>>>,
}

pub fn start(shared: Shared, mouth: Mouth, alerts: Alerts, watches: Watches, asks: super::asks::Asks, decisions: super::decisions::Decisions) {
    // What was known when the last run ended, before this run's first look updates it.
    let before = watches.snapshot();
    let radar =
        Radar { shared, mouth, alerts, watches: watches.clone(), asks, decisions, seen: Arc::new(Mutex::new(HashMap::new())), waiting: Arc::default() };
    let _ = watches.radar.set(radar.clone());
    let r = radar.clone();
    radar.asks.set_notify(move |address, text| r.announce(address, text));
    // The cards the last run left, settled before the first look reads the dialogs again.
    radar.asks.restore();
    // A dialog answered tab by tab in the terminal: its card follows.
    radar.asks.follow();
    let r = radar.clone();
    thread::Builder::new().name("reconcile".into()).spawn(move || r.reconcile(before)).ok();
    let r = radar.clone();
    thread::Builder::new()
        .name("outbox".into())
        .spawn(move || {
            loop {
                // Every few seconds, or at once when a report is owed.
                let (m, cv) = &*r.watches.kick;
                let kicked = lock(m);
                let mut kicked = cv.wait_timeout_while(kicked, Duration::from_secs(5), |k| !*k).map(|g| g.0).unwrap_or_else(|e| e.into_inner().0);
                *kicked = false;
                drop(kicked);
                r.deliver_now();
            }
        })
        .ok();
    let r = radar.clone();
    thread::Builder::new().name("events".into()).spawn(move || r.follow_local()).ok();
    let r = radar.clone();
    thread::Builder::new().name("remote".into()).spawn(move || r.poll_remote()).ok();
}

impl Radar {
    /// `hc events`, restarted whenever it ends. Each event carries Herdr's
    /// sequence number: a gap (events missed) or a restarted stream means
    /// looking at every terminal again, so none is lost track of.
    fn follow_local(&self) {
        loop {
            let child = Command::new(config::hc_bin())
                .args(["events", "--json"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[radar] hc events: {e}");
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            let Some(stdout) = child.stdout.take() else { continue };
            let mut last: Option<u64> = None;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let seq = v["seq"].as_u64().or_else(|| v["params"]["seq"].as_u64());
                if let (Some(prev), Some(seq)) = (last, seq)
                    && seq > prev + 1
                    && v["terminals"].is_null()
                {
                    eprintln!("[radar] missed {} event(s) from Herdr: looking at every terminal again", seq - prev - 1);
                    self.event(&v);
                    self.resync_local();
                } else {
                    self.event(&v);
                }
                if seq.is_some() {
                    last = seq;
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("[radar] hc events ended; following again");
            thread::sleep(Duration::from_secs(2));
        }
    }

    fn event(&self, v: &Value) {
        // The first line: a snapshot of every terminal. One the tower knew
        // that isn't in it ended while the stream was down.
        if let Some(list) = v["terminals"].as_array() {
            let first = lock(&self.seen).keys().all(|k| k.contains('/'));
            for t in list {
                self.observe(t, None, !first);
            }
            self.drop_missing_local(list);
            self.publish();
            return;
        }
        let p = &v["params"];
        let Some(address) = p["terminal"].as_str() else { return };
        let data = &p["data"];
        match p["type"].as_str().unwrap_or("") {
            "created" => {
                self.observe(data, None, true);
                self.check_startup(address, data);
                self.show_if_ours(address, data);
            }
            "finished" | "removed" | "lost" => self.gone(address),
            "status" | "turn" | "title" | "labels" | "restarted" => {
                // Re-read the terminal: events carry only what changed.
                match hc::lookup(address) {
                    hc::Lookup::Found(t) => self.observe(&t, None, true),
                    hc::Lookup::Gone => self.gone(address),
                    // hc didn't answer: what the event says, on what we knew.
                    hc::Lookup::Unreachable(e) => {
                        eprintln!("[radar] {address}: couldn't re-read it ({e}); going by the event");
                        if let Some(t) = self.with_event(address, p["type"].as_str().unwrap_or(""), data) {
                            self.observe(&t, None, true);
                        }
                    }
                }
            }
            _ => return,
        }
        self.publish();
    }

    /// A terminal's facts from what the radar last knew and one event's data
    /// (when hc can't be asked again).
    fn with_event(&self, address: &str, kind: &str, data: &Value) -> Option<Value> {
        let s = lock(&self.seen).get(address).cloned()?;
        let mut t = serde_json::json!({
            "address": address,
            "state": if s.live { "live" } else { "finished" },
            "status": { "state": s.status.word(), "app": s.app },
            "created_ms": s.created_ms,
            "turn": s.turn,
            "name": s.name,
        });
        match kind {
            "status" => {
                if let Some(st) = data["status"].as_object() {
                    t["status"] = Value::Object(st.clone());
                } else if let Some(st) = data["status"].as_str().or_else(|| data["state"].as_str()) {
                    t["status"]["state"] = Value::String(st.into());
                }
            }
            "turn" => {
                if let Some(n) = data["turn"].as_u64() {
                    t["turn"] = n.into();
                }
            }
            _ => {}
        }
        Some(t)
    }

    /// Every live terminal here, looked at again (after missed events, and
    /// every half minute): what changed is told, what's gone is gone. `hc ls`
    /// gives rows, which say less than an event did (see `Seen::of`): a row
    /// only shows whether a terminal moved, and one that did (or that the
    /// radar doesn't know) is read whole, as after an event.
    fn resync_local(&self) {
        match hc::list_on(None) {
            Ok(list) => {
                for row in &list {
                    let Some(address) = row["address"].as_str() else { continue };
                    let known = lock(&self.seen).get(address).cloned();
                    if !row_news(known.as_ref(), row) {
                        continue;
                    }
                    match hc::lookup(address) {
                        hc::Lookup::Found(t) => self.observe(&t, None, true),
                        hc::Lookup::Gone => self.gone(address),
                        // hc didn't answer: what the row says, on what we knew.
                        hc::Lookup::Unreachable(e) => {
                            eprintln!("[radar] {address}: couldn't re-read it ({e}); going by its row");
                            if let Some(mut t) = self.with_event(address, "", &Value::Null) {
                                match hc::status_word(row) {
                                    Some(word) => t["status"]["state"] = word.into(),
                                    None => t["status"] = Value::Null,
                                }
                                if let Some(n) = hc::turn(row) {
                                    t["turn"] = n.into();
                                }
                                self.observe(&t, None, true);
                            }
                        }
                    }
                }
                self.drop_missing_local(&list);
                self.publish();
            }
            Err(e) => eprintln!("[radar] hc ls: {e:#}"),
        }
    }

    /// The local terminals the radar knows that aren't in `list` (all of this
    /// machine's live ones) have ended.
    fn drop_missing_local(&self, list: &[Value]) {
        let live: HashSet<String> = list
            .iter()
            .filter(|t| t["state"].as_str().is_none_or(|s| s == "live"))
            .filter_map(|t| t["address"].as_str().map(String::from))
            .collect();
        let gone: Vec<String> = lock(&self.seen).keys().filter(|k| !k.contains('/') && !live.contains(*k)).cloned().collect();
        for g in gone {
            self.gone(&g);
        }
    }

    /// What an agent session's hook said: it says so before (or instead of)
    /// Herdr's status catching up.
    fn agent(&self, ev: &super::sessions::AgentEvent, change: &super::sessions::Change) {
        use super::sessions::Change;
        let address = ev.terminal.as_str();
        // A test session's hooks never fire anything (as with its alerts).
        if self.watches.watched(address) && hc::get(address).is_some_and(|t| left_alone(&t)) {
            return;
        }
        match change {
            Change::Started => {
                // Herdr keeps the tie between terminal and agent session, across tower restarts.
                let (a, label) = (address.to_string(), format!("tower.session={}", ev.session_id));
                thread::spawn(move || {
                    let _ = hc::run(&["label", &a, &label]);
                });
            }
            Change::Stopped(_) => {
                // Herdr may still say working for a moment: the watch fires now.
                if self.watches.watched(address) {
                    let (name, space) = self.name_space(address);
                    let st = if ev.event == "stop-failed" { SessionStatus::Error } else { SessionStatus::Done };
                    self.fire_watch(address, &name, space, &st, "");
                }
                self.recheck_soon(address);
            }
            Change::Waiting => self.recheck_soon(address),
            Change::Ended => {
                if self.watches.watched(address) {
                    let (name, space) = self.name_space(address);
                    self.fire_watch(address, &name, space, &SessionStatus::Exited, "");
                }
            }
            // The tower moves it to another account (`failover`): no finish to report.
            Change::Limited(_) | Change::Prompted | Change::Nothing => {}
        }
    }

    fn name_space(&self, address: &str) -> (String, Option<String>) {
        let known = lock(&self.seen).get(address).map(|s| (s.name.clone(), s.space.clone()));
        // One the radar hasn't looked at yet (it was opened a moment ago): as Herdr has it.
        known.or_else(|| hc::get(address).map(|t| (hc::spoken_name(&t), crate::spaces::space_of(&t)))).unwrap_or_else(|| (address.to_string(), None))
    }

    /// Reads a terminal again in a moment (a hook ran ahead of Herdr).
    fn recheck_soon(&self, address: &str) {
        let (me, address) = (self.clone(), address.to_string());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(700));
            if let Some(t) = hc::get(&address) {
                me.observe(&t, None, true);
                me.publish();
            }
        });
    }

    /// Other machines: their terminals by polling, and remote watches.
    fn poll_remote(&self) {
        let mut first: HashMap<String, bool> = HashMap::new();
        // Watches saved by an earlier run: check the local ones now.
        // (Those that ended meanwhile are the reconcile's.)
        for w in self.watches.all().into_iter().filter(|w| !w.contains('/')) {
            if let Some(t) = hc::get(&w).filter(|t| t["state"] == "live") {
                self.check_startup(&w, &t);
            }
        }
        let mut last_resync = std::time::Instant::now();
        loop {
            // This machine too, now and then: whatever the event stream missed.
            if last_resync.elapsed() >= Duration::from_secs(30) {
                last_resync = std::time::Instant::now();
                self.resync_local();
            }
            let machines = if self.alerts.remote_poll_secs == 0 { vec![] } else { hc::machines() };
            for m in machines {
                match hc::list_on(Some(&m)) {
                    Ok(list) => {
                        let announce = first.contains_key(&m);
                        first.insert(m.clone(), true);
                        let live: Vec<String> = list.iter().filter_map(|t| t["address"].as_str().map(String::from)).collect();
                        for t in &list {
                            self.observe(t, Some(&m), announce);
                        }
                        // Terminals that went away there.
                        let prefix = format!("{m}/");
                        let gone: Vec<String> =
                            lock(&self.seen).keys().filter(|k| k.starts_with(&prefix) && !live.contains(k)).cloned().collect();
                        for g in gone {
                            self.gone(&g);
                        }
                    }
                    Err(e) => eprintln!("[radar] {e:#}"),
                }
            }
            // A watched terminal here that never reported a status may be
            // stuck on a first-run question.
            for w in self.watches.all().into_iter().filter(|w| !w.contains('/')) {
                self.trust_check(&w);
            }
            // A watched remote terminal may sit on a machine not listed yet.
            for w in self.watches.remote() {
                if !lock(&self.seen).contains_key(&w)
                    && let Some(t) = hc::get(&w)
                {
                    let m = w.split('/').next().map(String::from);
                    self.observe(&t, m.as_deref(), true);
                }
            }
            self.publish();
            thread::sleep(Duration::from_secs(if self.alerts.remote_poll_secs == 0 { 15 } else { self.alerts.remote_poll_secs.min(15) }));
        }
    }

    /// Records a terminal's facts, and reacts to what changed.
    fn observe(&self, t: &Value, machine: Option<&str>, announce: bool) {
        let Some(raw) = t["address"].as_str() else { return };
        let address = match machine {
            Some(m) if !raw.contains('/') => format!("{m}/{raw}"),
            _ => raw.to_string(),
        };
        let is_voice = machine.is_none() && t["name"].as_str() == Some(brain::VOICE_NAME);
        // Another terminal at an address the radar knew: the old one ended.
        let created_ms = hc::created_ms(t);
        let replaced = lock(&self.seen).get(&address).is_some_and(|s| matches!((s.created_ms, created_ms), (Some(a), Some(b)) if a != b));
        if replaced {
            eprintln!("[radar] {address} is a new terminal now: the old one ended");
            self.gone(&address);
        }
        // What this look says, on what the radar knew: a look that says less
        // (a row of `hc ls`, an event's data) blanks nothing.
        let seen = {
            let known = lock(&self.seen);
            Seen::of(t, machine, known.get(&address))
        };
        let (status, app, name, live, space, turn) = (seen.status.clone(), seen.app.clone(), seen.name.clone(), seen.live, seen.space.clone(), seen.turn);
        let whose = seen.brain.clone();
        let before = lock(&self.seen).insert(address.clone(), seen);

        // A brain (the main one, or a space's own) is the tower itself: what
        // it does is kept with the brains, and nothing below is about it (no
        // watch, no question, no decision, no alert).
        if let Some(brain_name) = whose {
            super::brains::seen(&brain_name, t);
            if brain_name == brain::NAME {
                self.shared.update(|s| {
                    s.brain = if live { if status == SessionStatus::Starting { "starting".into() } else { status.word().to_string() } } else { "absent".into() };
                    s.brain_app = app.clone();
                });
            }
            return;
        }
        if is_voice || app.is_empty() {
            return;
        }
        // A test session (the tower's own fixtures): seen, never announced.
        if left_alone(t) {
            if live && status != SessionStatus::Starting {
                self.watches.seen(&address, &status);
            }
            return;
        }
        if live && status != SessionStatus::Starting {
            self.watches.seen(&address, &status);
        }
        // What it asks, as a question set (this machine's sessions).
        if machine.is_none() {
            self.asks.seen(&address, t, &if live { status.clone() } else { SessionStatus::Exited });
        }
        // A decision about it: it waits on that unless it works on something else.
        if live {
            self.decisions.session_seen(&address, status != SessionStatus::Working);
            // Blocked on what the tower couldn't read: over once it isn't blocked.
            self.decisions.session_blocked(&address, status == SessionStatus::Blocked);
        } else {
            self.decisions.session_gone(&address);
        }
        // First sight of a watched session (a machine just reached, a start):
        // how it stood when the watch was set.
        let watch = self.watches.watch(&address);
        let was = match &before {
            Some(b) => b.status.clone(),
            None => watch.as_ref().map(|w| w.last.clone()).unwrap_or_default(),
        };
        // A turn that ended since the last look, even if nobody saw it working.
        let before_turn = before.as_ref().and_then(|b| b.turn).or_else(|| watch.as_ref().and_then(|w| w.turn));
        let turned = matches!((before_turn, turn), (Some(a), Some(b)) if b > a) && status.ready();
        let now = crate::history::now_ms();
        // Each turn looked at, kept for the next start.
        if machine.is_none()
            && live
            && status.ready()
            && let Some(n) = turn
        {
            self.asks.looked_turn(&address, created_ms, n, now);
        }
        // A watch on another terminal that had this address: that one ended.
        if let Some(w) = &watch
            && matches!((w.created_ms, created_ms), (Some(a), Some(b)) if a != b)
        {
            self.fire_watch(&address, &name, space.clone(), &SessionStatus::Exited, "");
            return;
        }
        if was == status && !turned {
            return;
        }
        let working = SessionStatus::Working;
        let stopped_working = (was == working && status != working) || (turned && was != working);
        // A turn ended: its last message may ask the user something.
        if stopped_working && status.ready() {
            self.asks.finished(&address, t, self.waiting.clone());
        }
        // Blocked on a dialog this machine can read: the questions go through
        // the brain (answered for the user, or told to them), not a plain notice.
        let handled = machine.is_none() && status == SessionStatus::Blocked && matches!(hc::block_kind(t), hc::BlockKind::Question | hc::BlockKind::Permission);
        let fires = (stopped_working || (status == SessionStatus::Blocked && !handled) || !live) && self.watches.watched(&address);

        // Its open job follows what it does: back to working, blocked on what
        // only a person can answer, done, an error, ended. Not a dialog the
        // tower handles (its card says so), nor a change a watch reports (the
        // watch moves the job), nor what happened while the tower was down
        // (the start's reconcile reports it, then moves the job).
        // Waiting for the user (to merge, approve, test): the job stays
        // blocked through the turn's end as Herdr sees it, until the session
        // works again or ends.
        if status == SessionStatus::Working || !live {
            lock(&self.waiting).remove(&address);
        }
        let waits = status.ready() && (lock(&self.waiting).contains(&address) || self.asks.waits_on(&address));
        if let Some(job) = crate::history::JobState::of_session(&status, live)
            && before.is_some()
            && (was != status || turned)
            && !fires
            && !waits
            && !(handled && job == crate::history::JobState::Blocked)
        {
            let a = address.clone();
            crate::history::later(move || crate::history::job_follow(&a, job, ""));
        }

        // The brain asked to hear when this one stops.
        if fires {
            let status = if live { status.clone() } else { SessionStatus::Exited };
            self.fire_watch(&address, &name, space.clone(), &status, "");
            return;
        }
        if !announce {
            return;
        }
        // The session by its space and what it works on: as it is written, and as it is said.
        let named = |line: String| crate::names::notice(&line, space.as_deref(), &name);
        // A finish nobody watched is written down, not told, unless the tower tells everything.
        if stopped_working && status.ready() && self.alerts.done && crate::tell::level().quiet() {
            self.shared.record(&address, &named(format!("{name} finished")).0, "done");
            return;
        }
        // What it asks, in a sentence (or just that it needs you).
        let (notice, notice_said) = named(super::asks::blocked_notice(&name, &t["status"]));
        let (finished, finished_said) = named(format!("{name} finished"));
        let (error, error_said) = named(format!("{name} hit an error"));
        let news = match status {
            SessionStatus::Blocked if self.alerts.blocked && !handled => Some(notice.clone()),
            SessionStatus::Done | SessionStatus::Idle if stopped_working && self.alerts.done => Some(finished.clone()),
            SessionStatus::Error => Some(error.clone()),
            _ => None,
        };
        let word = status.word();
        if let Some(news) = news
            && self.held(&address, &name, space.as_deref(), &news, word, "alert")
        {
            return;
        }
        if status == SessionStatus::Blocked && self.alerts.blocked && !handled {
            self.shared.alert(&address, &notice, "blocked");
            self.mouth.needs_you(format!("{notice_said}."), Some((address.as_str(), word)));
        } else if stopped_working && status.ready() && self.alerts.done {
            self.shared.alert(&address, &finished, "done");
            self.mouth.alert(format!("{finished_said}."), Some((address.as_str(), word)));
        } else if status == SessionStatus::Error {
            self.shared.alert(&address, &error, "error");
            if self.alerts.blocked {
                self.mouth.needs_you(format!("{error_said}."), Some((address.as_str(), word)));
            }
        }
    }

    /// A question a session asks, told to the user (`asks`): as a blocked
    /// session's notice, held for another space in focus as usual.
    fn announce(&self, address: &str, text: &str) {
        let (name, space) = self.name_space(address);
        // Its session by its space and what it works on: as it is written, and as it is said.
        let (text, said) = crate::names::notice(text, space.as_deref(), &name);
        if self.held(address, &name, space.as_deref(), &text, "blocked", "alert") {
            return;
        }
        self.shared.alert(address, &text, "blocked");
        if self.alerts.blocked {
            self.mouth.needs_you(said, None);
        }
    }

    /// How news of `kind` and `status` from `space` goes with the focus as
    /// it is now, and the focus (empty: none).
    fn gate(&self, space: Option<&str>, status: &str, kind: &str) -> (crate::spaces::Gate, String) {
        let focus = crate::spaces::focus_now(self.alerts.focus_hours);
        let news = crate::spaces::News::of(kind, status);
        (crate::spaces::gate(space, focus.as_deref(), news, self.alerts.held_urgent), focus.unwrap_or_default())
    }

    /// With a space in focus, news from another space is kept for later
    /// instead of spoken (or handed to the brain): true when it was. An agent
    /// there needing the user still gets a bell and what it needs.
    fn held(&self, address: &str, name: &str, space: Option<&str>, text: &str, status: &str, kind: &str) -> bool {
        let (gate, focus) = self.gate(space, status, kind);
        self.keep(gate, &focus, address, name, space, text, status, kind)
    }

    /// Keeps news the gate holds: true when it did.
    #[allow(clippy::too_many_arguments)]
    fn keep(&self, gate: crate::spaces::Gate, focus: &str, address: &str, name: &str, space: Option<&str>, text: &str, status: &str, kind: &str) -> bool {
        use crate::spaces::Gate;
        if matches!(gate, Gate::Speak | Gate::Through) {
            return false;
        }
        let space = space.unwrap_or("");
        let (a, n, sp, t, st, k) =
            (address.to_string(), name.to_string(), space.to_string(), text.to_string(), status.to_string(), kind.to_string());
        crate::history::later(move || crate::spaces::hold(&k, &a, &n, &sp, &t, Some(&st)));
        if gate == Gate::Urgent {
            self.mouth.needs_you(crate::spaces::needs_you_line(space, text), Some((address, status)));
        }
        eprintln!("[radar] held for {space} (focus: {focus}): {text}");
        true
    }

    /// Herdr's window shows only terminals placed in a space; `hc run`
    /// leaves them hidden. Sessions the brain starts belong on screen.
    fn show_if_ours(&self, address: &str, t: &Value) {
        if !self.alerts.show_sessions || t["labels"].get("herdr.space").is_some() {
            return;
        }
        let parent = t["parent"].as_str().unwrap_or("");
        // Started by a brain: the main one, or a space's own.
        let brain = hc::get(brain::NAME).and_then(|b| b["address"].as_str().map(String::from)).unwrap_or_default();
        if parent.is_empty() || (parent != brain && !super::brains::is_brain_address(parent)) {
            return;
        }
        let address = address.to_string();
        thread::spawn(move || {
            // Let it register before the window places it.
            thread::sleep(Duration::from_millis(800));
            match crate::show_terminal(&address) {
                Ok(()) => eprintln!("[radar] {address}: shown in a tab"),
                Err(e) => eprintln!("[radar] {address}: couldn't show it: {e:#}"),
            }
        });
    }

    /// A new Claude Code or Codex reports no status while a first-run
    /// question (trusting its folder) holds it: treat that as needing a person.
    fn check_startup(&self, address: &str, t: &Value) {
        let program = t["argv"][0].as_str().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
        // (A brain's own first-run question is the tower's to answer: see `brain::settle`.)
        if !["claude", "codex"].contains(&program.as_str()) || brain::brain_of(t).is_some() || left_alone(t) {
            return;
        }
        let me = self.clone();
        let address = address.to_string();
        thread::spawn(move || {
            for _ in 0..12 {
                thread::sleep(Duration::from_secs(4));
                if me.trust_check(&address) {
                    return;
                }
            }
        });
    }

    /// True once there's nothing more to wait for: the terminal reports a
    /// status, is gone, or its trust question was passed on.
    fn trust_check(&self, address: &str) -> bool {
        let Some(t) = hc::get(address) else { return true };
        if t["state"] != "live" || hc::status_word(&t).is_some() {
            return true;
        }
        let screen = hc::run(&["read", address, "--lines", "0"]).map(|o| o.stdout).unwrap_or_default().to_lowercase();
        if !screen.contains("trust") {
            return false;
        }
        let name = hc::spoken_name(&t);
        let folder = t["cwd"].as_str().unwrap_or("its folder");
        let report = Pending {
            address: address.to_string(),
            name: name.clone(),
            space: crate::spaces::space_of(&t),
            status: "blocked".into(),
            msg: format!(
                "[watch] {address} ({name}) is waiting on its first-run question: may it trust the folder {folder}? It can't start until someone answers. Ask me before you answer it."
            ),
            speech: format!("{name} is asking whether to trust its folder."),
            kind: "alert".into(),
            at_ms: crate::history::now_ms(),
            spoken: false,
        };
        let r = report.clone();
        if self.watches.fire(address, move |_| r).is_some() {
            let a = address.to_string();
            crate::history::later(move || crate::history::job_update(&a, crate::history::JobState::Blocked, "asks to trust its folder"));
        } else {
            self.watches.owe(report);
        }
        self.watches.kick();
        true
    }

    fn gone(&self, address: &str) {
        super::brains::gone(address);
        self.asks.seen(address, &Value::Null, &SessionStatus::Gone);
        // A decision all of whose sessions are gone is over.
        self.decisions.session_gone(address);
        super::sessions::registry().forget(address);
        self.asks.forget_turn(address);
        let removed = lock(&self.seen).remove(address);
        if (address == brain::NAME || removed.as_ref().is_some_and(|_| hc::get(brain::NAME).is_none()))
            && hc::get(brain::NAME).filter(|t| t["state"] == "live").is_none()
        {
            self.shared.update(|s| s.brain = "absent".into());
        }
        // Gone while the tower looked: nothing owed at the next start for it.
        if removed.as_ref().is_some_and(|r| !r.app.is_empty()) {
            self.watches.seen(address, &SessionStatus::Gone);
            // A watched one's job is the watch's to move (below).
            if !self.watches.watched(address) {
                let a = address.to_string();
                crate::history::later(move || crate::history::job_follow(&a, crate::history::JobState::Exited, ""));
            }
        }
        if self.watches.watched(address) {
            let space = removed.as_ref().and_then(|r| r.space.clone());
            let name = removed.map(|r| r.name).unwrap_or_else(|| address.to_string());
            self.fire_watch(address, &name, space, &SessionStatus::Exited, "");
        }
    }

    /// A watched session stopped (`status`): its report goes out, once
    /// (`why` adds how it was found, e.g. while the tower was down).
    fn fire_watch(&self, address: &str, name: &str, space: Option<String>, status: &SessionStatus, why: &str) -> bool {
        // Asked of the queue and the ledger before the book is held (the queue tells through the book).
        let has_work = super::asks::has_work(address);
        let fired = self.watches.fire(address, |w| {
            let (t, a) = (address.to_string(), w.note.clone());
            // What it said at the end of its turn (its hook): often all the brain needs.
            let said = super::sessions::registry().said_since(address, w.since_ms).filter(|_| !status.ended());
            // It stopped to wait for the user (to merge, approve, test), or
            // ended its turn on a PR (it waits for their merge): not done.
            // (Idle is not blocked: one that only says it is ready, with nothing asked of it, waits for nothing.)
            // (`on_pr`: it said no more than a PR's link. That is the user's only once GitHub says the PR is open.)
            let mut on_pr = false;
            let waits = said.as_deref().filter(|s| status.ready() && (has_work || !crate::questions::ready_only(s))).and_then(|s| {
                crate::questions::waits_for_user(s).or_else(|| {
                    on_pr = true;
                    crate::prs::links(s)
                        .first()
                        .map(|(repo, n)| format!("Merge PR #{n} ({repo}), which it opened: the chat shows it with a Merge button"))
                })
            });
            // A status that says nothing for a job (none, a word not known): the turn is over, as the watch says.
            let job = match &waits {
                Some(_) => crate::history::JobState::Blocked,
                None => crate::history::JobState::of_session(status, true).unwrap_or(match status {
                    SessionStatus::Other(w) => crate::history::JobState::Other(w.clone()),
                    _ => crate::history::JobState::Done,
                }),
            };
            crate::history::later(move || crate::history::job_watched(&t, job, &a));
            let mut p = report(address, name, space.clone(), status, &w.note, why, "watch");
            if let Some(said) = said {
                let said: String = said.chars().take(1200).collect();
                let ask = match &waits {
                    Some(r) => wait_ask(crate::tell::level().quiet(), r, !on_pr),
                    None => " Tell me what happened, briefly (read its screen only if that isn't enough).".to_string(),
                };
                p.msg = p.msg.replace(
                    " Read its screen and tell me what happened, briefly.",
                    &format!(" It ended its turn saying: \"{}\"{ask}", said.replace('"', "'")),
                );
            }
            if let Some(r) = &waits {
                lock(&self.waiting).insert(address.to_string());
                p.status = "blocked".into();
                p.speech = format!("{}.", crate::questions::wait_line(name, r));
            }
            p
        });
        if fired.is_none() {
            return false;
        }
        eprintln!("[radar] watch on {address} fired: {status}{why}");
        // The outbox thread hands it over (typing into the brain can take a
        // while; this thread follows Herdr's events).
        self.watches.kick();
        true
    }

    /// Hands what's owed to the brain: kept in the outbox until it takes it
    /// (said out loud once meanwhile). From a space out of focus, a report
    /// on work the user asked for goes through, marked so; other news is held.
    fn deliver_now(&self) {
        self.watches.deliver(|p| {
            // At its usage limit, a session is no error and doesn't wait for the user: the failover moves it, and says so once.
            if p.kind != "queue" && p.kind != "agenda" && matches!(p.status.as_str(), "error" | "blocked") && super::failover::at_limit(&p.address) {
                eprintln!("[radar] {} ({}) is at its usage limit: the failover has it; not told", p.address, p.name);
                let line = format!("{} hit its account's usage limit", crate::names::shown(p.space.as_deref(), &p.name));
                self.shared.record(&p.address, &line, &p.status);
                return true;
            }
            let news = p.msg.trim_start_matches("[watch] ").trim_start_matches("[queue] ").trim_start_matches("[agenda] ").to_string();
            let (gate, focus) = self.gate(p.space.as_deref(), &p.status, &p.kind);
            if self.keep(gate, &focus, &p.address, &p.name, p.space.as_deref(), &news, &p.status, &p.kind) {
                return true;
            }
            let through = (gate == crate::spaces::Gate::Through).then(|| p.space.clone().unwrap_or_default());
            let msg = match &through {
                Some(space) => format!("{}{}", p.msg, crate::spaces::out_of_focus(space, &focus)),
                None => p.msg.clone(),
            };
            if let Some(space) = &through {
                eprintln!("[radar] through the focus ({focus}) from {space}: work the user asked for");
                if !p.speech.is_empty() && !p.speech.starts_with("In ") {
                    p.speech = format!("In {space}, {}", lower_first(&p.speech));
                }
            }
            // At a quiet level a finish, a delivery or other news is for the
            // record: the brain acts on it and nobody is told. What needs the
            // user (blocked, an error) is told at every level.
            let level = crate::tell::level();
            // A wait the tower put in front of the user as a decision (told
            // once, in its own words): the plain "is waiting for you" beside
            // it is only written down.
            let decided = beside_decision(level, &p.status, lock(&self.waiting).contains(&p.address));
            let record = crate::tell::for_record(level, &p.status) || decided;
            let status_line = |p: &Pending| {
                if p.kind == "queue" || p.kind == "pr" || p.kind == "agenda" {
                    return;
                }
                let line = format!("{} is {}", crate::names::shown(p.space.as_deref(), &p.name), say_status(&p.status));
                if record { self.shared.record(&p.address, &line, &p.status) } else { self.shared.alert(&p.address, &line, &p.status) }
            };
            // To the brain of its space, when that space has one that can take it; else the main brain.
            let space = p.space.clone().or_else(|| lock(&self.seen).get(&p.address).and_then(|s| s.space.clone()));
            match super::brains::note(space.as_deref(), &crate::tell::note_at(level, &msg), record) {
                Ok(()) => {
                    if !p.spoken {
                        status_line(p);
                    }
                    true
                }
                Err(e) => {
                    if !p.spoken {
                        eprintln!("[radar] the brain can't take it yet ({e:#}): {}", p.msg);
                        status_line(p);
                        if !p.speech.is_empty() && !record {
                            let said = crate::names::notice(&p.speech, p.space.as_deref(), &p.name).1;
                            let about = Some((p.address.as_str(), p.status.as_str()));
                            // What needs the user is a decision's sentence; the rest is news.
                            if crate::tell::needs_you(&p.status) { self.mouth.needs_you(said, about) } else { self.mouth.alert(said, about) }
                        }
                        p.spoken = true;
                    }
                    false
                }
            }
        });
    }

    /// A watch set just now: the session may have finished already (before
    /// the watch was asked for). A prompt sent just now has started within a
    /// few seconds.
    fn recheck(&self, terminal: &str) {
        let me = self.clone();
        let terminal = terminal.to_string();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(3));
            let Some(t) = hc::get(&terminal) else { return };
            let status = hc::status_live(&t);
            let turned = me.watches.watch(&terminal).and_then(|w| w.turn).zip(hc::turn(&t)).is_some_and(|(a, b)| b > a);
            if !matches!(status, SessionStatus::Done | SessionStatus::Blocked | SessionStatus::Error | SessionStatus::Exited)
                && !(turned && status == SessionStatus::Idle)
            {
                return;
            }
            me.fire_watch(&terminal, &hc::spoken_name(&t), crate::spaces::space_of(&t), &status, "");
        });
    }

    /// At start: what finished, blocked or ended while the voice was down,
    /// among the watched sessions (`before`: the book as the last run left
    /// it) and those with an open job.
    fn reconcile(&self, before: Book) {
        // Let the brain's terminal settle when everything is starting at once.
        thread::sleep(Duration::from_secs(2));
        let mut now: BTreeMap<String, Now> = BTreeMap::new();
        let mut facts: HashMap<String, Value> = HashMap::new();
        let mut reached: Vec<String> = vec![];
        let mut lists = vec![(None, hc::list_on(None))];
        if before.watches.keys().any(|k| k.contains('/')) || self.alerts.remote_poll_secs > 0 {
            for m in hc::machines() {
                let l = hc::list_on(Some(&m));
                lists.push((Some(m), l));
            }
        }
        for (m, list) in lists {
            let Ok(list) = list else { continue };
            reached.push(m.map(|m| format!("{m}/")).unwrap_or_default());
            for t in list {
                let Some(a) = t["address"].as_str().map(String::from) else { continue };
                now.insert(
                    a.clone(),
                    Now {
                        live: t["state"].as_str().is_none_or(|s| s == "live"),
                        status: hc::status(&t),
                        created_ms: hc::created_ms(&t),
                        turn: hc::turn(&t),
                    },
                );
                facts.insert(a, t);
            }
        }
        let reachable = |a: &str| match a.split_once('/') {
            Some((m, _)) => reached.contains(&format!("{m}/")),
            None => reached.iter().any(String::is_empty),
        };
        let jobs: Vec<OpenJob> = crate::history::jobs(&crate::history::load())
            .into_iter()
            .filter(|j| j.open())
            .map(|j| OpenJob { session: j.session, status: j.status, title: j.title })
            .collect();
        let owed = book::reconcile(&before, &jobs, &now, reachable);
        // A turn that ended while the tower was down (a higher turn count than the
        // last run looked at): its end is looked at now, as if the radar saw it.
        for (a, n) in now.iter().filter(|(a, n)| !a.contains('/') && n.live && n.status.ready()) {
            let Some(turn) = n.turn else { continue };
            if !self.asks.missed_turn(a, n.created_ms, turn, crate::history::now_ms()) {
                continue;
            }
            let Some(t) = hc::get(a).or_else(|| facts.get(a).cloned()) else { continue };
            eprintln!("[radar] {a} ended a turn while the tower was down: its end is looked at now");
            self.asks.finished(a, &t, self.waiting.clone());
        }
        eprintln!("[radar] start: {} watch(es), {} open job(s), {} owed", before.watches.len(), jobs.len(), owed.len());
        let name_of = |a: &str| facts.get(a).map(hc::spoken_name).unwrap_or_else(|| a.to_string());
        let space_of = |a: &str| facts.get(a).and_then(crate::spaces::space_of);
        for o in owed {
            match o {
                Owed::Watch { address, status, was } => {
                    let why = down_why(&status, &was);
                    self.fire_watch(&address, &name_of(&address), space_of(&address), &status, &why);
                }
                Owed::Job { address, status, was, title } => {
                    let name = name_of(&address);
                    let p = report(&address, &name, space_of(&address), &status, &title, &down_why(&status, &was), "watch");
                    if self.watches.owe(p)
                        && let Some(job) = crate::history::JobState::of_session(&status, true)
                    {
                        crate::history::later(move || crate::history::job_follow(&address, job, ""));
                    }
                }
            }
        }
        self.watches.kick();
    }

    fn publish(&self) {
        let mut agents: Vec<AgentLine> = lock(&self.seen)
            .iter()
            .filter(|(_, s)| s.live && !s.app.is_empty())
            .map(|(a, s)| AgentLine {
                address: a.clone(),
                name: s.name.clone(),
                status: s.status.word().to_string(),
                app: s.app.clone(),
                account: s.account.clone(),
                question: None,
                folder: s.cwd.clone(),
                queued: 0,
                waiting: vec![],
                jobs: super::running::jobs_of(a),
                servers: super::running::servers_of(a),
                browsers: super::running::browsers_of(a),
            })
            .filter(|a| !a.name.starts_with("tower"))
            .collect();
        let waiting = super::sendq::waiting_counts();
        let mut lines = super::sendq::waiting_lines();
        for a in &mut agents {
            a.queued = waiting.get(&a.address).copied().unwrap_or(0);
            a.waiting = lines.remove(&a.address).unwrap_or_default();
        }
        agents.sort_by(|a, b| a.address.cmp(&b.address));
        for set in self.asks.list() {
            if let Some(a) = agents.iter_mut().find(|a| a.address == set.session) {
                a.question = Some(set.id);
            }
        }
        self.shared.update(|s| s.agents = agents);
    }
}

/// A report on a watched (or job) session for the brain.
fn report(address: &str, name: &str, space: Option<String>, status: &SessionStatus, asked: &str, why: &str, kind: &str) -> Pending {
    let what = if asked.is_empty() { String::new() } else { format!(" You had asked it: {asked}.") };
    let (msg, speech) = if status.ended() {
        (format!("[watch] {address} ({name}) has exited{why}.{what} Tell me briefly."), format!("{name} has exited."))
    } else {
        (
            format!("[watch] {address} ({name}) is now {status}{why}.{what} Read its screen and tell me what happened, briefly."),
            format!("{name} is {}.", say_status(status.word())),
        )
    };
    Pending {
        address: address.into(),
        name: name.into(),
        space,
        status: status.word().to_string(),
        msg,
        speech,
        kind: kind.into(),
        at_ms: crate::history::now_ms(),
        spoken: false,
    }
}

/// What a watch's report asks of the brain when its session waits for the
/// user (`request`). At a quiet level the tower has made that a decision and
/// told the user itself: the brain neither tells nor raises it again.
/// `shown`: the tower has done so. False for a turn that only ended on a
/// PR's link: its decision comes when GitHub says the PR is open, and when
/// GitHub can't be asked there is none, so the brain checks and raises it.
fn wait_ask(quiet: bool, request: &str, shown: bool) -> String {
    let request = request.replace('"', "'");
    if quiet && !shown {
        format!(
            " It ended its turn on a PR: \"{request}\". The tower puts an open PR in front of the user itself, as a merge decision. Look at `tower decisions` once: if that PR is there, do nothing; if it isn't (GitHub couldn't be asked, or it was taken out before), raise it with `tower raise <its address> --why merge \"<the PR and what the user should do>\"`."
        )
    } else if quiet {
        format!(" It is waiting for the user to act: \"{request}\". The tower has put that in front of the user itself, as a decision, and said it once: don't raise it again.")
    } else {
        format!(" It is waiting for the user to act: \"{request}\". Tell the user plainly what they need to do (with the PR or link if there is one).")
    }
}

/// A report that a session waits for the user (`blocked`), when its turn
/// ended waiting for them (`waits`: that is a decision, which the tower
/// tells them of once): at a quiet level the plain "is waiting for you" is
/// not said beside it.
fn beside_decision(level: crate::tell::Level, status: &str, waits: bool) -> bool {
    level.quiet() && status == "blocked" && waits
}

/// How a report found at start came about.
fn down_why(status: &SessionStatus, was: &SessionStatus) -> String {
    let was = if *was == SessionStatus::Starting || was == status { String::new() } else { format!("; it was {was} when last seen") };
    if status.ended() { format!(" while the tower was down{was}") } else { format!(" (this happened while the tower was down{was})") }
}

/// A test session: labelled `tower.test`, or named `qfix-…`. It never raises
/// an alert, a trust notice or a question (start one with
/// `hc run --label tower.test=1 --name qfix-…`).
pub fn is_test(t: &Value) -> bool {
    t["labels"].get("tower.test").is_some() || t["name"].as_str().is_some_and(|n| n.starts_with("qfix-"))
}

/// A daemon started to test the tower (`HERDER_TOWER_TEST=1`, beside the
/// real one, with its own HERDER_TOWER_HOME/STATE/DATA).
pub fn test_daemon() -> bool {
    std::env::var_os("HERDER_TOWER_TEST").is_some_and(|v| !v.is_empty() && v != "0")
}

/// A session this daemon leaves alone (no alerts, cards, reports or jobs):
/// test sessions for the real tower, and every other session for a test
/// daemon, so the two never answer, announce or report the same session.
pub fn left_alone(t: &Value) -> bool {
    is_test(t) != test_daemon()
}

/// "Mediaroom is done." → "mediaroom is done." (after "In <space>, ").
fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) if c.clone().next().is_some_and(char::is_lowercase) => f.to_lowercase().chain(c).collect(),
        _ => s.to_string(),
    }
}

fn say_status(status: &str) -> &str {
    match status {
        "blocked" => "waiting for you",
        "done" | "idle" => "done",
        "error" => "stuck on an error",
        "" => "stopped",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::is_test;
    use serde_json::json;

    #[test]
    fn a_wait_that_is_a_decision_is_not_said_twice() {
        use crate::tell::Level::*;
        // The tower tells everything: the plain line and the brain's words, as before.
        assert!(!super::beside_decision(Everything, "blocked", true));
        let loud = super::wait_ask(false, "you need to merge \"it\"", true);
        assert!(loud.contains("Tell the user plainly what they need to do") && loud.contains("you need to merge 'it'"));
        // A quiet level: the decision says it, once.
        for level in [Decisions, Results] {
            assert!(super::beside_decision(level, "blocked", true));
            assert!(!super::beside_decision(level, "blocked", false), "blocked on a dialog the tower can't read: still told");
            assert!(!super::beside_decision(level, "error", true));
        }
        let quiet = super::wait_ask(true, "you need to merge it", true);
        assert!(quiet.contains("don't raise it again") && !quiet.contains("Tell the user"));
        // A turn that only ended on a PR's link: the card comes when GitHub says the PR is open. The
        // brain is never told it is there before it is; it looks, and raises it when it isn't.
        let on_pr = super::wait_ask(true, "Merge PR #12 (o/app), which it opened", false);
        assert!(!on_pr.contains("don't raise it again") && on_pr.contains("tower decisions") && on_pr.contains("--why merge"), "{on_pr}");
        assert!(super::wait_ask(false, "Merge PR #12 (o/app), which it opened", false).contains("Tell the user plainly"));
    }

    #[test]
    fn a_row_of_hc_ls_never_blanks_what_an_event_told() {
        use super::{Seen, row_news};
        use crate::hc::SessionStatus;
        // As `hc get` and the snapshot of `hc events` have a terminal.
        let whole = json!({
            "address": "t7", "name": null, "state": "live", "created_ms": 5000, "turn": 3, "cwd": "/w",
            "foreground": { "argv": ["node"], "cwd": "/w/.tower-worktrees/sandbox/app", "pid": 0 },
            "labels": { "where": "sandbox / app", "tower.name": "sandbox-app" },
            "status": { "app": "claude-code", "state": "working" },
        });
        let known = Seen { account: Some("orca".into()), ..Seen::of(&whole, None, None) };
        assert_eq!(known.brain, None, "an agent session is no brain");
        assert_eq!(
            (known.app.as_str(), known.space.as_deref(), known.name.as_str(), known.created_ms, known.turn, known.status.clone(), known.live),
            ("claude-code", Some("sandbox"), "app", Some(5000), Some(3), SessionStatus::Working, true),
            "its name within its space: the space isn't said twice"
        );
        // As `hc ls` has it half a minute later: its status a word, no labels, no pids, no dates.
        let row = json!({ "address": "t7", "agent": "claude-code", "cwd": "/w/.tower-worktrees/sandbox/app", "state": "live", "status": "working", "turn": 3, "where": "sandbox / app" });
        assert!(!row_news(Some(&known), &row), "nothing moved: the terminal isn't read again");
        let after = Seen::of(&row, None, Some(&known));
        assert_eq!(after.app, "claude-code", "still an agent: it stays in the state's agents");
        assert_eq!((after.space.as_deref(), after.account.as_deref(), after.name.as_str(), after.created_ms), (Some("sandbox"), Some("orca"), "app", Some(5000)));
        assert_eq!(after.cwd, "/w/.tower-worktrees/sandbox/app");
        // It moved without an event: the row says so (and the terminal is read whole), and takes nothing else away.
        let done = json!({ "address": "t7", "agent": "claude-code", "cwd": "/w", "state": "live", "status": "done", "turn": 4 });
        assert!(row_news(Some(&known), &done));
        let after = Seen::of(&done, None, Some(&known));
        assert_eq!((after.status.clone(), after.turn), (SessionStatus::Done, Some(4)));
        assert_eq!((after.app.as_str(), after.space.as_deref(), after.account.as_deref(), after.name.as_str()), ("claude-code", Some("sandbox"), Some("orca"), "app"));
        assert!(row_news(Some(&known), &json!({ "address": "t7", "state": "live", "status": "working", "turn": 4 })), "a turn ended and another began");
        assert!(row_news(Some(&known), &json!({ "address": "t7", "state": "finished", "status": "working", "turn": 3 })));
        // One the radar doesn't know, or knows only by a row, is read whole.
        assert!(row_news(None, &row));
        let by_row = Seen::of(&row, None, None);
        assert_eq!((by_row.app.as_str(), by_row.space.as_deref(), by_row.created_ms), ("claude-code", Some("sandbox"), None), "a row alone says this much");
        assert!(row_news(Some(&by_row), &row));
        // Its agent ended, a shell is left: no status is news, not a blank.
        let shell = json!({ "address": "t7", "agent": "zsh", "cwd": "/w", "state": "live", "turn": 4, "where": "sandbox / app" });
        assert!(row_news(Some(&known), &shell));
        assert_eq!(Seen::of(&shell, None, Some(&known)).app, "");
        // A whole look is believed as it is: a label that went is gone.
        let moved = json!({ "address": "t7", "state": "live", "created_ms": 5000, "turn": 3, "labels": {}, "status": { "app": "codex", "state": "idle" } });
        let after = Seen::of(&moved, None, Some(&known));
        assert_eq!((after.space, after.app.as_str(), after.account.as_deref()), (None, "codex", Some("orca")));
        // An event's data when hc can't be asked again (`with_event`): the same rule.
        let partial = json!({ "address": "t7", "state": "live", "status": { "state": "blocked", "app": "claude-code" }, "created_ms": 5000, "turn": 3, "name": "sandbox app" });
        let after = Seen::of(&partial, None, Some(&known));
        assert_eq!((after.status.clone(), after.space.as_deref(), after.account.as_deref(), after.cwd.as_str()), (SessionStatus::Blocked, Some("sandbox"), Some("orca"), "/w/.tower-worktrees/sandbox/app"));
        // Another machine's terminals are only ever rows: what one fuller look told (a watched one, by `hc get`) stays.
        let far = Seen::of(&json!({ "address": "vps/t2", "name": "api", "state": "live", "status": { "app": "codex", "state": "working" }, "labels": {} }), Some("vps"), None);
        let far_row = json!({ "address": "vps/t2", "name": "api", "agent": "codex", "state": "live", "status": "done", "turn": 1 });
        let after = Seen::of(&far_row, Some("vps"), Some(&far));
        assert_eq!((after.name.as_str(), after.app.as_str(), after.status.clone()), ("api on vps", "codex", SessionStatus::Done));
    }

    #[test]
    fn a_tower_terminal_is_a_brain_and_never_an_agent_to_watch() {
        use super::Seen;
        // A space's own brain, as `hc get` has it: a Claude Code session by every other sign.
        let whole = json!({
            "address": "t12", "name": "tower-sandbox", "state": "live", "created_ms": 9000, "turn": 2, "cwd": "/h/brains/sandbox",
            "argv": ["claude", "-n", "tower-sandbox", "--model", "claude-opus-5-5", "--effort", "high"],
            "labels": { "tower.space": "Sandbox" }, "status": { "app": "claude-code", "state": "working" },
        });
        let s = Seen::of(&whole, None, None);
        assert_eq!((s.brain.as_deref(), s.app.as_str()), (Some("tower-sandbox"), "claude-code"));
        // The main brain; and one named by hand counts as one too.
        assert_eq!(Seen::of(&json!({ "address": "t7", "name": "tower", "state": "live", "status": { "app": "claude-code", "state": "idle" } }), None, None).brain.as_deref(), Some("tower"));
        assert_eq!(Seen::of(&json!({ "address": "t8", "name": "tower-b2c-tickets", "state": "live", "status": "idle" }), None, None).brain.as_deref(), Some("tower-b2c-tickets"));
        // The daemon's own terminal, a session whose name only starts alike, and a session of the space are not.
        for name in ["tower-voice", "towering", "sandbox-app"] {
            assert_eq!(Seen::of(&json!({ "address": "t9", "name": name, "state": "live", "status": "idle" }), None, None).brain, None, "{name}");
        }
        // Its row in `hc ls` half a minute later names it; an event's data when hc can't be asked may not: it stays a brain.
        let row = json!({ "address": "t12", "name": "tower-sandbox", "agent": "claude-code", "state": "live", "status": "done", "turn": 3 });
        assert_eq!(Seen::of(&row, None, Some(&s)).brain.as_deref(), Some("tower-sandbox"));
        let partial = json!({ "address": "t12", "state": "live", "status": { "state": "blocked", "app": "claude-code" }, "created_ms": 9000, "name": "tower sandbox" });
        assert_eq!(Seen::of(&partial, None, Some(&s)).brain.as_deref(), Some("tower-sandbox"));
        // Another terminal at that address, read whole and named otherwise, is not one.
        let other = json!({ "address": "t12", "name": "api", "state": "live", "created_ms": 9500, "labels": {}, "status": { "app": "claude-code", "state": "idle" } });
        assert_eq!(Seen::of(&other, None, Some(&s)).brain, None);
        // A terminal called `tower-…` on another machine is that machine's business.
        assert_eq!(Seen::of(&json!({ "address": "vps/t2", "name": "tower-sandbox", "state": "live", "status": "idle" }), Some("vps"), None).brain, None);
        // It is left out of the agents the state lists (by its name, as the main brain is).
        assert!(s.name.starts_with("tower"));
    }

    #[test]
    fn test_sessions_are_known_by_label_or_name() {
        assert!(is_test(&json!({ "name": "qfix-ask", "labels": {} })));
        assert!(is_test(&json!({ "name": "anything", "labels": { "tower.test": "1" } })));
        assert!(!is_test(&json!({ "name": "orb-rust", "labels": { "herdr.space": "sp1" } })));
        assert!(!is_test(&json!({ "address": "t3" })));
    }
}
