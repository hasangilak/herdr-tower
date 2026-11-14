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
//!
//! A set that is the user's is a decision (`decisions`): the same thing
//! asked by several sessions of a space is one, told once.

use std::collections::{HashMap, HashSet};
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
/// A session's screen as it is now (None: it couldn't be read).
type Screen = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
/// Writes a line in the history.
type Record = Arc<dyn Fn(crate::history::Entry) + Send + Sync>;
/// Whether what the user typed in a session (its name, their words) answers
/// what the reply the tower holds for it answers (that reply): then the
/// reply is moot (`covered::answers_held`; tests give their own).
type Covers = Arc<dyn Fn(&str, &str, &str) -> Result<bool> + Send + Sync>;
/// A card's own buttons, read from what its session said (`crate::choices`):
/// (its name, its message, what it waits for).
type Choices = Arc<dyn Fn(&str, &str, &str) -> Result<Vec<questions::Rec>> + Send + Sync>;
/// Whether a message the tower passed on answers a question: (the session's
/// name, what it asked, the user's words to the tower, the message).
type Relays = Arc<dyn Fn(&str, &str, &str, &str) -> Result<bool> + Send + Sync>;

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

/// The user is answering the dialog in its terminal (a tab is answered
/// there, or it is on another tab than it opened on): the tower types
/// nothing of its own into it, and it is the user's card, quietly.
#[derive(Debug)]
struct Busy(String);

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Busy {}

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

/// Sets answered in their session's own terminal are remembered this long:
/// a prompt hook that comes after the radar finds its set, and a screen read
/// before the answer can't bring the card back.
const THERE_KEEP_MS: u64 = 60_000;

/// A set its session stopped showing without the tower answering it: the
/// user answered it in the session's own terminal (for a text question, the
/// prompt hook says whether it was the user who went on, and with what
/// words). Its card and its decision went at once; its line in the record
/// waits a moment for what was chosen.
#[derive(Debug, Clone)]
struct There {
    set: QuestionSet,
    /// The decision it stood in, when it was the user's.
    decision: Option<String>,
    /// That decision closed with it (nobody else asks the same).
    over: bool,
    closed_ms: u64,
    /// What the user typed there: a text question's answer.
    words: Option<String>,
    /// The next prompt was the tower's (or another terminal's), not the user's.
    not_users: bool,
    /// Its line is in the record.
    recorded: bool,
}

/// How long the record line of a set answered in its terminal waits: for
/// its screen to show what was chosen, and for a text question's words to
/// come with the prompt hook.
#[derive(Debug, Clone, Copy)]
struct Waits {
    screen_ms: u64,
    words_ms: u64,
}

impl Default for Waits {
    fn default() -> Waits {
        Waits { screen_ms: 700, words_ms: 6_000 }
    }
}

/// A prompt this soon after the tower typed into a session is taken to be
/// the tower's, for a question it asked at the end of its turn.
const TYPED_MS: u64 = 20_000;

/// A session working again while its question at the end of a turn is open.
#[derive(Debug, Clone, Copy, PartialEq)]
enum WentOn {
    /// On a prompt that isn't the user's: the question stays open.
    Kept,
    /// Whose prompt it is, its hook is still to say (since when it was seen working).
    Waiting(u64),
}

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
    /// Sets whose buttons were given (`give_buttons`): once each.
    buttoned: HashSet<String>,
    /// Dialogs the user dismissed (session, id): not shown again while the
    /// session still shows them.
    dismissed: Vec<(String, String)>,
    /// Sets that are over (closed, or dismissed while still up), newest
    /// last, in their last state: what a late answer to one is told.
    past: Vec<QuestionSet>,
    /// Sessions whose dialog is being read (the status line it was read
    /// for): one reader per screen.
    reading: HashMap<String, String>,
    /// Sets answered in their terminal lately (`There`).
    there: Vec<There>,
    /// What the user typed in a session's terminal while its text question
    /// was open, by session (and when): that question's answer.
    typed_words: HashMap<String, (String, u64)>,
    /// Sessions working again with a question at the end of a turn open, by
    /// session, until they stop: on a prompt that isn't the user's, or one
    /// whose hook is still to say whose it is (`Asks::went_on`).
    went_on: HashMap<String, WentOn>,
    /// The latest prompt each session's hook said was not the user's: when,
    /// and at which turn count.
    not_users: HashMap<String, (u64, Option<u64>)>,
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
    /// Kept by a tower that makes no card of a turn that only says its
    /// session is ready (false: an older one's file, looked through once).
    ready_checked: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedSet {
    #[serde(flatten)]
    set: QuestionSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    space: Option<String>,
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
    /// What needs the user, one decision per key: the sets that are theirs
    /// go there (none: tests that keep no decisions).
    decisions: Arc<std::sync::OnceLock<super::decisions::Decisions>>,
    /// A session's screen as it is now (`hc read`); tests give their own.
    screen: Screen,
    /// Writes a line in the history (tests keep what it would have been).
    record: Record,
    waits: Waits,
    covers: Covers,
    /// The buttons of a card that is the user's with nothing to tap (`give_buttons`).
    choices: Choices,
    /// Whether a message a brain passed on answers a session's question (`relayed`).
    relays: Relays,
    /// Answers a set the tower decided to answer itself (`auto_answer`,
    /// in a thread of its own); tests keep which ones it would have.
    answers: Auto,
}

/// How the tower goes on to answer a set it decided to answer itself.
type Auto = Arc<dyn Fn(&Asks, &str) + Send + Sync>;

/// Why the tower said yes itself to a prompt that only writes Claude's memory.
pub const MEMORY_YES: &str = "it only writes Claude's own memory, notes or files in its own folder (a plain yes, this once)";

