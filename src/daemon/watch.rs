//! The radar: follows every terminal through `hc events` (this machine) and
//! `hc ls` over `hc exec --on` (other machines). It keeps the brain's status
//! and the agent list current, says out loud when an agent needs the user,
//! and fires the brain's watches.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
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
}

impl Default for Watches {
    fn default() -> Self {
        Self {
            book: Arc::new(Mutex::new(Book::load_from(&Book::path()))),
            radar: Arc::new(OnceLock::new()),
            sending: Arc::new(Mutex::new(())),
        }
    }
}

impl Watches {
    fn save(book: &Book) {
        book.save_to(&Book::path());
    }

    pub fn add(&self, terminal: &str, note: &str, account: Option<String>) {
        let (t, n, acc) = (terminal.to_string(), note.to_string(), account.clone());
        crate::history::later(move || {
            crate::history::job_start_on(&t, &n, acc);
        });
        {
            let mut b = self.book.lock().unwrap();
            let last = b.seen.get(terminal).cloned().unwrap_or_default();
            b.watches.insert(terminal.to_string(), Watch { note: note.to_string(), since_ms: crate::history::now_ms(), last, account });
            Self::save(&b);
        }
        if let Some(r) = self.radar.get() {
            r.recheck(terminal);
        }
    }

    /// The watch fires: out of the watches, its report into the outbox, in
    /// one write. None when it had fired already (each finish is told once).
    fn fire(&self, address: &str, report: impl FnOnce(&Watch) -> Pending) -> Option<Pending> {
        let mut b = self.book.lock().unwrap();
        let p = b.fire(address, report)?;
        Self::save(&b);
        Some(p)
    }

    /// A report owed without a watch.
    fn owe(&self, p: Pending) -> bool {
        let mut b = self.book.lock().unwrap();
        let added = b.owe(p);
        if added {
            Self::save(&b);
        }
        added
    }

    /// Remembers a session's status (for the next start's reconcile).
    fn seen(&self, address: &str, status: &str) {
        let mut b = self.book.lock().unwrap();
        let mut changed = b.seen.get(address).map(String::as_str) != Some(status);
        if changed {
            b.seen.insert(address.to_string(), status.to_string());
        }
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
        self.book.lock().unwrap().watches.contains_key(terminal)
    }
    fn snapshot(&self) -> Book {
        self.book.lock().unwrap().clone()
    }
    fn remote(&self) -> Vec<String> {
        self.book.lock().unwrap().watches.keys().filter(|k| k.contains('/')).cloned().collect()
    }
    fn all(&self) -> Vec<String> {
        self.book.lock().unwrap().watches.keys().cloned().collect()
    }

    /// Hands the outbox to `send`, outside the lock (it talks to Herdr);
    /// what it couldn't hand over stays for the next try.
    fn deliver(&self, send: impl FnMut(&mut Pending) -> bool) {
        let _one = self.sending.lock().unwrap();
        let mut pending = {
            let b = self.book.lock().unwrap();
            if b.outbox.is_empty() {
                return;
            }
            Book { outbox: b.outbox.clone(), ..Default::default() }
        };
        let tried = pending.outbox.clone();
        if !pending.deliver(crate::history::now_ms(), send) {
            return;
        }
        let mut b = self.book.lock().unwrap();
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
    let r = radar.clone();
    thread::Builder::new().name("reconcile".into()).spawn(move || r.reconcile(before)).ok();
    let r = radar.clone();
    thread::Builder::new()
        .name("outbox".into())
        .spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(5));
                r.deliver();
            }
        })
        .ok();
    let r = radar.clone();
    thread::Builder::new().name("events".into()).spawn(move || r.follow_local()).ok();
    let r = radar.clone();
    thread::Builder::new().name("remote".into()).spawn(move || r.poll_remote()).ok();
}

