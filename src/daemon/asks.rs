//! The question sets open now, one per blocked session (see `questions`):
//! found when a session blocks (`watch` calls `seen`), published in the
//! state (`questions`, and each agent's `question`), pushed to the phones (a
//! `question` event), handed to the brain for recommendations (`[question]
//! …`), and answered by driving the session's dialog (`answer`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::lan::hub::Hub;
use super::state::Shared;
use crate::hc;
use crate::questions::{self, Answer, QuestionSet};

#[derive(Default)]
struct Inner {
    sets: Vec<QuestionSet>,
    /// The status line herdr-core last gave per session: a new one means a new screen to read.
    msgs: HashMap<String, String>,
    /// Sets the brain was told about.
    noted: Vec<String>,
}

#[derive(Clone)]
pub struct Asks {
    inner: Arc<Mutex<Inner>>,
    shared: Shared,
    hub: Arc<Hub>,
}

impl Asks {
    pub fn new(shared: Shared, hub: Arc<Hub>) -> Asks {
        Asks { inner: Arc::new(Mutex::new(Inner::default())), shared, hub }
    }

    pub fn list(&self) -> Vec<QuestionSet> {
        self.inner.lock().unwrap().sets.clone()
    }

    pub fn get(&self, id: &str) -> Option<QuestionSet> {
        self.inner.lock().unwrap().sets.iter().find(|s| s.id == id).cloned()
    }

    fn publish(&self) {
        let sets = self.list();
        let by_session: HashMap<String, String> = sets.iter().map(|s| (s.session.clone(), s.id.clone())).collect();
        self.shared.update(|st| {
            st.questions = sets.iter().map(|s| serde_json::to_value(s).unwrap_or_default()).collect();
            for a in st.agents.iter_mut() {
                a.question = by_session.get(&a.address).cloned();
            }
        });
    }

    fn event(&self, op: &str, set: Option<&QuestionSet>, id: &str) {
        let data = match set {
            Some(s) => json!({ "op": op, "id": id, "question": s }),
            None => json!({ "op": op, "id": id }),
        };
        self.hub.broadcast("question", data);
    }

    /// A session's status as the radar sees it. Blocked with a new status
    /// line: its screen is read (on a thread) and its question set updated.
    /// Not blocked: its set is closed.
    pub fn seen(&self, address: &str, t: &Value, blocked: bool) {
        if !blocked {
            self.close(address);
            return;
        }
        let msg = t["status"]["msg"].as_str().unwrap_or("").to_string();
        {
            let mut i = self.inner.lock().unwrap();
            if i.msgs.get(address) == Some(&msg) && i.sets.iter().any(|s| s.session == address) {
                return;
            }
            i.msgs.insert(address.to_string(), msg);
        }
        let (me, address) = (self.clone(), address.to_string());
        std::thread::spawn(move || {
            // Let the dialog finish drawing.
            std::thread::sleep(std::time::Duration::from_millis(400));
            me.refresh(&address);
        });
    }

    /// Reads a session's dialog again (after an answer, or a new status line).
    pub fn refresh(&self, address: &str) -> Option<QuestionSet> {
        match detect(address) {
            Some(set) => Some(self.upsert(set)),
            None => {
                self.close(address);
                None
            }
        }
    }

