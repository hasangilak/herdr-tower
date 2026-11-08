//! The question sets open now (see `questions`), one per session: a blocked
//! session's dialog (found when it blocks), or a question its last message
//! asks in plain text (found when it finishes a turn). Published in the
//! state (`questions`, each agent's `question`), pushed to the phones (a
//! `question` event), handed to the brain (`[question] …`,
//! `[question-text] …`), which recommends three answers per question and
//! decides: `auto` (the tower answers with recommendation 1, once, and logs
//! it) or `escalate` (the user is told what is asked and what the tower
//! would pick). The 90/10 rule (`questions::must_escalate`) and the user's
//! policy override the brain.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::lan::hub::Hub;
use super::state::Shared;
use crate::hc;
use crate::questions::{self, Answer, QuestionSet};

type Notify = Box<dyn Fn(&str, &str) + Send + Sync>;

#[derive(Default)]
struct Inner {
    sets: Vec<QuestionSet>,
    /// Replies to text questions waiting for their session to stop working.
    replies: HashMap<String, String>,
    /// `normal`, `keep-moving` or `ask-everything`.
    policy: String,
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
    /// Tells the user (address, one sentence): the radar's alert, held for other spaces as usual.
    notify: Arc<std::sync::OnceLock<Notify>>,
}

impl Asks {
    pub fn new(shared: Shared, hub: Arc<Hub>, policy: &str) -> Asks {
        let me = Asks { inner: Arc::new(Mutex::new(Inner::default())), shared, hub, notify: Arc::new(std::sync::OnceLock::new()) };
        me.set_policy(policy);
        me
    }

    pub fn set_notify(&self, f: impl Fn(&str, &str) + Send + Sync + 'static) {
        let _ = self.notify.set(Box::new(f));
    }

    fn tell(&self, address: &str, text: &str) {
        match self.notify.get() {
            Some(f) => f(address, text),
            None => eprintln!("[questions] (nobody to tell) {text}"),
        }
    }

    pub fn policy(&self) -> String {
        self.inner.lock().unwrap().policy.clone()
    }

    pub fn set_policy(&self, policy: &str) {
        let p = if ["normal", "keep-moving", "ask-everything"].contains(&policy) { policy } else { "normal" };
        self.inner.lock().unwrap().policy = p.to_string();
        let p = p.to_string();
        self.shared.update(|s| s.question_policy = p);
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

    /// A session's status as the radar sees it (`status`: its status word,
    /// "" when it is gone). Blocked with a new status line: its dialog is read
    /// (on a thread). Not blocked: its dialog's set closes. Working (or gone):
    /// a question its last message asked is stale. Idle or done: a reply
    /// waiting for it goes in.
    pub fn seen(&self, address: &str, t: &Value, status: &str) {
        let blocked = status == "blocked";
        if !blocked {
            self.close_where(address, |s| s.kind != "text");
            if status == "working" || status.is_empty() {
                self.close_where(address, |s| s.kind == "text");
            }
            if matches!(status, "idle" | "done") {
                let reply = self.inner.lock().unwrap().replies.remove(address);
                if let Some(text) = reply {
                    match type_into(address, &text) {
                        Ok(()) => eprintln!("[questions] {address}: the waiting reply went in"),
                        Err(e) => eprintln!("[questions] {address}: the waiting reply didn't go in: {e:#}"),
                    }
                }
            }
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
                let had = self.list().iter().any(|s| s.session == address && s.kind != "text");
                self.close_where(address, |s| s.kind != "text");
                // Blocked on something it can't read: the plain notice, as before.
                if !had
                    && let Some(t) = hc::get(address).filter(|t| hc::status_word(t).as_deref() == Some("blocked"))
                    && !super::watch::is_test(&t)
                {
                    self.tell(address, &blocked_notice(&hc::spoken_name(&t), &t["status"]));
                }
                None
            }
        }
    }

    /// A session finished a turn (`working` to idle or done): if its last
    /// message seems to ask the user something, a text question set, for the
    /// brain to read.
    pub fn finished(&self, address: &str, t: &Value) {
        if super::watch::is_test(t) {
            return;
        }
        let (me, address, t) = (self.clone(), address.to_string(), t.clone());
        std::thread::spawn(move || {
            let Some(message) = last_message(&address, &t) else { return };
            if !questions::looks_like_question(&message) {
                return;
            }
            let context = crate::history::jobs(&crate::history::load()).into_iter().find(|j| j.session == address).map(|j| j.title).unwrap_or_default();
            let mut set = questions::text_set(&address, &hc::spoken_name(&t), &message, &context);
            set.at_ms = crate::history::now_ms();
            me.upsert(set);
        });
    }

