//! The question sets open now (see `questions`), one per session: a blocked
//! session's dialog (found when it blocks), or a question its last message
//! asks in plain text (found when it finishes a turn). Published in the
//! state (`questions`, each agent's `question`), pushed to the phones (a
//! `question` event), handed to the brain (`[question] …`,
//! `[question-text] …`), which recommends three answers per question and
//! decides: `auto` (the tower answers with recommendation 1, once, and logs
//! it) or `escalate` (the user is told what is asked and what the tower
//! would pick). The 90/10 rule (`questions::must_escalate`), the menu check
//! (`questions::safe_auto`: only a plain one-time yes) and the user's policy
//! override the brain; both are checked again on the screen right before an
//! answer is typed for the user.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::lan::hub::Hub;
use super::state::Shared;
use crate::hc;
use crate::questions::{self, Answer, QuestionSet};

type Notify = Box<dyn Fn(&str, &str) + Send + Sync>;
type Note = Arc<dyn Fn(&str) + Send + Sync>;

/// A queued reply drops after this long (its question is stale by then).
const REPLY_TTL_MS: u64 = 30 * 60 * 1000;

/// A reply to a text question, waiting for its session to stop working.
#[derive(Debug, Clone, PartialEq)]
struct Reply {
    /// The question set it answers.
    set: String,
    text: String,
    at_ms: u64,
    /// The session's turn count then (herdr-core's `turn`; it goes up each
    /// time the session stops working, for a dialog too), when it says;
    /// raised by one for each dialog it shows meanwhile.
    turn: Option<u64>,
    /// The session's status when last seen.
    last: String,
}

impl Reply {
    /// Why it no longer goes in: too old, or the session has stopped and
    /// started again on its own since it was queued (a new turn).
    fn stale(&self, now_ms: u64, turn: Option<u64>, status: &str) -> Option<&'static str> {
        if now_ms.saturating_sub(self.at_ms) > REPLY_TTL_MS {
            return Some("it waited too long");
        }
        let (Some(then), Some(now)) = (self.turn, turn) else { return None };
        // One stop is the one it waits for; working again after it, or a second stop, is a turn of its own.
        if now > then + 1 || (now > then && status == "working") {
            return Some("the session moved on without it");
        }
        None
    }
}

/// herdr-core's turn count for a terminal.
fn turn_of(t: &Value) -> Option<u64> {
    t["turn"].as_u64().or_else(|| t["status"]["turn"].as_u64())
}

/// Why an answer the tower would give by itself goes to the user after all
/// (the screen, read again right before typing, fails the rule or the menu check).
#[derive(Debug)]
struct Held(String);

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Held {}

#[derive(Default)]
struct Inner {
    sets: Vec<QuestionSet>,
    /// Replies to text questions waiting for their session to stop working, by session.
    replies: HashMap<String, Reply>,
    /// `normal`, `keep-moving` or `ask-everything`.
    policy: String,
    /// The status line herdr-core last gave per session: a new one means a new screen to read.
    msgs: HashMap<String, String>,
    /// Sets the brain was told about. A set's id leaves when the session
    /// stops showing it (answered, gone), so the same dialog asked again is
    /// told again.
    noted: Vec<String>,
    /// Dialogs the user dismissed (session, id): not shown again while the
    /// session still shows them.
    dismissed: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct Asks {
    inner: Arc<Mutex<Inner>>,
    shared: Shared,
    hub: Arc<Hub>,
    /// Tells the user (address, one sentence): the radar's alert, held for other spaces as usual.
    notify: Arc<std::sync::OnceLock<Notify>>,
    /// Tells the brain (`[question] …`); tests keep what it would have been told.
    brain: Note,
}

impl Asks {
    pub fn new(shared: Shared, hub: Arc<Hub>, policy: &str) -> Asks {
        let brain: Note = Arc::new(|text: &str| {
            if let Err(e) = crate::brain::note(text) {
                eprintln!("[questions] the brain can't take it: {e:#}");
            }
        });
        let me = Asks { inner: Arc::new(Mutex::new(Inner::default())), shared, hub, notify: Arc::new(std::sync::OnceLock::new()), brain };
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
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).policy.clone()
    }

