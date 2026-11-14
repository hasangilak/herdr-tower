//! The missions in the daemon (see `crate::missions`): every 20 seconds, and
//! at once when a session starts or ends or a `tower space` command changed
//! something, it looks at Herdr's sessions, moves each space's mission
//! (`missions::step`, kept in `spaces.json`), does what a move asks for, and
//! publishes the missions in the state (`missions`).
//!
//! What a move asks for:
//! - quiet: what was open for the user there is over (not its merges), and
//!   a line goes into the record;
//! - archived, closed: its merges are over too;
//! - ready: the brain is asked once for a closing report (a `[space]` note,
//!   through the radar's outbox, so it waits for a brain that's away);
//! - ready and left alone for `[spaces] auto_close_hours`: closed, as
//!   `tower space close` does it.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::state::{Shared, lock};
use crate::history::{self, Entry};
use crate::missions::{self, Effect, Sources, State};
use crate::spaces;

#[derive(Clone)]
pub struct Missions {
    shared: Shared,
    decisions: super::decisions::Decisions,
    agendas: super::agendas::Agendas,
    /// Hands a note to the brain (through the radar's outbox).
    tell: Arc<dyn Fn(crate::agenda::Note) + Send + Sync>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    /// One look at a time.
    turn: Arc<Mutex<()>>,
    /// Each session's `hc get` (its labels), read again after `FULL_MS`.
    full: Arc<Mutex<HashMap<String, (u64, Value)>>>,
    /// The sessions as last seen: shown while Herdr can't be asked.
    last: Arc<Mutex<Vec<missions::Seen>>>,
    /// When Herdr last answered a look (None: not yet in this run).
    looked: Arc<Mutex<Option<u64>>>,
    /// Each space's active time, from the history's status lines (`crate::active`).
    active: Arc<Mutex<crate::active::Tracker>>,
    /// Each space's and session's brief, its versions (`crate::briefs`).
    briefs: Arc<Mutex<crate::briefs::History>>,
}

/// A session's labels are read again this long after they were read.
const FULL_MS: u64 = 30_000;

static RUNNING: OnceLock<Missions> = OnceLock::new();

/// Something changed (a session started or ended): look now.
pub fn kick() {
    if let Some(m) = RUNNING.get() {
        let (lock, cv) = &*m.wake;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }
}

/// A line for the record about a space.
fn record(space: &str, status: &str, text: String) {
    // Tests never write the history.
    if cfg!(test) {
        return;
    }
    let entry = Entry { kind: "space".into(), space: Some(space.into()), status: Some(status.into()), text, record: true, ..Default::default() };
    history::later(move || history::append(entry));
}

impl Missions {
    pub fn new(shared: Shared, decisions: super::decisions::Decisions, agendas: super::agendas::Agendas, tell: impl Fn(crate::agenda::Note) + Send + Sync + 'static) -> Missions {
        Missions {
            shared,
            decisions,
            agendas,
            tell: Arc::new(tell),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
            turn: Arc::default(),
            full: Arc::default(),
            last: Arc::default(),
            looked: Arc::default(),
            // (Tests never read the user's history.)
            active: Arc::new(Mutex::new(crate::active::Tracker::at(if cfg!(test) { std::path::PathBuf::from("/nonexistent/history.jsonl") } else { crate::history::path() }))),
            briefs: Arc::new(Mutex::new(if cfg!(test) { crate::briefs::History::default() } else { crate::briefs::History::load(crate::config::state_dir().join("briefs.json")) })),
        }
    }

    pub fn start(&self) {
        let _ = RUNNING.set(self.clone());
        let me = self.clone();
        thread::Builder::new()
            .name("missions".into())
            .spawn(move || {
                // The radar's first look, and the decisions the last run left, come first.
                thread::sleep(Duration::from_secs(10));
                loop {
                    me.round();
                    let (m, cv) = &*me.wake;
                    let woken = m.lock().unwrap_or_else(|e| e.into_inner());
                    let mut woken = cv.wait_timeout_while(woken, Duration::from_secs(20), |w| !*w).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
                    *woken = false;
                }
            })
            .ok();
    }

    /// `tower missions`: the missions as they are now. It looks first,
    /// unless a look is under way (then: as the state has them).
    pub fn now(&self) -> Value {
        if let Ok(_turn) = self.turn.try_lock() {
            return self.look();
        }
        self.shared.with(|s| s.missions.clone())
    }

