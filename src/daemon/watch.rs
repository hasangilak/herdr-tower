//! The radar: follows every terminal through `hc events` (this machine) and
//! `hc ls` over `hc exec --on` (other machines). It keeps the brain's status
//! and the agent list current, says out loud when an agent needs the user,
//! and fires the brain's watches.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

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
    live: bool,
    /// The Herdr space it's in, for focus (see `spaces`).
    space: Option<String>,
}

/// Terminals the brain asked to hear about: address -> what it asked. Kept
/// in `watches.json` too, so a restarted voice still reports back.
#[derive(Clone)]
pub struct Watches(Arc<Mutex<HashMap<String, String>>>);

impl Default for Watches {
    fn default() -> Self {
        let saved: HashMap<String, String> =
            std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let w = Self(Arc::new(Mutex::new(saved.clone())));
        for t in saved.keys() {
            w.recheck(t);
        }
        w
    }
}

impl Watches {
    fn path() -> std::path::PathBuf {
        config::state_dir().join("watches.json")
    }
    fn save(map: &HashMap<String, String>) {
        let _ = std::fs::write(Self::path(), serde_json::to_string_pretty(map).unwrap_or_default());
    }
    pub fn add(&self, terminal: &str, note: &str) {
        let (t, n) = (terminal.to_string(), note.to_string());
        crate::history::later(move || {
            crate::history::job_start(&t, &n);
        });
        {
            let mut m = self.0.lock().unwrap();
            m.insert(terminal.to_string(), note.to_string());
            Self::save(&m);
        }
        self.recheck(terminal);
    }

    /// A watched terminal may have finished already, before the watch was
    /// asked for (or while the voice was restarting). A prompt sent just now
    /// has started within a few seconds.
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
            if let Some(asked) = me.take(&terminal) {
                let name = hc::spoken_name(&t);
                let status = if t["state"] == "live" { status } else { "exited".into() };
                {
                    let (t, st, a) = (terminal.to_string(), status.to_string(), asked.clone());
                    crate::history::later(move || crate::history::job_watched(&t, &st, &a));
                }
                let what = if asked.is_empty() { String::new() } else { format!(" You had asked it: {asked}.") };
                // Another space than the one in focus: keep it for later (no bell here).
                let space = crate::spaces::space_of(&t);
                let focus = crate::spaces::load().focus;
                if crate::spaces::gate(space.as_deref(), focus.as_deref(), false, false) != crate::spaces::Gate::Speak {
                    let report = format!("{name} is now {status}.{what}");
                    crate::spaces::hold("watch", &terminal, &name, space.as_deref().unwrap_or(""), &report, Some(&status));
                    return;
                }
                let _ = brain::note(&format!(
                    "[watch] {terminal} ({name}) is now {status}.{what} Read its screen and tell me what happened, briefly."
                ));
            }
        });
    }
    fn take(&self, terminal: &str) -> Option<String> {
        let mut m = self.0.lock().unwrap();
        let note = m.remove(terminal);
        if note.is_some() {
            Self::save(&m);
        }
        note
    }
    fn remote(&self) -> Vec<String> {
        self.0.lock().unwrap().keys().filter(|k| k.contains('/')).cloned().collect()
    }
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().keys().cloned().collect()
    }
}

#[derive(Clone)]
struct Radar {
    shared: Shared,
    mouth: Mouth,
    alerts: Alerts,
    watches: Watches,
    seen: Arc<Mutex<HashMap<String, Seen>>>,
}

