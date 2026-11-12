//! The launcher (`docs/quiet-tower.md`, "Step 9"; W9 and W10 in the design):
//! a sentence, or a whole ticket, starts a space. The goal is said in the
//! conversation (the pane has no field of its own for it); beside it a
//! project, what the tower will do, Launch; then the new space's own view
//! opens.
//!
//! - **The projects** are the tower's (`tower setups --json`): each a name,
//!   an account, what it is and its repos with their roles. None picked, the
//!   tower's brain picks; one picked keeps a draft to its repos.
//! - **A goal whose first line names the repos** ("open sessions in eyzflow
//!   and mediaroom: …"): what the tower will do is its own word, the receipt
//!   of `tower launch … --dry-run --json` (`plan_from`, once the typing
//!   pauses), and Launch is the confirm.
//! - **Any other goal** (a pasted ticket): the button is "Draft the space",
//!   `tower launch "<ticket>" --draft --json`. While the brain reads it,
//!   "Reading the ticket…". Its draft shows as a card (`Drafted`): the
//!   space, its goal (the ticket's title), done-when, each session's repo
//!   and why, the project and account. Launch runs exactly that draft
//!   (`tower launch "<ticket>" --repo … --name … --title … --done-when …`,
//!   `crate::ticket::confirm_flags`); what to change is said in the
//!   conversation (below) and redrafted (`--adjust "<words>" --prior '<the
//!   draft>'`), which takes the card's place; Start over drops the draft,
//!   its conversation and the ticket. Nothing opens before Launch. A ticket
//!   added to after its draft is drafted again.
//! - **The conversation** (`new_chat_column`, in the column a space's
//!   conversation has, the panel's input at its foot): what you said (the
//!   ticket, what to change, that it be planned) and what the tower said
//!   back (each draft, `draft_said`, and what went wrong), and a planned
//!   draft's thread with the planner, oldest first (`story_with`). What is
//!   typed there (`launch_say`) is the ticket before a draft, a change to
//!   it after, and words for the planner while a plan of it can talk.
//!   Launch hands it to the new space (`--story-file`), whose conversation
//!   opens with it (`talk::story_of`).
//! - A tower whose `launch --help` has no `--draft` gets the launcher as it
//!   was: the first project, its plan, Launch.
//! - **Launch**'s receipt names the space, whose view opens with what was
//!   done said at its top (`launched_line`), and an error stays here.
//! - **Plan it first** (`crate::plans`): under a draft, **Plan as a page**
//!   and **Plan as a whiteboard** (`tower plan new --form … --draft '<the
//!   draft>' --ticket-file <the ticket>`) have the tower's planner write the
//!   full plan and publish it as an Artifact on the user's claude.ai
//!   account. The state's `plan` (`model::plan`) is followed: "The tower's
//!   brain is writing the plan…", then "Plan · page · Open" (`open <url>`),
//!   a planner stuck on a dialog in its terminal with **Show in Herdr**, or
//!   why it failed with Plan again. The talk about it is in the
//!   conversation: the user's lines (`tower plan say --as user`) and the
//!   planner's answers. While a plan is open the card is the plan's draft
//!   (the planner revises it), Launch carries `--plan <id>` (the sessions
//!   get the plan; the plan closes) and Start over closes it.
//!   A plan of another ticket still open says so, with Back to it and Close.
//! - `HERDER_TOWER_ORB_DRAFT` (looking at it with test data): a file's draft
//!   JSON shows as the card for the goal the window opened on; `reading`
//!   shows the launcher reading a ticket; `ready`, the button before it.
//!   `HERDER_TOWER_ORB_PLAN=<file>`: the plan in the file (the state's
//!   `plan`, as `Plan::view` writes it) is the open plan, of that ticket.
//!   `HERDER_TOWER_ORB_TALK=<file>`: its lines are said in the
//!   conversation one after another (`talk_from_start`).

use std::time::Duration;

use gpui::{AnyElement, AsyncApp, Context, Entity, FontWeight, SharedString, Subscription, Task, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use serde_json::Value;

use super::home::{LINE, SOFT, acts_row, bay_label, cstat, cstat_busy, mbtn, tg};
use super::window::whead;
use super::{CYAN, Panel, cli, glass};
use crate::history;
use crate::launch::Told;
use crate::orb::model::{self, Missions, PlanView};
use crate::orb::nav::Scope;

/// A saved project (a setup): the sessions a new space starts with.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Setup {
    pub name: String,
    pub account: String,
    /// What it is, in a line.
    pub about: String,
    pub repos: Vec<String>,
    /// What each repo is in it, beside `repos` ("": nothing said).
    pub roles: Vec<String>,
}

/// The projects `tower setups --json` printed: `{"setups": [...]}` or the
/// list itself, each `{name, account, about, repos, roles: {repo: role}}`
/// (`about` and `roles` only from a tower that has them). Anything else: none.
pub(super) fn setups_from(text: &str) -> Vec<Setup> {
    let v: Value = serde_json::from_str(text).unwrap_or_default();
    let list = v.get("setups").unwrap_or(&v);
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let repos: Vec<String> = s["repos"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(String::from)).collect();
            let roles = repos.iter().map(|r| s["roles"][r.as_str()].as_str().unwrap_or("").trim().to_string()).collect();
            Some(Setup {
                name: s["name"].as_str().filter(|n| !n.trim().is_empty())?.to_string(),
                account: s["account"].as_str().unwrap_or("").to_string(),
                about: s["about"].as_str().unwrap_or("").trim().to_string(),
                repos,
                roles,
            })
        })
        .collect()
}

/// A repo by its folder's name (`~/Work/eyz/eyzflow`: `eyzflow`).
pub(super) fn repo_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// The name a space gets from its goal, by the tower's own rule
/// (`launch::space_name_of`): its first two words that carry meaning, in
/// lower case with a dash (`Fix the login redirect`: `fix-login`), not the
/// words that only ask for sessions, numbered when a space of that name is
/// there already; `space` from no words at all. (The tower's own plan, which
/// also knows the repos the goal names, replaces it as soon as it comes.)
pub(super) fn slug(goal: &str, taken: &[&str]) -> String {
    let base = match crate::launch::space_name_of(goal, &[]) {
        b if b.is_empty() => "space".to_string(),
        b => b,
    };
    let taken: Vec<String> = taken.iter().map(|t| t.to_string()).collect();
    crate::launch::unique(&base, &taken)
}

/// What the tower will do with `goal` and `setup`, a line each: the name
/// first, then (with a setup) its sessions, their worktrees and the handing
/// out. Worked out here, for before the tower has said it itself.
pub(super) fn plan(goal: &str, setup: Option<&Setup>, taken: &[&str]) -> Vec<String> {
    let mut lines = vec![if goal.trim().is_empty() { "Name the space from its goal".to_string() } else { format!("{NAMES}{}", slug(goal, taken)) }];
    let Some(setup) = setup else { return lines };
    let n = setup.repos.len();
    let repos = setup.repos.iter().map(|r| repo_name(r)).collect::<Vec<_>>().join(", ");
    let on = if setup.account.is_empty() { String::new() } else { format!(" on {}", setup.account) };
    lines.push(match n {
        0 => format!("Open its sessions{on}"),
        1 => format!("Open 1 session{on}: {repos}"),
        n => format!("Open {n} sessions{on}: {repos}"),
    });
    lines.push(if n == 1 { "Give it its own worktree, branch and dev port".into() } else { "Give each its own worktree, branch and dev port".into() });
    lines.push(if n > 1 { "Introduce them to each other and hand out the goal".into() } else { "Hand out the goal".into() });
    lines
}

/// How the plan's first line begins; what follows is the name, drawn bold.
const NAMES: &str = "Name the space ";
/// The same when a space of that name is there already (`--name`).
const JOINS: &str = "Join the space ";

/// The tower's own word on a launch: the receipt `tower launch … --dry-run
/// --json` prints.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Dry {
    /// What it will do, a line each.
    pub lines: Vec<String>,
    /// What it chose by itself, in a sentence.
    pub assumed: String,
    /// Why it can't (no setup, a repo that is no folder).
    pub error: Option<String>,
    /// Its repos are those the goal's first line names: Launch is the confirm, no draft.
    pub first_line: bool,
}

/// A folder as the user writes it: `~/Work/eyz/eyzflow`.
fn tilde(folder: &str) -> String {
    let home = crate::config::home();
    let home = home.to_string_lossy();
    match folder.strip_prefix(home.as_ref()) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => folder.to_string(),
    }
}