    fn round(&self) {
        let _turn = lock(&self.turn);
        self.look();
    }

    /// One look at Herdr's sessions, then the missions follow.
    fn look(&self) -> Value {
        let now = history::now_ms();
        // (A file that doesn't read keeps the hours it last had: never 24 for a 0.)
        let auto_close_hours = crate::config::Config::live().spaces.auto_close_hours;
        let get = |address: &str| {
            if let Some((at, t)) = lock(&self.full).get(address)
                && now.saturating_sub(*at) < FULL_MS
            {
                return Some(t.clone());
            }
            let t = crate::hc::get(address)?;
            lock(&self.full).insert(address.to_string(), (now, t.clone()));
            Some(t)
        };
        let sessions = missions::scan(&get, &|address| super::sessions::registry().get(address).map(|s| s.said).filter(|s| !s.is_empty()));
        // Terminals that are gone are forgotten.
        if let Some(list) = &sessions {
            lock(&self.full).retain(|a, _| list.iter().any(|x| &x.address == a));
        }
        let seen = sessions.clone();
        // What is open, kept in case a restart closes it, and offered back when one did (`crate::reopen`).
        if let (Some(r), Some(list)) = (super::reopen::get(), &seen) {
            r.look(list, &get);
        }
        let view = self.settle(sessions, &crate::config::ui_dir(), now, auto_close_hours);
        // The spaces' own brains follow: started with a space's first session, stopped with its last.
        // (Not while Herdr can't be asked: nothing is known of who is there.)
        match (super::brains::get(), seen) {
            (Some(brains), Some(list)) => {
                brains.round(&list, &spaces::load_from(&crate::config::ui_dir()), &view);
                self.shared.with(|s| s.missions.clone())
            }
            _ => view,
        }
    }

    /// The missions after one look (`sessions`: every live session in a
    /// space; None: Herdr couldn't be asked, so the missions keep the
    /// sessions they had and nothing moves): move them, act on what moved,
    /// publish them.
    fn settle(&self, sessions: Option<Vec<missions::Seen>>, ui: &Path, now: u64, auto_close_hours: u64) -> Value {
        let asked = sessions.is_some();
        let sessions = match sessions {
            Some(list) => {
                *lock(&self.last) = list.clone();
                list
            }
            None => lock(&self.last).clone(),
        };
        let decisions = self.shared.with(|s| s.decisions.clone());
        let threads: BTreeMap<String, usize> = self.shared.with(|s| s.threads.iter().map(|(k, v)| (k.clone(), v.len())).collect());
        let ledger = self.agendas.ledger();
        let towers = super::brains::towers();
        // What the history added since the last look (status changes), and each space's active time from it.
        let tracker = {
            let mut t = lock(&self.active);
            t.read();
            t
        };
        let working: HashMap<String, bool> = sessions.iter().map(|x| (x.address.clone(), x.status == "working")).collect();
        let jobs: Vec<(String, u64)> =
            sessions.iter().flat_map(|x| super::running::jobs_of(&x.address).into_iter().map(|j| (x.address.clone(), j.since_ms)).collect::<Vec<_>>()).collect();
        let day = missions::day_start(now);
        let active_of = |space: &str, opened: u64| tracker.of(space, opened, day, now, &|a: &str| working.get(a).copied(), &jobs);
        let src = Sources { sessions: Some(&sessions), decisions: &decisions, ledger: &ledger, threads: &threads, towers: &towers, now, auto_close_hours, active: Some(&active_of) };
        // The first look of this run, or the first in a while (the Mac slept, Herdr
        // couldn't be asked, the clock moved): what wasn't watched isn't a space left alone.
        let away = asked && missions::was_away(lock(&self.looked).replace(now), now);
        let effects = match asked {
            true => spaces::update_in(ui, |s| {
                if away {
                    missions::after_away(s, now, auto_close_hours);
                }
                let facts = src.facts(s);
                missions::step(s, &facts, now, auto_close_hours)
            }),
            false => vec![],
        };
        for e in &effects {
            self.act(e, ui, now);
        }
        // What the tower gave each session, with what its hooks say.
        for x in sessions.iter().filter(|x| x.worktree.is_some() || x.port.is_some()) {
            super::sessions::registry().place(&x.address, x.worktree.as_deref(), x.branch.as_deref(), x.port);
        }
        let s = spaces::load_from(ui);
        // A closed or archived space keeps nothing in front of the user.
        for (name, _) in s.missions.iter().filter(|(_, m)| m.state == State::Closed || m.archived_ms.is_some()) {
            self.decisions.space_over(name, true);
        }
        // As it stands after all that.
        let decisions = self.shared.with(|s| s.decisions.clone());
        let mut view = missions::view(&s, &Sources { decisions: &decisions, ..src });
        // Where each space and session is at, in a few sentences.
        self.brief(&mut view, &ledger, &decisions, now);
        self.shared.update(|st| st.missions = view.clone());
        view
    }

