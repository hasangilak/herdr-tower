//! A space's goal follows the user's requests. A space is made for a goal
//! (its agenda in `spaces.json`, with what counts as done), but while the
//! user works there they ask for more, and a goal left as first written
//! stops saying what the space is for. So each request of the user's in a
//! space is weighed against its goal:
//!
//! - **What counts** (`daemon::goals`): what the user asks of a space's own
//!   tower, or of the main one about a space (`tower ask --space`, the
//!   space's view, the phone), and what the tower hands out into a space
//!   for a request of theirs (`tower dispatch`, `queue`, `start`). Each
//!   request once per space. A bare reply ("yes", "2") or a few words isn't
//!   weighed (`worth_weighing`).
//! - **Which spaces**: one with a goal, not standing, not closed. In a
//!   space opened by a chat (`Mission::chat`), not once the user set the
//!   goal by hand: their goal stays as they wrote it.
//! - **A space with no goal** that isn't standing (one opened by a chat
//!   with the tower: `tower start --space`/`--new-space`) gets its first
//!   goal from the request (`first_prompt`): the goal and what counts as
//!   done, as a launch's ticket would say them. A request that only sets
//!   the space up ("open a space with my second account") gives none yet:
//!   the next one is weighed for it. The space is then a chat's
//!   (`missions::first_goal_from_requests`): what that does to it is in
//!   `missions`.
//! - **Backfill** (`backfill`, `tower space goal-backfill <space>|all
//!   [--dry-run]`): an open space with no goal gets its first one from the
//!   user's requests there so far (the ledger's, oldest first).
//! - **One headless call** (as a ticket is drafted: `covered::ask`, the
//!   default account, `[brains] model` at low effort, no tools, nothing
//!   kept) with the space, its goal and done-when, how the goal came to be
//!   (`Mission::goals`) and the request (`prompt`). It answers JSON only:
//!   `{"changes": false, "why": "…"}` for a step toward the goal as it is,
//!   or `{"changes": true, "goal": "…", "done_when": "…", "why": "…"}`: a
//!   short rewrite that keeps what the space was for and adds the new
//!   scope, and what counts as done with it. Unsure: no change.
//! - **Checked** (`parse`): a goal of one line of at most `GOAL_CHARS`, a
//!   done-when of at most `DONE_CHARS`; one that is the goal as it was is no change.
//! - **Applied** (`missions::goal_from_request`) only when the goal is
//!   still the one it was weighed on: the user's own edit by hand (`tower
//!   space goal --by you`, the Goal card, the phone) wins over an answer
//!   that comes after it. A ready space is open again, and only what is
//!   finished after the change makes it ready (its ready and closing look
//!   at the goal now). The change is kept in the goal's history with the
//!   request that made it.
//! - `tower space goal-request <space> "<words>" [--dry-run]` weighs words as a
//!   request there by hand (`--dry-run`: says what it would do, changes nothing).

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::missions::{self, Change, GoalChange};
use crate::spaces::{self, Spaces};

/// The goal: one line of at most this many characters.
pub const GOAL_CHARS: usize = 200;
/// What counts as done: at most this many.
pub const DONE_CHARS: usize = 600;
/// A request is cut to this in the prompt and in the goal's history.
const REQUEST_CHARS: usize = 1500;

/// Whether `words` could change what a space is for: not a bare reply, and
/// more than a couple of words.
pub fn worth_weighing(words: &str) -> bool {
    let Some(task) = crate::introduce::users_task(words) else { return false };
    task.split_whitespace().count() >= 3
}

/// The request as the user said it: without the "In <space>: " the tower
/// puts before a request it hands the main brain about a space.
pub fn request_words(space: &str, words: &str) -> String {
    let w = words.trim();
    let lead = format!("In {}:", space.trim());
    match w.get(..lead.len()) {
        Some(head) if head.eq_ignore_ascii_case(&lead) => w[lead.len()..].trim().to_string(),
        _ => w.to_string(),
    }
}

/// What a space's goal is weighed on, if it is one whose goal follows the
/// requests (a goal, not standing, not closed).
pub struct On {
    pub name: String,
    pub goal: String,
    pub done_when: String,
    pub history: Vec<GoalChange>,
    /// When the goal was last set other than by a request (launched, by
    /// hand, by the brain): a request heard before that is in it already.
    pub set_ms: u64,
}

pub fn weighed_on(s: &Spaces, space: &str) -> Option<On> {
    let (name, m) = missions::of(s, space)?;
    let goal = s.agendas.get(name).map(|g| g.trim().to_string()).filter(|g| !g.is_empty())?;
    if m.standing || m.state == missions::State::Closed || m.archived_ms.is_some() {
        return None;
    }
    let set = m.goals.iter().rev().find(|c| c.by != "request");
    // A chat's space whose goal the user set by hand: it stays as they wrote it.
    if m.chat && set.is_some_and(|c| c.by == "you") {
        return None;
    }
    // A chat's first goal was written from a request: those heard before it are in it.
    let first = m.goals.first().filter(|_| m.chat).map(|c| c.at_ms);
    let set_ms = set.map(|c| c.at_ms).or(first).unwrap_or(m.goal_ms);
    Some(On { name: name.clone(), goal, done_when: m.done_when.trim().to_string(), history: m.goals.clone(), set_ms })
}

