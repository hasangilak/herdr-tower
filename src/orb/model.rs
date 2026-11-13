//! What needs the user and how the spaces stand, as one model for the orb's
//! count, the floating panel and the full window (`docs/quiet-tower.md`,
//! "Step 3" and the shapes under "Step 11").
//!
//! - A **decision** is the one thing shown in the Yours lane; a **mission**
//!   is a space with its goal, its sessions and its switches.
//! - Both are read from the state (`decisions`, `missions`), which the
//!   daemon writes: the decisions in queue order, each with who waits on it,
//!   its options and the tower's pick; the missions with their sessions
//!   (worktree, branch, port, last words), requests and when a ready one
//!   closes by itself. A state without those keys (an older daemon) is
//!   read through `adapt` and `adapt_missions`, which build the same from
//!   the question sets it leaves to the user, the PRs waiting for a merge
//!   (`prs.json`), `spaces.json` and Herdr's spaces.
//! - The queue's order is the daemon's: `weight = sessions waiting × time
//!   waited`, heaviest first, then oldest (`order` does the same for the
//!   adapter). A muted space's decisions stay out of the main queue and the
//!   count.
//! - `Deck` is what the views share about the queue: the card that was
//!   picked, and the one-line receipts of what was just answered.
//!
//! Nothing here draws or does I/O: it is all plain data and functions.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::prs::Pr;
use crate::questions::QuestionSet;
use crate::spaces::{Live, Spaces};

/// How long an answered card's receipt stays, in ms.
pub const RECEIPT_MS: u64 = 2400;
/// How long it stays when its answer can be made a rule (or until the next card is answered).
pub const RULE_MS: u64 = 8000;

/// A field that may be `null` on the wire reads as its default.
fn or_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Decisions.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Permission,
    #[default]
    Question,
    Merge,
    Choice,
    Failed,
    Raised,
    /// A session blocked on what the tower can't read: looked at in Herdr.
    Blocked,
    /// A kind this build doesn't know (a newer tower's): still the user's,
    /// shown as a plain card and counted, never dropped.
    #[serde(other)]
    Other,
}

/// Where a decision is in its life (`state` on the wire).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    #[default]
    Open,
    /// The answer is being typed into the session.
    Answering,
    Answered,
    /// Over without an answer: its sessions are gone, its PRs closed. (An
    /// answer that stopped half way is `Open` again: the card says so from
    /// its question set.)
    Stopped,
    Dismissed,
}

fn yes() -> bool {
    true
}

/// A session a decision is about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionRef {
    pub address: String,
    #[serde(deserialize_with = "or_default")]
    pub name: String,
    /// Blocked on it now (false: it works on something else meanwhile, or it has ended).
    pub waiting: bool,
    /// Its own question set, for a decision that stands for question sets.
    pub question_id: Option<String>,
}

impl Default for SessionRef {
    fn default() -> Self {
        Self { address: String::new(), name: String::new(), waiting: yes(), question_id: None }
    }
}

impl SessionRef {
    pub fn new(address: &str, name: &str) -> Self {
        Self { address: address.into(), name: name.into(), ..Default::default() }
    }

    /// Its name, else its address.
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.address } else { &self.name }
    }
}

/// One answer a decision offers; `pick` marks the tower's own.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Opt {
    pub label: String,
    #[serde(deserialize_with = "or_default")]
    pub why: String,
    pub pick: bool,
}

/// Whether a PR can be merged now and what to mind, as its decision says
/// it (`prs::Gate::view`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeGate {
    pub can: bool,
    pub blocked: Option<String>,
    pub needs_override: Option<String>,
    pub warnings: Vec<String>,
}

/// A pull request of a merge decision. `checks`: `pass`, `pending`, `fail`,
/// or `none` (no checks ran).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrRef {
    /// `owner/name#12`, as the daemon gives it (see `key`).
    #[serde(deserialize_with = "or_default")]
    pub key: String,
    pub repo: String,
    pub number: u64,
    #[serde(deserialize_with = "or_default")]
    pub base: String,
    #[serde(deserialize_with = "or_default")]
    pub title: String,
    #[serde(deserialize_with = "or_default")]
    pub url: String,
    #[serde(deserialize_with = "or_default")]
    pub checks: String,
    /// Its checks in a few words (`1 check failing: lint`).
    #[serde(deserialize_with = "or_default")]
    pub note: String,
    /// The session that opened it.
    #[serde(deserialize_with = "or_default")]
    pub session: String,
    /// None from a daemon that doesn't say.
    pub merge: Option<MergeGate>,
    /// Its review (`crate::reviews`), when it has one.
    pub review: Option<crate::reviews::Brief>,
    /// Whether it needs a human code review, and why (`crate::prscore`).
    pub score: Option<crate::prscore::Score>,
    /// Its plan's checks and edge cases, and where each check stands.
    pub validation: Option<crate::prscore::Validation>,
    /// Who outside depends on what it changes, and whether that breaks.
    pub impact: Option<crate::impact::Impact>,
}

impl PrRef {
    /// Its key in `prs.json` (`owner/name#12`).
    pub fn key(&self) -> String {
        if self.key.is_empty() { crate::prs::key(&self.repo, self.number) } else { self.key.clone() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Decision {
    pub id: String,
    pub key: String,
    pub kind: Kind,
    #[serde(deserialize_with = "or_default")]
    pub space: String,
    pub sessions: Vec<SessionRef>,
    /// How many sessions can't go on until it is answered.
    pub waiting: usize,
    pub title: String,
    /// What it is about, to read before deciding (the command, the plan, the
    /// message that asks; `tower raise --detail`), "" when it has none.
    #[serde(deserialize_with = "or_default")]
    pub detail: String,
    /// Why it is the user's: the reason a question went to them, `merge`,
    /// the category `tower raise` gave, `failed` (see `badge`).
    #[serde(deserialize_with = "or_default")]
    pub why: String,
    pub options: Vec<Opt>,
    /// The label of the option the tower would pick.
    pub pick: Option<String>,
    pub prs: Vec<PrRef>,
    /// The question set behind it, if it came from a session's dialog.
    pub question_id: Option<String>,
    /// A failed decision's agenda task.
    pub task: Option<String>,
    pub first_ms: u64,
    pub last_ms: u64,
    pub count: u64,
    pub state: Stage,
    /// Its space is muted, by the daemon's word (see `muted_spaces`).
    pub muted: bool,
    /// On hold (`crate::decisions::Hold`): the state has it in
    /// `decisions_held`, not in the queue.
    pub hold: Option<HoldOf>,
    /// It was on hold and came back: why.
    pub back: Option<BackOf>,
}

/// A decision's hold as the state carries it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HoldOf {
    /// The user's words (empty: none given).
    pub reason: String,
    /// Who held it: `mac`, `phone:<device>`, a brain's name, `cli`.
    pub by: String,
    pub since_ms: u64,
    /// The PRs it waits for: one merged, it comes back.
    pub until_prs: Vec<String>,
}

/// Why a decision came back from hold.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackOf {
    pub why: String,
    pub at_ms: u64,
}

impl HoldOf {
    /// Who held it, in words: "you" (here, the phone, a command), or the tower that heard you say so.
    pub fn who(&self) -> String {
        match self.by.as_str() {
            b if b.starts_with("tower-") => format!("the {} tower, from what you said", &b["tower-".len()..]),
            b if b.starts_with("tower") => "the tower, from what you said".into(),
            b if b.starts_with("phone") => "you, on the phone".into(),
            _ => "you".into(),
        }
    }
}

/// The open decisions on hold, newest hold first (the state's `decisions_held`).
pub fn held(state: &Value) -> Vec<Decision> {
    state["decisions_held"].as_array().map(|a| a.iter().filter_map(|v| serde_json::from_value::<Decision>(v.clone()).ok()).filter(|d| !d.id.is_empty()).collect()).unwrap_or_default()
}

impl Decision {
    /// Still in the queue: a card, or the line of an answer going in.
    pub fn live(&self) -> bool {
        matches!(self.state, Stage::Open | Stage::Answering)
    }

    /// The user's to act on now (an answer going in is not).
    pub fn yours(&self) -> bool {
        self.state == Stage::Open
    }

    /// In `space` (any case, as the daemon matches a space's name).
    pub fn in_space(&self, space: &str) -> bool {
        !self.space.is_empty() && self.space.eq_ignore_ascii_case(space.trim())
    }

    /// The session whose terminal its card names: the first that waits, else the first.
    pub fn lead(&self) -> Option<&SessionRef> {
        self.sessions.iter().find(|s| s.waiting).or(self.sessions.first())
    }

    /// Whole minutes since it first appeared.
    pub fn minutes(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.first_ms) / 60_000
    }

    /// What orders the queue: sessions waiting × how long (in ms, so two
    /// decisions in their first minute are still ordered), as the daemon has it.
    pub fn weight(&self, now_ms: u64) -> u64 {
        self.waiting as u64 * now_ms.saturating_sub(self.first_ms)
    }

    /// The word on its card for why it is the user's (`Merge`, `Production
    /// data`, `Design`): the daemon's `why` when it is a word or two, else
    /// what kind of decision it is.
    pub fn badge(&self) -> String {
        let why = self.why.trim().trim_end_matches('.');
        let kind = match self.kind {
            Kind::Merge => "Merge",
            Kind::Failed => "Failed twice",
            Kind::Permission => "Permission",
            Kind::Question => "Question",
            Kind::Blocked => "In its terminal",
            Kind::Choice | Kind::Raised | Kind::Other => "Decision",
        };
        match self.kind {
            Kind::Merge | Kind::Failed | Kind::Blocked => kind.into(),
            _ if why == crate::questions::WAITS => "Waits for you".into(),
            _ if why.is_empty() || why.chars().count() > 24 => kind.into(),
            _ => {
                let mut c = why.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        }
    }

    /// The reason in full, when `badge` was too short for it.
    pub fn reason(&self) -> Option<String> {
        let why = self.why.trim();
        (!matches!(self.kind, Kind::Merge | Kind::Failed | Kind::Blocked) && why != crate::questions::WAITS && why.chars().count() > 24).then(|| why.to_string())
    }

    /// `3 sessions wait`, `1 session waits`, `Nothing is blocked`.
    pub fn stake(&self) -> String {
        match (self.kind, self.waiting) {
            (_, 0) => "Nothing is blocked".into(),
            (Kind::Failed, 1) => "1 session stopped".into(),
            (Kind::Failed, n) => format!("{n} sessions stopped"),
            (_, 1) => "1 session waits".into(),
            (_, n) => format!("{n} sessions wait"),
        }
    }

    /// `just now`, `4 min`, `3 h`.
    pub fn waited(&self, now_ms: u64) -> String {
        match self.minutes(now_ms) {
            0 => "just now".into(),
            m @ 1..120 => format!("{m} min"),
            m => format!("{} h", m / 60),
        }
    }
}

/// The state's own `decisions`, if the daemon writes them, in its order
/// (entries that can't be read are left out).
pub fn from_state(state: &Value) -> Option<Vec<Decision>> {
    let list = state.get("decisions")?.as_array()?;
    Some(list.iter().filter_map(|v| serde_json::from_value::<Decision>(v.clone()).ok()).filter(|d| !d.id.is_empty()).collect())
}

/// The question sets the tower leaves to the user, oldest first.
pub fn escalated(state: &Value) -> Vec<QuestionSet> {
    let mut sets: Vec<QuestionSet> = state["questions"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| serde_json::from_value::<QuestionSet>(v.clone()).ok()).collect())
        .unwrap_or_default();
    sets.retain(|s| s.decision.as_deref() == Some("escalate") && !s.tabs.is_empty());
    sets.sort_by_key(|s| s.at_ms);
    sets
}

/// A question's text with case, punctuation and spacing set aside, so the
/// same question asked again has the same key.
pub fn normalise(text: &str) -> String {
    let words: String = text.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' }).collect();
    words.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a kind of dialog is called on its card.
fn why_of(kind: &str, tabs: usize) -> &'static str {
    match kind {
        "claude-permission" => "Permission",
        "claude-plan" => "Plan",
        "codex-menu" => "Approval",
        "text" => "Message",
        _ if tabs > 1 => "Questions",
        _ => "Question",
    }
}

/// A PR's checks as a merge decision carries them.
fn checks_word(pr: &Pr) -> &'static str {
    match pr.checks.state.as_str() {
        "failing" => "fail",
        "pending" => "pending",
        "passing" => "pass",
        _ => "none",
    }
}

/// `2 pull requests ready to merge`.
pub fn merge_title(n: usize) -> String {
    if n == 1 { "1 pull request ready to merge".into() } else { format!("{n} pull requests ready to merge") }
}

/// What the one button of a merge card says.
pub fn merge_label(prs: &[PrRef]) -> String {
    let staged = prs.iter().any(|p| !crate::prs::is_prod(&p.base)) && prs.iter().any(|p| crate::prs::is_prod(&p.base));
    match prs {
        [one] => format!("Merge #{}", one.number),
        [_, _] if staged => "Merge both, dev first".into(),
        [_, _] => "Merge both".into(),
        _ if staged => format!("Merge all {}, dev first", prs.len()),
        _ => format!("Merge all {}", prs.len()),
    }
}