/// A launch's receipt (the plan of a dry run) as the lines the launcher
/// shows. None when it isn't a receipt.
pub(super) fn plan_from(receipt: &Value) -> Option<Dry> {
    let space = receipt["space"].as_str().filter(|s| !s.is_empty())?;
    let sessions = receipt["sessions"].as_array().filter(|s| !s.is_empty())?;
    let a = &receipt["assumed"];
    let n = sessions.len();
    let repos = sessions.iter().map(|s| repo_name(s["repo"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ");
    let account = a["account"].as_str().or_else(|| sessions[0]["account"].as_str()).filter(|x| !x.is_empty());
    let on = account.map(|x| format!(" on {x}")).unwrap_or_default();
    let mut lines = vec![format!("{}{space}", if receipt["made"] == false { JOINS } else { NAMES })];
    let _ = repos;
    lines.push(if n == 1 { format!("Open 1 session{on}:") } else { format!("Open {n} sessions{on}:") });
    // Each with its folder.
    for s in sessions {
        let folder = s["repo"].as_str().unwrap_or("");
        lines.push(format!("{} in {}", repo_name(folder), tilde(folder)));
    }
    let own = sessions.iter().filter(|s| s["worktree"].as_str().is_some_and(|w| !w.is_empty())).count();
    lines.push(match (own, n) {
        (0, _) => match a["worktrees_why"].as_str().map(|w| w.trim_start_matches("none: ").trim_start_matches("off ")).filter(|w| !w.is_empty()) {
            Some(why) => format!("No worktrees: {why}"),
            None => "No worktrees: each session works in its repo itself".to_string(),
        },
        (_, 1) => "Give it its own worktree, branch and dev port".to_string(),
        (own, n) if own == n => "Give each its own worktree, branch and dev port".to_string(),
        (own, n) => format!("Give {own} of {n} their own worktree, branch and dev port (a folder that is no git checkout is used as it is)"),
    });
    lines.push(if n > 1 { "Introduce them to each other and hand out the goal".into() } else { "Hand out the goal".to_string() });
    // What it chose by itself: the setup and why (not one that was named
    // to it: that is the choice above), and an account it had to pick.
    let mut assumed = match (a["setup"].as_str().filter(|x| !x.is_empty()), a["setup_why"].as_str().filter(|x| !x.is_empty())) {
        (Some(_), Some("the one named")) => String::new(),
        (Some(_), Some("the repos its first line names")) => "It opens the repos the goal's first line names.".to_string(),
        (Some(setup), Some(why)) => format!("It takes the setup “{setup}” ({why})."),
        (Some(setup), None) => format!("It takes the setup “{setup}”."),
        _ => String::new(),
    };
    if let Some(why) = a["account_why"].as_str().filter(|w| *w != "the setup's" && !w.is_empty()) {
        assumed = format!("{assumed} The account: {why}.").trim().to_string();
    }
    if let Some(d) = receipt["done_when"].as_str().filter(|d| !d.is_empty()) {
        lines.push(format!("Done when: {d}"));
    }
    // (A tower from before says so only in its words.)
    let first_line = match receipt["repos_from"].as_str() {
        Some(from) => from == "first-line",
        None => matches!(a["setup_why"].as_str(), Some("the repos its first line names" | "the one that has the repos the goal names")),
    };
    Some(Dry { lines, assumed, error: None, first_line })
}

/// A draft the tower's brain made of a ticket (`tower launch "<ticket>"
/// --draft --json`), as its card shows it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Drafted {
    pub space: String,
    /// The space's goal: the ticket's title.
    pub goal: String,
    pub done_when: String,
    /// Each session: its repo's folder and why it is in the space.
    pub sessions: Vec<(String, String)>,
    pub project: Option<String>,
    pub account: Option<String>,
    pub notes: String,
    /// What the brain named that the tower left out.
    pub dropped: Vec<String>,
    /// The draft as the tower printed it: what Launch runs and Redraft starts from.
    pub raw: Value,
}

/// A draft's receipt as its card shows it. None when it isn't one.
pub(super) fn draft_from(v: &Value) -> Option<Drafted> {
    let space = v["space"].as_str().filter(|s| !s.is_empty())?.to_string();
    let sessions: Vec<(String, String)> = v["sessions"].as_array()?.iter().filter_map(|s| Some((s["repo"].as_str()?.to_string(), s["why"].as_str().unwrap_or("").to_string()))).collect();
    if sessions.is_empty() {
        return None;
    }
    let word = |x: &Value| x.as_str().map(str::trim).filter(|x| !x.is_empty()).map(String::from);
    Some(Drafted {
        space,
        goal: v["goal"].as_str().unwrap_or("").to_string(),
        done_when: v["done_when"].as_str().unwrap_or("").to_string(),
        sessions,
        project: word(&v["project"]),
        account: word(&v["assumed"]["account"]).or_else(|| word(&v["sessions"][0]["account"])),
        notes: v["notes"].as_str().unwrap_or("").to_string(),
        dropped: v["dropped"].as_array().into_iter().flatten().filter_map(|d| d.as_str().map(String::from)).collect(),
        raw: v.clone(),
    })
}

/// What Launch runs for a draft: exactly what its card says, for `ticket`
/// (and with the plan `plan` it was planned with: its sessions get it), and
/// whether its PRs get a reviewer (`review`: the card's switch).
pub(super) fn confirm_args(ticket: &str, d: &Drafted, plan: Option<&str>, review: bool) -> Vec<String> {
    let mut args = vec!["launch".to_string(), ticket.to_string()];
    args.extend(crate::ticket::confirm_flags(&d.raw));
    if let Some(id) = plan {
        args.extend(["--plan".to_string(), id.to_string()]);
    }
    args.push(review_flag(review).into());
    args.push("--json".into());
    args
}

/// `--review` or `--no-review`, as the switch stands.
pub(super) fn review_flag(on: bool) -> &'static str {
    if on { "--review" } else { "--no-review" }
}

/// What the "Review its PRs" switch says it does, as it stands.
pub(super) fn review_note(on: bool) -> &'static str {
    if on {
        "Each PR its sessions open gets a reviewer session that looks for critical issues only (behaviour, crashes, security, data, the build, contracts) and agrees with the PR's session before you merge. Merging stays yours."
    } else {
        "No reviewer: its PRs come to you as they are."
    }
}

/// What Plan runs: the planner for this draft, in `form` (`page` or
/// `whiteboard`), the ticket in `file`.
pub(super) fn plan_args(form: &str, d: &Drafted, file: &str) -> Vec<String> {
    ["plan", "new", "--form", form, "--draft", &d.raw.to_string(), "--ticket-file", file, "--json"].map(String::from).to_vec()
}

/// What Send runs: the user's words about the plan `id`, for its planner.
pub(super) fn say_args(id: &str, words: &str) -> Vec<String> {
    ["plan", "say", "--as", "user", id, "--", words].map(String::from).to_vec()
}

/// What the plan's line says while it is not ready: how it stands, in a
/// sentence, and whether it is bad news.
pub(super) fn plan_line(p: &PlanView) -> (String, bool) {
    match (p.status.as_str(), p.blocked, p.planner.as_str()) {
        ("failed", _, _) => (format!("The plan didn't come: {}.", p.why.trim_end_matches('.')), true),
        (_, true, _) => ("The planner waits on a question in its terminal (publishing the plan may ask for your go-ahead). Answer it there.".into(), false),
        ("writing", _, "starting") => ("The tower's brain is starting the planner…".into(), false),
        _ => ("The tower's brain is writing the plan… (it reads the repos first: about five minutes)".into(), false),
    }
}

/// What a draft is asked: the ticket, the project picked, and for a
/// redraft the user's words and the draft before.
pub(super) fn draft_args(ticket: &str, project: Option<&str>, adjust: Option<(&str, &Drafted)>) -> Vec<String> {
    let mut args = vec!["launch".to_string(), ticket.to_string(), "--draft".into(), "--json".into()];
    if let Some(p) = project {
        args.extend(["--setup".to_string(), p.to_string()]);
    }
    if let Some((words, before)) = adjust {
        args.extend(["--adjust".to_string(), words.to_string(), "--prior".into(), before.raw.to_string()]);
    }
    args
}

/// What the tower will do with a ticket it is to draft, a line each.
pub(super) fn draft_plan(project: Option<&Setup>) -> Vec<String> {
    vec![
        match project {
            Some(p) => format!("Read the ticket and pick the repos of {} it needs", p.name),
            None => "Read the ticket and pick the repos it needs, from your projects and the folders under ~/Work".to_string(),
        },
        "Name the space; its goal is the ticket's title, done when its acceptance criteria are met".into(),
        "Show you the draft: launch it as it is, or say in words what to change".into(),
        "Nothing opens until you launch".into(),
    ]
}

/// What a launch did, from its receipt, for the top of the new space:
/// `Launched fix-login: 2 sessions on orca (eyzflow, mediaroom), each in its
/// own worktree. …`. None when it isn't a receipt.
pub(super) fn launched_line(receipt: &Value) -> Option<String> {
    crate::launch::launched_line(receipt).map(|l| format!("{l} Nothing was asked. Change the goal from here."))
}

/// What the tower says in the conversation of a draft (`before`: the draft
/// it redrafted): the space, its sessions, its goal and done-when, and for a
/// redraft what changed.
pub(super) fn draft_said(d: &Drafted, before: Option<&Drafted>) -> String {
    let repos: Vec<&str> = d.sessions.iter().map(|(r, _)| repo_name(r)).collect();
    let sessions = match repos.as_slice() {
        [one] => format!("1 session ({one})"),
        list => format!("{} sessions ({}), each in its own worktree", list.len(), list.join(", ")),
    };
    let on = d.account.as_ref().map(|a| format!(" on {a}")).unwrap_or_default();
    let from = d.project.as_ref().map(|p| format!(", from {p}")).unwrap_or_default();
    let done = if d.done_when.trim().is_empty() { "not said (the ticket has no acceptance criteria)".to_string() } else { d.done_when.trim().to_string() };
    let mut out = format!(
        "{} {}: {sessions}{on}{from}. Goal: {}. Done when: {}.",
        if before.is_some() { "Redrafted" } else { "Drafted" },
        d.space,
        d.goal.trim().trim_end_matches('.'),
        done.trim_end_matches('.')
    );
    if !d.dropped.is_empty() {
        out += &format!(" Left out {}.", d.dropped.join("; ").trim_end_matches('.'));
    }
    match before.map(|b| changed(b, d)) {
        None => out += " Launch it as it is, or say here what to change.",
        Some(c) if c.is_empty() => out += " Nothing on the card changed.",
        Some(c) => out += &format!(" Changed: {}.", c.join("; ")),
    }
    out
}

/// What is different on the card from `b` to `d`, a phrase each.
fn changed(b: &Drafted, d: &Drafted) -> Vec<String> {
    let mut out = vec![];
    if b.space != d.space {
        out.push(format!("named {} (was {})", d.space, b.space));
    }
    if b.goal.trim() != d.goal.trim() {
        out.push("the goal".into());
    }
    if b.done_when.trim() != d.done_when.trim() {
        out.push("what counts as done".into());
    }
    let had: Vec<&str> = b.sessions.iter().map(|(r, _)| r.as_str()).collect();
    let has: Vec<&str> = d.sessions.iter().map(|(r, _)| r.as_str()).collect();
    out.extend(has.iter().filter(|r| !had.contains(r)).map(|r| format!("adds {}", repo_name(r))));
    out.extend(had.iter().filter(|r| !has.contains(r)).map(|r| format!("drops {}", repo_name(r))));
    if b.project != d.project {
        out.push(match &d.project {
            Some(p) => format!("the project is {p}"),
            None => "no project: the repos alone".into(),
        });
    }
    if b.account != d.account
        && let Some(a) = &d.account
    {
        out.push(format!("on {a}"));
    }
    out
}

/// The conversation that makes the space, as it stands: `story` (what you
/// said and what the tower said back) and, when the draft is planned, the
/// plan's thread (you and the planner) and when it was ready; oldest first.
pub(super) fn story_with(story: &[Told], plan: Option<&PlanView>) -> Vec<Told> {
    let mut out = story.to_vec();
    if let Some(p) = plan {
        out.extend(p.thread.iter().map(|m| Told { who: if m.mine() { "you".into() } else { "planner".into() }, text: m.text.trim().to_string(), at: m.at }));
        if let (Some(at), Some(url)) = (p.ready_ms, p.url.as_ref()) {
            out.push(Told { who: "tower".into(), text: format!("The plan is ready, a {} on your claude.ai account: {url}. Talk it over here; Launch hands each session its part.", p.form_word()), at });
        }
    }
    out.sort_by_key(|t| t.at);
    out
}