/// The prompt for the headless call.
pub fn prompt(space: &str, goal: &str, done_when: &str, history: &[GoalChange], request: &str) -> String {
    let mut p = String::from(
        "You keep the goal of a workspace (a \"space\") up to date with what the user asks for there. \
         The user is a developer; a space is one piece of work, with a goal and what counts as done. \
         While they work in it they ask for more. Decide whether this new request changes what the \
         space is for.\n\n\
         - It CHANGES it when it adds scope the goal doesn't cover (a new feature, another part of the \
         product, a new deliverable, a new requirement or constraint), or changes the goal's direction.\n\
         - It does NOT change it when it is a step toward the goal as it stands: fixing a bug in that \
         work, a test, a review comment, a refactor, a rename, a question, a status check, telling a \
         session to go on, commit or open the PR, or anything the goal and its done-when already cover. \
         When unsure, it does not change it.\n\n\
         When it changes it, rewrite the goal: one short line (at most 200 characters) that keeps what \
         the space was first for and adds the new scope, in the user's own terms; and what counts as \
         done, the conditions as they were plus what the request adds, joined with \"; \" (at most 600 \
         characters). Keep the words of a goal the user set by hand, and only add to it.\n\n\
         Answer with JSON only, no prose around it:\n\
         {\"changes\": false, \"why\": \"<a few words: why it is a step>\"}\n\
         or\n\
         {\"changes\": true, \"goal\": \"<the new goal>\", \"done_when\": \"<what counts as done now>\", \"why\": \"<a few words: what it adds>\"}\n\n",
    );
    p += &format!("## The space\n\n{space}\n\n## Its goal now\n\n{goal}\n\n## What counts as done now\n\n{}\n\n", if done_when.is_empty() { "(not said)" } else { done_when });
    let earlier: Vec<&GoalChange> = history.iter().rev().skip(1).take(5).collect();
    if !earlier.is_empty() {
        p += "## How the goal came to be (newest first)\n\n";
        for c in earlier {
            let who = match c.by.as_str() {
                "you" => "set by the user by hand".to_string(),
                "request" => format!("after the request \"{}\"", crate::launch::clip_words(&c.request, 160)),
                "launch" => "the space was made with it".to_string(),
                _ => "set".to_string(),
            };
            p += &format!("- {} ({who})\n", c.goal);
        }
        if history.last().is_some_and(|c| c.by == "you") {
            p += "\nThe goal now was set by the user by hand: keep its words.\n";
        }
        p += "\n";
    }
    p += &format!("## The new request\n\n{}\n", crate::launch::clip_words(request.trim(), REQUEST_CHARS));
    p
}

/// The prompt for a space's first goal: the user's `requests` there
/// (oldest first), and what its sessions said to each other (`notes`:
/// context, not requests).
pub fn first_prompt(space: &str, requests: &[String], notes: &[String]) -> String {
    let mut p = String::from(
        "You write the goal of a workspace (a \"space\") from what the user asked for there. The user is a \
         developer; a space is one piece of work, with a goal and what counts as done. They opened this \
         one by asking a tower (an assistant that opens coding sessions) for it, with no goal written down.\n\n\
         Write, in the user's own terms:\n\
         - the goal: one short line (at most 200 characters) saying what the work in this space is for: \
         the outcome, not the steps;\n\
         - what counts as done: two to four outcomes that would show the goal is met, joined with \"; \" \
         (at most 400 characters). Outcomes, not the steps on the way: not one-off instructions (merge \
         this PR, wait for that, keep a switch off for now). Only what the requests say or clearly imply; \
         keep a number the user gave, never invent one.\n\n\
         Requests that only set the space up (open a space, open sessions, which account, which folder) \
         or only steer the sessions (go on, wait, merge, deploy) say no goal by themselves. The user may \
         also have asked the sessions directly: what the sessions' notes relay as the user's own ask \
         (\"the user asked …\", \"new from the user …\") counts as their request. When neither names \
         work the space is for, write no goal: an empty one.\n\n\
         Answer with JSON only, no prose around it:\n\
         {\"goal\": \"<the goal>\", \"done_when\": \"<what counts as done>\", \"why\": \"<a few words: which request it comes from>\"}\n\
         or\n\
         {\"goal\": \"\", \"why\": \"<a few words: why there is none yet>\"}\n\n",
    );
    p += &format!("## The space\n\n{space}\n\n## The user's requests there, oldest first\n\n");
    let mut left = REQUEST_CHARS * 4;
    for r in requests {
        let r = crate::launch::clip_words(&r.split_whitespace().collect::<Vec<_>>().join(" "), 600);
        if left < r.len() {
            break;
        }
        left -= r.len();
        p += &format!("- {r}\n");
    }
    if !notes.is_empty() {
        p += "\n## What its sessions told each other lately (context only: not the user's requests)\n\n";
        for n in notes {
            p += &format!("- {}\n", crate::launch::clip_words(&n.split_whitespace().collect::<Vec<_>>().join(" "), 300));
        }
    }
    p
}

/// The first goal the call wrote (goal, done-when, why), checked; `None`
/// with why when it wrote none.
pub fn parse_first(stdout: &str) -> Result<std::result::Result<(String, String, String), String>> {
    let v = crate::ticket::parse_reply(stdout)?;
    let line = |k: &str, max: usize| crate::launch::clip_words(&v[k].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" "), max);
    let why = line("why", 160);
    let goal = line("goal", GOAL_CHARS);
    if goal.is_empty() {
        return Ok(Err(why));
    }
    let mut done = v["done_when"].as_str().unwrap_or("").trim().to_string();
    if done.chars().count() > DONE_CHARS {
        done = crate::launch::clip_words(&done, DONE_CHARS);
    }
    Ok(Ok((goal, done, why)))
}

