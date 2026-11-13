//! The brains in the daemon (see `crate::brains`): which run, which space
//! each is for, who gets a note or a request, and when a space's own brain
//! is started and stopped.
//!
//! - The radar tells it of every brain terminal it sees (`seen`, `gone`):
//!   the main one and each `tower-<space>`. That is all it knows them by,
//!   so a restarted daemon finds the brains that were running again.
//! - With the missions' look at the sessions (every 20 seconds, and when
//!   one starts or ends) comes `round`: `brains::step` says which spaces'
//!   brains to start or stop, and each running one's instructions are
//!   written again as its space stands now.
//! - `note` and `ask` hand a note or a request to the brain of its space
//!   when that one can take it, else to the main brain.
//! - The state carries them: `brains`, `brain_model`, and each mission's
//!   `tower`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use anyhow::Result;
use serde_json::{Value, json};

use super::narrate::Narrator;
use super::state::{Shared, lock};
use crate::brain::{self, Target};
use crate::brains::{self, Do, Known, Life, SpaceNow, To};
use crate::config::Config;
use crate::hc;

#[derive(Clone)]
pub struct Brains {
    inner: Arc<Mutex<Inner>>,
    shared: Shared,
    narrator: Narrator,
}

#[derive(Default)]
struct Inner {
    /// Every brain terminal Herdr has, by its name.
    known: BTreeMap<String, Known>,
    /// What is kept about each space's brain between looks.
    lives: BTreeMap<String, Life>,
    /// The spaces that are due a brain of their own, as of the last look.
    wanted: BTreeSet<String>,
}

static RUNNING: OnceLock<Brains> = OnceLock::new();

/// The daemon's brains, once it runs (None in a command, and in tests).
pub fn get() -> Option<&'static Brains> {
    RUNNING.get()
}

/// The settings as config.toml has them now (a file that doesn't read: as they last read).
fn settings() -> Config {
    Config::live()
}

/// A terminal's command line.
fn argv_of(t: &Value) -> Vec<String> {
    t["argv"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect()
}

/// A brain's terminal as the tower keeps it, on what it knew (`before`): a
/// look that says less (a row of `hc ls` has no labels and no command line)
/// blanks nothing.
pub fn known_of(name: &str, t: &Value, before: Option<&Known>) -> Known {
    let argv = argv_of(t);
    let main = !brains::is_space_brain(name);
    Known {
        name: name.to_string(),
        // Its label says its space; a look without labels keeps what was known; its folder knows too.
        space: match main {
            true => None,
            false => t["labels"][brains::LABEL_SPACE]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| before.and_then(|b| b.space.clone()))
                .or_else(|| brains::card_of(name).map(|c| c.space)),
        },
        address: t["address"].as_str().map(String::from).or_else(|| before.map(|b| b.address.clone())).unwrap_or_default(),
        live: t["state"].as_str().is_none_or(|s| s == "live"),
        status: hc::status(t),
        argv: if argv.is_empty() { before.map(|b| b.argv.clone()).unwrap_or_default() } else { argv },
        created_ms: hc::created_ms(t).or_else(|| before.and_then(|b| b.created_ms)),
    }
}

/// The spaces' own brains as each mission's `tower`: `{"name", "status",
/// "address"}` for a space that has one (`status`: `ready`, `working`,
/// `starting`, `blocked`; `off` for one that is due and doesn't run).
pub fn towers_of(known: &[Known], wanted: &BTreeSet<String>) -> BTreeMap<String, Value> {
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for space in wanted {
        out.insert(space.clone(), json!({ "name": brains::name_for(space), "status": "off", "address": null }));
    }
    for k in known.iter().filter(|k| k.live) {
        if let Some(space) = &k.space {
            out.insert(space.clone(), json!({ "name": k.name, "status": k.word(), "address": k.address }));
        }
    }
    out
}