/// `HERDER_TOWER_ORB_TALK=<file>` (looking at it with test data): what to
/// say in New space's conversation, a line each, one after another once
/// the tower is on none (`!launch` presses Launch; `!wait <s>` waits).
fn talk_from_start() -> std::collections::VecDeque<String> {
    let Some(path) = std::env::var("HERDER_TOWER_ORB_TALK").ok().filter(|p| !p.trim().is_empty()) else { return Default::default() };
    std::fs::read_to_string(path.trim()).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// The launcher's state.
pub(super) struct LaunchUi {
    /// The ticket, as the conversation has said it. It is not drawn: the
    /// conversation shows it, and what changes it is said there.
    goal: Entity<TextareaState>,
    /// The saved projects; `None` until the tower was asked.
    setups: Option<Vec<Setup>>,
    /// The project chosen. With drafts: none until one is (the brain picks);
    /// without: the first, until another is.
    picked: Option<String>,
    /// Whether this tower's `launch` has `--dry-run` (`None`: not asked yet).
    dry_known: Option<bool>,
    /// Whether it has `--draft`: a ticket that names no repo is drafted first.
    draft_known: Option<bool>,
    /// Whether it has `--story-file`: the conversation goes with the space it launches.
    story_known: bool,
    /// The conversation that makes the space, oldest first: what you said
    /// (the ticket, what to change, that it be planned) and what the tower
    /// said back (each draft, what went wrong). The planner's side is the
    /// plan's own thread (`story`). Launch hands it all to the new space
    /// (`--story-file`); Start over drops it.
    pub story: Vec<Told>,
    /// `HERDER_TOWER_ORB_TALK`: what is still to be said in the conversation, a line each.
    talk_hook: std::collections::VecDeque<String>,
    /// The hook waits until then before its next line.
    talk_at: Option<std::time::Instant>,
    /// What the tower said it will do, and for which goal and setup it said so.
    dry: Option<((String, Option<String>), Dry)>,
    /// The draft of a ticket, and the ticket it is of.
    draft: Option<(String, Drafted)>,
    /// `HERDER_TOWER_ORB_PLAN`: the open plan, from a file instead of the state.
    plan_hook: Option<PlanView>,
    /// Waits for the typing to pause before the tower is asked again.
    dry_wait: Option<Task<()>>,
    /// "Review its PRs": each PR its sessions open gets a reviewer
    /// (`--review`, on by default; off is `--no-review`).
    review: bool,
    _subs: Vec<Subscription>,
}

impl LaunchUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Panel>) -> Self {
        let goal = cx.new(|cx| TextareaState::new(window, cx));
        let subs = vec![cx.subscribe_in(&goal, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                this.ask_plan(cx);
                cx.notify();
            }
        })];
        let talk_hook = talk_from_start();
        Self {
            goal,
            setups: None,
            picked: None,
            dry_known: None,
            draft_known: None,
            story_known: false,
            story: vec![],
            talk_at: (!talk_hook.is_empty()).then(|| std::time::Instant::now() + Duration::from_secs(3)),
            talk_hook,
            dry: None,
            draft: None,
            plan_hook: None,
            dry_wait: None,
            review: true,
            _subs: subs,
        }
    }

    /// The saved projects, once the tower was asked.
    pub(super) fn setups(&self) -> Option<&[Setup]> {
        self.setups.as_deref()
    }

    /// The project that is chosen, of those there are: with drafts, only one
    /// that was picked; without, the first until another is.
    fn setup(&self) -> Option<&Setup> {
        let list = self.setups.as_ref()?;
        let picked = self.picked.as_ref().and_then(|p| list.iter().find(|s| &s.name == p));
        if self.drafts() { picked } else { picked.or(list.first()) }
    }

    /// A ticket that names no repo is drafted first (this tower's `launch` has `--draft`).
    fn drafts(&self) -> bool {
        self.draft_known == Some(true)
    }

    /// The project as the plan is asked for it: none with drafts (whether
    /// the first line names the repos is the question then).
    fn dry_setup(&self) -> Option<String> {
        if self.drafts() { None } else { self.setup().map(|s| s.name.clone()) }
    }

    /// The goal's first line names its repos, as the tower said for it: the
    /// plan, and Launch, with no draft.
    fn names_repos(&self, goal: &str) -> bool {
        self.dry.as_ref().is_some_and(|((g, s), d)| g == goal && s.is_none() && d.first_line && d.error.is_none())
    }

    /// The draft of `goal`, if there is one of it as it is now.
    fn draft_of(&self, goal: &str) -> Option<&Drafted> {
        self.draft.as_ref().filter(|(t, _)| t == goal).map(|(_, d)| d)
    }
}

/// A field on the white pill (a space's goal as it is changed, a project's name, a phone's).
pub(super) fn field(state: &Entity<TextareaState>) -> AnyElement {
    let g = glass();
    div()
        .w_full()
        .px(px(8.0))
        .py(px(7.0))
        .rounded(px(14.0))
        .bg(g.pill)
        .text_color(g.pill_text)
        .child(Textarea::new(state).appearance(false).bordered(false).small().w_full().text_size(px(15.0)).text_color(g.pill_text))
        .into_any_element()
}

