//! What every agent session tells the tower itself, through its hooks
//! (`tower hook agent <event>`, installed for Claude Code and Codex by
//! `tower hooks install`): which Herdr terminal it runs in, its session id
//! and transcript, the prompt it is working on, what it last said, and when
//! it started, stopped, asked for permission or ended.
//!
//! Herdr's own view (`hc events`) says *that* a terminal changed; the hooks
//! say *what* the agent did, at once, and tie the agent to its terminal for
//! good (`HC_TERMINAL_ID`, and a `tower.session` label on the terminal). So
//! the tower doesn't lose track of a session when an event stream drops, a
//! status reads late, or an address comes back for another terminal.
//!
//! Kept in `~/.local/state/herder-tower/sessions.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::config;

/// One hook call, as `tower hook agent` hands it over.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentEvent {
    /// `claude-code` or `codex`.
    pub app: String,
    /// `start`, `prompt`, `permission`, `notify`, `stop`, `stop-failed`, `end`.
    pub event: String,
    /// The Herdr terminal it runs in (`HC_TERMINAL_ID`), when in Herdr.
    #[serde(default)]
    pub terminal: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub transcript: String,
    #[serde(default)]
    pub cwd: String,
    /// The prompt (`prompt`), the last answer (`stop`), the notice (`notify`,
    /// `permission`), or why it ended (`end`).
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub at_ms: u64,
    /// The agent process that ran the hook (0: not known). Only the
    /// terminal's own agent (Herdr's foreground process) speaks for it: a
    /// `claude -p` run inside a session inherits its terminal, but isn't it.
    #[serde(default)]
    pub pid: u32,
}

/// Whether an event comes from the terminal's own agent: the hook's agent
/// is the terminal's foreground process (either not known: it is).
pub fn from_terminals_agent(ev: &AgentEvent, foreground_pid: Option<u64>) -> bool {
    match foreground_pid {
        Some(fg) if ev.pid != 0 => u64::from(ev.pid) == fg,
        _ => true,
    }
}

/// An agent session as its hooks describe it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentSession {
    pub terminal: String,
    pub app: String,
    pub session_id: String,
    #[serde(default)]
    pub transcript: String,
    #[serde(default)]
    pub cwd: String,
    /// `working`, `waiting` (on a permission or a question), `done`, `ended`.
    pub state: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub prompt_ms: u64,
    /// What it said at the end of its last turn.
    #[serde(default)]
    pub said: String,
    #[serde(default)]
    pub said_ms: u64,
    /// The last notice it raised (a permission it needs, waiting for input).
    #[serde(default)]
    pub notice: String,
    #[serde(default)]
    pub notice_ms: u64,
    pub event: String,
    pub event_ms: u64,
    #[serde(default)]
    pub started_ms: u64,
}

/// What an event changed, for the radar and the send queue.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A new session in this terminal (label it).
    Started,
    /// It took a prompt.
    Prompted,
    /// It needs a person (a permission, a question).
    Waiting,
    /// Its turn ended; what it said.
    Stopped(String),
    /// The agent ended (the terminal may stay, as a shell).
    Ended,
    Nothing,
}

/// The longest prompt or answer kept (the start and the end of it).
const KEEP_CHARS: usize = 2000;
/// Sessions not heard from in this long are forgotten.
const FORGET_MS: u64 = 7 * 24 * 3600 * 1000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    /// By terminal address.
    sessions: BTreeMap<String, AgentSession>,
}

pub struct Sessions {
    book: Mutex<Book>,
    path: PathBuf,
}

/// The daemon's registry.
pub fn registry() -> &'static Sessions {
    static R: OnceLock<Sessions> = OnceLock::new();
    R.get_or_init(|| {
        let path = if cfg!(test) {
            std::env::temp_dir().join(format!("tower-sessions-test-{}.json", std::process::id()))
        } else {
            config::state_dir().join("sessions.json")
        };
        Sessions::at(path)
    })
}

/// Where hooks leave events while no daemon runs (read at its start).
pub fn spool_path() -> PathBuf {
    config::state_dir().join("agent-events.jsonl")
}

