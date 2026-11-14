//! Decisions: what only the user can decide, one card each (the "Yours" lane
//! of docs/quiet-tower.md, step 3).
//!
//! A decision comes from one of four places:
//!
//! - an escalated question set (`permission` for a permission, plan or
//!   approval menu, `question` otherwise): see `questions`, `daemon::asks`;
//! - the pull requests of one repo that wait for a merge in one space
//!   (`merge`): one decision lists them in order, a PR into `dev` before one
//!   into `main` (`prs::is_prod`), and changes in place as they open, merge
//!   or their checks move;
//! - `tower raise` (`raised`, or `choice` when it offers options);
//! - an agenda task that failed again after it was sent once more (`failed`);
//! - the sessions a restart of the Mac or Herdr closed, offered back (a
//!   `choice`, `why` "reopen": see `crate::reopen`); the tower answers it
//!   itself, nothing is sent to a session;
//! - every Claude account out of usage, with sessions waiting for one (a
//!   `choice`, `why` "accounts": see `crate::failover`), answered by the
//!   tower itself too.
//!
//! **One key per decision.** The same key again (another session asking the
//! same thing, the same line raised twice, another PR of the repo) updates
//! `last_ms` and `count` and adds its session: never a second decision, and
//! so never a second alert or spoken sentence at a quiet level (`tell`).
//!
//! A decision closes when it is answered (its question set is, its PRs merge
//! or close, `tower answer`), dismissed, or when all its sessions are gone.
//! A merge outlives its sessions: a PR that is still open and waiting is
//! still the user's, so its decision goes when its PRs no longer wait, or
//! when its space is closed or archived (`Book::space_over`).
//! The open ones are ordered by weight (sessions waiting x how long they
//! have), heaviest first, then oldest first.
//!
//! This is the model, kept in `decisions.json` (the state folder) across
//! restarts; `daemon::decisions` holds it in the daemon and says who is told.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::prs::Pr;
use crate::questions::QuestionSet;

/// Decisions that closed are kept this many deep: a late answer hears how one ended.
const KEEP_CLOSED: usize = 50;
/// A key's text is cut to this many characters.
const KEY_TEXT: usize = 300;
/// A decision's `detail` is cut to about this many characters.
pub const DETAIL: usize = 600;
/// The card that offers back what a restart closed (`Book::reopen`): its key, and its `why`.
pub const REOPEN_KEY: &str = "reopen";
pub const REOPEN_WHY: &str = "reopen";
/// The card saying every Claude account is out of usage (`Book::accounts_out`): its key, and its `why`.
pub const ACCOUNTS_KEY: &str = "accounts-out";
pub const ACCOUNTS_WHY: &str = "accounts";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A permission, plan or approval menu a session is blocked on.
    Permission,
    /// What a session asks, in a dialog or at the end of its turn (a wait for the user too).
    #[default]
    Question,
    /// The PRs of one repo waiting for a merge in one space.
    Merge,
    /// `tower raise` with options to choose from.
    Choice,
    /// An agenda task that failed again after a retry.
    Failed,
    /// `tower raise`.
    Raised,
    /// A session blocked on something the tower can't read (no question set
    /// comes of it): the user looks at it in Herdr. One per session, over
    /// once the session is no longer blocked.
    Blocked,
}

impl Kind {
    pub fn word(&self) -> &'static str {
        match self {
            Kind::Permission => "permission",
            Kind::Question => "question",
            Kind::Merge => "merge",
            Kind::Choice => "choice",
            Kind::Failed => "failed",
            Kind::Raised => "raised",
            Kind::Blocked => "blocked",
        }
    }

    /// It stands for question sets (its sessions' own).
    pub fn asks(&self) -> bool {
        matches!(self, Kind::Permission | Kind::Question)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Open,
    /// Its answer is going in (keys into a dialog, a reply waiting for its session).
    Answering,
    Answered,
    /// Over without an answer: its sessions are gone, its PRs closed.
    Stopped,
    Dismissed,
}

impl State {
    pub fn word(&self) -> &'static str {
        match self {
            State::Open => "open",
            State::Answering => "answering",
            State::Answered => "answered",
            State::Stopped => "stopped",
            State::Dismissed => "dismissed",
        }
    }

    /// How a decision whose sessions left one by one ended: one answer makes
    /// it answered, else a dismissal dismissed, else stopped.
    fn over(self, other: State) -> State {
        let rank = |s: State| match s {
            State::Answered => 3,
            State::Dismissed => 2,
            _ => 1,
        };
        if rank(other) > rank(self) { other } else { self }
    }
}

/// A session a decision is about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Who {
    pub address: String,
    pub name: String,
    /// Blocked on it now (false: it works on something else meanwhile).
    pub waiting: bool,
    /// Its own question set, for a decision that stands for question sets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
}

impl Default for Who {
    fn default() -> Who {
        Who { address: String::new(), name: String::new(), waiting: true, question_id: None }
    }
}

impl Who {
    pub fn new(address: &str, name: &str) -> Who {
        Who { address: address.into(), name: name.into(), ..Default::default() }
    }
}

/// One thing the user can choose.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Opt {
    pub label: String,
    pub why: String,
    /// The tower's pick (at most one).
    pub pick: bool,
    /// A question's option: the answers that choose it (`[{"tab", "choice"}]`).
    #[serde(skip_serializing_if = "Value::is_null")]
    pub answers: Value,
    /// A raised choice: the words sent to its sessions when it is chosen
    /// (none: a sentence made from the label).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
}

/// A PR in a merge decision, as its card needs it. Merging it is the PR's
/// own flow (the chat's chip, the phone's confirm), by `key`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrRef {
    /// `owner/name#123`.
    pub key: String,
    pub repo: String,
    pub number: u64,
    pub base: String,
    pub title: String,
    pub url: String,
    /// `pass`, `pending`, `fail`, or `none` (no checks ran).
    pub checks: String,
    /// Its checks in a few words ("1 check failing: lint").
    pub note: String,
    /// The session that opened it.
    pub session: String,
    /// Its head commit when last looked at: new commits on a decision on hold bring it back.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sha: String,
    /// Whether it can be merged now, and what to mind (`prs::Gate::view`).
    pub merge: Value,
    /// Its review (`crate::reviews::Brief`), when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::reviews::Brief>,
    /// Whether it needs a human code review, and why (`crate::prscore`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<crate::prscore::Score>,
    /// Its plan's checks and edge cases, and where each check stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation: Option<crate::prscore::Validation>,
    /// Who outside depends on what it changes, and whether that breaks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<crate::impact::Impact>,
    /// Its description on GitHub (`""`: none); None: not looked at yet (`crate::describe`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Its description in one or two plain sentences (empty: not made yet).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// It had no description and its session was asked to write one.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub asked_body: bool,
}

impl PrRef {
    pub fn of(p: &Pr) -> PrRef {
        PrRef {
            key: p.key(),
            repo: p.repo.clone(),
            number: p.number,
            base: p.base.clone(),
            title: p.title.clone(),
            url: p.url.clone(),
            checks: match p.checks.state.as_str() {
                "passing" => "pass",
                "failing" => "fail",
                "pending" => "pending",
                _ => "none",
            }
            .into(),
            note: p.checks_line(),
            session: p.session.clone(),
            sha: p.head_sha.clone(),
            merge: crate::prs::gate(p).view(),
            review: p.review.clone(),
            score: p.score.clone(),
            validation: p.validation.clone(),
            impact: p.impact.clone(),
            body: p.body.clone(),
            summary: p.described.as_ref().filter(|d| p.body.as_deref().is_some_and(|b| crate::describe::hash(b) == d.of)).map(|d| d.summary.clone()).unwrap_or_default(),
            asked_body: p.asked_body,
        }
    }

    fn short(&self) -> String {
        format!("PR #{} into {}", self.number, if self.base.is_empty() { "?" } else { &self.base })
    }

    /// Why it stays out of its card's merge ("it's a draft"), as the Mac's
    /// card leaves it out: blocked, or its checks fail. None: it goes in.
    pub fn held_back(&self) -> Option<String> {
        crate::prs::held_back(self.merge["blocked"].as_str(), self.merge["needs_override"].as_str().is_some())
    }
}

/// A decision the user put on hold ("wait for the ingest filters, then we
/// go for prod"): still theirs and still listed, in an "On hold" section
/// of its own with their words, but out of the count, the orb's badge and
/// the phone's, and never told or reminded. It comes back to the queue
/// (counted, and told once) when it is updated (`Book::wake`: new commits
/// or checks on its PRs, it came up again, what it asks changed), when
/// what it waits for looks done (a PR it names merged, or one merged in its
/// space that the tower reads as it), or when it is let go (`tower unhold`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hold {
    /// The user's words: why it waits, or what for. Empty: none given.
    pub reason: String,
    /// Who held it: `mac` (its card), `phone:<device>`, a brain (`tower`,
    /// `tower-<space>`: a tower, from what the user said in its chat), `cli`.
    pub by: String,
    pub since_ms: u64,
    /// The PRs it waits for, as its reason names them (`owner/name#n`, or
    /// `#n` in its space): one of them merged, it comes back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub until_prs: Vec<String>,
    /// PRs merged in its space since it was held that were weighed against
    /// its reason already (`daemon::decisions`): each is read once.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub weighed: Vec<String>,
    /// How it stood when it was held, to tell an update from what it was.
    pub seen: Seen,
}

/// What of a decision counts as an update to one on hold.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Seen {
    /// Its PRs: key, head commit, checks.
    pub prs: Vec<(String, String, String)>,
    pub count: u32,
    pub title: String,
    pub detail: String,
}

/// Why a decision that was on hold came back, and when: its card says so.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Back {
    pub why: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Decision {
    /// `d` and 12 hex digits: this decision, from when it opened. The same
    /// thing needing the user again later is another decision.
    pub id: String,
    /// What makes it one decision (see `ask_key`, `merge_key`, `raise_key`, `failed_key`).
    pub key: String,
    pub kind: Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    pub sessions: Vec<Who>,
    /// What is asked, in one line.
    pub title: String,
    /// What it is about, to read before deciding (cut to about `DETAIL`
    /// characters): a question's set's detail (the command a permission
    /// would run, the plan, the message that asks), `tower raise --detail`.
    /// None for a merge (its PRs say it).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Why it is the user's: the rule or reason that sent a question to
    /// them, `merge`, the category `tower raise` gave, how a task failed.
    pub why: String,
    pub options: Vec<Opt>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<PrRef>,
    /// The question set to answer (its first session's), for one that stands for question sets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
    /// How many questions that set asks.
    pub tabs: usize,
    /// `failed`: the agenda task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub first_ms: u64,
    pub last_ms: u64,
    /// How often it came up: once per session that asked, per raise, per PR.
    pub count: u32,
    pub state: State,
    /// The user was told (one alert, one sentence).
    pub told: bool,
    /// How the sessions that left it so far ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<State>,
    /// What it was answered with, once it was.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub answer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_ms: Option<u64>,
    /// On hold (see `Hold`); None: it waits for the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold: Option<Hold>,
    /// It was on hold and came back: why (its card says so).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back: Option<Back>,
}

impl Decision {
    /// On hold: out of the count and never told (see `Hold`).
    pub fn held(&self) -> bool {
        self.hold.is_some()
    }

    /// How it stands now, as far as a hold minds (`Hold::seen`).
    pub fn seen(&self) -> Seen {
        Seen { prs: self.prs.iter().map(|p| (p.key.clone(), p.sha.clone(), p.checks.clone())).collect(), count: self.count, title: self.title.clone(), detail: self.detail.clone() }
    }

    /// What changed since it stood as `was`, in words; None: nothing a hold
    /// minds (its PRs' reviews and scores may move meanwhile).
    pub fn update_since(&self, was: &Seen) -> Option<String> {
        let mut said: Vec<String> = vec![];
        for p in &self.prs {
            match was.prs.iter().find(|(k, _, _)| *k == p.key) {
                None => said.push(format!("{} joined it", p.short())),
                Some((_, sha, checks)) => {
                    if !sha.is_empty() && !p.sha.is_empty() && *sha != p.sha {
                        said.push(format!("PR #{} has new commits", p.number));
                    } else if *checks != p.checks {
                        said.push(match p.checks.as_str() {
                            "pass" => format!("PR #{}'s checks pass now", p.number),
                            "fail" => format!("PR #{}'s checks fail now", p.number),
                            "pending" => format!("PR #{}'s checks run again", p.number),
                            _ => format!("PR #{}'s checks changed", p.number),
                        });
                    }
                }
            }
        }
        // (A blocked session is seen again at every new status line: only what it shows counts.)
        if said.is_empty() && self.count > was.count && self.kind != Kind::Blocked {
            said.push("it came up again".into());
        }
        if said.is_empty() && self.title != was.title {
            said.push("what it asks changed".into());
        }
        if said.is_empty() && self.detail != was.detail && !self.detail.trim().is_empty() {
            said.push("it has more to read".into());
        }
        (!said.is_empty()).then(|| said.join("; "))
    }

    /// The option the tower would pick.
    pub fn pick(&self) -> Option<&Opt> {
        self.options.iter().find(|o| o.pick)
    }

    /// The sessions blocked on it now.
    pub fn waiting(&self) -> usize {
        self.sessions.iter().filter(|w| w.waiting).count()
    }

    /// Sessions waiting x how long they have (in ms, so a minute is not a step).
    pub fn weight(&self, now: u64) -> u64 {
        self.waiting() as u64 * now.saturating_sub(self.first_ms)
    }

    /// In `space` (any case); a decision in no space is in none.
    pub fn in_space(&self, space: &str) -> bool {
        self.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(space.trim()))
    }

    /// As the state, `tower decisions` and the phone carry it. `muted`: its
    /// space is muted (it is out of the main queue and the orb's count).
    pub fn view(&self, muted: bool) -> Value {
        let sessions: Vec<Value> = self
            .sessions
            .iter()
            .map(|w| {
                let mut v = json!({ "address": w.address, "name": w.name, "waiting": w.waiting });
                if let Some(q) = &w.question_id {
                    v["question_id"] = json!(q);
                }
                v
            })
            .collect();
        let options: Vec<Value> = self.options.iter().map(|o| json!({ "label": o.label, "why": o.why, "pick": o.pick })).collect();
        json!({
            "id": self.id,
            "key": self.key,
            "kind": self.kind.word(),
            "space": self.space,
            "sessions": sessions,
            "waiting": self.waiting(),
            "title": self.title,
            "detail": self.detail,
            "why": self.why,
            "options": options,
            "pick": self.pick().map(|o| o.label.clone()),
            "prs": self.prs,
            "question_id": self.question_id,
            "task": self.task,
            "first_ms": self.first_ms,
            "last_ms": self.last_ms,
            "count": self.count,
            "state": self.state.word(),
            "muted": muted,
            "hold": self.hold.as_ref().map(|h| json!({ "reason": h.reason, "by": h.by, "since_ms": h.since_ms, "until_prs": h.until_prs })),
            "back": self.back.as_ref().map(|b| json!({ "why": b.why, "at_ms": b.at_ms })),
        })
    }

    /// One line for `tower decisions` on a terminal.
    pub fn row(&self) -> String {
        let who: Vec<&str> = self.sessions.iter().map(|w| if w.name.is_empty() { w.address.as_str() } else { w.name.as_str() }).collect();
        let pick = self.pick().map(|o| format!("  (pick: {})", o.label)).unwrap_or_default();
        format!(
            "{}  {:<10} {:<16} {} waiting, {}  {}{}{}",
            self.id,
            self.kind.word(),
            clip(self.space.as_deref().unwrap_or("-"), 16),
            self.waiting(),
            crate::phone::ago(self.first_ms),
            clip(&self.title, 120),
            if who.is_empty() { String::new() } else { format!("  [{}]", who.join(", ")) },
            pick
        )
    }
}

/// What the user always decides, whatever rule there is: deploys and
/// releases, deletes, production data, secrets, money. True when `text`
/// (a decision's title, why it is the user's, the answer given) is about one.
pub fn always_yours(text: &str) -> bool {
    // Words as they are, and words by how they begin ("deploys", "deleting", "pricing").
    const WORDS: &[&str] = &[
        "prod", "production", "key", "keys", "pay", "pays", "paid", "paying", "payment", "payments", "buy", "buying", "drop", "drops", "dropped", "dropping", "wipe", "wipes", "wiped", "wiping",
        // The same things as they are said at a shell or in passing: `rm -rf`, a force push, `reset --hard`, "ship it", "go live", `.env`.
        "rm", "force", "forced", "reset", "resets", "ship", "ships", "shipped", "shipping", "live", "main", "master", "env", "apikey", "creds", "passwd",
    ];
    const STEMS: &[&str] = &[
        "deploy", "redeploy", "releas", "publish", "delet", "remov", "destroy", "truncat", "overwrit", "secret", "token", "password", "credential", "money", "billing", "invoic", "pric",
        "charg", "refund", "purchas", "eras", "purg", "migrat", "rollout", "rollback", "push",
    ];
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| WORDS.contains(&w) || STEMS.iter().any(|stem| w.strip_prefix(stem).is_some_and(|rest| rest.len() <= 5)))
}

