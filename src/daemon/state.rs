//! What the tower is doing, shared by the daemon's threads and published to
//! `~/.local/state/herder-tower/ui/state.json` for the Herdr mod.

use std::sync::{Arc, Mutex};
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

#[derive(Debug, Clone, Serialize)]
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
    /// The paired phone that is the tower's voice now (the Mac is silent), or null.
    pub phone: Option<String>,
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
            agents: vec![],
            error: String::new(),
            talk_key: "off".into(),
            voice: voice.into(),
            stt_model: stt_model.into(),
            lan: String::new(),
            phone: None,
            screenshots: vec![],
            questions: vec![],
            question_policy: "normal".into(),
            behaviors: serde_json::Value::Null,
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

#[derive(Clone)]
pub struct Shared(Arc<Mutex<State>>);

impl Shared {
    pub fn new(state: State) -> Self {
        let s = Self(Arc::new(Mutex::new(state)));
        s.publish();
        s
    }

    pub fn get(&self) -> State {
        self.0.lock().unwrap().clone()
    }

    /// Changes the state; publishes it when something changed.
    pub fn update(&self, f: impl FnOnce(&mut State)) {
        let changed = {
            let mut s = self.0.lock().unwrap();
            let before = serde_json::to_string(&*s).unwrap_or_default();
            f(&mut s);
            let after = serde_json::to_string(&*s).unwrap_or_default();
            if before != after {
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
        let mut v = serde_json::to_value(&s).unwrap_or_default();
        v["phase"] = serde_json::Value::String(s.phase().into());
        v
    }

    fn publish(&self) {
        // Tests never touch the live state.json.
        if cfg!(test) {
            return;
        }
        let v = self.json();
        let dir = config::ui_dir();
        let _ = std::fs::create_dir_all(&dir);
        // Written beside the watched folder, then moved in: the mod wakes once.
        let tmp = config::state_dir().join(".state.json.tmp");
        let path = dir.join("state.json");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}