    /// Each space's and each of its sessions' brief (`crate::briefs`), into
    /// the missions as they are published: composed from the agenda, the
    /// decisions, the PRs and the release track, a version kept when what
    /// one says changes.
    fn brief(&self, view: &mut Value, ledger: &crate::agenda::Ledger, decisions: &[Value], now: u64) {
        use crate::briefs;
        let tasks: Vec<&crate::agenda::Task> = ledger.agendas.iter().flat_map(|a| a.tasks.iter()).collect();
        let prs = crate::prs::daemon().map(|p| p.list()).unwrap_or_default();
        let releases = self.shared.with(|s| s.releases.clone());
        let progress: HashMap<String, String> = progress_lines(&self.shared);
        let mut hist = lock(&self.briefs);
        let mut keys = std::collections::HashSet::new();
        for m in view["spaces"].as_array_mut().into_iter().flatten() {
            let name = m["name"].as_str().unwrap_or("").to_string();
            let in_space = |t: &&crate::agenda::Task| t.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&name));
            let waits_on = |addr: &str| -> Vec<String> {
                decisions
                    .iter()
                    .filter(|d| d["sessions"].as_array().is_some_and(|ss| ss.iter().any(|s| s["address"] == addr)))
                    .filter_map(|d| d["title"].as_str().map(String::from))
                    .collect()
            };
            let mut session_briefs: Vec<(String, briefs::SessionBrief, bool)> = vec![];
            for x in m["sessions"].as_array_mut().into_iter().flatten() {
                let addr = x["address"].as_str().unwrap_or("").to_string();
                let sname = x["name"].as_str().unwrap_or("").to_string();
                let status = x["status"].as_str().unwrap_or("").to_string();
                let mut b = briefs::session(&briefs::SessionFacts {
                    status: &status,
                    said: x["said"].as_str(),
                    tasks: tasks.iter().copied().filter(|t| t.session == addr).collect(),
                    waits_on: waits_on(&addr),
                    progress: progress.get(&addr).map(String::as_str),
                    now,
                });
                let key = format!("session:{addr}");
                (b.history, b.changed_ms) = hist.note(&key, &briefs::session_stable(&b), &b.text, now);
                keys.insert(key);
                x["brief"] = serde_json::to_value(&b).unwrap_or_default();
                session_briefs.push((sname, b, status == "working"));
            }
            let pr_risks: Vec<String> = prs
                .iter()
                .filter(|p| p.open() && !p.draft && p.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&name)))
                .filter_map(|p| {
                    let n = p.number;
                    match () {
                        _ if crate::fixes::line(p).is_some() => crate::fixes::line(p).map(|l| format!("PR #{n}: {}", l.trim_end_matches('.'))),
                        _ if p.mergeable == "conflicting" => Some(format!("PR #{n} has conflicts with {}", p.base)),
                        _ if p.checks.state == "failing" => Some(format!("PR #{n}'s checks fail")),
                        _ if p.impact.as_ref().is_some_and(|i| i.breaks()) => Some(format!("PR #{n} breaks what others use")),
                        _ => None,
                    }
                })
                .collect();
            let space_waits: Vec<String> = decisions
                .iter()
                .filter(|d| d["space"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(&name)))
                .filter_map(|d| d["title"].as_str().map(String::from))
                .collect();
            // Its release track in a line ("prod 14:20 · 1 on dev"), as the state has it.
            let release = releases
                .as_object()
                .and_then(|all| all.iter().find(|(k, _)| k.eq_ignore_ascii_case(&name)).map(|(_, v)| v.clone()))
                .and_then(|v| serde_json::from_value::<crate::releases::Track>(v).ok())
                .filter(|t| !t.repos.is_empty())
                .map(|t| t.summary(now))
                .filter(|s| !s.is_empty());
            let mut b = briefs::space(&briefs::SpaceFacts {
                goal: m["goal"].as_str().unwrap_or(""),
                state: m["state"].as_str().unwrap_or("open"),
                standing: m["standing"].as_bool().unwrap_or(false),
                sessions: session_briefs.iter().map(|(n, b, w)| (n.clone(), b, *w)).collect(),
                tasks: tasks.iter().copied().filter(in_space).collect(),
                waits: space_waits,
                pr_risks,
                release,
                now,
            });
            let key = format!("space:{name}");
            (b.history, b.changed_ms) = hist.note(&key, &briefs::space_stable(&b), &b.text, now);
            keys.insert(key);
            m["brief"] = serde_json::to_value(&b).unwrap_or_default();
        }
        hist.keep_only(&keys);
        hist.save();
    }

    fn act(&self, effect: &Effect, ui: &Path, now: u64) {
        match effect {
            Effect::Quiet(space) => {
                let closed = self.decisions.space_over(space, false);
                eprintln!("[missions] {space} is quiet: no session left ({closed} decision(s) over)");
                record(space, "quiet", format!("{space} is quiet: no session left there."));
            }
            Effect::Archived(space) => {
                self.decisions.space_over(space, true);
                eprintln!("[missions] {space} was quiet for a day: archived");
            }
            Effect::Reopened(space) => eprintln!("[missions] {space} is open again"),
            Effect::Ready(space) => {
                eprintln!("[missions] {space} is ready to close: every request there is finished");
                record(space, "ready", format!("{space} is ready to close: every request there is finished."));
                // Asked once each time it becomes ready.
                let note = spaces::update_in(ui, |s| {
                    let goal = s.agendas.get(space).cloned().unwrap_or_default();
                    let m = s.missions.get_mut(space)?;
                    if m.report_asked_ms > 0 || !m.report.is_empty() {
                        return None;
                    }
                    m.report_asked_ms = now;
                    Some(missions::report_note(space, &goal, &m.done_when))
                });
                if let Some(msg) = note {
                    (self.tell)(crate::agenda::Note { space: Some(space.clone()), msg, ..Default::default() });
                }
            }
            Effect::AutoClose(space) => match missions::close_named(space, false, false) {
                Ok(v) => {
                    eprintln!("[missions] {space} was ready and left alone: closed ({v})");
                    self.decisions.space_over(space, true);
                }
                Err(e) => {
                    eprintln!("[missions] {space} wasn't closed by itself: {e:#}");
                    // Not again at once: after its hours, or with the day's archive.
                    spaces::update_in(ui, |s| {
                        if let Some(m) = s.missions.get_mut(space) {
                            match m.state {
                                State::Quiet => m.archived_ms = Some(now),
                                _ => m.idle_since_ms = Some(now),
                            }
                        }
                    });
                }
            },
        }
    }
}