    fn upsert(&self, mut set: QuestionSet) -> QuestionSet {
        let (new, note) = {
            let mut i = self.inner.lock().unwrap();
            let old = i.sets.iter().position(|s| s.session == set.session);
            let new = match old {
                Some(k) if i.sets[k].id == set.id => {
                    // The same questions: keep what came for them.
                    set.recommendations = i.sets[k].recommendations.clone();
                    if i.sets[k].status == "answering" {
                        set.status = "answering".into();
                    }
                    i.sets[k] = set.clone();
                    false
                }
                Some(k) => {
                    i.sets[k] = set.clone();
                    true
                }
                None => {
                    i.sets.push(set.clone());
                    true
                }
            };
            let note = new && !i.noted.contains(&set.id);
            if note {
                i.noted.push(set.id.clone());
                if i.noted.len() > 200 {
                    i.noted.remove(0);
                }
            }
            (new, note)
        };
        self.publish();
        self.event(if new { "open" } else { "update" }, Some(&set), &set.id);
        if note {
            eprintln!("[questions] {} ({}) asks: {} ({} tab(s), id {})", set.session, set.kind, set.title, set.tabs.len(), set.id);
            // The brain recommends answers, quietly.
            let text = questions::brain_note(&set);
            std::thread::spawn(move || {
                if let Err(e) = crate::brain::note(&text) {
                    eprintln!("[questions] the brain can't take it: {e:#}");
                }
            });
        }
        set
    }

    fn close(&self, address: &str) {
        let gone: Vec<String> = {
            let mut i = self.inner.lock().unwrap();
            i.msgs.remove(address);
            let ids = i.sets.iter().filter(|s| s.session == address).map(|s| s.id.clone()).collect();
            i.sets.retain(|s| s.session != address);
            ids
        };
        if !gone.is_empty() {
            self.publish();
            for id in gone {
                self.event("closed", None, &id);
            }
        }
    }

    /// `tower recommend <id>`: the brain's three answers per tab.
    pub fn recommend(&self, id: &str, v: &Value) -> Result<QuestionSet> {
        let set = self.get(id).ok_or_else(|| anyhow::anyhow!("no open question set {id}"))?;
        let recs = questions::parse_recs(v, &set)?;
        let set = {
            let mut i = self.inner.lock().unwrap();
            let Some(s) = i.sets.iter_mut().find(|s| s.id == id) else { bail!("no open question set {id}") };
            s.recommendations = Some(recs);
            s.clone()
        };
        self.publish();
        self.event("update", Some(&set), id);
        Ok(set)
    }

    fn set_status(&self, id: &str, status: &str, error: Option<String>) {
        let set = {
            let mut i = self.inner.lock().unwrap();
            let Some(s) = i.sets.iter_mut().find(|s| s.id == id) else { return };
            s.status = status.into();
            s.error = error;
            s.clone()
        };
        self.publish();
        self.event("update", Some(&set), id);
    }

    /// Answers a question set (by id, or a session's address: its dialog is
    /// read now). The dialog is read again first: answers go to what it
    /// shows now, and nothing is typed if it changed.
    pub fn answer(&self, target: &str, answers: &[Answer], submit: bool) -> Result<QuestionSet> {
        let known = self.get(target);
        let address = known.as_ref().map(|s| s.session.clone()).unwrap_or_else(|| target.to_string());
        let Some(now) = detect(&address) else { bail!("{address} shows no question it can answer now") };
        if let Some(k) = &known
            && k.id != now.id
        {
            bail!("the questions changed since they were shown (now {}); look again", now.id);
        }
        {
            let i = self.inner.lock().unwrap();
            if i.sets.iter().any(|s| s.id == now.id && s.status == "answering") {
                bail!("answers are already going in");
            }
        }
        let tracked = known.is_some();
        if tracked {
            self.set_status(&now.id, "answering", None);
        }
        let mut term = questions::Hc { address: address.clone() };
        let result = questions::drive(&mut term, &now, answers, submit);
        match result {
            Ok(()) => {
                eprintln!("[questions] answered {} on {address}", now.id);
                if tracked {
                    self.set_status(&now.id, if submit || now.submit_state == "single" { "answered" } else { "open" }, None);
                }
                // The session moves on: read it again shortly.
                let (me, a) = (self.clone(), address.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    me.refresh(&a);
                });
                Ok(now)
            }
            Err(stop) => {
                eprintln!("[questions] stopped answering {} on {address}: {stop}", now.id);
                if tracked {
                    self.set_status(&now.id, "failed", Some(stop.to_string()));
                }
                bail!("stopped at {}", stop)
            }
        }
    }
}