/// Today's data as decisions (not yet in queue order):
///
/// - one per question set the tower leaves to the user, its session the one
///   that waits, its options the tower's recommendations for a single
///   question (the first is its pick);
/// - one per repo and space for the open PRs that wait for the user's merge,
///   listing them dev before main, then by number. Its sessions wait only if
///   the state says they are blocked.
///
/// `space_of` finds a session's space (empty when it isn't known).
pub fn adapt(sets: &[QuestionSet], prs: &BTreeMap<String, Pr>, agents: &Value, space_of: &dyn Fn(&str) -> String) -> Vec<Decision> {
    let mut out: Vec<Decision> = vec![];
    for set in sets {
        let space = space_of(&set.session);
        let title =
            if set.title.trim().is_empty() { set.tabs.first().map(|t| t.question.clone()).unwrap_or_default() } else { set.title.clone() };
        let options = match (set.tabs.len(), set.recommendations.as_ref().and_then(|r| r.iter().find(|r| r.tab == 0))) {
            (1, Some(recs)) => recs
                .options
                .iter()
                .take(3)
                .enumerate()
                .map(|(i, r)| Opt { label: r.label.clone(), why: r.why.clone(), pick: i == 0 })
                .collect(),
            _ => vec![],
        };
        out.push(Decision {
            id: set.id.clone(),
            key: format!("{space}|{}", normalise(&title)),
            kind: if matches!(set.kind.as_str(), "claude-permission" | "codex-menu") { Kind::Permission } else { Kind::Question },
            space,
            sessions: vec![SessionRef { question_id: Some(set.id.clone()), ..SessionRef::new(&set.session, &set.name) }],
            waiting: 1,
            title,
            detail: crate::decisions::detail_of(set),
            why: why_of(&set.kind, set.tabs.len()).into(),
            pick: options.first().map(|o: &Opt| o.label.clone()),
            options,
            prs: vec![],
            question_id: Some(set.id.clone()),
            task: None,
            first_ms: set.at_ms,
            last_ms: set.at_ms,
            count: 1,
            // An answer that stopped half way is the user's again.
            state: match set.status.as_str() {
                "answering" | "queued" => Stage::Answering,
                "answered" => Stage::Answered,
                _ => Stage::Open,
            },
            muted: false,
            hold: None,
            back: None,
        });
    }

    let blocked: HashSet<&str> =
        agents.as_array().into_iter().flatten().filter(|a| a["status"] == "blocked").filter_map(|a| a["address"].as_str()).collect();
    let mut groups: BTreeMap<(String, String), Vec<&Pr>> = BTreeMap::new();
    for pr in prs.values().filter(|p| p.open() && p.waiting) {
        groups.entry((pr.repo.clone(), pr.space.clone().unwrap_or_default())).or_default().push(pr);
    }
    for ((repo, space), mut list) in groups {
        list.sort_by_key(|p| (crate::prs::is_prod(&p.base), p.number));
        let mut sessions: Vec<SessionRef> = vec![];
        for p in &list {
            if !p.session.is_empty() && !sessions.iter().any(|s| s.address == p.session) {
                sessions.push(SessionRef { waiting: blocked.contains(p.session.as_str()), ..SessionRef::new(&p.session, &p.name) });
            }
        }
        let refs: Vec<PrRef> = list
            .iter()
            .map(|p| PrRef {
                key: p.key(),
                repo: p.repo.clone(),
                number: p.number,
                base: p.base.clone(),
                title: p.title.clone(),
                url: p.url.clone(),
                checks: checks_word(p).into(),
                note: p.checks_line(),
                session: p.session.clone(),
                merge: None,
                review: p.review.clone(),
                score: p.score.clone(),
                validation: p.validation.clone(),
                impact: p.impact.clone(),
            })
            .collect();
        let key = format!("merge:{repo}@{space}");
        out.push(Decision {
            id: key.clone(),
            key,
            kind: Kind::Merge,
            space,
            waiting: sessions.iter().filter(|s| s.waiting).count(),
            sessions,
            title: merge_title(refs.len()),
            detail: String::new(),
            why: "Merge".into(),
            pick: Some(merge_label(&refs)),
            options: vec![Opt { label: merge_label(&refs), why: String::new(), pick: true }],
            prs: refs,
            question_id: None,
            task: None,
            first_ms: list.iter().map(|p| p.found_ms).min().unwrap_or(0),
            last_ms: list.iter().map(|p| p.found_ms).max().unwrap_or(0),
            count: 1,
            state: Stage::Open,
            muted: false,
            hold: None,
            back: None,
        });
    }
    out
}

/// Every decision, in queue order: the state's own as the daemon ordered
/// them, else built from an older daemon's data and ordered here.
pub fn decisions(state: &Value, prs: &BTreeMap<String, Pr>, space_of: &dyn Fn(&str) -> String, now_ms: u64) -> Vec<Decision> {
    if let Some(list) = from_state(state) {
        return list;
    }
    let mut list = adapt(&escalated(state), prs, &state["agents"], space_of);
    order(&mut list, now_ms);
    list
}

/// The queue's order: heaviest first, then oldest.
pub fn order(list: &mut [Decision], now_ms: u64) {
    list.sort_by(|a, b| b.weight(now_ms).cmp(&a.weight(now_ms)).then(a.first_ms.cmp(&b.first_ms)).then(a.id.cmp(&b.id)));
}

/// The main queue: what is still live, outside muted spaces, in order.
pub fn main_queue(all: &[Decision], muted: &HashSet<String>) -> Vec<Decision> {
    all.iter().filter(|d| d.live() && !muted.iter().any(|m| d.in_space(m))).cloned().collect()
}

/// How many decisions of a queue are the user's now: the orb's number.
pub fn yours(queue: &[Decision]) -> usize {
    queue.iter().filter(|d| d.yours()).count()
}

/// The card shown in full: the picked one if it is still the user's, else
/// the first that is.
pub fn open_card<'a>(queue: &'a [Decision], focus: Option<&str>) -> Option<&'a Decision> {
    let mut mine = queue.iter().filter(|d| d.yours());
    let first = mine.next()?;
    focus.and_then(|f| queue.iter().find(|d| d.yours() && d.id == f)).or(Some(first))
}

/// What the open card is tagged: `Next` when it leads the queue, `Picked`
/// when the user chose one from behind it.
pub fn tag(queue: &[Decision], open: &Decision) -> &'static str {
    if queue.iter().find(|d| d.yours()).is_some_and(|first| first.id == open.id) { "Next" } else { "Picked" }
}

/// `3 need you`, `1 needs you`; `None` when nothing does.
pub fn needs(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("1 needs you".into()),
        n => Some(format!("{n} need you")),
    }
}

/// The status line in its two parts: what is the user's (drawn in amber,
/// absent when nothing is) and the rest. `3 need you` + `· 2 working`, or
/// only `Nothing needs you · 2 working`.
pub fn status_line(yours: usize, working: usize) -> (Option<String>, String) {
    match needs(yours) {
        Some(n) => (Some(n), format!("· {working} working")),
        None => (None, format!("Nothing needs you · {working} working")),
    }
}

// ---------------------------------------------------------------------------
// Missions.

/// A mission's `state` on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Open,
    /// No live session.
    Quiet,
    /// Its goal is met: ready to close.
    Ready,
    Closed,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Member {
    pub address: String,
    #[serde(deserialize_with = "or_default")]
    pub name: String,
    #[serde(deserialize_with = "or_default")]
    pub status: String,
    pub account: Option<String>,
    /// The worktree the tower gave it (a path), and its branch.
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// Its dev port.
    pub port: Option<u16>,
    /// Its last words, as its hooks heard them (cut by the daemon).
    pub said: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Request {
    /// A number or a string, as the daemon has it.
    pub id: Value,
    pub title: String,
    #[serde(deserialize_with = "or_default")]
    pub state: String,
    pub done: u64,
    pub total: u64,
}

/// A space's own tower (its brain, `tower-<space>`), as the daemon says it stands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnTower {
    pub name: String,
    /// `ready`, `working`, `starting`, `blocked` (it waits on its own
    /// screen), or `off` (the space is due one that doesn't run).
    #[serde(deserialize_with = "or_default")]
    pub status: String,
}

impl OwnTower {
    /// It runs: what is asked in its space reaches it (or will, once it is up).
    pub fn runs(&self) -> bool {
        !self.status.is_empty() && self.status != "off"
    }
}

/// One version of a space's goal (`missions::GoalChange`): what it became,
/// when, and what made it so (`by`: `you`, `request`, `launch`, `tower`, "").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalStep {
    #[serde(deserialize_with = "or_default")]
    pub goal: String,
    #[serde(deserialize_with = "or_default")]
    pub done_when: String,
    pub at_ms: u64,
    #[serde(deserialize_with = "or_default")]
    pub by: String,
    /// For `request`: the user's words that changed it.
    #[serde(deserialize_with = "or_default")]
    pub request: String,
    /// For `request`: what it added, in a phrase.
    #[serde(deserialize_with = "or_default")]
    pub why: String,
}

impl GoalStep {
    /// Who set it, for the Goal card: "by your request", "by you", "at launch", "by the tower".
    pub fn by_words(&self) -> &'static str {
        match self.by.as_str() {
            "request" => "updated by your request",
            "you" => "set by you",
            "launch" => "set at launch",
            "tower" => "set by the tower",
            _ => "set",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mission {
    pub name: String,
    #[serde(deserialize_with = "or_default")]
    pub goal: String,
    #[serde(deserialize_with = "or_default")]
    pub done_when: String,
    /// How the goal came to be, oldest first: the last is the goal now
    /// (empty from a tower that doesn't keep it, or a goal set before it did).
    #[serde(deserialize_with = "or_default")]
    pub goal_history: Vec<GoalStep>,
    pub standing: bool,
    /// Its PRs get a reviewer (`crate::reviews`): on unless the user turned
    /// it off, and on from a daemon that doesn't say.
    #[serde(default = "yes")]
    pub review: bool,
    pub state: Phase,
    pub follow: bool,
    pub mute: bool,
    /// Decisions in it that are the user's (set by `Missions::settle`).
    pub needs: usize,
    /// A session of it is at work (set by `Missions::settle`).
    #[serde(skip)]
    pub at_work: bool,
    #[serde(deserialize_with = "or_default")]
    pub report: String,
    pub sessions: Vec<Member>,
    pub requests: Vec<Request>,
    pub thread_count: usize,
    /// Its own tower, when it has one (never one of its `sessions`).
    pub tower: Option<OwnTower>,
    /// Since when it has been quiet, or ready to close.
    pub quiet_since_ms: Option<u64>,
    pub ready_since_ms: Option<u64>,
    /// When a ready space closes by itself (none: it doesn't, or not yet).
    pub auto_close_ms: Option<u64>,
}

impl Request {
    /// Nothing of it is left to do: done, closed or dropped, or every task of it is.
    pub fn finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "closed" | "abandoned") || (self.total > 0 && self.done >= self.total)
    }

    /// How far along it is: `done`, `dropped`, `5 of 6` (its tasks), or its
    /// state's own word while it has no task yet.
    pub fn progress(&self) -> String {
        if self.state == "abandoned" {
            "dropped".into()
        } else if self.finished() {
            "done".into()
        } else if self.total > 0 {
            format!("{} of {}", self.done, self.total)
        } else {
            self.state.clone()
        }
    }
}

/// A tower's own terminal (`tower`, or a space's `tower-…`): not a session of the work.
pub fn is_tower(name: &str) -> bool {
    name == "tower" || name.starts_with("tower-")
}

impl Mission {
    /// How its chip (and its row in the sidebar) is marked.
    pub fn chip(&self) -> Chip {
        match (self.needs, self.mute) {
            (0, _) if self.at_work => Chip::Working,
            (0, _) => Chip::Done,
            (n, true) => Chip::Muted(n),
            (n, false) => Chip::Yours(n),
        }
    }

    /// Its goal is met: the state says so, or its closing report is in.
    pub fn ready(&self) -> bool {
        self.state == Phase::Ready || !self.report.trim().is_empty()
    }

    /// The sessions that do its work (not its tower's own terminal).
    pub fn workers(&self) -> Vec<&Member> {
        self.sessions.iter().filter(|s| !is_tower(&s.name)).collect()
    }

    /// A tower of its own runs for it (the daemon says so: `tower`).
    pub fn own_tower(&self) -> bool {
        self.tower.as_ref().is_some_and(OwnTower::runs)
    }