/// Each mission's `tower`, set from `towers` (by the space's name, any case; none: null).
pub fn mark_missions(missions: &mut Value, towers: &BTreeMap<String, Value>) {
    for m in missions["spaces"].as_array_mut().into_iter().flatten() {
        let name = m["name"].as_str().unwrap_or("").to_string();
        m["tower"] = towers.iter().find(|(k, _)| k.eq_ignore_ascii_case(&name)).map(|(_, v)| v.clone()).unwrap_or(Value::Null);
    }
}

/// The brains as the state lists them: the main one first.
pub fn listed(known: &[Known]) -> Vec<Value> {
    let mut rows: Vec<&Known> = known.iter().filter(|k| k.live).collect();
    rows.sort_by(|a, b| (a.space.is_some(), &a.name).cmp(&(b.space.is_some(), &b.name)));
    rows.iter()
        .map(|k| {
            let s = brains::started_with(&k.argv);
            json!({ "name": k.name, "space": k.space, "address": k.address, "status": if k.status == hc::SessionStatus::Starting { "starting" } else { k.status.word() }, "model": s.model, "effort": s.effort })
        })
        .collect()
}

/// The brains that run, for the model's report.
pub fn running_of(known: &[Known]) -> Vec<brains::Running> {
    let mut rows: Vec<&Known> = known.iter().filter(|k| k.live).collect();
    rows.sort_by(|a, b| (a.space.is_some(), &a.name).cmp(&(b.space.is_some(), &b.name)));
    rows.iter().map(|k| brains::Running { brain: k.name.clone(), space: k.space.clone(), address: k.address.clone(), argv: k.argv.clone() }).collect()
}

/// The spaces' brains now, for the missions (`{}` before the daemon's brains run).
pub fn towers() -> BTreeMap<String, Value> {
    match get() {
        Some(b) => {
            let inner = lock(&b.inner);
            towers_of(&inner.known.values().cloned().collect::<Vec<_>>(), &inner.wanted)
        }
        None => BTreeMap::new(),
    }
}

/// A terminal at this address is a brain the tower knows.
pub fn is_brain_address(address: &str) -> bool {
    get().is_some_and(|b| lock(&b.inner).known.values().any(|k| k.address == address))
}

/// The brain whose terminal this is: its name.
pub fn name_at(address: &str) -> Option<String> {
    get().and_then(|b| lock(&b.inner).known.values().find(|k| k.address == address || k.name == address).map(|k| k.name.clone()))
}

/// The space of the brain called `name` (None: the main one, or not known).
pub fn space_of(name: &str) -> Option<String> {
    get().and_then(|b| lock(&b.inner).known.get(name).and_then(|k| k.space.clone()))
}

/// The radar saw a brain's terminal (`name`: `tower`, `tower-<space>`).
pub fn seen(name: &str, t: &Value) {
    if let Some(b) = get() {
        b.seen(name, t);
    }
}

/// The radar lost the terminal at `address`.
pub fn gone(address: &str) {
    if let Some(b) = get() {
        b.gone(address);
    }
}

/// Where what is about `space` goes now.
fn target(space: Option<&str>) -> Target {
    let Some(b) = get() else { return Target::main() };
    let known: Vec<Known> = lock(&b.inner).known.values().cloned().collect();
    match brains::route(space, settings().brains.per_space, &known) {
        To::Main => Target::main(),
        To::Space { brain, address, space } => Target { name: brain, terminal: address, space: Some(space) },
    }
}

/// Where a session's message for the main brain goes instead: the address
/// of its own space's brain, when that space has one that can take it (a
/// session's own tower is its space's). None: as it was addressed.
pub fn own_tower_of(sender_space: Option<&str>, sender_is_brain: bool, to_main: bool) -> Option<String> {
    let b = get()?;
    let known: Vec<Known> = lock(&b.inner).known.values().cloned().collect();
    match brains::session_to(sender_space, sender_is_brain, to_main, settings().brains.per_space, &known) {
        To::Space { address, .. } => Some(address),
        To::Main => None,
    }
}

/// The space a session is in, as Herdr says now.
fn space_at(address: &str) -> Option<String> {
    hc::get(address).as_ref().and_then(crate::spaces::space_of)
}