/// Each working session's live progress line, by address, when the tower
/// has one (the state's agents' `progress`: step N of M, time in, last
/// update), for its brief's "now".
fn progress_lines(shared: &Shared) -> HashMap<String, String> {
    let agents = serde_json::to_value(shared.with(|s| s.agents.clone())).unwrap_or_default();
    agents
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let line = a["progress"]["line"].as_str().or_else(|| a["progress"].as_str())?.trim();
            (!line.is_empty()).then(|| (a["address"].as_str().unwrap_or("").to_string(), line.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agenda::{Ledger, Reg, Tag, TaskEv};
    use crate::decisions::Raise;
    use crate::missions::Seen;

    const HOUR: u64 = 3_600_000;

    fn seen(space: &str, address: &str, status: &str) -> Seen {
        Seen { space: space.into(), address: address.into(), name: address.into(), status: status.into(), ..Default::default() }
    }

    fn pr(n: u64, session: &str, space: &str) -> crate::prs::Pr {
        crate::prs::Pr {
            repo: "o/app".into(),
            number: n,
            base: "dev".into(),
            state: "open".into(),
            mergeable: "mergeable".into(),
            session: session.into(),
            space: Some(space.into()),
            waiting: true,
            found_ms: 1_000,
            ..Default::default()
        }
    }

    /// The daemon's missions over a throwaway state folder, and the notes they hand the brain.
    fn missions(name: &str, ledger: &Ledger) -> (Missions, Shared, std::path::PathBuf, Arc<Mutex<Vec<String>>>) {
        let dir = std::env::temp_dir().join(format!("tower-missions-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        ledger.save_to(&dir.join("agendas.json")).unwrap();
        let shared = Shared::new(super::super::state::State::new("ptt", "test", "test"));
        let decisions = super::super::decisions::Decisions::new(shared.clone());
        let told: Arc<Mutex<Vec<String>>> = Arc::default();
        let notes = told.clone();
        let m = Missions::new(shared.clone(), decisions, super::super::agendas::Agendas::at(dir.join("agendas.json")), move |n| lock(&notes).push(n.msg));
        (m, shared, dir.join("ui"), told)
    }

    #[test]
    fn a_space_going_quiet_clears_what_was_open_there_but_its_merge() {
        let (m, shared, ui, told) = missions("quiet", &Ledger::default());
        let now = 100 * HOUR;
        // Held news from the space, not read yet.
        spaces::update_in(&ui, |s| {
            s.read_ms.insert("Shop".into(), 5);
        });
        let raised = m.decisions.raise(Raise { detail: String::new(), space: Some("Shop".into()), who: None, why: "design".into(), text: "Tabs or a sidebar?".into(), options: vec![] }).0.id;
        m.decisions.sync_prs(&[pr(88, "t1", "Shop")]);
        let v = m.settle(Some(vec![seen("Shop", "t1", "working"), seen("Docs", "t7", "idle")]), &ui, now, 24);
        // First sight: both are missions, open; the one that needs the user first.
        assert_eq!(v["spaces"].as_array().unwrap().iter().map(|x| (x["name"].as_str().unwrap(), x["state"].as_str().unwrap(), x["needs"].as_u64().unwrap())).collect::<Vec<_>>(), [("Shop", "open", 2), ("Docs", "open", 0)]);
        assert_eq!(shared.get().missions, v, "the state carries them");
        assert_eq!(shared.json()["missions"]["spaces"][0]["sessions"][0]["address"], "t1");
        // Herdr can't be asked: nothing moves, and the missions keep their sessions.
        let blind = m.settle(None, &ui, now + 1_000, 24);
        // (The active figures carry when they were made: a second later.)
        let undated = |mut v: serde_json::Value| {
            for s in v["spaces"].as_array_mut().into_iter().flatten() {
                s.as_object_mut().map(|o| o.remove("active"));
            }
            v
        };
        assert_eq!(undated(blind), undated(v.clone()));
        // Shop's last session goes.
        let v = m.settle(Some(vec![seen("Docs", "t7", "idle")]), &ui, now + 60_000, 24);
        assert_eq!((v["quiet"].clone(), v["spaces"][1]["name"].as_str(), v["spaces"][1]["state"].as_str()), (serde_json::json!(["Shop"]), Some("Shop"), Some("quiet")));
        let s = spaces::load_from(&ui);
        assert_eq!((s.missions["Shop"].state, s.missions["Shop"].quiet_since_ms, s.read_ms["Shop"]), (State::Quiet, Some(now + 60_000), now + 60_000), "its held news is read");
        // What was raised there is over; the PR that waits is still the user's.
        assert_eq!(m.decisions.get(&raised).map(|d| d.state), Some(crate::decisions::State::Stopped));
        let open: Vec<String> = shared.get().decisions.iter().map(|d| d["kind"].as_str().unwrap().to_string()).collect();
        assert_eq!(open, ["merge"]);
        assert_eq!(v["spaces"][1]["needs"], 1);
        // A day quiet: archived, out of the missions, and its merge goes too.
        let v = m.settle(Some(vec![seen("Docs", "t7", "idle")]), &ui, now + 60_000 + missions::ARCHIVE_MS, 24);
        assert_eq!(v["spaces"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect::<Vec<_>>(), ["Docs"]);
        assert_eq!(v["quiet"], serde_json::json!([]));
        assert!(shared.get().decisions.is_empty());
        assert!(lock(&told).is_empty(), "nothing of this is told to the brain");
        let _ = std::fs::remove_dir_all(ui.parent().unwrap());
    }

    #[test]
    fn a_ready_space_asks_the_brain_once_for_its_closing_report() {
        let now = 100 * HOUR;
        // One request about Shop, handed out after the mission began, its only task verified.
        let mut l = Ledger::default();
        let a = l.open("Ship the checkout", "", "user", 0, &["Shop".to_string()], now);
        let tag = Tag { agenda: Some(a), space: Some("Shop".into()), ..Default::default() };
        let task = l.register(&Reg { queue_id: 1, session: "t1".into(), name: "web".into(), text: "ship it".into(), tag }, &Default::default(), now).unwrap().task;
        let (m, shared, ui, told) = missions("ready", &l);
        spaces::update_in(&ui, |s| {
            s.agendas.insert("Shop".into(), "Ship the checkout".into());
            s.missions.insert("Shop".into(), missions::Mission { done_when: "it is live".into(), ..missions::Mission::born(now - HOUR) });
        });
        // Its task still runs: open.
        let v = m.settle(Some(vec![seen("Shop", "t1", "working")]), &ui, now, 24);
        assert_eq!((v["spaces"][0]["state"].as_str(), v["spaces"][0]["requests"][0]["done"].as_u64(), v["spaces"][0]["requests"][0]["total"].as_u64()), (Some("open"), Some(0), Some(1)));
        assert!(lock(&told).is_empty());
        // Verified: every request is finished and nothing waits for the user.
        l.apply(&task, TaskEv::Verify { how: "checked".into() }, now + 60_000).unwrap();
        l.save_to(&ui.parent().unwrap().join("agendas.json")).unwrap();
        let m = Missions { agendas: super::super::agendas::Agendas::at(ui.parent().unwrap().join("agendas.json")), ..m };
        let v = m.settle(Some(vec![seen("Shop", "t1", "idle")]), &ui, now + 120_000, 24);
        assert_eq!((v["spaces"][0]["state"].as_str(), v["spaces"][0]["ready_since_ms"].as_u64(), v["spaces"][0]["auto_close_ms"].as_u64()), (Some("ready"), Some(now + 120_000), Some(now + 120_000 + 24 * HOUR)));
        let notes = lock(&told).clone();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("[space] \"Shop\" looks finished") && notes[0].contains("tower space report \"Shop\"") && notes[0].contains("Done when: \"it is live\""), "{}", notes[0]);
        // Looked at again: asked once.
        m.settle(Some(vec![seen("Shop", "t1", "idle")]), &ui, now + 180_000, 24);
        assert_eq!(lock(&told).len(), 1);
        // The report is written (`tower space report`): the mission shows it.
        spaces::update_in(&ui, |s| s.missions.get_mut("Shop").unwrap().report = "The checkout is live.".into());
        let v = m.settle(Some(vec![seen("Shop", "t1", "idle")]), &ui, now + 240_000, 24);
        assert_eq!(v["spaces"][0]["report"], "The checkout is live.");
        // A decision there: open again, the report dropped; settled again: ready, and asked again.
        let d = m.decisions.raise(Raise { detail: String::new(), space: Some("Shop".into()), who: None, why: "deploy".into(), text: "Deploy it?".into(), options: vec![] }).0.id;
        let v = m.settle(Some(vec![seen("Shop", "t1", "idle")]), &ui, now + 300_000, 24);
        assert_eq!((v["spaces"][0]["state"].as_str(), v["spaces"][0]["report"].as_str(), v["spaces"][0]["needs"].as_u64()), (Some("open"), Some(""), Some(1)));
        m.decisions.answered(&d, "yes");
        let v = m.settle(Some(vec![seen("Shop", "t1", "idle")]), &ui, now + 360_000, 24);
        assert_eq!(v["spaces"][0]["state"], "ready");
        assert_eq!(lock(&told).len(), 2);
        // Closed (`tower space close`): out of the missions, in today's closed ones.
        spaces::update_in(&ui, |s| {
            let x = s.missions.get_mut("Shop").unwrap();
            (x.state, x.closed_ms, x.archived_ms) = (State::Closed, Some(now + 400_000), Some(now + 400_000));
        });
        m.decisions.sync_prs(&[pr(90, "t1", "Shop")]);
        let v = m.settle(Some(vec![]), &ui, now + 420_000, 24);
        assert_eq!((v["spaces"].as_array().map(Vec::len), v["closed_today"].clone()), (Some(0), serde_json::json!(["Shop"])));
        assert!(shared.get().decisions.is_empty(), "a closed space keeps nothing in front of the user, its merge neither");
        let _ = std::fs::remove_dir_all(ui.parent().unwrap());
    }
}