    /// What its head says of its own tower: nothing when it has none (the
    /// main tower has it), and how it stands when it isn't simply there.
    pub fn tower_line(&self) -> Option<&'static str> {
        if !self.own_tower() {
            return None;
        }
        match self.tower.as_ref()?.status.as_str() {
            "starting" => Some("its own tower is starting"),
            "blocked" => Some("its own tower waits on its own screen"),
            _ => Some("its own tower"),
        }
    }

    /// `3 sessions on orca`, `1 session`: the account only when every
    /// session is known to run on the same one.
    pub fn sub(&self) -> String {
        let workers = self.workers();
        let n = workers.len();
        let count = if n == 1 { "1 session".to_string() } else { format!("{n} sessions") };
        let first = workers.first().and_then(|s| s.account.as_deref());
        match first {
            Some(account) if workers.iter().all(|s| s.account.as_deref() == Some(account)) => format!("{count} on {account}"),
            _ => count,
        }
    }

    /// The small line under its name in the sidebar.
    pub fn side_line(&self) -> String {
        if self.ready() {
            "ready to close".into()
        } else if self.standing {
            format!("standing · {}", self.sub())
        } else {
            self.sub()
        }
    }

    /// The request it is on: the first with something left, else the last.
    pub fn current(&self) -> Option<&Request> {
        self.requests.iter().find(|r| !r.finished()).or(self.requests.last())
    }

    /// How far along it is, for the missions list, and whether that is
    /// "ready to close": else its current request's progress, else `working`
    /// while a session of it works (nothing when nothing says).
    pub fn progress(&self) -> (String, bool) {
        if self.ready() {
            return ("ready to close".into(), true);
        }
        match self.current() {
            Some(r) => (r.progress(), false),
            None if self.at_work => ("working".into(), false),
            None => (String::new(), false),
        }
    }

    /// What it is about, in a line: its goal; a standing space, or one with
    /// no goal, what it is asked now.
    pub fn about(&self) -> String {
        let goal = self.goal.trim();
        if !self.standing && !goal.is_empty() {
            return goal.to_string();
        }
        match self.current() {
            Some(r) => r.title.clone(),
            None => goal.to_string(),
        }
    }

    /// When it closes by itself, said from `now_ms`: `in 5 h`, `in 20 min`,
    /// `any moment`. None when it doesn't (not ready, a session isn't idle,
    /// or the tower is told never to).
    pub fn closes_in(&self, now_ms: u64) -> Option<String> {
        let left = self.auto_close_ms?.saturating_sub(now_ms) / 60_000;
        Some(match left {
            0 => "any moment".into(),
            m @ 1..90 => format!("in {m} min"),
            m => format!("in {} h", (m + 30) / 60),
        })
    }

    /// `2 sessions · own worktrees · 33 messages between them`: the
    /// worktrees only when its sessions have them, the messages when any.
    pub fn meta(&self, messages: usize) -> String {
        let workers = self.workers();
        let mut parts = vec![if workers.len() == 1 { "1 session".to_string() } else { format!("{} sessions", workers.len()) }];
        if workers.iter().any(|s| s.worktree.as_deref().is_some_and(|w| !w.is_empty())) {
            parts.push("own worktrees".into());
        }
        match messages.max(self.thread_count) {
            0 => {}
            1 => parts.push("1 message between them".into()),
            n => parts.push(format!("{n} messages between them")),
        }
        parts.join(" · ")
    }
}

/// A worktree's path as a row has room for: from `.tower-worktrees/` on
/// (`sandbox/eyzflow`), else its last two folders.
pub fn short_worktree(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if let Some((_, tail)) = path.rsplit_once(".tower-worktrees/") {
        return tail.to_string();
    }
    let mut parts: Vec<&str> = path.rsplit('/').filter(|p| !p.is_empty()).take(2).collect();
    parts.reverse();
    parts.join("/")
}

impl Member {
    /// `worktree and branch sandbox/eyzflow · port 3010` (the tower names a
    /// worktree's branch after it), `worktree shop · branch fix/login · port
    /// 3010`: as much of it as is known.
    pub fn place(&self) -> String {
        let mut parts = vec![];
        let branch = self.branch.as_deref().map(str::trim).filter(|b| !b.is_empty());
        match self.worktree.as_deref().map(short_worktree).filter(|w| !w.is_empty()) {
            Some(w) if branch == Some(w.as_str()) => parts.push(format!("worktree and branch {w}")),
            Some(w) => {
                parts.push(format!("worktree {w}"));
                parts.extend(branch.map(|b| format!("branch {b}")));
            }
            None => parts.extend(branch.map(|b| format!("branch {b}"))),
        }
        if let Some(p) = self.port {
            parts.push(format!("port {p}"));
        }
        parts.join(" · ")
    }

    /// Its last words, in one line.
    pub fn last_words(&self) -> Option<String> {
        self.said.as_deref().and_then(|s| s.lines().map(str::trim).find(|l| !l.is_empty())).map(String::from)
    }
}

/// A space's switch as the user just set it here, until the state says the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Marks {
    pub follow: Option<bool>,
    pub mute: Option<bool>,
    pub review: Option<bool>,
}

/// A space's mark: an amber count, the same as an outline when it is muted,
/// a pulse while it works, a tick when it rests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Chip {
    Yours(usize),
    Muted(usize),
    Working,
    Done,
}

/// The spaces: the open ones as missions, and the names of the rest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Missions {
    pub open: Vec<Mission>,
    pub quiet: Vec<String>,
    pub closed_today: Vec<String>,
    /// The quiet ones as the daemon describes them (their goal, since when).
    pub resting: Vec<Mission>,
}

impl Missions {
    /// The spaces whose decisions stay out of the main queue.
    pub fn muted(&self) -> HashSet<String> {
        self.open.iter().filter(|m| m.mute).map(|m| m.name.clone()).collect()
    }

    /// Follow and mute as they stand: a space followed in `spaces.json`
    /// (`follows`, any case) is followed, one of `muted` (see
    /// `muted_spaces`) is muted, and a follow the user just switched here
    /// (`marks`) counts before the state says so.
    pub fn mark(&mut self, follows: &[String], muted: &HashSet<String>, marks: &HashMap<String, Marks>) {
        for m in &mut self.open {
            m.follow = m.follow || follows.iter().any(|f| f.eq_ignore_ascii_case(&m.name));
            m.follow = marks.get(&m.name).and_then(|mark| mark.follow).unwrap_or(m.follow);
            m.mute = muted.iter().any(|n| n.eq_ignore_ascii_case(&m.name));
            m.review = marks.get(&m.name).and_then(|mark| mark.review).unwrap_or(m.review);
        }
    }

    /// How many open spaces have a session at work.
    pub fn working(&self) -> usize {
        self.open.iter().filter(|m| m.at_work).count()
    }

    /// Sets what each mission needs and whether it is at work, from the
    /// decisions: a session is at work when it is working, or blocked on
    /// something the tower answers itself (one waiting on a decision of the
    /// user's is not).
    pub fn settle(&mut self, all: &[Decision]) {
        let waiting: HashSet<&str> =
            all.iter().filter(|d| d.yours()).flat_map(|d| d.sessions.iter().filter(|s| s.waiting).map(|s| s.address.as_str())).collect();
        for m in &mut self.open {
            m.needs = all.iter().filter(|d| d.yours() && d.in_space(&m.name)).count();
            m.at_work =
                m.sessions.iter().any(|s| s.status == "working" || (s.status == "blocked" && !waiting.contains(s.address.as_str())));
        }
    }

    /// The open spaces as the chips show them: what needs the user first,
    /// then what works, then what rests (as listed within each).
    pub fn chips(&self) -> Vec<(&Mission, Chip)> {
        let mut out: Vec<(&Mission, Chip)> = self.open.iter().map(|m| (m, m.chip())).collect();
        out.sort_by_key(|(_, c)| match c {
            Chip::Yours(_) => 0,
            Chip::Muted(_) => 1,
            Chip::Working => 2,
            Chip::Done => 3,
        });
        out
    }
}

/// The state's own `missions`, if the daemon writes them: a list of
/// missions, or `{spaces, quiet, closed_today}` as the phone gets it.
pub fn missions_from_state(state: &Value) -> Option<Missions> {
    let v = state.get("missions")?;
    let (list, mut quiet, closed_today) = match v {
        Value::Array(a) => (a.clone(), vec![], vec![]),
        Value::Object(_) => {
            let names =
                |k: &str| -> Vec<String> { v[k].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect() };
            (v["spaces"].as_array().cloned().unwrap_or_default(), names("quiet"), names("closed_today"))
        }
        _ => return None,
    };
    let (mut open, mut resting) = (vec![], vec![]);
    for m in list.into_iter().filter_map(|m| serde_json::from_value::<Mission>(m).ok()).filter(|m| !m.name.is_empty()) {
        match m.state {
            Phase::Open | Phase::Ready => open.push(m),
            Phase::Quiet => {
                if !quiet.contains(&m.name) {
                    quiet.push(m.name.clone());
                }
                resting.push(m);
            }
            Phase::Closed => {}
        }
    }
    Some(Missions { open, quiet, closed_today, resting })
}

/// An older daemon's data as missions: a space of Herdr's with a live
/// session is open (its goal the agenda in `spaces.json`, what else the
/// tower keeps of its mission there, its sessions as Herdr lists them, their
/// accounts from the state's agents); every other space the tower knows
/// (`known`) is quiet, unless it was closed or archived. Follow and mute
/// are not set.
pub fn adapt_missions(spaces: &Spaces, live: &[Live], known: &[String], agents: &Value) -> Missions {
    let account = |addr: &str| -> Option<String> {
        agents.as_array()?.iter().find(|a| a["address"] == addr)?["account"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let mut out = Missions::default();
    for l in live.iter().filter(|l| !l.sessions.is_empty()) {
        let kept = spaces.missions.get(&l.name);
        out.open.push(Mission {
            name: l.name.clone(),
            goal: spaces.agendas.get(&l.name).cloned().unwrap_or_default(),
            done_when: kept.map(|m| m.done_when.clone()).unwrap_or_default(),
            standing: kept.is_some_and(|m| m.standing),
            review: kept.is_none_or(|m| m.review),
            report: kept.map(|m| m.report.clone()).unwrap_or_default(),
            state: if kept.is_some_and(|m| m.state == crate::missions::State::Ready) { Phase::Ready } else { Phase::Open },
            sessions: l
                .sessions
                .iter()
                .map(|(address, name, status)| Member {
                    address: address.clone(),
                    name: name.clone(),
                    status: status.clone(),
                    account: account(address),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
    }
    let over = |name: &String| spaces.missions.get(name).is_some_and(|m| m.state == crate::missions::State::Closed || m.archived_ms.is_some());
    for name in live.iter().map(|l| &l.name).chain(known).chain(spaces.agendas.keys()) {
        if !name.is_empty() && !out.open.iter().any(|m| &m.name == name) && !out.quiet.contains(name) && !over(name) {
            out.quiet.push(name.clone());
        }
    }
    out
}

/// The spaces, with what each needs: the state's own when it has them, else
/// built from today's data.
pub fn missions(state: &Value, spaces: &Spaces, live: &[Live], known: &[String], all: &[Decision]) -> Missions {
    let mut m = missions_from_state(state).unwrap_or_else(|| adapt_missions(spaces, live, known, &state["agents"]));
    m.settle(all);
    m
}

/// The spaces whose decisions stay out of the main queue and the orb's
/// number: muted by the state's missions, by a decision's own word
/// (`muted`), in `spaces.json` (`mutes`, any case), or just now here
/// (`marks`, which also unmute before the state says so).
pub fn muted_spaces(all: &[Decision], state: &Value, mutes: &[String], marks: &HashMap<String, Marks>) -> HashSet<String> {
    let mut out: HashSet<String> = missions_from_state(state).map(|m| m.muted()).unwrap_or_default();
    out.extend(all.iter().filter(|d| d.muted && !d.space.is_empty()).map(|d| d.space.clone()));
    for name in mutes.iter().chain(marks.iter().filter(|(_, m)| m.mute == Some(true)).map(|(n, _)| n)) {
        // As the decisions spell it, so the queue finds it.
        let spelt = all.iter().map(|d| &d.space).find(|s| s.eq_ignore_ascii_case(name)).unwrap_or(name);
        out.insert(spelt.clone());
    }
    for (name, _) in marks.iter().filter(|(_, m)| m.mute == Some(false)) {
        out.retain(|n| !n.eq_ignore_ascii_case(name));
    }
    out
}

/// The orb's number: the decisions that are the user's, outside the muted
/// spaces (`muted_spaces`).
pub fn count(all: &[Decision], muted: &HashSet<String>) -> usize {
    yours(&main_queue(all, muted))
}

/// Forgets the switches the state now agrees with: its missions, a
/// decision's `muted`, or the lists in `spaces.json` (`follows`, `mutes`).
/// One the state doesn't carry yet stays as the user set it.
pub fn settle_marks(marks: &mut HashMap<String, Marks>, state: &Value, all: &[Decision], follows: &[String], mutes: &[String]) {
    let known = missions_from_state(state);
    let muted = muted_spaces(all, state, mutes, &HashMap::new());
    marks.retain(|name, mark| {
        let m = known.as_ref().and_then(|k| k.open.iter().find(|m| &m.name == name));
        let followed = m.is_some_and(|m| m.follow) || follows.iter().any(|f| f.eq_ignore_ascii_case(name));
        if mark.follow == Some(followed) {
            mark.follow = None;
        }
        if mark.mute == Some(muted.iter().any(|n| n.eq_ignore_ascii_case(name))) {
            mark.mute = None;
        }
        mark.follow.is_some() || mark.mute.is_some()
    });
}

// ---------------------------------------------------------------------------
// The thread between a space's sessions, and how much the tower tells.

/// One message from a session to another (`threads` in the state, by space).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Message {
    pub at: u64,
    #[serde(deserialize_with = "or_default")]
    pub from: String,
    #[serde(deserialize_with = "or_default")]
    pub from_name: String,
    #[serde(deserialize_with = "or_default")]
    pub to: String,
    #[serde(deserialize_with = "or_default")]
    pub to_name: String,
    #[serde(deserialize_with = "or_default")]
    pub text: String,
    /// False for one that could not be handed over.
    pub delivered: bool,
}

impl Default for Message {
    fn default() -> Self {
        Self { at: 0, from: String::new(), from_name: String::new(), to: String::new(), to_name: String::new(), text: String::new(), delivered: yes() }
    }
}

impl Message {
    /// `mediaroom → eyzflow`, by name where it is known. Inside `space`'s
    /// own view a session `tower launch` named after its space (`fix login
    /// eyzflow` in `fix-login`) goes by the rest of its name (`eyzflow`).
    #[cfg(test)]
    pub fn who_in(&self, space: &str) -> String {
        let (from, to) = self.names_in(space);
        format!("{from} → {to}")
    }

    /// The two sides of `who_in`, apart.
    pub fn names_in(&self, space: &str) -> (String, String) {
        let lead: Vec<String> = space.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
        let short = |name: &str| -> String {
            let words: Vec<&str> = name.split_whitespace().collect();
            let named = !lead.is_empty() && words.len() > lead.len() && words.iter().zip(&lead).all(|(w, l)| w.to_lowercase() == *l);
            if named { words[lead.len()..].join(" ") } else { name.to_string() }
        };
        let pick = |name: &str, addr: &str| if name.is_empty() { addr.to_string() } else { short(name) };
        (pick(&self.from_name, &self.from), pick(&self.to_name, &self.to))
    }
}

/// A space's thread as the state has it, newest last (none while the daemon
/// doesn't write `threads`).
pub fn thread(state: &Value, space: &str) -> Vec<Message> {
    state["threads"][space]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| serde_json::from_value::<Message>(v.clone()).ok())
        .filter(|m| !m.text.trim().is_empty())
        .collect()
}

/// A line of the chat about a plan (`plan.thread` in the state).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct PlanMsg {
    /// `user` or `planner`.
    #[serde(deserialize_with = "or_default")]
    pub from: String,
    #[serde(deserialize_with = "or_default")]
    pub text: String,
    pub at: u64,
}

impl PlanMsg {
    pub fn mine(&self) -> bool {
        self.from == "user"
    }
}

/// The plan that is open, as the state has it (`plan`, see `crate::plans`).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct PlanView {
    #[serde(deserialize_with = "or_default")]
    pub id: String,
    /// `page` or `whiteboard`.
    #[serde(deserialize_with = "or_default")]
    pub form: String,
    /// `writing`, `ready` or `failed`.
    #[serde(deserialize_with = "or_default")]
    pub status: String,
    #[serde(deserialize_with = "or_default")]
    pub ticket: String,
    /// The draft as the planner last revised it.
    pub draft: Value,
    pub revision: u32,
    pub address: Option<String>,
    /// How the planner stands: `starting`, `working`, `idle`, `blocked`, `gone`.
    #[serde(deserialize_with = "or_default")]
    pub planner: String,
    pub blocked: bool,
    /// The user's last line has no answer yet.
    pub answering: bool,
    pub url: Option<String>,
    #[serde(deserialize_with = "or_default")]
    pub why: String,
    #[serde(deserialize_with = "or_default")]
    pub thread: Vec<PlanMsg>,
    pub created_ms: u64,
    pub ready_ms: Option<u64>,
    /// The pull requests it predicts, each with its review score (`tower plan prs`).
    #[serde(deserialize_with = "or_default")]
    pub prs: Vec<crate::prscore::Predicted>,
}

impl PlanView {
    /// What it is published as, in a word for the window: "page", "whiteboard".
    pub fn form_word(&self) -> &str {
        if self.form == "whiteboard" { "whiteboard" } else { "page" }
    }
}

/// The open plan, when the state has one (an older tower's has none).
pub fn plan(state: &Value) -> Option<PlanView> {
    let v = state.get("plan").filter(|v| v.is_object())?;
    let mut p: PlanView = serde_json::from_value(v.clone()).ok()?;
    p.thread.retain(|m| !m.text.trim().is_empty());
    (!p.id.is_empty() && p.status != "closed").then_some(p)
}

/// The release track of the space called `space`, as the state has it
/// (`releases`, worked out by the daemon from GitHub); none without one.
pub fn release_track(state: &Value, space: &str) -> Option<crate::releases::Track> {
    let all = state.get("releases")?.as_object()?;
    let v = all.iter().find(|(k, _)| k.eq_ignore_ascii_case(space)).map(|(_, v)| v)?;
    let t: crate::releases::Track = serde_json::from_value(v.clone()).ok()?;
    (!t.repos.is_empty()).then_some(t)
}

/// The space called `space` has a tower of its own that runs, as the state's missions say.
pub fn has_own_tower(state: &Value, space: &str) -> bool {
    state["missions"]["spaces"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(space)))
        .is_some_and(|m| m["tower"]["status"].as_str().is_some_and(|s| !s.is_empty() && s != "off"))
}

/// A model the picker offers.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ModelChoice {
    pub id: String,
    pub label: String,
}

