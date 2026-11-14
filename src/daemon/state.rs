//! What the tower is doing, shared by the daemon's threads and published to
//! `~/.local/state/herder-tower/ui/state.json` for the Herdr mod.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Alert {
    pub at_ms: u64,
    pub terminal: String,
    pub text: String,
    pub level: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct State {
    pub version: u64,
    pub updated_ms: u64,
    pub pid: u32,
    /// False once the daemon has quit.
    pub running: bool,
    /// `ptt`, `handsfree` or `off`.
    pub mode: String,
    /// The microphone: `closed`, `waiting` (hands-free, nobody talking),
    /// `hearing` (speech in progress), `recording` (a press-to-talk take).
    pub mic: String,
    /// Hands-free heard the wake word alone and waits for the request.
    pub armed: bool,
    pub transcribing: bool,
    pub speaking: bool,
    /// The tower is muted (`tower mute`): it says nothing, here or on a phone.
    pub muted: bool,
    /// What it says out loud (`tower tell --voice`): `off` (the mute),
    /// `decisions` (a decision, once, and nothing else) or `replies` (its
    /// answers too).
    pub tell_voice: String,
    /// Everything said plays on this Mac (`tower mac-voice`), never on a phone.
    pub voice_on_mac: bool,
    /// What is being said is an alert, not a reply to the user.
    pub announcing: bool,
    /// The brain terminal's status (`idle`, `working`, `blocked`, ...) or `absent`.
    pub brain: String,
    pub brain_app: String,
    /// What you are saying, while you say it: the take so far, transcribed
    /// every second or so (press-to-talk only). Cleared when the take is
    /// done; the final text then lands in `heard`.
    pub hearing: String,
    /// What the tower last heard you say.
    pub heard: String,
    pub heard_ms: u64,
    /// A take dictated into the panel's draft (see `draft`): its final text,
    /// for the panel to keep in the input, not sent.
    pub dictated: String,
    pub dictated_ms: u64,
    /// What it is saying, or last said.
    pub said: String,
    pub said_ms: u64,
    pub alerts: Vec<Alert>,
    /// Alerts that came in while muted, unsaid (newest last, at most
    /// `MISSED_MAX`). Cleared when the tower is unmuted.
    pub missed_while_muted: Vec<String>,
    /// Agents on every machine: (address, spoken name, status).
    pub agents: Vec<AgentLine>,
    pub error: String,
    /// The talk key: `ctrl`, `left_ctrl`, `right_ctrl`, `off`, or
    /// `needs-permission` (Input Monitoring for Herdr Core).
    pub talk_key: String,
    pub voice: String,
    pub stt_model: String,
    /// The phone API's address (`https://192.168.1.20:7843`), or "" when it's off.
    pub lan: String,
    /// Every address the phone API listens on, in the order a phone should
    /// try them: `[{"kind": "lan"|"tailscale", "host", "port"}]` (empty when off).
    pub lan_hosts: Vec<serde_json::Value>,
    /// Tailscale on this Mac: `{installed, running, state, ip, dns_name}`
    /// (null until the phone API has looked).
    pub tailscale: Option<serde_json::Value>,
    /// The paired phone that is the tower's voice now (the Mac is silent), or null.
    pub phone: Option<String>,
    /// The network the phone API's LAN listener is on: `{"range":
    /// "10.10.30", "name": "Office" | null, "lan": {"host", "port"} | null}`
    /// (null while the API is off, or without a LAN address).
    pub network: Option<serde_json::Value>,
    /// Said once in both windows when the phones in use can't reach the
    /// tower on this network (`lan::network::hint`), else null.
    pub phone_hint: Option<String>,
    /// The latest screenshots sessions sent (`tower screenshot`), newest
    /// first, at most 30: `{id, at, session, name, space, account, job, caption, path, url}`.
    pub screenshots: Vec<serde_json::Value>,
    /// What blocked sessions ask, as question sets (see `questions`):
    /// `{id, session, name, kind, title, detail, context, tabs, current_tab,
    /// submit_state, recommendations, status, error, exact, at_ms}`.
    pub questions: Vec<serde_json::Value>,
    /// How questions are handled: `normal` (the 90/10 rule), `keep-moving`
    /// (the brain answers more itself), `ask-everything` (never for the user).
    pub question_policy: String,
    /// The user's behaviors: `{count, enabled, by_category, updated_ms}`.
    pub behaviors: serde_json::Value,
    /// How much the tower tells the user (`tower tell`): `decisions`,
    /// `results` or `everything`.
    pub tell_level: String,
    /// The tower merges low-risk PRs into dev itself (`[prs]
    /// auto_merge_low_risk`, `tower automerge`): off unless the user turned it on.
    pub auto_merge: bool,
    /// What the sessions of each space wrote to each other (see `peers`):
    /// by space, the last 50 each, newest last:
    /// `{at, from, from_name, to, to_name, text, space, delivered}`.
    pub threads: std::collections::BTreeMap<String, Vec<serde_json::Value>>,
    /// What only the user can decide (see `decisions`), every open one in
    /// queue order (heaviest first): `{id, key, kind, space, sessions,
    /// waiting, title, why, options, pick, prs, question_id, task, first_ms,
    /// last_ms, count, state, muted, hold, back}`. The main queue is those
    /// whose space isn't muted (`muted: false`). Those on hold are not here.
    pub decisions: Vec<serde_json::Value>,
    /// The open decisions outside muted spaces: the orb's number (none on hold).
    pub decisions_count: usize,
    /// The open decisions the user put on hold (`decisions::Hold`), as
    /// `decisions` has the others, newest hold first: still theirs, listed
    /// apart ("On hold"), not counted and never told.
    #[serde(default)]
    pub decisions_held: Vec<serde_json::Value>,
    /// "Merge all N" chains the tower is merging, and those that ended
    /// lately (`crate::merges::Chain`: `{id, card, space, by, links: [{key,
    /// number, base, step: {step, why}}], started_ms, stopped}`, with `over`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merges: Vec<serde_json::Value>,
    /// Every space as a mission (see `crate::missions`): `{"spaces":
    /// [Mission], "quiet": [name], "closed_today": [name]}`, the open ones
    /// first (those that need the user, then those at work, then the rest;
    /// then by name), then the quiet ones. `Mission`: `{name, goal,
    /// done_when, standing, state, follow, mute, needs, report, sessions,
    /// requests, thread_count, quiet_since_ms, ready_since_ms, auto_close_ms}`.
    pub missions: serde_json::Value,
    /// The brains that run (see `crate::brains`), the main one first:
    /// `{name, space, address, status, model, effort}` (`space` null for
    /// the main one; `model` and `effort` as its command line says, null
    /// when it names none).
    pub brains: Vec<serde_json::Value>,
    /// The brains' model (`tower model`): `{program, model, effort,
    /// choices: {models: [{id, label}], efforts}, running: [{brain, space,
    /// address, model, effort, restart}], restart_needed, locked_by_args,
    /// locked: {model, effort}, says}`.
    pub brain_model: serde_json::Value,
    /// Each space's release track (`crate::releases`, `daemon::releases`):
    /// by space, `{space, repos: [{repo, dev, prod, changes, releases,
    /// deployed, lines, on_prod, on_dev_not_prod}], summary, checked_ms}`.
    pub releases: serde_json::Value,
    /// What the tower opened for the user lately (see `crate::acts`), newest
    /// first, at most 10: `{id, kind, at_ms, by, space, line, sessions,
    /// assumed: [{what, value, why}], request: {at, text} or null, state:
    /// "done" | "undone", undo: {possible, why}, closing}`.
    pub acts: Vec<serde_json::Value>,
    /// The plan that is open (see `crate::plans`, `Plan::view`), or null:
    /// `{id, form: "page" | "whiteboard", status: "writing" | "ready" |
    /// "failed", ticket, draft, revision, address, planner: "starting" |
    /// "working" | "idle" | "blocked" | "gone" | …, blocked, answering,
    /// dir, plan_md, url, why, thread: [{from: "user" | "planner", text,
    /// at}], created_ms, ready_ms, updated_ms, closed_ms, launched}`.
    pub plan: Option<serde_json::Value>,
    /// The plans launched and live (`Plan::view_with`), newest first: each
    /// with `launched` (its space), `version`, `stages` (each with `state`),
    /// `progress: {stages, done, at}`, `findings` and `revisions`.
    pub plans_live: Vec<serde_json::Value>,
    /// The PRs' reviews (see `crate::reviews`): every one not over, and
    /// those over in the last day, the latest first, as `Review::view`
    /// writes them (`{key, repo, number, url, space, owner, owner_name,
    /// reviewer, reviewer_name, status, rounds, critical, line, chip, …}`).
    pub reviews: Vec<serde_json::Value>,
    /// Each Claude account's usage and what the tower found of its limit
    /// (`crate::usage::Board`): `{list: [{name, email, default, five_hour:
    /// {used, resets_ms} | null, seven_day, out_until_ms, out_why}],
    /// starts_on, moves: [failover::Moved], moving: [address], waiting:
    /// [{address, account, since_ms, why}]}`; null until the first look.
    /// Who runs on which account is in `agents`, the missions' sessions and
    /// `brains` (each with its `account`).
    pub accounts: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentLine {
    pub address: String,
    pub name: String,
    pub status: String,
    pub app: String,
    /// The Claude account (a name from the claude-accounts mod, `cca`), or
    /// null: Codex, another machine, or not known.
    pub account: Option<String>,
    /// The id of its open question set, when it is blocked on one.
    pub question: Option<String>,
    /// The folder it works in.
    #[serde(default)]
    pub folder: String,
    /// Instructions waiting for it in the send queue.
    #[serde(default)]
    pub queued: usize,
    /// What waits for it in the send queue, in order: the tower's messages,
    /// its notes, messages from other sessions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<WaitingLine>,
    /// Jobs it left running in the background, or declared (`crate::running`):
    /// its turn may be over, its work isn't.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<crate::running::Job>,
    /// Servers it left running (a dev server): not jobs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<crate::running::Server>,
    /// Browsers it uses now (`crate::browsers`): Chrome, the built-in one, a
    /// headless one; each with its site, since when and its line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub browsers: Vec<crate::browsers::Use>,
    /// How its turn goes, while it works (`crate::progress`): step N of M,
    /// the call at work, time in, last update, in `line`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub progress: Option<crate::progress::Progress>,
}

/// One message waiting for a session in the send queue.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Default)]
pub struct WaitingLine {
    pub id: u64,
    pub text: String,
    /// From another session (its name): no message of the tower's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// A note (an introduction), not work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub note: bool,
    /// Parked: it won't go on its own (why).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<String>,
    /// Held while what the user typed in that session is checked against it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checking: bool,
}

impl State {
    pub fn new(mode: &str, voice: &str, stt_model: &str) -> Self {
        Self {
            version: 0,
            updated_ms: now_ms(),
            pid: std::process::id(),
            running: true,
            mode: mode.into(),
            mic: "closed".into(),
            armed: false,
            transcribing: false,
            speaking: false,
            muted: false,
            tell_voice: crate::tell::Voice::default().word().into(),
            voice_on_mac: false,
            announcing: false,
            brain: "absent".into(),
            brain_app: String::new(),
            hearing: String::new(),
            heard: String::new(),
            heard_ms: 0,
            dictated: String::new(),
            dictated_ms: 0,
            said: String::new(),
            said_ms: 0,
            alerts: vec![],
            missed_while_muted: vec![],
            agents: vec![],
            error: String::new(),
            talk_key: "off".into(),
            voice: voice.into(),
            stt_model: stt_model.into(),
            lan: String::new(),
            lan_hosts: vec![],
            tailscale: None,
            phone: None,
            network: None,
            phone_hint: None,
            screenshots: vec![],
            questions: vec![],
            question_policy: "normal".into(),
            behaviors: serde_json::Value::Null,
            tell_level: crate::tell::Level::default().word().into(),
            auto_merge: false,
            threads: Default::default(),
            decisions: vec![],
            decisions_count: 0,
            decisions_held: vec![],
            merges: vec![],
            missions: serde_json::json!({ "spaces": [], "quiet": [], "closed_today": [] }),
            brains: vec![],
            brain_model: serde_json::Value::Null,
            releases: serde_json::json!({}),
            acts: vec![],
            plan: None,
            plans_live: vec![],
            reviews: vec![],
            accounts: serde_json::Value::Null,
        }
    }

    /// One word for the orb: what it is doing right now, most important first.
    pub fn phase(&self) -> &'static str {
        if self.speaking && self.announcing {
            "announcing"
        } else if self.speaking {
            "speaking"
        } else if self.transcribing {
            "transcribing"
        } else if self.mic == "recording" || self.mic == "hearing" {
            "listening"
        } else if self.brain == "working" {
            "thinking"
        } else if self.brain == "blocked" {
            "blocked"
        } else if self.armed {
            "armed"
        } else if self.mode == "handsfree" && self.mic == "waiting" {
            "standby"
        } else {
            "idle"
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// How many unsaid alerts are kept while muted.
pub const MISSED_MAX: usize = 20;

/// Locks a mutex, even one a panicking thread left poisoned: what it holds
/// is still the latest, and one thread's panic shouldn't stop the others.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
pub struct Shared {
    state: Arc<Mutex<State>>,
    /// The version last written to state.json: one writer at a time, and
    /// never an older snapshot over a newer one.
    written: Arc<Mutex<Option<u64>>>,
}

impl Shared {
    pub fn new(state: State) -> Self {
        let s = Self { state: Arc::new(Mutex::new(state)), written: Arc::new(Mutex::new(None)) };
        s.publish();
        s
    }

    pub fn get(&self) -> State {
        lock(&self.state).clone()
    }

    /// Reads a part of the state without copying all of it.
    pub fn with<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&lock(&self.state))
    }

    /// Whether the tower is talking (read often: cheap).
    pub fn speaking(&self) -> bool {
        self.with(|s| s.speaking)
    }

    /// Changes the state; publishes it when something changed.
    pub fn update(&self, f: impl FnOnce(&mut State)) {
        let changed = {
            let mut s = lock(&self.state);
            let before = s.clone();
            f(&mut s);
            if *s != before {
                s.version += 1;
                s.updated_ms = now_ms();
                true
            } else {
                false
            }
        };
        if changed {
            self.publish();
        }
    }

    /// News that is only written down (a finish at a quiet level, see
    /// `tell`): into the history, not into the alerts and not a chat message.
    pub fn record(&self, terminal: &str, text: &str, level: &str) {
        // Tests never write the user's history (as they never touch the live state.json).
        if cfg!(test) {
            return;
        }
        let (t, x, l) = (terminal.to_string(), text.to_string(), level.to_string());
        crate::history::later(move || crate::history::record_session_as("alert", &t, &x, Some(&l), true));
    }

    pub fn alert(&self, terminal: &str, text: &str, level: &str) {
        let alert = Alert { at_ms: now_ms(), terminal: terminal.into(), text: text.into(), level: level.into() };
        let (t, x, l) = (terminal.to_string(), text.to_string(), level.to_string());
        crate::history::later(move || crate::history::record_session("alert", &t, &x, Some(&l)));
        self.update(|s| {
            s.alerts.insert(0, alert);
            s.alerts.truncate(8);
        });
    }

    pub fn json(&self) -> serde_json::Value {
        let s = self.get();
        to_json(&s)
    }

    fn publish(&self) {
        // Tests never touch the live state.json.
        if cfg!(test) {
            return;
        }
        // One writer at a time, and the snapshot is taken in turn: the
        // newest always lands last.
        let mut written = lock(&self.written);
        let s = self.get();
        if written.is_some_and(|v| s.version <= v) {
            return;
        }
        let v = to_json(&s);
        let dir = config::ui_dir();
        let _ = std::fs::create_dir_all(&dir);
        // Written beside the watched folder, then moved in: the mod wakes once.
        // Its own name, so no other writer (another process) clobbers it.
        static N: AtomicU64 = AtomicU64::new(0);
        let tmp = config::state_dir().join(format!(".state.json.{}.{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let path = dir.join("state.json");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap_or_default()).is_ok() {
            if std::fs::rename(&tmp, &path).is_ok() {
                *written = Some(s.version);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// The state as JSON, with its phase.
fn to_json(s: &State) -> serde_json::Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    v["phase"] = serde_json::Value::String(s.phase().into());
    v
}

/// The behaviors as their files have them now (`behaviors::Store::summary`),
/// into the state when they differ from what it carries: the count follows
/// the files, not only the changes the tower was told of. True when it
/// changed.
pub fn follow_behaviors(shared: &Shared, now: serde_json::Value) -> bool {
    if shared.with(|s| s.behaviors == now) {
        return false;
    }
    shared.update(|s| s.behaviors = now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_that_changes_nothing_keeps_the_version() {
        let shared = Shared::new(State::new("ptt", "test", "test"));
        shared.update(|s| s.speaking = false);
        assert_eq!(shared.get().version, 0);
        shared.update(|s| s.speaking = true);
        assert_eq!(shared.get().version, 1);
        assert!(shared.speaking());
        shared.update(|s| s.behaviors = serde_json::json!({ "count": 1 }));
        shared.update(|s| s.behaviors = serde_json::json!({ "count": 1 }));
        assert_eq!(shared.get().version, 2);
    }

    #[test]
    fn the_behaviors_count_follows_their_files() {
        let dir = std::env::temp_dir().join(format!("tower-state-behaviors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = crate::behaviors::Store { dir: dir.clone() };
        // The daemon started before anything was seeded: none.
        let shared = Shared::new(State::new("ptt", "test", "test"));
        shared.update(|s| s.behaviors = store.summary());
        assert_eq!(shared.get().behaviors["count"], 0);
        // An install seeds them, and tells nobody: the next look has them.
        let seeded = store.seed().unwrap().len() as u64;
        assert!(seeded > 0);
        assert!(follow_behaviors(&shared, store.summary()));
        let b = shared.get().behaviors;
        assert_eq!((b["count"].as_u64(), b["enabled"].as_u64()), (Some(seeded), Some(seeded)));
        // Nothing moved since: the state isn't written again.
        let version = shared.get().version;
        assert!(!follow_behaviors(&shared, store.summary()));
        assert_eq!(shared.get().version, version);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_poisoned_lock_still_opens() {
        let shared = Shared::new(State::new("ptt", "test", "test"));
        let s2 = shared.clone();
        let _ = std::thread::spawn(move || s2.update(|_| panic!("a thread dies holding the state"))).join();
        shared.update(|s| s.said = "still here".into());
        assert_eq!(shared.get().said, "still here");
    }
}