    fn upsert(&self, mut set: QuestionSet) -> QuestionSet {
        let (new, note) = {
            let mut i = self.inner.lock().unwrap();
            let old = i.sets.iter().position(|s| s.session == set.session && (s.kind == "text") == (set.kind == "text"));
            let new = match old {
                Some(k) if i.sets[k].id == set.id => {
                    // The same questions: keep what came for them.
                    set.recommendations = i.sets[k].recommendations.clone();
                    set.decision = i.sets[k].decision.clone();
                    set.reason = i.sets[k].reason.clone();
                    set.answered_with = i.sets[k].answered_with.clone();
                    if matches!(i.sets[k].status.as_str(), "answering" | "answered") {
                        set.status = i.sets[k].status.clone();
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
            let note = new && !i.noted.contains(&set.id) && set.decision.is_none();
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
            // The brain recommends answers and decides, quietly.
            let text = if set.kind == "text" { questions::text_note(&set) } else { questions::brain_note(&set) };
            let text = format!("{text} policy={}", self.policy());
            std::thread::spawn(move || {
                if let Err(e) = crate::brain::note(&text) {
                    eprintln!("[questions] the brain can't take it: {e:#}");
                }
            });
            // No word from the brain in time: a dialog goes to the user as it is.
            if set.kind != "text" {
                let (me, id) = (self.clone(), set.id.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(40));
                    if let Some(s) = me.get(&id).filter(|s| s.decision.is_none() && s.status == "open") {
                        me.escalate(&s.id, "no recommendation came in time");
                    }
                });
            }
        }
        set
    }

    fn close_where(&self, address: &str, which: impl Fn(&QuestionSet) -> bool) {
        let gone: Vec<String> = {
            let mut i = self.inner.lock().unwrap();
            let ids: Vec<String> = i.sets.iter().filter(|s| s.session == address && which(s)).map(|s| s.id.clone()).collect();
            // A dialog's status line is read again next time it blocks.
            if i.sets.iter().any(|s| s.session == address && which(s) && s.kind != "text") || !i.sets.iter().any(|s| s.session == address) {
                i.msgs.remove(address);
            }
            i.sets.retain(|s| !(s.session == address && which(s)));
            ids
        };
        if !gone.is_empty() {
            self.publish();
            for id in gone {
                self.event("closed", None, &id);
            }
        }
    }

    /// The user dismissed a question set (a text question that isn't one, or one they'll answer in the terminal).
    pub fn dismiss(&self, id: &str) -> Result<()> {
        let Some(set) = self.get(id) else { bail!("no open question set {id}") };
        self.close_where(&set.session, |s| s.id == id);
        Ok(())
    }

    /// `tower recommend <id>`: the brain's three answers per question, and
    /// its decision (`auto` or `escalate`, with why). For a text question
    /// set the brain also says what it asks; `none`: it asks nothing.
    pub fn recommend(&self, id: &str, v: &Value, decision: Option<&str>, reason: &str, none: bool) -> Result<QuestionSet> {
        let set = self.get(id).ok_or_else(|| anyhow::anyhow!("no open question set {id}"))?;
        if set.decision.is_some() {
            bail!("{id} is decided already ({})", set.decision.unwrap_or_default());
        }
        if none {
            if set.kind != "text" {
                bail!("--none is for a message that turned out to ask nothing; a dialog always asks");
            }
            self.close_where(&set.session, |s| s.id == id);
            eprintln!("[questions] {id}: the brain says {} asks nothing", set.session);
            return Ok(set);
        }
        let (tabs, recs) = if set.kind == "text" {
            let (tabs, recs) = questions::text_recs(v)?;
            (Some(tabs), recs)
        } else {
            (None, questions::parse_recs(v, &set)?)
        };
        let set = {
            let mut i = self.inner.lock().unwrap();
            let policy = i.policy.clone();
            let Some(s) = i.sets.iter_mut().find(|s| s.id == id) else { bail!("no open question set {id}") };
            if let Some(t) = tabs {
                s.title = t.first().map(|t| t.question.clone()).unwrap_or_default();
                s.tabs = t;
            }
            s.recommendations = Some(recs);
            let (d, why) = questions::decide(s, decision, reason, &policy);
            s.decision = Some(d);
            s.reason = why;
            s.clone()
        };
        self.publish();
        self.event("update", Some(&set), id);
        if set.decision.as_deref() == Some("auto") {
            let me = self.clone();
            let id = id.to_string();
            std::thread::spawn(move || me.auto_answer(&id));
        } else {
            self.tell_escalated(&set);
        }
        Ok(set)
    }

    /// The tower answers it itself: recommendation 1 for every question, once, logged.
    fn auto_answer(&self, id: &str) {
        let Some(set) = self.get(id) else { return };
        let Some(answers) = questions::first_picks(&set) else {
            self.escalate(id, "a question had no recommendation");
            return;
        };
        let picked = questions::picked_line(&set);
        match self.answer(id, &answers, true) {
            Ok(_) => {
                let line = format!("answered for you: {picked}, because {}", if set.reason.is_empty() { "it's routine" } else { &set.reason });
                eprintln!("[questions] {}: {line}", set.session);
                let (a, l) = (set.session.clone(), line.clone());
                crate::history::later(move || crate::history::record_session("answered", &a, &l, Some("auto")));
                if let Some(s) = self.inner.lock().unwrap().sets.iter_mut().find(|s| s.id == id) {
                    s.answered_with = picked;
                }
                self.publish();
            }
            Err(e) => self.escalate(id, &format!("answering it didn't work: {e:#}")),
        }
    }

    /// Goes to the user after all.
    fn escalate(&self, id: &str, why: &str) {
        let set = {
            let mut i = self.inner.lock().unwrap();
            let Some(s) = i.sets.iter_mut().find(|s| s.id == id) else { return };
            s.decision = Some("escalate".into());
            s.reason = why.to_string();
            if s.status == "failed" || s.status == "answering" {
                s.status = "open".into();
            }
            s.clone()
        };
        self.publish();
        self.event("update", Some(&set), id);
        self.tell_escalated(&set);
    }

    /// One sentence: what is asked, and what the tower would pick.
    fn tell_escalated(&self, set: &QuestionSet) {
        let what = match set.kind.as_str() {
            "claude-permission" | "codex-menu" => set.title.trim_end_matches('?').to_string(),
            "claude-plan" => "approval for its plan".to_string(),
            _ => set.tabs.first().map(|t| t.question.clone()).unwrap_or_default(),
        };
        let more = if set.tabs.len() > 1 { format!(" (and {} more)", set.tabs.len() - 1) } else { String::new() };
        let pick = set
            .recommendations
            .as_ref()
            .and_then(|r| r.iter().find(|r| r.tab == 0))
            .and_then(|r| r.options.first())
            .map(|o| format!(" I'd say: {}.", o.label.trim_end_matches('.')))
            .unwrap_or_default();
        let line = format!("{} asks: {}{more}.{pick}", set.name, what.trim_end_matches(['.', '?']));
        self.tell(&set.session, &line);
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
        if let Some(k) = &known {
            if k.status == "answered" {
                bail!("{} is answered already", k.id);
            }
            if k.kind == "text" {
                return self.reply(k, answers);
            }
        }
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

impl Asks {
    /// A text question's answer: one reply, typed into the session now if it
    /// is idle or done, else when it is.
    fn reply(&self, set: &QuestionSet, answers: &[Answer]) -> Result<QuestionSet> {
        let text = questions::compose_reply(set, answers)?;
        let status = hc::get(&set.session).and_then(|t| hc::status_word(&t)).unwrap_or_default();
        let queued = !matches!(status.as_str(), "idle" | "done");
        if queued {
            if status.is_empty() {
                bail!("{} isn't there any more", set.session);
            }
            self.inner.lock().unwrap().replies.insert(set.session.clone(), text.clone());
        } else {
            type_into(&set.session, &text)?;
        }
        let done = {
            let mut i = self.inner.lock().unwrap();
            let Some(s) = i.sets.iter_mut().find(|s| s.id == set.id) else { bail!("no open question set {}", set.id) };
            s.status = if queued { "queued".into() } else { "answered".into() };
            s.answered_with = text.clone();
            s.clone()
        };
        self.publish();
        self.event("update", Some(&done), &done.id);
        // Answered: the card goes (the reply shows in the session).
        let (me, a, id) = (self.clone(), set.session.clone(), set.id.clone());
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            me.close_where(&a, |s| s.id == id);
        });
        Ok(done)
    }
}

/// Types a reply into a session as the user would (no note saying who sent it).
fn type_into(address: &str, text: &str) -> Result<()> {
    let out = std::process::Command::new(crate::config::hc_bin())
        .args(["send", address, text])
        .env_remove("HC_TERMINAL_ID")
        .env_remove("HC_SCOPE")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        bail!("hc send: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// The last message a session wrote: its transcript (Claude Code here), else its screen.
fn last_message(address: &str, t: &Value) -> Option<String> {
    if hc::agent_app(t).as_deref() == Some("claude-code")
        && !address.contains('/')
        && let Some(pid) = t["foreground"]["pid"].as_u64()
        && let Some(text) = questions::transcript_of(pid as u32).and_then(|j| questions::last_assistant_text(&j))
    {
        return Some(text);
    }
    let screen = hc::run(&["read", address, "--lines", "60"]).ok()?.stdout;
    questions::message_from_screen(&screen)
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