impl Radar {
    /// `hc events`, restarted whenever it ends.
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
            let stdout = child.stdout.take().unwrap();
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                self.event(&v);
            }
            let _ = child.wait();
            thread::sleep(Duration::from_secs(2));
        }
    }

    fn event(&self, v: &Value) {
        // The first line: a snapshot of every terminal.
        if let Some(list) = v["terminals"].as_array() {
            for t in list {
                self.observe(t, None, false);
            }
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
                if let Some(t) = hc::get(address) {
                    self.observe(&t, None, true);
                }
            }
            _ => return,
        }
        self.publish();
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
        loop {
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
                            self.seen.lock().unwrap().keys().filter(|k| k.starts_with(&prefix) && !live.contains(k)).cloned().collect();
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
                if !self.seen.lock().unwrap().contains_key(&w)
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
        let before = self.seen.lock().unwrap().insert(
            address.clone(),
            Seen { name: name.clone(), status: status.clone(), app: app.clone(), account, live, space: space.clone() },
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
        let was = before.as_ref().map(|b| b.status.as_str()).unwrap_or("");
        if was == status {
            return;
        }
        let stopped_working = was == "working" && status != "working";
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
        let (name, space) = self.seen.lock().unwrap().get(address).map(|s| (s.name.clone(), s.space.clone())).unwrap_or_else(|| (address.to_string(), None));
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
            crate::history::later(move || crate::history::job_update(&a, "blocked", "asks to trust its folder"));
        } else {
            self.watches.owe(report);
        }
        self.deliver();
        true
    }

    fn gone(&self, address: &str) {
        self.asks.seen(address, &Value::Null, "");
        let removed = self.seen.lock().unwrap().remove(address);
        if (address == brain::NAME || removed.as_ref().is_some_and(|_| hc::get(brain::NAME).is_none()))
            && hc::get(brain::NAME).filter(|t| t["state"] == "live").is_none()
        {
            self.shared.update(|s| s.brain = "absent".into());
        }
        // Gone while the tower looked: nothing owed at the next start for it.
        if removed.as_ref().is_some_and(|r| !r.app.is_empty()) {
            self.watches.seen(address, "gone");
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
            crate::history::later(move || crate::history::job_watched(&t, if st == "gone" { "exited" } else { &st }, &a));
            report(address, name, space.clone(), status, &w.note, why, "watch")
        });
        if fired.is_none() {
            return false;
        }
        eprintln!("[radar] watch on {address} fired: {status}{why}");
        self.deliver();
        true
    }

    /// Hands what's owed to the brain: kept in the outbox until it takes it
    /// (said out loud once meanwhile). News from a space out of focus is held instead.
    fn deliver(&self) {
        self.watches.deliver(|p| {
            let news = p.msg.trim_start_matches("[watch] ").to_string();
            if self.held(&p.address, &p.name, p.space.as_deref(), &news, &p.status, &p.kind) {
                return true;
            }
            match brain::note(&p.msg) {
                Ok(()) => {
                    if !p.spoken {
                        self.shared.alert(&p.address, &format!("{} is {}", p.name, say_status(&p.status)), &p.status);
                    }
                    true
                }
                Err(e) => {
                    if !p.spoken {
                        eprintln!("[radar] the brain can't take it yet ({e:#}): {}", p.msg);
                        self.shared.alert(&p.address, &format!("{} is {}", p.name, say_status(&p.status)), &p.status);
                        self.mouth.alert(p.speech.clone(), Some((p.address.as_str(), p.status.as_str())));
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
            if t["state"] == "live" && !matches!(status.as_str(), "done" | "blocked" | "error") {
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
                    Now { live: t["state"].as_str().is_none_or(|s| s == "live"), status: hc::status_word(&t).unwrap_or_default() },
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
                    if self.watches.owe(p) {
                        let job_status = match status.as_str() {
                            "gone" | "exited" => "exited",
                            "idle" => "done",
                            s => s,
                        }
                        .to_string();
                        crate::history::later(move || crate::history::job_update(&address, &job_status, ""));
                    }
                }
            }
        }
        self.deliver();
    }

    fn publish(&self) {
        let mut agents: Vec<AgentLine> = self
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.live && !s.app.is_empty())
            .map(|(a, s)| AgentLine {
                address: a.clone(),
                name: s.name.clone(),
                status: s.status.clone(),
                app: s.app.clone(),
                account: s.account.clone(),
                question: None,
            })
            .filter(|a| !a.name.starts_with("tower"))
            .collect();
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