impl Asks {
    pub fn new(shared: Shared, hub: Arc<Hub>, policy: &str) -> Asks {
        // To the brain of the asking session's space, when it has one (the note names the session).
        let brain: Note = Arc::new(|text: &str| {
            if let Err(e) = super::brains::note_about(text, false) {
                eprintln!("[questions] the brain can't take it: {e:#}");
            }
        });
        let me = Asks {
            inner: Arc::new(Mutex::new(Inner::default())),
            store: None,
            shared,
            hub,
            notify: Arc::new(std::sync::OnceLock::new()),
            brain,
            decisions: Arc::default(),
            screen: Arc::new(|address: &str| hc::run(&["read", address, "--lines", "60"]).ok().filter(|o| o.code == 0).map(|o| o.stdout)),
            record: Arc::new(|e: crate::history::Entry| {
                // Tests never write the history.
                if !cfg!(test) {
                    crate::history::later(move || crate::history::append(e));
                }
            }),
            waits: Waits::default(),
            covers: Arc::new(crate::covered::answers_held),
            // (Tests never ask the model: they give their own, else the generic buttons stay.)
            choices: if cfg!(test) { Arc::new(|_: &str, _: &str, _: &str| -> Result<Vec<questions::Rec>> { bail!("no model in tests") }) } else { Arc::new(crate::choices::derive) },
            relays: Arc::new(crate::covered::answers_question),
            answers: Arc::new(|a: &Asks, id: &str| {
                let (me, id) = (a.clone(), id.to_string());
                std::thread::spawn(move || me.auto_answer(&id));
            }),
        };
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

    /// Tells no brain anything (tests outside this file: nothing reaches a real session).
    #[cfg(test)]
    pub fn telling_no_brain(mut self) -> Asks {
        self.brain = Arc::new(|_: &str| ());
        self
    }

    /// The sets that are the user's become decisions there, from now on.
    pub fn with_decisions(self, decisions: super::decisions::Decisions) -> Asks {
        let _ = self.decisions.set(decisions);
        self
    }

    /// Whether the user is told of a set that is theirs now. It is one
    /// decision per key: told when the decision first appears, and the same
    /// thing asked again says nothing; unless the tower tells everything,
    /// when it is told as it was before (`loud`). Never from a muted space.
    fn tells(&self, set: &str, loud: bool) -> bool {
        match self.decisions.get() {
            Some(d) => d.tells(&d.for_question(set), loud),
            None => loud,
        }
    }

    /// Writes what a restart needs (every change to the cards ends here).
    fn save(&self) {
        let Some(store) = &self.store else { return };
        let _one = store.1.lock().unwrap_or_else(|e| e.into_inner());
        let saved = {
            let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            Saved {
                sets: i.sets.iter().map(|s| SavedSet { set: s.clone(), created_ms: s.created_ms, space: s.space.clone() }).collect(),
                dismissed: i.dismissed.clone(),
                replies: i.replies.clone(),
                noted: i.noted.clone(),
                ready_checked: true,
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
            // No card is left: a decision that stood for one is over.
            self.publish();
            return;
        }
        let mut looked: HashMap<String, Terminal> = HashMap::new();
        let mut look = |a: &str| looked.entry(a.to_string()).or_insert_with(|| Terminal::of(a)).clone();
        // What was asked of a session, as the ledger's file says (the queue and the ledger start after this).
        let now = crate::history::now_ms();
        let ledger = crate::agenda::Ledger::load_from(&crate::agenda::path());
        let asked = |a: &str| ledger.at_work_on(a, now, 60_000).is_some_and(|t| !questions::ready_only(&t.instruction));
        self.restore_from(saved, &mut look, &asked, now);
    }

    /// `asked`: something was asked of that session (`has_work`, as far as the files say).
    fn restore_from(&self, saved: Saved, look: &mut dyn FnMut(&str) -> Terminal, asked: &dyn Fn(&str) -> bool, now_ms: u64) {
        let mut kept: Vec<QuestionSet> = vec![];
        let mut over: Vec<QuestionSet> = vec![];
        // An older tower made a "waits for you" card of a turn that only said its session was ready.
        // This one never does, so its own cards are never looked through for that.
        let older = !saved.ready_checked;
        for s in saved.sets {
            let mut set = s.set;
            set.created_ms = s.created_ms;
            set.space = s.space;
            set.state = card::SetState::from_wire(&set.status, set.decision.as_deref(), set.error.as_deref());
            card::sync(&mut set);
            let t = look(&set.session);
            let why = match &t {
                _ if !t.same(set.created_ms) => Some(CloseWhy::SessionGone),
                // Idle is not blocked: a card that only said its session was ready (an older tower made those).
                // By the whole message it kept, not the one sentence in its title: "The build failed.
                // I'm waiting for your instructions." is a real wait.
                _ if older && set.reason == questions::WAITS && questions::ready_only(if set.detail.trim().is_empty() { &set.title } else { &set.detail }) && !asked(&set.session) => {
                    Some(CloseWhy::NothingAsked)
                }
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

    /// The session waits for the user to act (an open wait card: `waiting`).
    pub fn waits_on(&self, address: &str) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).sets.iter().any(|s| s.session == address && s.reason == questions::WAITS)
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
        let (sets, past) = {
            let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let past: Vec<(String, crate::decisions::State)> = i.past.iter().map(|p| (p.id.clone(), ended_as(p))).collect();
            (i.sets.clone(), past)
        };
        let by_session: HashMap<String, String> = sets.iter().map(|s| (s.session.clone(), s.id.clone())).collect();
        self.shared.update(|st| {
            st.questions = sets.iter().map(|s| serde_json::to_value(s).unwrap_or_default()).collect();
            for a in st.agents.iter_mut() {
                a.question = by_session.get(&a.address).cloned();
            }
        });
        // The sets that are the user's, as decisions: one per key.
        if let Some(d) = self.decisions.get() {
            d.sync_questions(&sets, &|id| past.iter().rev().find(|(p, _)| p == id).map(|(_, s)| *s).unwrap_or(crate::decisions::State::Stopped));
        }
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
            if gone {
                self.inner.lock().unwrap_or_else(|e| e.into_inner()).went_on.remove(address);
                self.close_where(address, |s| s.kind == "text", |s| over_why(s, gone), true);
            } else if *status == SessionStatus::Working {
                self.went_on(address, turn_of(t));
            } else {
                // It stopped: when it works again, that is looked at anew.
                self.inner.lock().unwrap_or_else(|e| e.into_inner()).went_on.remove(address);
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

    /// The session works again while a question it asked at the end of its
    /// turn is open. One answered here (its reply is what went in) is over.
    /// Any other stays the user's unless the prompt it works on is theirs:
    /// one the tower typed (an instruction, a message from another session,
    /// an introduction, also one typed in while it worked and sent as it
    /// stopped), another terminal's, or the session's own (its background
    /// work reporting back) leaves it open, and its decision with it; the
    /// user's words close it, with them (`typed_there`). The prompt hook says
    /// whose it is, and the radar can be first: it is waited for a moment,
    /// and with no word from it the question was answered in the terminal,
    /// as before.
    fn went_on(&self, address: &str, turn: Option<u64>) {
        self.close_where(address, |s| s.kind == "text" && s.state.answered_here(), |s| over_why(s, false), true);
        if !self.list().iter().any(|s| s.session == address && s.kind == "text") {
            return;
        }
        let seen_ms = crate::history::now_ms();
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if i.went_on.contains_key(address) {
                return;
            }
            if not_the_users(&i, address, turn, seen_ms) {
                i.went_on.insert(address.to_string(), WentOn::Kept);
                eprintln!("[questions] {address} works on a prompt that isn't the user's: its question stays open");
                return;
            }
            i.went_on.insert(address.to_string(), WentOn::Waiting(seen_ms));
        }
        let (me, address) = (self.clone(), address.to_string());
        std::thread::spawn(move || {
            let until = seen_ms + me.waits.words_ms;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let open = me.list().iter().any(|s| s.session == address && s.kind == "text" && s.at_ms <= seen_ms);
                let mut i = me.inner.lock().unwrap_or_else(|e| e.into_inner());
                let mine = i.went_on.get(&address) == Some(&WentOn::Waiting(seen_ms));
                if i.went_on.get(&address) == Some(&WentOn::Kept) || (!mine && i.went_on.contains_key(&address)) {
                    return;
                }
                if !open {
                    if mine {
                        i.went_on.remove(&address);
                    }
                    return;
                }
                let now = crate::history::now_ms();
                if not_the_users(&i, &address, turn, now) {
                    i.went_on.insert(address.clone(), WentOn::Kept);
                    eprintln!("[questions] {address} works on a prompt that isn't the user's: its question stays open");
                    return;
                }
                if now >= until {
                    if mine {
                        i.went_on.remove(&address);
                    }
                    drop(i);
                    // No word whose prompt it was: the session went on, so it was answered there.
                    me.close_where(&address, |s| s.kind == "text" && s.at_ms <= seen_ms, |s| over_why(s, false), true);
                    return;
                }
            }
        });
    }

    /// From now on (the daemon's): every open tabbed dialog is looked at
    /// again about every 1.5 seconds while its session shows it. The user
    /// may be answering it in the terminal, tab by tab, which changes nothing
    /// in Herdr's status: when its screen shows another tab, or other tabs
    /// answered, it is read again whole, and its card (the Mac's, the
    /// phone's, its decision's) follows.
    pub fn follow(&self) {
        let me = self.clone();
        let _ = std::thread::Builder::new().name("questions-follow".into()).spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(1_500));
                me.look_again();
            }
        });
    }

    /// One look at every open tabbed dialog (one `hc read` each).
    fn look_again(&self) {
        let open: Vec<QuestionSet> = {
            let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.sets
                .iter()
                // Being typed into (the driver reads it itself), or being read: not now.
                .filter(|s| s.kind == "claude-ask" && !s.session.contains('/') && matches!(s.state.phase, Phase::Open | Phase::Failed { .. }))
                .filter(|s| !i.reading.contains_key(&s.session))
                .cloned()
                .collect()
        };
        for set in open {
            let Ok(out) = hc::run(&["read", &set.session]) else { continue };
            let Some(screen) = questions::parse_screen(&out.stdout) else { continue };
            if questions::dialog_moved(&set, &screen) {
                eprintln!("[questions] {} ({}): its dialog moved on in the terminal; reading it again", set.id, set.session);
                self.refresh(&set.session);
            }
        }
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
                let blocked = hc::get(address).filter(|t| hc::status(t) == SessionStatus::Blocked);
                // Still blocked, on a screen it can't read now: not answered, only out of sight.
                let still = blocked.is_some();
                self.close_where(address, |s| s.kind != "text", |s| if still && !s.state.answered_here() { CloseWhy::Unread } else { over_why(s, false) }, true);
                // Blocked on something it can't read: a decision of its own (on
                // the orb's count, in the queue, with Herdr one tap away), told
                // as the plain notice was: once at a quiet level.
                if !had
                    && let Some(t) = blocked
                    && !super::watch::left_alone(&t)
                {
                    let notice = blocked_notice(&hc::spoken_name(&t), &t["status"]);
                    let told = match self.decisions.get() {
                        Some(d) => {
                            let who = crate::decisions::Who::new(address, &hc::spoken_name(&t));
                            d.blocked(crate::spaces::space_of(&t), who, &blocked_title(&t["status"]))
                        }
                        None => true,
                    };
                    if told {
                        self.tell(address, &notice);
                    }
                }
                None
            }
        }
    }

    /// A session finished a turn (`working` to idle or done): if its last
    /// message waits for the user to do something (merge, approve, test,
    /// decide), it needs them as a blocked session does (`waiting`); if it
    /// seems to ask something, a text question set, for the brain to read.
    /// `reported`: sessions whose watch's report told the brain of their
    /// wait (it says it then); looked at once the turn's message is in.
    pub fn finished(&self, address: &str, t: &Value, reported: Arc<Mutex<std::collections::HashSet<String>>>) {
        if super::watch::left_alone(t) {
            return;
        }
        // A PR's reviewer (`reviews`): it talks to the PR's owner itself, and
        // what it can't agree on comes to the user as the review's own
        // decision. Its turn's end opens no PR, card or question.
        if super::reviews::is_reviewer(address) || super::reviews::labelled_reviewer(t) {
            return;
        }
        let (me, address, t) = (self.clone(), address.to_string(), t.clone());
        let ended_ms = crate::history::now_ms();
        std::thread::spawn(move || {
            let Some(message) = turn_message(&address, &t, ended_ms) else { return };
            // A turn the user started in its own terminal: the user is in that conversation.
            let own = super::typed::users_turn(&address);
            let context = crate::history::jobs(&crate::history::load()).into_iter().find(|j| j.session == address).map(|j| j.title).unwrap_or_default();
            let waits = questions::waits_for_user(&message);
            let watched = reported.lock().unwrap_or_else(|e| e.into_inner()).contains(&address);
            let (name, space) = (hc::spoken_name(&t), crate::spaces::space_of(&t));
            // A merge on hold whose PR it names: the session posted something new about it.
            if let Some(d) = me.decisions.get() {
                d.posted(space.as_deref(), &address, &name, &message);
            }
            // It only says it is ready: that hands the user nothing.
            let ready = questions::ready_only(&message);
            // PRs it opened (or ends on): chips in the chat, waiting for the user's merge.
            let found = crate::prs::links(&format!("{message}\n{}", turn_transcript(&address, &t)));
            if !found.is_empty()
                && let Some(store) = crate::prs::daemon()
            {
                let open = store.found(&found, &address, &name, space.as_deref(), true);
                // Its wait is the merge (or it just ended on the PR): the chip is what the user acts on.
                let on_merge = waits.as_deref().filter(|_| !ready).is_none_or(|r| r.to_lowercase().contains("merg"));
                if !open.is_empty() && on_merge {
                    // One merge decision per repo and space: said when it first appears.
                    let tells = match me.decisions.get() {
                        Some(d) => d.tells_merge(&open, !watched),
                        None => !watched,
                    };
                    if tells {
                        me.tell(&address, &pr_line(&name, &open));
                    }
                    return;
                }
            }
            // Idle is not blocked: no card, no alert, no decision.
            if idle_ready(&address, &message) {
                eprintln!("[questions] {address} ({name}) is ready: it only says it waits for instructions, and nothing was asked of it");
                me.shared.record(&address, &format!("{name} is ready"), "ready");
                return;
            }
            if let Some(request) = waits {
                me.waiting_in(space, &address, &name, hc::created_ms(&t), &message, &request, &context, watched, own);
                return;
            }
            if !questions::looks_like_question(&message) {
                return;
            }
            let mut set = questions::text_set(&address, &name, &message, &context);
            set.at_ms = crate::history::now_ms();
            set.created_ms = hc::created_ms(&t);
            set.space = space;
            me.asked_after(set, own);
        });
    }

    /// A question a session's message asks at the end of its turn: a card,
    /// for the brain to read (it recommends replies, and decides). `own`: the
    /// user typed that turn in the session's own terminal, so the question is
    /// theirs as it is: their card at once, no note to the brain, nothing said.
    fn asked_after(&self, mut set: QuestionSet, own: bool) -> QuestionSet {
        if own {
            let _ = card::apply(&mut set, &Ev::Decide { auto: false });
            set.reason = questions::TYPED_THERE.into();
            eprintln!("[questions] {} ({}) asks, at the end of a turn the user typed in its terminal: {}", set.session, set.name, set.title);
        }
        self.upsert(set)
    }

    /// A turn that ended waiting for the user to act: a card that goes
    /// straight to them (the brain can't merge or test for them, so it isn't
    /// asked to answer it), the session's open job blocked on it, and the
    /// request said in plain words: by the tower, unless a watch reports
    /// this turn to the brain (it says it then, naming the space). At a
    /// quiet level the tower says it itself either way, and once per
    /// decision: the same wait from another session of the space adds its
    /// session to the card and says nothing.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn waiting(&self, address: &str, name: &str, created_ms: Option<u64>, message: &str, request: &str, context: &str, watched: bool) -> Option<QuestionSet> {
        self.waiting_in(None, address, name, created_ms, message, request, context, watched, false)
    }

    /// `waiting`, for a session in `space`. `own`: the turn was the user's,
    /// typed in the session's terminal: its card is there, and nothing is said.
    #[allow(clippy::too_many_arguments)]
    fn waiting_in(&self, space: Option<String>, address: &str, name: &str, created_ms: Option<u64>, message: &str, request: &str, context: &str, watched: bool, own: bool) -> Option<QuestionSet> {
        let mut set = questions::wait_set(address, name, message, request, context);
        set.at_ms = crate::history::now_ms();
        set.created_ms = created_ms;
        set.space = space;
        let known = self.get(&set.id).is_some();
        let set = self.upsert(set);
        if known {
            return None;
        }
        eprintln!("[questions] {address} ({name}) waits for the user: {request}");
        let (a, r) = (address.to_string(), request.to_string());
        crate::history::later(move || crate::history::job_follow(&a, crate::history::JobState::Blocked, &r));
        if own {
            eprintln!("[questions] {address} ({name}): the user typed that turn in its terminal; its wait isn't said");
        } else if self.tells(&set.id, !watched) {
            self.tell(address, &questions::wait_line(name, request));
        }
        Some(set)
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
            // Read before the user answered it in the terminal (a reader a step behind the
            // radar): that screen is gone, and its card doesn't come back.
            if set.kind != "text" && set.at_ms > 0 && i.there.iter().any(|x| x.set.session == set.session && x.set.id == set.id && set.at_ms <= x.closed_ms) {
                eprintln!("[questions] {} ({}): read before it was answered in its terminal; not shown again", set.id, set.session);
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
                    set.space = set.space.take().or_else(|| o.space.clone());
                    set.state = o.state.clone();
                    // What a tab was answered with stays known after the screen stops showing it.
                    for (t, was) in set.tabs.iter_mut().zip(&o.tabs) {
                        if t.answered && t.answer.is_empty() && was.answered {
                            t.answer = was.answer.clone();
                        }
                    }
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
                        set.space = set.space.take().or_else(|| old.space.clone());
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
        // Another dialog in place of one the tower didn't answer: that one was answered in its terminal.
        if let Some(c) = closed.as_ref().filter(|c| c.kind != "text" && c.state.phase == Phase::Closed { why: CloseWhy::Unblocked }) {
            self.answered_there(vec![c.clone()]);
        }
        self.publish();
        self.event(if new { "open" } else { "update" }, Some(&set), &set.id);
        if let Some(c) = &closed {
            eprintln!("[questions] {} closed: {:?}", c.id, c.state.phase);
            self.event("closed", None, &c.id);
        }
        // The user's with nothing to tap (a wait, a question after their own turn): buttons.
        if set.kind == "text" && set.state.decision == Decision::Escalated && self.give_buttons(&set.id) {
            set = self.get(&set.id).unwrap_or(set);
        }
        // Claude's own memory or notes: the tower's plain yes, nobody asked.
        if note && self.memory_rule(&set) {
            return self.get(&set.id).unwrap_or(set);
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
        let mut there: Vec<QuestionSet> = vec![];
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
                // The session went on without the tower: the user answered it in its own terminal.
                let answered_there = matches!(w, CloseWhy::Unblocked | CloseWhy::AnsweredElsewhere);
                // Closing never fails but for a re-read while typing, which isn't closed here.
                let _ = card::apply(&mut s, &Ev::Close(w));
                if answered_there {
                    there.push(s.clone());
                }
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
        // Their decisions close first, as answered there (the queue published next leaves them out).
        self.answered_there(there);
        if !gone.is_empty() {
            self.publish();
            for id in gone {
                self.event("closed", None, &id);
            }
        }
    }

    /// Sets the session stopped showing without the tower answering them:
    /// the user answered them in the session's own terminal. The decision
    /// each stood in closes now, as answered (the Mac's windows and the phone
    /// lose its card with the next state), nothing is said, and a line in the
    /// record follows in a moment with what was chosen: what the screen shows
    /// for a dialog, the words the user typed for a text question.
    fn answered_there(&self, sets: Vec<QuestionSet>) {
        let now = crate::history::now_ms();
        for set in sets {
            let decided = self.decisions.get().map(|d| d.answered_in_terminal(&set.id)).unwrap_or_default();
            let (decision, over) = match decided.first() {
                Some((id, over)) => (Some(id.clone()), *over),
                None => (None, false),
            };
            {
                let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                i.there.retain(|x| now.saturating_sub(x.closed_ms) < THERE_KEEP_MS && x.set.id != set.id);
                i.typed_words.retain(|_, (_, at)| now.saturating_sub(*at) < THERE_KEEP_MS);
                let words = if set.kind == "text" { i.typed_words.remove(&set.session).map(|(w, _)| w) } else { None };
                i.there.push(There { set: set.clone(), decision, over, closed_ms: now, words, not_users: false, recorded: false });
            }
            eprintln!("[questions] {} ({}): its session went on without the tower: answered in its terminal", set.id, set.session);
            self.record_there_later(&set.id);
        }
    }

    /// A PR was merged: the open questions of its space's sessions that are
    /// about it (they name `#<n>` or its link: "Close release PR #786 into
    /// main, or leave it open?") are settled, and their decisions with them.
    pub fn settled_by_merge(&self, pr: &crate::prs::Pr, by: &str) -> Vec<String> {
        let what = format!("PR #{} was merged{}", pr.number, if by == "github" { " on GitHub" } else { "" });
        let about: Vec<(String, String)> = {
            let i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.sets
                .iter()
                .filter(|s| s.session == pr.session || (pr.space.is_some() && s.space == pr.space))
                .filter(|s| names_pr(&serde_json::to_string(s).unwrap_or_default(), &pr.repo, pr.number))
                .map(|s| (s.session.clone(), s.id.clone()))
                .collect()
        };
        for (session, id) in &about {
            self.settle(session, id, &what);
        }
        about.into_iter().map(|(_, id)| id).collect()
    }

    /// A set settled outside its dialog (`what` settled it): closed, and the
    /// decision it stood in answered with `what`.
    fn settle(&self, session: &str, id: &str, what: &str) {
        // The decision first: closing the set would close it with no answer.
        if let Some(d) = self.decisions.get() {
            d.settled(id, what);
        }
        let w = what.to_string();
        self.close_where(session, |s| s.id == id, move |_| CloseWhy::Settled { what: w.clone() }, true);
        eprintln!("[questions] {id} ({session}): {what}: settled");
    }

    /// A brain passed `message` on to `session` (`tower queue`) after the
    /// user spoke to it (`users`: their words, at `users_ms`). A question the
    /// session asked at the end of its turn before that, which is the user's
    /// (it stands in a decision), is the user's answer relayed when the
    /// message answers it (`relays`: one headless call): settled with it,
    /// and its decision answered. One asked after the user spoke, a dialog,
    /// a question the tower answers itself, or a message about something
    /// else stays open, as one the tower types for itself (a note, a message
    /// from another session) always does. The sets settled.
    pub fn relayed(&self, session: &str, users: &str, users_ms: u64, message: &str) -> Vec<String> {
        let theirs = |s: &QuestionSet| self.decisions.get().is_some_and(|d| !d.for_question(&s.id).is_empty());
        let sets: Vec<QuestionSet> = self.list().into_iter().filter(|s| s.session == session && s.kind == "text" && s.at_ms <= users_ms && theirs(s)).collect();
        let mut out = vec![];
        for s in sets {
            let asked: Vec<&str> = s.tabs.iter().map(|t| t.question.trim()).filter(|q| !q.is_empty()).collect();
            let question = format!("{}\n\n(The end of its message:) {}", if asked.is_empty() { s.title.clone() } else { asked.join("\n") }, s.detail);
            match (self.relays)(&s.name, &question, users, message) {
                Ok(true) => {
                    // Answered some other way while the model was asked (the card, the terminal): nothing more to do.
                    if self.get(&s.id).is_none() {
                        eprintln!("[questions] {} ({session}): the tower passed on the user's answer, but it was over already", s.id);
                        continue;
                    }
                    let what = format!("answered through the tower: {}", clip(message.trim(), 200));
                    // Its session leaves its decision (over when nobody else asks the same), answered with it.
                    let left = self.decisions.get().map(|d| d.passed_on(&s.id, &what)).unwrap_or_default();
                    self.close_where(session, |x| x.id == s.id, |_| CloseWhy::Settled { what: what.clone() }, true);
                    let cards: Vec<String> = left.iter().map(|(d, over)| format!("{d}{}", if *over { " (over)" } else { " (others still wait)" })).collect();
                    eprintln!("[questions] {} ({session}): {what}: settled; its card: {}", s.id, if cards.is_empty() { "none".to_string() } else { cards.join(", ") });
                    out.push(s.id.clone());
                }
                Ok(false) => eprintln!("[questions] {} ({session}): what the tower passed on doesn't answer it: it stays open", s.id),
                Err(e) => eprintln!("[questions] {} ({session}): couldn't tell whether what the tower passed on answers it ({e:#}): it stays open", s.id),
            }
        }
        out
    }

    fn there(&self, id: &str) -> Option<There> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).there.iter().rev().find(|x| x.set.id == id).cloned()
    }

    /// The record line of a set answered in its terminal, once what was
    /// chosen is known (or waited for long enough).
    fn record_there_later(&self, id: &str) {
        let (me, id) = (self.clone(), id.to_string());
        std::thread::spawn(move || {
            let Some(x) = me.there(&id) else { return };
            let chosen = if x.set.kind == "text" {
                // The words come with the prompt hook, which may be a moment behind the radar.
                let until = crate::history::now_ms() + me.waits.words_ms;
                loop {
                    match me.there(&id) {
                        Some(x) if x.words.is_some() || x.not_users => break x.words,
                        None => return,
                        Some(_) if crate::history::now_ms() >= until => break None,
                        Some(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
                    }
                }
            } else {
                // What was chosen, once the screen has moved on.
                std::thread::sleep(std::time::Duration::from_millis(me.waits.screen_ms));
                (me.screen)(&x.set.session).and_then(|s| questions::chosen_on_screen(&x.set, &s))
            };
            me.record_there(&id, chosen);
        });
    }

    /// Writes the line of a set answered in its terminal (once): "Answered
    /// in <space › session>: <what was chosen>", or "Answered in its
    /// terminal" when nothing shows what. A text question whose session the
    /// tower (or another terminal) gave its next prompt wasn't answered by
    /// the user: its decision gets the plain line any other gets.
    fn record_there(&self, id: &str, chosen: Option<String>) {
        let x = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(x) = i.there.iter_mut().rev().find(|x| x.set.id == id) else { return };
            if x.recorded {
                return;
            }
            x.recorded = true;
            x.clone()
        };
        let set = &x.set;
        let decisions = self.decisions.get();
        let towers = x.not_users || (set.kind == "text" && x.words.is_none() && super::typed::typed_lately(&set.session, 20_000));
        if towers {
            eprintln!("[questions] {id}: its session was given its next prompt by the tower, not answered by the user");
            if let (Some(d), true, Some(ds)) = (&x.decision, x.over, decisions) {
                ds.record_closed(d);
            }
            return;
        }
        let name = if set.name.is_empty() { set.session.clone() } else { set.name.clone() };
        let place = crate::names::shown(set.space.as_deref(), &name);
        let chosen = chosen.map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|c| !c.is_empty()).map(|c| clip(&c, 300));
        let how = match &chosen {
            Some(c) => format!("Answered in {place}: {c}"),
            None => "Answered in its terminal".to_string(),
        };
        if let (Some(d), Some(ds)) = (&x.decision, decisions) {
            ds.note_answer(d, &match &chosen {
                Some(c) => format!("in its terminal: {c}"),
                None => "in its terminal".to_string(),
            });
        }
        let asked = if set.title.trim().is_empty() { set.tabs.first().map(|t| t.question.clone()).unwrap_or_default() } else { set.title.clone() };
        eprintln!("[questions] {id}: {how}");
        (self.record)(crate::history::Entry {
            kind: "decision".into(),
            id: Some(x.decision.clone().unwrap_or_else(|| set.id.clone())),
            session: Some(set.session.clone()),
            name: Some(set.name.clone()).filter(|n| !n.is_empty()),
            space: set.space.clone(),
            status: Some("answered".into()),
            why: Some(set.reason.clone()).filter(|w| !w.is_empty()),
            source: Some(format!("terminal:{}", set.session)),
            text: if asked.trim().is_empty() { how } else { format!("{} -> {how}", clip(asked.trim(), 200)) },
            record: true,
            ..Default::default()
        });
    }

    /// The user typed `words` into the session's own terminal (its prompt
    /// hook said so, and nothing the tower typed): a question the session
    /// asked at the end of its turn is answered with them; and a reply the
    /// tower held for it (the user's answer on the card, waiting for the
    /// session's turn to end) is never typed when these words answer the
    /// same (`covers`: the same answer, or another one), written down, not
    /// said. Words about something else leave the held reply to go as it
    /// would. True when it was an answer, not new work.
    pub fn typed_there(&self, address: &str, words: &str, at_ms: u64) -> bool {
        let words = words.trim().to_string();
        let mut answered = false;
        {
            // The user's prompt is the one it works on now.
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            i.went_on.remove(address);
            i.not_users.remove(address);
        }
        // The reply the tower held: moot if these words answer it too.
        let waiting = self.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.get(address).cloned();
        let moot = waiting.as_ref().is_some_and(|r| {
            let same = super::typed::words(&words) == super::typed::words(&r.text);
            same || (self.covers)(&r.name, &words, &r.text).unwrap_or_else(|e| {
                eprintln!("[questions] {address}: couldn't tell whether what the user typed answers {} ({e:#}): its reply still goes", r.set);
                false
            })
        });
        if let Some(r) = waiting.as_ref().filter(|_| !moot) {
            eprintln!("[questions] {address}: what the user typed there is something else: the waiting reply to {} still goes", r.set);
        }
        // Still the same reply (none came or went meanwhile): out.
        let held = waiting.filter(|_| moot).and_then(|w| {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if i.replies.get(address).is_some_and(|r| r.set == w.set && r.at_ms == w.at_ms) { i.replies.remove(address) } else { None }
        });
        if let Some(r) = held {
            self.save();
            eprintln!("[questions] {address}: the waiting reply to {} is not sent: the user answered in the terminal", r.set);
            (self.record)(crate::history::Entry {
                kind: "queue".into(),
                session: Some(address.to_string()),
                name: Some(r.name.clone()).filter(|n| !n.is_empty()),
                space: self.past(&r.set).or_else(|| self.get(&r.set)).and_then(|s| s.space),
                status: Some("not-sent".into()),
                text: format!("Not sent: you answered it in the terminal (the reply was \"{}\")", clip(r.text.trim(), 200)),
                record: true,
                ..Default::default()
            });
            answered = true;
        }
        // Its question, still open: these words answer it.
        if self.list().iter().any(|s| s.session == address && s.kind == "text") {
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).typed_words.insert(address.to_string(), (words.clone(), at_ms));
            self.close_where(address, |s| s.kind == "text", |_| CloseWhy::Unblocked, true);
            return true;
        }
        // Or the one the radar closed a moment ago, as the session started on them.
        let now = crate::history::now_ms();
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(x) = i
            .there
            .iter_mut()
            .rev()
            .find(|x| x.set.session == address && x.set.kind == "text" && !x.recorded && x.words.is_none() && now.saturating_sub(x.closed_ms) < THERE_KEEP_MS)
        {
            x.words = Some(words);
            x.not_users = false;
            answered = true;
        }
        answered
    }

    /// The session's next prompt was the tower's (or another terminal's, or
    /// its own), at turn count `turn`: a question it asked at the end of its
    /// turn stays open while it works on it (`went_on`), and one the radar
    /// closed a moment ago was not the user's to have answered there.
    pub fn typed_by_tower(&self, address: &str, turn: Option<u64>) {
        let now = crate::history::now_ms();
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        i.not_users.insert(address.to_string(), (now, turn));
        if matches!(i.went_on.get(address), Some(WentOn::Waiting(_))) {
            i.went_on.insert(address.to_string(), WentOn::Kept);
            eprintln!("[questions] {address} works on a prompt that isn't the user's: its question stays open");
        }
        for x in i.there.iter_mut().filter(|x| x.set.session == address && x.set.kind == "text" && !x.recorded && x.words.is_none() && now.saturating_sub(x.closed_ms) < THERE_KEEP_MS) {
            x.not_users = true;
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
            (self.answers)(self, id);
        } else {
            self.tell_escalated(&set);
        }
        Ok(set)
    }

    /// A prompt that only writes Claude's own memory or notes
    /// (`questions::memory_yes`): the tower answers its plain one-time yes
    /// at once, without the brain and without the user (unless they asked
    /// to decide everything). Whether it did.
    fn memory_rule(&self, set: &QuestionSet) -> bool {
        let Some(yes) = questions::memory_yes(set) else { return false };
        let decided = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if i.policy == "ask-everything" {
                return false;
            }
            let Some(s) = i.sets.iter_mut().find(|s| s.id == set.id) else { return false };
            let label = s.tabs.first().and_then(|t| t.options.get(yes)).map(|o| o.label.clone()).unwrap_or_default();
            s.recommendations = Some(vec![questions::TabRecs { tab: 0, options: vec![questions::Rec { label, why: MEMORY_YES.into(), choice: json!(yes) }] }]);
            if card::apply(s, &Ev::Decide { auto: true }).is_err() {
                return false;
            }
            s.reason = MEMORY_YES.into();
            s.clone()
        };
        eprintln!("[questions] {} ({}): {}; answering {:?}", decided.session, decided.id, MEMORY_YES, decided.tabs[0].options[yes].label);
        self.publish();
        self.event("update", Some(&decided), &decided.id);
        (self.answers)(self, &decided.id);
        true
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
                // A routine answer is written down, not told, unless the tower tells everything.
                let record = crate::tell::for_record(crate::tell::level(), "auto");
                crate::history::later(move || crate::history::record_session_as("answered", &a, &l, Some("auto"), record));
                if let Some(s) = self.inner.lock().unwrap_or_else(|e| e.into_inner()).sets.iter_mut().find(|s| s.id == now.id) {
                    s.answered_with = picked;
                }
                self.publish();
            }
            Err(e) => {
                if let Some(held) = e.downcast_ref::<Held>() {
                    self.escalate(&set.id, &held.0);
                } else if let Some(busy) = e.downcast_ref::<Busy>() {
                    eprintln!("[questions] {}: not answering {id}: {busy}", set.session);
                    self.escalate_quietly(&set.id, &busy.0);
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
        // Read, but with nothing to tap: buttons all the same.
        self.give_buttons(id);
    }

    /// A text set that is the user's with nothing to tap (one question, no
    /// answers: a wait, a question after a turn they typed, one escalated
    /// without any): the generic buttons at once, then choices of its own
    /// read from what the session said (`choices`: one headless call, in a
    /// thread) in their place, while nobody has answered. Once per set.
    /// Whether it gave them.
    fn give_buttons(&self, id: &str) -> bool {
        let set = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(k) = i.sets.iter().position(|s| s.id == id) else { return false };
            let s = &i.sets[k];
            let has = s.recommendations.as_ref().is_some_and(|r| r.iter().any(|t| !t.options.is_empty()));
            if s.kind != "text" || s.tabs.len() != 1 || s.state.decision != Decision::Escalated || has || i.buttoned.contains(id) {
                return false;
            }
            i.buttoned.insert(id.to_string());
            let s = &mut i.sets[k];
            s.recommendations = Some(vec![questions::TabRecs { tab: 0, options: crate::choices::generic() }]);
            s.clone()
        };
        self.save();
        self.publish();
        self.event("update", Some(&set), id);
        let me = self.clone();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let got = (me.choices)(&set.name, &set.detail, &set.title);
            let id = set.id.clone();
            match got {
                Ok(recs) => {
                    let now = {
                        let mut i = me.inner.lock().unwrap_or_else(|e| e.into_inner());
                        // Still open, nobody answering, and still the generic ones.
                        i.sets.iter_mut().find(|s| s.id == id && !matches!(s.status.as_str(), "answering" | "queued")).and_then(|s| {
                            let generic = s.recommendations.as_ref().and_then(|r| r.first()).is_some_and(|t| crate::choices::is_generic(&t.options));
                            generic.then(|| {
                                s.recommendations = Some(vec![questions::TabRecs { tab: 0, options: recs.clone() }]);
                                s.clone()
                            })
                        })
                    };
                    match now {
                        Some(s) => {
                            me.save();
                            me.publish();
                            me.event("update", Some(&s), &id);
                            eprintln!(
                                "[questions] {id} ({}): its buttons, read from what it said in {} ms: {}",
                                s.session,
                                started.elapsed().as_millis(),
                                recs.iter().map(|r| r.label.as_str()).collect::<Vec<_>>().join(" / ")
                            );
                        }
                        None => eprintln!("[questions] {id}: answered (or over) before its own buttons came; left as it is"),
                    }
                }
                Err(e) => eprintln!("[questions] {id}: no buttons of its own ({e:#}): Go ahead / Not yet stay"),
            }
        });
        true
    }

    /// The user's card after all, said to nobody: they are in its dialog already.
    fn escalate_quietly(&self, id: &str, why: &str) {
        let set = {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(s) = i.sets.iter_mut().find(|s| s.id == id) else { return };
            if card::apply(s, &Ev::Escalate).is_err() {
                return;
            }
            s.reason = why.to_string();
            s.clone()
        };
        self.publish();
        // Told, as far as the decision goes: nothing says it later either.
        let _ = self.tells(&set.id, false);
        self.event("update", Some(&set), id);
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
        // One decision per key: the same thing asked again by another session says nothing.
        if !self.tells(&set.id, true) {
            eprintln!("[questions] {} ({}): in front of the user already, not said again", set.id, set.session);
            return;
        }
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
                // it was read again as too), nothing is typed. Still blocked on a
                // screen the tower can't read: not answered, and said so.
                let why = if hc::get(&address).is_some_and(|t| hc::status(&t) == SessionStatus::Blocked) { CloseWhy::Unread } else { CloseWhy::AnsweredElsewhere };
                self.close_where(&address, |s| s.kind != "text", |_| why.clone(), true);
                bail!(Late(why.late(&k.id, if k.name.is_empty() { &k.session } else { &k.name })));
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
        // The user is in the dialog, answering it in the terminal: the tower's own answer stays out.
        let there = questions::answered_there(&now);
        if auto && now.kind == "claude-ask" && now.tabs.len() > 1 && (!there.is_empty() || now.current_tab != 0) {
            bail!(Busy(format!("the user is answering it in its terminal ({} of {} answered there)", there.len(), now.tabs.len())));
        }
        // What the user answered in the terminal stands: those tabs get nothing typed, and the
        // keys start where the dialog is now (its tab: `now.current_tab`).
        let (answers, skipped) = questions::answers_left(&now, answers);
        if !skipped.is_empty() {
            eprintln!("[questions] {}: tab(s) {skipped:?} answered in its terminal already; their answers are left out", now.id);
        }
        if let Some(why) = questions::nothing_to_type(&now, &answers, &skipped, submit) {
            bail!("{why}");
        }
        let answers = if auto { self.recheck(&now, &answers).map_err(Held)? } else { answers };
        // Checked and marked at once: a second answer (the phone, the panel, the tower) waits its turn.
        let tracked = match self.start_answering(&now.id, by) {
            Ok(t) => t,
            Err(Refused::AnsweredAlready) => bail!(Late(format!("already answered: {} is answered already", now.id))),
            Err(e) => return Err(anyhow::Error::new(e).context(now.id.clone())),
        };
        let result = {
            let _typing = hc::typing(&address);
            // Once its dialog is gone (the user answered it in the terminal as the keys went
            // in), nothing more goes in: not a key, not a word.
            let id = now.id.clone();
            let open = || !tracked || self.get(&id).is_some();
            let mut term = Guarded { term: questions::Hc { address: address.clone() }, open: &open };
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

/// A session's terminal for the keys that answer one set: while the set is
/// open. Once the radar closed it (its dialog went: answered in the terminal)
/// no key and no word goes in, whatever the driver was about to press.
struct Guarded<'a, T: questions::Term> {
    term: T,
    open: &'a dyn Fn() -> bool,
}

impl<T: questions::Term> Guarded<'_, T> {
    fn still(&self) -> Result<()> {
        if !(self.open)() {
            bail!("its dialog is gone (answered in its terminal): nothing more is typed");
        }
        Ok(())
    }
}