/// `HERDER_TOWER_ORB_DRAFT`: `reading`, `ready`, or a file holding a draft's JSON.
fn draft_from_start() -> Option<String> {
    std::env::var("HERDER_TOWER_ORB_DRAFT").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl Panel {
    /// Asks the tower for its projects, and once whether its launcher can
    /// say what it would do (`--dry-run` in its help) and draft a ticket (`--draft`).
    pub(super) fn read_setups(&mut self, cx: &mut Context<Self>) {
        self.run("setups", vec!["setups".into(), "--json".into()], cx, |this, result, _| {
            let Some(full) = this.full.as_mut() else { return };
            full.launch.setups = Some(result.as_ref().map(|out| setups_from(out)).unwrap_or_default());
        });
        if self.full.as_ref().is_some_and(|f| f.launch.dry_known.is_none()) {
            let task = cx.background_executor().spawn(async { cli::tower(&["launch", "--help"]) });
            cx.spawn(async move |this, cx: &mut AsyncApp| {
                let help = task.await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(full) = this.full.as_mut() {
                        let text = help.unwrap_or_default();
                        full.launch.dry_known = Some(text.contains("--dry-run"));
                        full.launch.draft_known.get_or_insert(text.contains("--draft"));
                        full.launch.story_known = text.contains("--story-file");
                    }
                    this.ask_plan(cx);
                });
            })
            .detach();
        }
    }

    /// `goal` is the launcher's ticket, said in its conversation (the plan
    /// follows, as when it is said there).
    pub(super) fn launch_with(&mut self, goal: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(full) = self.full.as_ref() {
            let (field, goal) = (full.launch.goal.clone(), goal.to_string());
            field.update(cx, |s, cx| s.set_value(goal, window, cx));
        }
        self.tell("you", goal);
        self.draft_hook(cx);
    }

    /// `HERDER_TOWER_ORB_DRAFT` (see the top): the launcher as it is while
    /// a ticket is drafted, or with a draft, to look at it with test data.
    fn draft_hook(&mut self, cx: &mut Context<Self>) {
        // `HERDER_TOWER_ORB_PLAN`: the open plan, of the ticket the window opened on.
        if let Some(path) = std::env::var("HERDER_TOWER_ORB_PLAN").ok().filter(|p| !p.trim().is_empty())
            && let Some(full) = self.full.as_mut()
        {
            let ticket = full.launch.goal.read(cx).value().trim().to_string();
            let text = std::fs::read_to_string(path.trim()).unwrap_or_default();
            match serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| model::plan(&serde_json::json!({ "plan": v }))) {
                Some(mut p) => {
                    // (A plan of another ticket keeps its own: the launcher says it is still open.)
                    if p.ticket.trim().is_empty() {
                        p.ticket = ticket;
                    }
                    full.launch.plan_hook = Some(p);
                }
                None => eprintln!("[orb] HERDER_TOWER_ORB_PLAN: no plan in {path}"),
            }
        }
        let Some(hook) = draft_from_start() else { return };
        let Some(full) = self.full.as_mut() else { return };
        let ticket = full.launch.goal.read(cx).value().trim().to_string();
        full.launch.draft_known = Some(true);
        match hook.as_str() {
            "ready" => {}
            "reading" => {
                full.busy.insert("draft".into());
            }
            path => match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).as_ref().and_then(draft_from) {
                Some(d) => full.launch.draft = Some((ticket, d)),
                None => eprintln!("[orb] HERDER_TOWER_ORB_DRAFT: no draft in {path}"),
            },
        }
    }

    /// Asks the tower what it would do with the goal and the setup as they
    /// stand, once the typing has paused; only a tower that has a dry run.
    fn ask_plan(&mut self, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_mut() else { return };
        if full.launch.dry_known != Some(true) {
            return;
        }
        let goal = full.launch.goal.read(cx).value().trim().to_string();
        let key = (goal.clone(), full.launch.dry_setup());
        if goal.is_empty() || full.launch.dry.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        full.launch.dry_wait = Some(cx.spawn(async move |this, cx: &mut AsyncApp| {
            cx.background_executor().timer(Duration::from_millis(450)).await;
            let mut args = vec!["launch".to_string(), key.0.clone()];
            if let Some(setup) = &key.1 {
                args.extend(["--setup".to_string(), setup.clone()]);
            }
            args.extend(["--dry-run".to_string(), "--json".to_string()]);
            let said = cx.background_executor().spawn(async move { cli::tower(&args) }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(full) = this.full.as_mut() else { return };
                // Its plan, or why it can't launch; anything else is no word at all.
                let dry = match said {
                    Ok(out) => serde_json::from_str::<Value>(&out).ok().as_ref().and_then(plan_from),
                    Err(e) => Some(Dry { error: Some(e), ..Default::default() }),
                };
                full.launch.dry = dry.map(|d| (key, d));
                cx.notify();
            });
        }));
    }

    /// Launch: the space is made, and its view opens with what the tower said.
    fn launch(&mut self, missions: &Missions, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_ref() else { return };
        let goal = full.launch.goal.read(cx).value().trim().to_string();
        if goal.is_empty() {
            return;
        }
        let mut args = vec!["launch".to_string(), goal.clone()];
        if let Some(setup) = full.launch.setup().filter(|_| !full.launch.drafts()) {
            args.extend(["--setup".to_string(), setup.name.clone()]);
        }
        args.push(review_flag(full.launch.review).into());
        args.push("--json".into());
        // Launched as its first line says: the goal is what was said.
        if full.launch.story.iter().all(|t| t.who != "you") {
            self.tell("you", &goal);
        }
        self.launch_args(&goal, args, missions, cx);
    }

    /// Launch, on the draft as its card shows it: exactly that.
    fn launch_draft(&mut self, missions: &Missions, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_ref() else { return };
        let ticket = full.launch.goal.read(cx).value().trim().to_string();
        // Planned: the plan's own draft (the planner may have revised it), and the plan with it.
        let any = self.plan_for(&ticket, cx);
        let plan = any.clone().filter(|p| p.status != "failed");
        let Some(full) = self.full.as_ref() else { return };
        let d = match plan.as_ref().and_then(|p| draft_from(&p.draft)).or_else(|| full.launch.draft_of(&ticket).cloned()).or_else(|| any.as_ref().and_then(|p| draft_from(&p.draft))) {
            Some(d) => d,
            None => return,
        };
        let with = plan.as_ref().filter(|p| p.url.is_some()).map(|p| p.id.clone());
        let args = confirm_args(&ticket, &d, with.as_deref(), full.launch.review);
        // A plan that goes without it (not written yet, failed) is closed (`--plan` closes a written one itself).
        if let Some(p) = any.filter(|p| with.as_deref() != Some(p.id.as_str())) {
            self.plan_close(&p.id, cx);
        }
        self.launch_args(&ticket, args, missions, cx);
    }

    /// The open plan (the state's, or `HERDER_TOWER_ORB_PLAN`'s), whatever ticket it is of.
    pub(super) fn plan_now(&self, cx: &Context<Self>) -> Option<PlanView> {
        if let Some(p) = self.full.as_ref().and_then(|f| f.launch.plan_hook.clone()) {
            return Some(p);
        }
        model::plan(self.tower.read(cx).state.as_ref()?)
    }

    /// The open plan, if it is of `ticket` as it is now.
    fn plan_for(&self, ticket: &str, cx: &Context<Self>) -> Option<PlanView> {
        self.plan_now(cx).filter(|p| p.ticket.trim() == ticket.trim())
    }

    /// Plan as a page / as a whiteboard: the planner for the draft on the
    /// card (another plan that is open ends).
    fn plan_new(&mut self, form: &'static str, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_ref() else { return };
        let ticket = full.launch.goal.read(cx).value().trim().to_string();
        let local = full.launch.draft_of(&ticket).cloned();
        let d = match self.plan_for(&ticket, cx).and_then(|p| draft_from(&p.draft)).or(local) {
            Some(d) => d,
            None => return,
        };
        // The ticket goes as a file: it can be a long one.
        let file = std::env::temp_dir().join(format!("tower-plan-ticket-{}.md", history::now_ms()));
        if let Err(e) = std::fs::write(&file, &ticket) {
            if let Some(full) = self.full.as_mut() {
                full.notes.insert("plan".into(), (false, format!("Couldn't hand the ticket over: {e}")));
            }
            cx.notify();
            return;
        }
        let args = plan_args(form, &d, &file.to_string_lossy());
        self.tell("you", &format!("Plan it first, as a {}.", if form == "whiteboard" { "whiteboard" } else { "page" }));
        self.run("plan", args, cx, move |this, result, _| {
            let _ = std::fs::remove_file(&file);
            match result {
                Ok(_) => this.tell("tower", "The planner is on it: it reads the repos and the draft, writes the plan and publishes it on your claude.ai account, private (about five minutes). Nothing opens."),
                Err(e) => this.tell("tower", &format!("The planner didn't start: {}.", e.trim().trim_end_matches('.'))),
            }
        });
    }

    /// The user's words about the plan `id`, for its planner; they and its
    /// answer come in the plan's thread (the state).
    fn plan_say(&mut self, id: &str, words: &str, cx: &mut Context<Self>) {
        self.run("plan-say", say_args(id, words), cx, |_, _, _| {});
    }

    /// A message of the conversation that makes the space: `who` said `text` now.
    fn tell(&mut self, who: &str, text: &str) {
        if let Some(full) = self.full.as_mut()
            && !text.trim().is_empty()
        {
            full.launch.story.push(Told { who: who.into(), text: text.trim().to_string(), at: history::now_ms() });
        }
    }

    /// What was typed (or said) in New space's conversation: to the planner
    /// while a plan of the draft can talk; a change to the draft while
    /// there is one (it is redrafted); else the ticket, or more of it, and
    /// it is drafted (or, for a first line that names the repos, said what
    /// Launch will do). Why it can't be taken now, else.
    pub(super) fn launch_say(&mut self, words: &str, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        let words = words.trim();
        if words.is_empty() {
            return Err(String::new());
        }
        if self.running("launch") {
            return Err("It is launching: say it in the new space.".into());
        }
        if self.running("draft") {
            return Err("The tower's brain is still on the last one: say it once its answer is here.".into());
        }
        let Some(full) = self.full.as_ref() else { return Err(String::new()) };
        let ticket = full.launch.goal.read(cx).value().trim().to_string();
        if let Some(p) = self.plan_for(&ticket, cx).filter(|p| p.status != "failed" && p.address.is_some()) {
            if self.running("plan-say") {
                return Err("Still handing your last words to the planner.".into());
            }
            self.plan_say(&p.id, words, cx);
            return Ok(());
        }
        let Some(full) = self.full.as_ref() else { return Err(String::new()) };
        let ui = &full.launch;
        // A draft (of this ticket, or of one changed since): these words change it.
        if ui.drafts() && !ticket.is_empty() && !ui.names_repos(&ticket) && ui.draft.is_some() {
            self.draft(Some(words.to_string()), cx);
            return Ok(());
        }
        // No draft yet: the ticket, or more of it.
        let goal = if ticket.is_empty() { words.to_string() } else { format!("{ticket}\n\n{words}") };
        let (field, drafts, dry) = (ui.goal.clone(), ui.drafts(), ui.dry_known == Some(true));
        field.update(cx, |s, cx| s.set_value(goal.clone(), window, cx));
        match (drafts, dry) {
            (true, true) => self.dry_then_draft(goal, cx),
            (true, false) => self.draft(None, cx),
            (false, _) => {
                self.tell("you", &goal);
                self.tell("tower", "This tower drafts nothing: it opens the repos the first line names. What it will do is beside; Launch when it is right.");
            }
        }
        Ok(())
    }

    /// The goal said in the conversation: a first line that names the repos
    /// is launched as it says (the tower says what that will open), any
    /// other is drafted.
    fn dry_then_draft(&mut self, goal: String, cx: &mut Context<Self>) {
        let args = vec!["launch".to_string(), goal.clone(), "--dry-run".into(), "--json".into()];
        self.run("draft", args, cx, move |this, result, cx| {
            let dry = result.as_ref().ok().and_then(|out| serde_json::from_str::<Value>(out).ok()).as_ref().and_then(plan_from);
            match dry.filter(|d| d.first_line && d.error.is_none()) {
                Some(d) => {
                    this.tell("you", &goal);
                    this.tell("tower", &format!("Its first line names the repos, so there is no draft: {}. Launch opens exactly that.", d.lines.join("; ").trim_end_matches('.')));
                    if let Some(full) = this.full.as_mut() {
                        full.launch.dry = Some(((goal, None), d));
                    }
                }
                None => this.draft(None, cx),
            }
        });
    }

    /// The plan ends, and its planner.
    fn plan_close(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(full) = self.full.as_mut()
            && full.launch.plan_hook.as_ref().is_some_and(|p| p.id == id)
        {
            full.launch.plan_hook = None;
        }
        self.run("plan-close", vec!["plan".into(), "close".into(), id.to_string()], cx, |_, _, _| {});
    }

    /// Back to the plan that is open: its ticket is the launcher's (said
    /// first in the conversation, the plan's thread after it), its draft on the card.
    fn plan_back(&mut self, p: &PlanView, window: &mut Window, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_mut() else { return };
        let ticket = p.ticket.trim().to_string();
        if let Some(d) = draft_from(&p.draft) {
            full.launch.draft = Some((ticket.clone(), d));
        }
        full.launch.draft_known = Some(true);
        full.launch.story = vec![Told { who: "you".into(), text: ticket.clone(), at: p.created_ms }];
        let goal = full.launch.goal.clone();
        goal.update(cx, |s, cx| s.set_value(ticket, window, cx));
        cx.notify();
    }

    /// Runs `tower launch` with `args`; its receipt names the space, whose view opens with what it did.
    fn launch_args(&mut self, goal: &str, mut args: Vec<String>, missions: &Missions, cx: &mut Context<Self>) {
        let before: Vec<String> = missions.open.iter().map(|m| m.name.clone()).chain(missions.quiet.iter().cloned()).collect();
        let taken: Vec<&str> = before.iter().map(String::as_str).collect();
        let expected = args.iter().position(|a| a == "--name").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| slug(goal, &taken));
        // The conversation that made it goes with it: the new space keeps it, at its top.
        let story = self.story_now(cx);
        let file = std::env::temp_dir().join(format!("tower-launch-story-{}.json", history::now_ms()));
        let handed = self.full.as_ref().is_some_and(|f| f.launch.story_known)
            && !story.is_empty()
            && serde_json::to_string(&story).ok().is_some_and(|text| std::fs::write(&file, text).is_ok());
        if handed {
            args.extend(["--story-file".to_string(), file.to_string_lossy().into_owned()]);
        }
        self.run("launch", args, cx, move |this, result, cx| {
            if handed {
                let _ = std::fs::remove_file(&file);
            }
            let out = match result {
                Ok(out) => out,
                Err(e) => return this.tell("tower", &format!("The launch didn't go: {}.", e.trim().trim_end_matches('.'))),
            };
            // Its receipt names the space and says what was opened.
            let receipt: Value = serde_json::from_str(out).unwrap_or_default();
            let space = receipt["space"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or(expected);
            let text = launched_line(&receipt).unwrap_or_else(|| {
                let said = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
                if said.is_empty() { format!("Launched {space}. Nothing was asked. Change the goal from here.") } else { said }
            });
            if let Some(full) = this.full.as_mut() {
                (full.launch.dry, full.launch.draft) = (None, None);
                // It is the new space's now.
                full.launch.story.clear();
            }
            // What it opened is an act the tower keeps: Undo shows with what it said, as with a reply.
            let act = receipt["act"].as_str().map(String::from);
            this.tower.update(cx, |t, cx| {
                t.nav.launched(&space, before, text, history::now_ms());
                t.nav.receipt_of(act);
                cx.notify();
            });
        });
    }

    /// "Draft the space" (and Redraft, with the user's words): the tower's
    /// brain reads the ticket; its draft takes the card's place.
    fn draft(&mut self, adjust: Option<String>, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_ref() else { return };
        let ticket = full.launch.goal.read(cx).value().trim().to_string();
        if ticket.is_empty() {
            return;
        }
        // A change is to the latest draft (of this ticket, or of it before it was changed).
        let before = match &adjust {
            Some(_) => full.launch.draft.as_ref().map(|(_, d)| d.clone()),
            None => full.launch.draft_of(&ticket).cloned(),
        };
        let project = full.launch.setup().map(|s| s.name.clone());
        let args = draft_args(&ticket, project.as_deref(), adjust.as_deref().zip(before.as_ref()));
        // The conversation: what you said (the ticket once, or what to change).
        let said = adjust.clone().unwrap_or_else(|| ticket.clone());
        let again = full.launch.story.iter().rev().find(|t| t.who == "you").is_some_and(|t| adjust.is_none() && t.text == ticket);
        if !again {
            self.tell("you", &said);
        }
        self.run("draft", args, cx, move |this, result, _| {
            let out = match result {
                Ok(out) => out,
                Err(e) => return this.tell("tower", &format!("The draft didn't come: {}.", e.trim().trim_end_matches('.'))),
            };
            match serde_json::from_str::<Value>(out).ok().as_ref().and_then(draft_from) {
                Some(d) => {
                    this.tell("tower", &draft_said(&d, before.as_ref().filter(|_| adjust.is_some())));
                    if let Some(full) = this.full.as_mut() {
                        full.launch.draft = Some((ticket, d));
                    }
                }
                None => {
                    let why = "The tower answered no draft. Try again, or name the repos in the first line.";
                    this.tell("tower", why);
                    if let Some(full) = this.full.as_mut() {
                        full.notes.insert("draft".into(), (false, why.into()));
                    }
                }
            }
        });
    }

    /// Start over: the draft, its conversation and the ticket are dropped
    /// (the conversation was the only place the ticket showed), and the
    /// conversation's input waits for the next one.
    fn start_over(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A plan of it ends, and its planner.
        if let Some(full) = self.full.as_ref() {
            let ticket = full.launch.goal.read(cx).value().trim().to_string();
            if let Some(p) = self.plan_for(&ticket, cx) {
                self.plan_close(&p.id, cx);
            }
        }
        let Some(full) = self.full.as_mut() else { return };
        full.launch.draft = None;
        // Its conversation goes with it: a new one starts with the next draft.
        full.launch.story.clear();
        full.notes.remove("plan");
        full.notes.remove("draft");
        let goal = full.launch.goal.clone();
        goal.update(cx, |s, cx| s.set_value("", window, cx));
        self.text.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// The draft as a card: the space, its goal and done-when, each
    /// session's repo and why, the project and account, what was left out.
    fn draft_card(&self, d: &Drafted, dim: bool) -> AnyElement {
        let g = glass();
        let row = |word: &str, text: &str| super::window::named(word, text);
        let sessions = v_flex().gap(px(5.0)).children(d.sessions.iter().map(|(repo, why)| {
            h_flex()
                .gap(px(9.0))
                .items_start()
                .child(div().flex_none().child("•"))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_x(px(8.0))
                                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(g.text).child(repo_name(repo).to_string()))
                                .child(div().text_size(px(12.0)).text_color(g.faint).child(tilde(repo))),
                        )
                        .when(!why.is_empty(), |el| el.child(div().text_size(px(12.0)).line_height(px(16.0)).text_color(g.muted).child(why.clone()))),
                )
        }));
        let project = match (&d.project, &d.account) {
            (Some(p), Some(a)) => format!("{p} · on {a}"),
            (Some(p), None) => p.clone(),
            (None, Some(a)) => format!("none fits: the repos alone · on {a}"),
            (None, None) => "none fits: the repos alone".to_string(),
        };
        let n = d.sessions.len();
        v_flex()
            .w_full()
            .px(px(14.0))
            .py(px(12.0))
            .gap(px(7.0))
            .rounded(px(14.0))
            .bg(rgba(SOFT))
            .border_1()
            .border_color(rgba(LINE))
            .text_size(px(13.0))
            .line_height(px(18.0))
            .text_color(g.muted)
            .when(dim, |el| el.opacity(0.55))
            .child(h_flex().gap(px(5.0)).child(div().flex_none().font_weight(FontWeight::SEMIBOLD).text_color(g.text).child("Space")).child(div().font_weight(FontWeight::SEMIBOLD).text_color(g.text).child(d.space.clone())))
            .child(row("Goal", &d.goal))
            .child(row("Done when", if d.done_when.is_empty() { "not said: the ticket has no acceptance criteria" } else { &d.done_when }))
            .child(div().pt(px(3.0)).font_weight(FontWeight::SEMIBOLD).text_color(g.text).child(if n == 1 { "1 session".to_string() } else { format!("{n} sessions, each in its own worktree") }))
            .child(sessions)
            .child(div().pt(px(3.0)).child(row("Project", &project)))
            .when(!d.notes.is_empty(), |el| el.child(div().text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(d.notes.clone())))
            .when(!d.dropped.is_empty(), |el| el.child(div().text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(format!("Left out {}.", d.dropped.join("; ")))))
            .into_any_element()
    }

    /// "See the plan first": Plan as a page, Plan as a whiteboard (`again`:
    /// after one that failed or ended).
    fn plan_offer(&self, again: bool, can: bool, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let starting = self.running("plan");
        let can = can && !starting;
        let pill = |id: &'static str, text: &str, form: &'static str| {
            tg(id, text, false).when(!can, |el| el.opacity(0.5)).when(can, |el| el.on_click(cx.listener(move |this, _, _, cx| this.plan_new(form, cx))))
        };
        let (page, board) = if again { ("Plan again as a page", "Plan again as a whiteboard") } else { ("Plan as a page", "Plan as a whiteboard") };
        v_flex()
            .gap(px(6.0))
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().px(px(4.0)).text_size(px(13.0)).text_color(g.muted).child(if again { "Plan it again:" } else { "Or see the plan first:" }))
                    .child(pill("plan-page", page, "page"))
                    .child(pill("plan-board", board, "whiteboard"))
                    .when(starting, |el| el.child(cstat_busy("Starting the planner…"))),
            )
            .when(!again, |el| {
                el.child(
                    div()
                        .px(px(4.0))
                        .text_size(px(12.0))
                        .line_height(px(17.0))
                        .text_color(g.faint)
                        .child("The tower's brain writes the full plan (each session's part and its order, how the repos fit together, the risks, the open questions, the tests) and publishes it to your claude.ai account, private. Then you talk it over in the conversation. Nothing opens until you launch."),
                )
            })
            .into_any_element()
    }

    /// The plan of the draft on the card: how it stands (writing, waiting on
    /// its terminal, failed, or "Plan · page · Open"), then the chat about it.
    fn plan_section(&self, p: &PlanView, can: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let mut items: Vec<AnyElement> = vec![bay_label("The plan").px(px(0.0)).into_any_element()];
        let boxed = || v_flex().w_full().px(px(14.0)).py(px(12.0)).gap(px(8.0)).rounded(px(14.0)).bg(rgba(SOFT)).border_1().border_color(rgba(LINE));
        let ready = p.status == "ready" && p.url.is_some();
        if ready {
            let url = p.url.clone().unwrap_or_default();
            let open = div()
                .id("plan-open")
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(CYAN))
                .cursor_pointer()
                .hover(|s| s.underline())
                .child("Open")
                .on_click(cx.listener(move |_, _, _, cx| {
                    let url = url.clone();
                    cx.background_executor().spawn(async move { std::process::Command::new("open").arg(url).status() }).detach();
                }));
            let line = h_flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .text_size(px(14.0))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(g.text).child("Plan"))
                .child(div().text_color(g.faint).child("·"))
                .child(div().text_color(g.muted).child(p.form_word().to_string()))
                .child(div().text_color(g.faint).child("·"))
                .child(open);
            let revised = match p.revision {
                0 => None,
                1 => Some("The planner changed the draft once: the card above is the draft as it is now.".to_string()),
                n => Some(format!("The planner changed the draft {n} times: the card above is the draft as it is now.")),
            };
            items.push(
                boxed()
                    .child(line)
                    .children(revised.map(|r| cstat(r, false)))
                    .child(div().text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child("Launch hands each session the plan's link and its own part of it."))
                    .when(p.blocked, |el| el.child(cstat("The planner waits on a question in its terminal. Answer it there.", false)))
                    .children(p.address.clone().filter(|_| p.blocked).map(|a| {
                        acts_row().child(mbtn("plan-show", "Show in Herdr", false, None).on_click(cx.listener(move |this, _, _, cx| this.show_in_herdr(&a, cx))))
                    }))
                    .when(!p.why.is_empty(), |el| el.child(cstat(p.why.clone(), false)))
                    .into_any_element(),
            );
        } else {
            let (line, bad) = plan_line(p);
            let busy_now = !bad && !p.blocked;
            // Its terminal holds a dialog: there, one tap away.
            let show = p.address.clone().filter(|_| p.blocked).map(|a| {
                acts_row().child(mbtn("plan-show", "Show in Herdr", false, None).on_click(cx.listener(move |this, _, _, cx| this.show_in_herdr(&a, cx))))
            });
            items.push(
                boxed()
                    .child(if busy_now { cstat_busy(line) } else { cstat(line, bad) })
                    .when(busy_now, |el| {
                        el.child(div().text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(format!(
                            "It reads the repos and the draft, writes the plan, and publishes it as a {} on your claude.ai account (private). Nothing opens.",
                            p.form_word()
                        )))
                    })
                    .children(show)
                    .into_any_element(),
            );
        }
        // Failed, or its planner ended: plan it again.
        if p.status == "failed" || (ready && p.address.is_none()) {
            items.push(self.plan_offer(true, can, cx));
        }
        // The talk about it is in the conversation beside.
        if p.address.is_some() && p.status != "failed" {
            items.push(div().px(px(4.0)).text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child("Talk it over in the conversation: what you say there goes to the planner, and it answers there.").into_any_element());
        }
        items
    }

    /// The launcher's head and body.
    pub(super) fn launch_view(&mut self, missions: &Missions, window: &mut Window, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let g = glass();
        let head = whead("New space", vec![div().child("Say in the conversation what it should get done, or paste the ticket there. Nothing opens until you launch.").into_any_element()], None);
        self.talk_tick(missions, window, cx);
        let plan_open = self.plan_now(cx);
        let Some(full) = self.full.as_ref() else { return (head, vec![]) };
        let ui = &full.launch;
        let goal = ui.goal.read(cx).value().trim().to_string();
        let taken: Vec<&str> = missions.open.iter().map(|m| m.name.as_str()).chain(missions.quiet.iter().map(String::as_str)).collect();
        let setup = ui.setup().cloned();
        let drafts = ui.drafts() && !ui.names_repos(&goal);
        // The plan of this ticket, if one is open; one of another ticket is said so below.
        let planned = plan_open.clone().filter(|p| p.ticket.trim() == goal);
        let stale = plan_open.filter(|p| p.ticket.trim() != goal);
        // While its planner can talk, the chat is how the draft changes; the card is the plan's draft.
        let chatting = planned.as_ref().is_some_and(|p| p.status != "failed");
        let draft = planned
            .as_ref()
            .filter(|_| chatting)
            .and_then(|p| draft_from(&p.draft))
            .or_else(|| ui.draft_of(&goal).cloned())
            .or_else(|| planned.as_ref().and_then(|p| draft_from(&p.draft)))
            .filter(|_| drafts);
        // The tower's own word on what it will do, if it gave it for this goal and setup.
        let key = (goal.clone(), ui.dry_setup());
        let dry = ui.dry.as_ref().filter(|(k, _)| *k == key).map(|(_, d)| d.clone()).unwrap_or_default();
        let lines = if drafts {
            draft_plan(setup.as_ref())
        } else if dry.lines.is_empty() {
            plan(&goal, setup.as_ref(), &taken)
        } else {
            dry.lines.clone()
        };
        let (busy, drafting) = (self.running("launch"), self.running("draft"));
        let label = |text: &str| bay_label(text).px(px(0.0));
        let note = |text: String| div().px(px(4.0)).text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(text);

        // The projects, as a choice: with drafts, none picked lets the brain pick.
        let chosen = setup.as_ref().map(|s| s.name.clone());
        let projects: AnyElement = match ui.setups.as_deref() {
            None => cstat("Asking the tower…", false),
            Some([]) if drafts => v_flex()
                .gap(px(6.0))
                .child(cstat("None saved: the tower's brain picks among the folders under ~/Work. Save your projects, with what each repo is for, and it picks better.", false))
                .child(h_flex().child(tg("to-projects", "Projects", false).on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::Projects, cx)))))
                .into_any_element(),
            Some([]) => cstat("None needed: the tower opens the repos your goal's first line names, on your default account.", false),
            Some(list) => v_flex()
                .gap(px(6.0))
                .child(h_flex().flex_wrap().gap(px(6.0)).children(list.iter().enumerate().map(|(i, s)| {
                    let name = s.name.clone();
                    let on = chosen.as_deref() == Some(s.name.as_str());
                    let toggles = ui.drafts();
                    tg(SharedString::from(format!("setup-{i}")), &s.name, on).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(full) = this.full.as_mut() {
                            // With drafts a second click lets the brain pick again.
                            full.launch.picked = if toggles && on { None } else { Some(name.clone()) };
                        }
                        this.ask_plan(cx);
                        cx.notify();
                    }))
                })))
                .when(drafts, |el| {
                    el.child(note(match &setup {
                        Some(s) if s.account.is_empty() => format!("A draft takes its repos from {} only.", s.name),
                        Some(s) => format!("A draft takes its repos from {} only, on {}.", s.name, s.account),
                        None => "None picked: the tower's brain picks the project, or none, from the ticket.".to_string(),
                    }))
                })
                .into_any_element(),
        };
        let bullets = v_flex().w_full().px(px(14.0)).py(px(12.0)).gap(px(4.0)).rounded(px(14.0)).bg(rgba(SOFT)).text_size(px(13.0)).line_height(px(18.0)).text_color(g.muted).children(
            lines.iter().map(|line| {
                let named = [NAMES, JOINS].into_iter().find_map(|lead| line.strip_prefix(lead).map(|name| (lead, name)));
                let text: AnyElement = match named {
                    Some((lead, name)) => h_flex()
                        .gap(px(4.0))
                        .child(lead.trim_end())
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(g.text).child(name.to_string()))
                        .into_any_element(),
                    None => div().min_w_0().child(line.clone()).into_any_element(),
                };
                h_flex().gap(px(9.0)).items_start().child(div().flex_none().child("•")).child(text)
            }),
        );
        // What the first line is for (the goal itself is in the conversation), until it names the repos.
        let first_line = (!ui.names_repos(&goal)).then(|| {
            if ui.drafts() {
                "A first line that names the repos (\"open sessions in eyzflow and mediaroom\") skips the draft."
            } else {
                "The first line says what to open (\"open sessions in eyzflow and mediaroom\"); the rest can be the whole ticket, and goes to the sessions as it is."
            }
        });

        // What it will do: the plan; or, for a ticket to draft, the draft (or that it is being read).
        let (section, middle): (&str, AnyElement) = match (&draft, drafting) {
            (None, true) => (
                "What the tower will do",
                v_flex()
                    .gap(px(6.0))
                    .child(div().w_full().px(px(14.0)).py(px(12.0)).rounded(px(14.0)).bg(rgba(SOFT)).child(cstat_busy("Reading the ticket…")))
                    .child(note("It takes a little while: the tower's brain reads the ticket, your projects and what each repo is for.".into()))
                    .into_any_element(),
            ),
            (Some(d), _) => ("The draft", self.draft_card(d, drafting)),
            (None, false) => (
                "What the tower will do",
                match dry.error.as_ref().filter(|_| !drafts) {
                    Some(e) => cstat(e.clone(), true),
                    None => v_flex()
                        .gap(px(7.0))
                        .child(bullets)
                        .when(!drafts && !dry.assumed.is_empty(), |el| el.child(note(dry.assumed.clone())))
                        .children(first_line.map(|t| note(t.into())))
                        .into_any_element(),
                },
            ),
        };
        let can = !goal.is_empty() && !busy && !drafting;
        // "Review its PRs", on the draft and on the plan (not while a ticket is only to be drafted).
        let reviewing = ui.review;
        let review_row = (draft.is_some() || !drafts).then(|| {
            h_flex()
                .w_full()
                .gap(px(10.0))
                .items_center()
                .px(px(4.0))
                .child(tg("launch-review", "Review its PRs", reviewing).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(full) = this.full.as_mut() {
                        full.launch.review = !reviewing;
                    }
                    cx.notify();
                })))
                .child(div().flex_1().min_w_0().text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(review_note(reviewing)))
                .into_any_element()
        });
        let mut actions: Vec<AnyElement> = vec![];
        match (&draft, drafts) {
            (Some(_), _) if chatting => {
                // Planned: Launch hands the sessions the plan; Start over ends it.
                let m = missions.clone();
                actions.push(
                    acts_row()
                        .child(mbtn("launch", if busy { "Launching…" } else { "Launch" }, true, None).when(!can, |el| el.opacity(0.5)).when(can, |el| el.on_click(cx.listener(move |this, _, _, cx| this.launch_draft(&m, cx)))))
                        .child(mbtn("start-over", "Start over", false, None).when(busy, |el| el.opacity(0.5)).when(!busy, |el| el.on_click(cx.listener(|this, _, window, cx| this.start_over(window, cx)))))
                        .into_any_element(),
                );
            }
            (Some(_), _) => {
                // Launch it as it is; what to change is said in the conversation.
                actions.push(note("Say in the conversation what to change: the tower's brain redrafts it there, and the card follows.".into()).into_any_element());
                let m = missions.clone();
                actions.push(
                    acts_row()
                        .child(mbtn("launch", if busy { "Launching…" } else { "Launch" }, true, None).when(!can, |el| el.opacity(0.5)).when(can, |el| el.on_click(cx.listener(move |this, _, _, cx| this.launch_draft(&m, cx)))))
                        .child(mbtn("start-over", "Start over", false, None).when(busy, |el| el.opacity(0.5)).when(!busy, |el| el.on_click(cx.listener(|this, _, window, cx| this.start_over(window, cx)))))
                        .into_any_element(),
                );
                // Or see the plan first (a failed one's line, with Plan again, is in the plan's own section).
                if planned.is_none() {
                    actions.push(self.plan_offer(false, can, cx));
                }
            }
            (None, true) => actions.push(
                acts_row()
                    .child(mbtn("draft", if drafting { "Reading the ticket…" } else { "Draft the space" }, true, None).when(!can, |el| el.opacity(0.5)).when(can, |el| el.on_click(cx.listener(|this, _, _, cx| this.draft(None, cx)))))
                    .into_any_element(),
            ),
            (None, false) => {
                let m = missions.clone();
                actions.push(
                    acts_row()
                        .child(mbtn("launch", if busy { "Launching…" } else { "Launch" }, true, None).when(!can, |el| el.opacity(0.5)).when(can, |el| el.on_click(cx.listener(move |this, _, _, cx| this.launch(&m, cx)))))
                        .into_any_element(),
                );
            }
        }
        // The plan of this draft, under the card (the talk about it is in the conversation).
        let plan_items: Vec<AnyElement> = match (&planned, &draft) {
            (Some(p), Some(_)) => self.plan_section(p, can, cx),
            _ => vec![],
        };
        // A plan of another ticket is still open: back to it, or close it.
        let stale_line = stale.map(|p| {
            let space = p.draft["space"].as_str().unwrap_or("its space").to_string();
            let (back, close) = (p.clone(), p.id.clone());
            h_flex()
                .w_full()
                .flex_wrap()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(12.0))
                .bg(rgba(SOFT))
                .text_size(px(13.0))
                .text_color(g.muted)
                .child(div().min_w_0().child(format!("A plan of “{space}” is still open ({}, {}).", p.form_word(), p.status)))
                .child(tg("plan-back", "Back to it", false).on_click(cx.listener(move |this, _, window, cx| this.plan_back(&back, window, cx))))
                .child(tg("plan-close-stale", "Close it", false).on_click(cx.listener(move |this, _, _, cx| this.plan_close(&close, cx))))
                .into_any_element()
        });
        let body = v_flex()
            .w_full()
            .max_w(px(760.0))
            .gap(px(16.0))
            .children(stale_line)
            .child(v_flex().gap(px(7.0)).child(label("Project")).child(projects).children(self.said("setups")))
            .child(v_flex().gap(px(7.0)).child(label(section)).child(middle).children(review_row).children(self.said("draft")))
            .when(!plan_items.is_empty(), |el| el.child(v_flex().gap(px(10.0)).children(plan_items)))
            .child(v_flex().gap(px(10.0)).children(actions))
            .children(self.said("plan"))
            .children(self.said("plan-close"))
            .children(self.said("launch"))
            .into_any_element();
        (head, vec![body])
    }
}

