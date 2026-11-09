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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::card::{self, Answerer, CloseWhy, Decision, Ev, Phase, Refused};
use super::lan::hub::Hub;
use super::state::Shared;
use crate::hc::{self, SessionStatus};
use crate::questions::{self, Answer, QuestionSet};

type Notify = Box<dyn Fn(&str, &str) + Send + Sync>;
type Note = Arc<dyn Fn(&str) + Send + Sync>;

/// A queued reply drops after this long (its question is stale by then).
const REPLY_TTL_MS: u64 = 30 * 60 * 1000;

/// A reply to a text question, waiting for its session to stop working.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    last: SessionStatus,
    /// When the session's terminal was made: the reply is for that one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_ms: Option<u64>,
    /// The session's name, to tell the user if it doesn't go in.
    #[serde(default)]
    name: String,
}

impl Reply {
    /// Why it no longer goes in: too old, or the session has stopped and
    /// started again on its own since it was queued (a new turn).
    fn stale(&self, now_ms: u64, turn: Option<u64>, status: &SessionStatus) -> Option<&'static str> {
        if now_ms.saturating_sub(self.at_ms) > REPLY_TTL_MS {
            return Some("it waited too long");
        }
        let (Some(then), Some(now)) = (self.turn, turn) else { return None };
        // One stop is the one it waits for; working again after it, or a second stop, is a turn of its own.
        if now > then + 1 || (now > then && *status == SessionStatus::Working) {
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

/// An answer that comes too late: the question was answered (or closed, or
/// read again) meanwhile. Nothing was typed; the tower doesn't take it to
/// the user as a failure.
#[derive(Debug)]
struct Late(String);

impl std::fmt::Display for Late {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Late {}

/// Closed sets remembered, so a late action on one hears how it ended.
const KEEP_PAST: usize = 100;

/// Why a session's set is over when the session stops showing it.
fn over_why(s: &QuestionSet, gone: bool) -> CloseWhy {
    if gone {
        CloseWhy::SessionGone
    } else if s.state.answered_here() {
        CloseWhy::AnsweredHere
    } else {
        CloseWhy::Unblocked
    }
}

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
    /// Sets that are over (closed, or dismissed while still up), newest
    /// last, in their last state: what a late answer to one is told.
    past: Vec<QuestionSet>,
    /// Sessions whose dialog is being read (the status line it was read
    /// for): one reader per screen.
    reading: HashMap<String, String>,
}

impl Inner {
    fn remember(&mut self, set: QuestionSet) {
        self.past.retain(|p| p.id != set.id);
        self.past.push(set);
        if self.past.len() > KEEP_PAST {
            self.past.remove(0);
        }
    }
}

/// What `questions.json` keeps (in the state folder), so a restart picks
/// the cards up where they were: the open sets in their state (as their
/// wire words), the dialogs dismissed, the replies waiting, and which sets
/// the brain was told about.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    sets: Vec<SavedSet>,
    dismissed: Vec<(String, String)>,
    replies: HashMap<String, Reply>,
    noted: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedSet {
    #[serde(flatten)]
    set: QuestionSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_ms: Option<u64>,
}

impl Saved {
    fn load(path: &std::path::Path) -> Saved {
        let Ok(text) = std::fs::read_to_string(path) else { return Saved::default() };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            let aside = path.with_extension(format!("bad-{}.json", crate::history::now_ms()));
            let _ = std::fs::copy(path, &aside);
            eprintln!("[questions] {} unreadable ({e}); kept as {}", path.display(), aside.display());
            Saved::default()
        })
    }