/// What the user would say to make the answer they just gave a standing
/// rule: asked of the tower as any request, and the tower adds a behavior
/// (`tower behavior add`) in those words. `space`: where it was asked.
pub fn rule_request(space: Option<&str>, title: &str, answer: &str) -> String {
    let flat = |s: &str, n: usize| {
        let t = s.split_whitespace().collect::<Vec<_>>().join(" ").replace('"', "'");
        let t = t.trim().trim_end_matches('.').to_string();
        if t.chars().count() > n { format!("{}…", t.chars().take(n).collect::<String>().trim_end()) } else { t }
    };
    let whose = match space.map(str::trim).filter(|s| !s.is_empty()) {
        Some(space) => format!("a session in {space}"),
        None => "a session".to_string(),
    };
    format!("Make this a rule: when {whose} asks \"{}\", answer \"{}\" without asking me.", flat(title, 200), flat(answer, 200))
}

/// The rule an answered decision can be made into (`rule_request`), or None
/// when it is not offered: only for what a session asked (`question`) and a
/// choice the tower raised (`choice`), with an answer in words, and never
/// for what the user always decides (`always_yours`): no merge, no
/// permission, no failed task, nothing about a deploy, a delete, production
/// data, secrets or money.
pub fn rule_for(kind: Kind, why: &str, title: &str, space: Option<&str>, answer: &str) -> Option<String> {
    if !matches!(kind, Kind::Question | Kind::Choice) || answer.trim().is_empty() || title.trim().is_empty() {
        return None;
    }
    // What to put back after a restart, and waiting for an account's reset, are asked each time.
    if why == REOPEN_WHY || why == ACCOUNTS_WHY {
        return None;
    }
    if always_yours(title) || always_yours(why) || always_yours(answer) {
        return None;
    }
    Some(rule_request(space, title, answer))
}

/// A decision as the user hears it: who it is about (`name`, when known),
/// what is asked, and what the tower would pick.
pub fn said(name: &str, what: &str, pick: Option<&str>) -> String {
    let what = what.trim().trim_end_matches('.');
    let line = if name.trim().is_empty() { what.to_string() } else { format!("{} needs you: {what}", name.trim()) };
    match pick.map(|p| p.trim().trim_end_matches('.')).filter(|p| !p.is_empty()) {
        Some(pick) => format!("{line}{} I'd say: {pick}", if line.ends_with('?') { "" } else { "." }),
        None => line,
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>().trim_end())
    }
}

/// A decision's `detail`: `text` trimmed and cut to about `DETAIL`
/// characters with an ellipsis. `end`: keep its end (a message that asks
/// something asks it last).
pub fn detail_cut(text: &str, end: bool) -> String {
    let t = text.trim();
    if t.chars().count() <= DETAIL {
        return t.to_string();
    }
    if end {
        let n = t.chars().count();
        format!("…{}", t.chars().skip(n - (DETAIL - 1)).collect::<String>().trim_start())
    } else {
        clip(t, DETAIL)
    }
}

/// What a question set's decision shows as its detail: the command a
/// permission or an approval would run, the plan to approve, the message
/// that asks (its end).
pub fn detail_of(set: &QuestionSet) -> String {
    detail_cut(&set.detail, set.kind == "text")
}

/// A decision's id (`d` and 12 hex digits), not a question set's or a session's address.
pub fn is_id(s: &str) -> bool {
    s.len() == 13 && s.starts_with('d') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn id_of(key: &str, first_ms: u64) -> String {
    format!("d{}", &crate::daemon::lan::auth::sha256_hex(format!("{key}|{first_ms}").as_bytes())[..12])
}

/// A line as a key has it: lowercase, punctuation and whitespace collapsed,
/// so the same thing asked twice reads the same.
pub fn normalise(text: &str) -> String {
    let mut out = String::new();
    for w in text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w.to_lowercase());
    }
    out.chars().take(KEY_TEXT).collect()
}

fn space_part(space: Option<&str>) -> String {
    space.unwrap_or("").trim().to_lowercase()
}

/// What a session asks: its space and the question's words.
pub fn ask_key(space: Option<&str>, text: &str) -> String {
    format!("ask:{}:{}", space_part(space), normalise(text))
}

/// The merges of one repo in one space.
pub fn merge_key(space: Option<&str>, repo: &str) -> String {
    format!("merge:{}:{}", space_part(space), repo.trim().to_lowercase())
}

/// `tower raise`: its space and the line.
pub fn raise_key(space: Option<&str>, line: &str) -> String {
    format!("raise:{}:{}", space_part(space), normalise(line))
}

/// A session blocked on what the tower can't read: one decision per session.
pub fn blocked_key(space: Option<&str>, address: &str) -> String {
    format!("blocked:{}:{}", space_part(space), address.trim())
}

/// A task that failed twice: its space and the task.
pub fn failed_key(space: Option<&str>, task: &str) -> String {
    format!("failed:{}:{}", space_part(space), task.trim())
}

// ---- from a question set ----

/// What an escalated question set asks, as its decision has it.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked {
    pub key: String,
    pub kind: Kind,
    pub space: Option<String>,
    /// Its session, with the set's id.
    pub who: Who,
    pub title: String,
    /// The set's detail, as the decision shows it (`detail_of`).
    pub detail: String,
    pub why: String,
    pub options: Vec<Opt>,
    pub tabs: usize,
    /// Its answer is going in.
    pub answering: bool,
    pub at_ms: u64,
    /// The set this one replaces (the same dialog, read again).
    pub replaces: Option<String>,
}

/// The words that make two question sets the same question: what a message
/// or dialog asks; for a menu its title and what it is about (another
/// command or plan is another decision).
fn question_text(set: &QuestionSet) -> String {
    match set.kind.as_str() {
        "text" | "claude-ask" => {
            let asked: Vec<&str> = set.tabs.iter().map(|t| t.question.trim()).filter(|q| !q.is_empty()).collect();
            if asked.is_empty() { set.title.clone() } else { asked.join(" | ") }
        }
        _ if set.detail.trim().is_empty() => set.title.clone(),
        _ => format!("{} {}", set.title, &crate::daemon::lan::auth::sha256_hex(set.detail.trim().as_bytes())[..8]),
    }
}

/// The options of a question set's decision: the tower's recommendations
/// (the first is its pick; for several questions, one answer to each); else
/// a menu's own options, with no pick.
pub fn options_of(set: &QuestionSet) -> Vec<Opt> {
    let tabs = set.tabs.len();
    if let Some(recs) = &set.recommendations {
        let of = |t: usize| recs.iter().find(|r| r.tab == t).map(|r| r.options.as_slice()).unwrap_or(&[]);
        let n = (0..tabs).map(|t| of(t).len()).min().unwrap_or(0);
        if n > 0 {
            return (0..n)
                .map(|i| Opt {
                    label: (0..tabs).map(|t| of(t)[i].label.trim()).collect::<Vec<_>>().join(" · "),
                    why: of(0)[i].why.clone(),
                    pick: i == 0,
                    answers: Value::Array((0..tabs).map(|t| json!({ "tab": t, "choice": of(t)[i].choice })).collect()),
                    text: String::new(),
                })
                .collect();
        }
    }
    match set.tabs.as_slice() {
        [tab] if set.kind != "text" && !tab.multi => tab
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| Opt {
                label: o.label.clone(),
                why: o.description.clone(),
                answers: json!([{ "tab": 0, "choice": i }]),
                ..Default::default()
            })
            .collect(),
        _ => vec![],
    }
}

/// A question decision's title: what its dialog asks now. With several
/// questions, the one it is on (else the first still open), and how many
/// more are open; those the user answered in the terminal already say so;
/// all of them answered there: only Submit is left.
fn question_title(set: &QuestionSet) -> String {
    let asked = |t: &crate::questions::Tab| t.question.trim().to_string();
    let first = set.tabs.first().map(asked).filter(|q| !q.is_empty()).unwrap_or_else(|| set.title.trim().to_string());
    let n = set.tabs.len();
    if n < 2 {
        return first;
    }
    let there = crate::questions::answered_there(set).len();
    let open: Vec<usize> = (0..n).filter(|i| !set.tabs[*i].answered).collect();
    let cur = open.iter().copied().find(|i| *i == set.current_tab).or_else(|| open.iter().copied().find(|i| *i > set.current_tab)).or(open.first().copied());
    let Some(cur) = cur else {
        return format!("All {n} answered in the terminal, not submitted yet: {first}");
    };
    let q = Some(asked(&set.tabs[cur])).filter(|q| !q.is_empty()).unwrap_or(first);
    match (open.len() - 1, there) {
        (0, 0) => q,
        (more, 0) => format!("{q} (and {more} more)"),
        (0, there) => format!("{q} ({there} answered in the terminal)"),
        (more, there) => format!("{q} (and {more} more; {there} answered in the terminal)"),
    }
}

/// A question set that is the user's to answer, as a decision sees it. None:
/// the tower answers it, or nobody has decided yet.
pub fn asked(set: &QuestionSet) -> Option<Asked> {
    use crate::daemon::card::{Decision as Decided, Phase};
    if set.state.decision != Decided::Escalated || matches!(set.state.phase, Phase::Closed { .. } | Phase::Dismissed) {
        return None;
    }
    let kind = match set.kind.as_str() {
        "text" | "claude-ask" => Kind::Question,
        _ => Kind::Permission,
    };
    let title = match kind {
        Kind::Question => question_title(set),
        _ => set.title.clone(),
    };
    Some(Asked {
        key: ask_key(set.space.as_deref(), &question_text(set)),
        kind,
        space: set.space.clone(),
        who: Who { address: set.session.clone(), name: set.name.clone(), waiting: true, question_id: Some(set.id.clone()) },
        title,
        detail: detail_of(set),
        why: set.reason.clone(),
        options: options_of(set),
        tabs: set.tabs.len(),
        answering: matches!(set.state.phase, Phase::Answering { .. } | Phase::Answered | Phase::Queued),
        at_ms: set.at_ms,
        replaces: set.replaces.clone(),
    })
}

// ---- from `tower raise`, and a task that failed twice ----

/// `tower raise`: a decision the brain puts to the user.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Raise {
    pub space: Option<String>,
    /// The session it is about (None: one that has ended).
    pub who: Option<Who>,
    /// Its category (merge, deploy, delete, data, secrets, money, design, failed, waiting).
    pub why: String,
    /// The decision, in one line.
    pub text: String,
    /// What it is about, more than the line says (`--detail`): the command,
    /// the plan, the change. Cut to about `DETAIL` characters.
    pub detail: String,
    pub options: Vec<Opt>,
}

/// An agenda task that failed again after its retry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Failure {
    pub task: String,
    pub title: String,
    pub space: Option<String>,
    pub who: Option<Who>,
    /// How it failed.
    pub why: String,
}

/// What a sighting did: the decision it belongs to, and whether it opened it.
#[derive(Debug, Clone, PartialEq)]
pub struct Raised {
    pub id: String,
    /// It opened the decision (false: one more sighting of an open one).
    pub fresh: bool,
    /// It was folded into a decision about the same thing under another key
    /// (a merge the tower already shows, a question its session already asks).
    pub folded: bool,
    pub count: u32,
    pub waiting: usize,
}

/// The options `tower raise --options` gives: a list of `{"label", "why",
/// "text"}` (or plain words), the recommended one first.
pub fn options_from(v: &Value) -> Result<Vec<Opt>> {
    let Some(list) = v.as_array() else { bail!("--options is a list: '[{{\"label\":\"…\",\"why\":\"…\"}}]'") };
    let mut out = vec![];
    for (i, o) in list.iter().enumerate() {
        let (label, why, text) = match o {
            Value::String(s) => (s.trim().to_string(), String::new(), String::new()),
            _ => (
                o["label"].as_str().unwrap_or("").trim().to_string(),
                o["why"].as_str().unwrap_or("").trim().to_string(),
                o["text"].as_str().unwrap_or("").trim().to_string(),
            ),
        };
        if label.is_empty() {
            bail!("--options: choice {} has no label", i + 1);
        }
        out.push(Opt { label, why, pick: i == 0, answers: Value::Null, text });
    }
    Ok(out)
}

/// The PR numbers a line names (`#42`, `PR 42`).
pub(crate) fn pr_numbers(text: &str) -> Vec<u64> {
    let low = text.to_lowercase();
    let mut out = vec![];
    let mut take = |rest: &str| {
        let digits: String = rest.trim_start().chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = digits.parse::<u64>()
            && !out.contains(&n)
        {
            out.push(n);
        }
    };
    for (i, _) in low.match_indices('#') {
        take(&low[i + 1..]);
    }
    for (i, _) in low.match_indices("pr ") {
        if i == 0 || !low[..i].ends_with(|c: char| c.is_alphanumeric()) {
            take(low[i + 3..].trim_start_matches('#'));
        }
    }
    out
}

/// What changed in a round: decisions that opened, and those that closed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    pub fresh: Vec<String>,
    pub closed: Vec<Decision>,
}

// ---- the book ----

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub open: Vec<Decision>,
    /// The last ones that closed, newest last.
    pub closed: Vec<Decision>,
    /// Waiting PRs that make no merge decision again, by their key: the user
    /// dismissed theirs, or their space was closed or archived. Forgotten
    /// once the PR no longer waits.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub settled_prs: BTreeMap<String, State>,
    /// Decisions on hold that came back in this change (id, why), for the
    /// daemon to tell once (`Decisions::change` takes them).
    #[serde(skip)]
    pub came_back: Vec<(String, String)>,
}