/// What the call said, checked.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// A step toward the goal as it is (why).
    Step(String),
    /// The goal and done-when it should have now, and why.
    Grows { goal: String, done_when: String, why: String },
}

/// The answer (`claude -p --output-format json`'s, or the bare JSON),
/// checked against the goal it was asked about. Errors say why.
pub fn parse(stdout: &str, goal: &str, done_when: &str) -> Result<Verdict> {
    let v = crate::ticket::parse_reply(stdout)?;
    let line = |k: &str, max: usize| crate::launch::clip_words(&v[k].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" "), max);
    let why = line("why", 160);
    let changes = match &v["changes"] {
        Value::Bool(b) => *b,
        Value::String(s) => s.trim().eq_ignore_ascii_case("true"),
        _ => bail!("the answer says neither changes: true nor false"),
    };
    if !changes {
        return Ok(Verdict::Step(why));
    }
    let new_goal = line("goal", GOAL_CHARS);
    if new_goal.is_empty() {
        bail!("the answer changes the goal but gives none");
    }
    let mut new_done = v["done_when"].as_str().unwrap_or("").trim().to_string();
    if new_done.chars().count() > DONE_CHARS {
        new_done = crate::launch::clip_words(&new_done, DONE_CHARS);
    }
    if new_done.is_empty() {
        new_done = done_when.trim().to_string();
    }
    if new_goal == goal.trim() && new_done == done_when.trim() {
        return Ok(Verdict::Step(why));
    }
    Ok(Verdict::Grows { goal: new_goal, done_when: new_done, why })
}

/// What came of a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The space's goal doesn't follow requests (none, standing, closed), or the words weren't weighed.
    NotWeighed(String),
    /// A step toward the goal: it stays (why).
    Unchanged(String),
    /// The goal changed: its new version.
    Updated(GoalChange),
    /// A space with no goal got its first one, from the user's requests.
    Written(GoalChange),
    /// A space with no goal: the requests name no work yet (why).
    NoGoalYet(String),
    /// It would have changed, but the goal had changed meanwhile (the user's own edit wins).
    Overtaken,
    /// `--dry-run`: what it would be.
    WouldBe { goal: String, done_when: String, why: String },
}

impl Outcome {
    pub fn json(&self, space: &str) -> Value {
        match self {
            Outcome::NotWeighed(why) => json!({ "space": space, "outcome": "not-weighed", "why": why }),
            Outcome::Unchanged(why) => json!({ "space": space, "outcome": "unchanged", "why": why }),
            Outcome::Updated(c) => json!({ "space": space, "outcome": "updated", "goal": c.goal, "done_when": c.done_when, "why": c.why, "at_ms": c.at_ms }),
            Outcome::Written(c) => json!({ "space": space, "outcome": "written", "goal": c.goal, "done_when": c.done_when, "why": c.why, "at_ms": c.at_ms }),
            Outcome::NoGoalYet(why) => json!({ "space": space, "outcome": "no-goal-yet", "why": why }),
            Outcome::Overtaken => json!({ "space": space, "outcome": "overtaken", "why": "the goal changed while the request was weighed: it stays as it is now" }),
            Outcome::WouldBe { goal, done_when, why } => json!({ "space": space, "outcome": "would-update", "goal": goal, "done_when": done_when, "why": why }),
        }
    }
}

/// Weighs `words`, a request of the user's in `space` (heard at
/// `heard_ms`, when known), against its goal in the `spaces.json` of
/// `ui_dir`, with `ask` as the headless call; changes the goal when it
/// grows (not with `dry_run`). A request heard before the goal was last set
/// is in it already (the space was launched for it, or the user set the
/// goal by hand after it).
pub fn follow_with(ui_dir: &std::path::Path, space: &str, words: &str, heard_ms: Option<u64>, dry_run: bool, ask: &dyn Fn(&str) -> Result<String>) -> Result<Outcome> {
    let words = request_words(space, words);
    if !worth_weighing(&words) {
        return Ok(Outcome::NotWeighed("a reply or a few words: no request to weigh".into()));
    }
    let s = spaces::load_from(ui_dir);
    let Some(On { name, goal, done_when, history, set_ms }) = weighed_on(&s, space) else {
        // No goal yet: this request may say what the space is for.
        if missions::writes_first_goal(&s, space).is_some() {
            return write_first(ui_dir, space, &[words], &[], dry_run, ask);
        }
        return Ok(Outcome::NotWeighed("its goal doesn't follow requests: it is standing, closed, set by the user by hand, or cleared by them".into()));
    };
    if heard_ms.is_some_and(|at| at > 0 && at <= set_ms) {
        return Ok(Outcome::NotWeighed("the goal was set after it was asked: it is in the goal already".into()));
    }
    let reply = ask(&prompt(&name, &goal, &done_when, &history, &words))?;
    match parse(&reply, &goal, &done_when)? {
        Verdict::Step(why) => Ok(Outcome::Unchanged(why)),
        Verdict::Grows { goal: g, done_when: d, why } if dry_run => Ok(Outcome::WouldBe { goal: g, done_when: d, why }),
        Verdict::Grows { goal: g, done_when: d, why } => {
            let change = Change { by: "request".into(), request: crate::launch::clip_words(&words, REQUEST_CHARS), why };
            let now = crate::history::now_ms();
            let done = spaces::update_in(ui_dir, |s| missions::goal_from_request(s, &name, (&goal, &done_when), &g, &d, &change, now));
            Ok(match done {
                Some(c) => Outcome::Updated(c),
                None => Outcome::Overtaken,
            })
        }
    }
}

