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
//! - **Which spaces**: one with a goal, not standing, not closed.
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
    let set_ms = m.goals.iter().rev().find(|c| c.by != "request").map(|c| c.at_ms).unwrap_or(m.goal_ms);
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
    let Some(On { name, goal, done_when, history, set_ms }) = weighed_on(&spaces::load_from(ui_dir), space) else {
        return Ok(Outcome::NotWeighed("its goal doesn't follow requests: it has none, it is standing, or it is closed".into()));
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
    if matches!(out, Outcome::Updated(_)) {
        missions::poke();
    }
    let v = out.json(&name);
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        match &out {
            Outcome::NotWeighed(why) => println!("{name}: not weighed ({why})."),
            Outcome::Unchanged(why) => println!("{name}: the goal stays ({}).", why.trim_end_matches('.')),
            Outcome::Updated(c) => println!("{name}: goal updated: {}. Done when: {}.", c.goal.trim_end_matches('.'), c.done_when.trim_end_matches('.')),
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
    fn a_standing_space_a_space_without_a_goal_and_a_closed_one_are_left_alone() {
        let ui = dir("left");
        with_space(&ui, "keep", "My own space", "");
        spaces::update_in(&ui, |s| {
            s.missions.get_mut("keep").unwrap().standing = true;
            s.missions.insert("bare".into(), Mission::born(1));
            missions::goal_by(s, "done", Some("Old work"), None, &Change::by("launch"), 1);
            s.missions.get_mut("done").unwrap().state = State::Closed;
        });
        let never = |_: &str| -> Result<String> { panic!("not asked") };
        for space in ["keep", "bare", "done"] {
            assert!(matches!(follow_with(&ui, space, "also build the admin dashboard for it", None, false, &never).unwrap(), Outcome::NotWeighed(_)), "{space}");
        }
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