/// `note`, for a note that names its session (`[question] t5 …`): to the
/// brain of that session's space.
pub fn note_about(text: &str, record: bool) -> Result<()> {
    let space = get().and_then(|_| brains::note_space(None, text, &space_at));
    note(space.as_deref(), text, record)
}

/// Hands a note to the brain it belongs to: the brain of `space`, if that
/// space has one that can take it; else the main brain. A space's brain
/// that fails to take it (ended this moment, stuck on its own screen)
/// passes it to the main one too, so nothing is dropped.
pub fn note(space: Option<&str>, text: &str, record: bool) -> Result<()> {
    if get().is_none() {
        return brain::note_as(text, record);
    }
    let to = target(space);
    if to.space.is_none() {
        return brain::note_to(&to, text, record);
    }
    match brain::note_to(&to, text, record) {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("[brains] {} can't take it ({e:#}): to the main brain", to.name);
            brain::note_to(&Target::main(), text, record)
        }
    }
}

/// The words the main brain gets when a space's own can't take what was
/// asked there: the space is named in front (as the panel did it before
/// spaces had towers).
pub fn in_space(space: &str, text: &str) -> String {
    format!("In {space}: {text}")
}

/// The space `asked` names among those the state has (its whole name in
/// any case, else as it was asked).
fn space_named(shared: &Shared, asked: &str) -> String {
    let names: Vec<String> = shared.with(|s| s.missions["spaces"].as_array().into_iter().flatten().filter_map(|m| m["name"].as_str().map(String::from)).collect());
    crate::spaces::resolve(asked, &names).unwrap_or_else(|| asked.trim().to_string())
}

/// The radar line a space's brain gets: its own sessions only.
fn radar_of(shared: &Shared, space: &str) -> String {
    let (mine, agents) = shared.with(|s| {
        let mine: Vec<String> = s.missions["spaces"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(space)))
            .flat_map(|m| m["sessions"].as_array().cloned().unwrap_or_default())
            .filter_map(|x| x["address"].as_str().map(String::from))
            .collect();
        (mine, s.agents.clone())
    });
    let agents: Vec<super::state::AgentLine> = agents.into_iter().filter(|a| mine.contains(&a.address)).collect();
    super::stt::radar_line(&agents)
}

/// Hands what the user said to a brain. Asked in a space's view (`space`):
/// that space's own brain gets it, with its own sessions as the radar
/// line, and the request is kept as that space's. Asked of the tower, or of
/// a space whose brain can't take it: the main brain (for a space, with the
/// space named in front).
pub fn ask(space: Option<&str>, text: &str, shared: &Shared, source: &str) -> Result<()> {
    let Some(asked) = space.map(str::trim).filter(|s| !s.is_empty()) else {
        return brain::ask(text, &super::stt::radar(shared), source);
    };
    let name = space_named(shared, asked);
    let to = target(Some(&name));
    if let Some(own) = to.space.clone() {
        match brain::ask_as(&to, text, &radar_of(shared, &own), source) {
            Ok(()) => {
                eprintln!("[brains] asked of {} ({own})", to.name);
                return Ok(());
            }
            Err(e) => eprintln!("[brains] {} can't take the request ({e:#}): to the main brain", to.name),
        }
    }
    brain::ask(&in_space(&name, text), &super::stt::radar(shared), source)
}