/// A space with no goal (`missions::writes_first_goal`) gets its first one
/// from the user's `requests` there (oldest first; `notes`: what its
/// sessions said, as context), unless `dry_run`.
fn write_first(ui_dir: &std::path::Path, space: &str, requests: &[String], notes: &[String], dry_run: bool, ask: &dyn Fn(&str) -> Result<String>) -> Result<Outcome> {
    let name = missions::writes_first_goal(&spaces::load_from(ui_dir), space).unwrap_or_else(|| space.trim().to_string());
    let (goal, done_when, why) = match parse_first(&ask(&first_prompt(&name, requests, notes))?)? {
        Ok(g) => g,
        Err(why) => return Ok(Outcome::NoGoalYet(why)),
    };
    if dry_run {
        return Ok(Outcome::WouldBe { goal, done_when, why });
    }
    let request = crate::launch::clip_words(&requests.iter().map(|r| r.trim()).collect::<Vec<_>>().join(" · "), REQUEST_CHARS);
    let change = Change { by: "request".into(), request, why };
    let now = crate::history::now_ms();
    let done = spaces::update_in(ui_dir, |s| missions::first_goal_from_requests(s, &name, &goal, &done_when, &change, now));
    Ok(match done {
        Some(c) => Outcome::Written(c),
        None => Outcome::Overtaken,
    })
}

/// The user's requests in `space` so far, oldest first (asked of the tower,
/// or typed in a session's terminal there), and the latest words its
/// sessions sent each other through the tower (context), from `ledger`.
pub fn requests_so_far(ledger: &crate::agenda::Ledger, space: &str) -> (Vec<String>, Vec<String>) {
    let here = |a: &crate::agenda::Agenda| a.spaces.iter().chain(a.tasks.iter().filter_map(|t| t.space.as_ref())).any(|s| s.trim().eq_ignore_ascii_case(space.trim()));
    let mut asked: Vec<(u64, String)> = vec![];
    let mut notes: Vec<(u64, String)> = vec![];
    for a in ledger.agendas.iter().filter(|a| here(a)) {
        let words = a.words.trim();
        if words.is_empty() {
            continue;
        }
        match a.origin.as_str() {
            "user" | "terminal" if worth_weighing(words) => {
                if !asked.iter().any(|(_, w)| w == words) {
                    asked.push((a.request_ms.max(a.created_ms), words.to_string()));
                }
            }
            "tower" => notes.push((a.created_ms, words.to_string())),
            _ => {}
        }
    }
    asked.sort_by_key(|(at, _)| *at);
    notes.sort_by_key(|(at, _)| *at);
    // The latest, and those that relay what the user asked the sessions first.
    let relayed = |w: &str| w.to_lowercase().contains("the user");
    let mut notes: Vec<(u64, String)> = notes.iter().rev().filter(|(_, w)| relayed(w)).take(8).chain(notes.iter().rev().filter(|(_, w)| !relayed(w)).take(2)).cloned().collect();
    notes.sort_by_key(|(at, _)| *at);
    let notes = notes.into_iter().map(|(_, w)| w).collect();
    (asked.into_iter().map(|(_, w)| w).collect(), notes)
}

/// Backfill: each open space with no goal (`missions::writes_first_goal`;
/// only `only`, when given) that has requests of the user's gets its first
/// goal from them (`dry_run`: says what it would be, changes nothing).
pub fn backfill_with(ui_dir: &std::path::Path, ledger: &crate::agenda::Ledger, only: Option<&str>, dry_run: bool, ask: &dyn Fn(&str) -> Result<String>) -> Vec<(String, Result<Outcome>)> {
    let s = spaces::load_from(ui_dir);
    let mut names: Vec<String> = match only {
        Some(one) => vec![one.trim().to_string()],
        None => s.missions.iter().filter(|(_, m)| m.state != missions::State::Closed && m.archived_ms.is_none()).map(|(k, _)| k.clone()).collect(),
    };
    names.dedup();
    let mut out = vec![];
    for name in names {
        if missions::writes_first_goal(&s, &name).is_none() {
            let why = if missions::of(&s, &name).is_some_and(|(_, m)| m.standing) { "it is standing: no goal unless you set one" } else { "it has a goal already, or is closed, launched, or cleared by hand" };
            out.push((name, Ok(Outcome::NotWeighed(why.into()))));
            continue;
        }
        let (asked, notes) = requests_so_far(ledger, &name);
        if asked.is_empty() {
            out.push((name, Ok(Outcome::NoGoalYet("no request of yours there yet".into()))));
            continue;
        }
        let r = write_first(ui_dir, &name, &asked, &notes, dry_run, ask);
        out.push((name, r));
    }
    out
}