    fn write(&self, path: &std::path::Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// A session's terminal as Herdr has it, for settling what a restart left.
#[derive(Debug, Clone, PartialEq)]
pub enum Terminal {
    Live { created_ms: Option<u64>, status: SessionStatus },
    /// The terminal is there; its program ended.
    Ended,
    Gone,
    /// hc couldn't say.
    Unknown,
}

impl Terminal {
    fn of(address: &str) -> Terminal {
        match hc::lookup(address) {
            hc::Lookup::Found(t) if t["state"] == "live" => Terminal::Live { created_ms: hc::created_ms(&t), status: hc::status(&t) },
            hc::Lookup::Found(_) => Terminal::Ended,
            hc::Lookup::Gone => Terminal::Gone,
            hc::Lookup::Unreachable(_) => Terminal::Unknown,
        }
    }

    /// Still the terminal `created_ms` says (when both say).
    fn same(&self, created_ms: Option<u64>) -> bool {
        match self {
            Terminal::Live { created_ms: now, .. } => !matches!((created_ms, *now), (Some(a), Some(b)) if a != b),
            Terminal::Unknown => true,
            _ => false,
        }
    }
}

#[derive(Clone)]
pub struct Asks {
    inner: Arc<Mutex<Inner>>,
    /// `questions.json`, when the cards are kept (the daemon's; tests keep none).
    store: Option<Arc<(std::path::PathBuf, Mutex<()>)>>,
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
        let me = Asks { inner: Arc::new(Mutex::new(Inner::default())), store: None, shared, hub, notify: Arc::new(std::sync::OnceLock::new()), brain };
        me.set_policy(policy);
        me
    }

    /// Keeps the cards in `path` from now on (`restore` reads them back).
    pub fn stored_at(mut self, path: std::path::PathBuf) -> Asks {
        self.store = Some(Arc::new((path, Mutex::new(()))));
        self
    }

    pub fn path() -> std::path::PathBuf {
        crate::config::state_dir().join("questions.json")
    }

    /// Writes what a restart needs (every change to the cards ends here).
    fn save(&self) {
        let Some(store) = &self.store else { return };
        let _one = store.1.lock().unwrap_or_else(|e| e.into_inner());
        let saved = {
            let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            Saved {
                sets: i.sets.iter().map(|s| SavedSet { set: s.clone(), created_ms: s.created_ms }).collect(),
                dismissed: i.dismissed.clone(),
                replies: i.replies.clone(),
                noted: i.noted.clone(),
            }
        };
        if let Err(e) = saved.write(&store.0) {
            eprintln!("[questions] can't keep the cards in {}: {e:#}", store.0.display());
        }
    }

    /// At start, before the radar's first look: the cards the last run left,
    /// settled against Herdr. A set whose session still shows it on the same
    /// terminal comes back as it was (decision, recommendations, dismissal),
    /// so the brain isn't told again nor the user asked again; the radar's
    /// first read then keeps it, or replaces it when it reads differently.
    /// The rest is over: dropped quietly. A waiting reply whose session is
    /// still there waits again; otherwise the user hears it didn't go in.
    pub fn restore(&self) {
        let Some(store) = &self.store else { return };
        let saved = Saved::load(&store.0);
        if saved.sets.is_empty() && saved.replies.is_empty() && saved.dismissed.is_empty() {
            return;
        }
        let mut looked: HashMap<String, Terminal> = HashMap::new();
        let mut look = |a: &str| looked.entry(a.to_string()).or_insert_with(|| Terminal::of(a)).clone();
        self.restore_from(saved, &mut look, crate::history::now_ms());
    }

    fn restore_from(&self, saved: Saved, look: &mut dyn FnMut(&str) -> Terminal, now_ms: u64) {
        let mut kept: Vec<QuestionSet> = vec![];
        let mut over: Vec<QuestionSet> = vec![];
        for s in saved.sets {
            let mut set = s.set;
            set.created_ms = s.created_ms;
            set.state = card::SetState::from_wire(&set.status, set.decision.as_deref(), set.error.as_deref());
            card::sync(&mut set);
            let t = look(&set.session);
            let why = match &t {
                _ if !t.same(set.created_ms) => Some(CloseWhy::SessionGone),
                Terminal::Live { status, .. } if set.kind != "text" && *status != SessionStatus::Blocked => Some(over_why(&set, false)),
                Terminal::Live { status, .. } if set.kind == "text" && matches!(status, SessionStatus::Working | SessionStatus::Starting) => {
                    Some(over_why(&set, false))
                }
                _ => None,
            };
            match why {
                Some(why) => {
                    let _ = card::apply(&mut set, &Ev::Close(why));
                    over.push(set);
                }
                None => kept.push(set),
            }
        }
        let mut dropped: Vec<(String, Reply)> = vec![];
        let mut replies = HashMap::new();
        for (a, r) in saved.replies {
            if look(&a).same(r.created_ms) && r.stale(now_ms, None, &SessionStatus::Starting).is_none() {
                replies.insert(a, r);
            } else {
                dropped.push((a, r));
            }
        }
        let dismissed: Vec<(String, String)> =
            saved.dismissed.into_iter().filter(|(a, _)| matches!(look(a), Terminal::Live { status: SessionStatus::Blocked, .. }) || look(a) == Terminal::Unknown).collect();
        eprintln!("[questions] start: {} card(s) back, {} over, {} reply(ies) waiting, {} dropped", kept.len(), over.len(), replies.len(), dropped.len());
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.sets = kept.clone();
            i.replies = replies;
            i.dismissed = dismissed;
            i.noted = saved.noted;
            for o in over {
                i.noted.retain(|n| *n != o.id);
                i.remember(o);
            }
        }
        self.publish();
        for (a, r) in dropped {
            let who = if r.name.is_empty() { a.clone() } else { r.name.clone() };
            eprintln!("[questions] {a}: the waiting reply to {} is dropped at start", r.set);
            self.tell(&a, &format!("Your reply to {who} didn't go in: the tower restarted and {who} isn't there as it was."));
        }
        for set in kept.into_iter().filter(|s| s.kind != "text" && s.state.phase == Phase::Open) {
            match set.state.decision {
                // The brain was told, and may never answer now: the usual wait, from when it was read.
                Decision::Deciding => {
                    let wait = 40_000u64.saturating_sub(now_ms.saturating_sub(set.at_ms)).max(5_000);
                    let (me, id) = (self.clone(), set.id.clone());
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(wait));
                        if me.get(&id).is_some_and(|s| s.state == card::SetState::default()) {
                            me.escalate(&id, "no recommendation came in time");
                        }
                    });
                }
                // The tower was to answer it, and stopped first: not typed after a
                // restart without a look; it goes to the user.
                Decision::Auto => {
                    let (me, id) = (self.clone(), set.id.clone());
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(5));
                        if me.get(&id).is_some_and(|s| s.state.phase == Phase::Open && s.state.decision == Decision::Auto) {
                            me.escalate(&id, "the tower restarted before it could answer it");
                        }
                    });
                }
                Decision::Escalated => {}
            }
        }
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

    /// A set that is over (or dismissed), as it ended.
    fn past(&self, id: &str) -> Option<QuestionSet> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).past.iter().rev().find(|s| s.id == id).cloned()
    }

    fn publish(&self) {
        self.save();
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

    /// Moves the set `id` by `ev` (`card::SetState::next`), and tells
    /// everyone. None: the tower doesn't track it.
    fn transition(&self, id: &str, ev: &Ev) -> Result<Option<QuestionSet>, Refused> {
        let set = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(s) = i.sets.iter_mut().find(|s| s.id == id) else { return Ok(None) };
            card::apply(s, ev)?;
            s.clone()
        };
        self.publish();
        self.event("update", Some(&set), id);
        Ok(Some(set))
    }

    /// A session's status as the radar sees it (Gone or Exited when it is
    /// over). Blocked with a new status line: its dialog is read (on a
    /// thread). Not blocked: its dialog's set closes. Working (or over): a
    /// question its last message asked is stale. Idle or done: a reply
    /// waiting for it goes in.
    pub fn seen(&self, address: &str, t: &Value, status: &SessionStatus) {
        let blocked = *status == SessionStatus::Blocked;
        self.reply_sees(address, status);
        if !blocked {
            let gone = status.ended();
            self.close_where(address, |s| s.kind != "text", |s| over_why(s, gone), true);
            if *status == SessionStatus::Working || gone {
                self.close_where(address, |s| s.kind == "text", |s| over_why(s, gone), true);
            }
            if let Some(r) = self.due_reply(address, status, turn_of(t), hc::created_ms(t), crate::history::now_ms()) {
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
            // Being read for this very screen already.
            if i.reading.get(address) == Some(&msg) {
                return;
            }
            i.msgs.insert(address.to_string(), msg.clone());
            i.reading.insert(address.to_string(), msg.clone());
        }
        let (me, address) = (self.clone(), address.to_string());
        std::thread::spawn(move || {
            // Let the dialog finish drawing.
            std::thread::sleep(std::time::Duration::from_millis(400));
            // Claude Code writes the questions to its transcript a moment after it
            // draws them: until then only the tab on screen is known (the others
            // have no question), and the set's id changes once the rest is. It
            // isn't shown, nor given to the brain, until it is whole (or after a
            // few seconds, as much as there is).
            for _ in 0..10 {
                if !detect(&address).is_some_and(|s| s.kind == "claude-ask" && !s.exact) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(800));
            }
            me.refresh(&address);
            let mut i = me.inner.lock().unwrap_or_else(|e| e.into_inner());
            if i.reading.get(&address) == Some(&msg) {
                i.reading.remove(&address);
            }
        });
    }

    /// A waiting reply's session has `status` now: stopping for a dialog
    /// isn't the turn's end the reply waits for (its count goes up by one).
    fn reply_sees(&self, address: &str, status: &SessionStatus) {
        let changed = match self.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.get_mut(address) {
            Some(r) if r.last != *status => {
                if *status == SessionStatus::Blocked {
                    r.turn = r.turn.map(|n| n + 1);
                }
                r.last = status.clone();
                true
            }
            _ => false,
        };
        if changed {
            self.save();
        }
    }

    /// A reply waiting for `address`, taken when it may go in now (`status`
    /// idle or done). Dropped when the session is gone (or another terminal
    /// is at its address now), it is too old, or the session moved on
    /// without it: logged, and the user, who was told it would go in, hears it didn't.
    fn due_reply(&self, address: &str, status: &SessionStatus, turn: Option<u64>, created_ms: Option<u64>, now_ms: u64) -> Option<Reply> {
        let (taken, dropped) = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let r = i.replies.get(address)?;
            let other = matches!((r.created_ms, created_ms), (Some(a), Some(b)) if a != b);
            let why = if status.ended() || other { Some("the session is gone") } else { r.stale(now_ms, turn, status) };
            match why {
                Some(why) => (None, i.replies.remove(address).map(|r| (r, why))),
                None if status.ready() => (i.replies.remove(address), None),
                None => return None,
            }
        };
        self.save();
        if let Some((r, why)) = dropped {
            eprintln!("[questions] {address}: the waiting reply to {} is dropped: {why}", r.set);
            let who = if r.name.is_empty() { address.to_string() } else { r.name.clone() };
            self.tell(address, &format!("Your reply to {who} didn't go in: {why}"));
        }
        taken
    }

    /// Reads a session's dialog again (after an answer, or a new status line).
    pub fn refresh(&self, address: &str) -> Option<QuestionSet> {
        match detect(address) {
            Some(set) => Some(self.upsert(set)),
            None => {
                let had = self.list().iter().any(|s| s.session == address && s.kind != "text");
                self.close_where(address, |s| s.kind != "text", |s| over_why(s, false), true);
                // Blocked on something it can't read: the plain notice, as before.
                if !had
                    && let Some(t) = hc::get(address).filter(|t| hc::status(t) == SessionStatus::Blocked)
                    && !super::watch::left_alone(&t)
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
        if super::watch::left_alone(t) {
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
            set.created_ms = hc::created_ms(&t);
            me.upsert(set);
        });
    }

    /// A set read from the session: new, the same one again (it keeps its
    /// state), or another in its place (the old one closes: replaced when it
    /// is the same dialog read again). A set being answered isn't replaced:
    /// the answer reads the screen again when its keys are in.
    fn upsert(&self, mut set: QuestionSet) -> QuestionSet {
        let (new, note, closed) = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            // Dismissed while the session still shows it: it stays away.
            if i.dismissed.iter().any(|(a, id)| *a == set.session && *id == set.id) {
                return set;
            }
            let mut old = i.sets.iter().position(|s| s.session == set.session && (s.kind == "text") == (set.kind == "text"));
            let mut closed = None;
            // Another terminal at the address now: the old card went with its session.
            if let Some(k) = old
                && matches!((i.sets[k].created_ms, set.created_ms), (Some(a), Some(b)) if a != b)
            {
                let mut o = i.sets.remove(k);
                let _ = card::apply(&mut o, &Ev::Close(CloseWhy::SessionGone));
                i.noted.retain(|n| *n != o.id);
                i.remember(o.clone());
                closed = Some(o);
                old = None;
            }
            let new = match old {
                Some(k) if i.sets[k].id == set.id => {
                    // The same questions: keep what came for them, and where it stands.
                    let o = &i.sets[k];
                    set.recommendations = o.recommendations.clone();
                    set.reason = o.reason.clone();
                    set.answered_with = o.answered_with.clone();
                    set.replaces = set.replaces.take().or_else(|| o.replaces.clone());
                    set.created_ms = set.created_ms.or(o.created_ms);
                    set.state = o.state.clone();
                    card::sync(&mut set);
                    i.sets[k] = set.clone();
                    false
                }
                Some(k) => {
                    let old = i.sets[k].clone();
                    let redrawn = questions::redrawn(&old, &set);
                    let why = if redrawn { CloseWhy::Replaced { by: set.id.clone() } } else { over_why(&old, false) };
                    let mut over = old.clone();
                    if let Err(e) = card::apply(&mut over, &Ev::Close(why)) {
                        eprintln!("[questions] {} reads {} now, but {} stays until its answer is in ({e})", set.session, set.id, old.id);
                        return old;
                    }
                    if redrawn {
                        // The same dialog read again: its card carries on (the user
                        // decides it still), with what was recommended for the
                        // questions that didn't change.
                        set.replaces = Some(old.id.clone());
                        if old.state.decision == Decision::Escalated {
                            set.state.decision = Decision::Escalated;
                            set.reason = old.reason.clone();
                        }
                        set.recommendations = old.recommendations.as_ref().map(|recs| {
                            recs.iter().filter(|r| matches!((old.tabs.get(r.tab), set.tabs.get(r.tab)), (Some(a), Some(b)) if questions::same_tab(a, b))).cloned().collect()
                        });
                    }
                    card::sync(&mut set);
                    // Other questions now: the old ones may be told again if they come back.
                    i.sets[k] = set.clone();
                    i.noted.retain(|n| *n != old.id);
                    i.remember(over.clone());
                    closed = Some(over);
                    true
                }
                None => {
                    card::sync(&mut set);
                    i.sets.push(set.clone());
                    true
                }
            };
            let note = new && !i.noted.contains(&set.id) && set.state.decision == Decision::Deciding;
            if note {
                i.noted.push(set.id.clone());
                if i.noted.len() > 200 {
                    i.noted.remove(0);
                }
            }
            (new, note, closed)
        };
        self.publish();
        self.event(if new { "open" } else { "update" }, Some(&set), &set.id);
        if let Some(c) = &closed {
            eprintln!("[questions] {} closed: {:?}", c.id, c.state.phase);
            self.event("closed", None, &c.id);
        }
        if note {
            eprintln!("[questions] {} ({}) asks: {} ({} tab(s), id {})", set.session, set.kind, set.title, set.tabs.len(), set.id);
            // The brain recommends answers and decides, quietly.
            let text = if set.kind == "text" { questions::text_note(&set) } else { questions::brain_note(&set) };
            let text = format!("{text} policy={}", self.policy());
            let brain = self.brain.clone();
            std::thread::spawn(move || brain(&text));
        }
        // No word from the brain in time: a newly shown dialog goes to the user as it is.
        if new && set.kind != "text" && set.state.decision == Decision::Deciding {
            let (me, id) = (self.clone(), set.id.clone());
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(40));
                if me.get(&id).is_some_and(|s| s.state == card::SetState::default()) {
                    me.escalate(&id, "no recommendation came in time");
                }
            });
        }
        set
    }

    /// Closes a session's sets that `which` picks, each for the reason `why`
    /// gives. `over`: the session no longer shows them (answered, unblocked,
    /// gone), so the same questions asked again later are new ones (told to
    /// the brain again); without it (the user dismissed a dialog still up)
    /// they stay known.
    fn close_where(&self, address: &str, which: impl Fn(&QuestionSet) -> bool, why: impl Fn(&QuestionSet) -> CloseWhy, over: bool) {
        let gone: Vec<String> = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<String> = i.sets.iter().filter(|s| s.session == address && which(s)).map(|s| s.id.clone()).collect();
            let dialog = i.sets.iter().any(|s| s.session == address && which(s) && s.kind != "text");
            // A dialog's status line is read again next time it blocks.
            if over && (dialog || !i.sets.iter().any(|s| s.session == address)) {
                i.msgs.remove(address);
            }
            let (closing, keep): (Vec<QuestionSet>, Vec<QuestionSet>) = std::mem::take(&mut i.sets).into_iter().partition(|s| s.session == address && which(s));
            i.sets = keep;
            for mut s in closing {
                let w = why(&s);
                // Closing never fails but for a re-read while typing, which isn't closed here.
                let _ = card::apply(&mut s, &Ev::Close(w));
                i.remember(s);
            }
            if over {
                i.noted.retain(|n| !ids.contains(n));
                // Its dialogs are over: dismissed ones may come back as new.
                if dialog || !i.sets.iter().any(|s| s.session == address && s.kind != "text") {
                    let dismissed: Vec<String> = i.dismissed.iter().filter(|(a, _)| a == address).map(|(_, id)| id.clone()).collect();
                    i.noted.retain(|n| !dismissed.contains(n));
                    i.dismissed.retain(|(a, _)| a != address);
                    for p in i.past.iter_mut().filter(|p| dismissed.contains(&p.id) && p.state.phase == Phase::Dismissed) {
                        let _ = card::apply(p, &Ev::Close(CloseWhy::Unblocked));
                    }
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
        let Some(set) = self.get(id) else {
            if let Some(p) = self.past(id) {
                return match &p.state.phase {
                    Phase::Dismissed => Ok(()),
                    Phase::Closed { .. } => Ok(()),
                    _ => bail!("no open question set {id}"),
                };
            }
            bail!("no open question set {id}")
        };
        // A dialog stays on screen until it's answered there: it stays dismissed until then.
        let dialog = set.kind != "text";
        let ev = if dialog { Ev::Dismiss } else { Ev::Close(CloseWhy::Dismissed) };
        if let Err(e) = set.state.next(&ev) {
            bail!("{id} can't be dismissed now: {e}");
        }
        if dialog {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.dismissed.push((set.session.clone(), id.to_string()));
            let mut hidden = set.clone();
            let _ = card::apply(&mut hidden, &Ev::Dismiss);
            i.remember(hidden);
            i.sets.retain(|s| s.id != id);
            drop(i);
            self.publish();
            self.event("closed", None, id);
        } else {
            self.close_where(&set.session, |s| s.id == id, |_| CloseWhy::Dismissed, true);
        }
        Ok(())
    }

    /// `tower recommend <id>`: the brain's three answers per question, and
    /// its decision (`auto` or `escalate`, with why). For a text question
    /// set the brain also says what it asks; `none`: it asks nothing.
    pub fn recommend(&self, id: &str, v: &Value, decision: Option<&str>, reason: &str, none: bool) -> Result<QuestionSet> {
        let Some(set) = self.get(id) else {
            match self.past(id) {
                Some(p) => bail!("{id} is over: {}", late_of(&p)),
                None => bail!("no open question set {id}"),
            }
        };
        if set.state.decision != Decision::Deciding {
            bail!("{id} is decided already ({})", set.decision.unwrap_or_default());
        }
        if none {
            if set.kind != "text" {
                bail!("--none is for a message that turned out to ask nothing; a dialog always asks");
            }
            self.close_where(&set.session, |s| s.id == id, |_| CloseWhy::NothingAsked, true);
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
            card::apply(s, &Ev::Decide { auto: d == "auto" }).map_err(|e| anyhow::anyhow!("{id}: {e}"))?;
            s.reason = why;
            s.clone()
        };
        self.publish();
        self.event("update", Some(&set), id);
        if !matches!(set.state.phase, Phase::Open | Phase::Failed { .. }) {
            // Answered meanwhile (by the user): nothing to answer or announce.
            eprintln!("[questions] {id}: decided {:?}, but it is {} already", set.state.decision, set.status);
        } else if set.state.decision == Decision::Auto {
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
            Ok(now) => {
                let line =
                    format!("answered for you: {picked}, because {}", if set.reason.is_empty() { "it's routine" } else { &set.reason });
                eprintln!("[questions] {}: {line}", set.session);
                let (a, l) = (set.session.clone(), line.clone());
                crate::history::later(move || crate::history::record_session("answered", &a, &l, Some("auto")));
                if let Some(s) = self.inner.lock().unwrap_or_else(|e| e.into_inner()).sets.iter_mut().find(|s| s.id == now.id) {
                    s.answered_with = picked;
                }
                self.publish();
            }
            Err(e) => {
                if let Some(held) = e.downcast_ref::<Held>() {
                    self.escalate(&set.id, &held.0);
                } else if e.downcast_ref::<Late>().is_some() || e.downcast_ref::<Refused>().is_some() {
                    // Answered (or closed) meanwhile: nothing for the user to do.
                    eprintln!("[questions] {}: not answering {id} for the user: {e:#}", set.session);
                } else {
                    self.escalate(&set.id, &format!("answering it didn't work: {e:#}"));
                }
            }
        }
    }

    /// Goes to the user after all (unless it can't: answered, the user typing it).
    fn escalate(&self, id: &str, why: &str) {
        let set = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(s) = i.sets.iter_mut().find(|s| s.id == id) else { return };
            if let Err(e) = card::apply(s, &Ev::Escalate) {
                eprintln!("[questions] {id} isn't escalated ({why}): {e}");
                return;
            }
            s.reason = why.to_string();
            s.clone()
        };
        eprintln!("[questions] {id} goes to the user: {why}");
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

    /// Marks the set `id` as being answered by `by`, unless the table says
    /// no (answers going in already, answered, not the tower's): one lock for
    /// the check and the mark. False: the tower doesn't track it.
    fn start_answering(&self, id: &str, by: Answerer) -> Result<bool, Refused> {
        Ok(self.transition(id, &Ev::Answer { by })?.is_some())
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
        let by = if auto { Answerer::Tower } else { Answerer::User };
        let mut known = self.get(target);
        if let Some(k) = &known {
            // Answered already, going in, not the tower's: said before anything is read.
            if let Err(e) = k.state.next(&Ev::Answer { by }) {
                return Err(match e {
                    Refused::AnsweredAlready => anyhow::Error::new(Late(format!("already answered: {} is answered already", k.id))),
                    e => anyhow::Error::new(e).context(k.id.clone()),
                });
            }
            if k.kind == "text" {
                return self.reply(k, answers);
            }
        } else if let Some(p) = self.past(target) {
            // A card that is over. Read again (or hidden): what the screen
            // shows now may still take these answers. Otherwise it says how it ended.
            match &p.state.phase {
                Phase::Closed { why: CloseWhy::Replaced { .. } } | Phase::Dismissed if p.kind != "text" => known = Some(p),
                _ => bail!(Late(late_of(&p))),
            }
        }
        let address = known.as_ref().map(|s| s.session.clone()).unwrap_or_else(|| target.to_string());
        let Some(mut now) = detect(&address) else {
            if let Some(k) = &known {
                // Answered in the session (or it moved on): its card goes (the one
                // it was read again as too), nothing is typed.
                self.close_where(&address, |s| s.kind != "text", |_| CloseWhy::AnsweredElsewhere, true);
                bail!(Late(CloseWhy::AnsweredElsewhere.late(&k.id, if k.name.is_empty() { &k.session } else { &k.name })));
            }
            if is_set_id(target) {
                // A card's id, no longer open: its dialog was answered (or closed) meanwhile.
                bail!(Late(format!("already answered: {target} isn't open any more (it was answered or closed in the session)")));
            }
            bail!("{address} shows no question it can answer now")
        };
        if let Some(k) = &known
            && k.id != now.id
        {
            if questions::answers_still_fit(k, &now, answers) {
                // The same dialog, read better since (a question it didn't have yet):
                // what was answered is unchanged, so it goes in.
                eprintln!("[questions] {} is {} read again; the answers still fit", k.id, now.id);
                now = self.upsert(now);
                // The tower's own answer carries over to the set it was decided for.
                if auto && now.state.decision == Decision::Deciding {
                    let _ = self.transition(&now.id, &Ev::Decide { auto: true });
                }
            } else {
                // The card shows what the dialog asks now, keeping what still fits.
                let shown = self.upsert(now.clone());
                bail!("the questions changed since they were shown (now {}); the card shows them now: check it and submit again", shown.id);
            }
        }
        let answers = if auto { self.recheck(&now, answers).map_err(Held)? } else { answers.to_vec() };
        // Checked and marked at once: a second answer (the phone, the panel, the tower) waits its turn.
        let tracked = match self.start_answering(&now.id, by) {
            Ok(t) => t,
            Err(Refused::AnsweredAlready) => bail!(Late(format!("already answered: {} is answered already", now.id))),
            Err(e) => return Err(anyhow::Error::new(e).context(now.id.clone())),
        };
        let result = {
            let _typing = hc::typing(&address);
            let mut term = questions::Hc { address: address.clone() };
            questions::drive(&mut term, &now, &answers, submit)
        };
        match result {
            Ok(()) => {
                eprintln!("[questions] answered {} on {address}", now.id);
                if tracked {
                    let ev = if submit || now.submit_state == "single" { Ev::Submitted } else { Ev::Filled };
                    // Closed meanwhile (the dialog went as the keys went in): that's fine.
                    if let Err(e) = self.transition(&now.id, &ev) {
                        eprintln!("[questions] {}: {e}", now.id);
                    }
                }
                // The session moves on: read it again shortly.
                let (me, a) = (self.clone(), address.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    me.refresh(&a);
                });
                Ok(self.get(&now.id).unwrap_or(now))
            }
            Err(stop) => {
                eprintln!("[questions] stopped answering {} on {address}: {stop}", now.id);
                if tracked && let Err(e) = self.transition(&now.id, &Ev::Stopped { error: stop.to_string() }) {
                    eprintln!("[questions] {}: {e}", now.id);
                }
                bail!("stopped at {}", stop)
            }
        }
    }
}