impl Panel {
    /// The conversation that makes the space, as it stands now (`story_with`).
    pub(super) fn story_now(&self, cx: &Context<Self>) -> Vec<Told> {
        let Some(full) = self.full.as_ref() else { return vec![] };
        let ticket = full.launch.goal.read(cx).value().trim().to_string();
        let plan = self.plan_for(&ticket, cx).filter(|_| full.launch.draft.is_some());
        story_with(&full.launch.story, plan.as_ref())
    }

    /// Where the input's words go in New space, as its placeholder says.
    pub(super) fn new_hint(&self, cx: &Context<Self>) -> String {
        let Some(full) = self.full.as_ref() else { return String::new() };
        let ticket = full.launch.goal.read(cx).value().trim().to_string();
        let ui = &full.launch;
        // The column's input is narrow in a narrow window: the short words.
        let narrow = full.talk_w < 360.0;
        let pick = |wide: &str, short: &str| if narrow { short.to_string() } else { wide.to_string() };
        if self.plan_for(&ticket, cx).is_some_and(|p| p.status != "failed" && p.address.is_some()) {
            pick("Ask about the plan or change it…", "To the planner…")
        } else if ui.drafts() && !ticket.is_empty() && !ui.names_repos(&ticket) && ui.draft.is_some() {
            pick("Say what to change…", "What to change…")
        } else if ticket.is_empty() {
            pick("Paste the ticket, or say what to get done…", "The ticket, or the goal…")
        } else {
            pick("Add to the ticket, and it is drafted…", "More of the ticket…")
        }
    }