/// `tower space goal-backfill <space>|all [--dry-run]`.
pub fn cmd_backfill(space: &str, dry_run: bool) -> Result<()> {
    let only = match space.trim() {
        "" | "all" | "--all" => None,
        one => Some(spaces::pick(one)?),
    };
    let ledger = crate::agenda::Ledger::load_from(&crate::agenda::path());
    let out = backfill_with(&crate::config::ui_dir(), &ledger, only.as_deref(), dry_run, &|p| crate::covered::ask(p));
    if out.iter().any(|(_, r)| matches!(r, Ok(Outcome::Written(_)))) {
        missions::poke();
    }
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let mut rows = vec![];
    for (name, r) in &out {
        let v = match r {
            Ok(o) => o.json(name),
            Err(e) => json!({ "space": name, "outcome": "error", "why": format!("{e:#}") }),
        };
        if tty {
            let why = v["why"].as_str().unwrap_or("").trim_end_matches('.');
            match r {
                Ok(Outcome::Written(c)) => println!("{name}: goal written: {}. Done when: {}. ({why})", c.goal.trim_end_matches('.'), c.done_when.trim_end_matches('.')),
                Ok(Outcome::WouldBe { goal, done_when, .. }) => println!("{name}: it would get: {}. Done when: {}. ({why})", goal.trim_end_matches('.'), done_when.trim_end_matches('.')),
                Ok(Outcome::NoGoalYet(_)) => println!("{name}: no goal yet ({why})."),
                Ok(Outcome::Overtaken) => println!("{name}: it got a goal meanwhile; left as it is."),
                Ok(_) => println!("{name}: left alone ({why})."),
                Err(_) => println!("{name}: failed ({why})."),
            }
        }
        rows.push(v);
    }
    if !tty {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    }
    Ok(())
}

/// The same, with the real call, in the tower's own `spaces.json`.
pub fn follow(space: &str, words: &str, heard_ms: Option<u64>, dry_run: bool) -> Result<Outcome> {
    follow_with(&crate::config::ui_dir(), space, words, heard_ms, dry_run, &|p| crate::covered::ask(p))
}