    pub fn set_policy(&self, policy: &str) {
        let p = if ["normal", "keep-moving", "ask-everything"].contains(&policy) { policy } else { "normal" };
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).policy = p.to_string();
        let p = p.to_string();
        self.shared.update(|s| s.question_policy = p);
    }

    pub fn list(&self) -> Vec<QuestionSet> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).sets.clone()
    }

    pub fn get(&self, id: &str) -> Option<QuestionSet> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).sets.iter().find(|s| s.id == id).cloned()
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
        self.reply_sees(address, status);
        if !blocked {
            self.close_where(address, |s| s.kind != "text", true);
            if status == "working" || status.is_empty() {
                self.close_where(address, |s| s.kind == "text", true);
            }
            if let Some(r) = self.due_reply(address, status, turn_of(t), crate::history::now_ms()) {
                match type_into(address, &r.text) {
                    Ok(()) => eprintln!("[questions] {address}: the waiting reply to {} went in", r.set),
                    Err(e) => eprintln!("[questions] {address}: the waiting reply to {} didn't go in: {e:#}", r.set),
                }
            }
            return;
        }
        let msg = t["status"]["msg"].as_str().unwrap_or("").to_string();
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let showing = i.sets.iter().any(|s| s.session == address) || i.dismissed.iter().any(|(a, _)| a == address);
            if i.msgs.get(address) == Some(&msg) && showing {
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

    /// A waiting reply's session has `status` now: stopping for a dialog
    /// isn't the turn's end the reply waits for (its count goes up by one).
    fn reply_sees(&self, address: &str, status: &str) {
        if let Some(r) = self.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.get_mut(address) {
            if status == "blocked" && r.last != "blocked" {
                r.turn = r.turn.map(|n| n + 1);
            }
            r.last = status.to_string();
        }
    }

    /// A reply waiting for `address`, taken when it may go in now (`status`
    /// idle or done). Dropped (and logged) when the session is gone, it is
    /// too old, or the session moved on without it.
    fn due_reply(&self, address: &str, status: &str, turn: Option<u64>, now_ms: u64) -> Option<Reply> {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let r = i.replies.get(address)?;
        let why = if status.is_empty() { Some("the session is gone") } else { r.stale(now_ms, turn, status) };
        if let Some(why) = why {
            let r = i.replies.remove(address)?;
            eprintln!("[questions] {address}: the waiting reply to {} is dropped: {why}", r.set);
            return None;
        }
        if matches!(status, "idle" | "done") { i.replies.remove(address) } else { None }
    }

    /// Reads a session's dialog again (after an answer, or a new status line).
    pub fn refresh(&self, address: &str) -> Option<QuestionSet> {
        match detect(address) {
            Some(set) => Some(self.upsert(set)),
            None => {
                let had = self.list().iter().any(|s| s.session == address && s.kind != "text");
                self.close_where(address, |s| s.kind != "text", true);
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
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Dismissed while the session still shows it: it stays away.
            if i.dismissed.iter().any(|(a, id)| *a == set.session && *id == set.id) {
                return set;
            }
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
                    // Other questions now: the old ones may be told again if they come back.
                    let gone = std::mem::replace(&mut i.sets[k], set.clone()).id;
                    i.noted.retain(|n| *n != gone);
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
            let brain = self.brain.clone();
            std::thread::spawn(move || brain(&text));
        }
        // No word from the brain in time: a newly shown dialog goes to the user as it is.
        if new && set.kind != "text" && set.decision.is_none() {
            let (me, id) = (self.clone(), set.id.clone());
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(40));
                if let Some(s) = me.get(&id).filter(|s| s.decision.is_none() && s.status == "open") {
                    me.escalate(&s.id, "no recommendation came in time");
                }
            });
        }
        set
    }

    /// Closes a session's sets that `which` picks. `over`: the session no
    /// longer shows them (answered, unblocked, gone), so the same questions
    /// asked again later are new ones (told to the brain again); without it
    /// (the user dismissed a dialog still up) they stay known.
    fn close_where(&self, address: &str, which: impl Fn(&QuestionSet) -> bool, over: bool) {
        let gone: Vec<String> = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<String> = i.sets.iter().filter(|s| s.session == address && which(s)).map(|s| s.id.clone()).collect();
            let dialog = i.sets.iter().any(|s| s.session == address && which(s) && s.kind != "text");
            // A dialog's status line is read again next time it blocks.
            if over && (dialog || !i.sets.iter().any(|s| s.session == address)) {
                i.msgs.remove(address);
            }
            i.sets.retain(|s| !(s.session == address && which(s)));
            if over {
                i.noted.retain(|n| !ids.contains(n));
                // Its dialogs are over: dismissed ones may come back as new.
                if dialog || !i.sets.iter().any(|s| s.session == address && s.kind != "text") {
                    let dismissed: Vec<String> = i.dismissed.iter().filter(|(a, _)| a == address).map(|(_, id)| id.clone()).collect();
                    i.noted.retain(|n| !dismissed.contains(n));
                    i.dismissed.retain(|(a, _)| a != address);
                }
            }
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
        // A dialog stays on screen until it's answered there: it stays dismissed until then.
        let dialog = set.kind != "text";
        if dialog {
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).dismissed.push((set.session.clone(), id.to_string()));
        }
        self.close_where(&set.session, |s| s.id == id, !dialog);
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
            self.close_where(&set.session, |s| s.id == id, true);
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
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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

    /// The tower answers it itself: recommendation 1 for every question, once,
    /// logged. On a menu only a plain one-time yes: a pick that would stop
    /// future prompts is swapped for it (and the swap logged), else it escalates.
    fn auto_answer(&self, id: &str) {
        let Some(set) = self.get(id) else { return };
        let Some(answers) = questions::first_picks(&set) else {
            self.escalate(id, "a question had no recommendation");
            return;
        };
        let (answers, swap) = match questions::safe_auto(&set, &answers) {
            Ok(x) => x,
            Err(why) => return self.escalate(id, &why),
        };
        let picked = match &swap {
            Some(sw) => {
                eprintln!("[questions] {}: \"{}\" would stop future prompts; answering \"{}\" instead", set.session, sw.from, sw.to);
                format!("{} -> {} (not \"{}\")", set.tabs.first().map(|t| t.question.as_str()).unwrap_or(""), sw.to, sw.from)
            }
            None => questions::picked_line(&set),
        };
        match self.answer_as(id, &answers, true, true) {
            Ok(_) => {
                let line =
                    format!("answered for you: {picked}, because {}", if set.reason.is_empty() { "it's routine" } else { &set.reason });
                eprintln!("[questions] {}: {line}", set.session);
                let (a, l) = (set.session.clone(), line.clone());
                crate::history::later(move || crate::history::record_session("answered", &a, &l, Some("auto")));
                if let Some(s) = self.inner.lock().unwrap_or_else(|e| e.into_inner()).sets.iter_mut().find(|s| s.id == id) {
                    s.answered_with = picked;
                }
                self.publish();
            }
            Err(e) => match e.downcast_ref::<Held>() {
                Some(held) => self.escalate(id, &held.0),
                None => self.escalate(id, &format!("answering it didn't work: {e:#}")),
            },
        }
    }

    /// Goes to the user after all.
    fn escalate(&self, id: &str, why: &str) {
        let set = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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

    /// The policy, the 90/10 rule and the menu check, on the set the screen
    /// shows now: the answers to type, or why it goes to the user.
    fn recheck(&self, now: &QuestionSet, answers: &[Answer]) -> Result<Vec<Answer>, String> {
        if self.policy() == "ask-everything" {
            return Err("you asked to decide everything yourself".into());
        }
        if let Some(why) = questions::must_escalate(now) {
            return Err(why.into());
        }
        questions::safe_auto(now, answers).map(|(a, _)| a)
    }

    /// Marks the set `id` as answering, unless answers are going in already
    /// (one lock for both). False: the tower doesn't track it.
    fn start_answering(&self, id: &str) -> Result<bool> {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = i.sets.iter_mut().find(|s| s.id == id) else { return Ok(false) };
        if s.status == "answering" {
            bail!("answers are already going in");
        }
        s.status = "answering".into();
        s.error = None;
        Ok(true)
    }

    fn set_status(&self, id: &str, status: &str, error: Option<String>) {
        let set = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
        self.answer_as(target, answers, submit, false)
    }

    /// `auto`: the tower answers for the user, so the policy, the rule and
    /// the menu check are applied again to the screen as it is now (a `Held`
    /// error when it must go to the user after all).
    fn answer_as(&self, target: &str, answers: &[Answer], submit: bool, auto: bool) -> Result<QuestionSet> {
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
        let answers = if auto { self.recheck(&now, answers).map_err(Held)? } else { answers.to_vec() };
        // Checked and marked at once: a second answer (the phone, the panel, the tower) waits its turn.
        let tracked = self.start_answering(&now.id)?;
        if tracked {
            self.set_status(&now.id, "answering", None);
        }
        let result = {
            let _typing = hc::typing(&address);
            let mut term = questions::Hc { address: address.clone() };
            questions::drive(&mut term, &now, &answers, submit)
        };
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
            let turn = hc::get(&set.session).as_ref().and_then(turn_of);
            let r = Reply { set: set.id.clone(), text: text.clone(), at_ms: crate::history::now_ms(), turn, last: status.clone() };
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert(set.session.clone(), r);
        } else {
            type_into(&set.session, &text)?;
        }
        let done = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
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
            me.close_where(&a, |s| s.id == id, true);
        });
        Ok(done)
    }
}