impl Book {
    /// The book in `path`. A file that doesn't parse is kept aside and the book starts empty.
    pub fn load(path: &Path) -> Book {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                let aside = path.with_extension(format!("bad-{}.json", crate::history::now_ms()));
                let _ = std::fs::copy(path, &aside);
                eprintln!("[decisions] {} unreadable ({e}); kept as {}", path.display(), aside.display());
                Book::default()
            }),
            Err(_) => Book::default(),
        }
    }

    /// Written whole: a temporary file, then a rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// A decision by its id: an open one, else one that closed lately.
    pub fn find(&self, id: &str) -> Option<&Decision> {
        self.open.iter().find(|d| d.id == id).or_else(|| self.closed.iter().rev().find(|d| d.id == id))
    }

    /// The open decisions in queue order: heaviest first (sessions waiting x
    /// how long they have), then oldest first.
    pub fn ordered(&self, now: u64) -> Vec<&Decision> {
        let mut v: Vec<&Decision> = self.open.iter().collect();
        v.sort_by(|a, b| b.weight(now).cmp(&a.weight(now)).then(a.first_ms.cmp(&b.first_ms)).then(a.id.cmp(&b.id)));
        v
    }

    fn add(&mut self, mut d: Decision, now: u64) -> String {
        if d.first_ms == 0 || d.first_ms > now {
            d.first_ms = now;
        }
        d.last_ms = now;
        d.count = d.count.max(1);
        d.id = id_of(&d.key, d.first_ms);
        // The same key opened in the same millisecond as one that closed: still its own id.
        while self.find(&d.id).is_some() {
            d.first_ms += 1;
            d.id = id_of(&d.key, d.first_ms);
        }
        let id = d.id.clone();
        self.open.push(d);
        id
    }

    /// Closes an open decision as `state`. None: it isn't open.
    pub fn close(&mut self, id: &str, state: State, now: u64) -> Option<Decision> {
        let i = self.open.iter().position(|d| d.id == id)?;
        let mut d = self.open.remove(i);
        d.state = state;
        d.closed_ms = Some(now);
        for w in d.sessions.iter_mut() {
            w.waiting = false;
        }
        self.closed.push(d.clone());
        if self.closed.len() > KEEP_CLOSED {
            let n = self.closed.len() - KEEP_CLOSED;
            self.closed.drain(..n);
        }
        Some(d)
    }

    /// The user answered it (`with`: the option's label, or their words).
    pub fn answered(&mut self, id: &str, with: &str, now: u64) -> Option<Decision> {
        self.close(id, State::Answered, now)?;
        let d = self.closed.iter_mut().rev().find(|c| c.id == id)?;
        d.answer = with.trim().to_string();
        Some(d.clone())
    }

    /// What an open decision is being answered with (a question's answer
    /// goes in through its dialog, and the decision closes when that does):
    /// kept, so the answer can be made a rule afterwards.
    pub fn note_answer(&mut self, id: &str, with: &str) {
        if let Some(d) = self.open.iter_mut().chain(self.closed.iter_mut()).find(|d| d.id == id) {
            d.answer = with.trim().to_string();
        }
    }

    /// The user was told of `id` now, unless they were before: true the first time.
    pub fn claim_told(&mut self, id: &str) -> Option<bool> {
        let d = self.open.iter_mut().find(|d| d.id == id)?;
        let first = !d.told;
        d.told = true;
        Some(first)
    }

    /// The question sets that are the user's now, as decisions: each joins
    /// the decision of its key (a new session there counts once), a session
    /// that no longer asks leaves, and a decision nobody asks any more
    /// closes. `ended`: how a question set that is over ended, by its id.
    pub fn sync_questions(&mut self, asked: &[Asked], ended: &dyn Fn(&str) -> State, now: u64) -> Changes {
        let mut ch = Changes::default();
        // The same dialog read again under other words: its decision follows it.
        for a in asked {
            let Some(old) = a.replaces.as_deref() else { continue };
            if self.open.iter().any(|d| d.key == a.key) {
                continue;
            }
            if let Some(d) =
                self.open.iter_mut().find(|d| d.kind.asks() && d.sessions.len() == 1 && d.sessions[0].question_id.as_deref() == Some(old))
            {
                d.key = a.key.clone();
            }
        }
        for a in asked {
            match self.open.iter_mut().find(|d| d.key == a.key) {
                Some(d) => match d.sessions.iter_mut().find(|w| w.address == a.who.address) {
                    Some(w) => *w = a.who.clone(),
                    None => {
                        d.sessions.push(a.who.clone());
                        d.count += 1;
                        d.last_ms = now;
                    }
                },
                None => {
                    let d = Decision {
                        key: a.key.clone(),
                        kind: a.kind,
                        space: a.space.clone(),
                        sessions: vec![a.who.clone()],
                        first_ms: a.at_ms,
                        // The same dialog under another key, beside others who asked its old words: told with them.
                        told: a.replaces.is_some(),
                        ..Default::default()
                    };
                    ch.fresh.push(self.add(d, now));
                }
            }
        }
        let mut over = vec![];
        for d in self.open.iter_mut().filter(|d| d.kind.asks()) {
            let mine: Vec<&Asked> = asked.iter().filter(|a| a.key == d.key).collect();
            let mut left = d.left;
            d.sessions.retain(|w| {
                let stays = mine.iter().any(|a| a.who.address == w.address);
                if !stays {
                    let how = w.question_id.as_deref().map(ended).unwrap_or(State::Stopped);
                    left = Some(left.map_or(how, |l| l.over(how)));
                }
                stays
            });
            d.left = left;
            // Its card is its first session's set.
            if let Some(lead) = d.sessions.first().and_then(|w| mine.iter().find(|a| a.who.address == w.address)) {
                d.kind = lead.kind;
                d.title = lead.title.clone();
                d.detail = lead.detail.clone();
                d.why = lead.why.clone();
                d.options = lead.options.clone();
                d.tabs = lead.tabs;
                d.question_id = lead.who.question_id.clone();
            }
            d.state = if mine.iter().any(|a| a.answering) { State::Answering } else { State::Open };
            if d.sessions.is_empty() {
                over.push((d.id.clone(), d.left.unwrap_or(State::Stopped)));
            }
        }
        for (id, state) in over {
            ch.closed.extend(self.close(&id, state, now));
        }
        ch
    }

    /// The PRs that wait for a merge, as decisions: one per repo and space,
    /// its PRs in order (into `dev` before into `main`), changed in place. A
    /// decision whose PRs all merged or closed is over.
    pub fn sync_merges(&mut self, prs: &[Pr], now: u64) -> Changes {
        let mut ch = Changes::default();
        // A draft, or one still under review, waits for nobody yet: in when it is ready.
        let waiting: Vec<&Pr> = prs.iter().filter(|p| p.open() && p.waiting && p.ready_for_user()).collect();
        self.settled_prs.retain(|k, _| waiting.iter().any(|p| p.key() == *k));
        let mut groups: BTreeMap<String, Vec<&Pr>> = BTreeMap::new();
        for p in waiting.iter().filter(|p| !self.settled_prs.contains_key(&p.key())) {
            groups.entry(merge_key(p.space.as_deref(), &p.repo)).or_default().push(p);
        }
        for (key, list) in groups.iter_mut() {
            list.sort_by_key(|p| (crate::prs::is_prod(&p.base), p.number));
            let refs: Vec<PrRef> = list.iter().map(|p| PrRef::of(p)).collect();
            let was = self.open.iter().find(|d| d.key == *key);
            let mut sessions: Vec<Who> = vec![];
            for p in list.iter().filter(|p| !p.session.is_empty()) {
                if !sessions.iter().any(|w| w.address == p.session) {
                    let waiting = was.and_then(|d| d.sessions.iter().find(|w| w.address == p.session)).is_none_or(|w| w.waiting);
                    sessions.push(Who { address: p.session.clone(), name: p.name.clone(), waiting, question_id: None });
                }
            }
            let (title, options) = (merge_title(&refs), merge_options(&refs));
            match self.open.iter_mut().find(|d| d.key == *key) {
                Some(d) => {
                    if refs.iter().any(|r| !d.prs.iter().any(|o| o.key == r.key)) {
                        d.count += 1;
                        d.last_ms = now;
                    }
                    (d.sessions, d.prs, d.title, d.options) = (sessions, refs, title, options);
                }
                None => {
                    let d = Decision {
                        key: key.clone(),
                        kind: Kind::Merge,
                        space: list.first().and_then(|p| p.space.clone()),
                        sessions,
                        title,
                        why: "merge".into(),
                        options,
                        prs: refs,
                        first_ms: list.iter().map(|p| p.found_ms).filter(|m| *m > 0).min().unwrap_or(now),
                        ..Default::default()
                    };
                    ch.fresh.push(self.add(d, now));
                }
            }
        }
        let over: Vec<(String, State)> = self
            .open
            .iter()
            .filter(|d| d.kind == Kind::Merge && !groups.contains_key(&d.key))
            .map(|d| {
                let merged = d.prs.iter().any(|r| prs.iter().any(|p| p.key() == r.key && p.state == "merged"));
                (d.id.clone(), if merged { State::Answered } else { State::Stopped })
            })
            .collect();
        for (id, state) in over {
            ch.closed.extend(if state == State::Answered { self.answered(&id, "merged", now) } else { self.close(&id, state, now) });
        }
        // What a decision on hold waits for merged: it comes back.
        self.wake_on_merged(prs, now);
        ch
    }

    /// An open decision about the same thing as a raise, under another key:
    /// the merge it names (or its session's), the question its session
    /// already asks, the failure the tower already put to the user.
    fn same_thing(&self, r: &Raise) -> Option<usize> {
        let why = r.why.trim().to_lowercase();
        let about = |d: &Decision| r.who.as_ref().is_some_and(|w| d.sessions.iter().any(|s| s.address == w.address));
        let same_space = |d: &Decision| space_part(d.space.as_deref()) == space_part(r.space.as_deref());
        match why.as_str() {
            "merge" => {
                let named = pr_numbers(&r.text);
                self.open.iter().position(|d| {
                    d.kind == Kind::Merge
                        && same_space(d)
                        && if named.is_empty() { about(d) } else { d.prs.iter().any(|p| named.contains(&p.number)) }
                })
            }
            "waiting" | "blocked" | "question" => self.open.iter().position(|d| (d.kind.asks() || d.kind == Kind::Blocked) && about(d)),
            "failed" => self.open.iter().position(|d| d.kind == Kind::Failed && about(d)),
            _ => None,
        }
    }

    /// The card offering back what a restart closed (`crate::reopen`): one
    /// at a time, in the main queue, changed in place as what it lists
    /// changes (sessions brought back by hand leave it). None: nothing is
    /// left to offer, and an open one stops. What it is now (None: none).
    pub fn reopen(&mut self, offer: Option<crate::reopen::Offer>, now: u64) -> Option<Raised> {
        let at = self.open.iter().position(|d| d.key == REOPEN_KEY);
        let Some(o) = offer else {
            if let Some(i) = at {
                let id = self.open[i].id.clone();
                self.close(&id, State::Stopped, now);
            }
            return None;
        };
        let options: Vec<Opt> =
            o.options.into_iter().map(|(label, why, pick, tower)| Opt { label, why, pick: tower, text: pick.code(), ..Default::default() }).collect();
        if let Some(i) = at {
            let d = &mut self.open[i];
            let detail = detail_cut(&o.detail, false);
            if d.title != o.title || d.detail != detail || d.options != options {
                (d.title, d.detail, d.options, d.last_ms) = (o.title, detail, options, now);
            }
            return Some(Raised { id: d.id.clone(), fresh: false, folded: false, count: d.count, waiting: 0 });
        }
        let d = Decision {
            key: REOPEN_KEY.into(),
            kind: Kind::Choice,
            title: o.title,
            detail: detail_cut(&o.detail, false),
            why: REOPEN_WHY.into(),
            options,
            ..Default::default()
        };
        Some(Raised { id: self.add(d, now), fresh: true, folded: false, count: 1, waiting: 0 })
    }

    /// The card saying every Claude account is out of usage
    /// (`crate::failover::card`): one at a time, in the main queue, changed
    /// in place as what waits changes. None: nothing waits, and an open one
    /// stops. What it is now (None: none).
    pub fn accounts_out(&mut self, card: Option<crate::failover::Card>, now: u64) -> Option<Raised> {
        let at = self.open.iter().position(|d| d.key == ACCOUNTS_KEY);
        let Some(c) = card else {
            if let Some(i) = at {
                let id = self.open[i].id.clone();
                self.close(&id, State::Stopped, now);
            }
            return None;
        };
        let options: Vec<Opt> = c.options.into_iter().map(|(label, why, code, tower)| Opt { label, why, pick: tower, text: code, ..Default::default() }).collect();
        if let Some(i) = at {
            let d = &mut self.open[i];
            let detail = detail_cut(&c.detail, false);
            if d.title != c.title || d.detail != detail || d.options != options {
                (d.title, d.detail, d.options, d.last_ms) = (c.title, detail, options, now);
            }
            return Some(Raised { id: d.id.clone(), fresh: false, folded: false, count: d.count, waiting: 0 });
        }
        let d = Decision {
            key: ACCOUNTS_KEY.into(),
            kind: Kind::Choice,
            title: c.title,
            detail: detail_cut(&c.detail, false),
            why: ACCOUNTS_WHY.into(),
            options,
            ..Default::default()
        };
        Some(Raised { id: self.add(d, now), fresh: true, folded: false, count: 1, waiting: 0 })
    }

    /// `tower raise`: one decision per space and line. The same line again,
    /// or a raise about something already in front of the user, is that
    /// decision once more (`count`, `last_ms`, its session), never a second one.
    pub fn raise(&mut self, r: Raise, now: u64) -> Raised {
        let key = raise_key(r.space.as_deref(), &r.text);
        let by_key = self.open.iter().position(|d| d.key == key);
        if let Some(i) = by_key.or_else(|| self.same_thing(&r)) {
            let d = &mut self.open[i];
            d.count += 1;
            d.last_ms = now;
            // A merge's or a question's sessions are its PRs' and its sets'.
            if !d.kind.asks()
                && d.kind != Kind::Merge
                && let Some(w) = r.who
                && !d.sessions.iter().any(|s| s.address == w.address)
            {
                d.sessions.push(w);
            }
            if d.options.is_empty() && d.kind == Kind::Raised && !r.options.is_empty() {
                d.options = r.options;
                d.kind = Kind::Choice;
            }
            // Raised again with more to read: the card shows it (a question's or a merge's own stays).
            if matches!(d.kind, Kind::Raised | Kind::Choice) && !r.detail.trim().is_empty() {
                d.detail = detail_cut(&r.detail, false);
            }
            return Raised { id: d.id.clone(), fresh: false, folded: by_key.is_none(), count: d.count, waiting: d.waiting() };
        }
        let d = Decision {
            key,
            kind: if r.options.is_empty() { Kind::Raised } else { Kind::Choice },
            space: r.space,
            sessions: r.who.into_iter().collect(),
            title: r.text.trim().to_string(),
            detail: detail_cut(&r.detail, false),
            why: r.why.trim().to_string(),
            options: r.options,
            ..Default::default()
        };
        let waiting = d.waiting();
        Raised { id: self.add(d, now), fresh: true, folded: false, count: 1, waiting }
    }

    /// A task failed again after its retry: the user's to say whether it is
    /// tried once more or dropped.
    pub fn failed(&mut self, f: Failure, now: u64) -> Raised {
        let key = failed_key(f.space.as_deref(), &f.task);
        if let Some(d) = self.open.iter_mut().find(|d| d.key == key) {
            d.count += 1;
            d.last_ms = now;
            return Raised { id: d.id.clone(), fresh: false, folded: false, count: d.count, waiting: d.waiting() };
        }
        let why = f.why.trim().trim_end_matches('.');
        let d = Decision {
            key,
            kind: Kind::Failed,
            space: f.space,
            sessions: f.who.into_iter().collect(),
            title: format!(
                "\"{}\" failed after a retry{}",
                clip(f.title.trim(), 120),
                if why.is_empty() { String::new() } else { format!(": {}", clip(why, 200)) }
            ),
            why: "failed".into(),
            options: vec![
                Opt { label: "Try again".into(), why: "its instruction goes to its session once more".into(), ..Default::default() },
                Opt { label: "Drop it".into(), why: "the task is abandoned".into(), ..Default::default() },
            ],
            task: Some(f.task),
            ..Default::default()
        };
        let waiting = d.waiting();
        Raised { id: self.add(d, now), fresh: true, folded: false, count: 1, waiting }
    }

    /// A session is blocked on something the tower can't read: the user's
    /// to look at in Herdr. One decision per session; seen again, it is that
    /// decision once more, its line brought up to date.
    pub fn blocked(&mut self, space: Option<String>, who: Who, title: &str, now: u64) -> Raised {
        let key = blocked_key(space.as_deref(), &who.address);
        if let Some(d) = self.open.iter_mut().find(|d| d.key == key) {
            d.count += 1;
            d.last_ms = now;
            if !title.trim().is_empty() {
                d.title = title.trim().to_string();
            }
            for w in d.sessions.iter_mut() {
                w.waiting = true;
            }
            return Raised { id: d.id.clone(), fresh: false, folded: false, count: d.count, waiting: d.waiting() };
        }
        let d = Decision {
            key,
            kind: Kind::Blocked,
            space,
            sessions: vec![Who { waiting: true, ..who }],
            title: title.trim().to_string(),
            why: "blocked".into(),
            ..Default::default()
        };
        let waiting = d.waiting();
        Raised { id: self.add(d, now), fresh: true, folded: false, count: 1, waiting }
    }

    /// A session as the radar sees it, blocked or not: once it is not, what
    /// it was blocked on is over (it was answered in its own terminal, or
    /// went away by itself).
    pub fn sync_blocked(&mut self, address: &str, blocked: bool, now: u64) -> Changes {
        if blocked {
            return Changes::default();
        }
        let over: Vec<String> =
            self.open.iter().filter(|d| d.kind == Kind::Blocked && d.sessions.iter().any(|w| w.address == address)).map(|d| d.id.clone()).collect();
        Changes { fresh: vec![], closed: over.iter().filter_map(|id| self.answered(id, "in its own terminal", now)).collect() }
    }

    /// The question set `set` was answered in its session's own terminal: its
    /// session leaves the decision it stood in, as answered, and a decision
    /// nobody else asks any more is over, answered (other sessions that ask
    /// the same still wait on their own dialogs). What it was answered with
    /// is the caller's to say (`note_answer`), once it knows. The decisions
    /// it was in: (id, over now).
    pub fn answered_in_terminal(&mut self, set: &str, now: u64) -> Vec<(String, bool)> {
        self.answered_with(set, "", now)
    }

    /// `answered_in_terminal`, the answer known (`with`): one that is over
    /// closes with it.
    pub fn answered_with(&mut self, set: &str, with: &str, now: u64) -> Vec<(String, bool)> {
        let mut out = vec![];
        for d in self.open.iter_mut().filter(|d| d.kind.asks()) {
            if !d.sessions.iter().any(|w| w.question_id.as_deref() == Some(set)) {
                continue;
            }
            d.sessions.retain(|w| w.question_id.as_deref() != Some(set));
            d.left = Some(d.left.map_or(State::Answered, |l| l.over(State::Answered)));
            out.push((d.id.clone(), d.sessions.is_empty()));
        }
        for (id, over) in &out {
            if *over {
                self.answered(id, with, now);
            }
        }
        out
    }

    /// The tasks that still stand failed after a retry: a `failed` decision
    /// about any other task is over (it was sent again, verified or dropped).
    pub fn sync_failed(&mut self, still: &[String], now: u64) -> Changes {
        let over: Vec<String> = self
            .open
            .iter()
            .filter(|d| d.kind == Kind::Failed && d.task.as_ref().is_none_or(|t| !still.contains(t)))
            .map(|d| d.id.clone())
            .collect();
        Changes { fresh: vec![], closed: over.iter().filter_map(|id| self.close(id, State::Answered, now)).collect() }
    }

    /// A session is gone: it leaves its decisions, and a decision left with
    /// no session is over. (One that stands for question sets follows its
    /// sets: `sync_questions` takes the session out.) Not a merge: a PR that
    /// is still open and waiting is still the user's, whoever opened it. Its
    /// session only stops waiting on it; the decision goes when its PRs no
    /// longer wait, or with its space (`space_over`).
    pub fn session_gone(&mut self, address: &str, now: u64) -> Changes {
        let mut over = vec![];
        for d in self.open.iter_mut().filter(|d| !d.kind.asks() && d.sessions.iter().any(|w| w.address == address)) {
            if d.kind == Kind::Merge {
                for w in d.sessions.iter_mut().filter(|w| w.address == address) {
                    w.waiting = false;
                }
                continue;
            }
            d.sessions.retain(|w| w.address != address);
            d.left = Some(d.left.map_or(State::Stopped, |l| l.over(State::Stopped)));
            if d.sessions.is_empty() {
                over.push((d.id.clone(), d.left.unwrap_or(State::Stopped)));
            }
        }
        Changes { fresh: vec![], closed: over.iter().filter_map(|(id, s)| self.close(id, *s, now)).collect() }
    }

    /// A space has no session left (`merges` false), or was closed or
    /// archived (`merges` true): what was raised there, and its failed
    /// tasks, are over (`stopped`). Its merges only with `merges`: until
    /// then a waiting PR stays the user's. Those PRs make no decision again
    /// while they wait. (What a session asks follows its question sets.)
    pub fn space_over(&mut self, space: &str, merges: bool, now: u64) -> Changes {
        let over: Vec<String> =
            self.open.iter().filter(|d| d.in_space(space) && !d.kind.asks() && (merges || d.kind != Kind::Merge)).map(|d| d.id.clone()).collect();
        for d in self.open.iter().filter(|d| over.contains(&d.id)) {
            for p in &d.prs {
                self.settled_prs.insert(p.key.clone(), State::Stopped);
            }
        }
        Changes { fresh: vec![], closed: over.iter().filter_map(|id| self.close(id, State::Stopped, now)).collect() }
    }

    /// A session's turn ended waiting on the user while a decision the brain
    /// raised about that session is open: that decision is what it waits for
    /// (the brain read its screen and put it to the user), so the wait is
    /// that decision seen once more, not a second card beside it. Two cards
    /// for one approval got opposite answers within a minute (13 Nov). Its
    /// id, or None: nothing raised about it is open.
    pub fn waits_on_raised(&mut self, address: &str, now: u64) -> Option<String> {
        let d = self
            .open
            .iter_mut()
            .filter(|d| matches!(d.kind, Kind::Raised | Kind::Choice) && d.key.starts_with("raise:"))
            .find(|d| d.sessions.iter().any(|w| w.address == address))?;
        d.count += 1;
        d.last_ms = now;
        for w in d.sessions.iter_mut().filter(|w| w.address == address) {
            w.waiting = true;
        }
        Some(d.id.clone())
    }

    /// A session works (`waiting` false) or waits again: true when that changed a decision.
    pub fn session_waits(&mut self, address: &str, waiting: bool) -> bool {
        let mut changed = false;
        // One that asks is blocked on its question for as long as it shows it.
        for w in self.open.iter_mut().filter(|d| !d.kind.asks()).flat_map(|d| d.sessions.iter_mut()).filter(|w| w.address == address) {
            changed |= w.waiting != waiting;
            w.waiting = waiting;
        }
        changed
    }

    /// Puts the open decision `id` on hold (see `Hold`): `reason`, the
    /// user's words (may be empty); `by`, who did. Held again, its reason is
    /// the new one and what counts as an update starts from now.
    pub fn hold(&mut self, id: &str, reason: &str, by: &str, now: u64) -> Result<Decision> {
        let Some(d) = self.open.iter_mut().find(|d| d.id == id) else {
            match self.find(id) {
                Some(d) => bail!("{id} is {} already: only an open decision is put on hold", d.state.word()),
                None => bail!("no decision {id} (`tower decisions` lists them)"),
            }
        };
        let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
        // The PRs its reason names (its own PRs aside: their updates bring it back anyway).
        let repo = d.prs.first().map(|p| p.repo.clone());
        let own: Vec<String> = d.prs.iter().map(|p| p.key.clone()).collect();
        let mut until: Vec<String> = crate::prs::links(&reason).into_iter().map(|(r, n)| format!("{r}#{n}")).collect();
        for n in pr_numbers(&reason) {
            let key = match &repo {
                Some(r) => format!("{r}#{n}"),
                None => format!("#{n}"),
            };
            if !until.iter().any(|u| u.ends_with(&format!("#{n}"))) {
                until.push(key);
            }
        }
        until.retain(|u| !own.contains(u));
        d.hold = Some(Hold { reason, by: by.trim().to_string(), since_ms: now, until_prs: until, weighed: vec![], seen: d.seen() });
        d.back = None;
        Ok(d.clone())
    }

    /// Lets the decision `id` go from hold: it waits for the user again,
    /// counted, and is told once more (`told` is reset). `why`: what brought it back.
    pub fn unhold(&mut self, id: &str, why: &str, now: u64) -> Result<Decision> {
        let Some(d) = self.open.iter_mut().find(|d| d.id == id) else {
            match self.find(id) {
                Some(d) => bail!("{id} is {} already", d.state.word()),
                None => bail!("no decision {id} (`tower decisions` lists them)"),
            }
        };
        if d.hold.is_none() {
            bail!("{id} isn't on hold");
        }
        d.hold = None;
        d.back = Some(Back { why: why.trim().to_string(), at_ms: now });
        d.told = false;
        let d = d.clone();
        self.came_back.push((d.id.clone(), d.back.as_ref().map(|b| b.why.clone()).unwrap_or_default()));
        Ok(d)
    }

    /// The decisions on hold that were updated since they were held (see
    /// `Decision::update_since`) come back: (id, why) each.
    pub fn wake(&mut self, now: u64) -> Vec<(String, String)> {
        let woke: Vec<(String, String)> =
            self.open.iter().filter_map(|d| d.hold.as_ref().and_then(|h| d.update_since(&h.seen)).map(|why| (d.id.clone(), why))).collect();
        for (id, why) in &woke {
            let _ = self.unhold(id, why, now);
        }
        woke
    }

    /// The decisions on hold whose reason names a PR that merged now (in
    /// `prs`, the store's): they come back, what they waited for done.
    fn wake_on_merged(&mut self, prs: &[Pr], now: u64) {
        let merged = |u: &str, d: &Decision| {
            prs.iter().find(|p| {
                p.state == "merged"
                    && (p.key() == u || (u.starts_with('#') && u == format!("#{}", p.number) && space_part(p.space.as_deref()) == space_part(d.space.as_deref())))
            })
        };
        let done: Vec<(String, String)> = self
            .open
            .iter()
            .filter_map(|d| {
                let h = d.hold.as_ref()?;
                let p = h.until_prs.iter().find_map(|u| merged(u, d))?;
                Some((d.id.clone(), format!("PR #{} merged, which it waited for", p.number)))
            })
            .collect();
        for (id, why) in done {
            let _ = self.unhold(&id, &why, now);
        }
    }

    /// The user dismissed it. A merge's PRs make no decision again while
    /// they wait; a question's sets are dismissed by the daemon.
    pub fn dismiss(&mut self, id: &str, now: u64) -> Result<Decision> {
        let Some(d) = self.open.iter().find(|d| d.id == id) else {
            match self.find(id) {
                Some(d) => bail!("{id} is {} already", d.state.word()),
                None => bail!("no decision {id} (`tower decisions` lists them)"),
            }
        };
        for p in d.prs.clone() {
            self.settled_prs.insert(p.key, State::Dismissed);
        }
        Ok(self.close(id, State::Dismissed, now).expect("open"))
    }
}