impl<T: questions::Term> questions::Term for Guarded<'_, T> {
    fn read(&mut self) -> String {
        self.term.read()
    }
    fn keys(&mut self, keys: &[&str]) -> Result<()> {
        self.still()?;
        self.term.keys(keys)
    }
    fn paste(&mut self, text: &str) -> Result<()> {
        self.still()?;
        self.term.paste(text)
    }
    fn settle(&mut self) {
        self.term.settle()
    }
}

/// `s` on one line, cut to `n` characters.
fn clip(s: &str, n: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n { flat } else { format!("{}…", flat.chars().take(n).collect::<String>().trim_end()) }
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
        // Words of the user's own in it: their request in that session (what it works on now).
        if let Some(words) = questions::users_words(answers) {
            super::agendas::answered_on_card(&set.session, &words);
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
    // Written down before it is typed: the prompt its hook reports is the tower's reply, not the user's.
    let id = super::typed::note(address, text, "reply");
    let out = std::process::Command::new(crate::config::hc_bin())
        .args(&args)
        .env_remove("HC_TERMINAL_ID")
        .env_remove("HC_SCOPE")
        .stdin(std::process::Stdio::null())
        .output();
    let ok = out.as_ref().is_ok_and(|o| o.status.success());
    super::typed::done(id, ok);
    let out = out?;
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

/// Whether the prompt `address` works on now (at turn count `turn`) is not
/// the user's: the tower is typing into it or just was, the send queue typed
/// a message in while it worked that it hasn't taken yet, or its hook said
/// so about this stretch of work.
fn not_the_users(i: &Inner, address: &str, turn: Option<u64>, now: u64) -> bool {
    if super::typed::typed_lately(address, TYPED_MS) || super::sendq::typed_in_while_working(address) {
        return true;
    }
    i.not_users.get(address).is_some_and(|(at, was)| match (*was, turn) {
        (Some(a), Some(b)) => a == b,
        _ => now.saturating_sub(*at) <= TYPED_MS,
    })
}

/// The last message a session wrote: its transcript (Claude Code here), else its screen.
/// What a session said at the end of the turn that ended at `ended_ms`:
/// its Stop hook's message (exactly that turn's), waited for a moment since
/// Herdr can see the turn end first; else its transcript or screen. (Read
/// right at the end, the transcript can lack the turn's last message, or
/// still end with the one before.)
fn turn_message(address: &str, t: &Value, ended_ms: u64) -> Option<String> {
    let since = ended_ms.saturating_sub(5_000);
    for _ in 0..6 {
        if let Some(said) = super::sessions::registry().said_since(address, since) {
            return Some(said);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    last_message(address, t)
}

/// What this machine's Claude Code session's turn said and its `gh pr
/// create` printed (`prs::turn_text`), from its transcript; empty otherwise.
fn turn_transcript(address: &str, t: &Value) -> String {
    if hc::agent_app(t).as_deref() == Some("claude-code")
        && !address.contains('/')
        && let Some(pid) = t["foreground"]["pid"].as_u64()
        && let Some(j) = questions::transcript_of(pid as u32)
    {
        return crate::prs::turn_text(&j);
    }
    String::new()
}

/// What is said of a session's turn that ended on open PRs: they wait for
/// the user's merge (from the chip in the chat).
pub fn pr_line(name: &str, open: &[crate::prs::Pr]) -> String {
    let each: Vec<String> = open
        .iter()
        .map(|p| {
            let trouble = match (p.checks.state.as_str(), p.mergeable.as_str()) {
                (_, "conflicting") => ", but it has conflicts".to_string(),
                ("failing", _) => format!(", but {}", p.checks_line()),
                _ => String::new(),
            };
            format!("PR #{} into {}{trouble}", p.number, if p.base.is_empty() { "?" } else { &p.base })
        })
        .collect();
    let it = if open.len() == 1 { "it waits" } else { "they wait" };
    format!("{name} opened {}: {it} for your merge, in the chat", each.join(" and "))
}

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
    // When the screen was read: a set read before its dialog was answered in the terminal is stale.
    let read_ms = crate::history::now_ms();
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
    set.at_ms = read_ms;
    set.created_ms = hc::created_ms(&t);
    set.space = crate::spaces::space_of(&t);
    Some(set)
}

/// `text` names PR `number` of `repo`: its link, or `#<number>` as a word
/// ("#786", not "#7860"); the repo's link names that repo only.
pub fn names_pr(text: &str, repo: &str, number: u64) -> bool {
    let n = number.to_string();
    if text.contains(&format!("github.com/{repo}/pull/{n}")) && !text.contains(&format!("github.com/{repo}/pull/{n}0")) {
        return true;
    }
    let hash = format!("#{n}");
    text.match_indices(&hash).any(|(i, _)| {
        let after = text[i + hash.len()..].chars().next();
        let before = text[..i].chars().next_back();
        !after.is_some_and(|c| c.is_ascii_digit()) && !before.is_some_and(|c| c.is_alphanumeric())
    })
}

/// How a set that is over ended, for the decision it stood in.
fn ended_as(p: &QuestionSet) -> crate::decisions::State {
    use crate::decisions::State;
    match &p.state.phase {
        Phase::Dismissed | Phase::Closed { why: CloseWhy::Dismissed } => State::Dismissed,
        Phase::Closed { why: CloseWhy::AnsweredHere | CloseWhy::AnsweredElsewhere | CloseWhy::Unblocked | CloseWhy::Settled { .. } } => State::Answered,
        _ => State::Stopped,
    }
}

/// Something was asked of the session: an instruction waits for it in the
/// queue, or it is on an agenda task (other than one that itself told it to
/// wait for instructions).
pub(super) fn has_work(address: &str) -> bool {
    if super::sendq::waiting_counts().get(address).copied().unwrap_or(0) > 0 {
        return true;
    }
    super::agendas::working_on(address).is_some_and(|asked| !questions::ready_only(&asked))
}

/// Idle is not blocked: a turn's end that only says the session is ready
/// (`questions::ready_only`), from a session nothing was asked of. It is no
/// wait for the user: no card, no alert, no decision.
pub fn idle_ready(address: &str, message: &str) -> bool {
    questions::ready_only(message) && !has_work(address)
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
            Some(("Edit" | "Update" | "Write" | "MultiEdit" | "NotebookEdit", path)) => format!("{name} asks to edit {}", edited(path)),
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

/// The file a blocked edit names, for a card: its name, never the whole
/// path. Herdr's status line cuts a long path short ("…/claude-profiles/
/// 1f2995fa-2a8…"), so a cut one is said by where it is: Claude's own notes,
/// or only "a file".
fn edited(path: &str) -> String {
    let path = path.trim();
    if path.ends_with('…') || path.ends_with("...") {
        let claude = ["/.claude/", "/.claude-accounts/", "claude-profiles/", "/memory/"].iter().any(|d| path.contains(d));
        return if claude { "its Claude notes".into() } else { "a file".into() };
    }
    let name = path.rsplit('/').find(|p| !p.is_empty()).unwrap_or(path);
    if name.chars().count() <= 60 { name.to_string() } else { format!("{}…", name.chars().take(60).collect::<String>()) }
}

/// The same as a decision's title, which names no session (its card shows
/// who): "Asks to run npm test", "Has a plan ready for you to approve".
pub fn blocked_title(status: &Value) -> String {
    let line = blocked_notice("\u{1}", status);
    let rest = line.strip_prefix("\u{1} ").unwrap_or(&line);
    let rest = if rest == "needs you" { "Needs you: it shows something the tower can't read" } else { rest };
    let mut c = rest.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
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
        // No screen is read (no hc), nothing is written, nobody waits long.
        a.screen = Arc::new(|_: &str| None);
        a.record = Arc::new(|_: crate::history::Entry| ());
        a.waits = Waits { screen_ms: 0, words_ms: 1_500 };
        // No model: words that open with "no" or "yes" answer the reply the tower holds.
        a.covers = Arc::new(|_: &str, words: &str, _: &str| Ok(["no", "yes"].iter().any(|w| words.to_lowercase().starts_with(w))));
        (a, told)
    }

    /// `deciding`, keeping what the record would get, with `screen` for every session's screen.
    #[allow(clippy::type_complexity)]
    fn recording(screen: Option<String>) -> (Asks, super::super::decisions::Decisions, Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<crate::history::Entry>>>) {
        let (mut a, d, user) = deciding(|| crate::tell::Level::Decisions);
        let lines = Arc::new(Mutex::new(vec![]));
        let l = lines.clone();
        a.record = Arc::new(move |e: crate::history::Entry| l.lock().unwrap_or_else(|e| e.into_inner()).push(e));
        a.screen = Arc::new(move |_: &str| screen.clone());
        (a, d, user, lines)
    }

    /// The record's lines, once its thread has written `n`.
    fn lines(l: &Arc<Mutex<Vec<crate::history::Entry>>>, n: usize) -> Vec<crate::history::Entry> {
        for _ in 0..300 {
            if l.lock().unwrap_or_else(|e| e.into_inner()).len() >= n {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        l.lock().unwrap_or_else(|e| e.into_inner()).clone()
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
    fn a_question_names_a_pr_by_its_number_or_its_link() {
        assert!(names_pr("Close release PR #786 into main, or leave it open?", "eyzhub/eyzflow", 786));
        assert!(names_pr("see https://github.com/eyzhub/eyzflow/pull/786 first", "eyzhub/eyzflow", 786));
        assert!(!names_pr("PR #7860 is another one", "eyzhub/eyzflow", 786));
        assert!(!names_pr("issue a#786", "eyzhub/eyzflow", 786));
        assert!(!names_pr("https://github.com/eyzhub/other/pull/786", "eyzhub/eyzflow", 786), "another repo's link names another PR");
        assert!(!names_pr("https://github.com/eyzhub/eyzflow/pull/7860", "eyzhub/eyzflow", 786));
        assert!(CloseWhy::Settled { what: "PR #786 was merged on GitHub".into() }.late("q1", "web").starts_with("already answered"));
    }

    #[test]
    fn a_turn_ending_on_a_pr_says_it_waits_for_the_merge() {
        let pr = |n: u64, base: &str, checks: &str, mergeable: &str| crate::prs::Pr {
            repo: "o/app".into(),
            number: n,
            base: base.into(),
            state: "open".into(),
            mergeable: mergeable.into(),
            checks: crate::prs::Checks { state: checks.into(), failing: vec!["lint".into()], ..Default::default() },
            ..Default::default()
        };
        assert_eq!(pr_line("mediaroom", &[pr(1250, "main", "passing", "mergeable")]), "mediaroom opened PR #1250 into main: it waits for your merge, in the chat");
        assert_eq!(
            pr_line("sdk", &[pr(88, "dev", "failing", "mergeable"), pr(89, "main", "pending", "conflicting")]),
            "sdk opened PR #88 into dev, but 1 check failing: lint and PR #89 into main, but it has conflicts: they wait for your merge, in the chat"
        );
    }

    /// The missed release: a turn that ends waiting for the user to merge
    /// becomes a card that is theirs from the start (the brain isn't asked
    /// to answer it), said in plain words unless a watch reports that turn,
    /// and said once.
    #[test]
    fn a_turn_waiting_for_the_user_goes_straight_to_them() {
        let (a, brain) = asks();
        let user = Arc::new(Mutex::new(Vec::<String>::new()));
        let u = user.clone();
        a.set_notify(move |addr, text| u.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{addr}: {text}")));
        let said = || user.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let msg = "PR #1250 (dev to main) is open and its checks passed. The merge command is blocked for me, so you need to merge it. I'll wait.";
        let req = questions::waits_for_user(msg).unwrap();
        let set = a.waiting("tw-wait-1", "mediaroom", None, msg, &req, "Release the ticket change", false).unwrap();
        assert_eq!(set.title, "PR #1250 (dev to main) is open and its checks passed. The merge command is blocked for me, so you need to merge it.");
        assert_eq!((set.kind.as_str(), set.decision.as_deref(), set.status.as_str()), ("text", Some("escalate"), "open"));
        assert_eq!(set.reason, questions::WAITS);
        assert!(a.list().iter().any(|s| s.id == set.id), "on the panel and the phone as an open card");
        assert_eq!(said(), vec!["tw-wait-1: mediaroom waits for you: PR #1250 (dev to main) is open and its checks passed. The merge command is blocked for me, so you need to merge it"]);
        assert!(told(&brain, 1).is_empty(), "the brain isn't asked to answer it");
        // The same turn read again: nothing new, nothing said again.
        assert!(a.waiting("tw-wait-1", "mediaroom", None, msg, &req, "", false).is_none());
        assert_eq!(said().len(), 1);
        // Watched: the card, but the watch's report says it (through the brain).
        assert!(a.waiting("tw-wait-2", "sdk", None, msg, &req, "", true).is_some());
        assert_eq!(said().len(), 1);
        // Its job stays blocked while the card is open (the radar asks).
        assert!(a.waits_on("tw-wait-1") && a.waits_on("tw-wait-2") && !a.waits_on("tw-other"));
    }

    /// Asks whose sets that are the user's become decisions, in a tower that
    /// tells `level`; and what the user is told.
    fn deciding(level: fn() -> crate::tell::Level) -> (Asks, super::super::decisions::Decisions, Arc<Mutex<Vec<String>>>) {
        let (a, _) = asks();
        let d = super::super::decisions::Decisions::telling(a.shared.clone(), level);
        let a = a.with_decisions(d.clone());
        let user = Arc::new(Mutex::new(Vec::<String>::new()));
        let u = user.clone();
        a.set_notify(move |addr, text| u.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{addr}: {text}")));
        (a, d, user)
    }

    /// Three sessions end their turns waiting for the same thing: one
    /// decision with three waiting, one alert, one sentence. It closes when
    /// they are answered, dismissed, or gone.
    #[test]
    fn the_same_wait_from_three_sessions_is_one_decision_said_once() {
        let (a, d, user) = deciding(|| crate::tell::Level::Decisions);
        let said = || user.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let msg = "PR #1273 is ready. Still waiting on you: merging #1273 into dev.";
        let req = questions::waits_for_user(msg).unwrap();
        // The first is watched: at a quiet level the brain's words are for the record, so the tower says it itself.
        let first = a.waiting("tw-d1", "web", None, msg, &req, "", true).unwrap();
        a.waiting("tw-d2", "api", None, msg, &req, "", false).unwrap();
        a.waiting("tw-d3", "sdk", None, msg, &req, "", false).unwrap();
        assert_eq!(said(), vec![format!("tw-d1: web waits for you: {}", req.trim_end_matches('.'))], "said once, for the first");
        let st = a.shared.get();
        assert_eq!(st.questions.len(), 3, "each session keeps its card");
        assert_eq!((st.decisions.len(), st.decisions_count), (1, 1), "one decision");
        let v = &st.decisions[0];
        assert_eq!((v["kind"].as_str(), v["waiting"].as_u64(), v["count"].as_u64(), v["state"].as_str()), (Some("question"), Some(3), Some(3), Some("open")));
        assert_eq!((v["title"].as_str(), v["why"].as_str(), v["question_id"].as_str()), (Some(req.as_str()), Some(questions::WAITS), Some(first.id.as_str())));
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(d.for_question(&first.id), vec![id.clone()]);
        // One of them works again, on what the user typed there (its hook says so): two wait.
        a.seen("tw-d1", &json!({}), &SessionStatus::Working);
        a.typed_there("tw-d1", "merged it, go on", crate::history::now_ms());
        let st = a.shared.get();
        assert_eq!((st.decisions.len(), st.decisions[0]["waiting"].as_u64(), st.decisions[0]["id"].as_str()), (1, Some(2), Some(id.as_str())));
        // One is dismissed, the last one's session ends: the decision is over, as answered (one of them was).
        let second = a.list().into_iter().find(|s| s.session == "tw-d2").unwrap();
        a.dismiss(&second.id).unwrap();
        assert_eq!(a.shared.get().decisions_count, 1);
        a.seen("tw-d3", &Value::Null, &SessionStatus::Gone);
        assert_eq!((a.shared.get().decisions.len(), a.shared.get().decisions_count), (0, 0));
        assert_eq!(d.get(&id).map(|d| d.state), Some(crate::decisions::State::Answered));
        // The same wait later is a new decision, said again.
        a.waiting("tw-d4", "docs", None, msg, &req, "", false).unwrap();
        assert_eq!(said().len(), 2);
        assert_ne!(a.shared.get().decisions[0]["id"].as_str(), Some(id.as_str()));
        // Dismissed, it is over as dismissed.
        let fourth = a.list().into_iter().find(|s| s.session == "tw-d4").unwrap();
        a.dismiss(&fourth.id).unwrap();
        assert_eq!(d.list(None, true)["closed"][0]["state"], "dismissed");
    }

    /// The same permission asked in two sessions: one decision with the
    /// tower's pick, said once. While the tower tells everything, each is
    /// said, as before; the decision is still one.
    #[test]
    fn a_repeat_is_not_said_at_decisions_but_still_is_at_everything() {
        let ask = |a: &Asks, n: u64, session: &str, name: &str| {
            let mut p = perm("approve Bash: git push origin main");
            (p.id, p.session, p.name) = (format!("q{n:012x}"), session.into(), name.into());
            a.upsert(p.clone());
            let recs = json!([{"tab": 0, "options": [{"label": "Yes", "why": "the tests pass", "choice": 0}, {"label": "No", "why": "", "choice": 2}]}]);
            a.recommend(&p.id, &recs, Some("auto"), "routine", false).unwrap()
        };
        for (level, alerts) in [(|| crate::tell::Level::Decisions, 1), (|| crate::tell::Level::Results, 1), (|| crate::tell::Level::Everything, 2)] as [(fn() -> crate::tell::Level, usize); 3] {
            let (a, _, user) = deciding(level);
            ask(&a, 1, "tw-p1", "web");
            ask(&a, 2, "tw-p2", "api");
            let said = user.lock().unwrap().clone();
            assert_eq!(said.len(), alerts, "{said:?}");
            assert!(said[0].starts_with("tw-p1: web asks: Allow Bash: git push origin main") && said[0].ends_with("I'd say: Yes."), "{said:?}");
            let st = a.shared.get();
            assert_eq!((st.decisions.len(), st.questions.len()), (1, 2));
            let v = &st.decisions[0];
            assert_eq!((v["kind"].as_str(), v["waiting"].as_u64(), v["pick"].as_str(), v["why"].as_str()), (Some("permission"), Some(2), Some("Yes"), Some("it pushes or merges to a shared branch")), "why: the rule that made it the user's");
            assert_eq!(v["options"][0], json!({ "label": "Yes", "why": "the tests pass", "pick": true }));
        }
    }

    /// Idle is not blocked: a session that only says it is ready, with
    /// nothing asked of it, waits for nothing the user must do.
    #[test]
    fn a_session_that_only_says_it_is_ready_is_not_waiting_for_the_user() {
        for msg in ["I'm waiting for your instructions", "I have no task yet and I'm waiting for your instructions", "I'm on dev and waiting for your instructions"] {
            assert!(questions::waits_for_user(msg).is_some(), "what made it a wait before: {msg}");
            assert!(idle_ready("tw-idle", msg), "{msg}");
        }
        for msg in ["Still waiting on you: merging #1273 into dev", "PR #1250 is open, so you need to merge it. I'll wait."] {
            assert!(!idle_ready("tw-idle", msg), "a real wait: {msg}");
        }
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
        b.restore_from(Saved::load(&path), &mut look, &|_| false, now);
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
    fn a_restart_keeps_every_card_that_waits_for_the_user() {
        let dir = tmp("restart-waits");
        let path = dir.0.join("questions.json");
        let idle = |_: &str| Terminal::Live { created_ms: Some(1), status: SessionStatus::Idle };
        {
            let (a, _) = stored(&path);
            // A real wait whose asking sentence, alone, reads as "ready".
            a.waiting("t9", "api", Some(1), "The build failed on the lint step. I'm waiting for your instructions.", "I'm waiting for your instructions.", "", false).unwrap();
            // A session that was asked something and ends "waiting for your instructions": its card too.
            a.waiting("t8", "web", Some(1), "I'm waiting for your instructions.", "I'm waiting for your instructions.", "", false).unwrap();
            assert_eq!(a.list().len(), 2);
        }
        // The tower restarts (an install, `tower restart`): both are still the user's.
        let (b, _) = stored(&path);
        b.restore_from(Saved::load(&path), &mut { idle }, &|_| false, 2);
        assert_eq!(b.list().iter().map(|s| s.session.as_str()).collect::<Vec<_>>(), ["t9", "t8"], "no card is dropped by a restart");
        // An older tower's file (it has no mark): its card for a turn that only said "ready" goes, once,
        // judged by the whole message; a real wait and one of a session that was asked something stay.
        let mut older: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        older.as_object_mut().unwrap().remove("ready_checked");
        std::fs::write(&path, older.to_string()).unwrap();
        let (c, _) = stored(&path);
        c.restore_from(Saved::load(&path), &mut { idle }, &|_| false, 3);
        assert_eq!(c.list().iter().map(|s| s.session.as_str()).collect::<Vec<_>>(), ["t9"]);
        std::fs::write(&path, older.to_string()).unwrap();
        let (d, _) = stored(&path);
        d.restore_from(Saved::load(&path), &mut { idle }, &|a| a == "t8", 3);
        assert_eq!(d.list().len(), 2);
    }

    #[test]
    fn a_broken_or_old_file_never_stops_the_start() {
        let dir = tmp("broken");
        let path = dir.0.join("questions.json");
        std::fs::write(&path, "{ not json").unwrap();
        let (a, _) = stored(&path);
        a.restore_from(Saved::load(&path), &mut |_| Terminal::Unknown, &|_| false, 1);
        assert!(a.list().is_empty());
        assert!(std::fs::read_dir(&dir.0).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("bad-")), "kept aside");
        // Words this tower doesn't know, and fields it doesn't have: loaded as open, undecided.
        std::fs::write(&path, r#"{"sets": [{"id": "q1", "session": "t9", "kind": "claude-permission", "status": "pondering", "decision": "later", "future": 1}], "extra": true}"#).unwrap();
        a.restore_from(Saved::load(&path), &mut |_| Terminal::Live { created_ms: None, status: SessionStatus::Blocked }, &|_| false, 1);
        let s = a.get("q1").unwrap();
        assert_eq!((s.status.as_str(), s.decision.as_deref()), ("open", None));
    }

    /// A prompt that only writes Claude's own memory (the memory note that
    /// waited 34 minutes for the user) is answered by the tower at once with
    /// its plain yes: no brain, no card. With "ask everything" it is the
    /// user's, as every other.
    #[test]
    fn a_prompt_that_only_writes_claudes_memory_is_answered_by_the_tower_at_once() {
        let home = crate::config::home().to_string_lossy().into_owned();
        let screen = questions::parse_screen(&fixture("claude-permission-memory-note.txt").replace("{home}", &home)).unwrap();
        let set = questions::build("t7p", "m5pro herder tower", false, &screen, None, "", "Don't merge or push.");
        let answering = |a: &mut Asks| {
            let went = Arc::new(Mutex::new(vec![]));
            let w = went.clone();
            a.answers = Arc::new(move |_: &Asks, id: &str| w.lock().unwrap_or_else(|e| e.into_inner()).push(id.to_string()));
            went
        };
        let (mut a, brain) = asks();
        let went = answering(&mut a);
        let s = a.upsert(set.clone());
        assert_eq!((s.state.decision, s.reason.as_str()), (Decision::Auto, MEMORY_YES));
        let rec = &s.recommendations.as_ref().unwrap()[0].options[0];
        assert_eq!((rec.label.as_str(), &rec.choice), ("Yes", &json!(0)));
        assert_eq!(*went.lock().unwrap(), [set.id.clone()], "the tower answers it");
        assert!(told(&brain, 1).is_empty(), "the brain isn't asked");
        // On the screen, just before its keys go in: its Yes, never its No.
        let pick = |i: usize| vec![Answer { tab: 0, choice: json!(i), attachments: vec![] }];
        assert_eq!(a.recheck(&set, &pick(0)).unwrap(), pick(0));
        assert!(a.recheck(&set, &pick(1)).is_err());
        // Asked to decide everything: the user's, through the brain as before.
        let (mut b, brain) = asks();
        let went = answering(&mut b);
        b.set_policy("ask-everything");
        let s = b.upsert(set.clone());
        assert_eq!(s.state.decision, Decision::Deciding);
        assert!(went.lock().unwrap().is_empty());
        assert_eq!(told(&brain, 1).len(), 1);
        // Another sensitive file: the brain decides it, as before.
        let other = questions::build("t7p", "m5pro herder tower", false, &questions::parse_screen(&fixture("claude-permission-memory-note.txt").replace("{home}", &home).replace("/memory/quiet-tower-implementation.md which is", "/settings.json which is")).unwrap(), None, "", "");
        let (mut c, brain) = asks();
        let went = answering(&mut c);
        assert_eq!(c.upsert(other).state.decision, Decision::Deciding);
        assert!(went.lock().unwrap().is_empty());
        assert_eq!(told(&brain, 1).len(), 1);
    }

    /// A file prompt for a Claude folder (the memory edit through orca's
    /// profile that waited 20 minutes as a bare card) is answered by the
    /// tower at once with its plain yes; one for a file elsewhere, and a
    /// menu the tower doesn't know, are questions for the brain.
    #[test]
    fn a_file_prompt_for_a_claude_folder_is_answered_at_once_and_others_go_to_the_brain() {
        let home = crate::config::home().to_string_lossy().into_owned();
        let answering = |a: &mut Asks| {
            let went = Arc::new(Mutex::new(vec![]));
            let w = went.clone();
            a.answers = Arc::new(move |_: &Asks, id: &str| w.lock().unwrap_or_else(|e| e.into_inner()).push(id.to_string()));
            went
        };
        let screen = questions::parse_screen(&fixture("claude-edit-claude-folder.txt").replace("{home}", &home)).unwrap();
        let msg = format!("approve Update: {home}/Library/Application Support/orca/claude-profiles/4c1e9b2a-0000-4000-8000-000000000001/home/projects/-Users-me-Work-shop/memory/search-notes.md");
        let set = questions::build("t12v", "mediaroom", false, &screen, None, &msg, "Measure the search.");
        let (mut a, brain) = asks();
        let went = answering(&mut a);
        let s = a.upsert(set.clone());
        assert_eq!((s.state.decision, s.reason.as_str()), (Decision::Auto, MEMORY_YES));
        assert_eq!(*went.lock().unwrap(), [set.id.clone()], "the tower answers it");
        assert!(told(&brain, 1).is_empty(), "the brain isn't asked");
        let pick = |i: usize| vec![Answer { tab: 0, choice: json!(i), attachments: vec![] }];
        assert_eq!(a.recheck(&set, &pick(0)).unwrap(), pick(0), "its Yes goes in, checked on the screen again");
        assert!(a.recheck(&set, &pick(1)).is_err(), "never its No");

        // A file in the session's own project: the brain's question, with its three options.
        let edit = questions::build("t12t", "sooner", false, &questions::parse_screen(&fixture("claude-edit-file.txt")).unwrap(), None, "approve Update: /Users/me/Work/shop/src/search.rs", "");
        let (mut b, brain) = asks();
        let went = answering(&mut b);
        assert_eq!(b.upsert(edit.clone()).state.decision, Decision::Deciding);
        assert!(went.lock().unwrap().is_empty());
        let note = told(&brain, 1);
        assert!(note.len() == 1 && note[0].starts_with(&format!("[question] t12t {}", edit.id)) && note[0].contains("Yes, allow all edits"), "{note:?}");

        // A dialog of another kind: the brain's question as well, not a bare card.
        let menu = questions::build("t5", "db", false, &questions::parse_screen(&fixture("claude-unknown-menu.txt")).unwrap(), None, "", "");
        let (mut c, brain) = asks();
        let went = answering(&mut c);
        assert_eq!(c.upsert(menu.clone()).state.decision, Decision::Deciding);
        assert!(went.lock().unwrap().is_empty());
        assert!(told(&brain, 1)[0].contains("Use this MCP server?"));

        // Put to the user (the brain escalated it): a decision with the dialog's own options,
        // each with its answer, so the phone can answer it as the Mac does, not "Show on Mac".
        let mut mine = edit.clone();
        card::apply(&mut mine, &Ev::Decide { auto: false }).unwrap();
        let asked = crate::decisions::asked(&mine).expect("the user's");
        assert_eq!(asked.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Yes", "Yes, allow all edits during this session (shift+tab)", "No, and tell Claude what to do differently (esc)"]);
        assert_eq!(asked.options[2].answers, json!([{ "tab": 0, "choice": 2 }]));
        assert_eq!(asked.who.question_id.as_deref(), Some(edit.id.as_str()));
    }

    /// `cargo test replay_a_captured_prompt_through_the_tower -- --ignored
    /// --nocapture` with `HERDER_TOWER_REPLAY=<a screen>` (and
    /// `HERDER_TOWER_REPLAY_CONTEXT=<what the session was asked>`,
    /// `HERDER_TOWER_REPLAY_MSG=<Herdr's status line for it>`): the
    /// screen as the radar would hand it to the tower, said: how it is
    /// decided, and whether the brain or the user would get it. Nothing is typed.
    #[test]
    #[ignore]
    fn replay_a_captured_prompt_through_the_tower() {
        let screen = std::fs::read_to_string(std::env::var("HERDER_TOWER_REPLAY").expect("HERDER_TOWER_REPLAY=<screen file>")).unwrap();
        let context = std::env::var("HERDER_TOWER_REPLAY_CONTEXT").ok().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        // `HERDER_TOWER_REPLAY_MSG`: Herdr's status line for it (`approve Update: /path`), as `hc get` had it.
        let msg = std::env::var("HERDER_TOWER_REPLAY_MSG").unwrap_or_default();
        let set = questions::build("t7p", "replay", false, &questions::parse_screen(&screen).expect("no dialog on that screen"), None, &msg, &context);
        eprintln!("read as {} {:?}: {:?} {:?}", set.kind, set.title, set.tabs[0].question, set.tabs[0].options.iter().map(|o| &o.label).collect::<Vec<_>>());
        let (mut a, brain) = asks();
        let went = Arc::new(Mutex::new(vec![]));
        let w = went.clone();
        a.answers = Arc::new(move |_: &Asks, id: &str| w.lock().unwrap_or_else(|e| e.into_inner()).push(id.to_string()));
        let s = a.upsert(set.clone());
        let pick = s.recommendations.as_ref().and_then(|r| r.first()).and_then(|r| r.options.first()).map(|o| o.label.clone());
        eprintln!("decision: {:?}, because {:?}; it answers {pick:?}", s.state.decision, s.reason);
        eprintln!("answered by the tower: {}; the brain asked: {}", !went.lock().unwrap().is_empty(), !told(&brain, 1).is_empty());
        eprintln!("checked again on the screen, its Yes: {:?}", a.recheck(&set, &[Answer { tab: 0, choice: json!(0), attachments: vec![] }]).map(|_| "goes in"));
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
        // Claude Code's newer name for an edit, with the path Herdr cut short (the evening of 13 Nov: a raw path on the phone).
        assert_eq!(n("permission", "approve Update: /w/app/src/login.rs"), "The orb session asks to edit login.rs");
        assert_eq!(
            n("permission", "approve Update: /Users/me/Library/Application Support/orca/claude-profiles/1f2995fa-2a8…"),
            "The orb session asks to edit its Claude notes"
        );
        assert_eq!(n("permission", "approve Write: /Users/me/Work/app/src/very/long/pa…"), "The orb session asks to edit a file");
        assert_eq!(blocked_title(&json!({ "kind": "permission", "msg": "approve Update: /Users/me/.claude/projects/x/memory/notes.md" })), "Asks to edit notes.md");
        assert_eq!(n("permission", "approve mcp__x__deploy: prod"), "The orb session asks to use mcp__x__deploy: prod");
        assert_eq!(n("question", ""), "The orb session has questions");
        assert_eq!(n("", ""), "The orb session needs you", "nothing to say: the bare notice");
        let long = format!("approve Bash: {}", "x".repeat(200));
        assert!(n("permission", &long).chars().count() < 120);
    }

    // ---- answered in the session's own terminal ----

    const PERM: &str = "approve Bash: touch /tmp/qfix-perm-test";

    /// The tower's three picks for the permission prompt, and the user's to decide.
    fn escalate_perm(a: &Asks, set: &QuestionSet) {
        let recs = json!([{"tab": 0, "options": [{"label": "Yes", "why": "it only touches a file in /tmp", "choice": 0}]}]);
        a.recommend(&set.id, &recs, Some("escalate"), "the user decides", false).unwrap();
    }

    fn open_decisions(a: &Asks) -> usize {
        a.shared.get().decisions.len()
    }

    /// The user answers a permission prompt in the session's terminal instead
    /// of on its card: the card and its decision go at once, as answered; the
    /// record says so with what the screen shows was chosen; nothing is said;
    /// a late card answer, a late recommendation, a screen read before the
    /// answer: none brings anything back or types anything.
    #[test]
    fn a_dialog_answered_in_its_terminal_closes_its_decision_at_once() {
        let screen = "⏺ Bash(touch /tmp/qfix-perm-test)\n  ⎿  User rejected tool use\n\n❯ \n";
        let (a, d, user, record) = recording(Some(screen.into()));
        let mut set = perm(PERM);
        set.at_ms = crate::history::now_ms();
        a.upsert(set.clone());
        escalate_perm(&a, &set);
        assert_eq!(open_decisions(&a), 1);
        let id = d.for_question(&set.id)[0].clone();
        assert_eq!(user.lock().unwrap().len(), 1, "said once, as it appeared");

        // The radar: the session works again, and nothing of the tower's went in.
        let before = crate::history::now_ms();
        a.seen("t37", &Value::Null, &SessionStatus::parse("working"));
        assert!(a.list().is_empty());
        assert_eq!(open_decisions(&a), 0, "off the Mac's windows and the phone with this state");
        let closed = d.get(&id).unwrap();
        assert_eq!(closed.state, crate::decisions::State::Answered);
        let line = lines(&record, 1);
        assert_eq!(line.len(), 1);
        let e = &line[0];
        assert_eq!((e.kind.as_str(), e.status.as_deref(), e.source.as_deref(), e.id.as_deref()), ("decision", Some("answered"), Some("terminal:t37"), Some(id.as_str())));
        assert_eq!(e.text, "Allow Bash: touch /tmp/qfix-perm-test? -> Answered in qfix perm: No");
        assert!(e.record, "for the record, never a chat message");
        assert_eq!(d.get(&id).unwrap().answer, "in its terminal: No");
        assert_eq!(user.lock().unwrap().len(), 1, "nothing more is said");

        // The phone's card a moment late: already answered, nothing typed.
        let e = a.answer(&set.id, &ans0(), true).unwrap_err();
        assert!(format!("{e:#}").starts_with("already answered"), "{e:#}");
        assert!(a.recommend(&set.id, &json!([]), Some("auto"), "", false).is_err(), "a recommendation still coming finds it over");
        a.escalate(&set.id, "no recommendation came in time");
        a.auto_answer(&set.id);
        assert_eq!(user.lock().unwrap().len(), 1, "no late escalation is said");

        // A reader a step behind: the screen it read had the dialog, before the answer. It stays away.
        let mut stale = set.clone();
        stale.at_ms = before;
        a.upsert(stale);
        assert!(a.list().is_empty() && open_decisions(&a) == 0, "no card comes back");
        assert_eq!(lines(&record, 2).len(), 1, "one line");
        // The same prompt asked again later is a new question.
        let mut again = perm(PERM);
        again.at_ms = crate::history::now_ms() + 5;
        a.upsert(again);
        assert_eq!(a.list().len(), 1);
    }

    /// Three sessions ask the same; the user answers one in its terminal: that
    /// session leaves the decision (its line is in the record), the others
    /// still wait on theirs, and the decision ends with the last of them.
    #[test]
    fn one_session_answered_in_its_terminal_leaves_a_decision_others_still_wait_on() {
        let (a, d, _user, record) = recording(None);
        let other = |session: &str| {
            let screen = questions::parse_screen(&fixture("claude-permission.txt")).unwrap();
            let mut s = questions::build(session, "qfix two", false, &screen, None, PERM, "");
            s.at_ms = crate::history::now_ms();
            s
        };
        let (one, two) = (other("t37"), other("t38"));
        for s in [&one, &two] {
            a.upsert(s.clone());
            escalate_perm(&a, s);
        }
        let id = d.for_question(&one.id)[0].clone();
        assert_eq!(d.get(&id).unwrap().sessions.len(), 2, "one decision");
        a.seen("t37", &Value::Null, &SessionStatus::parse("idle"));
        let open = d.get(&id).unwrap();
        assert!(open.closed_ms.is_none() && open.sessions.len() == 1 && open.sessions[0].address == "t38");
        assert_eq!(open_decisions(&a), 1);
        assert_eq!(lines(&record, 1)[0].text, "Allow Bash: touch /tmp/qfix-perm-test? -> Answered in its terminal", "nothing on the screen says what");
        a.seen("t38", &Value::Null, &SessionStatus::parse("working"));
        assert_eq!(d.get(&id).unwrap().state, crate::decisions::State::Answered);
        assert_eq!(open_decisions(&a), 0);
        assert_eq!(lines(&record, 2).len(), 2);
    }

    fn text_q(session: &str, name: &str) -> QuestionSet {
        let mut q = questions::text_set(session, name, "I made the change in src/api.rs. Want me to open the PR, or wait?", "");
        q.at_ms = crate::history::now_ms();
        q
    }

    /// The user answers a session's question by telling a brain, which passes
    /// it on (`tower queue`): the question and its card close, answered
    /// through the tower. What the tower passes on that doesn't answer it,
    /// and anything when the user said nothing since the question came,
    /// leaves the card waiting for them.
    #[test]
    fn the_users_answer_passed_on_by_the_tower_closes_the_question() {
        let (mut a, d, _user, record) = recording(None);
        let asked = Arc::new(Mutex::new(vec![]));
        let seen = asked.clone();
        a.relays = Arc::new(move |name: &str, question: &str, users: &str, message: &str| {
            seen.lock().unwrap().push(format!("{name} | {users} | {message} | {}", question.contains("open the PR, or wait?")));
            Ok(message.contains("go ahead"))
        });
        // The question is theirs (a card), then the user tells the tower, which passes it on.
        let q = a.asked_after(text_q("tw-rl1", "eyzflow"), false);
        a.escalate(&q.id, "the user decides");
        let id = d.for_question(&q.id)[0].clone();
        let said = q.at_ms + 1000;
        a.seen("tw-rl1", &Value::Null, &SessionStatus::parse("working"));
        a.typed_by_tower("tw-rl1", None);
        assert_eq!(a.relayed("tw-rl1", "its saved. go ahead", said, "The user saved the webhook; go ahead and open the PR."), vec![q.id.clone()]);
        assert!(a.get(&q.id).is_none() && open_decisions(&a) == 0, "settled, and its card with it");
        let decided = d.get(&id).unwrap();
        assert_eq!(decided.state, crate::decisions::State::Answered);
        assert_eq!(decided.answer, "answered through the tower: The user saved the webhook; go ahead and open the PR.");
        assert_eq!(asked.lock().unwrap().as_slice(), ["eyzflow | its saved. go ahead | The user saved the webhook; go ahead and open the PR. | true"]);
        // (Its line in the record is the decision's: "<question> -> answered through the tower: …".)
        let _ = &record;

        // A card that came at once (a turn the user typed in its terminal): the same.
        let mut typed = questions::text_set("tw-rl0", "web", "Done. Should I add the line hello to notes.txt now, or wait?", "");
        typed.at_ms = crate::history::now_ms();
        let q0 = a.asked_after(typed, true);
        let id0 = d.for_question(&q0.id)[0].clone();
        assert_eq!(a.relayed("tw-rl0", "yes, add it", q0.at_ms + 1000, "The user says: go ahead, add the line hello to notes.txt now."), vec![q0.id.clone()]);
        assert_eq!(d.get(&id0).unwrap().answer, "answered through the tower: The user says: go ahead, add the line hello to notes.txt now.");

        // Passed on, but about something else: still theirs.
        let q2 = a.asked_after(text_q("tw-rl2", "api"), false);
        a.escalate(&q2.id, "the user decides");
        assert!(a.relayed("tw-rl2", "is the import done?", q2.at_ms + 1000, "For your information: the import ran.").is_empty());
        assert!(a.get(&q2.id).is_some() && open_decisions(&a) == 1, "still waits for the user");

        // Another session asks the same (one card, two waiting): the answer passed on to
        // one leaves the card to the other, who still waits.
        let q3 = a.asked_after(text_q("tw-rl3", "docs"), false);
        a.escalate(&q3.id, "the user decides");
        let shared = d.for_question(&q3.id)[0].clone();
        assert_eq!(d.for_question(&q2.id), vec![shared.clone()], "the same question: one card");
        assert_eq!(a.relayed("tw-rl3", "go ahead", q3.at_ms + 1000, "go ahead with it"), vec![q3.id.clone()]);
        assert!(a.get(&q3.id).is_none() && a.get(&q2.id).is_some());
        let card = d.get(&shared).unwrap();
        assert!(card.closed_ms.is_none() && card.sessions.iter().all(|w| w.address == "tw-rl2"), "the other session still waits on it");

        // The user spoke before the session asked: whatever the brain sends, it isn't their answer to it.
        let mut later = questions::text_set("tw-rl4", "sdk", "The build is green. Ship it to staging now, or hold?", "");
        later.at_ms = crate::history::now_ms();
        let q4 = a.asked_after(later, false);
        a.escalate(&q4.id, "the user decides");
        let before = asked.lock().unwrap().len();
        assert!(a.relayed("tw-rl4", "go ahead", q4.at_ms.saturating_sub(5_000), "go ahead with it").is_empty());
        assert_eq!(asked.lock().unwrap().len(), before, "not even asked");
        assert!(a.get(&q4.id).is_some());
        assert_eq!(open_decisions(&a), 2, "both still wait");

        // The judge fails (no model): it stays theirs.
        a.relays = Arc::new(|_: &str, _: &str, _: &str, _: &str| Err(anyhow::anyhow!("claude: timed out")));
        assert!(a.relayed("tw-rl2", "go ahead", q2.at_ms + 2000, "go ahead").is_empty());
        assert!(a.get(&q2.id).is_some());
    }

    /// Waits until the set's buttons are no longer the generic ones (or a second has gone by).
    fn own_buttons(a: &Asks, id: &str) -> Vec<String> {
        for _ in 0..200 {
            let recs = a.get(id).and_then(|s| s.recommendations).and_then(|r| r.first().cloned()).map(|t| t.options).unwrap_or_default();
            if !crate::choices::is_generic(&recs) {
                return recs.iter().map(|r| r.label.clone()).collect();
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        vec![]
    }

    /// A turn that ends waiting for the user is a card with buttons: "Go
    /// ahead" and "Not yet" at once, then two or three of its own, read from
    /// what the session said, the tower's pick first; each is an option of
    /// its decision (the phone's buttons) and sends its instruction. A vague
    /// wait keeps the generic ones; one answered before its own came keeps
    /// what it had; a question the brain read keeps its own answers.
    #[test]
    fn a_card_that_waits_for_the_user_has_buttons_to_answer_it() {
        let (mut a, d, _user, _record) = recording(None);
        let message = "Production confirms the number: 22 offers. Both commits (1a235c3a, 8d2d649d) are still local on isa/eyzflow, waiting for your call on a push and PR.";
        let request = "Both commits are still local on isa/eyzflow, waiting for your call on a push and PR.";
        let asked = Arc::new(Mutex::new(vec![]));
        let seen = asked.clone();
        a.choices = Arc::new(move |name: &str, msg: &str, req: &str| -> Result<Vec<questions::Rec>> {
            seen.lock().unwrap().push(format!("{name} | {} | {req}", msg.contains("22 offers")));
            // As the model takes a moment.
            std::thread::sleep(std::time::Duration::from_millis(150));
            if !req.contains("push") {
                bail!("nothing specific");
            }
            Ok(vec![
                questions::Rec { label: "Push and open the PR into dev".into(), why: "production confirmed the count".into(), choice: json!({ "other": "Push both commits and open a PR into dev." }) },
                questions::Rec { label: "Push, no PR yet".into(), why: String::new(), choice: json!({ "other": "Push both commits, but don't open a PR yet." }) },
                questions::Rec { label: "Not yet".into(), why: String::new(), choice: json!({ "other": "Not yet: keep them local and wait for me." }) },
            ])
        });
        let set = a.waiting("tw-b1", "eyzflow", None, message, request, "", true).unwrap();
        // At once: never a card with nothing to tap.
        let now = a.get(&set.id).unwrap();
        assert_eq!(now.recommendations.as_ref().unwrap()[0].options, crate::choices::generic());
        // A moment later: its own, read from what it said.
        assert_eq!(own_buttons(&a, &set.id), ["Push and open the PR into dev", "Push, no PR yet", "Not yet"]);
        assert_eq!(asked.lock().unwrap().as_slice(), [format!("eyzflow | true | {request}")]);
        // Its decision offers them, the tower's pick first: the phone shows the same.
        let id = d.for_question(&set.id)[0].clone();
        let opts = d.get(&id).unwrap().options;
        assert_eq!(opts.iter().map(|o| (o.label.as_str(), o.pick)).collect::<Vec<_>>(), [("Push and open the PR into dev", true), ("Push, no PR yet", false), ("Not yet", false)]);
        assert_eq!(opts[0].why, "production confirmed the count");
        // A tap sends its instruction (the reply typed into the session).
        let answers: Vec<questions::Answer> = serde_json::from_value(opts[0].answers.clone()).unwrap();
        assert_eq!(questions::compose_reply(&a.get(&set.id).unwrap(), &answers).unwrap(), "Push both commits and open a PR into dev.");

        // A vague wait: nothing specific comes, the generic ones stay, and they send plain words.
        let vague = a.waiting("tw-b2", "api", None, "Done on my side.", "Waits for you.", "", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let recs = a.get(&vague.id).unwrap().recommendations.unwrap()[0].options.clone();
        assert!(crate::choices::is_generic(&recs));
        let did = d.get(&d.for_question(&vague.id)[0]).unwrap().options;
        assert_eq!(did.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Go ahead", "Not yet"]);
        let go: Vec<questions::Answer> = serde_json::from_value(did[0].answers.clone()).unwrap();
        assert_eq!(questions::compose_reply(&a.get(&vague.id).unwrap(), &go).unwrap(), "Go ahead with what you proposed.");

        // A question after a turn the user typed: buttons too (generic here: the fake reads only pushes).
        let typed = a.asked_after(text_q("tw-b3", "web"), true);
        assert!(a.get(&typed.id).unwrap().recommendations.is_some_and(|r| !r[0].options.is_empty()));
        // A question the brain still reads (it recommends its own): no buttons from here.
        let read = a.asked_after(text_q("tw-b4", "docs"), false);
        assert!(a.get(&read.id).unwrap().recommendations.is_none());
    }

    /// The user replies in the terminal to the question a session asked at
    /// the end of its turn: the card and its decision close as answered, with
    /// the user's words, whether the hook or the radar is first; a reply the
    /// tower held is not sent; and when the tower gave the session its next
    /// prompt, nobody answered it there.
    #[test]
    fn a_question_answered_by_a_prompt_in_the_terminal_closes_with_the_users_words() {
        let (a, d, user, record) = recording(None);
        // The hook first: the card is still up.
        let q1 = a.asked_after(text_q("tw-q1", "web"), true);
        assert_eq!(open_decisions(&a), 1, "a turn the user typed there: their card at once");
        let id1 = d.for_question(&q1.id)[0].clone();
        assert!(a.typed_there("tw-q1", "Wait, I'll review it first", crate::history::now_ms()));
        assert!(a.list().is_empty() && open_decisions(&a) == 0);
        assert_eq!(d.get(&id1).unwrap().state, crate::decisions::State::Answered);
        let line = lines(&record, 1);
        assert!(line[0].text.ends_with("-> Answered in web: Wait, I'll review it first"), "{}", line[0].text);
        assert_eq!(d.get(&id1).unwrap().answer, "in its terminal: Wait, I'll review it first");

        // The radar first (the session works): the card waits for the hook to
        // say whose prompt that was; the user's words, a moment after, close it.
        let q2 = a.asked_after(text_q("tw-q2", "api"), false);
        a.escalate(&q2.id, "the user decides");
        assert_eq!(user.lock().unwrap().len(), 1);
        a.seen("tw-q2", &Value::Null, &SessionStatus::parse("working"));
        assert_eq!(open_decisions(&a), 1, "not answered until the hook says whose prompt it was");
        assert!(a.typed_there("tw-q2", "yes, open it", crate::history::now_ms()));
        assert_eq!(open_decisions(&a), 0, "the user's words: answered");
        let line = lines(&record, 2);
        assert!(line[1].text.ends_with("-> Answered in api: yes, open it"), "{}", line[1].text);

        // The tower typed the next prompt (the queue): not the user's answer. The
        // question stays theirs, and so does its decision; no line says it was answered.
        let q3 = a.asked_after(text_q("tw-q3", "docs"), false);
        a.escalate(&q3.id, "the user decides");
        a.seen("tw-q3", &Value::Null, &SessionStatus::parse("working"));
        a.typed_by_tower("tw-q3", None);
        std::thread::sleep(std::time::Duration::from_millis(1_800));
        assert!(a.get(&q3.id).is_some() && open_decisions(&a) == 1, "still open");
        assert_eq!(lines(&record, 3).len(), 2, "nothing says the user answered it");
        assert_eq!(user.lock().unwrap().len(), 2, "two said as they appeared, nothing since");
        assert!(!a.typed_there("tw-q9", "build the docs site", crate::history::now_ms()), "no question open: new work, not an answer");

        // The session works again and no hook ever says on whose prompt (an
        // agent with none): a moment later it was answered in its terminal, as before.
        let q4 = a.asked_after(text_q("tw-q4", "sdk"), false);
        a.escalate(&q4.id, "the user decides");
        a.seen("tw-q4", &Value::Null, &SessionStatus::parse("working"));
        assert!(a.get(&q4.id).is_some());
        std::thread::sleep(std::time::Duration::from_millis(1_800));
        assert!(a.get(&q4.id).is_none(), "over once no word came");
        let line = lines(&record, 3);
        assert!(line[2].text.ends_with("-> Answered in its terminal"), "{}", line[2].text);
    }

    /// A message from another session (or anything the tower typed) that
    /// arrives after a question at the end of a turn leaves that question,
    /// and its decision, open: the session works on it, but nobody answered.
    /// Only the user's own words (on the card, or typed in its terminal)
    /// answer it.
    #[test]
    fn a_message_from_another_session_leaves_the_users_question_open() {
        let (a, d, user, record) = recording(None);
        let q = a.asked_after(text_q("tw-pm1", "web"), false);
        a.escalate(&q.id, "the user decides");
        let id = d.for_question(&q.id)[0].clone();
        assert_eq!(open_decisions(&a), 1);
        // The send queue types a peer message in (written down first, as `Herdr::send` does).
        let typed = super::super::typed::note("tw-pm1", "From Shop › api (t6): the schema changed, see api/CHANGES.md", "queue");
        super::super::typed::done(typed, true);
        // 0.3 s later the radar sees the session working; its hook comes after.
        std::thread::sleep(std::time::Duration::from_millis(300));
        a.seen("tw-pm1", &json!({ "turn": 4 }), &SessionStatus::Working);
        assert!(a.get(&q.id).is_some(), "the question stays the user's");
        assert_eq!(d.get(&id).map(|x| x.state), Some(crate::decisions::State::Open), "and so does its decision");
        a.typed_by_tower("tw-pm1", Some(4));
        for _ in 0..3 {
            a.seen("tw-pm1", &json!({ "turn": 4 }), &SessionStatus::Working);
        }
        std::thread::sleep(std::time::Duration::from_millis(1_800));
        assert!(a.get(&q.id).is_some() && open_decisions(&a) == 1, "still open once the moment for the hook is over");
        // Its turn on the message ends without asking again: the question still waits for the user.
        a.seen("tw-pm1", &json!({ "turn": 5 }), &SessionStatus::Done);
        assert!(a.get(&q.id).is_some());
        assert!(lines(&record, 1).is_empty(), "nothing says it was answered");
        // The user answers it in the terminal: now it is answered, with their words.
        a.seen("tw-pm1", &json!({ "turn": 5 }), &SessionStatus::Working);
        assert!(a.typed_there("tw-pm1", "open the PR", crate::history::now_ms()));
        assert_eq!(d.get(&id).map(|x| x.state), Some(crate::decisions::State::Answered));
        let line = lines(&record, 1);
        assert!(line[0].text.ends_with("-> Answered in web: open the PR"), "{}", line[0].text);

        // Hook first: a session's message that came in some other way (another
        // terminal's `hc send`), then the radar: open as well.
        let q2 = a.asked_after(text_q("tw-pm2", "api"), false);
        a.escalate(&q2.id, "the user decides");
        a.typed_by_tower("tw-pm2", Some(9));
        a.seen("tw-pm2", &json!({ "turn": 9 }), &SessionStatus::Working);
        std::thread::sleep(std::time::Duration::from_millis(1_800));
        assert!(a.get(&q2.id).is_some(), "another terminal's prompt is no answer either");
        // A word about an earlier stretch of work says nothing about this one.
        let q3 = a.asked_after(text_q("tw-pm3", "docs"), false);
        a.escalate(&q3.id, "the user decides");
        a.typed_by_tower("tw-pm3", Some(2));
        a.seen("tw-pm3", &json!({ "turn": 3 }), &SessionStatus::Working);
        std::thread::sleep(std::time::Duration::from_millis(1_800));
        assert!(a.get(&q3.id).is_none(), "the turn it worked on before isn't this one: answered in its terminal");
        // An answer the user gave on the card goes in as the session's next prompt: over, as before.
        let q4 = a.asked_after(text_q("tw-pm4", "sdk"), false);
        a.escalate(&q4.id, "the user decides");
        {
            let mut i = a.inner.lock().unwrap_or_else(|e| e.into_inner());
            let s = i.sets.iter_mut().find(|s| s.id == q4.id).unwrap();
            card::apply(s, &Ev::Reply { queued: false }).unwrap();
        }
        a.seen("tw-pm4", &json!({ "turn": 1 }), &SessionStatus::Working);
        assert!(a.get(&q4.id).is_none(), "its own reply: over at once");
        assert!(!user.lock().unwrap().is_empty(), "the question was put to the user, once");
    }

    /// The user answered on the card while the session was busy (the reply
    /// waits for its turn to end), then typed their answer in the terminal:
    /// the held reply is never typed, and the record says so.
    #[test]
    fn a_reply_the_tower_held_is_not_sent_once_the_user_answers_in_the_terminal() {
        let (a, _d, user, record) = recording(None);
        let r = Reply { set: "q1".into(), text: "Yes, open the PR.".into(), at_ms: 1_000, turn: Some(4), last: SessionStatus::Blocked, created_ms: None, name: "web".into() };
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("tw-r1".into(), r);
        assert!(a.typed_there("tw-r1", "no, wait", crate::history::now_ms()));
        assert!(a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.is_empty());
        assert_eq!(a.due_reply("tw-r1", &SessionStatus::parse("idle"), Some(5), None, 2_000), None, "nothing left to type");
        let line = lines(&record, 1);
        assert_eq!((line[0].kind.as_str(), line[0].status.as_deref()), ("queue", Some("not-sent")));
        assert!(line[0].text.starts_with("Not sent: you answered it in the terminal") && line[0].text.contains("Yes, open the PR."), "{}", line[0].text);
        assert!(user.lock().unwrap().is_empty(), "never said");
    }

    /// Words typed in the terminal about something else (a link to keep in
    /// mind) are no answer: the reply the user gave on the card still goes.
    #[test]
    fn words_about_something_else_leave_the_held_reply_to_go() {
        let (a, _d, _user, record) = recording(None);
        let r = Reply { set: "q1".into(), text: "Yes, open the release PR.".into(), at_ms: 1_000, turn: Some(4), last: SessionStatus::Blocked, created_ms: None, name: "eyzflow".into() };
        a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("tw-r2".into(), r);
        assert!(!a.typed_there("tw-r2", "https://claude.ai/artifact/X keep in mind to update this artifact", crate::history::now_ms()), "new work, not an answer");
        assert!(a.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.contains_key("tw-r2"), "the card's answer still waits");
        assert!(record.lock().unwrap().is_empty(), "nothing said to be not sent");
        // A check that fails keeps it too.
        let (mut b, _d, _user, _record) = recording(None);
        b.covers = Arc::new(|_: &str, _: &str, _: &str| Err(anyhow::anyhow!("claude: timed out")));
        let r = Reply { set: "q2".into(), text: "Yes, open the release PR.".into(), at_ms: 1_000, turn: Some(4), last: SessionStatus::Blocked, created_ms: None, name: "eyzflow".into() };
        b.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("tw-r3".into(), r);
        assert!(!b.typed_there("tw-r3", "yes do it", crate::history::now_ms()));
        assert!(b.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.contains_key("tw-r3"));
        // The very words of the reply need no model.
        let (mut c, _d, _user, _record) = recording(None);
        c.covers = Arc::new(|_: &str, _: &str, _: &str| Err(anyhow::anyhow!("not asked")));
        let r = Reply { set: "q3".into(), text: "Yes, open the release PR.".into(), at_ms: 1_000, turn: Some(4), last: SessionStatus::Blocked, created_ms: None, name: "eyzflow".into() };
        c.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.insert("tw-r4".into(), r);
        assert!(c.typed_there("tw-r4", "yes, open the release PR.", crate::history::now_ms()));
        assert!(!c.inner.lock().unwrap_or_else(|e| e.into_inner()).replies.contains_key("tw-r4"));
    }

    /// The question at the end of a turn the user typed in its terminal is
    /// theirs as it is: a card, no note to the brain, nothing said.
    #[test]
    fn a_question_after_a_turn_the_user_typed_goes_to_them_quietly() {
        let (a, brain) = asks();
        let d = super::super::decisions::Decisions::telling(a.shared.clone(), || crate::tell::Level::Decisions);
        let a = a.with_decisions(d.clone());
        let user = Arc::new(Mutex::new(Vec::<String>::new()));
        let u = user.clone();
        a.set_notify(move |addr, text| u.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{addr}: {text}")));
        let q = a.asked_after(text_q("tw-o1", "web"), true);
        assert_eq!((q.decision.as_deref(), q.reason.as_str()), (Some("escalate"), questions::TYPED_THERE));
        assert_eq!(open_decisions(&a), 1);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(brain.lock().unwrap().is_empty(), "the brain is not asked to answer it");
        assert!(user.lock().unwrap().is_empty(), "nothing said");
        // A wait at the end of such a turn: its card, not said either.
        let msg = "PR #12 is open. You need to merge it; I'll wait.";
        a.waiting_in(None, "tw-o2", "api", None, msg, &questions::waits_for_user(msg).unwrap(), "", false, true).unwrap();
        assert_eq!(open_decisions(&a), 2);
        assert!(user.lock().unwrap().is_empty());
        // Another session's own question still goes through the brain.
        a.asked_after(text_q("tw-o3", "docs"), false);
        assert_eq!(told(&brain, 1).len(), 1);
    }

    /// The driver presses nothing once the set it answers is over (the
    /// user answered the dialog in its terminal as the keys went in).
    #[test]
    fn nothing_is_typed_into_a_dialog_that_is_gone() {
        struct Fake {
            screen: String,
            keys: Vec<String>,
        }
        impl questions::Term for Fake {
            fn read(&mut self) -> String {
                if self.keys.is_empty() { self.screen.clone() } else { String::new() }
            }
            fn keys(&mut self, keys: &[&str]) -> Result<()> {
                self.keys.extend(keys.iter().map(|k| k.to_string()));
                Ok(())
            }
            fn paste(&mut self, text: &str) -> Result<()> {
                self.keys.push(text.to_string());
                Ok(())
            }
        }
        let set = perm(PERM);
        let closed = || false;
        let mut term = Guarded { term: Fake { screen: fixture("claude-permission.txt"), keys: vec![] }, open: &closed };
        let stop = questions::drive(&mut term, &set, &ans0(), true).unwrap_err();
        assert!(stop.message.contains("answered in its terminal"), "{stop}");
        assert!(term.term.keys.is_empty(), "not a key");
        let open = || true;
        let mut term = Guarded { term: Fake { screen: fixture("claude-permission.txt"), keys: vec![] }, open: &open };
        questions::drive(&mut term, &set, &ans0(), true).unwrap();
        assert_eq!(term.term.keys, ["1"], "while it is open, its answer goes in");

        // The next permission prompt (another command) is no redraw of this one: what was
        // decided for the first never goes into it.
        let mut next = questions::parse_screen(&fixture("claude-permission.txt")).unwrap();
        next.detail = next.detail.replace("touch /tmp/qfix-perm-test", "rm -rf /tmp/qfix");
        let next = questions::build("t37", "qfix perm", false, &next, None, "approve Bash: rm -rf /tmp/qfix", "");
        assert!(!questions::redrawn(&set, &next) && !questions::answers_still_fit(&set, &next, &ans0()));
        assert!(questions::redrawn(&set, &perm("approve Bash: touch it")), "the same command read under another status line still is");
    }

    /// A dialog the session still shows but the tower can't read any more is
    /// not answered: its card goes, nothing says the user answered it.
    #[test]
    fn a_dialog_out_of_sight_is_not_answered() {
        let (a, d, _user, record) = recording(None);
        let set = perm(PERM);
        a.upsert(set.clone());
        escalate_perm(&a, &set);
        let id = d.for_question(&set.id)[0].clone();
        a.close_where("t37", |s| s.kind != "text", |_| CloseWhy::Unread, true);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(lines(&record, 1).is_empty());
        assert_eq!(d.get(&id).unwrap().state, crate::decisions::State::Stopped);
    }

    /// The dialog read again as the user answers it in the terminal: the
    /// same set moves on (its tab, what is answered), keeps its decision, and
    /// what the review showed of an answer stays known once it doesn't show.
    #[test]
    fn a_dialog_read_again_tab_by_tab_follows_and_keeps_its_answers() {
        let (a, d, user) = deciding(|| crate::tell::Level::Decisions);
        let tabs = questions::transcript_questions(&fixture("claude-ask-transcript.jsonl")).unwrap();
        let read = |name: &str| {
            let mut s = questions::build("t21", "web", false, &questions::parse_screen(&fixture(name)).unwrap(), Some(tabs.clone()), "", "");
            s.at_ms = crate::history::now_ms();
            s
        };
        let first = read("claude-ask-tab1.txt");
        a.upsert(first.clone());
        a.escalate(&first.id, "the user decides");
        let id = d.for_question(&first.id)[0].clone();
        assert_eq!(user.lock().unwrap().len(), 1);
        // The user ticks Features in the terminal, then goes to the review.
        let review = read("claude-ask-review-tagged.txt");
        assert_eq!(review.id, first.id);
        a.upsert(review);
        let now = a.get(&first.id).unwrap();
        assert_eq!((now.current_tab, now.tabs[1].answered, now.tabs[1].answer.as_str(), now.decision.as_deref()), (3, true, "Search (full text)", Some("escalate")));
        assert!(d.get(&id).unwrap().title.contains("2 answered in the terminal"), "{}", d.get(&id).unwrap().title);
        // Back on a question tab: the review's answer isn't on screen any more, and is still known.
        let mut back = read("claude-ask-tab3.txt");
        back.tabs[1].answered = true;
        a.upsert(back);
        let now = a.get(&first.id).unwrap();
        assert_eq!((now.current_tab, now.tabs[1].answer.as_str()), (2, "Search (full text)"));
        assert_eq!(user.lock().unwrap().len(), 1, "nothing said again");
        assert_eq!(d.get(&id).unwrap().state, crate::decisions::State::Open);
    }
}