pub fn start(shared: Shared, mouth: Mouth, alerts: Alerts, watches: Watches) {
    let radar = Radar { shared, mouth, alerts, watches, seen: Arc::new(Mutex::new(HashMap::new())) };
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
        for w in self.watches.all().into_iter().filter(|w| !w.contains('/')) {
            match hc::get(&w) {
                Some(t) if t["state"] == "live" => self.check_startup(&w, &t),
                _ => self.gone(&w),
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
        let before = self.seen.lock().unwrap().insert(
            address.clone(),
            Seen { name: name.clone(), status: status.clone(), app: app.clone(), live, space: space.clone() },
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
        let was = before.as_ref().map(|b| b.status.as_str()).unwrap_or("");
        if was == status {
            return;
        }
        let stopped_working = was == "working" && status != "working";

        // The brain asked to hear when this one stops.
        if (stopped_working || status == "blocked" || !live)
            && let Some(asked) = self.watches.take(&address)
        {
            let what = if asked.is_empty() { String::new() } else { format!(" You had asked it: {asked}.") };
            let msg = format!("[watch] {address} ({name}) is now {status}.{what} Read its screen and tell me what happened, briefly.");
                {
                    let (t, st, a) = (address.to_string(), status.to_string(), asked.clone());
                    crate::history::later(move || crate::history::job_watched(&t, &st, &a));
                }
            if self.held(&address, &name, space.as_deref(), &format!("{name} is now {status}.{what}"), &status, "watch") {
                return;
            }
            if let Err(e) = brain::note(&msg) {
                eprintln!("[radar] watch note: {e:#}");
                self.mouth.alert(format!("{name} is {}.", say_status(&status)), Some((address.as_str(), status.as_str())));
            }
            self.shared.alert(&address, &format!("{name} is {}", say_status(&status)), &status);
            return;
        }
        if !announce {
            return;
        }
        let news = match status.as_str() {
            "blocked" if self.alerts.blocked => Some(format!("{name} needs you")),
            "done" | "idle" if stopped_working && self.alerts.done => Some(format!("{name} finished")),
            "error" => Some(format!("{name} hit an error")),
            _ => None,
        };
        if let Some(news) = news
            && self.held(&address, &name, space.as_deref(), &news, &status, "alert")
        {
            return;
        }
        if status == "blocked" && self.alerts.blocked {
            self.shared.alert(&address, &format!("{name} needs you"), "blocked");
            self.mouth.alert(format!("{name} needs you."), Some((address.as_str(), status.as_str())));
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
        let (a, n, sp, t, st, k) = (address.to_string(), name.to_string(), space.to_string(), text.to_string(), status.to_string(), kind.to_string());
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
        if !["claude", "codex"].contains(&program.as_str()) || t["name"].as_str() == Some(brain::NAME) {
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
        if self.held(address, &name, crate::spaces::space_of(&t).as_deref(), &format!("{name} asks to trust its folder {folder}"), "blocked", "alert") {
            if self.watches.take(address).is_some() {
                let a = address.to_string();
                crate::history::later(move || crate::history::job_update(&a, "blocked", "asks to trust its folder"));
            }
            return true;
        }
        self.shared.alert(address, &format!("{name} asks to trust its folder"), "blocked");
        let msg = format!(
            "[watch] {address} ({name}) is waiting on its first-run question: may it trust the folder {folder}? It can't start until someone answers. Ask me before you answer it."
        );
        if self.watches.take(address).is_some() {
            let a = address.to_string();
            crate::history::later(move || crate::history::job_update(&a, "blocked", "asks to trust its folder"));
        }
        if brain::note(&msg).is_err() {
            self.mouth.alert(format!("{name} is asking whether to trust its folder."), None);
        }
        true
    }

    fn gone(&self, address: &str) {
        let removed = self.seen.lock().unwrap().remove(address);
        if (address == brain::NAME || removed.as_ref().is_some_and(|_| hc::get(brain::NAME).is_none()))
            && hc::get(brain::NAME).filter(|t| t["state"] == "live").is_none()
        {
            self.shared.update(|s| s.brain = "absent".into());
        }
        if let Some(asked) = self.watches.take(address) {
            let (a, q) = (address.to_string(), asked.clone());
            crate::history::later(move || crate::history::job_watched(&a, "exited", &q));
            let space = removed.as_ref().and_then(|r| r.space.clone());
            let name = removed.map(|r| r.name).unwrap_or_else(|| address.to_string());
            if self.held(address, &name, space.as_deref(), &format!("{name} has exited. You had asked it: {asked}."), "exited", "watch") {
                return;
            }
            let _ = brain::note(&format!("[watch] {address} ({name}) has exited. You had asked it: {asked}. Tell me briefly."));
        }
    }

    fn publish(&self) {
        let mut agents: Vec<AgentLine> = self
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.live && !s.app.is_empty())
            .map(|(a, s)| AgentLine { address: a.clone(), name: s.name.clone(), status: s.status.clone(), app: s.app.clone() })
            .filter(|a| !a.name.starts_with("tower"))
            .collect();
        agents.sort_by(|a, b| a.address.cmp(&b.address));
        self.shared.update(|s| s.agents = agents);
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