/// `tower space goal-request <space> "<words>" [--dry-run]`.
pub fn cmd_request(space: &str, words: &str, dry_run: bool) -> Result<()> {
    if words.trim().is_empty() {
        bail!("tower space goal-request <space> \"<the user's words>\" [--dry-run]");
    }
    let name = spaces::pick(space)?;
    let out = follow(&name, words, None, dry_run)?;
    if matches!(out, Outcome::Updated(_) | Outcome::Written(_)) {
        missions::poke();
    }
    let v = out.json(&name);
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        match &out {
            Outcome::NotWeighed(why) => println!("{name}: not weighed ({why})."),
            Outcome::Unchanged(why) => println!("{name}: the goal stays ({}).", why.trim_end_matches('.')),
            Outcome::Updated(c) => println!("{name}: goal updated: {}. Done when: {}.", c.goal.trim_end_matches('.'), c.done_when.trim_end_matches('.')),
            Outcome::Written(c) => println!("{name}: goal written: {}. Done when: {}.", c.goal.trim_end_matches('.'), c.done_when.trim_end_matches('.')),
            Outcome::NoGoalYet(why) => println!("{name}: no goal yet ({}).", why.trim_end_matches('.')),
            Outcome::Overtaken => println!("{name}: the goal changed meanwhile; it stays as it is now."),
            Outcome::WouldBe { goal, done_when, .. } => println!("{name}: it would become: {}. Done when: {}.", goal.trim_end_matches('.'), done_when.trim_end_matches('.')),
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&v)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::missions::{Mission, State};

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tower-goals-{tag}-{}-{}", std::process::id(), crate::history::now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn with_space(ui: &std::path::Path, name: &str, goal: &str, done: &str) {
        spaces::update_in(ui, |s| {
            missions::goal_by(s, name, Some(goal), Some(done), &Change::by("launch"), 1_000);
        });
    }

    #[test]
    fn small_words_and_replies_are_not_weighed() {
        assert!(!worth_weighing("yes"));
        assert!(!worth_weighing("2"));
        assert!(!worth_weighing("go on"));
        assert!(worth_weighing("also export the cases as CSV"));
        assert_eq!(request_words("Case exports", "In case exports: also add CSV"), "also add CSV");
        assert_eq!(request_words("Case exports", "also add CSV"), "also add CSV");
    }

    #[test]
    fn an_answer_is_read_and_checked() {
        let step = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"changes\": false, \"why\": \"a fix within the export\"}"}"#;
        assert_eq!(parse(step, "Export cases as PDF", "").unwrap(), Verdict::Step("a fix within the export".into()));
        let grows = r#"Here: {"changes": true, "goal": "Export cases as PDF and CSV", "done_when": "PDF export works; CSV export works", "why": "adds CSV"}"#;
        assert_eq!(
            parse(grows, "Export cases as PDF", "PDF export works").unwrap(),
            Verdict::Grows { goal: "Export cases as PDF and CSV".into(), done_when: "PDF export works; CSV export works".into(), why: "adds CSV".into() }
        );
        // The goal as it was is no change; a change with no goal is no answer.
        assert!(matches!(parse(r#"{"changes": true, "goal": "Export cases as PDF", "done_when": "x"}"#, "Export cases as PDF", "x").unwrap(), Verdict::Step(_)));
        assert!(parse(r#"{"changes": true, "goal": ""}"#, "g", "").is_err());
        assert!(parse("no json here", "g", "").is_err());
        // A done-when it leaves out stays as it was; a goal is one line.
        match parse("{\"changes\": true, \"goal\": \"Export\\n cases as PDF and CSV\"}", "Export cases as PDF", "PDF works").unwrap() {
            Verdict::Grows { goal, done_when, .. } => assert_eq!((goal.as_str(), done_when.as_str()), ("Export cases as PDF and CSV", "PDF works")),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn the_prompt_has_the_goal_its_history_and_the_request() {
        let history = vec![
            GoalChange { goal: "Export cases as PDF".into(), by: "launch".into(), ..Default::default() },
            GoalChange { goal: "Export cases as PDF and CSV".into(), by: "you".into(), ..Default::default() },
        ];
        let p = prompt("case-exports", "Export cases as PDF and CSV", "", &history, "and email the export to the client");
        assert!(p.contains("## Its goal now\n\nExport cases as PDF and CSV"));
        assert!(p.contains("(not said)"));
        assert!(p.contains("- Export cases as PDF (the space was made with it)"));
        assert!(p.contains("set by the user by hand: keep its words"));
        assert!(p.ends_with("## The new request\n\nand email the export to the client\n"));
    }

    #[test]
    fn a_request_that_adds_scope_updates_the_goal_and_a_step_leaves_it() {
        let ui = dir("follow");
        with_space(&ui, "case-exports", "Export cases as PDF", "PDF export works");
        let grows = |_: &str| -> Result<String> { Ok(r#"{"changes": true, "goal": "Export cases as PDF and CSV", "done_when": "PDF export works; CSV export works", "why": "adds CSV export"}"#.into()) };
        let out = follow_with(&ui, "case-exports", "In case-exports: also export them as CSV for the finance team", Some(5_000), false, &grows).unwrap();
        let Outcome::Updated(c) = out else { panic!("{out:?}") };
        assert_eq!((c.by.as_str(), c.request.as_str(), c.why.as_str()), ("request", "also export them as CSV for the finance team", "adds CSV export"));
        let s = spaces::load_from(&ui);
        assert_eq!(s.agendas["case-exports"], "Export cases as PDF and CSV");
        let m = &s.missions["case-exports"];
        assert_eq!(m.done_when, "PDF export works; CSV export works");
        assert_eq!(m.goals.iter().map(|g| (g.goal.as_str(), g.by.as_str())).collect::<Vec<_>>(), [("Export cases as PDF", "launch"), ("Export cases as PDF and CSV", "request")]);
        assert_eq!(m.goal_ms, c.at_ms, "only what is finished after it counts toward ready");
        // A step: nothing changes.
        let step = |_: &str| -> Result<String> { Ok(r#"{"changes": false, "why": "a fix in the CSV export"}"#.into()) };
        assert_eq!(follow_with(&ui, "case-exports", "the CSV header row is wrong, fix it please", None, false, &step).unwrap(), Outcome::Unchanged("a fix in the CSV export".into()));
        assert_eq!(spaces::load_from(&ui).missions["case-exports"].goals.len(), 2);
        // A request heard before the goal was set (launched, by hand) is in it already (a launch's own ticket).
        let never = |_: &str| -> Result<String> { panic!("not asked") };
        assert!(matches!(follow_with(&ui, "case-exports", "build the whole PDF export for the court", Some(1_000), false, &never).unwrap(), Outcome::NotWeighed(_)));
        // One heard while the last request was weighed (before its change) is weighed all the same.
        let json = |_: &str| -> Result<String> { Ok(r#"{"changes": true, "goal": "Export cases as PDF, CSV and JSON", "why": "adds JSON"}"#.into()) };
        assert!(matches!(follow_with(&ui, "case-exports", "and a JSON export for the API", Some(c.at_ms - 1), true, &json).unwrap(), Outcome::WouldBe { .. }));
        // A dry run says what it would be and changes nothing.
        assert!(matches!(follow_with(&ui, "case-exports", "and send the CSV to the client by email", None, true, &json).unwrap(), Outcome::WouldBe { .. }));
        assert_eq!(spaces::load_from(&ui).missions["case-exports"].goals.len(), 2);
        let _ = std::fs::remove_dir_all(&ui);
    }

    #[test]
    fn the_users_own_edit_wins_over_an_answer_that_comes_after_it() {
        let ui = dir("hand");
        with_space(&ui, "case-exports", "Export cases as PDF", "");
        // While the request is weighed, the user changes the goal by hand.
        let ui2 = ui.clone();
        let meanwhile = move |_: &str| -> Result<String> {
            spaces::update_in(&ui2, |s| {
                missions::goal_by(s, "case-exports", Some("Export cases as PDF, for the court"), None, &Change::by("you"), 2_000);
            });
            Ok(r#"{"changes": true, "goal": "Export cases as PDF and CSV", "done_when": "CSV works", "why": "adds CSV"}"#.into())
        };
        assert_eq!(follow_with(&ui, "case-exports", "also export them as CSV for the finance team", None, false, &meanwhile).unwrap(), Outcome::Overtaken);
        let s = spaces::load_from(&ui);
        assert_eq!(s.agendas["case-exports"], "Export cases as PDF, for the court");
        assert_eq!(s.missions["case-exports"].goals.last().unwrap().by, "you");
        let _ = std::fs::remove_dir_all(&ui);
    }

    #[test]
    fn a_standing_space_and_a_closed_one_are_left_alone() {
        // (A space without a goal gets its first one: see the chat tests below.)
        let ui = dir("left");
        with_space(&ui, "keep", "My own space", "");
        spaces::update_in(&ui, |s| {
            s.missions.get_mut("keep").unwrap().standing = true;
            missions::goal_by(s, "done", Some("Old work"), None, &Change::by("launch"), 1);
            s.missions.get_mut("done").unwrap().state = State::Closed;
        });
        let never = |_: &str| -> Result<String> { panic!("not asked") };
        for space in ["keep", "done"] {
            assert!(matches!(follow_with(&ui, space, "also build the admin dashboard for it", None, false, &never).unwrap(), Outcome::NotWeighed(_)), "{space}");
        }
        let _ = std::fs::remove_dir_all(&ui);
    }

    const FIRST: &str = r#"{"goal": "Fine-tune the search model for the MediaRoom API", "done_when": "it picks the right title for descriptive queries; answers within 800 ms", "why": "from the fine-tune request"}"#;

    #[test]
    fn a_first_goal_is_read_and_its_prompt_has_the_requests() {
        let p = first_prompt("search-llm", &["open a space at eyz-search-llm. we want to fine tune the search".into()], &["From t12r: the 1.2B is trained".into()]);
        assert!(p.contains("## The space\n\nsearch-llm"));
        assert!(p.contains("- open a space at eyz-search-llm. we want to fine tune the search\n"));
        assert!(p.contains("context only") && p.contains("- From t12r: the 1.2B is trained"));
        assert!(p.contains("only set the space up"));
        let (goal, done, why) = parse_first(FIRST).unwrap().unwrap();
        assert_eq!((goal.as_str(), why.as_str()), ("Fine-tune the search model for the MediaRoom API", "from the fine-tune request"));
        assert!(done.starts_with("it picks the right title"));
        assert_eq!(parse_first(r#"{"goal": "", "why": "it only opens the space"}"#).unwrap(), Err("it only opens the space".into()));
        assert!(parse_first("no json").is_err());
    }

    #[test]
    fn a_space_opened_by_chat_gets_its_goal_from_its_first_request_and_the_next_updates_it() {
        let ui = dir("chat");
        spaces::update_in(&ui, |s| {
            s.missions.insert("search-llm".into(), Mission::born(1));
        });
        // A request that only opens the space: no goal yet.
        let setup = |p: &str| -> Result<String> {
            assert!(p.contains("open a new herdr space with my second account"));
            Ok(r#"{"goal": "", "why": "it only opens the space"}"#.into())
        };
        assert_eq!(follow_with(&ui, "search-llm", "open a new herdr space with my second account", Some(2_000), false, &setup).unwrap(), Outcome::NoGoalYet("it only opens the space".into()));
        assert!(spaces::load_from(&ui).agendas.get("search-llm").is_none());
        // The first request that names the work: its goal and done-when.
        let first = |_: &str| -> Result<String> { Ok(FIRST.into()) };
        let out = follow_with(&ui, "search-llm", "we want to fine tune the search for the mediaroom api", Some(3_000), false, &first).unwrap();
        let Outcome::Written(c) = out else { panic!("{out:?}") };
        assert_eq!((c.by.as_str(), c.request.as_str()), ("request", "we want to fine tune the search for the mediaroom api"));
        let s = spaces::load_from(&ui);
        assert_eq!(s.agendas["search-llm"], "Fine-tune the search model for the MediaRoom API");
        let m = &s.missions["search-llm"];
        assert!(m.chat && m.done_when.ends_with("answers within 800 ms") && m.goals.len() == 1);
        // A second request that adds scope: updated, with its history.
        let grows = |p: &str| -> Result<String> {
            assert!(p.contains("## Its goal now\n\nFine-tune the search model for the MediaRoom API"));
            Ok(r#"{"changes": true, "goal": "Fine-tune the search model for the MediaRoom API and pass its picks into MediaRoom's query", "done_when": "it picks the right title; answers within 800 ms; MediaRoom's query uses its picks", "why": "adds passing the picks to MediaRoom"}"#.into())
        };
        let out = follow_with(&ui, "search-llm", "if we can keep the llm answers under 1s then we can pass its results to the mediaroom", None, false, &grows).unwrap();
        assert!(matches!(out, Outcome::Updated(_)), "{out:?}");
        let m = &spaces::load_from(&ui).missions["search-llm"];
        assert_eq!(m.goals.iter().map(|g| g.by.as_str()).collect::<Vec<_>>(), ["request", "request"]);
        assert!(m.goals[1].goal.ends_with("into MediaRoom's query") && m.goals[1].request.starts_with("if we can keep"));
        // A request heard before its first goal was written is in it already.
        let never = |_: &str| -> Result<String> { panic!("not asked") };
        assert!(matches!(follow_with(&ui, "search-llm", "open two sessions for sooner and mediaroom", Some(c.at_ms - 1), false, &never).unwrap(), Outcome::NotWeighed(_)));
        // The user sets the goal by hand: requests leave it as they wrote it.
        spaces::update_in(&ui, |s| {
            missions::goal_by(s, "search-llm", Some("Ship the LLM search behind the per-storefront switch"), Some("on in prod for one storefront"), &Change::by("you"), crate::history::now_ms());
        });
        assert!(matches!(follow_with(&ui, "search-llm", "also add a reranker for the catalogue route", None, false, &never).unwrap(), Outcome::NotWeighed(_)));
        let s = spaces::load_from(&ui);
        assert_eq!(s.agendas["search-llm"], "Ship the LLM search behind the per-storefront switch");
        assert_eq!(s.missions["search-llm"].goals.last().unwrap().by, "you");
        let _ = std::fs::remove_dir_all(&ui);
    }

    #[test]
    fn a_standing_space_stays_without_a_goal() {
        let ui = dir("standing");
        spaces::update_in(&ui, |s| {
            s.missions.insert("M5 Pro".into(), Mission { standing: true, ..Mission::born(1) });
        });
        let never = |_: &str| -> Result<String> { panic!("not asked") };
        assert!(matches!(follow_with(&ui, "M5 Pro", "build the tower's goal following for chat spaces", None, false, &never).unwrap(), Outcome::NotWeighed(_)));
        let mut ledger = crate::agenda::Ledger::default();
        ledger.agendas.push(crate::agenda::Agenda { id: "a1".into(), words: "fix the tower's merge cards on the phone".into(), origin: "user".into(), request_ms: 5, spaces: vec!["M5 Pro".into()], ..Default::default() });
        let out = backfill_with(&ui, &ledger, None, false, &never);
        assert!(matches!(out.as_slice(), [(n, Ok(Outcome::NotWeighed(_)))] if n == "M5 Pro"), "{out:?}");
        assert!(spaces::load_from(&ui).agendas.is_empty());
        let _ = std::fs::remove_dir_all(&ui);
    }

    #[test]
    fn the_backfill_writes_a_goal_from_the_requests_so_far_and_shows_it_first() {
        let ui = dir("backfill");
        spaces::update_in(&ui, |s| {
            s.missions.insert("search-llm".into(), Mission::born(1));
            s.missions.insert("quiet-one".into(), Mission::born(1));
            missions::goal_by(s, "Shop", Some("Ship the checkout"), None, &Change::by("launch"), 1);
        });
        let a = |id: &str, words: &str, origin: &str, at: u64, space: &str| crate::agenda::Agenda {
            id: id.into(),
            words: words.into(),
            origin: origin.into(),
            request_ms: at,
            created_ms: at,
            spaces: vec![space.into()],
            ..Default::default()
        };
        let mut ledger = crate::agenda::Ledger::default();
        ledger.agendas.push(a("a3", "Run option 1: add genres and a synopsis and retrain the 1.2B", "terminal", 30, "search-llm"));
        ledger.agendas.push(a("a1", "open a new herdr space at eyz-search-llm. we want to fine tune the search for mediaroom", "user", 10, "search-llm"));
        ledger.agendas.push(a("a2", "From t12r: the checkpoints are scored", "tower", 20, "search-llm"));
        ledger.agendas.push(a("a4", "yes", "user", 40, "search-llm"));
        let (asked, notes) = requests_so_far(&ledger, "Search-LLM");
        assert_eq!(asked.len(), 2, "a bare reply isn't one");
        assert!(asked[0].starts_with("open a new herdr space") && asked[1].starts_with("Run option 1"), "oldest first");
        assert_eq!(notes, ["From t12r: the checkpoints are scored"]);
        let first = |p: &str| -> Result<String> {
            assert!(p.find("- open a new herdr space").unwrap() < p.find("- Run option 1").unwrap());
            Ok(FIRST.into())
        };
        // Read-only first: what it would be, nothing changed.
        let out = backfill_with(&ui, &ledger, None, true, &first);
        let by: std::collections::BTreeMap<_, _> = out.into_iter().map(|(n, r)| (n, r.unwrap())).collect();
        assert!(matches!(&by["search-llm"], Outcome::WouldBe { goal, .. } if goal.starts_with("Fine-tune")));
        assert_eq!(by["quiet-one"], Outcome::NoGoalYet("no request of yours there yet".into()));
        assert!(matches!(by["Shop"], Outcome::NotWeighed(_)), "it has a goal");
        assert!(spaces::load_from(&ui).agendas.get("search-llm").is_none());
        // Then applied.
        let out = backfill_with(&ui, &ledger, Some("search-llm"), false, &first);
        assert!(matches!(out.as_slice(), [(_, Ok(Outcome::Written(_)))]), "{out:?}");
        let s = spaces::load_from(&ui);
        assert_eq!(s.agendas["search-llm"], "Fine-tune the search model for the MediaRoom API");
        let m = &s.missions["search-llm"];
        assert!(m.chat && m.goals[0].request.contains(" · Run option 1"));
        assert_eq!(m.state, State::Open);
        let _ = std::fs::remove_dir_all(&ui);
    }

    #[test]
    fn a_ready_space_whose_goal_grew_is_open_again() {
        let ui = dir("ready");
        with_space(&ui, "case-exports", "Export cases as PDF", "");
        spaces::update_in(&ui, |s| {
            let m = s.missions.get_mut("case-exports").unwrap();
            (m.state, m.ready_since_ms, m.report) = (State::Ready, Some(5), "Done: PDF export.".into());
        });
        let grows = |_: &str| -> Result<String> { Ok(r#"{"changes": true, "goal": "Export cases as PDF and CSV", "why": "adds CSV"}"#.into()) };
        assert!(matches!(follow_with(&ui, "case-exports", "also export them as CSV for the finance team", None, false, &grows).unwrap(), Outcome::Updated(_)));
        let m = &spaces::load_from(&ui).missions["case-exports"];
        assert_eq!((m.state, m.ready_since_ms, m.report.as_str()), (State::Open, None, ""));
        let _ = std::fs::remove_dir_all(&ui);
    }
}
