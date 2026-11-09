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
use crate::hc;

#[derive(Clone, Debug)]
struct Seen {
    name: String,
    status: String,
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
                hc::status_word(&t).unwrap_or_default(),
            ),
            hc::Lookup::Found(_) => anyhow::bail!("{terminal} has ended: nothing to watch"),
            hc::Lookup::Gone => anyhow::bail!("no session {terminal}"),
            hc::Lookup::Unreachable(e) => {
                eprintln!("[radar] watching {terminal}, out of reach right now: {e}");
                (terminal.to_string(), None, None, String::new())
            }
        };
        let (t, n, acc) = (address.clone(), note.to_string(), account.clone());
        crate::history::later(move || {
            crate::history::job_start_on(&t, &n, acc);
        });
        {
            let mut b = lock(&self.book);
            let last = if status.is_empty() { b.seen.get(&address).cloned().unwrap_or_default() } else { status };
            b.watches.insert(address.clone(), Watch { note: note.to_string(), since_ms: crate::history::now_ms(), last, account, created_ms, turn });
            Self::save(&b);
        }
        if let Some(r) = self.radar.get() {
            r.recheck(&address);
        }
        Ok(address)
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
    fn seen(&self, address: &str, status: &str) {
        let mut b = lock(&self.book);
        let mut changed = b.note_seen(address, status);
        if let Some(w) = b.watches.get_mut(address)
            && w.last != status
        {
            w.last = status.to_string();
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
    seen: Arc<Mutex<HashMap<String, Seen>>>,
}

pub fn start(shared: Shared, mouth: Mouth, alerts: Alerts, watches: Watches, asks: super::asks::Asks) {
    // What was known when the last run ended, before this run's first look updates it.
    let before = watches.snapshot();
    let radar = Radar { shared, mouth, alerts, watches: watches.clone(), asks, seen: Arc::new(Mutex::new(HashMap::new())) };
    let _ = watches.radar.set(radar.clone());
    let r = radar.clone();
    radar.asks.set_notify(move |address, text| r.announce(address, text));
    // The cards the last run left, settled before the first look reads the dialogs again.
    radar.asks.restore();
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
            "status": { "state": s.status, "app": s.app },
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
    /// every half minute): what changed is told, what's gone is gone.
    fn resync_local(&self) {
        match hc::list_on(None) {
            Ok(list) => {
                for t in &list {
                    self.observe(t, None, true);
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
        if self.watches.watched(address) && hc::get(address).is_some_and(|t| is_test(&t)) {
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
                    self.fire_watch(address, &name, space, if ev.event == "stop-failed" { "error" } else { "done" }, "");
                }
                self.recheck_soon(address);
            }
            Change::Waiting => self.recheck_soon(address),
            Change::Ended => {
                if self.watches.watched(address) {
                    let (name, space) = self.name_space(address);
                    self.fire_watch(address, &name, space, "exited", "");
                }
            }
            Change::Prompted | Change::Nothing => {}
        }
    }

    fn name_space(&self, address: &str) -> (String, Option<String>) {
        lock(&self.seen).get(address).map(|s| (s.name.clone(), s.space.clone())).unwrap_or_else(|| (address.to_string(), None))
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
        let status = hc::status_word(t).unwrap_or_default();
        let app = hc::agent_app(t).unwrap_or_default();
        let mut name = hc::spoken_name(t);
        if let Some(m) = machine {
            name = format!("{name} on {m}");
        }
        let live = t["state"].as_str().is_none_or(|s| s == "live");
        let is_brain = machine.is_none() && t["name"].as_str() == Some(brain::NAME);
        let is_voice = machine.is_none() && t["name"].as_str() == Some(brain::VOICE_NAME);
        let space = crate::spaces::space_of(t);
        let account = if machine.is_none() { crate::accounts::of_terminal(t) } else { None };
        let (created_ms, turn) = (hc::created_ms(t), hc::turn(t));
        // Another terminal at an address the radar knew: the old one ended.
        let replaced = lock(&self.seen).get(&address).is_some_and(|s| matches!((s.created_ms, created_ms), (Some(a), Some(b)) if a != b));
        if replaced {
            eprintln!("[radar] {address} is a new terminal now: the old one ended");
            self.gone(&address);
        }
        let before = lock(&self.seen).insert(
            address.clone(),
            Seen {
                name: name.clone(),
                status: status.clone(),
                created_ms,
                turn,
                cwd: t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str()).unwrap_or("").to_string(),
                app: app.clone(),
                account,
                live,
                space: space.clone(),
            },
        );

        if is_brain {
            self.shared.update(|s| {
                s.brain = if live { if status.is_empty() { "starting".into() } else { status.clone() } } else { "absent".into() };
                s.brain_app = app.clone();
            });
            return;
        }
        if is_voice || app.is_empty() {
            return;
        }
        // A test session (the tower's own fixtures): seen, never announced.
        if is_test(t) {
            if live && !status.is_empty() {
                self.watches.seen(&address, &status);
            }
            return;
        }
        if live && !status.is_empty() {
            self.watches.seen(&address, &status);
        }
        // What it asks, as a question set (this machine's sessions).
        if machine.is_none() {
            self.asks.seen(&address, t, if live { status.as_str() } else { "" });
        }
        // First sight of a watched session (a machine just reached, a start):
        // how it stood when the watch was set.
        let watch = self.watches.watch(&address);
        let was = match &before {
            Some(b) => b.status.clone(),
            None => watch.as_ref().map(|w| w.last.clone()).unwrap_or_default(),
        };
        let was = was.as_str();
        // A turn that ended since the last look, even if nobody saw it working.
        let before_turn = before.as_ref().and_then(|b| b.turn).or_else(|| watch.as_ref().and_then(|w| w.turn));
        let turned = matches!((before_turn, turn), (Some(a), Some(b)) if b > a) && matches!(status.as_str(), "idle" | "done");
        // A watch on another terminal that had this address: that one ended.
        if let Some(w) = &watch
            && matches!((w.created_ms, created_ms), (Some(a), Some(b)) if a != b)
        {
            self.fire_watch(&address, &name, space.clone(), "exited", "");
            return;
        }
        if was == status && !turned {
            return;
        }
        // Its open job follows what it does: blocked, back to working, done, an
        // error, ended. Only changes seen while the tower is up: what happened
        // while it was down is the start's reconcile (it reports it, then moves the job).
        if let Some(job) = crate::history::JobState::of_session(&status, live)
            && before.is_some()
            && (was != status || turned)
        {
            let a = address.clone();
            crate::history::later(move || crate::history::job_follow(&a, job, ""));
        }
        let stopped_working = (was == "working" && status != "working") || (turned && was != "working");
        // A turn ended: its last message may ask the user something.
        if stopped_working && matches!(status.as_str(), "idle" | "done") {
            self.asks.finished(&address, t);
        }
        // Blocked on a dialog this machine can read: the questions go through
        // the brain (answered for the user, or told to them), not a plain notice.
        let handled = machine.is_none() && status == "blocked" && matches!(t["status"]["kind"].as_str(), Some("question" | "permission"));

        // The brain asked to hear when this one stops.
        if (stopped_working || (status == "blocked" && !handled) || !live) && self.watches.watched(&address) {
            let status = if live { status.clone() } else { "exited".into() };
            self.fire_watch(&address, &name, space.clone(), &status, "");
            return;
        }
        if !announce {
            return;
        }
        // What it asks, in a sentence (or just that it needs you).
        let notice = super::asks::blocked_notice(&name, &t["status"]);
        let news = match status.as_str() {
            "blocked" if self.alerts.blocked && !handled => Some(notice.clone()),
            "done" | "idle" if stopped_working && self.alerts.done => Some(format!("{name} finished")),
            "error" => Some(format!("{name} hit an error")),
            _ => None,
        };
        if let Some(news) = news
            && self.held(&address, &name, space.as_deref(), &news, &status, "alert")
        {
            return;
        }
        if status == "blocked" && self.alerts.blocked && !handled {
            self.shared.alert(&address, &notice, "blocked");
            self.mouth.alert(format!("{notice}."), Some((address.as_str(), status.as_str())));
        } else if stopped_working && (status == "done" || status == "idle") && self.alerts.done {
            self.shared.alert(&address, &format!("{name} finished"), "done");
            self.mouth.alert(format!("{name} finished."), Some((address.as_str(), status.as_str())));
        } else if status == "error" {
            self.shared.alert(&address, &format!("{name} hit an error"), "error");
            if self.alerts.blocked {
                self.mouth.alert(format!("{name} hit an error."), Some((address.as_str(), status.as_str())));
            }
        }
    }

    /// A question a session asks, told to the user (`asks`): as a blocked
    /// session's notice, held for another space in focus as usual.
    fn announce(&self, address: &str, text: &str) {
        let (name, space) = self.name_space(address);
        if self.held(address, &name, space.as_deref(), text, "blocked", "alert") {
            return;
        }
        self.shared.alert(address, text, "blocked");
        if self.alerts.blocked {
            self.mouth.alert(text.to_string(), None);
        }
    }

    /// With a space in focus, news from another space is kept for later
    /// instead of spoken (or handed to the brain): true when it was. An agent
    /// there needing the user still gets a bell and "<space> needs you".
    fn held(&self, address: &str, name: &str, space: Option<&str>, text: &str, status: &str, kind: &str) -> bool {
        let focus = crate::spaces::load().focus;
        let gate = crate::spaces::gate(space, focus.as_deref(), status == "blocked", self.alerts.held_urgent);
        if gate == crate::spaces::Gate::Speak {
            return false;
        }
        let space = space.unwrap_or("");
        let (a, n, sp, t, st, k) =
            (address.to_string(), name.to_string(), space.to_string(), text.to_string(), status.to_string(), kind.to_string());
        crate::history::later(move || crate::spaces::hold(&k, &a, &n, &sp, &t, Some(&st)));
        if gate == crate::spaces::Gate::Urgent {
            self.mouth.alert(format!("{space} needs you."), Some((address, status)));
        }
        eprintln!("[radar] held for {space} (focus: {}): {text}", focus.unwrap_or_default());
        true
    }

    /// Herdr's window shows only terminals placed in a space; `hc run`
    /// leaves them hidden. Sessions the brain starts belong on screen.
    fn show_if_ours(&self, address: &str, t: &Value) {
        if !self.alerts.show_sessions || t["labels"].get("herdr.space").is_some() {
            return;
        }
        let parent = t["parent"].as_str().unwrap_or("");
        let brain = hc::get(brain::NAME).and_then(|b| b["address"].as_str().map(String::from)).unwrap_or_default();
        if parent.is_empty() || parent != brain {
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
        if !["claude", "codex"].contains(&program.as_str()) || t["name"].as_str() == Some(brain::NAME) || is_test(t) {
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
            crate::history::later(move || crate::history::job_follow(&a, crate::history::JobState::Blocked, "asks to trust its folder"));
        } else {
            self.watches.owe(report);
        }
        self.watches.kick();
        true
    }

    fn gone(&self, address: &str) {
        self.asks.seen(address, &Value::Null, "");
        super::sessions::registry().forget(address);
        let removed = lock(&self.seen).remove(address);
        if (address == brain::NAME || removed.as_ref().is_some_and(|_| hc::get(brain::NAME).is_none()))
            && hc::get(brain::NAME).filter(|t| t["state"] == "live").is_none()
        {
            self.shared.update(|s| s.brain = "absent".into());
        }
        // Gone while the tower looked: nothing owed at the next start for it.
        if removed.as_ref().is_some_and(|r| !r.app.is_empty()) {
            self.watches.seen(address, "gone");
            let a = address.to_string();
            crate::history::later(move || crate::history::job_follow(&a, crate::history::JobState::Exited, ""));
        }
        if self.watches.watched(address) {
            let space = removed.as_ref().and_then(|r| r.space.clone());
            let name = removed.map(|r| r.name).unwrap_or_else(|| address.to_string());
            self.fire_watch(address, &name, space, "exited", "");
        }
    }

    /// A watched session stopped (`status`): its report goes out, once
    /// (`why` adds how it was found, e.g. while the tower was down).
    fn fire_watch(&self, address: &str, name: &str, space: Option<String>, status: &str, why: &str) -> bool {
        let fired = self.watches.fire(address, |w| {
            let (t, st, a) = (address.to_string(), status.to_string(), w.note.clone());
            let job = crate::history::JobState::of_session(&st, true).unwrap_or(crate::history::JobState::Exited);
            crate::history::later(move || crate::history::job_watched(&t, job, &a));
            let mut p = report(address, name, space.clone(), status, &w.note, why, "watch");
            // What it said at the end of its turn (its hook): often all the brain needs.
            if let Some(said) = super::sessions::registry().said_since(address, w.since_ms).filter(|_| status != "exited" && status != "gone") {
                let said: String = said.chars().take(1200).collect();
                p.msg = p.msg.replace(
                    " Read its screen and tell me what happened, briefly.",
                    &format!(" It ended its turn saying: \"{}\" Tell me what happened, briefly (read its screen only if that isn't enough).", said.replace('"', "'")),
                );
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
    /// (said out loud once meanwhile). News from a space out of focus is held instead.
    fn deliver_now(&self) {
        self.watches.deliver(|p| {
            let news = p.msg.trim_start_matches("[watch] ").trim_start_matches("[queue] ").to_string();
            if self.held(&p.address, &p.name, p.space.as_deref(), &news, &p.status, &p.kind) {
                return true;
            }
            match brain::note(&p.msg) {
                Ok(()) => {
                    if !p.spoken && p.kind != "queue" {
                        self.shared.alert(&p.address, &format!("{} is {}", p.name, say_status(&p.status)), &p.status);
                    }
                    true
                }
                Err(e) => {
                    if !p.spoken {
                        eprintln!("[radar] the brain can't take it yet ({e:#}): {}", p.msg);
                        if p.kind != "queue" {
                            self.shared.alert(&p.address, &format!("{} is {}", p.name, say_status(&p.status)), &p.status);
                        }
                        if !p.speech.is_empty() {
                            self.mouth.alert(p.speech.clone(), Some((p.address.as_str(), p.status.as_str())));
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
            let status = hc::status_word(&t).unwrap_or_default();
            let turned = me.watches.watch(&terminal).and_then(|w| w.turn).zip(hc::turn(&t)).is_some_and(|(a, b)| b > a);
            if t["state"] == "live" && !matches!(status.as_str(), "done" | "blocked" | "error") && !(turned && status == "idle") {
                return;
            }
            let status = if t["state"] == "live" { status } else { "exited".into() };
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
                        status: hc::status_word(&t).unwrap_or_default(),
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
                status: s.status.clone(),
                app: s.app.clone(),
                account: s.account.clone(),
                question: None,
                folder: s.cwd.clone(),
                queued: 0,
            })
            .filter(|a| !a.name.starts_with("tower"))
            .collect();
        let waiting = super::sendq::waiting_counts();
        for a in &mut agents {
            a.queued = waiting.get(&a.address).copied().unwrap_or(0);
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
fn report(address: &str, name: &str, space: Option<String>, status: &str, asked: &str, why: &str, kind: &str) -> Pending {
    let what = if asked.is_empty() { String::new() } else { format!(" You had asked it: {asked}.") };
    let (msg, speech) = match status {
        "exited" | "gone" => (format!("[watch] {address} ({name}) has exited{why}.{what} Tell me briefly."), format!("{name} has exited.")),
        _ => (
            format!("[watch] {address} ({name}) is now {status}{why}.{what} Read its screen and tell me what happened, briefly."),
            format!("{name} is {}.", say_status(status)),
        ),
    };
    Pending {
        address: address.into(),
        name: name.into(),
        space,
        status: status.into(),
        msg,
        speech,
        kind: kind.into(),
        at_ms: crate::history::now_ms(),
        spoken: false,
    }
}

/// How a report found at start came about.
fn down_why(status: &str, was: &str) -> String {
    let was = if was.is_empty() || was == status { String::new() } else { format!("; it was {was} when last seen") };
    match status {
        "exited" | "gone" => format!(" while the tower was down{was}"),
        _ => format!(" (this happened while the tower was down{was})"),
    }
}

/// A test session: labelled `tower.test`, or named `qfix-…`. It never raises
/// an alert, a trust notice or a question (start one with
/// `hc run --label tower.test=1 --name qfix-…`).
pub fn is_test(t: &Value) -> bool {
    t["labels"].get("tower.test").is_some() || t["name"].as_str().is_some_and(|n| n.starts_with("qfix-"))
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
    fn test_sessions_are_known_by_label_or_name() {
        assert!(is_test(&json!({ "name": "qfix-ask", "labels": {} })));
        assert!(is_test(&json!({ "name": "anything", "labels": { "tower.test": "1" } })));
        assert!(!is_test(&json!({ "name": "orb-rust", "labels": { "herdr.space": "sp1" } })));
        assert!(!is_test(&json!({ "address": "t3" })));
    }
}