/// What a late action on a set that is over is told.
fn late_of(p: &QuestionSet) -> String {
    let who = if p.name.is_empty() { &p.session } else { &p.name };
    match &p.state.phase {
        Phase::Closed { why } => why.late(&p.id, who),
        Phase::Dismissed => format!("{} was dismissed; answer it in the session, or by its address ({})", p.id, p.session),
        _ => format!("{} isn't open any more", p.id),
    }
}

impl Asks {
    /// A text question's answer: one reply, typed into the session now if it
    /// is idle or done, else when it is.
    fn reply(&self, set: &QuestionSet, answers: &[Answer]) -> Result<QuestionSet> {
        let text = questions::compose_reply(set, answers)?;
        let status = hc::get(&set.session).map(|t| hc::status_live(&t)).unwrap_or(SessionStatus::Gone);
        let queued = !status.ready();
        if queued && (status.ended() || status == SessionStatus::Starting) {
            self.close_where(&set.session, |s| s.id == set.id, |_| CloseWhy::SessionGone, true);
            bail!(Late(CloseWhy::SessionGone.late(&set.id, if set.name.is_empty() { &set.session } else { &set.name })));
        }
        // Checked first: nothing is typed or queued for a set that can't take it.
        let ev = Ev::Reply { queued };
        if let Some(Err(e)) = self.get(&set.id).map(|s| s.state.next(&ev)) {
            return Err(match e {
                Refused::AnsweredAlready => anyhow::Error::new(Late(format!("already answered: {} is answered already", set.id))),
                e => anyhow::Error::new(e).context(set.id.clone()),
            });
        }
        if queued {
            let t = hc::get(&set.session);
            let r = Reply {
                set: set.id.clone(),
                text: text.clone(),
                at_ms: crate::history::now_ms(),
                turn: t.as_ref().and_then(turn_of),
                last: status.clone(),
                created_ms: t.as_ref().and_then(hc::created_ms).or(set.created_ms),
                name: set.name.clone(),
            };
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert(set.session.clone(), r);
        } else {
            type_into(&set.session, &text)?;
        }
        let done = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match i.sets.iter_mut().find(|s| s.id == set.id) {
                Some(s) => {
                    if let Err(e) = card::apply(s, &ev) {
                        eprintln!("[questions] {}: {e}", set.id);
                    }
                    s.answered_with = text.clone();
                    s.clone()
                }
                // Closed meanwhile; the reply went in (or waits) all the same.
                None => {
                    let mut s = set.clone();
                    let _ = card::apply(&mut s, &ev);
                    s.answered_with = text.clone();
                    s
                }
            }
        };
        self.publish();
        self.event("update", Some(&done), &done.id);
        // Answered: the card goes (the reply shows in the session).
        let (me, a, id) = (self.clone(), set.session.clone(), set.id.clone());
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            me.close_where(&a, |s| s.id == id, |_| CloseWhy::AnsweredHere, true);
        });
        Ok(done)
    }
}