impl Sessions {
    pub fn at(path: PathBuf) -> Sessions {
        let book = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Sessions { book: Mutex::new(book), path }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes in one event; says what changed.
    pub fn apply(&self, ev: &AgentEvent) -> Change {
        if ev.terminal.is_empty() {
            return Change::Nothing;
        }
        let now = if ev.at_ms > 0 { ev.at_ms } else { crate::history::now_ms() };
        let mut b = self.lock();
        let s = b.sessions.entry(ev.terminal.clone()).or_default();
        // Another agent session in the same terminal starts over.
        if !ev.session_id.is_empty() && s.session_id != ev.session_id {
            *s = AgentSession { terminal: ev.terminal.clone(), session_id: ev.session_id.clone(), started_ms: now, ..Default::default() };
        }
        // An event older than what we have (a spooled one) only fills gaps.
        let stale = now < s.event_ms;
        s.terminal = ev.terminal.clone();
        if !ev.app.is_empty() {
            s.app = ev.app.clone();
        }
        if !ev.transcript.is_empty() {
            s.transcript = ev.transcript.clone();
        }
        if !ev.cwd.is_empty() {
            s.cwd = ev.cwd.clone();
        }
        if stale {
            return Change::Nothing;
        }
        s.event = ev.event.clone();
        s.event_ms = now;
        let text = keep(&ev.text);
        let change = match ev.event.as_str() {
            "start" => {
                s.state = "idle".into();
                if s.started_ms == 0 {
                    s.started_ms = now;
                }
                Change::Started
            }
            "prompt" => {
                s.state = "working".into();
                s.prompt = text;
                s.prompt_ms = now;
                Change::Prompted
            }
            "permission" | "notify" => {
                s.notice = text;
                s.notice_ms = now;
                // "Waiting for your input" after a finished turn is no question.
                if ev.event == "permission" || s.state == "working" {
                    s.state = "waiting".into();
                    Change::Waiting
                } else {
                    Change::Nothing
                }
            }
            "stop" | "stop-failed" => {
                s.state = "done".into();
                if !text.is_empty() {
                    s.said = text.clone();
                    s.said_ms = now;
                }
                Change::Stopped(text)
            }
            "end" => {
                s.state = "ended".into();
                Change::Ended
            }
            _ => Change::Nothing,
        };
        b.sessions.retain(|_, s| now.saturating_sub(s.event_ms) < FORGET_MS);
        self.save(&b);
        change
    }

    pub fn get(&self, terminal: &str) -> Option<AgentSession> {
        self.lock().sessions.get(terminal).cloned()
    }

    pub fn all(&self) -> Vec<AgentSession> {
        self.lock().sessions.values().cloned().collect()
    }

    /// The terminal is gone (Herdr removed it): its session goes too, so a
    /// terminal that later gets the same address starts clean.
    pub fn forget(&self, terminal: &str) {
        let mut b = self.lock();
        if b.sessions.remove(terminal).is_some() {
            self.save(&b);
        }
    }

    /// What the session said at the end of its last turn, if that was after `since_ms`.
    pub fn said_since(&self, terminal: &str, since_ms: u64) -> Option<String> {
        self.get(terminal).filter(|s| s.said_ms >= since_ms && !s.said.is_empty()).map(|s| s.said)
    }

    /// Events hooks left while no daemon ran: into the registry (they're
    /// history now, so nothing fires for them).
    pub fn take_spool(&self, path: &Path) -> usize {
        let Ok(text) = std::fs::read_to_string(path) else { return 0 };
        let _ = std::fs::remove_file(path);
        let mut n = 0;
        for line in text.lines() {
            if let Ok(ev) = serde_json::from_str::<AgentEvent>(line) {
                self.apply(&ev);
                n += 1;
            }
        }
        n
    }

    fn save(&self, b: &Book) {
        let tmp = self.path.with_extension(format!("json.{}.tmp", std::process::id()));
        if let Ok(bytes) = serde_json::to_vec_pretty(b)
            && std::fs::write(&tmp, bytes).is_ok()
        {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// The start and the end of a long text.
fn keep(text: &str) -> String {
    let text = text.trim();
    let n = text.chars().count();
    if n <= KEEP_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(KEEP_CHARS / 2).collect();
    let tail: String = text.chars().skip(n - KEEP_CHARS / 2).collect();
    format!("{head} … {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, text: &str, at: u64) -> AgentEvent {
        AgentEvent {
            app: "claude-code".into(),
            event: event.into(),
            terminal: "t5".into(),
            session_id: "s1".into(),
            cwd: "/w/app".into(),
            text: text.into(),
            at_ms: at,
            ..Default::default()
        }
    }

    fn reg() -> (Sessions, PathBuf) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("tower-sessions-{}-{n}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        (Sessions::at(p.clone()), p)
    }

    #[test]
    fn a_turn_from_prompt_to_answer() {
        let (r, p) = reg();
        let t0 = crate::history::now_ms();
        assert_eq!(r.apply(&ev("start", "", t0)), Change::Started);
        assert_eq!(r.apply(&ev("prompt", "fix the login", t0 + 1)), Change::Prompted);
        assert_eq!(r.get("t5").unwrap().state, "working");
        assert_eq!(r.apply(&ev("notify", "Claude needs your permission to use Bash", t0 + 2)), Change::Waiting);
        assert_eq!(r.apply(&ev("stop", "Fixed it; tests pass.", t0 + 3)), Change::Stopped("Fixed it; tests pass.".into()));
        let s = r.get("t5").unwrap();
        assert_eq!((s.state.as_str(), s.prompt.as_str(), s.said.as_str()), ("done", "fix the login", "Fixed it; tests pass."));
        assert_eq!(r.said_since("t5", t0), Some("Fixed it; tests pass.".into()));
        assert_eq!(r.said_since("t5", t0 + 10), None);
        // Kept across a restart.
        assert_eq!(Sessions::at(p.clone()).get("t5").unwrap().said, "Fixed it; tests pass.");
        // An idle reminder after the turn is no question.
        assert_eq!(r.apply(&ev("notify", "Claude is waiting for your input", t0 + 4)), Change::Nothing);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_new_session_in_the_terminal_starts_clean() {
        let (r, p) = reg();
        let t0 = crate::history::now_ms();
        r.apply(&ev("stop", "old answer", t0));
        let mut e = ev("start", "", t0 + 1);
        e.session_id = "s2".into();
        r.apply(&e);
        let s = r.get("t5").unwrap();
        assert_eq!((s.session_id.as_str(), s.said.as_str()), ("s2", ""));
        r.forget("t5");
        assert!(r.get("t5").is_none());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn spooled_events_fill_in_but_never_go_back() {
        let (r, p) = reg();
        let t0 = crate::history::now_ms();
        r.apply(&ev("stop", "newest", t0 + 5));
        let spool = p.with_extension("spool.jsonl");
        std::fs::write(&spool, serde_json::to_string(&ev("prompt", "older", t0)).unwrap() + "\n").unwrap();
        assert_eq!(r.take_spool(&spool), 1);
        assert_eq!(r.get("t5").unwrap().state, "done");
        assert!(!spool.exists());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_nested_agent_is_not_the_terminal() {
        let mut e = ev("end", "", 1);
        e.pid = 52490;
        assert!(!from_terminals_agent(&e, Some(48435)), "a claude -p run inside the session");
        e.pid = 48435;
        assert!(from_terminals_agent(&e, Some(48435)));
        assert!(from_terminals_agent(&e, None), "foreground not known");
        e.pid = 0;
        assert!(from_terminals_agent(&e, Some(48435)), "pid not known");
    }

    #[test]
    fn long_texts_keep_both_ends() {
        let long = "a".repeat(3000) + "END";
        let k = keep(&long);
        assert!(k.chars().count() < 2100 && k.ends_with("END"));
    }

    #[test]
    fn events_without_a_terminal_are_ignored() {
        let (r, p) = reg();
        let mut e = ev("prompt", "x", 1);
        e.terminal.clear();
        assert_eq!(r.apply(&e), Change::Nothing);
        let _ = std::fs::remove_file(p);
    }
}