/// The brains' model as the state has it (`brain_model`, see `crate::brains::report`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct BrainModel {
    /// `claude` or `codex`.
    pub program: String,
    /// The setting: what a brain is started on next.
    pub model: String,
    pub effort: String,
    #[serde(skip)]
    pub models: Vec<ModelChoice>,
    #[serde(skip)]
    pub efforts: Vec<String>,
    /// A running brain was started on something else.
    pub restart_needed: bool,
    /// The user's own `claude_args` set the model or the effort: they win.
    pub locked_by_args: bool,
    /// What runs, and what a change does, in a sentence or two.
    pub says: String,
}

/// The brains' model, when the state says it (an older tower doesn't).
pub fn brain_model(state: &Value) -> Option<BrainModel> {
    let v = state.get("brain_model").filter(|v| v.is_object())?;
    let mut m: BrainModel = serde_json::from_value(v.clone()).ok()?;
    m.models = serde_json::from_value(v["choices"]["models"].clone()).unwrap_or_default();
    m.efforts = serde_json::from_value(v["choices"]["efforts"].clone()).unwrap_or_default();
    (!m.models.is_empty()).then_some(m)
}

/// How much the tower tells (`tell_level` in the state): `decisions`,
/// `results` or `everything`; `None` when the state doesn't say.
pub fn tell_level(state: &Value) -> Option<&str> {
    state["tell_level"].as_str().filter(|l| matches!(*l, "decisions" | "results" | "everything"))
}

/// What the tower says out loud, as the state says it: `off`, `decisions`
/// or `replies`. A state from before the setting says only whether it is
/// muted: `off` or `replies`.
pub fn tell_voice(state: &Value) -> &'static str {
    match (state["tell_voice"].as_str(), state["muted"].as_bool()) {
        (Some("off"), _) | (None, Some(true)) => "off",
        (Some("decisions"), _) => "decisions",
        _ => "replies",
    }
}

// ---------------------------------------------------------------------------
// What the views share about the queue.

/// The answer just given, as a standing rule: what would be asked of the
/// tower (`crate::decisions::rule_request`), and in which space's view.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleOffer {
    pub space: Option<String>,
    pub text: String,
}

/// The rule an answer to `d` can be made into, if one is offered: only for a
/// question or a choice, never for what the user always decides
/// (`crate::decisions::rule_for`).
pub fn rule_offer(d: &Decision, answer: &str) -> Option<RuleOffer> {
    let kind = match d.kind {
        Kind::Permission => crate::decisions::Kind::Permission,
        Kind::Question => crate::decisions::Kind::Question,
        Kind::Merge => crate::decisions::Kind::Merge,
        Kind::Choice => crate::decisions::Kind::Choice,
        Kind::Failed => crate::decisions::Kind::Failed,
        Kind::Raised => crate::decisions::Kind::Raised,
        Kind::Blocked => crate::decisions::Kind::Blocked,
        // Not known here: no rule is offered for it.
        Kind::Other => return None,
    };
    let space = Some(d.space.trim()).filter(|s| !s.is_empty());
    crate::decisions::rule_for(kind, &d.why, &d.title, space, answer).map(|text| RuleOffer { space: space.map(String::from), text })
}

/// The line an answered card leaves for a moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Receipt {
    pub id: String,
    pub text: String,
    pub at_ms: u64,
    /// "Make it a rule" is offered on it.
    pub rule: Option<RuleOffer>,
}

impl Receipt {
    /// How long it shows: long enough to be used while it offers a rule.
    fn life(&self) -> u64 {
        if self.rule.is_some() { RULE_MS } else { RECEIPT_MS }
    }
}

/// The queue as the user left it: the card picked from behind the open one,
/// what was answered here and not yet seen to land, and the receipts
/// showing. Kept with the tower's state, so every view has the same.
#[derive(Debug, Default)]
pub struct Deck {
    /// The decision picked to be the open card.
    pub focus: Option<String>,
    /// The request (its time) whose reply the user closed.
    pub reply_closed: u64,
    /// Answered here: the receipt each will leave once it is gone from the
    /// queue, and the rule its answer can be made into.
    pending: HashMap<String, (String, Option<RuleOffer>)>,
    /// The decisions that were live when last looked at.
    seen: HashSet<String>,
    /// Those whose answer was going in when last looked at.
    going: HashSet<String>,
    receipts: Vec<Receipt>,
}

impl Deck {
    /// Picks a card: it is the open one while it is the user's.
    pub fn pick(&mut self, id: &str) {
        self.focus = Some(id.to_string());
    }

    /// The user answered `id` here; `text` is what its receipt will say.
    #[cfg(test)]
    pub fn answered(&mut self, id: &str, text: String) {
        self.answered_with(id, text, None);
    }

    /// `answered`, with the rule the answer can be made into: its receipt
    /// offers it. The receipts of cards answered before offer theirs no more.
    pub fn answered_with(&mut self, id: &str, text: String, rule: Option<RuleOffer>) {
        self.pending.insert(id.to_string(), (text, rule));
        for r in &mut self.receipts {
            r.rule = None;
        }
        if self.focus.as_deref() == Some(id) {
            self.focus = None;
        }
    }

    /// The user took a receipt's offer: it says so for a moment, and offers nothing more.
    pub fn rule_asked(&mut self, id: &str, now_ms: u64) {
        if let Some(r) = self.receipts.iter_mut().find(|r| r.id == id) {
            (r.text, r.rule, r.at_ms) = ("Asked the tower to make it a rule.".into(), None, now_ms);
        }
    }

    /// The answer didn't go: no receipt is owed.
    pub fn retract(&mut self, id: &str) {
        self.pending.remove(id);
    }

    /// A receipt at once (a merge that went through).
    pub fn note(&mut self, id: &str, text: String, now_ms: u64) {
        self.receipts.retain(|r| r.id != id);
        self.receipts.push(Receipt { id: id.to_string(), text, at_ms: now_ms, rule: None });
    }

    /// Looks at the decisions again. One that left the queue (gone, or
    /// answered) leaves a receipt if it was answered here, or if the state
    /// says it was answered. One whose answer stopped half way (it was
    /// going in, and is open again) is the user's again and owes none; nor
    /// does one that is over without an answer. True when a receipt was added.
    pub fn sync(&mut self, all: &[Decision], now_ms: u64) -> bool {
        self.receipts.retain(|r| now_ms.saturating_sub(r.at_ms) < r.life());
        // (Before the leavers are looked at: a list may say both at once.)
        for d in all.iter().filter(|d| d.state == Stage::Stopped || (d.state == Stage::Open && self.going.contains(&d.id))) {
            self.pending.remove(&d.id);
        }
        let live: HashSet<String> = all.iter().filter(|d| d.live()).map(|d| d.id.clone()).collect();
        let mut added = false;
        for id in self.seen.difference(&live) {
            let said = all.iter().find(|d| &d.id == id && d.state == Stage::Answered).map(|d| (format!("Answered: {}", d.title), None));
            if let Some((text, rule)) = self.pending.remove(id).or(said) {
                self.receipts.push(Receipt { id: id.clone(), text, at_ms: now_ms, rule });
                added = true;
            }
        }
        if self.focus.as_ref().is_some_and(|f| !all.iter().any(|d| d.yours() && &d.id == f)) {
            self.focus = None;
        }
        self.going = all.iter().filter(|d| d.state == Stage::Answering).map(|d| d.id.clone()).collect();
        self.seen = live;
        added
    }

    /// The receipts still showing, oldest first.
    pub fn receipts(&self, now_ms: u64) -> Vec<&Receipt> {
        self.receipts.iter().filter(|r| now_ms.saturating_sub(r.at_ms) < r.life()).collect()
    }
}