/// What the session `address` asks now, read from its screen (and its
/// transcript for Claude's questions). None: not blocked on a dialog it reads.
pub fn detect(address: &str) -> Option<QuestionSet> {
    let t = hc::get(address)?;
    if hc::status_word(&t).as_deref() != Some("blocked") {
        return None;
    }
    let screen_text = hc::run(&["read", address]).ok()?.stdout;
    let screen = questions::parse_screen(&screen_text)?;
    let app = hc::agent_app(&t).unwrap_or_default();
    let transcript = if matches!(screen.kind.as_str(), "ask" | "review") && app == "claude-code" {
        t["foreground"]["pid"].as_u64().and_then(|p| questions::transcript_of(p as u32)).and_then(|j| questions::transcript_questions(&j))
    } else {
        None
    };
    let addr = t["address"].as_str().unwrap_or(address).to_string();
    let context = crate::history::jobs(&crate::history::load()).into_iter().find(|j| j.session == addr && j.open()).map(|j| j.title).unwrap_or_default();
    let msg = t["status"]["msg"].as_str().unwrap_or("");
    let mut set = questions::build(&addr, &hc::spoken_name(&t), app == "codex", &screen, transcript, msg, &context);
    set.at_ms = crate::history::now_ms();
    Some(set)
}

/// The one-line notice for a blocked session, from herdr-core's status
/// (`kind`, `msg`): what it asks, in plain words.
pub fn blocked_notice(name: &str, status: &Value) -> String {
    let msg = status["msg"].as_str().unwrap_or("").trim();
    let kind = status["kind"].as_str().unwrap_or("");
    let short = |s: &str, n: usize| -> String {
        let s = s.trim();
        if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>().trim_end()) }
    };
    if let Some(q) = msg.strip_prefix("answer: ") {
        // "Which database? (Postgres · SQLite)": the question, not the options.
        let q = q.split(" (").next().unwrap_or(q);
        return format!("{name} asks: {}", short(q, 120));
    }
    if msg == "approve plan" {
        return format!("{name} has a plan ready for you to approve");
    }
    if let Some(rest) = msg.strip_prefix("approve ") {
        return match rest.split_once(": ") {
            Some(("Bash", cmd)) => format!("{name} asks to run {}", short(cmd, 80)),
            Some(("Edit" | "Write" | "MultiEdit", path)) => format!("{name} asks to edit {}", short(path.rsplit('/').next().unwrap_or(path), 60)),
            Some(("WebFetch", url)) => format!("{name} asks to open {}", short(url, 60)),
            Some((tool, what)) => format!("{name} asks to use {tool}: {}", short(what, 80)),
            None => format!("{name} asks to {}", short(rest, 80)),
        };
    }
    if kind == "question" {
        return format!("{name} has questions");
    }
    format!("{name} needs you")
}

#[cfg(test)]
mod tests {
    use super::blocked_notice;
    use serde_json::json;

    #[test]
    fn notices_say_what_is_asked() {
        let n = |kind: &str, msg: &str| blocked_notice("The orb session", &json!({ "kind": kind, "msg": msg }));
        assert_eq!(
            n("question", "answer: Which database should the app use? (Postgres (managed, scales) · SQLite (single file))"),
            "The orb session asks: Which database should the app use?"
        );
        assert_eq!(n("permission", "approve Bash: touch /tmp/x"), "The orb session asks to run touch /tmp/x");
        assert_eq!(n("permission", "approve plan"), "The orb session has a plan ready for you to approve");
        assert_eq!(n("permission", "approve Edit: /w/app/src/login.rs"), "The orb session asks to edit login.rs");
        assert_eq!(n("permission", "approve mcp__x__deploy: prod"), "The orb session asks to use mcp__x__deploy: prod");
        assert_eq!(n("question", ""), "The orb session has questions");
        assert_eq!(n("", ""), "The orb session needs you", "nothing to say: the bare notice");
        let long = format!("approve Bash: {}", "x".repeat(200));
        assert!(n("permission", &long).chars().count() < 120);
    }
}