/// What the card merges, and only that: a PR that can't go with the
/// others (a draft, conflicts, failing checks: `PrRef::held_back`) is left
/// out of it; its own row says why. None can go: they're named as not ready.
fn merge_title(refs: &[PrRef]) -> String {
    let repo = refs.first().map(|r| format!(" ({})", r.repo)).unwrap_or_default();
    let ready: Vec<String> = refs.iter().filter(|r| r.held_back().is_none()).map(PrRef::short).collect();
    if ready.is_empty() {
        let each: Vec<String> = refs.iter().map(PrRef::short).collect();
        return format!("Not ready to merge: {}{repo}", each.join(", "));
    }
    format!("Merge {}{repo}", ready.join(", then "))
}

/// One option per PR, in order. The tower's pick is the first that goes in the card's merge, when it can
/// be merged, its checks pass, it doesn't ship to production (that one is
/// the user's call alone), its score doesn't say a person must read it and
/// its plan's checks all passed. A PR that needs review, or whose planned
/// checks are missing, says so in its option.
fn merge_options(refs: &[PrRef]) -> Vec<Opt> {
    // The first that goes in (one left out comes after it, whatever its number).
    let first = refs.iter().position(|r| r.held_back().is_none());
    refs.iter()
        .enumerate()
        .map(|(i, p)| {
            let blocked = p.merge["blocked"].as_str();
            let review = p.score.as_ref().filter(|s| s.needs_review);
            let missing = p.validation.as_ref().filter(|v| !v.checks.is_empty() && !v.all_passed());
            let mut why = match (blocked, review) {
                (Some(b), _) => b.to_string(),
                (None, Some(s)) => format!("{}; {}", p.note, s.line),
                (None, None) => p.note.clone(),
            };
            if let Some(v) = missing.filter(|_| blocked.is_none()) {
                why += &format!("; {}", v.line());
            }
            // Left out of the card's merge: said so, never offered as a merge.
            let label = match p.held_back() {
                Some(r) => format!("#{} isn't in this merge: {r}", p.number),
                None => format!("Merge {}", p.short()),
            };
            Opt {
                label,
                why,
                pick: Some(i) == first && blocked.is_none() && p.checks == "pass" && !crate::prs::is_prod(&p.base) && review.is_none() && missing.is_none(),
                ..Default::default()
            }
        })
        .collect()
}

// ---- answering ----

/// What an answer to a decision does.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// Its question sets (session, set id) are answered with `answers`.
    Questions { sets: Vec<(String, String)>, answers: Value, submit: bool },
    /// `text` goes to its sessions as an instruction (`label`: what was chosen).
    Send { to: Vec<Who>, text: String, label: String },
    /// Its failed task goes to its session once more (`text`: more words for it).
    Requeue { task: String, text: String },
    /// Its failed task is dropped.
    Abandon { task: String },
    /// What a restart closed is put back, or let go (`label`: what was chosen).
    Reopen { pick: crate::reopen::Pick, label: String },
    /// The sessions waiting for an account go on at the reset (`wait`), or are let go.
    Accounts { wait: bool, label: String },
}

/// What answering `d` with `body` does: `{"choice": n}` (an option's index),
/// `{"other": "text"}`, or for a question the answers a question set takes
/// (`[{"tab", "choice"}]`, or `{"answers": […], "submit": bool}`).
pub fn act(d: &Decision, body: &Value, submit: bool) -> Result<Act> {
    let choice = body.get("choice").and_then(Value::as_u64).map(|n| n as usize);
    let other = body.get("other").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
    let answers = if body.is_array() { Some(body.clone()) } else { body.get("answers").filter(|a| a.is_array()).cloned() };
    let submit = body.get("submit").and_then(Value::as_bool).unwrap_or(submit);
    let option = |n: usize| match d.options.get(n) {
        Some(o) => Ok(o),
        None if d.options.is_empty() => bail!("{} offers no options: answer it in words", d.id),
        None => bail!("{} has no option {n}: it has {} (0 to {})", d.id, d.options.len(), d.options.len() - 1),
    };
    match d.kind {
        Kind::Question | Kind::Permission => {
            let sets: Vec<(String, String)> =
                d.sessions.iter().filter_map(|w| w.question_id.clone().map(|q| (w.address.clone(), q))).collect();
            if sets.is_empty() {
                bail!("{} has no open question to answer", d.id);
            }
            let answers = match (answers, choice, other) {
                (Some(a), _, _) => a,
                (None, Some(n), _) => {
                    let o = option(n)?;
                    if !o.answers.is_array() {
                        bail!("{}: option {n} can't be answered by its number; give the answers", d.id);
                    }
                    o.answers.clone()
                }
                (None, None, Some(text)) if d.tabs <= 1 => json!([{ "tab": 0, "choice": { "other": text } }]),
                (None, None, Some(_)) => bail!("{} asks {} questions: answer each one ([{{\"tab\":0,\"choice\":…}}, …])", d.id, d.tabs),
                (None, None, None) => bail!("give {{\"choice\": n}}, {{\"other\": \"text\"}} or the answers"),
            };
            Ok(Act::Questions { sets, answers, submit })
        }
        Kind::Merge => bail!(
            "a merge isn't answered here: the user merges its PR from the chat's chip or the phone ({}); `tower dismiss {}` takes it out of the queue",
            d.prs.iter().map(|p| p.key.as_str()).collect::<Vec<_>>().join(", "),
            d.id
        ),
        // Nothing typed reaches a dialog the tower can't read: the user answers it in the session.
        Kind::Blocked => bail!(
            "{} is answered in the session itself ({}): `tower show` brings it on screen; it closes by itself once the session goes on",
            d.id,
            d.sessions.iter().map(|w| w.address.as_str()).collect::<Vec<_>>().join(", ")
        ),
        // What was lost to a restart: the tower puts it back itself, nothing is sent.
        Kind::Choice if d.why == REOPEN_WHY => {
            let (pick, label) = match (choice, other) {
                (Some(n), _) => {
                    let o = option(n)?;
                    let Some(pick) = crate::reopen::Pick::of_code(&o.text) else { bail!("{}: option {n} says nothing to do", d.id) };
                    (pick, o.label.trim().to_string())
                }
                (None, Some(text)) => (crate::reopen::Pick::Words(text.to_string()), text.to_string()),
                (None, None) => bail!("give {{\"choice\": n}} or {{\"other\": \"the spaces to reopen\"}} (`tower dismiss {}`: not now)", d.id),
            };
            Ok(Act::Reopen { pick, label })
        }
        // Every account out: the tower goes on at the reset, or lets them go; nothing is sent.
        Kind::Choice if d.why == ACCOUNTS_WHY => {
            let (wait, label) = match (choice, other) {
                (Some(n), _) => {
                    let o = option(n)?;
                    let Some(wait) = crate::failover::of_code(&o.text) else { bail!("{}: option {n} says nothing to do", d.id) };
                    (wait, o.label.trim().to_string())
                }
                (None, Some(text)) => {
                    let l = text.to_lowercase();
                    let stop = ["stop", "leave", "no", "don't", "forget"].iter().any(|w| l.split(|c: char| !c.is_alphanumeric() && c != '\'').any(|x| x == *w));
                    (!stop, text.to_string())
                }
                (None, None) => bail!("give {{\"choice\": n}} or {{\"other\": \"wait\" or \"leave them stopped\"}}"),
            };
            Ok(Act::Accounts { wait, label })
        }
        Kind::Choice | Kind::Raised => {
            let about = clip(d.title.trim().trim_end_matches('.'), 240);
            let (text, label) = match (choice, other) {
                (Some(n), _) => {
                    let o = option(n)?;
                    let label = o.label.trim().trim_end_matches('.').to_string();
                    (if o.text.is_empty() { format!("About \"{about}\": the user decided: {label}.") } else { o.text.clone() }, label)
                }
                (None, Some(text)) => (format!("About \"{about}\": the user says: {text}"), text.to_string()),
                (None, None) => {
                    bail!("give {{\"choice\": n}} or {{\"other\": \"text\"}} (`tower dismiss {}` closes it without an answer)", d.id)
                }
            };
            let mut to: Vec<Who> = d.sessions.iter().filter(|w| w.waiting).cloned().collect();
            // A PR's review the two sessions can't agree on (`reviews`): the user's word is for both.
            if to.is_empty() || d.why == "review" {
                to = d.sessions.clone();
            }
            Ok(Act::Send { to, text, label })
        }
        Kind::Failed => {
            let Some(task) = d.task.clone() else { bail!("{} names no task", d.id) };
            match (choice, other) {
                (Some(0), _) => Ok(Act::Requeue { task, text: String::new() }),
                (Some(1), _) => Ok(Act::Abandon { task }),
                (Some(n), _) => bail!("{} has no option {n}: 0 tries it again, 1 drops it", d.id),
                (None, Some(text)) => Ok(Act::Requeue { task, text: text.to_string() }),
                (None, None) => {
                    bail!("give {{\"choice\": 0}} (try again), {{\"choice\": 1}} (drop it) or {{\"other\": \"what to do instead\"}}")
                }
            }
        }
    }
}