/// Types a reply into a session as the user would (no note saying who sent
/// it), only while it is idle or done (read again just before), without
/// control characters, and never at the same time as other keys for it.
fn type_into(address: &str, text: &str) -> Result<()> {
    let _typing = hc::typing(address);
    let status = hc::get(address).map(|t| hc::status_live(&t)).unwrap_or(SessionStatus::Gone);
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
fn send_args(address: &str, text: &str, status: &SessionStatus) -> Result<Vec<String>> {
    let mut args = vec!["send".to_string()];
    match status {
        SessionStatus::Idle => args.extend(["--if".to_string(), "idle".to_string()]),
        SessionStatus::Done => {}
        SessionStatus::Gone | SessionStatus::Exited | SessionStatus::Starting => bail!("{address} isn't there any more"),
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
/// A question set's id (`q` and 12 hex digits, see `questions::id_of`), not a session's address.
fn is_set_id(s: &str) -> bool {
    s.len() == 13 && s.starts_with('q') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

pub fn detect(address: &str) -> Option<QuestionSet> {
    let t = hc::get(address)?;
    if hc::status(&t) != SessionStatus::Blocked {
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
    set.created_ms = hc::created_ms(&t);
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

    /// t41's two-tab dialog: read from the screen only (tab 1 has no question
    /// yet), and read whole once the transcript has it.
    fn gutter_sets() -> (QuestionSet, QuestionSet) {
        let screen = questions::parse_screen(&fixture("claude-ask-gutter-wrapped.txt")).unwrap();
        let opt = |l: &str, i: usize| questions::Opt { label: l.into(), description: String::new(), key: (i + 1).to_string() };
        let tabs = vec![
            questions::Tab {
                title: "Commit".into(),
                question: screen.question.clone(),
                options: ["Commit on a new branch (Recommended)", "Commit on main", "Don't commit"].iter().enumerate().map(|(i, l)| opt(l, i)).collect(),
                allows_other: true,
                ..Default::default()
            },
            questions::Tab {
                title: "Unmute test".into(),
                question: "Your tower is muted. Do you want it run on your real tower?".into(),
                options: ["Yes, unmute and test", "No, leave it muted"].iter().enumerate().map(|(i, l)| opt(l, i)).collect(),
                allows_other: true,
                ..Default::default()
            },
        ];
        (questions::build("t41", "review", false, &screen, None, "", ""), questions::build("t41", "review", false, &screen, Some(tabs), "", ""))
    }

    #[test]
    fn a_dialog_read_again_keeps_its_card_and_what_still_fits() {
        let (a, t) = asks();
        let (half, whole) = gutter_sets();
        assert!(!half.exact && half.tabs[1].question.is_empty() && half.tabs[1].options.is_empty());
        assert_ne!(half.id, whole.id);
        a.upsert(half.clone());
        assert_eq!(told(&t, 1).len(), 1);
        let recs = json!([
            {"tab": 0, "options": [{"label": "Commit on a new branch", "why": "", "choice": 0}]},
            {"tab": 1, "options": [{"label": "Skip", "why": "", "choice": {"other": "Skip"}}]},
        ]);
        let _ = a.recommend(&half.id, &recs, Some("escalate"), "the user decides", false);
        {
            // Recommended for both tabs (the brain may not get to; the rule decided first).
            let mut i = a.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.sets[0].recommendations = Some(questions::parse_recs(&recs, &half).unwrap());
        }
        let before = a.get(&half.id).unwrap();
        assert_eq!(before.decision.as_deref(), Some("escalate"));

        // Read whole: the same dialog, under a new id that replaces the old one.
        let shown = a.upsert(whole.clone());
        assert_eq!(shown.replaces.as_deref(), Some(half.id.as_str()));
        assert_eq!((shown.decision.as_deref(), shown.reason.as_str()), (Some("escalate"), before.reason.as_str()), "still the user's to decide");
        let kept: Vec<usize> = shown.recommendations.iter().flatten().map(|r| r.tab).collect();
        assert_eq!(kept, [0], "what was recommended for a question nobody had read (\"Skip\") goes");
        assert_eq!(a.list().iter().map(|s| s.id.clone()).collect::<Vec<_>>(), [whole.id.clone()], "one card, the new one");
        assert_eq!(told(&t, 2).len(), 1, "the brain isn't asked again for a card that is the user's");

        // Answers to the old card: tab 0's still fit; tab 1's was to a question not shown.
        let ans = |tab: usize, choice: Value| questions::Answer { tab, choice, attachments: vec![] };
        assert!(questions::answers_still_fit(&half, &whole, &[ans(0, json!(0))]));
        assert!(!questions::answers_still_fit(&half, &whole, &[ans(0, json!(0)), ans(1, json!({"other": "Skip"}))]));

        // Another dialog in the same session isn't a redraw: no carrying over.
        let mut other = whole.clone();
        other.tabs[0].title = "Deploy".into();
        other.id = "qother".into();
        let shown = a.upsert(other);
        assert!(shown.replaces.is_none() && shown.decision.is_none());
    }

    #[test]
    fn answering_a_card_whose_dialog_is_gone_closes_it() {
        let (a, _t) = asks();
        let (_, mut whole) = gutter_sets();
        // A session that doesn't exist: no dialog to read now.
        whole.session = "t-qfix-nowhere-0".into();
        a.upsert(whole.clone());
        let e = a.answer(&whole.id, &[questions::Answer { tab: 0, choice: json!(0), attachments: vec![] }], true).unwrap_err();
        assert!(format!("{e:#}").starts_with("already answered"), "{e:#}");
        assert!(a.list().is_empty(), "the card is gone, not left failing");
        // Its id again, once closed (the radar closed it first): still "already answered".
        let e = a.answer(&whole.id, &[questions::Answer { tab: 0, choice: json!(0), attachments: vec![] }], true).unwrap_err();
        assert!(format!("{e:#}").starts_with("already answered"), "{e:#}");
        assert!(is_set_id(&whole.id) && !is_set_id("t4q") && !is_set_id("m5/t3"));
    }

    #[test]
    fn the_same_dialog_asked_again_is_told_again() {
        let (a, t) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        a.upsert(set.clone());
        assert_eq!(told(&t, 1).len(), 1, "the same dialog, still up: told once");
        // Answered: the session works again, the set closes.
        a.seen("t37", &Value::Null, &SessionStatus::parse("working"));
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
        a.seen("t37", &Value::Null, &SessionStatus::parse("idle"));
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
                std::thread::spawn(move || a.start_answering(&id, Answerer::User).is_ok())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert_eq!(won.iter().filter(|w| **w).count(), 1, "{won:?}");
        assert_eq!(a.get(&set.id).unwrap().status, "answering");
        assert!(!a.start_answering("q000000000000", Answerer::User).unwrap(), "untracked: nothing to mark");
    }

    fn phase(a: &Asks, id: &str) -> Phase {
        a.get(id).or_else(|| a.past(id)).map(|s| s.state.phase).unwrap()
    }
    fn ans0() -> Vec<Answer> {
        vec![Answer { tab: 0, choice: json!(0), attachments: vec![] }]
    }

    #[test]
    fn answered_in_the_terminal_closes_the_card_and_a_late_answer_says_so() {
        let (a, _) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        // The user answered it in the terminal: the session works again.
        a.seen("t37", &Value::Null, &SessionStatus::parse("working"));
        assert!(a.list().is_empty());
        assert_eq!(phase(&a, &set.id), Phase::Closed { why: CloseWhy::Unblocked });
        // The phone's card, a moment late: told it was answered, nothing typed (no hc asked).
        let e = a.answer(&set.id, &ans0(), true).unwrap_err();
        assert!(e.downcast_ref::<Late>().is_some() && format!("{e:#}").starts_with("already answered"), "{e:#}");
        assert!(a.dismiss(&set.id).is_ok(), "dismissing what is over is fine");
    }

    #[test]
    fn a_session_that_ends_takes_its_cards() {
        let (a, _) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        let mut text = questions::text_set("t37", "qfix perm", "Done. Should I also push it?", "");
        text.at_ms = 1;
        a.upsert(text.clone());
        assert_eq!(a.list().len(), 2);
        a.seen("t37", &Value::Null, &SessionStatus::Gone);
        assert!(a.list().is_empty());
        assert_eq!(phase(&a, &set.id), Phase::Closed { why: CloseWhy::SessionGone });
        assert_eq!(phase(&a, &text.id), Phase::Closed { why: CloseWhy::SessionGone });
        let e = a.answer(&text.id, &ans0(), true).unwrap_err();
        assert!(format!("{e:#}").contains("isn't there any more"), "{e:#}");
    }

    #[test]
    fn a_card_answered_by_the_tower_closes_as_answered_here() {
        let (a, _) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        assert_eq!(a.start_answering(&set.id, Answerer::User), Ok(true));
        assert_eq!(a.transition(&set.id, &Ev::Submitted).unwrap().unwrap().status, "answered");
        a.seen("t37", &Value::Null, &SessionStatus::parse("working"));
        assert_eq!(phase(&a, &set.id), Phase::Closed { why: CloseWhy::AnsweredHere });
        let e = a.answer(&set.id, &ans0(), true).unwrap_err();
        assert!(format!("{e:#}").starts_with("already answered"), "{e:#}");
    }

    #[test]
    fn a_card_being_answered_isnt_replaced_until_its_keys_are_in() {
        let (a, _) = asks();
        let (mut half, mut whole) = gutter_sets();
        half.session = "t-qfix-nowhere-1".into();
        whole.session = half.session.clone();
        a.upsert(half.clone());
        assert_eq!(a.start_answering(&half.id, Answerer::User), Ok(true));
        // Read whole while the keys go in: the card stays as it is.
        let shown = a.upsert(whole.clone());
        assert_eq!(shown.id, half.id);
        assert_eq!(a.list().iter().map(|s| (s.id.clone(), s.status.clone())).collect::<Vec<_>>(), [(half.id.clone(), "answering".to_string())]);
        // A second answer meanwhile: one at a time.
        assert_eq!(a.start_answering(&half.id, Answerer::User), Err(Refused::AlreadyGoingIn));
        // Filled without submitting: open again, and now the re-read replaces it.
        a.transition(&half.id, &Ev::Filled).unwrap();
        let shown = a.upsert(whole.clone());
        assert_eq!((shown.id.as_str(), shown.replaces.as_deref()), (whole.id.as_str(), Some(half.id.as_str())));
        assert_eq!(phase(&a, &half.id), Phase::Closed { why: CloseWhy::Replaced { by: whole.id.clone() } });
        // An answer to the old card follows it to the screen (no dialog there: answered meanwhile).
        let e = a.answer(&half.id, &ans0(), true).unwrap_err();
        assert!(format!("{e:#}").starts_with("already answered"), "{e:#}");
        assert!(a.list().is_empty(), "the new card closed with it");
    }

    #[test]
    fn the_same_set_read_again_keeps_where_it_stands() {
        let (a, _) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        a.start_answering(&set.id, Answerer::User).unwrap();
        a.transition(&set.id, &Ev::Stopped { error: "the dialog moved".into() }).unwrap();
        let again = a.upsert(set.clone());
        assert_eq!((again.status.as_str(), again.error.as_deref()), ("failed", Some("the dialog moved")), "still failed: try again");
        assert_eq!(a.start_answering(&set.id, Answerer::User), Ok(true), "and it may be tried again");
    }

    #[test]
    fn the_tower_answers_only_what_it_decided_and_never_over_the_user() {
        let (a, _) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        assert_eq!(a.start_answering(&set.id, Answerer::Tower), Err(Refused::NotTheTowers));
        // The user is typing; the tower's timeout (or a failed auto answer) can't take it away.
        a.start_answering(&set.id, Answerer::User).unwrap();
        a.escalate(&set.id, "no recommendation came in time");
        let s = a.get(&set.id).unwrap();
        assert_eq!((s.status.as_str(), s.decision.as_deref()), ("answering", None));
        assert!(a.dismiss(&set.id).is_err(), "not hidden while its keys go in");
        // Decided while the user answered: the decision is kept, nothing else happens.
        a.transition(&set.id, &Ev::Submitted).unwrap();
        let recs = json!([{"tab": 0, "options": [{"label": "Yes", "why": "", "choice": 0}]}]);
        let s = a.recommend(&set.id, &recs, Some("auto"), "routine", false).unwrap();
        assert_eq!((s.status.as_str(), s.decision.as_deref()), ("answered", Some("auto")));
        assert!(a.recommend(&set.id, &recs, Some("auto"), "again", false).is_err(), "decided once");
    }

    #[test]
    fn a_dismissed_dialog_closes_when_it_goes() {
        let (a, _) = asks();
        let set = perm("approve Bash: touch /tmp/qfix-perm-test");
        a.upsert(set.clone());
        a.dismiss(&set.id).unwrap();
        assert_eq!(phase(&a, &set.id), Phase::Dismissed);
        a.seen("t37", &Value::Null, &SessionStatus::parse("idle"));
        assert_eq!(phase(&a, &set.id), Phase::Closed { why: CloseWhy::Unblocked });
    }

    #[test]
    fn another_terminal_at_the_address_is_another_card() {
        let (a, t) = asks();
        let mut set = perm("approve Bash: touch /tmp/qfix-perm-test");
        set.created_ms = Some(100);
        a.upsert(set.clone());
        a.start_answering(&set.id, Answerer::User).unwrap();
        a.transition(&set.id, &Ev::Submitted).unwrap();
        // The same dialog (same id) in a new terminal at t37: new, not "answered".
        let mut again = set.clone();
        again.created_ms = Some(200);
        let shown = a.upsert(again);
        assert_eq!((shown.status.as_str(), shown.created_ms), ("open", Some(200)));
        assert_eq!(told(&t, 2).len(), 2, "told to the brain as the new question it is");
    }

    struct Tmp(std::path::PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("tower-asks-{tag}-{}-{}", std::process::id(), crate::history::now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    fn stored(path: &std::path::Path) -> (Asks, Arc<Mutex<Vec<String>>>) {
        let (a, t) = asks();
        (a.stored_at(path.to_path_buf()), t)
    }

    #[test]
    fn a_restart_picks_the_cards_up_where_they_were() {
        let dir = tmp("restart");
        let path = dir.0.join("questions.json");
        let now = crate::history::now_ms();
        let live = |c: u64, st: &str| Terminal::Live { created_ms: Some(c), status: SessionStatus::parse(st) };
        let with = |mut s: QuestionSet, session: &str| {
            s.session = session.into();
            s.created_ms = Some(1);
            s.at_ms = now;
            s
        };
        {
            let (a, t) = stored(&path);
            // t37: told to the brain, decided: the user's.
            let p = with(perm("approve Bash: touch /tmp/qfix-perm-test"), "t37");
            a.upsert(p.clone());
            let recs = json!([{"tab": 0, "options": [{"label": "Yes", "why": "", "choice": 0}]}]);
            a.recommend(&p.id, &recs, Some("escalate"), "the user decides", false).unwrap();
            // t38: dismissed while it is up.
            let d = with(perm("approve Bash: rm -rf /tmp/qfix-x"), "t38");
            a.upsert(d.clone());
            a.dismiss(&d.id).unwrap();
            // t39: a reply waiting for the session to stop; t40: one for a terminal that will be another.
            for (addr, created) in [("t39", 1), ("t40", 1)] {
                a.inner.lock().unwrap().replies.insert(
                    addr.into(),
                    Reply { set: format!("q-{addr}"), text: "yes".into(), at_ms: now, turn: Some(3), last: SessionStatus::Working, created_ms: Some(created), name: format!("{addr} name") },
                );
            }
            // t41: open when the tower stopped, idle by its start (answered in the terminal meanwhile).
            a.upsert(with(perm("approve Bash: ls /tmp/qfix"), "t41"));
            // t42: its session ended meanwhile; t43: keys going in when the tower stopped.
            a.upsert(with(perm("approve Bash: ls /tmp/qfix-2"), "t42"));
            let typing = with(perm("approve Bash: ls /tmp/qfix-3"), "t43");
            a.upsert(typing.clone());
            a.start_answering(&typing.id, Answerer::User).unwrap();
            assert_eq!(told(&t, 5).len(), 5);
        }
        let (b, t) = stored(&path);
        let heard = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let h = heard.clone();
        b.set_notify(move |a, text| h.lock().unwrap().push((a.to_string(), text.to_string())));
        let mut look = |a: &str| match a {
            "t37" | "t38" | "t43" => live(1, "blocked"),
            "t39" => live(1, "working"),
            "t40" => live(2, "idle"),
            "t41" => live(1, "idle"),
            _ => Terminal::Gone,
        };
        b.restore_from(Saved::load(&path), &mut look, now);
        let cards: Vec<(String, String, Option<String>)> = b.list().iter().map(|s| (s.session.clone(), s.status.clone(), s.decision.clone())).collect();
        assert_eq!(
            cards,
            [("t37".into(), "open".into(), Some("escalate".into())), ("t43".into(), "failed".into(), None)],
            "the user's card comes back decided; the one being typed failed part-way"
        );
        let p = b.list()[0].clone();
        assert_eq!(p.reason, "the user decides");
        assert!(p.recommendations.is_some());
        // The radar's first look reads the same dialogs again: nothing is told or asked twice.
        b.upsert(with(perm("approve Bash: touch /tmp/qfix-perm-test"), "t37"));
        b.upsert(with(perm("approve Bash: rm -rf /tmp/qfix-x"), "t38"));
        assert_eq!(b.list().len(), 2, "the dismissed dialog stays away");
        assert!(told(&t, 1).is_empty(), "the brain isn't told again");
        // Over while the tower was down.
        let over = |addr: &str| b.inner.lock().unwrap().past.iter().find(|s| s.session == addr).map(|s| s.state.phase.clone());
        assert_eq!(over("t41"), Some(Phase::Closed { why: CloseWhy::Unblocked }));
        assert_eq!(over("t42"), Some(Phase::Closed { why: CloseWhy::SessionGone }));
        let replies: Vec<String> = b.inner.lock().unwrap().replies.keys().cloned().collect();
        assert_eq!(replies, ["t39"], "the reply for a terminal that is another one now is dropped");
        let heard = heard.lock().unwrap().clone();
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert_eq!(heard[0].0, "t40");
        assert!(heard[0].1.starts_with("Your reply to t40 name didn't go in"), "the user was told it would go in: {heard:?}");
        // Saved again as it is now: a second restart sees the same.
        let again = Saved::load(&path);
        assert_eq!(again.sets.len(), 2);
        assert_eq!(again.replies.len(), 1);
    }

    #[test]
    fn a_broken_or_old_file_never_stops_the_start() {
        let dir = tmp("broken");
        let path = dir.0.join("questions.json");
        std::fs::write(&path, "{ not json").unwrap();
        let (a, _) = stored(&path);
        a.restore_from(Saved::load(&path), &mut |_| Terminal::Unknown, 1);
        assert!(a.list().is_empty());
        assert!(std::fs::read_dir(&dir.0).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("bad-")), "kept aside");
        // Words this tower doesn't know, and fields it doesn't have: loaded as open, undecided.
        std::fs::write(&path, r#"{"sets": [{"id": "q1", "session": "t9", "kind": "claude-permission", "status": "pondering", "decision": "later", "future": 1}], "extra": true}"#).unwrap();
        a.restore_from(Saved::load(&path), &mut |_| Terminal::Live { created_ms: None, status: SessionStatus::Blocked }, 1);
        let s = a.get("q1").unwrap();
        assert_eq!((s.status.as_str(), s.decision.as_deref()), ("open", None));
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
        let r = Reply { set: "q1".into(), text: "yes".into(), at_ms: 1_000, turn: Some(4), last: SessionStatus::Working, created_ms: None, name: String::new() };
        assert_eq!(r.stale(2_000, Some(4), &SessionStatus::parse("working")), None, "still on the turn it waits for");
        assert_eq!(r.stale(2_000, Some(5), &SessionStatus::parse("idle")), None, "that turn ended: it goes in");
        assert!(r.stale(2_000, Some(5), &SessionStatus::parse("working")).is_some(), "stopped and started again on its own");
        assert!(r.stale(2_000, Some(6), &SessionStatus::parse("idle")).is_some(), "a turn of its own since");
        assert!(r.stale(1_000 + REPLY_TTL_MS + 1, Some(4), &SessionStatus::Idle).is_some(), "too old");
        assert_eq!(Reply { turn: None, ..r.clone() }.stale(2_000, None, &SessionStatus::Idle), None);

        let (a, _) = asks();
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("t5".into(), r.clone());
        assert_eq!(a.due_reply("t5", &SessionStatus::parse("working"), Some(4), None, 2_000), None);
        assert_eq!(a.due_reply("t5", &SessionStatus::parse("idle"), Some(5), None, 2_000), Some(r.clone()));
        assert_eq!(a.due_reply("t5", &SessionStatus::parse("idle"), Some(5), None, 2_000), None, "taken once");
        // A dialog on the way (the count goes up for it): still the turn it waits for.
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("t5".into(), r.clone());
        for st in ["blocked", "blocked", "working"] {
            a.reply_sees("t5", &SessionStatus::parse(st));
        }
        assert_eq!(a.due_reply("t5", &SessionStatus::parse("working"), Some(5), None, 2_000), None);
        assert_eq!(a.due_reply("t5", &SessionStatus::parse("idle"), Some(6), None, 2_000).map(|r| r.set), Some("q1".into()), "it goes in after the dialog");
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("t5".into(), r.clone());
        a.seen("t5", &Value::Null, &SessionStatus::Gone);
        assert!(a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.is_empty(), "the session is gone: dropped");
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("t5".into(), r);
        assert_eq!(a.due_reply("t5", &SessionStatus::parse("done"), Some(5), None, 1_000 + REPLY_TTL_MS + 1), None);
        assert!(a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.is_empty(), "too old: dropped");
    }

    #[test]
    fn replies_go_in_only_when_the_session_is_free() {
        assert_eq!(send_args("t3", "- yes", &SessionStatus::parse("idle")).unwrap(), ["send", "--if", "idle", "--", "t3", "- yes"]);
        assert_eq!(send_args("t3", "-x", &SessionStatus::parse("done")).unwrap(), ["send", "--", "t3", "-x"]);
        assert!(send_args("t3", "yes", &SessionStatus::parse("working")).is_err());
        assert!(send_args("t3", "yes", &SessionStatus::parse("blocked")).is_err());
        assert!(send_args("t3", "yes", &SessionStatus::parse("")).is_err());
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