/// A receipt's sentence: `Answered: B, magnified view.`
pub fn receipt_text(what: &str) -> String {
    let what = what.trim().trim_end_matches('.');
    if what.is_empty() { "Answered.".into() } else { format!("Answered: {what}.") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MIN: u64 = 60_000;

    fn decision(id: &str, space: &str, waiting: usize, first_ms: u64) -> Decision {
        Decision {
            id: id.into(),
            key: id.into(),
            space: space.into(),
            waiting,
            title: format!("{id}?"),
            first_ms,
            last_ms: first_ms,
            count: 1,
            ..Default::default()
        }
    }

    fn ids(list: &[Decision]) -> Vec<&str> {
        list.iter().map(|d| d.id.as_str()).collect()
    }

    fn pr(repo: &str, number: u64, base: &str, space: &str, session: &str, found_ms: u64) -> Pr {
        Pr {
            repo: repo.into(),
            number,
            base: base.into(),
            state: "open".into(),
            waiting: true,
            space: Some(space.into()).filter(|s: &String| !s.is_empty()),
            session: session.into(),
            name: format!("{session}-name"),
            found_ms,
            ..Default::default()
        }
    }

    fn prs(list: Vec<Pr>) -> BTreeMap<String, Pr> {
        list.into_iter().map(|p| (p.key(), p)).collect()
    }

    #[test]
    fn the_queue_is_heaviest_first_then_oldest() {
        let now = 100 * MIN;
        // As the design has it: 3 sessions for 2 minutes (6), 1 for 4 (4), none for 31 (0).
        let mut list = vec![
            decision("pr", "isa", 0, now - 31 * MIN),
            decision("pick", "landing", 1, now - 4 * MIN),
            decision("db", "sandbox", 3, now - 2 * MIN),
        ];
        order(&mut list, now);
        assert_eq!(ids(&list), ["db", "pick", "pr"]);
        // Equal weights: the one that waited longer leads.
        let mut tie = vec![
            decision("new", "a", 0, now - MIN),
            decision("old", "a", 0, now - 9 * MIN),
            decision("two", "a", 2, now - 3 * MIN),
            decision("six", "a", 1, now - 6 * MIN),
        ];
        order(&mut tie, now);
        assert_eq!(ids(&tie), ["six", "two", "old", "new"], "6 and 6: the older one first; then the unblocked, oldest first");
        // Time moves it: one session waiting long outweighs three that just began.
        let mut later = vec![decision("three", "a", 3, now - MIN), decision("one", "a", 1, now - 10 * MIN)];
        order(&mut later, now);
        assert_eq!(ids(&later), ["one", "three"]);
        assert_eq!((later[0].weight(now), later[1].weight(now)), (10 * MIN, 3 * MIN));
        // In ms, as the daemon weighs them: two in their first minute are still ordered.
        let mut fresh = vec![decision("b", "a", 1, now - 20_000), decision("a", "a", 2, now - 15_000)];
        order(&mut fresh, now);
        assert_eq!(ids(&fresh), ["a", "b"], "2 × 15 s outweighs 1 × 20 s");
    }

    #[test]
    fn a_muted_space_is_out_of_the_main_queue_and_the_count() {
        let all = vec![decision("db", "sandbox", 3, 0), decision("pick", "landing", 1, 0), decision("pr", "isa", 0, 0)];
        let none = HashSet::new();
        assert_eq!(yours(&main_queue(&all, &none)), 3);
        let muted: HashSet<String> = ["sandbox".to_string()].into();
        let q = main_queue(&all, &muted);
        assert_eq!(ids(&q), ["pick", "pr"]);
        assert_eq!(yours(&q), 2);
        // It is still a decision of its own space.
        assert!(all.iter().any(|d| d.live() && d.space == "sandbox"));
    }

    #[test]
    fn the_open_card_is_the_picked_one_while_it_is_open() {
        let mut q = vec![decision("db", "sandbox", 3, 0), decision("pick", "landing", 1, 0), decision("pr", "isa", 0, 0)];
        let open = |q: &[Decision], f: Option<&str>| open_card(q, f).map(|d| d.id.clone());
        assert_eq!(open(&q, None).as_deref(), Some("db"));
        assert_eq!(tag(&q, &q[0]), "Next");
        assert_eq!(open(&q, Some("pr")).as_deref(), Some("pr"));
        assert_eq!(tag(&q, &q[2]), "Picked");
        // Picked, then answered elsewhere: back to the first.
        assert_eq!(open(&q, Some("gone")).as_deref(), Some("db"));
        // The first one's answer is going in: the next is open, and it is "Next".
        q[0].state = Stage::Answering;
        assert_eq!(open(&q, None).as_deref(), Some("pick"));
        assert_eq!(open(&q, Some("db")).as_deref(), Some("pick"), "an answer going in is not picked again");
        assert_eq!(tag(&q, &q[1]), "Next");
        assert_eq!(yours(&q), 2);
        // Its answer stopped half way: the daemon says it is open again, and it is the user's.
        q[0].state = Stage::Open;
        assert_eq!(open(&q, None).as_deref(), Some("db"));
        // Over without an answer (its sessions are gone): not a card any more.
        q[0].state = Stage::Stopped;
        assert!(!q[0].live() && !q[0].yours());
        assert_eq!(open(&q, Some("db")).as_deref(), Some("pick"));
        assert_eq!(yours(&q), 2);
        assert!(open_card(&[], None).is_none());
    }

    #[test]
    fn the_status_line_counts_what_needs_you() {
        assert_eq!(status_line(3, 3), (Some("3 need you".to_string()), "· 3 working".to_string()));
        assert_eq!(status_line(1, 0), (Some("1 needs you".to_string()), "· 0 working".to_string()));
        assert_eq!(status_line(0, 2), (None, "Nothing needs you · 2 working".to_string()));
    }

    #[test]
    fn a_card_says_who_waits_and_for_how_long() {
        let now = 200 * MIN;
        let d = decision("a", "s", 3, now - 2 * MIN);
        assert_eq!((d.stake(), d.waited(now)), ("3 sessions wait".to_string(), "2 min".to_string()));
        let d = decision("a", "s", 1, now - 20_000);
        assert_eq!((d.stake(), d.waited(now)), ("1 session waits".to_string(), "just now".to_string()));
        let d = decision("a", "s", 0, now - 185 * MIN);
        assert_eq!((d.stake(), d.waited(now)), ("Nothing is blocked".to_string(), "3 h".to_string()));
        let failed = Decision { kind: Kind::Failed, ..decision("a", "s", 1, 0) };
        assert_eq!(failed.stake(), "1 session stopped");
    }

    #[test]
    fn merges_of_one_repo_and_space_are_one_decision() {
        let all = prs(vec![
            pr("acme/shop", 1277, "main", "isa", "t2", 5000),
            pr("acme/shop", 1273, "dev", "isa", "t1", 9000),
            pr("acme/shop", 1280, "dev", "sandbox", "t3", 7000),
            pr("acme/site", 12, "main", "isa", "t1", 8000),
            // Not waiting, merged, closed: none of the user's business.
            Pr { waiting: false, ..pr("acme/shop", 1290, "dev", "isa", "t1", 1) },
            Pr { state: "merged".into(), ..pr("acme/shop", 1291, "dev", "isa", "t1", 1) },
        ]);
        let agents = json!([{ "address": "t2", "status": "blocked" }, { "address": "t1", "status": "idle" }]);
        let out = adapt(&[], &all, &agents, &|_| String::new());
        assert_eq!(out.len(), 3);
        let shop = out.iter().find(|d| d.id == "merge:acme/shop@isa").expect("one for the repo in isa");
        assert_eq!(shop.kind, Kind::Merge);
        assert_eq!(
            shop.prs.iter().map(|p| (p.number, p.base.as_str())).collect::<Vec<_>>(),
            [(1273, "dev"), (1277, "main")],
            "dev before main"
        );
        assert_eq!(shop.title, "2 pull requests ready to merge");
        assert_eq!(shop.options, [Opt { label: "Merge both, dev first".into(), why: String::new(), pick: true }]);
        assert_eq!((shop.first_ms, shop.last_ms), (5000, 9000));
        assert_eq!(shop.sessions.iter().map(|s| s.address.as_str()).collect::<Vec<_>>(), ["t1", "t2"]);
        assert_eq!(shop.waiting, 1, "only the blocked session waits");
        assert_eq!(shop.prs[0].key(), "acme/shop#1273");
        let other_space = out.iter().find(|d| d.id == "merge:acme/shop@sandbox").unwrap();
        assert_eq!((other_space.space.as_str(), other_space.prs.len(), other_space.waiting), ("sandbox", 1, 0));
        assert_eq!(other_space.options[0].label, "Merge #1280");
        assert_eq!(other_space.title, "1 pull request ready to merge");
        assert!(out.iter().any(|d| d.id == "merge:acme/site@isa"));
    }

    #[test]
    fn a_merge_card_names_what_its_button_does() {
        let r = |n: u64, base: &str| PrRef { repo: "a/b".into(), number: n, base: base.into(), checks: "pass".into(), ..Default::default() };
        assert_eq!(merge_label(&[r(4, "dev")]), "Merge #4");
        assert_eq!(merge_label(&[r(4, "dev"), r(5, "main")]), "Merge both, dev first");
        assert_eq!(merge_label(&[r(4, "dev"), r(5, "dev")]), "Merge both");
        assert_eq!(merge_label(&[r(4, "dev"), r(5, "dev"), r(6, "main")]), "Merge all 3, dev first");
        assert_eq!(merge_label(&[r(4, "main"), r(5, "main"), r(6, "main")]), "Merge all 3");
    }

    fn set(id: &str, session: &str, title: &str, at: u64) -> Value {
        json!({
            "id": id, "session": session, "name": format!("{session}-name"), "kind": "claude-ask", "title": title,
            "tabs": [{ "title": "Q", "question": title, "options": [{ "label": "A" }, { "label": "B" }], "allows_other": true }],
            "decision": "escalate", "status": "open", "at_ms": at,
        })
    }

    #[test]
    fn todays_state_becomes_the_queue() {
        let now = 100 * MIN;
        let mut asked = set("q1", "t1", "Which microscope drawing?", now - 4 * MIN);
        asked["recommendations"] = json!([{ "tab": 0, "options": [
            { "label": "B, magnified view", "why": "It reads at a glance.", "choice": 1 },
            { "label": "A", "why": "", "choice": 0 },
        ] }]);
        let mut permission = set("q2", "t7", "Run the migration?", now - 2 * MIN);
        permission["kind"] = json!("claude-permission");
        let mut several = set("q3", "t8", "Release questions", now - MIN);
        several["tabs"] = json!([{ "question": "One?" }, { "question": "Two?" }]);
        several["status"] = json!("answering");
        let mut failed = set("q4", "t9", "Deploy?", now - MIN);
        failed["status"] = json!("failed");
        let mut auto = set("q5", "t9", "Routine?", now);
        auto["decision"] = json!("auto");
        let state = json!({
            "questions": [permission, asked, several, failed, auto],
            "agents": [{ "address": "t1", "status": "blocked" }],
        });
        let all_prs = prs(vec![
            pr("acme/shop", 1273, "dev", "isa", "t5", now - 31 * MIN),
            pr("acme/shop", 1277, "main", "isa", "t5", now - 30 * MIN),
        ]);
        let space_of = |a: &str| if a == "t1" { "landing".to_string() } else { "sandbox".to_string() };
        let all = decisions(&state, &all_prs, &space_of, now);
        // 1 × 4, 1 × 2, 1 × 1 (twice, the older id first on a tie), then the merge nobody waits for.
        assert_eq!(ids(&all), ["q1", "q2", "q3", "q4", "merge:acme/shop@isa"]);
        let q1 = &all[0];
        assert_eq!((q1.kind, q1.space.as_str(), q1.why.as_str(), q1.waiting), (Kind::Question, "landing", "Question", 1));
        assert_eq!(q1.question_id.as_deref(), Some("q1"));
        assert_eq!(q1.key, "landing|which microscope drawing");
        assert_eq!(q1.sessions, [SessionRef { question_id: Some("q1".into()), ..SessionRef::new("t1", "t1-name") }]);
        assert_eq!((q1.pick.as_deref(), q1.badge().as_str()), (Some("B, magnified view"), "Question"));
        assert_eq!(q1.options.iter().map(|o| (o.label.as_str(), o.pick)).collect::<Vec<_>>(), [("B, magnified view", true), ("A", false)]);
        assert_eq!((all[1].kind, all[1].why.as_str()), (Kind::Permission, "Permission"));
        assert_eq!((all[2].state, all[2].why.as_str(), all[2].options.len()), (Stage::Answering, "Questions", 0));
        assert_eq!(all[3].state, Stage::Open, "an answer that stopped half way is the user's again");
        // All five are in the queue; four are the user's now (one answer is going in).
        let q = main_queue(&all, &HashSet::new());
        assert_eq!((q.len(), yours(&q)), (5, 4));
        assert_eq!(count(&all, &muted_spaces(&all, &state, &[], &HashMap::new())), 4);
        assert_eq!(count(&decisions(&json!({}), &BTreeMap::new(), &space_of, now), &HashSet::new()), 0);
    }

    #[test]
    fn the_states_own_decisions_and_missions_are_read_as_they_are() {
        let now = 500 * MIN;
        // As the daemon writes them (`decisions::Decision::view`, `missions::view`).
        let state = json!({
            "questions": [set("old", "t1", "Ignored once the daemon writes decisions", 0)],
            "decisions_count": 2,
            "decisions": [
                { "id": "d111111111111", "key": "ask:sandbox:allow one read of the production database", "kind": "permission", "space": "sandbox",
                  "sessions": [{ "address": "t1", "name": "shop", "waiting": true, "question_id": "q9" }, { "address": "t2", "name": "api", "waiting": true, "question_id": "q10" }, { "address": "t3", "name": "docs", "waiting": false, "question_id": "q11" }],
                  "waiting": 2, "title": "Allow one read of the production database", "why": "production data",
                  "options": [{ "label": "Allow once", "why": "It only reads.", "pick": true }, { "label": "No", "why": "", "pick": false }], "pick": "Allow once",
                  "prs": [], "question_id": "q9", "task": null, "first_ms": now - 2 * MIN, "last_ms": now, "count": 3, "state": "open", "muted": true },
                { "id": "d222222222222", "key": "merge:isa:acme/shop", "kind": "merge", "space": "isa", "sessions": [{ "address": "t5", "name": "release", "waiting": false }], "waiting": 0,
                  "title": "PR #1273 into dev, PR #1277 into main", "why": "merge", "options": [{ "label": "PR #1273 into dev", "why": "", "pick": true }], "pick": "PR #1273 into dev",
                  "prs": [
                    { "key": "acme/shop#1273", "repo": "acme/shop", "number": 1273, "base": "dev", "title": "Cart totals", "url": "https://github.com/acme/shop/pull/1273", "checks": "pass", "note": "3 checks pass", "session": "t5",
                      "merge": { "can": true, "blocked": null, "needs_override": null, "warnings": [] } },
                    { "key": "acme/shop#1277", "repo": "acme/shop", "number": 1277, "base": "main", "title": "Release", "url": "", "checks": "fail", "note": "1 check failing: lint", "session": "t5",
                      "merge": { "can": true, "blocked": null, "needs_override": "1 check failing: lint. Merge anyway only if you know the failure is unrelated.", "warnings": ["It merges into main: that ships to production."] } },
                  ],
                  "question_id": null, "task": null, "first_ms": now - 31 * MIN, "last_ms": now, "count": 2, "state": "open", "muted": false },
                { "id": "d333333333333", "key": "raise::pick a logo", "kind": "choice", "space": null, "sessions": [], "waiting": 0, "title": "Pick a logo", "why": "design",
                  "options": [{ "label": "The blue one", "why": "", "pick": true }], "pick": "The blue one", "prs": [], "question_id": null, "task": null,
                  "first_ms": now, "last_ms": now, "count": 1, "state": "answering", "muted": false },
                { "id": "d444444444444", "key": "failed:isa:a7.2", "kind": "failed", "space": "isa", "sessions": [{ "address": "t5", "name": "release", "waiting": true }], "waiting": 1,
                  "title": "\"Run the suite\" failed after a retry: the build broke", "why": "failed", "options": [{ "label": "Try again", "why": "", "pick": false }, { "label": "Drop it", "why": "", "pick": false }],
                  "pick": null, "prs": [], "question_id": null, "task": "a7.2", "first_ms": now - MIN, "last_ms": now, "count": 1, "state": "open", "muted": false },
                { "nonsense": true },
            ],
            "missions": {
                "spaces": [
                    { "name": "isa", "goal": "Voice clips for the Ingest tour, live", "done_when": "", "standing": false, "state": "ready", "follow": true, "mute": false, "needs": 2, "report": "Released.",
                      "sessions": [{ "address": "t5", "name": "release", "status": "idle", "account": null, "worktree": null, "branch": null, "port": null, "said": null }],
                      "requests": [{ "id": "a7", "title": "Record the clips", "state": "done", "done": 3, "total": 3 }], "thread_count": 0,
                      "quiet_since_ms": null, "ready_since_ms": now - 60 * MIN, "auto_close_ms": now + 5 * 60 * MIN },
                    { "name": "sandbox", "goal": "A plan", "done_when": "the plan is written", "standing": false, "state": "open", "follow": false, "mute": true, "needs": 1, "report": "",
                      "sessions": [{ "address": "t1", "name": "shop", "status": "working", "account": "side", "worktree": "/w/.tower-worktrees/sandbox/shop", "branch": "sandbox/shop", "port": 3010, "said": "Wrote the plan.\nThree files." }],
                      "requests": [{ "id": "a4", "title": "Plan it", "state": "open", "done": 1, "total": 3 }, { "id": "a5", "title": "Dropped", "state": "abandoned", "done": 0, "total": 1 }], "thread_count": 33,
                      "quiet_since_ms": null, "ready_since_ms": null, "auto_close_ms": null },
                    { "name": "old", "goal": "Old work", "done_when": "", "standing": false, "state": "quiet", "follow": false, "mute": false, "needs": 0, "report": "", "sessions": [], "requests": [], "thread_count": 0,
                      "quiet_since_ms": now - 3 * 60 * MIN, "ready_since_ms": null, "auto_close_ms": null },
                ],
                "quiet": ["old"],
                "closed_today": ["gone"],
            },
        });
        let all = decisions(&state, &BTreeMap::new(), &|_| "never asked".into(), now);
        let (d1, d2, d3, d4) = ("d111111111111", "d222222222222", "d333333333333", "d444444444444");
        assert_eq!(ids(&all), [d1, d2, d3, d4], "in the daemon's order; unreadable entries left out");
        let first = &all[0];
        assert_eq!((first.kind, first.count, first.waiting, first.muted), (Kind::Permission, 3, 2, true));
        assert_eq!(first.sessions.iter().map(|s| (s.label(), s.waiting, s.question_id.as_deref())).collect::<Vec<_>>(), [("shop", true, Some("q9")), ("api", true, Some("q10")), ("docs", false, Some("q11"))]);
        assert_eq!((first.badge().as_str(), first.reason(), first.pick.as_deref()), ("Production data", None, Some("Allow once")));
        assert_eq!(first.options[1], Opt { label: "No".into(), why: String::new(), pick: false });
        assert_eq!((first.lead().map(|s| s.label()), first.stake().as_str()), (Some("shop"), "2 sessions wait"));
        // A merge: its PRs as the daemon has them, each with whether it can be merged.
        let merge = &all[1];
        assert_eq!((merge.badge().as_str(), merge.prs[0].key().as_str(), merge.prs[0].title.as_str(), merge.prs[1].checks.as_str(), merge.prs[1].note.as_str()), ("Merge", "acme/shop#1273", "Cart totals", "fail", "1 check failing: lint"));
        assert!(merge.prs[1].merge.as_ref().is_some_and(|m| m.can && m.needs_override.is_some() && m.warnings.len() == 1));
        assert_eq!(merge.lead().map(|s| (s.label(), s.waiting)), Some(("release", false)));
        // One in no space, its answer going in; one for a task that failed twice.
        assert_eq!((all[2].space.as_str(), all[2].state, all[2].badge().as_str()), ("", Stage::Answering, "Design"));
        assert_eq!((all[3].badge().as_str(), all[3].task.as_deref(), all[3].stake().as_str()), ("Failed twice", Some("a7.2"), "1 session stopped"));
        // A long reason is said in full under the card, not on its badge.
        let long = Decision { why: "it would send email to every customer on the list".into(), ..all[0].clone() };
        assert_eq!((long.badge().as_str(), long.reason().as_deref()), ("Permission", Some("it would send email to every customer on the list")));
        let waits = Decision { kind: Kind::Question, why: crate::questions::WAITS.into(), ..all[0].clone() };
        assert_eq!((waits.badge().as_str(), waits.reason()), ("Waits for you", None));

        let m = missions(&state, &Spaces::default(), &[], &[], &all);
        assert_eq!(m.open.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["isa", "sandbox"], "as the daemon lists them");
        assert_eq!((m.quiet.as_slice(), m.closed_today.as_slice(), m.working()), (["old".to_string()].as_slice(), ["gone".to_string()].as_slice(), 1));
        assert_eq!(m.resting.iter().map(|r| (r.name.as_str(), r.goal.as_str(), r.quiet_since_ms)).collect::<Vec<_>>(), [("old", "Old work", Some(now - 3 * 60 * MIN))]);
        let (isa, sandbox) = (&m.open[0], &m.open[1]);
        assert_eq!((isa.chip(), isa.state, isa.report.as_str(), isa.follow), (Chip::Yours(2), Phase::Ready, "Released.", true));
        assert_eq!((isa.auto_close_ms, isa.closes_in(now).as_deref(), isa.side_line().as_str()), (Some(now + 5 * 60 * MIN), Some("in 5 h"), "ready to close"));
        assert_eq!((sandbox.needs, sandbox.chip()), (1, Chip::Muted(1)), "needs are counted from the decisions that are the user's");
        let shop = &sandbox.sessions[0];
        assert_eq!((shop.port, shop.branch.as_deref(), shop.place().as_str()), (Some(3010), Some("sandbox/shop"), "worktree and branch sandbox/shop · port 3010"));
        assert_eq!(shop.last_words().as_deref(), Some("Wrote the plan."));
        assert_eq!(sandbox.requests.iter().map(|r| (r.id.as_str(), r.progress())).collect::<Vec<_>>(), [(Some("a4"), "1 of 3".to_string()), (Some("a5"), "dropped".to_string())]);
        assert_eq!((sandbox.current().map(|r| r.title.as_str()), sandbox.closes_in(now)), (Some("Plan it"), None));
        // Muted: sandbox's decision is out of the main queue and the orb's number; an answer going in isn't counted.
        assert_eq!(ids(&main_queue(&all, &m.muted())), [d2, d3, d4]);
        assert_eq!(count(&all, &muted_spaces(&all, &state, &[], &HashMap::new())), 2);
        assert_eq!(Some(count(&all, &muted_spaces(&all, &state, &[], &HashMap::new())) as u64), state["decisions_count"].as_u64().map(|n| n), "the daemon's own count");
        // A bare list of missions reads too.
        let list = json!({ "missions": [{ "name": "a", "state": "open" }, { "name": "b", "state": "quiet" }, { "name": "c", "state": "closed" }] });
        let m = missions_from_state(&list).unwrap();
        assert_eq!((m.open.len(), m.quiet, m.closed_today.len()), (1, vec!["b".to_string()], 0));
        assert!(missions_from_state(&json!({})).is_none() && from_state(&json!({})).is_none());
    }

    #[test]
    fn the_state_says_which_space_has_its_own_tower_and_what_the_brains_run_on() {
        let state = json!({
            "missions": { "spaces": [
                { "name": "sandbox", "state": "open", "sessions": [{ "address": "t5", "name": "sandbox app", "status": "working" }], "tower": { "name": "tower-sandbox", "status": "working", "address": "t12" } },
                { "name": "docs", "state": "open", "tower": { "name": "tower-docs", "status": "off", "address": null } },
                { "name": "isa", "state": "open", "tower": null },
            ], "quiet": [], "closed_today": [] },
            "brain_model": {
                "program": "claude", "model": "claude-opus-5-5", "effort": "high",
                "choices": { "models": [{ "id": "claude-fable-5-2", "label": "Fable 5.2" }, { "id": "claude-opus-5-5", "label": "Opus 5.5" }], "efforts": ["low", "medium", "high", "xhigh", "max"] },
                "running": [{ "brain": "tower", "space": null, "address": "t7", "model": "sonnet", "effort": "medium", "restart": true }],
                "restart_needed": true, "locked_by_args": false, "locked": { "model": null, "effort": null },
                "says": "The tower's brain runs on sonnet at medium effort. Opus 5.5 at high effort applies when it restarts.",
            },
        });
        let m = missions_from_state(&state).unwrap();
        assert_eq!(m.open.iter().map(|m| (m.name.as_str(), m.own_tower(), m.tower_line())).collect::<Vec<_>>(), [("sandbox", true, Some("its own tower")), ("docs", false, None), ("isa", false, None)]);
        assert_eq!(m.open[0].workers().len(), 1, "its tower is not one of its sessions");
        assert!(has_own_tower(&state, "Sandbox") && !has_own_tower(&state, "docs") && !has_own_tower(&state, "isa") && !has_own_tower(&state, "elsewhere"));
        let b = brain_model(&state).unwrap();
        assert_eq!((b.program.as_str(), b.model.as_str(), b.effort.as_str(), b.restart_needed, b.locked_by_args), ("claude", "claude-opus-5-5", "high", true, false));
        assert_eq!(b.models.iter().map(|c| (c.id.as_str(), c.label.as_str())).collect::<Vec<_>>(), [("claude-fable-5-2", "Fable 5.2"), ("claude-opus-5-5", "Opus 5.5")]);
        assert_eq!(b.efforts.len(), 5);
        assert!(b.says.starts_with("The tower's brain runs on sonnet"));
        // What the daemon writes is what the window reads.
        let own = brain_model(&json!({ "brain_model": crate::brains::report(&crate::config::Config::default(), &[]) })).unwrap();
        assert_eq!((own.model.as_str(), own.effort.as_str(), own.models.len(), own.efforts.len(), own.restart_needed), ("claude-opus-5-5", "high", 4, 5, false));
        // An older tower says neither: no picker, and no space has its own.
        assert!(brain_model(&json!({})).is_none() && brain_model(&json!({ "brain_model": null })).is_none());
        assert!(!has_own_tower(&json!({}), "sandbox"));
    }

    #[test]
    fn herdrs_spaces_become_missions() {
        let live = vec![
            Live {
                name: "sandbox".into(),
                sessions: vec![("t1".into(), "shop".into(), "working".into()), ("t2".into(), "api".into(), "idle".into())],
            },
            Live { name: "isa".into(), sessions: vec![("t3".into(), "media".into(), "idle".into())] },
            Live { name: "anna".into(), sessions: vec![("t4".into(), "case".into(), "blocked".into())] },
            Live { name: "empty".into(), sessions: vec![] },
        ];
        let mut spaces = Spaces::default();
        spaces.agendas.insert("sandbox".into(), "A plan for the live editor".into());
        spaces.agendas.insert("archive".into(), "Old work".into());
        let known = vec!["sandbox".to_string(), "uiux".to_string(), "empty".to_string()];
        let agents = json!([{ "address": "t1", "account": "side" }, { "address": "t2", "account": " " }]);
        let state = json!({ "agents": agents });
        let waits = |id: &str, space: &str, session: &str| Decision {
            sessions: vec![SessionRef::new(session, "")],
            ..decision(id, space, 1, 0)
        };
        let all = vec![waits("q", "isa", "t3"), Decision { state: Stage::Answering, ..waits("r", "sandbox", "t2") }];
        let m = missions(&state, &spaces, &live, &known, &all);
        assert_eq!(m.open.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["sandbox", "isa", "anna"]);
        assert_eq!(m.quiet, ["empty", "uiux", "archive"], "no live session: quiet, each once");
        assert_eq!(m.open[0].goal, "A plan for the live editor");
        assert_eq!(m.open[0].sessions.iter().map(|s| s.account.as_deref()).collect::<Vec<_>>(), [Some("side"), None]);
        assert!(m.muted().is_empty(), "nothing is muted until the daemon says so");
        // Chips: what needs you first, then at work, then at rest.
        let chips: Vec<(&str, Chip)> = m.chips().into_iter().map(|(m, c)| (m.name.as_str(), c)).collect();
        assert_eq!(chips, [("isa", Chip::Yours(1)), ("sandbox", Chip::Working), ("anna", Chip::Working)]);
        assert_eq!(m.working(), 2, "anna's session is blocked on something the tower answers itself: at work");
        // A session waiting on the user's decision is not at work; its space rests once that is all.
        let all = vec![waits("q", "anna", "t4")];
        let m = missions(&state, &spaces, &live, &known, &all);
        assert_eq!((m.open[2].chip(), m.open[2].at_work, m.working()), (Chip::Yours(1), false, 1));
        assert_eq!(m.open[1].chip(), Chip::Done, "isa's one session is idle");
        // One the decision names but that works on something else meanwhile is at work.
        let mut busy = waits("q", "anna", "t4");
        busy.sessions[0].waiting = false;
        assert!(missions(&state, &spaces, &live, &known, &[busy]).open[2].at_work);
        // What the tower keeps of a mission in spaces.json is read with it; a closed or archived space is not quiet.
        spaces.missions.insert("sandbox".into(), crate::missions::Mission { done_when: "the plan is agreed".into(), standing: true, ..Default::default() });
        spaces.missions.insert("isa".into(), crate::missions::Mission { state: crate::missions::State::Ready, report: "Released.".into(), ..Default::default() });
        spaces.missions.insert("uiux".into(), crate::missions::Mission { state: crate::missions::State::Closed, ..Default::default() });
        spaces.missions.insert("archive".into(), crate::missions::Mission { state: crate::missions::State::Quiet, archived_ms: Some(1), ..Default::default() });
        let m = missions(&state, &spaces, &live, &known, &[]);
        assert_eq!((m.open[0].done_when.as_str(), m.open[0].standing, m.open[1].ready(), m.open[1].report.as_str()), ("the plan is agreed", true, true, "Released."));
        assert_eq!(m.quiet, ["empty"]);
    }

    #[test]
    fn an_answer_leaves_a_receipt_for_a_moment() {
        let mut deck = Deck::default();
        let mut all = vec![decision("db", "sandbox", 3, 0), decision("pick", "landing", 1, 0)];
        assert!(!deck.sync(&all, 1000));
        deck.pick("pick");
        assert_eq!(open_card(&all, deck.focus.as_deref()).unwrap().id, "pick");
        // Answered here: it goes in, then leaves the queue.
        deck.answered("pick", receipt_text("B, magnified view"));
        assert_eq!(deck.focus, None, "the next card is open at once");
        all[1].state = Stage::Answering;
        assert!(!deck.sync(&all, 2000));
        assert!(deck.receipts(2000).is_empty(), "no receipt while it is still going in");
        all.remove(1);
        assert!(deck.sync(&all, 3000));
        assert_eq!(deck.receipts(3000).iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["Answered: B, magnified view."]);
        assert_eq!(deck.receipts(3000 + RECEIPT_MS - 1).len(), 1);
        assert!(deck.receipts(3000 + RECEIPT_MS).is_empty(), "about two seconds, then gone");
        // Gone without an answer from here (dismissed, answered in the terminal): no receipt.
        all.clear();
        assert!(!deck.sync(&all, 9000));
        assert!(deck.receipts(9000).is_empty());
    }

    #[test]
    fn a_stopped_answer_owes_no_receipt_and_the_states_word_counts() {
        let mut deck = Deck::default();
        let mut all = vec![decision("q", "s", 1, 0), decision("d", "s", 1, 0), decision("x", "s", 1, 0)];
        deck.sync(&all, 0);
        // Answered here; it goes in, then stops half way: the daemon says it is open again.
        deck.answered("q", "Answered: yes.".into());
        all[0].state = Stage::Answering;
        deck.sync(&all, 5);
        all[0].state = Stage::Open;
        deck.sync(&all, 10);
        // Answered later in the terminal: it leaves without the old receipt.
        all.remove(0);
        assert!(!deck.sync(&all, 20));
        // Answered here, but over without the answer landing (its session ended): none either.
        deck.answered("x", "Answered: no.".into());
        all[1].state = Stage::Stopped;
        assert!(!deck.sync(&all, 22));
        all.remove(1);
        assert!(!deck.sync(&all, 24));
        assert!(deck.receipts(24).is_empty());
        // The daemon says a decision was answered (from the phone, say).
        all[0].state = Stage::Answered;
        assert!(deck.sync(&all, 30));
        assert_eq!(deck.receipts(30)[0].text, "Answered: d?");
        // Seen answered once; not again.
        assert!(!deck.sync(&all, 40));
        // A merge's receipt is immediate, and a picked card that is gone is forgotten.
        deck.pick("d");
        deck.note("acme/shop#1273", "Merged #1273 into dev.".into(), 50);
        deck.sync(&all, 60);
        assert_eq!(deck.focus, None);
        assert_eq!(deck.receipts(60).len(), 2);
        assert_eq!(receipt_text(" Postgres. "), "Answered: Postgres.");
        assert_eq!(receipt_text(""), "Answered.");
    }

    #[test]
    fn the_voice_setting_reads_from_any_tower() {
        assert_eq!(tell_voice(&json!({ "tell_voice": "decisions", "muted": false })), "decisions");
        assert_eq!(tell_voice(&json!({ "tell_voice": "off", "muted": true })), "off");
        assert_eq!(tell_voice(&json!({ "tell_voice": "replies" })), "replies");
        // A tower from before the setting: muted or not.
        assert_eq!((tell_voice(&json!({ "muted": true })), tell_voice(&json!({ "muted": false })), tell_voice(&json!({}))), ("off", "replies", "replies"));
    }

    #[test]
    fn an_answered_cards_receipt_offers_to_make_it_a_rule() {
        let kind = |kind: Kind, why: &str, title: &str| Decision { kind, why: why.into(), title: title.into(), ..decision("pick", "landing", 1, 0) };
        // Offered for what a session asked and for a choice; in the user's words, for that space.
        let offer = rule_offer(&kind(Kind::Choice, "design", "A or B for the magnified view?"), "B").unwrap();
        assert_eq!(offer.space.as_deref(), Some("landing"));
        assert_eq!(offer.text, "Make this a rule: when a session in landing asks \"A or B for the magnified view?\", answer \"B\" without asking me.");
        assert!(rule_offer(&kind(Kind::Question, "design", "Which chart library?"), "Recharts").is_some());
        // Never for a merge, a permission of any kind, a failed task, a plain raise, or what is always the user's.
        for k in [Kind::Merge, Kind::Permission, Kind::Failed, Kind::Raised] {
            assert_eq!(rule_offer(&kind(k, "design", "Which chart library?"), "Recharts"), None, "{k:?}");
        }
        assert_eq!(rule_offer(&kind(Kind::Question, "it deletes or overwrites", "Clear the cache folder?"), "Yes"), None);
        assert_eq!(rule_offer(&kind(Kind::Choice, "design", "Deploy to staging or production?"), "Staging"), None);
        // Its receipt stays long enough to be used: about eight seconds, not two.
        let mut deck = Deck::default();
        let mut all = vec![decision("pick", "landing", 1, 0), decision("next", "landing", 1, 0)];
        deck.sync(&all, 0);
        deck.answered_with("pick", receipt_text("B"), Some(offer.clone()));
        all.remove(0);
        assert!(deck.sync(&all, 1000));
        assert_eq!(deck.receipts(1000 + RECEIPT_MS)[0].rule, Some(offer.clone()), "still there after two seconds");
        assert_eq!(deck.receipts(1000 + RULE_MS - 1).len(), 1);
        assert!(deck.receipts(1000 + RULE_MS).is_empty());
        // Or until the next card is answered: then it offers no more, and goes as any receipt does.
        deck.answered("next", receipt_text("yes"));
        assert_eq!(deck.receipts(2000)[0].rule, None);
        assert!(deck.receipts(1000 + RECEIPT_MS).is_empty());
        // Taken: it says so for a moment.
        let mut deck = Deck::default();
        let mut all = vec![decision("pick", "landing", 1, 0)];
        deck.sync(&all, 0);
        deck.answered_with("pick", receipt_text("B"), Some(offer));
        all.clear();
        deck.sync(&all, 1000);
        deck.rule_asked("pick", 4000);
        let shown = deck.receipts(4000 + RECEIPT_MS - 1);
        assert_eq!((shown[0].text.as_str(), shown[0].rule.is_none()), ("Asked the tower to make it a rule.", true));
        assert!(deck.receipts(4000 + RECEIPT_MS).is_empty());
    }

    fn member(name: &str, status: &str, account: Option<&str>) -> Member {
        Member { address: format!("t-{name}"), name: name.into(), status: status.into(), account: account.map(String::from), ..Default::default() }
    }

    fn request(title: &str, state: &str, done: u64, total: u64) -> Request {
        Request { id: json!(1), title: title.into(), state: state.into(), done, total }
    }

    #[test]
    fn the_sidebar_says_how_each_space_stands() {
        let mut m = Mission {
            name: "sandbox".into(),
            goal: "A plan for the storefront live editor".into(),
            sessions: vec![member("eyzflow", "idle", Some("orca")), member("mediaroom", "working", Some("orca")), member("tower-sandbox", "idle", None)],
            tower: Some(OwnTower { name: "tower-sandbox".into(), status: "ready".into() }),
            ..Default::default()
        };
        // Its tower's own terminal is not one of its sessions.
        assert_eq!(m.workers().len(), 2);
        assert!(m.own_tower());
        assert_eq!(m.tower_line(), Some("its own tower"));
        assert_eq!((m.sub(), m.side_line()), ("2 sessions on orca".to_string(), "2 sessions on orca".to_string()));
        // Accounts that differ, or aren't known, are left out.
        m.sessions[0].account = Some("default".into());
        assert_eq!(m.sub(), "2 sessions");
        m.sessions[0].account = None;
        assert_eq!(m.sub(), "2 sessions");
        m.sessions.truncate(1);
        m.sessions[0].account = Some("orca".into());
        assert_eq!((m.sub(), m.own_tower()), ("1 session on orca".to_string(), true), "its tower is what the daemon says, not one of its sessions");
        // How its own tower stands; one that is due and doesn't run, or none at all, is the main tower's.
        for (status, line) in [("working", Some("its own tower")), ("starting", Some("its own tower is starting")), ("blocked", Some("its own tower waits on its own screen")), ("off", None), ("", None)] {
            m.tower = Some(OwnTower { name: "tower-sandbox".into(), status: status.into() });
            assert_eq!((m.tower_line(), m.own_tower()), (line, line.is_some()), "{status}");
        }
        m.tower = None;
        assert_eq!((m.tower_line(), m.own_tower()), (None, false));
        // Standing, then ready to close (by the state's word, or by its report).
        m.standing = true;
        assert_eq!(m.side_line(), "standing · 1 session on orca");
        m.standing = false;
        m.state = Phase::Ready;
        assert_eq!((m.ready(), m.side_line()), (true, "ready to close".to_string()));
        m.state = Phase::Open;
        assert!(!m.ready());
        m.report = "Released.".into();
        assert_eq!(m.side_line(), "ready to close");
        // The mark: what needs you, muted or not, then work, then rest.
        m.needs = 2;
        assert_eq!(m.chip(), Chip::Yours(2));
        m.mute = true;
        assert_eq!(m.chip(), Chip::Muted(2));
        m.needs = 0;
        m.at_work = true;
        assert_eq!(m.chip(), Chip::Working);
        m.at_work = false;
        assert_eq!(m.chip(), Chip::Done);
        assert!(is_tower("tower") && is_tower("tower-m5-pro") && !is_tower("towering") && !is_tower("herder-tower"));
    }

    #[test]
    fn a_mission_says_how_far_it_is() {
        let mut m = Mission {
            name: "isa".into(),
            goal: "Voice clips for the Ingest tour, live".into(),
            sessions: vec![member("eyzflow", "idle", None), member("mediaroom", "working", None)],
            ..Default::default()
        };
        // Nothing says: nothing is drawn.
        assert_eq!(m.progress(), (String::new(), false));
        m.at_work = true;
        assert_eq!(m.progress(), ("working".to_string(), false));
        assert_eq!((m.about(), m.meta(0)), ("Voice clips for the Ingest tour, live".to_string(), "2 sessions".to_string()));
        // Its requests: the first with something left.
        m.requests = vec![request("Record the clips", "done", 3, 3), request("Voice clips for the Ingest tour", "working", 5, 6), request("Later", "open", 0, 1)];
        assert_eq!(m.current().unwrap().title, "Voice clips for the Ingest tour");
        assert_eq!(m.progress(), ("5 of 6".to_string(), false));
        assert_eq!(m.requests.iter().map(|r| r.progress()).collect::<Vec<_>>(), ["done", "5 of 6", "0 of 1"]);
        assert_eq!(request("No task yet", "open", 0, 0).progress(), "open");
        // All done: the last one, and "done".
        m.requests.truncate(1);
        assert_eq!((m.current().unwrap().title.as_str(), m.progress().0.as_str()), ("Record the clips", "done"));
        // A standing space is about what it is asked now, not a goal.
        m.standing = true;
        assert_eq!(m.about(), "Record the clips");
        m.standing = false;
        m.goal.clear();
        assert_eq!(m.about(), "Record the clips", "no goal: the same");
        m.requests.clear();
        assert_eq!(m.about(), "");
        // Ready wins, and is marked.
        m.report = "Released at 14:55.".into();
        assert_eq!(m.progress(), ("ready to close".to_string(), true));
        // Worktrees and messages show when there are any.
        m.sessions[0].worktree = Some("eyzflow@isa".into());
        m.sessions[0].port = Some(3013);
        assert_eq!(m.meta(1), "2 sessions · own worktrees · 1 message between them");
        m.thread_count = 33;
        assert_eq!(m.meta(5), "2 sessions · own worktrees · 33 messages between them");
        assert_eq!(m.sessions[0].place(), "worktree eyzflow@isa · port 3013");
        assert_eq!(m.sessions[1].place(), "");
        m.sessions[1].port = Some(3014);
        assert_eq!(m.sessions[1].place(), "port 3014");
    }

    #[test]
    fn a_switch_set_here_counts_until_the_state_agrees() {
        let state = json!({ "missions": [{ "name": "sandbox", "state": "open" }, { "name": "isa", "state": "open", "follow": true, "mute": true }] });
        let all = vec![decision("db", "sandbox", 3, 0), decision("pr", "isa", 0, 0)];
        let mut marks: HashMap<String, Marks> = HashMap::new();
        let number = |marks: &HashMap<String, Marks>, state: &Value, mutes: &[String]| count(&all, &muted_spaces(&all, state, mutes, marks));
        assert_eq!(number(&marks, &state, &[]), 1, "isa is muted by the state");
        // Muted here: off the count at once.
        marks.insert("sandbox".into(), Marks { follow: Some(true), mute: Some(true), review: None });
        assert_eq!(number(&marks, &state, &[]), 0);
        let mut m = missions(&state, &Spaces::default(), &[], &[], &all);
        m.mark(&[], &muted_spaces(&all, &state, &[], &marks), &marks);
        assert_eq!((m.open[0].follow, m.open[0].mute, m.open[0].chip()), (true, true, Chip::Muted(1)));
        assert!(main_queue(&all, &m.muted()).is_empty());
        // Unmuted here, before the state says so.
        marks.insert("isa".into(), Marks { follow: None, mute: Some(false), review: None });
        assert_eq!(number(&marks, &state, &[]), 1);
        // The state doesn't agree yet: both stay.
        settle_marks(&mut marks, &state, &all, &[], &[]);
        assert_eq!(marks.len(), 2);
        // It says the same: forgotten, one switch at a time.
        let agreed = json!({ "missions": [{ "name": "sandbox", "state": "open", "mute": true }, { "name": "isa", "state": "open", "follow": true }] });
        settle_marks(&mut marks, &agreed, &all, &[], &[]);
        assert_eq!(marks.get("sandbox"), Some(&Marks { follow: Some(true), mute: None, review: None }));
        assert!(!marks.contains_key("isa"));
        // Followed in spaces.json (any case): that settles it too, and marks a mission.
        settle_marks(&mut marks, &agreed, &all, &["SANDBOX".to_string()], &[]);
        assert!(marks.is_empty());
        let mut m = missions(&agreed, &Spaces::default(), &[], &[], &[]);
        m.mark(&["Sandbox".to_string()], &muted_spaces(&[], &agreed, &[], &marks), &marks);
        assert!(m.open[0].follow && m.open[1].follow && m.open[0].mute && !m.open[1].mute);
    }

    #[test]
    fn a_space_is_muted_by_whoever_says_so() {
        let all = vec![decision("db", "Sandbox", 3, 0), decision("pr", "isa", 0, 0), Decision { muted: true, ..decision("pick", "landing", 1, 0) }];
        let none = HashMap::new();
        // A state without missions: the decision's own word, and the list in spaces.json (any case).
        assert_eq!(muted_spaces(&all, &json!({}), &[], &none), ["landing".to_string()].into());
        let muted = muted_spaces(&all, &json!({}), &["sandbox".to_string()], &none);
        assert_eq!(muted, ["landing".to_string(), "Sandbox".to_string()].into(), "spelt as the decisions spell it");
        assert_eq!((ids(&main_queue(&all, &muted)), count(&all, &muted)), (vec!["pr"], 1));
        // Unmuted here: back in the queue before the daemon says so.
        let mut marks: HashMap<String, Marks> = [("landing".to_string(), Marks { follow: None, mute: Some(false), review: None })].into();
        assert_eq!(count(&all, &muted_spaces(&all, &json!({}), &["sandbox".to_string()], &marks)), 2);
        // The daemon still says muted: the switch stays; once it doesn't, it is forgotten.
        settle_marks(&mut marks, &json!({}), &all, &[], &[]);
        assert_eq!(marks.len(), 1);
        settle_marks(&mut marks, &json!({}), &all[..2], &[], &[]);
        assert!(marks.is_empty());
        // Muted here, then listed in spaces.json: forgotten too.
        let mut marks: HashMap<String, Marks> = [("isa".to_string(), Marks { follow: Some(false), mute: Some(true), review: None })].into();
        settle_marks(&mut marks, &json!({}), &all, &[], &[]);
        assert_eq!(marks.get("isa"), Some(&Marks { follow: None, mute: Some(true), review: None }), "not followed anywhere: that much is settled");
        settle_marks(&mut marks, &json!({}), &all, &[], &["ISA".to_string()]);
        assert!(marks.is_empty());
    }

    #[test]
    fn the_thread_and_the_tell_level_read_from_the_state() {
        let state = json!({
            "tell_level": "results",
            "threads": { "sandbox": [
                { "at": 5, "from": "t2", "from_name": "mediaroom", "to": "t1", "to_name": "eyzflow", "text": "Answers on stale writes.", "space": "sandbox", "delivered": true },
                { "at": 6, "from": "t3", "from_name": null, "to": "t1", "text": "Four changes.", "delivered": false },
                { "at": 7, "from": "t3", "to": "t1", "text": "  " },
                "nonsense",
            ] },
        });
        let t = thread(&state, "sandbox");
        assert_eq!(t.iter().map(|m| (m.who_in("sandbox"), m.text.as_str(), m.delivered)).collect::<Vec<_>>(), [
            ("mediaroom → eyzflow".to_string(), "Answers on stale writes.", true),
            ("t3 → t1".to_string(), "Four changes.", false),
        ]);
        assert!(thread(&state, "isa").is_empty() && thread(&json!({}), "sandbox").is_empty());
        // In its space's own view a session named after the space goes by the rest of its name.
        let m = Message { from: "t2".into(), from_name: "fix login eyzflow".into(), to: "t3".into(), to_name: "fix login mediaroom".into(), ..Default::default() };
        assert_eq!((m.who_in(""), m.who_in("fix-login"), m.who_in("Fix_Login")), ("fix login eyzflow → fix login mediaroom".to_string(), "eyzflow → mediaroom".to_string(), "eyzflow → mediaroom".to_string()));
        assert_eq!(m.who_in("isa"), m.who_in(""));
        let own = Message { from_name: "fix login".into(), to_name: "fix".into(), ..Default::default() };
        assert_eq!(own.who_in("fix-login"), "fix login → fix", "a name that is only the space's stays");
        assert_eq!(tell_level(&state), Some("results"));
        assert_eq!((tell_level(&json!({})), tell_level(&json!({ "tell_level": "loud" }))), (None, None));
    }

    #[test]
    fn the_open_plan_reads_from_the_state() {
        let state = json!({ "plan": {
            "id": "p2", "form": "whiteboard", "status": "ready", "ticket": "EYZFlow Support – Translate messages",
            "draft": { "space": "msg-translation", "sessions": [{ "repo": "/w/a" }] }, "revision": 1,
            "address": "t9", "planner": "idle", "blocked": false, "answering": true, "url": "https://claude.ai/artifact/wb", "why": "",
            "thread": [{ "from": "user", "text": "which session starts first?", "at": 5 }, { "from": "planner", "text": " ", "at": 6 }, { "from": null, "text": "x" }],
            "created_ms": 1, "ready_ms": 4, "some_newer_field": true,
        } });
        let p = plan(&state).unwrap();
        assert_eq!((p.id.as_str(), p.form_word(), p.status.as_str(), p.url.as_deref(), p.revision, p.answering), ("p2", "whiteboard", "ready", Some("https://claude.ai/artifact/wb"), 1, true));
        assert_eq!(p.draft["space"], "msg-translation");
        assert_eq!(p.thread.iter().map(|m| (m.mine(), m.text.as_str())).collect::<Vec<_>>(), [(true, "which session starts first?"), (false, "x")], "an empty line goes");
        // Blocked on a dialog in its terminal; a page; none; a closed one; a state from before plans.
        let mut blocked = state.clone();
        blocked["plan"]["blocked"] = true.into();
        blocked["plan"]["form"] = "page".into();
        let p = plan(&blocked).unwrap();
        assert!(p.blocked && p.form_word() == "page");
        assert!(plan(&json!({ "plan": null })).is_none() && plan(&json!({})).is_none());
        let mut closed = state.clone();
        closed["plan"]["status"] = "closed".into();
        assert!(plan(&closed).is_none());
    }

    #[test]
    fn the_same_question_has_the_same_key() {
        assert_eq!(normalise("  Which  database — should we USE? "), "which database should we use");
        assert_eq!(normalise("Which database should we use"), normalise("which database, should we use?!"));
    }

    #[test]
    fn a_blocked_card_and_one_of_a_kind_not_known_here_both_count() {
        let state = json!({ "decisions": [
            { "id": "d000000000001", "key": "blocked:shop:t4", "kind": "blocked", "space": "Shop", "sessions": [{ "address": "t4", "name": "api", "waiting": true }],
              "waiting": 1, "title": "Asks to use Frobnicate", "why": "blocked", "options": [], "first_ms": 1, "last_ms": 1, "count": 1, "state": "open", "muted": false },
            { "id": "d000000000002", "key": "x:shop:t5", "kind": "something-newer", "space": "Shop", "sessions": [{ "address": "t5", "name": "web", "waiting": true }],
              "waiting": 1, "title": "A newer tower's card", "why": "", "options": [], "first_ms": 2, "last_ms": 2, "count": 1, "state": "open", "muted": false }
        ] });
        let list = from_state(&state).unwrap();
        assert_eq!(list.iter().map(|d| d.kind).collect::<Vec<_>>(), [Kind::Blocked, Kind::Other], "neither is left out");
        assert_eq!(count(&list, &HashSet::new()), 2, "both on the orb's number");
        assert_eq!((list[0].badge(), list[0].reason()), ("In its terminal".to_string(), None));
        assert!(rule_offer(&list[0], "yes").is_none() && rule_offer(&list[1], "yes").is_none());
    }

    #[test]
    fn the_window_reads_a_spaces_track_from_the_state() {
        use crate::releases::tests::{LocalGit, MIN, pr, rel, scratch};
        use crate::releases::{track, when};
        let (dir, [m829, r1, m830, _]) = scratch();
        let now = 1_794_560_000_000 + 120 * MIN;
        let prs = vec![pr(829, "dev", "merged", &m829, Some(now - 90 * MIN)), pr(830, "dev", "merged", &m830, Some(now - 60 * MIN))];
        let t = track("checkout", &prs, &[rel(840, &r1, "merged", Some(now - 80 * MIN))], &std::collections::BTreeMap::new(), &LocalGit(dir), now);
        let state = serde_json::json!({ "releases": { "checkout": t.view(now) } });
        // By any case of its name; a space without PRs has none.
        let back = release_track(&state, "Checkout").unwrap();
        assert_eq!(back, t, "the view reads back as the track");
        assert_eq!(back.repos[0].lines(now), ["On main since ".to_string() + &when(now - 80 * MIN, now) + ": #829", "On dev, not on main: #830".into()]);
        assert_eq!(back.summary(now), format!("prod {} · 1 on dev", when(now - 80 * MIN, now)));
        assert!(release_track(&state, "elsewhere").is_none());
        assert!(release_track(&serde_json::json!({}), "checkout").is_none(), "an older daemon's state");
    }
}