    /// New space's conversation, in the column a space's has, `width`
    /// points wide: what you said and what the tower and the planner said
    /// back, oldest first, what is under way, and the input at its foot.
    pub(super) fn new_chat_column(&mut self, width: f32, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let story = self.story_now(cx);
        let ticket = self.full.as_ref().map(|f| f.launch.goal.read(cx).value().trim().to_string()).unwrap_or_default();
        let plan = self.plan_for(&ticket, cx);
        let drafted = self.full.as_ref().and_then(|f| f.launch.draft.as_ref().map(|(_, d)| d.space.clone()));
        let redrafting = drafted.is_some();
        // A new message, or one now awaited: the column goes to its end.
        let waiting = self.running("draft") || self.running("launch") || self.running("plan") || plan.as_ref().is_some_and(|p| p.answering);
        if let Some(f) = self.full.as_mut() {
            let key = (Some(String::new()), story.len() + usize::from(waiting), story.last().map(|t| t.at).unwrap_or(0));
            if f.chat_seen.as_ref() != Some(&key) {
                f.chat_seen = Some(key);
                f.chat_scroll.scroll_to_bottom();
                window.request_animation_frame();
            }
        }
        let mut rows: Vec<AnyElement> = story.iter().map(super::talk::told_row).collect();
        let busy = |text: String| div().px(px(4.0)).child(cstat_busy(text)).into_any_element();
        if self.running("draft") {
            rows.push(busy(if redrafting { "The tower's brain is redrafting it…".into() } else { "The tower's brain is reading the ticket…".into() }));
        }
        if self.running("plan") {
            rows.push(busy("Starting the planner…".into()));
        }
        if self.running("plan-say") {
            rows.push(busy("Handing it to the planner…".into()));
        }
        if let Some(p) = &plan {
            if p.status == "writing" && !p.blocked {
                rows.push(busy(plan_line(p).0));
            } else if p.answering {
                rows.push(busy(if p.planner == "working" { "The planner is answering…".into() } else { "With the planner: it answers once its turn is over…".into() }));
            }
        }
        if self.running("launch") {
            rows.push(busy("Launching…".into()));
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .w_full()
                    .p(px(14.0))
                    .rounded(px(14.0))
                    .border_1()
                    .border_dashed()
                    .border_color(rgba(LINE))
                    .text_size(px(13.0))
                    .line_height(px(19.0))
                    .text_color(g.muted)
                    .child("Say below what the space should get done, or paste the ticket. The tower's brain drafts it here, and you change it in words here too. When you launch it, this conversation goes with the space, at the top of its own: how it was created.")
                    .into_any_element(),
            );
        }
        let mut status: Vec<AnyElement> = self.said("plan-say").into_iter().map(|e| div().px(px(4.0)).child(e).into_any_element()).collect();
        if let Some((ok, msg)) = self.status.clone() {
            status.push(div().px(px(4.0)).child(cstat(msg, !ok)).into_any_element());
        }
        let scroll = self.full.as_ref().map(|f| f.chat_scroll.clone());
        v_flex()
            .w(px(width))
            .flex_none()
            .h_full()
            .px(px(14.0))
            .pb(px(16.0))
            .gap(px(10.0))
            .border_r_1()
            .border_color(rgba(LINE))
            .child(div().pt(px(22.0)).px(px(4.0)).child(bay_label(&match &drafted {
                Some(name) => format!("Conversation · {name}, not launched yet"),
                None => "Conversation · the new space".to_string(),
            })))
            .child(
                div()
                    .id("chat-new-space")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when_some(scroll, |el, s| el.track_scroll(&s))
                    .child(v_flex().w_full().gap(px(8.0)).pb(px(4.0)).children(rows)),
            )
            .children(status)
            .child(self.input(window, cx))
            .into_any_element()
    }

    /// `HERDER_TOWER_ORB_TALK` (see `talk_from_start`): its next line, once
    /// the tower is on nothing and the last line was a moment ago.
    fn talk_tick(&mut self, missions: &Missions, window: &mut Window, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_ref() else { return };
        let ui = &full.launch;
        if ui.talk_hook.is_empty() || ui.draft_known.is_none() || ["draft", "launch", "plan", "plan-say", "setups"].iter().any(|p| self.running(p)) {
            return;
        }
        if ui.talk_at.is_some_and(|t| std::time::Instant::now() < t) {
            window.request_animation_frame();
            return;
        }
        let Some(full) = self.full.as_mut() else { return };
        let Some(line) = full.launch.talk_hook.pop_front() else { return };
        full.launch.talk_at = Some(std::time::Instant::now() + Duration::from_millis(1500));
        let left = full.launch.talk_hook.len();
        eprintln!("[orb] talk: {line} ({left} left)");
        if let Some(secs) = line.strip_prefix("!wait").map(str::trim) {
            full.launch.talk_at = Some(std::time::Instant::now() + Duration::from_secs(secs.parse().unwrap_or(5)));
        } else if line == "!launch" {
            let ticket = full.launch.goal.read(cx).value().trim().to_string();
            if full.launch.draft_of(&ticket).is_some() || self.plan_for(&ticket, cx).is_some() {
                self.launch_draft(missions, cx);
            } else {
                self.launch(missions, cx);
            }
        } else if let Err(why) = self.launch_say(&line, window, cx) {
            eprintln!("[orb] talk: not taken: {why}");
        }
        window.request_animation_frame();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_is_named_from_its_goal() {
        assert_eq!(slug("Case exports as PDF", &[]), "case-exports");
        assert_eq!(slug("  Voice clips, for the Ingest tour!  ", &[]), "voice-clips");
        assert_eq!(slug("Rework", &[]), "rework");
        assert_eq!(slug("PR #40: fix it", &[]), "pr-40");
        // As the tower names it: small words are passed over.
        assert_eq!(slug("Fix the login redirect", &[]), "fix-login");
        assert_eq!(slug("A plan for the storefront live editor", &[]), "plan-storefront");
        // Nothing to name it by.
        assert_eq!(slug("", &[]), "space");
        assert_eq!(slug("— ¿?", &[]), "space");
        // A name that is taken (in any case) gets a number.
        assert_eq!(slug("Case exports as PDF", &["sandbox", "Case-Exports"]), "case-exports-2");
        assert_eq!(slug("Case exports as PDF", &["case-exports", "case-exports-2"]), "case-exports-3");
        assert_eq!(slug("", &["space"]), "space-2");
        // A goal that opens by asking for sessions is not named "open-session".
        assert_eq!(slug("open a session at eyzflow and mediaroom. this is the ticket: EYZFlow Support", &[]), "eyzflow-mediaroom");
    }

    #[test]
    fn the_towers_own_plan_is_read_from_its_receipt() {
        // `tower launch "Fix the login redirect" --dry-run --json`.
        let receipt = serde_json::json!({
            "space": "fix-login", "goal": "Fix the login redirect", "made": true, "dry_run": true, "agenda": null,
            "sessions": [
                { "name": "fix-login-eyzflow", "repo": "~/Work/eyz/eyzflow", "address": null, "account": "orca", "worktree": "/w/.tower-worktrees/fix-login/eyzflow", "branch": "fix-login/eyzflow", "port": null, "task": null, "error": null },
                { "name": "fix-login-mediaroom", "repo": "~/Work/eyz/mediaroom/", "address": null, "account": "orca", "worktree": "/w/.tower-worktrees/fix-login/mediaroom", "branch": "fix-login/mediaroom", "port": null, "task": null, "error": null },
            ],
            "assumed": { "setup": "eyzflow + mediaroom on orca", "setup_why": "the first of 2", "account": "orca", "account_why": "the setup's", "name": "fix-login", "name_why": "the goal's first two words, small ones left out", "worktrees": true, "worktrees_why": "one per repo, on a branch of its own" },
        });
        let dry = plan_from(&receipt).unwrap();
        assert_eq!(
            dry.lines,
            [
                "Name the space fix-login",
                "Open 2 sessions on orca:",
                "eyzflow in ~/Work/eyz/eyzflow",
                "mediaroom in ~/Work/eyz/mediaroom/",
                "Give each its own worktree, branch and dev port",
                "Introduce them to each other and hand out the goal"
            ]
        );
        assert_eq!(dry.assumed, "It takes the setup “eyzflow + mediaroom on orca” (the first of 2).");
        // A standing space joined by name: no worktrees, and it says why; an account it picked itself is said too.
        let mut joined = receipt.clone();
        joined["made"] = false.into();
        joined["space"] = "M5 Pro".into();
        joined["sessions"].as_array_mut().unwrap().truncate(1);
        joined["sessions"][0]["worktree"] = Value::Null;
        joined["assumed"]["worktrees_why"] = "none: it is a standing space, so each session works in its repo itself".into();
        joined["assumed"]["account_why"] = "the default one: the setup names none".into();
        joined["assumed"]["setup_why"] = "the one named".into();
        let dry = plan_from(&joined).unwrap();
        assert_eq!(dry.assumed, "The account: the default one: the setup names none.", "a setup that was named to it isn't its own choice");
        assert_eq!(dry.lines, ["Join the space M5 Pro", "Open 1 session on orca:", "eyzflow in ~/Work/eyz/eyzflow", "No worktrees: it is a standing space, so each session works in its repo itself", "Hand out the goal"]);
        assert!(dry.assumed.ends_with("The account: the default one: the setup names none."), "{}", dry.assumed);
        // Not a receipt: no word.
        assert!(plan_from(&serde_json::json!({})).is_none() && plan_from(&serde_json::json!({ "space": "x", "sessions": [] })).is_none());
        // After the launch: what opened, where, and what didn't.
        let mut done = receipt.clone();
        done["dry_run"] = false.into();
        done["sessions"][0]["address"] = "t7".into();
        done["sessions"][0]["port"] = 3004.into();
        done["sessions"][1]["error"] = "hc run: no".into();
        assert_eq!(
            launched_line(&done).unwrap(),
            "Launched fix-login: 1 session on orca (eyzflow), in its own worktree (port 3004). mediaroom didn’t open: hc run: no. It took the setup “eyzflow + mediaroom on orca”. Nothing was asked. Change the goal from here."
        );
        done["sessions"][1]["error"] = Value::Null;
        done["sessions"][1]["port"] = 3005.into();
        assert!(launched_line(&done).unwrap().starts_with("Launched fix-login: 2 sessions on orca (eyzflow, mediaroom), each in its own worktree (ports 3004, 3005)."));
        assert!(launched_line(&serde_json::json!({})).is_none());
    }

    #[test]
    fn the_plan_says_what_a_launch_does() {
        let pair = Setup { name: "eyzflow + mediaroom on orca".into(), account: "orca".into(), repos: vec!["~/Work/eyz/eyzflow".into(), "~/Work/eyz/mediaroom/".into()], ..Default::default() };
        assert_eq!(
            plan("Case exports as PDF", Some(&pair), &["isa"]),
            [
                "Name the space case-exports",
                "Open 2 sessions on orca: eyzflow, mediaroom",
                "Give each its own worktree, branch and dev port",
                "Introduce them to each other and hand out the goal",
            ]
        );
        let one = Setup { name: "landing".into(), account: String::new(), repos: vec!["landing-page".into()], ..Default::default() };
        assert_eq!(
            plan("New product drawings", Some(&one), &["product-drawings"]),
            ["Name the space product-drawings-2", "Open 1 session: landing-page", "Give it its own worktree, branch and dev port", "Hand out the goal"]
        );
        // No setup: only what is known, the name; and no goal yet, not even that.
        assert_eq!(plan("Rework", None, &[]), ["Name the space rework"]);
        assert_eq!(plan(" ", None, &[]), ["Name the space from its goal"]);
        assert_eq!((repo_name("~/Work/eyz/eyzflow"), repo_name("eyzflow"), repo_name("/a/b/")), ("eyzflow", "eyzflow", "b"));
    }

    #[test]
    fn the_towers_setups_read_in_either_shape() {
        let wrapped = r#"{"setups":[{"name":"pair","account":"orca","repos":["~/a","~/b"]},{"name":"solo"},{"account":"x"},{"name":" "}]}"#;
        let list = setups_from(wrapped);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], Setup { name: "pair".into(), account: "orca".into(), repos: vec!["~/a".into(), "~/b".into()], roles: vec![String::new(); 2], ..Default::default() });
        assert_eq!(list[1], Setup { name: "solo".into(), ..Default::default() });
        assert_eq!(setups_from(r#"[{"name":"pair","account":"orca","repos":[]}]"#).len(), 1);
        // A tower without the command prints nothing, or something else.
        assert!(setups_from("").is_empty() && setups_from("no setups yet").is_empty() && setups_from("{}").is_empty());
        // A project: what it is, and each repo's role beside it.
        let with = r#"{"setups":[{"name":"EYZFlow","account":"default","about":" the video platform ","repos":["~/a","~/b"],"roles":{"~/b":"the backend"}}]}"#;
        let list = setups_from(with);
        assert_eq!((list[0].about.as_str(), list[0].roles.clone()), ("the video platform", vec![String::new(), "the backend".to_string()]));
    }

    #[test]
    fn a_ticket_that_names_no_repo_is_drafted_and_launched_as_its_card_says() {
        // `tower launch "<ticket>" --draft --json`.
        let draft = serde_json::json!({
            "space": "support-translation", "goal": "EYZFlow Support – Translate messages", "done_when": "Messages show translated; replies go out translated",
            "made": true, "dry_run": true, "draft": true, "project": "EYZFlow", "notes": "Mediaroom may be needed.", "dropped": ["/x/y: not a project folder here"],
            "sessions": [
                { "name": "support-translation-eyz-customer-support", "repo": "/w/eyz/eyz-customer-support", "account": "default", "why": "the inbox service" },
                { "name": "support-translation-eyzflow-admin-dashboard", "repo": "/w/eyz/eyzflow-admin-dashboard", "account": "default", "why": "the agents' UI" },
            ],
            "assumed": { "setup": "EYZFlow", "account": "default", "name": "support-translation" },
        });
        let d = draft_from(&draft).unwrap();
        assert_eq!((d.space.as_str(), d.goal.as_str(), d.project.as_deref(), d.account.as_deref()), ("support-translation", "EYZFlow Support – Translate messages", Some("EYZFlow"), Some("default")));
        assert_eq!(d.sessions, [("/w/eyz/eyz-customer-support".to_string(), "the inbox service".to_string()), ("/w/eyz/eyzflow-admin-dashboard".to_string(), "the agents' UI".to_string())]);
        assert_eq!((d.notes.as_str(), d.dropped.len()), ("Mediaroom may be needed.", 1));
        // Launch runs exactly the card.
        let ticket = "EYZFlow Support – Translate messages\n\nCustomers write in many languages.";
        assert_eq!(
            confirm_args(ticket, &d, None, true),
            [
                "launch", ticket, "--repo", "/w/eyz/eyz-customer-support", "--repo", "/w/eyz/eyzflow-admin-dashboard", "--name", "support-translation", "--title", "EYZFlow Support – Translate messages",
                "--done-when", "Messages show translated; replies go out translated", "--account", "default", "--setup", "EYZFlow", "--review", "--json",
            ]
        );
        // The card's "Review its PRs" switch, off: no reviewer.
        let off = confirm_args(ticket, &d, None, false);
        assert_eq!(&off[off.len() - 2..], ["--no-review", "--json"]);
        assert!(!off.contains(&"--review".to_string()));
        assert!(review_note(true).contains("critical issues only") && review_note(false).starts_with("No reviewer"));
        // A draft is asked for the ticket (and the project picked); a redraft brings the words and the draft before.
        assert_eq!(draft_args(ticket, None, None), ["launch", ticket, "--draft", "--json"]);
        let again = draft_args(ticket, Some("EYZFlow"), Some(("leave the dashboard out", &d)));
        assert_eq!(&again[4..8], ["--setup", "EYZFlow", "--adjust", "leave the dashboard out"]);
        assert_eq!(again[8], "--prior");
        assert_eq!(serde_json::from_str::<Value>(&again[9]).unwrap(), draft);
        // Not a draft: none.
        assert!(draft_from(&serde_json::json!({ "space": "x", "sessions": [] })).is_none() && draft_from(&serde_json::json!({})).is_none());
        // What the tower will do before the draft: read it, from a project if one is picked.
        let picked = Setup { name: "EYZFlow".into(), ..Default::default() };
        assert!(draft_plan(Some(&picked))[0].contains("the repos of EYZFlow"));
        assert!(draft_plan(None)[0].contains("from your projects") && draft_plan(None).last().unwrap().contains("Nothing opens"));
    }

    /// New space's conversation: each draft is said in it (a redraft with
    /// what changed), and a plan's thread and its being ready join it in time order.
    #[test]
    fn the_conversation_says_each_draft_and_takes_in_the_plans_thread() {
        let first = draft_from(&serde_json::json!({ "space": "scratch-check", "goal": "Check New space.", "done_when": "", "sessions": [{ "repo": "/w/tower-scratch", "why": "the throwaway repo" }], "assumed": { "account": "default" } })).unwrap();
        assert_eq!(
            draft_said(&first, None),
            "Drafted scratch-check: 1 session (tower-scratch) on default. Goal: Check New space. Done when: not said (the ticket has no acceptance criteria). Launch it as it is, or say here what to change."
        );
        let mut second = first.clone();
        second.space = "ns-check".into();
        second.sessions.push(("/w/eyzflow".into(), "the app".into()));
        second.project = Some("EYZFlow".into());
        second.done_when = "it answers ok".into();
        let said = draft_said(&second, Some(&first));
        assert!(said.starts_with("Redrafted ns-check: 2 sessions (tower-scratch, eyzflow), each in its own worktree on default, from EYZFlow. Goal: Check New space. Done when: it answers ok."), "{said}");
        assert!(said.ends_with("Changed: named ns-check (was scratch-check); what counts as done; adds eyzflow; the project is EYZFlow."), "{said}");
        assert!(draft_said(&first, Some(&first)).ends_with("Nothing on the card changed."));
        let mut left = first.clone();
        left.dropped = vec!["/nowhere/x: not a project folder here".into()];
        assert!(draft_said(&left, None).contains("Left out /nowhere/x: not a project folder here. Launch it"));

        // The plan's thread (you and the planner) and when it was ready, in time order among the rest.
        let story = vec![Told { who: "you".into(), text: "the ticket".into(), at: 10 }, Told { who: "tower".into(), text: "Drafted…".into(), at: 20 }, Told { who: "you".into(), text: "Plan it first, as a page.".into(), at: 30 }];
        let p = PlanView {
            id: "p3".into(),
            form: "page".into(),
            status: "ready".into(),
            url: Some("https://claude.ai/artifact/x".into()),
            ready_ms: Some(40),
            thread: vec![model::PlanMsg { from: "user".into(), text: "why two sessions?".into(), at: 50 }, model::PlanMsg { from: "planner".into(), text: "One per repo.".into(), at: 60 }],
            ..Default::default()
        };
        let all = story_with(&story, Some(&p));
        assert_eq!(all.iter().map(|t| (t.who.as_str(), t.at)).collect::<Vec<_>>(), [("you", 10), ("tower", 20), ("you", 30), ("tower", 40), ("you", 50), ("planner", 60)]);
        assert!(all[3].text.starts_with("The plan is ready, a page on your claude.ai account: https://claude.ai/artifact/x."));
        assert_eq!(story_with(&story, None), story);
    }

    #[test]
    fn a_draft_is_planned_talked_over_and_launched_with_its_plan() {
        let draft = serde_json::json!({ "space": "msg-translation", "goal": "Translate", "done_when": "", "sessions": [{ "repo": "/w/eyz/eyz-customer-support", "why": "the inbox" }], "assumed": { "account": "default" } });
        let d = draft_from(&draft).unwrap();
        // Plan as a page / as a whiteboard: the card's draft, the ticket in its file.
        let args = plan_args("whiteboard", &d, "/tmp/t.md");
        assert_eq!(&args[..4], ["plan", "new", "--form", "whiteboard"]);
        assert_eq!((args[4].as_str(), args[6].as_str(), args[7].as_str(), args[8].as_str()), ("--draft", "--ticket-file", "/tmp/t.md", "--json"));
        assert_eq!(serde_json::from_str::<Value>(&args[5]).unwrap(), draft);
        // Send: the user's words, whatever they start with.
        assert_eq!(say_args("p3", "--json is a word"), ["plan", "say", "--as", "user", "p3", "--", "--json is a word"]);
        let parsed = crate::plans::parse_args(&say_args("p3", "--json is a word")[1..]).unwrap();
        assert_eq!((parsed.id.as_deref(), parsed.words.as_str(), parsed.who), (Some("p3"), "--json is a word", Some(crate::plans::Who::User)));
        // Launch with a published plan: the sessions get it.
        let launch = confirm_args("T", &d, Some("p3"), true);
        assert_eq!(&launch[launch.len() - 4..], ["--plan", "p3", "--review", "--json"]);
        assert!(!confirm_args("T", &d, None, true).contains(&"--plan".to_string()));
        // How a plan that isn't ready stands, in a line.
        let mut p = PlanView { id: "p3".into(), status: "writing".into(), planner: "starting".into(), ..Default::default() };
        assert_eq!(plan_line(&p), ("The tower's brain is starting the planner…".to_string(), false));
        p.planner = "working".into();
        assert_eq!(plan_line(&p).0, "The tower's brain is writing the plan… (it reads the repos first: about five minutes)");
        p.blocked = true;
        assert!(plan_line(&p).0.starts_with("The planner waits on a question in its terminal"));
        p.status = "failed".into();
        p.why = "no Artifact tool.".into();
        assert_eq!(plan_line(&p), ("The plan didn't come: no Artifact tool.".to_string(), true));
    }

    #[test]
    fn a_goal_whose_first_line_names_its_repos_skips_the_draft() {
        let receipt = |from: Option<&str>, why: &str| {
            let mut r = serde_json::json!({
                "space": "x", "made": true, "done_when": "", "sessions": [{ "repo": "/w/a", "worktree": null }],
                "assumed": { "setup": "the goal's a", "setup_why": why, "account": "default", "account_why": "the default one: the goal names none" },
            });
            if let Some(f) = from {
                r["repos_from"] = f.into();
            }
            r
        };
        assert!(plan_from(&receipt(Some("first-line"), "the repos its first line names")).unwrap().first_line);
        assert!(!plan_from(&receipt(Some("setup"), "the first of 2")).unwrap().first_line, "a project's: drafted");
        // A tower from before says it only in its words.
        assert!(plan_from(&receipt(None, "the one that has the repos the goal names")).unwrap().first_line);
        assert!(!plan_from(&receipt(None, "the only one")).unwrap().first_line);
        // What counts as done, when the tower says it.
        let mut done = receipt(Some("first-line"), "the repos its first line names");
        done["done_when"] = "the inbox shows both".into();
        assert_eq!(plan_from(&done).unwrap().lines.last().map(String::as_str), Some("Done when: the inbox shows both"));
    }
}