/// The user takes back the request at `at` (the chat's "Take back"): its
/// brain is stopped (`hc interrupt`) if that is still the latest request
/// it was given and it is working, and the chat is told no answer comes.
/// One that was answered already, or given up on, is left as it is.
pub fn withdraw(at: u64) -> Result<Value> {
    let entries = crate::history::load_tail(4 << 20);
    let Some(asked) = entries.iter().find(|e| e.at == at && e.kind == "request") else {
        anyhow::bail!("there is no request at {at} to take back");
    };
    if entries.iter().any(|e| e.reply_to == Some(at) && matches!(e.kind.as_str(), "answer" | "unanswered")) {
        return Ok(json!({ "taken_back": false, "why": "it was answered already" }));
    }
    let space = asked.space.clone();
    // Its brain, if that brain still runs for it (a space's that is gone is not the main one's).
    let to = target(space.as_deref());
    let working = to.space == space && latest_request(&entries, space.as_deref()) == Some(at) && hc::get(&to.terminal).is_some_and(|t| hc::status(&t) == hc::SessionStatus::Working);
    let stopped = working && hc::run(&["interrupt", &to.terminal]).is_ok_and(|o| o.code == 0);
    if to.space.is_none() && (stopped || latest_request(&entries, None) == Some(at)) {
        crate::activity::clear();
    }
    crate::history::record_unanswered(at, "taken-back", "You took it back.", space);
    eprintln!("[brains] the request at {at} was taken back{}", if stopped { &", its brain stopped" } else { &"" });
    Ok(json!({ "taken_back": true, "stopped": stopped }))
}

/// The latest request of the user's to a brain (`space`: a space's own,
/// None: the main one), by its `at`.
fn latest_request(entries: &[crate::history::Entry], space: Option<&str>) -> Option<u64> {
    entries
        .iter()
        .rev()
        .filter(|e| e.kind == "request" && e.space.as_deref() == space)
        .find(|e| !e.source.as_deref().is_some_and(|s| s.starts_with("terminal:") || s.starts_with("answer:")))
        .map(|e| e.at)
}

/// Tells every brain that runs (but `except`, by its terminal's address)
/// to read its instructions again: `what` says why, and each is given its
/// own file. Each is told on a thread of its own (typing waits for a
/// person to pause): joined, one gives the brain's name if it took it.
pub fn reread(what: &str, except: Option<&str>) -> Vec<thread::JoinHandle<Option<String>>> {
    let mut told = vec![];
    let main = crate::config::brain_dir().join("CLAUDE.md");
    let mut all: Vec<(Target, std::path::PathBuf)> = vec![(Target::main(), main)];
    // (In a command, with no daemon's brains to ask: as Herdr has them.)
    let known: Vec<Known> = match get() {
        Some(b) => lock(&b.inner).known.values().cloned().collect(),
        None => brains::known_from_herdr(),
    };
    for k in known.iter().filter(|k| k.live && k.space.is_some()) {
        all.push((Target { name: k.name.clone(), terminal: k.address.clone(), space: k.space.clone() }, brains::dir_of(&k.name).join("CLAUDE.md")));
    }
    let except = except.map(str::trim).filter(|a| !a.is_empty()).and_then(|a| hc::get(a).and_then(|t| t["address"].as_str().map(String::from)));
    for (to, file) in all {
        // The one that made the change knows it.
        if except.is_some() && hc::get(&to.terminal).and_then(|t| t["address"].as_str().map(String::from)) == except {
            continue;
        }
        let note = format!("[behaviors] {what}: read {} again, and write nothing.", file.display());
        told.push(thread::spawn(move || brain::note_to(&to, &note, false).is_ok().then_some(to.name)));
    }
    told
}

impl Brains {
    pub fn start(shared: Shared, narrator: Narrator) -> Brains {
        let b = Brains { inner: Arc::default(), shared, narrator };
        // A daemon that tests the tower beside the real one keeps its hands
        // off the real tower's brains: it knows none, starts and stops
        // none, and everything of its own goes to its own main brain.
        if !super::watch::test_daemon() {
            let _ = RUNNING.set(b.clone());
        }
        b.publish();
        b
    }

    fn known(&self) -> Vec<Known> {
        lock(&self.inner).known.values().cloned().collect()
    }

    fn seen(&self, name: &str, t: &Value) {
        let k = {
            let mut inner = lock(&self.inner);
            // A `tower-…` terminal that isn't run from this tower's folders is somebody else's.
            if !inner.known.contains_key(name) && !brains::is_ours(name, t) {
                return;
            }
            let k = known_of(name, t, inner.known.get(name));
            if inner.known.get(name) == Some(&k) {
                return;
            }
            inner.known.insert(name.to_string(), k.clone());
            k
        };
        // A space's own brain speaks like the main one: its transcripts are followed.
        if k.space.is_some() && k.live {
            self.narrator.follow(&k.name, &brains::dir_of(&k.name));
        }
        self.publish();
    }