/// Types a reply into a session as the user would (no note saying who sent
/// it), only while it is idle or done (read again just before), without
/// control characters, and never at the same time as other keys for it.
fn type_into(address: &str, text: &str) -> Result<()> {
    let _typing = hc::typing(address);
    let status = hc::get(address).and_then(|t| hc::status_word(&t)).unwrap_or_default();
    let args = send_args(address, &questions::clean_text(text), &status)?;
    let out = std::process::Command::new(crate::config::hc_bin())
        .args(&args)
        .env_remove("HC_TERMINAL_ID")
        .env_remove("HC_SCOPE")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        bail!("hc send: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// `hc send`'s arguments for a reply to a session whose status is `status`
/// now: only idle or done takes one (idle: hc checks it again as it types,
/// with `--if idle`). `--` before the terminal and the text, so a reply like
/// "- yes" isn't taken for a flag.
fn send_args(address: &str, text: &str, status: &str) -> Result<Vec<String>> {
    let mut args = vec!["send".to_string()];
    match status {
        "idle" => args.extend(["--if".to_string(), "idle".to_string()]),
        "done" => {}
        "" => bail!("{address} isn't there any more"),
        s => bail!("{address} is {s} now"),
    }
    args.extend(["--".to_string(), address.to_string(), text.to_string()]);
    Ok(args)
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
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/questions/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    /// The permission prompt in `t37`, for `msg` (herdr-core's status line).
    fn perm(msg: &str) -> QuestionSet {
        let screen = questions::parse_screen(&fixture("claude-permission.txt")).unwrap();
        questions::build("t37", "qfix perm", false, &screen, None, msg, "")
    }

    /// Asks that keeps what it would tell the brain (nothing goes to a real session).
    fn asks() -> (Asks, Arc<Mutex<Vec<String>>>) {
        let shared = Shared::new(super::super::state::State::new("ptt", "af_bella", "test"));
        let mut a = Asks::new(shared, Arc::new(Hub::default()), "normal");
        let told = Arc::new(Mutex::new(vec![]));
        let t = told.clone();
        a.brain = Arc::new(move |text: &str| t.lock().unwrap_or_else(|e| e.into_inner()).push(text.to_string()));
        (a, told)
    }

    /// What the brain was told, once its thread has run.
    fn told(t: &Arc<Mutex<Vec<String>>>, n: usize) -> Vec<String> {
        for _ in 0..100 {
            if t.lock().unwrap_or_else(|e| e.into_inner()).len() >= n {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        t.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[test]
    fn the_same_dialog_asked_again_is_told_again() {
        let (a, t) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        a.upsert(set.clone());
        assert_eq!(told(&t, 1).len(), 1, "the same dialog, still up: told once");
        // Answered: the session works again, the set closes.
        a.seen("t37", &Value::Null, "working");
        assert!(a.list().is_empty());
        assert!(a.inner.lock().unwrap_or_else(|e| e.into_inner()).noted.is_empty(), "closed: forgotten");
        // The next "Allow Bash: npm test?" looks the same, and is a new question.
        a.upsert(set.clone());
        assert_eq!(told(&t, 2).len(), 2, "asked again: told again");
        // Another command (the detail differs): another set.
        let mut other = questions::parse_screen(&fixture("claude-permission.txt")).unwrap();
        other.detail = other.detail.replace("touch /tmp/qfix-perm-test", "touch /tmp/other");
        let other = questions::build("t37", "qfix perm", false, &other, None, "approve Bash: touch /tmp/qfix-perm-test", "");
        assert_ne!(other.id, set.id, "the command is part of the id");
        a.upsert(other);
        assert_eq!(told(&t, 3).len(), 3);
    }

    #[test]
    fn a_dismissed_dialog_stays_away_while_it_is_up() {
        let (a, t) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        a.dismiss(&set.id).unwrap();
        a.upsert(set.clone());
        assert!(a.list().is_empty(), "dismissed: not shown again while the session shows it");
        assert_eq!(told(&t, 1).len(), 1);
        a.seen("t37", &Value::Null, "idle");
        a.upsert(set.clone());
        assert_eq!(a.list().len(), 1, "a new episode: shown");
        assert_eq!(told(&t, 2).len(), 2, "and told again");
    }

    #[test]
    fn one_answer_at_a_time() {
        let (a, _) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        let won: Vec<bool> = (0..8)
            .map(|_| {
                let (a, id) = (a.clone(), set.id.clone());
                std::thread::spawn(move || a.start_answering(&id).is_ok())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert_eq!(won.iter().filter(|w| **w).count(), 1, "{won:?}");
        assert_eq!(a.get(&set.id).unwrap().status, "answering");
        assert!(!a.start_answering("q000000000000").unwrap(), "untracked: nothing to mark");
    }

    #[test]
    fn answers_for_the_user_are_checked_again_on_the_screen() {
        let (a, _) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        let pick = |i: usize| vec![Answer { tab: 0, choice: json!(i), attachments: vec![] }];
        assert_eq!(a.recheck(&set, &pick(0)).unwrap(), pick(0));
        assert_eq!(a.recheck(&set, &pick(2)).unwrap(), pick(0), "auto mode: the plain yes instead");
        assert!(a.recheck(&set, &pick(3)).is_err(), "No isn't answered for the user");
        let push = perm("approve Bash: git push origin main");
        assert_eq!(a.recheck(&push, &pick(0)).unwrap_err(), "it pushes or merges to a shared branch");
        a.set_policy("ask-everything");
        assert!(a.recheck(&set, &pick(0)).is_err());
    }

    #[test]
    fn waiting_replies_expire() {
        let r = Reply { set: "q1".into(), text: "yes".into(), at_ms: 1_000, turn: Some(4), last: "working".into() };
        assert_eq!(r.stale(2_000, Some(4), "working"), None, "still on the turn it waits for");
        assert_eq!(r.stale(2_000, Some(5), "idle"), None, "that turn ended: it goes in");
        assert!(r.stale(2_000, Some(5), "working").is_some(), "stopped and started again on its own");
        assert!(r.stale(2_000, Some(6), "idle").is_some(), "a turn of its own since");
        assert!(r.stale(1_000 + REPLY_TTL_MS + 1, Some(4), "idle").is_some(), "too old");
        assert_eq!(Reply { turn: None, ..r.clone() }.stale(2_000, None, "idle"), None);

        let (a, _) = asks();
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("t5".into(), r.clone());
        assert_eq!(a.due_reply("t5", "working", Some(4), 2_000), None);
        assert_eq!(a.due_reply("t5", "idle", Some(5), 2_000), Some(r.clone()));
        assert_eq!(a.due_reply("t5", "idle", Some(5), 2_000), None, "taken once");
        // A dialog on the way (the count goes up for it): still the turn it waits for.
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("t5".into(), r.clone());
        for st in ["blocked", "blocked", "working"] {
            a.reply_sees("t5", st);
        }
        assert_eq!(a.due_reply("t5", "working", Some(5), 2_000), None);
        assert_eq!(a.due_reply("t5", "idle", Some(6), 2_000).map(|r| r.set), Some("q1".into()), "it goes in after the dialog");
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("t5".into(), r.clone());
        a.seen("t5", &Value::Null, "");
        assert!(a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.is_empty(), "the session is gone: dropped");
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("t5".into(), r);
        assert_eq!(a.due_reply("t5", "done", Some(5), 1_000 + REPLY_TTL_MS + 1), None);
        assert!(a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.is_empty(), "too old: dropped");
    }

    #[test]
    fn replies_go_in_only_when_the_session_is_free() {
        assert_eq!(send_args("t3", "- yes", "idle").unwrap(), ["send", "--if", "idle", "--", "t3", "- yes"]);
        assert_eq!(send_args("t3", "-x", "done").unwrap(), ["send", "--", "t3", "-x"]);
        assert!(send_args("t3", "yes", "working").is_err());
        assert!(send_args("t3", "yes", "blocked").is_err());
        assert!(send_args("t3", "yes", "").is_err());
    }

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