/// Which of a question decision's sets an answer goes to, so nobody answers
/// what they didn't see. `open`: its sessions' sets that are still open.
/// The answer was put together on one card: the set `body["shown"]` names
/// (the windows say which they drew), else the decision's own (its first
/// session's). An option chosen by its number (`{"choice": n}`, with
/// `"label"` when the sender knows what it showed) must still be what that
/// card offers. Another session's set takes the answer only when it means
/// the same there (`questions::answers_mean_the_same`); one that offers
/// other answers is left open, with why. `Err`: the card changed since it
/// was shown, and nothing is to be typed anywhere.
pub fn takers<'a>(
    d: &Decision,
    body: &Value,
    answers: &[crate::questions::Answer],
    open: &'a [QuestionSet],
) -> std::result::Result<(Vec<&'a QuestionSet>, Vec<(String, String)>), String> {
    let changed = || format!("{} changed since it was shown (a session answered or moved on, or its answers were read again): look at the card again", d.id);
    let shown = body.get("shown").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let card = match shown {
        Some(id) => open.iter().find(|s| s.id == id).filter(|_| d.sessions.iter().any(|w| w.question_id.as_deref() == Some(id))),
        None => d.question_id.as_deref().and_then(|q| open.iter().find(|s| s.id == q)).or_else(|| open.first()),
    };
    let Some(card) = card else {
        // None of its sets is open (each says so itself), unless the one that was shown is the one that went.
        return if shown.is_some() { Err(changed()) } else { Ok((open.iter().collect(), vec![])) };
    };
    let by_number = body.get("answers").is_none() && !body.is_array();
    if let Some(n) = body.get("choice").and_then(Value::as_u64).filter(|_| by_number) {
        let (kept, now) = (d.options.get(n as usize), options_of(card));
        let same = matches!((kept, now.get(n as usize)), (Some(a), Some(b)) if a.label == b.label && a.answers == b.answers);
        let as_shown = body.get("label").and_then(Value::as_str).is_none_or(|l| kept.is_some_and(|o| o.label.trim() == l.trim()));
        if !same || !as_shown {
            return Err(changed());
        }
    }
    let (mut take, mut skip) = (vec![], vec![]);
    for set in open {
        // Only submitting (every tab was answered in its terminal): that card's dialog, nobody else's.
        let submit_only = answers.is_empty() && set.id != card.id;
        if set.id == card.id || (!submit_only && crate::questions::answers_mean_the_same(card, set, answers)) {
            take.push(set);
        } else {
            skip.push((set.session.clone(), "it offers other answers than the card showed: it stays open and gets its own card".to_string()));
        }
    }
    Ok((take, skip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_goes_only_where_it_means_what_the_card_showed() {
        use crate::questions::{Answer, Opt as Offer, Tab};
        // Two sessions of one space ask the same words; their dialogs offer different things.
        let dialog = |id: &str, session: &str, labels: &[&str]| {
            let tab = Tab { title: "Next".into(), question: "How would you like to proceed?".into(), options: labels.iter().map(|l| Offer { label: l.to_string(), ..Default::default() }).collect(), allows_other: true, ..Default::default() };
            let mut set = QuestionSet { id: id.into(), session: session.into(), name: session.into(), kind: "claude-ask".into(), title: "How would you like to proceed?".into(), tabs: vec![tab], space: Some("Shop".into()), ..Default::default() };
            set.state.decision = crate::daemon::card::Decision::Escalated;
            set
        };
        let a = dialog("qa", "t1", &["Keep the table", "Drop the table"]);
        let b = dialog("qb", "t2", &["Drop the table", "Keep the table"]);
        let c = dialog("qc", "t3", &["Keep the table", "Drop the table"]);
        let mut book = Book::default();
        let sets = [a.clone(), b.clone(), c.clone()];
        book.sync_questions(&sets.iter().filter_map(asked).collect::<Vec<_>>(), &|_| State::Stopped, 1_000);
        let d = book.open[0].clone();
        assert_eq!((book.open.len(), d.sessions.len(), d.question_id.as_deref(), d.options[0].label.as_str()), (1, 3, Some("qa"), "Keep the table"), "one decision, its card the first session's");
        // "1" on the card is "Keep the table": it goes to the two dialogs where the first answer is that one.
        let body = json!({ "choice": 0 });
        let Ok(Act::Questions { answers, .. }) = act(&d, &body, true) else { panic!("a question") };
        let answers: Vec<Answer> = serde_json::from_value(answers).unwrap();
        let (take, skip) = takers(&d, &body, &answers, &sets).unwrap();
        assert_eq!(take.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["qa", "qc"], "never into the dialog whose first answer is \"Drop the table\"");
        assert_eq!((skip.len(), skip[0].0.as_str()), (1, "t2"));
        // Words typed mean the same in all three.
        let words = vec![Answer { tab: 0, choice: json!({ "other": "keep it, and add an index" }), ..Default::default() }];
        assert_eq!(takers(&d, &json!({ "other": "keep it, and add an index" }), &words, &sets).unwrap().0.len(), 3);
        // The first session answered in its own terminal meanwhile: the card a window drew is gone.
        let left = [b.clone(), c.clone()];
        let drawn = json!({ "answers": [{ "tab": 0, "choice": 0 }], "shown": "qa" });
        assert!(takers(&d, &drawn, &answers, &left).unwrap_err().contains("changed since it was shown"));
        // The decision's card was drawn again from the next session before the key came: its number is another answer now.
        book.sync_questions(&left.iter().filter_map(asked).collect::<Vec<_>>(), &|_| State::Answered, 2_000);
        let d2 = book.open[0].clone();
        assert_eq!((d2.id == d.id, d2.question_id.as_deref(), d2.options[0].label.as_str()), (true, Some("qb"), "Drop the table"));
        assert!(takers(&d2, &json!({ "choice": 0, "label": "Keep the table" }), &answers, &left).is_err(), "what was shown at 1 isn't what 1 is now");
        let (take, _) = takers(&d2, &json!({ "choice": 0, "label": "Drop the table" }), &answers, &left).unwrap();
        assert_eq!(take.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["qb"]);
        // A set whose answers were read again (recommendations came) since the decision was kept: look again.
        let mut recommended = b.clone();
        recommended.recommendations = Some(vec![crate::questions::TabRecs { tab: 0, options: vec![crate::questions::Rec { label: "Keep it".into(), why: "it is in use".into(), choice: json!(1) }] }]);
        assert!(takers(&d2, &json!({ "choice": 0 }), &answers, &[recommended, c.clone()]).is_err());
        // The answers a window sends for the set it drew go to that set, and to those that offer the same.
        let (take, skip) = takers(&d2, &json!({ "answers": [{ "tab": 0, "choice": 0 }], "shown": "qc" }), &answers, &left).unwrap();
        assert_eq!((take.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), skip.len()), (vec!["qc"], 1));
    }

    #[test]
    fn an_answer_becomes_a_rule_only_where_that_is_safe() {
        // What a session asked, answered from its card: offered, in the user's words, for that space.
        let rule = rule_for(Kind::Question, "design", "Which chart library for the dashboard?", Some("sandbox"), "Recharts.").unwrap();
        assert_eq!(rule, "Make this a rule: when a session in sandbox asks \"Which chart library for the dashboard?\", answer \"Recharts\" without asking me.");
        // A choice the tower raised; no space: any session.
        assert_eq!(
            rule_for(Kind::Choice, "design", "Tabs or a sidebar for the \"settings\" page", None, "A sidebar").as_deref(),
            Some("Make this a rule: when a session asks \"Tabs or a sidebar for the 'settings' page\", answer \"A sidebar\" without asking me.")
        );
        // Never for a merge, a permission, a failed task, a plain raise; nor with nothing answered.
        for kind in [Kind::Merge, Kind::Permission, Kind::Failed, Kind::Raised] {
            assert_eq!(rule_for(kind, "design", "Which chart library?", Some("sandbox"), "Recharts"), None, "{kind:?}");
        }
        assert_eq!(rule_for(Kind::Question, "design", "Which chart library?", Some("sandbox"), "  "), None);
        // Never for what the user always decides: deploys, deletes, production data, secrets, money.
        for (why, title, answer) in [
            ("it deploys or releases", "Ship it now?", "Yes"),
            ("design", "Should I deploy the dashboard?", "Yes"),
            ("design", "Delete the old migrations?", "Yes"),
            ("design", "Run the backfill against production?", "Yes"),
            ("design", "Which API token goes into .env?", "The staging one"),
            ("money", "Upgrade the plan?", "Yes"),
            ("design", "Which tier?", "The paid one, charge the card"),
            ("design", "Drop the users table first?", "No"),
        ] {
            assert_eq!(rule_for(Kind::Question, why, title, Some("sandbox"), answer), None, "{title}");
        }
        assert!(always_yours("it deletes or overwrites") && always_yours("Deploying to PROD") && always_yours("secrets") && always_yours("a refund"));
        assert!(!always_yours("Which keyboard layout?") && !always_yours("Use a dropdown or tabs?") && !always_yours("Remote or local cache"));
        // As they are said at a shell or in passing: never a rule either.
        for text in ["Run rm -rf build first?", "Force push the branch?", "git reset --hard to origin?", "Push to main now?", "Ship it?", "Go live tonight?", "Erase the cache table?", "Purge the old rows?", "Run the migration?", "Put it in .env?", "Start the rollout?"] {
            assert!(always_yours(text), "{text}");
        }
        assert_eq!(rule_for(Kind::Question, "design", "Force push the branch?", Some("sandbox"), "Yes"), None);
        assert!(!always_yours("Which chart library for the dashboard?") && !always_yours("Tabs or a sidebar?") && !always_yours("Environment picker or a toggle?"));
        // A long question is cut, not dropped.
        let long = rule_request(Some("isa"), &"why ".repeat(100), "because");
        assert!(long.chars().count() < 320 && long.contains('…') && long.ends_with("answer \"because\" without asking me."), "{long}");
    }
    use crate::questions;

    const MIN: u64 = 60_000;

    /// A turn that ended waiting for the user, in `space`: a card that is theirs.
    fn waits(session: &str, name: &str, space: &str, request: &str) -> QuestionSet {
        let mut set = questions::wait_set(session, name, request, request, "");
        set.space = Some(space.into());
        crate::daemon::card::sync(&mut set);
        set
    }

    fn asked_all(sets: &[QuestionSet]) -> Vec<Asked> {
        sets.iter().filter_map(asked).collect()
    }

    fn pr(n: u64, base: &str, session: &str, space: &str, checks: &str) -> Pr {
        Pr {
            repo: "o/app".into(),
            number: n,
            base: base.into(),
            state: "open".into(),
            mergeable: "mergeable".into(),
            checks: crate::prs::Checks { state: checks.into(), ..Default::default() },
            session: session.into(),
            name: format!("s-{session}"),
            space: Some(space.into()),
            waiting: true,
            found_ms: 1_000,
            ..Default::default()
        }
    }

    fn who(address: &str) -> Option<Who> {
        Some(Who::new(address, &format!("s-{address}")))
    }

    /// 13 Nov: the brain raised "build the search layer?" for t12v, then
    /// t12v's turn ended waiting for the same approval and became a second
    /// card; the user said yes on one and "Not yet" on the other. A wait from
    /// a session the brain raised something about is that decision again.
    #[test]
    fn a_wait_from_a_session_with_a_raised_decision_joins_it() {
        let mut b = Book::default();
        let options = vec![Opt { label: "Yes, build it".into(), ..Default::default() }, Opt { label: "Not yet".into(), ..Default::default() }];
        let r = b.raise(Raise { space: Some("search-llm".into()), who: who("t12v"), why: "data".into(), text: "Build the search layer?".into(), options, ..Default::default() }, MIN);
        assert_eq!(b.waits_on_raised("t12v", 2 * MIN), Some(r.id.clone()));
        let d = b.find(&r.id).unwrap();
        assert_eq!((d.count, d.last_ms, b.open.len()), (2, 2 * MIN, 1));
        assert!(d.sessions[0].waiting);
        assert_eq!(b.waits_on_raised("t9", 2 * MIN), None, "nothing raised about it: its own card");
        // Over once answered: the next wait is a card of its own.
        b.answered(&r.id, "Yes, build it", 3 * MIN);
        assert_eq!(b.waits_on_raised("t12v", 4 * MIN), None);
        // Only what the brain raised: a blocked session's decision isn't one.
        b.blocked(Some("search-llm".into()), Who::new("t12t", "sooner"), "Asks to edit its Claude notes", 5 * MIN);
        assert_eq!(b.waits_on_raised("t12t", 5 * MIN), None);
    }

    #[test]
    fn keys_read_the_same_thing_the_same() {
        assert_eq!(normalise("  Should I   deploy to PRODUCTION?! "), "should i deploy to production");
        assert_eq!(normalise("Merging #1273 into dev."), normalise("merging 1273 -- into: dev"));
        assert_eq!(ask_key(Some("Shop"), "Deploy now?"), ask_key(Some("shop "), "deploy   now"));
        assert_ne!(ask_key(Some("Shop"), "Deploy now?"), ask_key(Some("Docs"), "Deploy now?"), "another space is another decision");
        assert_ne!(ask_key(Some("Shop"), "Deploy now?"), raise_key(Some("Shop"), "Deploy now?"));
        assert_eq!(merge_key(Some("Shop"), "O/App"), "merge:shop:o/app");
        assert!(is_id(&id_of("ask:shop:x", 5)) && !is_id("q2bd2c2bc9b6a") && !is_id("t5") && !is_id("decisions"));
    }

    #[test]
    fn three_sessions_asking_the_same_thing_are_one_decision() {
        let ask = "Still waiting on you: merging #1273 into dev.";
        let sets = vec![
            waits("t1", "web", "Shop", ask),
            waits("t2", "api", "Shop", ask),
            waits("t3", "sdk", "Shop", "still waiting on you:  merging #1273 into dev"),
        ];
        let mut b = Book::default();
        // They come in one after the other, as their turns end.
        let first = b.sync_questions(&asked_all(&sets[..1]), &|_| State::Stopped, 10 * MIN);
        assert_eq!(first.fresh.len(), 1, "the first one opens it");
        let more = b.sync_questions(&asked_all(&sets), &|_| State::Stopped, 11 * MIN);
        assert!(more.fresh.is_empty() && more.closed.is_empty(), "the same key again opens nothing");
        assert_eq!(b.open.len(), 1);
        let d = b.open[0].clone();
        assert_eq!((d.kind, d.waiting(), d.count, d.sessions.len()), (Kind::Question, 3, 3, 3));
        assert_eq!((d.title.as_str(), d.why.as_str(), d.space.as_deref()), (ask, questions::WAITS, Some("Shop")));
        assert_eq!(d.question_id.as_deref(), Some(sets[0].id.as_str()), "its card is the first session's");
        assert_eq!(d.last_ms, 11 * MIN);
        // The same round again changes nothing (it is not asked a fourth time).
        let again = b.clone();
        b.sync_questions(&asked_all(&sets), &|_| State::Stopped, 12 * MIN);
        assert_eq!(b, again);
        let v = d.view(false);
        assert_eq!(
            (v["waiting"].as_u64(), v["count"].as_u64(), v["kind"].as_str(), v["state"].as_str()),
            (Some(3), Some(3), Some("question"), Some("open"))
        );
        assert_eq!(v["sessions"][1], json!({ "address": "t2", "name": "api", "waiting": true, "question_id": sets[1].id }));
        // The same words in another space are that space's own decision.
        let mut other = sets.clone();
        other.push(waits("t9", "docs", "Docs", ask));
        assert_eq!(b.sync_questions(&asked_all(&other), &|_| State::Stopped, 13 * MIN).fresh.len(), 1);
        assert_eq!(b.open.len(), 2);
    }

    #[test]
    fn a_questions_decision_carries_the_towers_pick_and_closes_with_its_sets() {
        let mut set = questions::text_set("t5", "web", "Deploy to production now, or wait for Monday?", "release the tickets change");
        set.space = Some("Shop".into());
        set.recommendations = Some(vec![questions::TabRecs {
            tab: 0,
            options: vec![
                questions::Rec {
                    label: "Wait for Monday".into(),
                    why: "nobody is on call tonight".into(),
                    choice: json!({ "other": "Wait for Monday." }),
                },
                questions::Rec { label: "Deploy now".into(), why: "the checks pass".into(), choice: json!({ "other": "Deploy now." }) },
            ],
        }]);
        // Not the user's yet: the brain hasn't said.
        assert!(asked(&set).is_none());
        set.state.decision = crate::daemon::card::Decision::Escalated;
        set.reason = "it deploys to production".into();
        let mut b = Book::default();
        b.sync_questions(&asked_all(std::slice::from_ref(&set)), &|_| State::Stopped, MIN);
        let d = b.open[0].clone();
        assert_eq!(
            (d.pick().map(|o| o.label.as_str()), d.options.len(), d.why.as_str()),
            (Some("Wait for Monday"), 2, "it deploys to production")
        );
        assert_eq!(d.view(false)["options"][0], json!({ "label": "Wait for Monday", "why": "nobody is on call tonight", "pick": true }));
        assert_eq!(d.view(false)["pick"], "Wait for Monday");
        // By its number, in words, or with the answers a question set takes.
        let sets = vec![("t5".to_string(), set.id.clone())];
        assert_eq!(
            act(&d, &json!({ "choice": 1 }), true).unwrap(),
            Act::Questions { sets: sets.clone(), answers: json!([{ "tab": 0, "choice": { "other": "Deploy now." } }]), submit: true }
        );
        assert_eq!(
            act(&d, &json!({ "other": " Friday " }), true).unwrap(),
            Act::Questions { sets: sets.clone(), answers: json!([{ "tab": 0, "choice": { "other": "Friday" } }]), submit: true }
        );
        let own = json!([{ "tab": 0, "choice": { "other": "x" } }]);
        assert_eq!(
            act(&d, &json!({ "answers": own, "submit": false }), true).unwrap(),
            Act::Questions { sets, answers: own, submit: false }
        );
        assert!(act(&d, &json!({ "choice": 7 }), true).is_err() && act(&d, &json!({}), true).is_err());
        // Its reply waits for the session to stop: answering. Then the set closes, answered.
        set.state.phase = crate::daemon::card::Phase::Queued;
        b.sync_questions(&asked_all(std::slice::from_ref(&set)), &|_| State::Stopped, 2 * MIN);
        assert_eq!(b.open[0].state, State::Answering);
        let ch = b.sync_questions(&[], &|id| if id == set.id { State::Answered } else { State::Stopped }, 3 * MIN);
        assert_eq!((ch.closed.len(), ch.closed[0].state, ch.closed[0].closed_ms), (1, State::Answered, Some(3 * MIN)));
        assert!(b.open.is_empty());
        assert_eq!(b.find(&d.id).map(|d| d.state), Some(State::Answered), "a late answer hears how it ended");
        // Asked again later: another decision, with its own id.
        set.state.phase = crate::daemon::card::Phase::Open;
        let again = b.sync_questions(&asked_all(std::slice::from_ref(&set)), &|_| State::Stopped, 9 * MIN);
        assert!(again.fresh.len() == 1 && again.fresh[0] != d.id);
    }

    #[test]
    fn a_menu_is_a_permission_and_another_command_is_another_decision() {
        let menu = |session: &str, cmd: &str| {
            let tab = questions::Tab {
                title: "Permission".into(),
                question: "Do you want to proceed?".into(),
                options: vec![
                    questions::Opt { label: "Yes".into(), key: "1".into(), ..Default::default() },
                    questions::Opt { label: "No".into(), description: "tell it what to do instead".into(), key: "2".into() },
                ],
                ..Default::default()
            };
            let mut set = QuestionSet {
                id: format!("q{session}{}", cmd.len()),
                session: session.into(),
                name: session.into(),
                kind: "claude-permission".into(),
                title: "Allow Bash: git push?".into(),
                detail: cmd.into(),
                tabs: vec![tab],
                ..Default::default()
            };
            set.state.decision = crate::daemon::card::Decision::Escalated;
            set
        };
        let a = asked(&menu("t1", "git push origin main")).unwrap();
        assert_eq!((a.kind, a.title.as_str()), (Kind::Permission, "Allow Bash: git push?"));
        // No recommendation came: the menu's own options, with no pick.
        assert_eq!(a.options.iter().map(|o| (o.label.as_str(), o.pick)).collect::<Vec<_>>(), [("Yes", false), ("No", false)]);
        assert_eq!(a.options[1].answers, json!([{ "tab": 0, "choice": 1 }]));
        assert_eq!(a.key, asked(&menu("t2", "git push origin main")).unwrap().key, "the same command in another session");
        assert_ne!(a.key, asked(&menu("t2", "git push --force origin main")).unwrap().key, "another command");
    }

    /// A decision carries what it is about to read before deciding: the
    /// command a permission would run, the plan, the message that asks (its
    /// end), `tower raise --detail`; cut to about 600 characters. A merge has none.
    #[test]
    fn a_decision_carries_its_detail_cut_short() {
        let mut perm = QuestionSet {
            id: "qperm1".into(),
            session: "t1".into(),
            name: "web".into(),
            kind: "claude-permission".into(),
            title: "Allow Bash?".into(),
            detail: "  $ rm -rf build && npm run deploy -- --prod  ".into(),
            tabs: vec![questions::Tab { title: "Permission".into(), question: "Do you want to proceed?".into(), ..Default::default() }],
            ..Default::default()
        };
        perm.state.decision = crate::daemon::card::Decision::Escalated;
        let mut b = Book::default();
        b.sync_questions(&asked_all(&[perm.clone()]), &|_| State::Stopped, MIN);
        let d = &b.open[0];
        assert_eq!(d.detail, "$ rm -rf build && npm run deploy -- --prod", "the command it would run");
        assert_eq!(d.view(false)["detail"], "$ rm -rf build && npm run deploy -- --prod");
        // A plan longer than a card holds: its start, cut with an ellipsis.
        let plan = format!("Plan: {}", "move the billing page to the new router. ".repeat(40));
        let mut p = QuestionSet { id: "qplan1".into(), session: "t2".into(), kind: "claude-plan".into(), title: "Approve the plan?".into(), detail: plan.clone(), ..perm.clone() };
        p.tabs[0].title = "Plan".into();
        let a = asked(&p).unwrap();
        assert!(a.detail.starts_with("Plan: move the billing page") && a.detail.ends_with('…'), "{}", a.detail);
        assert!(a.detail.chars().count() <= DETAIL, "{}", a.detail.chars().count());
        // A message that asks at its end: its end is kept.
        let long = format!("{} Want me to open the PR, or wait?", "I changed the login form and its tests. ".repeat(60));
        let text = crate::questions::text_set("t3", "api", &long, "");
        let a = Asked { ..asked(&QuestionSet { state: perm.state.clone(), ..text }).unwrap() };
        assert!(a.detail.starts_with('…') && a.detail.ends_with("Want me to open the PR, or wait?"), "{}", a.detail);
        assert!(a.detail.chars().count() <= DETAIL);
        // Raised with a detail; raised again with another: the card shows the latest.
        let mut b = Book::default();
        let r = Raise { space: Some("Shop".into()), who: None, why: "deploy".into(), text: "Deploy the tickets change?".into(), detail: "x".repeat(900), options: vec![] };
        let id = b.raise(r.clone(), MIN).id;
        let d = b.find(&id).unwrap();
        assert_eq!((d.detail.chars().count(), d.detail.ends_with('…')), (DETAIL, true));
        b.raise(Raise { detail: "kubectl rollout restart deploy/tickets".into(), ..r.clone() }, 2 * MIN);
        assert_eq!(b.find(&id).unwrap().detail, "kubectl rollout restart deploy/tickets");
        b.raise(Raise { detail: String::new(), ..r }, 3 * MIN);
        assert_eq!(b.find(&id).unwrap().detail, "kubectl rollout restart deploy/tickets", "raised again with none: it keeps it");
        // A merge says it with its PRs.
        let ch = b.sync_merges(&[pr(12, "dev", "t1", "Shop", "passing")], 4 * MIN);
        let m = b.find(&ch.fresh[0]).unwrap();
        assert_eq!((m.kind, m.view(false)["detail"].as_str()), (Kind::Merge, Some("")));
        // Kept across a restart.
        let back: Book = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back.find(&id).unwrap().detail, "kubectl rollout restart deploy/tickets");
    }

    #[test]
    fn the_same_dialog_read_again_keeps_its_decision() {
        let mut old = waits("t1", "web", "Shop", "Please review the plan.");
        old.id = "qold".into();
        let mut b = Book::default();
        b.sync_questions(&asked_all(std::slice::from_ref(&old)), &|_| State::Stopped, MIN);
        let id = b.open[0].id.clone();
        b.claim_told(&id);
        let mut new = waits("t1", "web", "Shop", "Please review the plan and the migration.");
        new.id = "qnew".into();
        new.replaces = Some("qold".into());
        let ch = b.sync_questions(&asked_all(std::slice::from_ref(&new)), &|_| State::Stopped, 2 * MIN);
        assert!(ch.fresh.is_empty() && ch.closed.is_empty(), "nothing opens, nothing closes");
        let d = &b.open[0];
        assert_eq!(
            (d.id.as_str(), d.question_id.as_deref(), d.title.as_str(), d.told, d.count),
            (id.as_str(), Some("qnew"), "Please review the plan and the migration.", true, 1)
        );
    }

    #[test]
    fn the_same_line_raised_three_times_is_one_decision() {
        let mut b = Book::default();
        let raise = |a: &str, text: &str| Raise {
            detail: String::new(),
            space: Some("Shop".into()),
            who: who(a),
            why: "deploy".into(),
            text: text.into(),
            options: vec![],
        };
        let first = b.raise(raise("t1", "Deploy the tickets change to production?"), MIN);
        assert!(first.fresh && !first.folded);
        let second = b.raise(raise("t2", "deploy the tickets change to production"), 2 * MIN);
        let third = b.raise(raise("t3", "Deploy the tickets change  to production?!"), 3 * MIN);
        assert_eq!((second.id.as_str(), second.fresh, third.fresh, third.count, third.waiting), (first.id.as_str(), false, false, 3, 3));
        assert_eq!(b.open.len(), 1);
        let d = &b.open[0];
        assert_eq!((d.kind, d.first_ms, d.last_ms, d.why.as_str()), (Kind::Raised, MIN, 3 * MIN, "deploy"));
        assert_eq!(said("s-t1", &d.title, None), "s-t1 needs you: Deploy the tickets change to production?");
        // The same session raising it again adds nobody.
        assert_eq!(b.raise(raise("t1", "Deploy the tickets change to production?"), 4 * MIN).waiting, 3);
        // Another line is another decision; with options it is a choice, the first one the pick.
        let options = options_from(&json!([{ "label": "Keep the old schema", "why": "no migration" }, "Migrate now"])).unwrap();
        let c = b.raise(Raise { detail: String::new(), options, ..raise("t1", "Which schema for tickets") }, 5 * MIN);
        assert!(c.fresh);
        let d = b.find(&c.id).unwrap();
        assert_eq!((d.kind, d.pick().map(|o| o.label.as_str())), (Kind::Choice, Some("Keep the old schema")));
        assert_eq!(
            said("s-t1", &d.title, d.pick().map(|o| o.label.as_str())),
            "s-t1 needs you: Which schema for tickets. I'd say: Keep the old schema"
        );
        assert_eq!(said("", "Deploy now?", Some("Wait.")), "Deploy now? I'd say: Wait", "about no session it can name");
        assert!(options_from(&json!([{ "why": "no label" }])).is_err() && options_from(&json!({})).is_err());
    }

    #[test]
    fn a_raised_decision_is_answered_with_words_for_its_sessions() {
        let mut b = Book::default();
        let options =
            options_from(&json!([{ "label": "Wait for Monday." }, { "label": "Deploy now", "text": "Deploy the tickets change now." }]))
                .unwrap();
        let r =
            b.raise(Raise { detail: String::new(), space: None, who: who("t1"), why: "deploy".into(), text: "Deploy the tickets change?".into(), options }, MIN);
        b.raise(
            Raise { detail: String::new(), space: None, who: who("t2"), why: "deploy".into(), text: "Deploy the tickets change?".into(), options: vec![] },
            MIN,
        );
        // One of them works on something else meanwhile: the other is who waits.
        assert!(b.session_waits("t2", false) && !b.session_waits("t2", false));
        let d = b.find(&r.id).unwrap().clone();
        assert_eq!(d.waiting(), 1);
        let to = vec![Who::new("t1", "s-t1")];
        assert_eq!(
            act(&d, &json!({ "choice": 0 }), true).unwrap(),
            Act::Send {
                to: to.clone(),
                text: "About \"Deploy the tickets change?\": the user decided: Wait for Monday.".into(),
                label: "Wait for Monday".into()
            }
        );
        assert_eq!(
            act(&d, &json!({ "choice": 1 }), true).unwrap(),
            Act::Send { to: to.clone(), text: "Deploy the tickets change now.".into(), label: "Deploy now".into() }
        );
        assert_eq!(
            act(&d, &json!({ "other": "Friday morning" }), true).unwrap(),
            Act::Send {
                to,
                text: "About \"Deploy the tickets change?\": the user says: Friday morning".into(),
                label: "Friday morning".into()
            }
        );
        assert!(act(&d, &json!({}), true).is_err() && act(&d, &json!({ "choice": 2 }), true).is_err());
        // Nobody waits: it goes to all of them.
        b.session_waits("t1", false);
        assert!(matches!(act(b.find(&r.id).unwrap(), &json!({ "choice": 0 }), true).unwrap(), Act::Send { to, .. } if to.len() == 2));
    }

    #[test]
    fn a_stuck_review_is_answered_to_its_owner_and_its_reviewer_alike() {
        let mut b = Book::default();
        let r = crate::reviews::Review { key: "acme/shop#12".into(), repo: "acme/shop".into(), number: 12, owner: "t1".into(), reviewer: "t2".into(), rounds: 3, critical: 1, line: "totals lose cents".into(), ..Default::default() };
        let (title, detail, options) = crate::reviews::stuck_decision(&r);
        let raise = |w: &str| Raise { space: Some("shop".into()), who: who(w), why: "review".into(), text: title.clone(), detail: detail.clone(), options: options.clone() };
        let first = b.raise(raise("t1"), MIN);
        let second = b.raise(raise("t2"), MIN);
        assert_eq!((first.id.clone(), first.fresh, second.fresh), (second.id.clone(), true, false), "one decision about both");
        let d = b.find(&first.id).unwrap().clone();
        assert_eq!((d.kind, d.pick().is_none(), d.sessions.len()), (Kind::Choice, true, 2));
        // The owner works on something meanwhile: the user's word still goes to both.
        b.session_waits("t1", false);
        match act(b.find(&first.id).unwrap(), &json!({ "choice": 1 }), true).unwrap() {
            Act::Send { to, text, label } => {
                assert_eq!(to.iter().map(|w| w.address.as_str()).collect::<Vec<_>>(), ["t1", "t2"]);
                assert_eq!(label, "Side with the reviewer");
                assert!(text.starts_with("About the review of PR #12 (acme/shop): the user sides with the reviewer."), "{text}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_merge_decision_carries_each_prs_review_score() {
        let mut b = Book::default();
        let ev = |files: &[&str]| crate::prscore::Evidence { title: "x".into(), files: files.iter().map(|f| f.to_string()).collect(), ..Default::default() };
        let migration = Pr { score: Some(crate::prscore::score(&ev(&["db/migrations/0042_orders.sql"]))), ..pr(88, "dev", "t1", "Shop", "passing") };
        let ui = Pr { score: Some(crate::prscore::score(&ev(&["src/Cart.tsx"]))), ..pr(89, "dev", "t2", "Shop", "passing") };
        let id = b.sync_merges(&[migration, ui], MIN).fresh[0].clone();
        let d = b.find(&id).unwrap();
        let v = d.view(false);
        assert_eq!(v["prs"][0]["score"]["needs_review"], true);
        assert_eq!(v["prs"][0]["score"]["line"], "Needs review: a database migration (db/migrations/0042_orders.sql)");
        assert_eq!(v["prs"][1]["score"]["needs_review"], false);
        // The tower doesn't pick a PR a person must read; its option says why.
        assert!(d.pick().is_none(), "{:?}", d.options);
        assert_eq!(d.options[0].why, "checks passing; Needs review: a database migration (db/migrations/0042_orders.sql)");
        assert_eq!(d.options[1].why, "checks passing");
    }

    /// "Wait for the ingest filters, then we go for prod": the merge card
    /// stays, on hold, until its PR changes, what it waits for merges, or
    /// it is let go; held, nothing about it counts as an update but that.
    #[test]
    fn a_held_decision_comes_back_when_it_is_updated() {
        let mut b = Book::default();
        let release = |checks: &str, sha: &str| Pr { head_sha: sha.into(), ..pr(830, "main", "t1", "isa", checks) };
        let id = b.sync_merges(&[release("failing", "aaa")], MIN).fresh[0].clone();
        let d = b.hold(&id, "wait for the ingest   filters then we go for prod", "tower-isa", 2 * MIN).unwrap();
        let h = d.hold.as_ref().unwrap();
        assert_eq!((h.reason.as_str(), h.by.as_str(), h.since_ms, h.until_prs.len()), ("wait for the ingest filters then we go for prod", "tower-isa", 2 * MIN, 0));
        assert!(d.held() && d.state == State::Open, "still open: answered, merged or dismissed as any other");
        let v = d.view(false);
        assert_eq!((v["hold"]["reason"].as_str(), v["back"].is_null()), (Some("wait for the ingest filters then we go for prod"), true));
        // Seen again as it was (a review or score moving is no update): still on hold.
        let mut same = release("failing", "aaa");
        same.review = Some(Default::default());
        b.sync_merges(&[same], 3 * MIN);
        assert!(b.wake(3 * MIN).is_empty() && b.find(&id).unwrap().held());
        // Its checks change: it comes back, why said, told once more.
        b.sync_merges(&[release("passing", "aaa")], 4 * MIN);
        assert_eq!(b.wake(4 * MIN), [(id.clone(), "PR #830's checks pass now".to_string())]);
        let d = b.find(&id).unwrap();
        assert!(!d.held() && !d.told && d.back.as_ref().is_some_and(|x| x.why == "PR #830's checks pass now" && x.at_ms == 4 * MIN));
        assert_eq!(std::mem::take(&mut b.came_back), [(id.clone(), "PR #830's checks pass now".to_string())]);
        // Held again: new commits bring it back.
        b.hold(&id, "", "mac", 5 * MIN).unwrap();
        assert!(b.find(&id).unwrap().back.is_none(), "held again: the old \"back\" is gone");
        b.sync_merges(&[release("passing", "bbb")], 6 * MIN);
        assert_eq!(b.wake(6 * MIN), [(id.clone(), "PR #830 has new commits".to_string())]);
        // Held until a PR its words name: that PR merging brings it back (another one doesn't).
        let d = b.hold(&id, "wait for #829 to land", "mac", 7 * MIN).unwrap();
        assert_eq!(d.hold.as_ref().unwrap().until_prs, ["o/app#829"]);
        let ingest = |state: &str, n: u64| Pr { state: state.into(), waiting: false, ..pr(n, "dev", "t2", "isa", "passing") };
        b.sync_merges(&[release("passing", "bbb"), ingest("merged", 828), ingest("open", 829)], 8 * MIN);
        assert!(b.find(&id).unwrap().held(), "another PR merged; #829 is still open");
        b.came_back.clear();
        b.sync_merges(&[release("passing", "bbb"), ingest("merged", 829)], 9 * MIN);
        assert_eq!(b.came_back, [(id.clone(), "PR #829 merged, which it waited for".to_string())]);
        assert!(!b.find(&id).unwrap().held());
        // Its own PR named in its words isn't something it waits for.
        assert!(b.hold(&id, "until #830's checks pass", "mac", 10 * MIN).unwrap().hold.unwrap().until_prs.is_empty());
        // Let go by the user; then there is no hold to let go of.
        assert_eq!(b.unhold(&id, "you let it go from hold", 11 * MIN).unwrap().back.unwrap().why, "you let it go from hold");
        assert!(b.unhold(&id, "x", 12 * MIN).unwrap_err().to_string().contains("isn't on hold"));
        // A raise held, raised again (its session posted something new about it): back.
        let r = b.raise(Raise { space: Some("isa".into()), who: who("t3"), why: "design".into(), text: "Tabs or a sidebar?".into(), ..Default::default() }, 13 * MIN);
        b.hold(&r.id, "after the demo", "mac", 14 * MIN).unwrap();
        b.raise(Raise { space: Some("isa".into()), who: who("t3"), why: "design".into(), text: "tabs or a sidebar".into(), ..Default::default() }, 15 * MIN);
        assert_eq!(b.wake(15 * MIN), [(r.id.clone(), "it came up again".to_string())]);
        // A blocked session seen again on the same screen is no update; another screen is.
        let blocked = b.blocked(Some("isa".into()), Who::new("t4", "api"), "Asks to use Frobnicate", 16 * MIN).id;
        b.hold(&blocked, "", "mac", 16 * MIN).unwrap();
        b.blocked(Some("isa".into()), Who::new("t4", "api"), "Asks to use Frobnicate", 17 * MIN);
        assert!(b.wake(17 * MIN).is_empty());
        b.blocked(Some("isa".into()), Who::new("t4", "api"), "Asks to delete the cache", 18 * MIN);
        assert_eq!(b.wake(18 * MIN), [(blocked, "what it asks changed".to_string())]);
        // Only an open one is held.
        b.dismiss(&r.id, 16 * MIN).unwrap();
        assert!(b.hold(&r.id, "", "mac", 17 * MIN).unwrap_err().to_string().contains("is dismissed already"));
        assert!(b.hold("d000000000000", "", "mac", 17 * MIN).unwrap_err().to_string().contains("no decision"));
    }

    /// 14 Nov: eyzflow's card in sooner-chatbot-answers listed #840 and
    /// #841, drafts, beside #839 and #842. A draft, or a PR still under
    /// review, waits for nobody: it is in no merge decision (nor its count)
    /// until it is ready, then comes in, dev before main. One that can't go
    /// with the others (checks fail) is on the card, said to be left out.
    #[test]
    fn a_merge_decision_takes_its_prs_once_they_are_ready() {
        let mut b = Book::default();
        let draft = |n| Pr { draft: true, ..pr(n, "dev", "t1", "Sooner", "pending") };
        let reviewing = |n, base| Pr { review: Some(crate::reviews::Brief { status: "reviewing".into(), ..Default::default() }), ..pr(n, base, "t1", "Sooner", "passing") };
        let mut prs = vec![pr(839, "dev", "t1", "Sooner", "passing"), draft(840), draft(841), reviewing(842, "dev"), reviewing(830, "main")];
        let id = b.sync_merges(&prs, 5 * MIN).fresh[0].clone();
        let d = b.find(&id).unwrap();
        assert_eq!(d.prs.iter().map(|p| p.number).collect::<Vec<_>>(), [839], "drafts and PRs under review aren't the user's yet");
        assert_eq!(d.title, "Merge PR #839 into dev (o/app)");
        // Only drafts: no decision at all.
        assert!(Book::default().sync_merges(&[draft(840), draft(841)], 5 * MIN).fresh.is_empty());
        // #842's review agreed, #841 marked ready, #830 (into main) agreed: in, dev before main.
        prs[3].review.as_mut().unwrap().status = "agreed".into();
        prs[2].draft = false;
        prs[4].review.as_mut().unwrap().status = "agreed".into();
        let ch = b.sync_merges(&prs, 6 * MIN);
        assert!(ch.fresh.is_empty(), "the same card, more on it");
        let d = b.find(&id).unwrap();
        assert_eq!(d.prs.iter().map(|p| p.number).collect::<Vec<_>>(), [839, 841, 842, 830]);
        assert_eq!(d.title, "Merge PR #839 into dev, then PR #841 into dev, then PR #842 into dev, then PR #830 into main (o/app)");
        // Checks that fail: on the card, but not in its merge, and said so.
        prs[2].checks.state = "failing".into();
        b.sync_merges(&prs, 7 * MIN);
        let d = b.find(&id).unwrap();
        assert_eq!(d.title, "Merge PR #839 into dev, then PR #842 into dev, then PR #830 into main (o/app)");
        assert_eq!(
            d.options.iter().map(|o| (o.label.as_str(), o.pick)).collect::<Vec<_>>(),
            [("Merge PR #839 into dev", true), ("#841 isn't in this merge: its checks fail", false), ("Merge PR #842 into dev", false), ("Merge PR #830 into main", false)]
        );
        // The first in order failing: the pick is the first that merges.
        let mut b = Book::default();
        let id = b.sync_merges(&[pr(838, "dev", "t1", "Sooner", "failing"), pr(839, "dev", "t1", "Sooner", "passing")], 5 * MIN).fresh[0].clone();
        assert_eq!(b.find(&id).unwrap().pick().map(|o| o.label.as_str()), Some("Merge PR #839 into dev"));
        // None can merge: named as not ready.
        let mut b = Book::default();
        let id = b.sync_merges(&[pr(840, "dev", "t1", "Sooner", "failing")], 5 * MIN).fresh[0].clone();
        assert_eq!(b.find(&id).unwrap().title, "Not ready to merge: PR #840 into dev (o/app)");
    }

    /// The card's PRs carry their description and its summary (for the Mac's card and the phone's).
    #[test]
    fn a_merge_decisions_prs_carry_what_they_do() {
        let mut b = Book::default();
        let body = "Adds the Knowledge gaps column.";
        let described = Pr {
            body: Some(body.into()),
            described: Some(crate::describe::Described { summary: "Support staff see open gaps.".into(), of: crate::describe::hash(body) }),
            ..pr(839, "dev", "t1", "Sooner", "passing")
        };
        let stale = Pr { body: Some("Edited.".into()), ..described.clone() };
        let empty = Pr { body: Some(String::new()), asked_body: true, ..pr(842, "dev", "t1", "Sooner", "passing") };
        let id = b.sync_merges(&[described, Pr { number: 840, ..stale }, empty], 5 * MIN).fresh[0].clone();
        let v = b.find(&id).unwrap().view(false);
        assert_eq!((v["prs"][0]["body"].as_str(), v["prs"][0]["summary"].as_str()), (Some(body), Some("Support staff see open gaps.")));
        assert_eq!(v["prs"][1]["summary"], serde_json::Value::Null, "a summary of an older text isn't passed on");
        assert_eq!((v["prs"][2]["body"].as_str(), v["prs"][2]["asked_body"].as_bool()), (Some(""), Some(true)));
    }

    #[test]
    fn a_repos_waiting_prs_are_one_merge_decision_in_order() {
        let mut b = Book::default();
        let mut prs = vec![pr(89, "main", "t1", "Shop", "pending")];
        let first = b.sync_merges(&prs, 5 * MIN);
        assert_eq!(first.fresh.len(), 1);
        // Another session's PR of the same repo, into dev: the same decision, dev first.
        prs.push(pr(88, "dev", "t2", "Shop", "passing"));
        // A third session's, and one in another repo and one in another space: their own.
        prs.push(pr(90, "dev", "t3", "Shop", "failing"));
        prs.push(Pr { repo: "o/sdk".into(), ..pr(7, "main", "t4", "Shop", "passing") });
        prs.push(pr(91, "dev", "t5", "Docs", "passing"));
        // One nobody waits on, and one that is merged: not the user's to merge now.
        prs.push(Pr { waiting: false, ..pr(92, "dev", "t1", "Shop", "passing") });
        prs.push(Pr { state: "merged".into(), ..pr(93, "dev", "t1", "Shop", "passing") });
        let more = b.sync_merges(&prs, 6 * MIN);
        assert_eq!(more.fresh.len(), 2, "the other repo, the other space");
        assert_eq!(b.open.len(), 3);
        let d = b.find(&first.fresh[0]).unwrap();
        assert_eq!((d.kind, d.key.as_str(), d.why.as_str(), d.first_ms), (Kind::Merge, "merge:shop:o/app", "merge", 1_000));
        assert_eq!(
            d.prs.iter().map(|p| (p.number, p.base.as_str(), p.checks.as_str())).collect::<Vec<_>>(),
            [(88, "dev", "pass"), (90, "dev", "fail"), (89, "main", "pending")]
        );
        // #90's checks fail: it isn't in the card's merge, and its answer says so.
        assert_eq!(d.title, "Merge PR #88 into dev, then PR #89 into main (o/app)");
        assert_eq!(d.options[1].label, "#90 isn't in this merge: its checks fail");
        assert_eq!((d.waiting(), d.count, d.last_ms), (3, 2, 6 * MIN), "three sessions wait on one card");
        assert_eq!(d.pick().map(|o| o.label.as_str()), Some("Merge PR #88 into dev"), "the first in order, its checks pass");
        let v = d.view(false);
        assert_eq!(v["prs"][0]["key"], "o/app#88");
        assert_eq!(
            (v["prs"][0]["repo"].as_str(), v["prs"][0]["number"].as_u64(), v["prs"][0]["base"].as_str(), v["prs"][0]["checks"].as_str()),
            (Some("o/app"), Some(88), Some("dev"), Some("pass"))
        );
        assert_eq!(v["prs"][0]["merge"]["can"], true);
        assert!(act(d, &json!({ "choice": 0 }), true).unwrap_err().to_string().contains("o/app#88"), "a merge is its PR's own flow");
        // Its checks move: changed in place, nothing new.
        prs[0].checks.state = "passing".into();
        let id = d.id.clone();
        let ch = b.sync_merges(&prs, 7 * MIN);
        assert!(ch.fresh.is_empty() && ch.closed.is_empty());
        assert_eq!(b.find(&id).unwrap().prs[2].checks, "pass");
        // Into production with passing checks alone: the tower picks nothing there.
        let sdk = b.open.iter().find(|d| d.key == "merge:shop:o/sdk").unwrap();
        assert!(sdk.pick().is_none());
        // They merge one by one; the decision goes with the last.
        prs[1].state = "merged".into();
        prs[1].waiting = false;
        prs[2].state = "closed".into();
        b.sync_merges(&prs, 8 * MIN);
        assert_eq!(b.find(&id).unwrap().prs.iter().map(|p| p.number).collect::<Vec<_>>(), [89]);
        assert_eq!(b.find(&id).unwrap().waiting(), 1);
        prs[0].state = "merged".into();
        let ch = b.sync_merges(&prs, 9 * MIN);
        assert_eq!(ch.closed.iter().map(|d| (d.id.as_str(), d.state)).collect::<Vec<_>>(), [(id.as_str(), State::Answered)]);
        assert_eq!(b.find(&id).unwrap().answer, "merged");
        assert_eq!(b.open.len(), 2);
    }

    #[test]
    fn a_raise_about_what_is_already_in_front_of_the_user_joins_it() {
        let mut b = Book::default();
        let merge = b.sync_merges(&[pr(88, "dev", "t1", "Shop", "passing")], MIN).fresh[0].clone();
        let wait = waits("t2", "api", "Shop", "Please test the APK on your phone.");
        let asks = b.sync_questions(&asked_all(std::slice::from_ref(&wait)), &|_| State::Stopped, MIN).fresh[0].clone();
        let failed = b
            .failed(
                Failure {
                    task: "a3.2".into(),
                    title: "fix the login".into(),
                    space: Some("Shop".into()),
                    who: who("t3"),
                    why: "the tests fail".into(),
                },
                MIN,
            )
            .id;
        let raise = |a: &str, why: &str, text: &str| Raise {
            detail: String::new(),
            space: Some("Shop".into()),
            who: who(a),
            why: why.into(),
            text: text.into(),
            options: vec![],
        };
        // The brain raises the merge the tower already shows: by its number, or as its session's.
        let r = b.raise(raise("t7", "merge", "Merge PR #88 into dev: the checks pass"), 2 * MIN);
        assert_eq!((r.id.as_str(), r.fresh, r.folded), (merge.as_str(), false, true));
        assert_eq!(b.raise(raise("t1", "merge", "Its release PR waits for the merge"), 2 * MIN).id, merge);
        assert_eq!(b.find(&merge).unwrap().sessions.len(), 1, "a merge's sessions are its PRs'");
        // A session that already waits for the user, and a failure already put to them.
        assert_eq!(b.raise(raise("t2", "waiting", "The api session needs the APK tested"), 2 * MIN).id, asks);
        assert_eq!(b.raise(raise("t3", "failed", "The login fix failed again"), 2 * MIN).id, failed);
        assert_eq!(b.open.len(), 3, "no fourth decision");
        // Something else about the same session is its own decision.
        assert!(b.raise(raise("t2", "money", "Turn on the paid SMS provider?"), 2 * MIN).fresh);
        assert!(b.raise(raise("t7", "merge", "Merge PR #99 into dev"), 2 * MIN).fresh, "a PR the tower doesn't show");
        assert_eq!(pr_numbers("Merge PR #88, pr 89 and PR#90; not the spr 7 or #x"), [88, 90, 89]);
    }

    #[test]
    fn a_task_that_failed_twice_is_tried_again_or_dropped() {
        let mut b = Book::default();
        let f = Failure {
            task: "a3.2".into(),
            title: "fix the login".into(),
            space: Some("Shop".into()),
            who: who("t5"),
            why: "it says it couldn't: the tests fail.".into(),
        };
        let r = b.failed(f.clone(), MIN);
        assert!(r.fresh && !b.failed(f, 2 * MIN).fresh, "once per task");
        let d = b.find(&r.id).unwrap().clone();
        assert_eq!((d.kind, d.task.as_deref(), d.why.as_str()), (Kind::Failed, Some("a3.2"), "failed"));
        assert_eq!(d.title, "\"fix the login\" failed after a retry: it says it couldn't: the tests fail");
        assert!(d.pick().is_none());
        assert_eq!(act(&d, &json!({ "choice": 0 }), true).unwrap(), Act::Requeue { task: "a3.2".into(), text: String::new() });
        assert_eq!(act(&d, &json!({ "choice": 1 }), true).unwrap(), Act::Abandon { task: "a3.2".into() });
        assert_eq!(
            act(&d, &json!({ "other": "use the staging database" }), true).unwrap(),
            Act::Requeue { task: "a3.2".into(), text: "use the staging database".into() }
        );
        // Still failed: it stays. Sent again, verified or dropped by someone: it is over.
        assert!(b.sync_failed(&["a3.2".into()], 3 * MIN).closed.is_empty());
        assert_eq!(b.sync_failed(&[], 4 * MIN).closed[0].state, State::Answered);
        assert!(b.open.is_empty());
    }

    #[test]
    fn the_queue_is_heaviest_first_then_oldest() {
        let d = |id: &str, first_ms: u64, waiting: usize, working: usize| Decision {
            id: id.into(),
            first_ms,
            sessions: (0..waiting + working)
                .map(|i| Who { address: format!("{id}-{i}"), waiting: i < waiting, ..Default::default() })
                .collect(),
            ..Default::default()
        };
        let now = 100 * MIN;
        let b = Book {
            open: vec![
                d("one-short", now - 2 * MIN, 1, 0),   // 2
                d("three-short", now - 2 * MIN, 3, 0), // 6
                d("one-long", now - 10 * MIN, 1, 0),   // 10
                d("two-mid", now - 5 * MIN, 2, 0),     // 10, and newer than one-long
                d("busy-old", now - 90 * MIN, 0, 2),   // nobody waits: 0
                d("busy-new", now - MIN, 0, 1),        // 0, and newer
            ],
            ..Default::default()
        };
        let order: Vec<&str> = b.ordered(now).iter().map(|d| d.id.as_str()).collect();
        assert_eq!(order, ["one-long", "two-mid", "three-short", "one-short", "busy-old", "busy-new"]);
        assert_eq!((b.open[1].weight(now), b.open[4].weight(now)), (3 * 2 * MIN, 0));
        // Later the three sessions outweigh the one that has waited longer.
        let order: Vec<&str> = b.ordered(now + 10 * MIN).iter().map(|d| d.id.as_str()).take(2).collect();
        assert_eq!(order, ["three-short", "two-mid"]);
    }

    #[test]
    fn a_decision_goes_when_its_sessions_are_gone() {
        let mut b = Book::default();
        let raise = |a: &str| Raise {
            detail: String::new(),
            space: Some("Shop".into()),
            who: who(a),
            why: "design".into(),
            text: "Tabs or a sidebar?".into(),
            options: vec![],
        };
        let id = b.raise(raise("t1"), MIN).id;
        b.raise(raise("t2"), MIN);
        // One of two leaves: it stays, with one session less.
        let ch = b.session_gone("t1", 2 * MIN);
        assert!(ch.closed.is_empty());
        assert_eq!(b.find(&id).unwrap().waiting(), 1);
        // The other too: it is over, stopped.
        let ch = b.session_gone("t2", 3 * MIN);
        assert_eq!(ch.closed.iter().map(|d| d.state).collect::<Vec<_>>(), [State::Stopped]);
        assert!(b.open.is_empty());
        // A session nobody knows changes nothing.
        assert!(b.session_gone("t99", 6 * MIN).closed.is_empty());
        // A decision about a session that had ended when it was raised stays until it is answered.
        let orphan = b
            .raise(
                Raise { detail: String::new(), who: None, text: "Delete the old staging database?".into(), why: "delete".into(), ..Default::default() },
                7 * MIN,
            )
            .id;
        assert!(b.session_gone("t1", 8 * MIN).closed.is_empty());
        assert_eq!(b.dismiss(&orphan, 9 * MIN).unwrap().state, State::Dismissed);
        assert!(b.dismiss(&orphan, 9 * MIN).unwrap_err().to_string().contains("dismissed already"));
        assert!(b.dismiss("d000000000000", 9 * MIN).is_err());
    }

    /// A PR that is still open and waiting is still the user's, whoever
    /// opened it: its session ending doesn't take the merge away.
    #[test]
    fn a_merge_outlives_its_sessions_and_goes_with_its_prs_or_its_space() {
        let mut b = Book::default();
        let prs = [pr(88, "dev", "t1", "Shop", "passing"), pr(89, "main", "t2", "Shop", "passing")];
        let merge = b.sync_merges(&prs, MIN).fresh[0].clone();
        assert_eq!(b.find(&merge).unwrap().waiting(), 2);
        // Both sessions end: the decision stays, with both PRs, and nobody waiting on it.
        assert!(b.session_gone("t1", 2 * MIN).closed.is_empty());
        assert!(b.session_gone("t2", 3 * MIN).closed.is_empty());
        let d = b.find(&merge).unwrap();
        assert_eq!((d.state, d.waiting(), d.prs.len(), d.closed_ms), (State::Open, 0, 2, None));
        assert_eq!(d.title, "Merge PR #88 into dev, then PR #89 into main (o/app)");
        assert!(b.settled_prs.is_empty());
        // The PRs as they are, read again: still that decision, and its gone sessions don't wait again.
        assert!(b.sync_merges(&prs, 4 * MIN).fresh.is_empty());
        assert_eq!((b.open.len(), b.find(&merge).unwrap().waiting()), (1, 0));
        // One merges: it changes in place. The other closes on GitHub: the decision is over.
        let one = [Pr { state: "merged".into(), ..prs[0].clone() }, prs[1].clone()];
        assert!(b.sync_merges(&one, 5 * MIN).closed.is_empty());
        assert_eq!(b.find(&merge).unwrap().prs.iter().map(|p| p.number).collect::<Vec<_>>(), [89]);
        let none = [one[0].clone(), Pr { state: "closed".into(), ..prs[1].clone() }];
        let ch = b.sync_merges(&none, 6 * MIN);
        assert_eq!(ch.closed.iter().map(|d| (d.id.as_str(), d.state)).collect::<Vec<_>>(), [(merge.as_str(), State::Stopped)]);

        // A space with no session left: what was raised there is over, its merge stays.
        let mut b = Book::default();
        let merge = b.sync_merges(&prs, MIN).fresh[0].clone();
        let raised = b.raise(Raise { detail: String::new(), space: Some("Shop".into()), who: None, why: "design".into(), text: "Tabs?".into(), options: vec![] }, MIN).id;
        let other = b.raise(Raise { detail: String::new(), space: Some("Docs".into()), who: None, why: "design".into(), text: "Tabs?".into(), options: vec![] }, MIN).id;
        let ch = b.space_over("shop", false, 2 * MIN);
        assert_eq!(ch.closed.iter().map(|d| (d.id.as_str(), d.state)).collect::<Vec<_>>(), [(raised.as_str(), State::Stopped)]);
        assert!(b.find(&merge).unwrap().closed_ms.is_none() && b.find(&other).unwrap().closed_ms.is_none());
        // The space is closed (or archived): the merge goes too, and its PRs make no decision again while they wait.
        let ch = b.space_over("Shop", true, 3 * MIN);
        assert_eq!(ch.closed.iter().map(|d| (d.id.as_str(), d.state)).collect::<Vec<_>>(), [(merge.as_str(), State::Stopped)]);
        assert_eq!(b.settled_prs.len(), 2);
        assert!(b.sync_merges(&prs, 4 * MIN).fresh.is_empty());
        assert_eq!(b.open.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), [other.as_str()], "another space's is left alone");
        // Once they no longer wait they are forgotten.
        b.sync_merges(&[], 5 * MIN);
        assert!(b.settled_prs.is_empty());
        assert!(b.space_over("Shop", true, 6 * MIN).closed.is_empty());
    }

    #[test]
    fn a_dismissed_merge_stays_away_while_its_prs_wait() {
        let mut b = Book::default();
        let prs = [pr(88, "dev", "t1", "Shop", "passing")];
        let id = b.sync_merges(&prs, MIN).fresh[0].clone();
        assert_eq!(b.dismiss(&id, 2 * MIN).unwrap().state, State::Dismissed);
        assert!(b.sync_merges(&prs, 3 * MIN).fresh.is_empty() && b.open.is_empty());
        // Another PR of the repo is a new decision, without the dismissed one.
        let more = [prs[0].clone(), pr(90, "dev", "t1", "Shop", "passing")];
        assert_eq!(b.sync_merges(&more, 4 * MIN).fresh.len(), 1);
        assert_eq!(b.open[0].prs.iter().map(|p| p.number).collect::<Vec<_>>(), [90]);
    }

    #[test]
    fn the_book_is_kept_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("tower-decisions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("decisions.json");
        assert_eq!(Book::load(&path), Book::default(), "no file yet");
        let mut b = Book::default();
        let options = options_from(&json!([{ "label": "Tabs", "why": "less to build" }, { "label": "A sidebar" }])).unwrap();
        let id = b
            .raise(
                Raise { detail: String::new(), space: Some("Shop".into()), who: who("t1"), why: "design".into(), text: "Tabs or a sidebar?".into(), options },
                MIN,
            )
            .id;
        b.claim_told(&id);
        b.sync_merges(&[pr(88, "dev", "t2", "Shop", "passing")], 2 * MIN);
        let gone = b.raise(Raise { detail: String::new(), who: who("t3"), text: "Something else".into(), ..Default::default() }, 3 * MIN).id;
        b.dismiss(&gone, 4 * MIN).unwrap();
        b.settled_prs.insert("o/app#7".into(), State::Dismissed);
        b.save(&path).unwrap();
        let back = Book::load(&path);
        assert_eq!(back, b);
        assert_eq!(back.find(&id).map(|d| d.told), Some(true), "told before the restart: not told again");
        assert_eq!(back.find(&gone).map(|d| d.state), Some(State::Dismissed));
        // A file that doesn't parse is kept aside, never lost.
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Book::load(&path), Book::default());
        assert!(std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("bad-")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_user_is_told_once_per_decision() {
        let mut b = Book::default();
        let id = b.raise(Raise { detail: String::new(), who: who("t1"), text: "Deploy?".into(), ..Default::default() }, MIN).id;
        assert_eq!(b.claim_told(&id), Some(true));
        assert_eq!(b.claim_told(&id), Some(false));
        assert_eq!(b.claim_told("d000000000000"), None);
    }

    #[test]
    fn a_session_blocked_on_what_the_tower_cant_read_is_a_decision_until_it_goes_on() {
        let mut b = Book::default();
        let who = || Who::new("t4", "api");
        let first = b.blocked(Some("Shop".into()), who(), "Asks to use Frobnicate: a dialog the tower can't parse", MIN);
        assert!(first.fresh && first.waiting == 1);
        let d = b.find(&first.id).unwrap().clone();
        assert_eq!((d.kind, d.why.as_str(), d.state, d.space.as_deref()), (Kind::Blocked, "blocked", State::Open, Some("Shop")));
        assert_eq!(b.ordered(2 * MIN).len(), 1, "it stands in the queue, so it counts on the orb");
        // Seen again (a new status line): the same decision, its line brought up to date.
        let again = b.blocked(Some("Shop".into()), who(), "Asks to use Frobnicate: step 2", 2 * MIN);
        assert_eq!((again.id.as_str(), again.fresh, b.open.len()), (first.id.as_str(), false, 1));
        assert_eq!(b.find(&first.id).unwrap().title, "Asks to use Frobnicate: step 2");
        // Another session blocked the same way is its own card: each has to be looked at in its own terminal.
        let other = b.blocked(Some("Shop".into()), Who::new("t5", "web"), "Asks to use Frobnicate: step 2", 2 * MIN);
        assert!(other.fresh && other.id != first.id);
        // `tower raise` about it joins it.
        let r = b.raise(Raise { detail: String::new(), space: Some("Shop".into()), who: Some(who()), why: "blocked".into(), text: "api is blocked on a dialog".into(), options: vec![] }, 3 * MIN);
        assert_eq!((r.id.as_str(), r.fresh), (first.id.as_str(), false));
        // Nothing typed reaches it; no rule is made of it.
        assert!(act(&d, &json!({ "other": "yes" }), true).unwrap_err().to_string().contains("answered in the session itself"));
        assert!(rule_for(Kind::Blocked, "blocked", &d.title, Some("Shop"), "yes").is_none());
        // Still blocked: it stays. No longer blocked: over, answered in its own terminal.
        assert!(b.sync_blocked("t4", true, 4 * MIN).closed.is_empty());
        let ch = b.sync_blocked("t4", false, 5 * MIN);
        assert_eq!((ch.closed.len(), ch.closed[0].state, ch.closed[0].answer.as_str()), (1, State::Answered, "in its own terminal"));
        // The other one's session ends: over, stopped.
        let ch = b.session_gone("t5", 6 * MIN);
        assert_eq!((ch.closed.len(), ch.closed[0].state, b.open.len()), (1, State::Stopped, 0));
    }

    /// A question set answered in its session's own terminal: that session
    /// leaves its decision as answered; the decision ends with the last one.
    #[test]
    fn a_set_answered_in_its_terminal_leaves_its_decision_as_answered() {
        let mut b = Book::default();
        let sets = vec![waits("t1", "web", "Shop", "merge PR #12"), waits("t2", "api", "Shop", "merge PR #12")];
        b.sync_questions(&asked_all(&sets), &|_| State::Stopped, MIN);
        let id = b.open[0].id.clone();
        assert_eq!(b.open[0].sessions.len(), 2);
        assert_eq!(b.answered_in_terminal(&sets[0].id, 2 * MIN), [(id.clone(), false)]);
        assert_eq!(b.open[0].sessions.iter().map(|w| w.address.as_str()).collect::<Vec<_>>(), ["t2"], "the other still waits on its own");
        assert!(b.answered_in_terminal("q000000000000", 2 * MIN).is_empty(), "a set no decision stands for");
        assert_eq!(b.answered_in_terminal(&sets[1].id, 3 * MIN), [(id.clone(), true)]);
        let d = b.find(&id).unwrap();
        assert_eq!((d.state, d.closed_ms, d.answer.as_str()), (State::Answered, Some(3 * MIN), ""), "what it was answered with comes from its screen or its words");
        // The next sync, without them: nothing more happens to it.
        assert!(b.sync_questions(&[], &|_| State::Stopped, 4 * MIN).closed.is_empty());
    }

    /// A question decision's title follows its dialog: the question it is
    /// on, what is open, what was answered in the terminal.
    #[test]
    fn a_questions_title_follows_its_dialog() {
        use crate::questions::Tab;
        let tab = |q: &str| Tab { title: q.into(), question: format!("{q}?"), ..Default::default() };
        let mut set = QuestionSet { id: "q1".into(), kind: "claude-ask".into(), tabs: vec![tab("Database"), tab("Features"), tab("Deploy"), tab("Region")], ..Default::default() };
        assert_eq!(question_title(&set), "Database? (and 3 more)");
        set.tabs[0].answered = true;
        set.tabs[1].answered = true;
        set.current_tab = 2;
        assert_eq!(question_title(&set), "Deploy? (and 1 more; 2 answered in the terminal)");
        set.tabs[2].answered = true;
        set.current_tab = 3;
        assert_eq!(question_title(&set), "Region? (3 answered in the terminal)");
        set.tabs[3].answered = true;
        set.current_tab = 4;
        assert_eq!(question_title(&set), "All 4 answered in the terminal, not submitted yet: Database?");
        let one = QuestionSet { kind: "claude-ask".into(), tabs: vec![tab("Color")], ..Default::default() };
        assert_eq!(question_title(&one), "Color?");
    }

    /// Only submitting (every tab answered in the terminal) goes to the card it was given on, nobody else's dialog.
    #[test]
    fn submit_only_goes_to_its_own_card() {
        let sets = vec![waits("t1", "web", "Shop", "merge PR #12"), waits("t2", "api", "Shop", "merge PR #12")];
        let mut b = Book::default();
        b.sync_questions(&asked_all(&sets), &|_| State::Stopped, MIN);
        let d = b.open[0].clone();
        let (take, _) = takers(&d, &json!({ "answers": [], "shown": sets[1].id }), &[], &sets).unwrap();
        assert_eq!(take.iter().map(|s| s.session.as_str()).collect::<Vec<_>>(), ["t2"]);
        let answers = vec![crate::questions::Answer { tab: 0, choice: json!({"other": "merged"}), attachments: vec![] }];
        let (take, _) = takers(&d, &json!({ "answers": [], "shown": sets[1].id }), &answers, &sets).unwrap();
        assert_eq!(take.len(), 2, "an answer that means the same goes to every session that asks it, as before");
    }

    #[test]
    fn every_account_out_is_one_card_the_tower_answers_itself() {
        let mut b = Book::default();
        let card = |title: &str| crate::failover::Card {
            title: title.into(),
            detail: "default: out until 15:00".into(),
            options: vec![
                ("Go on at 15:00".into(), "".into(), crate::failover::WAIT.into(), true),
                ("Leave them stopped".into(), "".into(), crate::failover::STOP.into(), false),
            ],
        };
        let r = b.accounts_out(Some(card("Every Claude account is out of usage: fix-login waits.")), 1).unwrap();
        assert!(r.fresh);
        // More waiting: the same card, changed in place.
        let again = b.accounts_out(Some(card("Every Claude account is out of usage: 2 sessions wait.")), 2).unwrap();
        assert_eq!((again.id.as_str(), again.fresh), (r.id.as_str(), false));
        let d = b.open.iter().find(|d| d.id == r.id).unwrap().clone();
        assert_eq!((d.kind, d.why.as_str(), d.title.as_str()), (Kind::Choice, ACCOUNTS_WHY, "Every Claude account is out of usage: 2 sessions wait."));
        assert!(matches!(act(&d, &json!({ "choice": 0 }), true).unwrap(), Act::Accounts { wait: true, .. }));
        assert!(matches!(act(&d, &json!({ "choice": 1 }), true).unwrap(), Act::Accounts { wait: false, .. }));
        assert!(matches!(act(&d, &json!({ "other": "leave them, I'll do it" }), true).unwrap(), Act::Accounts { wait: false, .. }));
        assert!(matches!(act(&d, &json!({ "other": "wait for it" }), true).unwrap(), Act::Accounts { wait: true, .. }));
        assert_eq!(rule_for(Kind::Choice, ACCOUNTS_WHY, &d.title, None, "Go on at 15:00"), None, "never made a rule");
        // Nothing waits any more: it stops.
        assert_eq!(b.accounts_out(None, 3), None);
        assert!(!b.open.iter().any(|d| d.key == ACCOUNTS_KEY));
    }
}