    fn gone(&self, address: &str) {
        let lost: Vec<String> = {
            let mut inner = lock(&self.inner);
            let names: Vec<String> = inner.known.values().filter(|k| k.address == address).map(|k| k.name.clone()).collect();
            inner.known.retain(|_, k| k.address != address);
            names
        };
        if lost.is_empty() {
            return;
        }
        for name in &lost {
            eprintln!("[brains] {name} ({address}) is gone");
            if brains::is_space_brain(name) {
                self.narrator.unfollow(name);
                brain::forget_turn(name);
            }
        }
        self.publish();
    }

    /// The brains Herdr has now, taken in at once (the radar tells of them
    /// as it sees them; a daemon that has only just started may be asked
    /// about them before its first look).
    fn discover(&self) {
        for k in brains::known_from_herdr() {
            if let Some(t) = hc::get(&k.address) {
                self.seen(&k.name, &t);
            }
        }
    }

    /// The brains' model now (`brain_model`).
    pub fn model(&self) -> Value {
        brains::report(&settings(), &running_of(&self.known()))
    }

    /// The state says which brains run, on what, and each mission's own.
    pub fn publish(&self) {
        let (known, wanted) = {
            let inner = lock(&self.inner);
            (inner.known.values().cloned().collect::<Vec<_>>(), inner.wanted.clone())
        };
        let (list, model, towers) = (listed(&known), brains::report(&settings(), &running_of(&known)), towers_of(&known, &wanted));
        self.shared.update(|s| {
            s.brains = list;
            s.brain_model = model;
            mark_missions(&mut s.missions, &towers);
        });
    }

    /// One look, with the missions': `sessions` are the live sessions of
    /// every space, `spaces` the missions as kept, `view` the state's
    /// missions (a space's row describes it to its brain). Refreshes what
    /// each brain is doing, starts and stops the spaces' own, and keeps
    /// their instructions current.
    pub fn round(&self, sessions: &[crate::missions::Seen], spaces: &crate::spaces::Spaces, view: &Value) {
        let now = crate::history::now_ms();
        let cfg = settings();
        // Each brain as Herdr has it now (the radar tells of changes; this catches what it missed).
        for k in self.known() {
            match hc::lookup(&k.address) {
                hc::Lookup::Found(t) if hc::name(&t) == Some(k.name.as_str()) => self.seen(&k.name, &t),
                hc::Lookup::Found(_) | hc::Lookup::Gone => self.gone(&k.address),
                hc::Lookup::Unreachable(_) => {}
            }
        }
        let known = self.known();
        let up: Vec<String> = known.iter().filter(|k| k.live).filter_map(|k| k.space.clone()).collect();
        let mut now_spaces: BTreeMap<String, SpaceNow> = BTreeMap::new();
        for x in sessions {
            now_spaces.entry(x.space.clone()).or_default().sessions += 1;
        }
        for (name, m) in &spaces.missions {
            if m.state == crate::missions::State::Closed || m.archived_ms.is_some() {
                now_spaces.entry(name.clone()).or_default().over = true;
            }
        }
        // A brain whose space has no session left: is the space itself still there?
        let empty: Vec<&String> = up.iter().filter(|s| now_spaces.get(*s).is_none_or(|n| n.sessions == 0)).collect();
        if !empty.is_empty() {
            let windows = crate::spaces::windows();
            for s in empty {
                if !windows.is_empty() && !windows.iter().any(|w| w.name.eq_ignore_ascii_case(s)) {
                    now_spaces.entry(s.clone()).or_default().gone = true;
                }
            }
        }
        let main_up = known.iter().any(|k| k.space.is_none() && k.live) || hc::is_live(brain::NAME);
        let per_space = cfg.brains.per_space;
        let todo = {
            let mut inner = lock(&self.inner);
            let todo = brains::step(&mut inner.lives, &now_spaces, &up, per_space, main_up, now);
            inner.wanted = if per_space { now_spaces.iter().filter(|(_, s)| s.sessions > 0 && !s.over).map(|(n, _)| n.clone()).collect() } else { BTreeSet::new() };
            todo
        };
        let mission_of = |space: &str| -> Value {
            view["spaces"].as_array().into_iter().flatten().find(|m| m["name"].as_str() == Some(space)).cloned().unwrap_or_else(|| json!({ "name": space }))
        };
        for d in todo {
            match d {
                Do::Start(space) => match brain::launch_space(&space, &mission_of(&space), &cfg) {
                    Ok(address) => {
                        let name = brains::name_for(&space);
                        eprintln!("[brains] started {name} ({address}) for {space}");
                        if let Some(t) = hc::get(&address) {
                            self.seen(&name, &t);
                        }
                        // Its folder is new to its program: the first-run question is ours to answer.
                        thread::spawn(move || brain::settle(&address));
                    }
                    Err(e) => eprintln!("[brains] {space}'s own brain didn't start: {e:#}"),
                },
                Do::Stop { space, why } => {
                    for k in self.known().into_iter().filter(|k| k.space.as_deref() == Some(space.as_str())) {
                        eprintln!("[brains] stopping {} ({}): {why}", k.name, k.address);
                        brain::stop(&k.address);
                        self.gone(&k.address);
                    }
                }
            }
        }
        // Each running brain's instructions say how its space stands now (it reads them when told to).
        for k in self.known().iter().filter(|k| k.live) {
            if let Some(space) = &k.space
                && let Err(e) = brains::write_space_files(space, &mission_of(space))
            {
                eprintln!("[brains] {}: its folder couldn't be written: {e:#}", k.name);
            }
        }
        self.publish();
    }

    /// A space is being closed (`tower space close`): its brain is ended
    /// first, and not started again for a while.
    pub fn close(&self, space: &str) -> Vec<String> {
        let now = crate::history::now_ms();
        let name = brains::name_for(space);
        if !self.known().iter().any(|k| k.name == name) {
            self.discover();
        }
        let mut ended = vec![];
        for k in self.known().into_iter().filter(|k| k.name == name || k.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(space))) {
            eprintln!("[brains] stopping {} ({}): its space is being closed", k.name, k.address);
            brain::stop(&k.address);
            self.gone(&k.address);
            ended.push(k.name);
        }
        {
            let mut inner = lock(&self.inner);
            let held = now + 2 * 60_000;
            let keys: Vec<String> = inner.lives.keys().filter(|k| k.eq_ignore_ascii_case(space)).cloned().collect();
            for k in keys {
                inner.lives.entry(k).and_modify(|l| l.held_until_ms = held);
            }
            inner.lives.entry(space.to_string()).or_insert_with(|| Life { last_live_ms: now, held_until_ms: held, ..Default::default() });
        }
        self.publish();
        ended
    }

    /// Restarts every brain that runs, so each starts on the setting as it
    /// is now: the spaces' own are stopped (the next look starts them
    /// again, at once), the main one is stopped and started here. Returns
    /// the names of those it restarts.
    pub fn restart(&self) -> Vec<String> {
        self.restart_as(super::watch::test_daemon())
    }

    /// `restart`. `testing`: this daemon tests the tower beside the real
    /// one (`HERDER_TOWER_TEST`); the brains Herdr has are the real tower's,
    /// and it restarts none of them.
    fn restart_as(&self, testing: bool) -> Vec<String> {
        if testing {
            eprintln!("[brains] a test daemon restarts no brain: those that run are the real tower's");
            return vec![];
        }
        self.discover();
        let known: Vec<Known> = self.known().into_iter().filter(|k| k.live).collect();
        let names: Vec<String> = known.iter().map(|k| k.name.clone()).collect();
        let me = self.clone();
        thread::spawn(move || {
            for k in known.iter().filter(|k| k.space.is_some()) {
                eprintln!("[brains] restarting {} ({})", k.name, k.address);
                brain::stop(&k.address);
                me.gone(&k.address);
            }
            {
                // Back at once, whatever came before.
                let mut inner = lock(&me.inner);
                for l in inner.lives.values_mut() {
                    (l.tries, l.next_try_ms, l.up_since_ms) = (0, 0, None);
                }
            }
            if let Some(main) = known.iter().find(|k| k.space.is_none()) {
                eprintln!("[brains] restarting {} ({})", main.name, main.address);
                brain::stop(&main.address);
                me.gone(&main.address);
                let cfg = settings();
                // The program it ran (it may have been started as the other one: `tower up --brain`).
                let which = match brains::started_with(&main.argv).program.as_str() {
                    p @ ("claude" | "codex") => p.to_string(),
                    _ => cfg.brain.clone(),
                };
                match brain::launch(&cfg, &which) {
                    Ok(address) => {
                        eprintln!("[brains] {} runs again as {address}", brain::NAME);
                        if let Some(t) = hc::get(&address) {
                            me.seen(brain::NAME, &t);
                        }
                    }
                    Err(e) => eprintln!("[brains] the main brain didn't start again: {e:#}"),
                }
            }
            // The spaces' own come back with the next look.
            super::missions::kick();
        });
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hc::SessionStatus;

    #[test]
    fn a_test_daemon_restarts_no_brain() {
        // It was told of the real tower's brain (a `tower model --restart` in a sandbox finds it by its name).
        let mouth = crate::daemon::tts::Mouth::silent(Arc::new(crate::daemon::lan::hub::Hub::default()));
        let b = Brains { inner: Arc::default(), shared: Shared::new(crate::daemon::state::State::new("ptt", "test", "test")), narrator: Narrator::start(false, mouth, 300) };
        b.seen("tower", &json!({ "address": "t1", "name": "tower", "state": "live", "argv": ["claude", "-n", "tower"], "status": { "state": "idle" } }));
        assert_eq!(b.known().len(), 1);
        assert!(b.restart_as(true).is_empty(), "the real tower's brain is left running");
        assert!(b.known()[0].live);
    }

    #[test]
    fn a_brains_terminal_is_kept_as_herdr_describes_it() {
        // As `hc get` has a space's brain.
        let whole = json!({
            "address": "t12", "name": "tower-sandbox", "state": "live", "created_ms": 7000, "cwd": "/h/brains/sandbox",
            "argv": ["claude", "-n", "tower-sandbox", "--model", "claude-opus-5-5", "--effort", "high"],
            "labels": { "tower.space": "Sandbox" }, "status": { "app": "claude-code", "state": "idle" },
        });
        let k = known_of("tower-sandbox", &whole, None);
        assert_eq!((k.space.as_deref(), k.address.as_str(), k.live, k.status.clone(), k.created_ms), (Some("Sandbox"), "t12", true, SessionStatus::Idle, Some(7000)));
        assert_eq!(k.argv.len(), 7);
        assert!(k.takes() && k.word() == "ready");
        // As a row of `hc ls` has it later: no labels, no command line. What was known stays.
        let row = json!({ "address": "t12", "name": "tower-sandbox", "agent": "claude-code", "state": "live", "status": "working", "turn": 3 });
        let later = known_of("tower-sandbox", &row, Some(&k));
        assert_eq!((later.space.as_deref(), later.argv.len(), later.status.clone(), later.created_ms, later.word()), (Some("Sandbox"), 7, SessionStatus::Working, Some(7000), "working"));
        // Still starting: no status yet. It takes nothing, and its space's view says so.
        let fresh = known_of("tower-docs", &json!({ "address": "t13", "name": "tower-docs", "state": "live", "labels": { "tower.space": "Docs" }, "argv": ["claude"] }), None);
        assert_eq!((fresh.status.clone(), fresh.takes(), fresh.word()), (SessionStatus::Starting, false, "starting"));
        // The main brain has no space, whatever its labels say.
        let main = known_of("tower", &json!({ "address": "t7", "name": "tower", "state": "live", "labels": { "tower.space": "X" }, "argv": ["claude", "-n", "tower", "--model", "sonnet", "--effort", "medium"], "status": { "state": "done" } }), None);
        assert_eq!((main.space.clone(), main.takes()), (None, true));
        // Its program ended: the terminal is there, the brain isn't.
        let ended = known_of("tower-sandbox", &json!({ "address": "t12", "name": "tower-sandbox", "state": "finished" }), Some(&k));
        assert_eq!((ended.live, ended.takes(), ended.word()), (false, false, "off"));

        // The state's list, the model's report and the missions' `tower` are made from them.
        let known = [k.clone(), fresh.clone(), main.clone(), ended];
        let list = listed(&[fresh.clone(), k.clone(), main.clone()]);
        assert_eq!(list.iter().map(|b| (b["name"].as_str().unwrap(), b["status"].as_str().unwrap(), b["model"].as_str())).collect::<Vec<_>>(), [("tower", "done", Some("sonnet")), ("tower-docs", "starting", None), ("tower-sandbox", "idle", Some("claude-opus-5-5"))]);
        assert_eq!(list[2], json!({ "name": "tower-sandbox", "space": "Sandbox", "address": "t12", "status": "idle", "model": "claude-opus-5-5", "effort": "high" }));
        let running = running_of(&[k.clone(), main.clone()]);
        assert_eq!(running.iter().map(|r| (r.brain.as_str(), r.space.as_deref())).collect::<Vec<_>>(), [("tower", None), ("tower-sandbox", Some("Sandbox"))]);
        let wanted: BTreeSet<String> = ["Sandbox".to_string(), "Docs".to_string(), "Shop".to_string()].into();
        let towers = towers_of(&known[..3], &wanted);
        assert_eq!(towers["Sandbox"], json!({ "name": "tower-sandbox", "status": "ready", "address": "t12" }));
        assert_eq!(towers["Docs"]["status"], "starting");
        assert_eq!(towers["Shop"], json!({ "name": "tower-shop", "status": "off", "address": null }), "due one, and it doesn't run (it died, or the main brain is down)");
        assert_eq!(towers.len(), 3, "the main brain is nobody's own tower");
    }

    #[test]
    fn each_mission_says_whether_it_has_its_own_tower() {
        let mut missions = json!({ "spaces": [{ "name": "Sandbox", "sessions": [] }, { "name": "docs", "sessions": [] }, { "name": "Shop" }], "quiet": [], "closed_today": [] });
        let towers: BTreeMap<String, Value> = [("Sandbox".to_string(), json!({ "name": "tower-sandbox", "status": "working", "address": "t12" })), ("Docs".to_string(), json!({ "name": "tower-docs", "status": "off", "address": null }))].into();
        mark_missions(&mut missions, &towers);
        assert_eq!(missions["spaces"][0]["tower"], json!({ "name": "tower-sandbox", "status": "working", "address": "t12" }));
        assert_eq!(missions["spaces"][1]["tower"]["name"], "tower-docs", "by the space's name, in any case");
        assert!(missions["spaces"][2]["tower"].is_null(), "one brain for everything, or a space that isn't due one");
        // Its brain is gone again: the mission says so.
        mark_missions(&mut missions, &BTreeMap::new());
        assert!(missions["spaces"][0]["tower"].is_null());
        // The brain itself is never one of a mission's sessions.
        assert!(crate::missions::seen_of(&json!({ "address": "t12", "name": "tower-sandbox", "state": "live", "status": "idle", "where": "Sandbox / tower-sandbox" }), None, None).is_none());
    }

    #[test]
    fn without_the_daemons_brains_everything_is_the_main_brains() {
        // In a command, and in tests: nothing is known, so nothing is routed away.
        assert!(get().is_none());
        assert_eq!(target(Some("Sandbox")), Target::main());
        assert!(towers().is_empty() && !is_brain_address("t12") && name_at("t12").is_none() && space_of("tower-sandbox").is_none());
        assert_eq!(in_space("sandbox", "what's left?"), "In sandbox: what's left?", "as the panel named a space before it had a tower");
    }
}
