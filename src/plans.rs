//! A space's plan, before it is launched (`docs/quiet-tower.md`, "Step 9",
//! "The plan, and the chat about it"): New space's **Plan as a page** or
//! **Plan as a whiteboard** has the tower's planner write the full plan of
//! a draft, publish it as an Artifact on the user's claude.ai account, and
//! talk it over with the user, before anything opens.
//!
//! - **The planner** is one Claude Code terminal the tower owns, `tower-
//!   planner` (a `tower-…` name: never a session of the user's, never
//!   introduced, never in a space's members; `brains` takes it for no
//!   space's brain), started on the default account with the brains' model
//!   and effort (`argv`), in its own folder `<state dir>/plans/<id>/`:
//!   `ticket.md`, `draft.json`, `instructions.md` (also its `CLAUDE.md`)
//!   and its permissions (`settings`: it may read anywhere, write only in
//!   its folder, run `tower plan …`, and publish its artifact; it may never
//!   launch, start, queue, or edit a repo). One runs at a time: a new plan,
//!   Start over or Launch ends it (`hc kill`, the terminal removed).
//! - **Its first task** (`first_task`): write the plan (per session what it
//!   does and in what order, how the repos fit together, the risks, the
//!   open questions, how it is tested) into `plan.md`, publish it as the
//!   form asked (`Form`: an HTML page, or a Whiteboard from its Artifact
//!   type), and say so with `tower plan published <id> --url <url>`. Then
//!   it waits: what the user says reaches it as a prompt through the send
//!   queue (`chat_prompt`), and it answers with `tower plan say`; a change
//!   agreed is `tower plan draft` (a revised draft) and the same artifact
//!   republished.
//! - **The store** (`Book`): every plan, kept by the daemon in
//!   `<state dir>/plans.json` and shown in `state.json` as `plan` (the open
//!   one, `Plan::view`). The CLI (`Args`, `cmd`) talks to the daemon.
//! - **The PRs it predicts** (`crate::prscore`): when the plan changes
//!   code, the planner lists the pull requests it will open (`tower plan
//!   prs <id> --json-file prs.json`: one per PR, its repo, its branch, dev
//!   first, its title, what it changes, roughly which files, and whether it
//!   is a migration, a breaking change or on the escalate list). The tower
//!   checks and scores each (needs review, or low risk, and why), keeps
//!   them with the plan (`Plan::prs`), and prints the scores for the plan's
//!   page. When a session of the launched space opens a PR, it is matched to
//!   its prediction (`Book::match_pr`) and carries the score.
//! - **A launch with a plan** (`tower launch … --plan <id>`): each session's
//!   first instruction carries the plan's link and its part of `plan.md`
//!   (`part_for`, at most `BRIEF_CHARS`), and the plan is closed.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The planner's terminal: one at a time.
pub const PLANNER: &str = "tower-planner";
/// The labels its terminal carries: what it is, and which plan it writes.
pub const LABEL_ROLE: &str = "tower.role";
pub const LABEL_PLAN: &str = "tower.plan";
/// The account's Artifact type a whiteboard plan is made from.
pub const WHITEBOARD_TYPE: &str = "https://claude.ai/artifact/PLsEyfjXbDUM1iasW7rjLR";
/// How much of the plan a launched session's first instruction carries.
pub const BRIEF_CHARS: usize = 6_000;
/// The planner's environment says which plan it writes: its `tower plan say` is its answer.
pub const ENV_PLAN: &str = "HERDER_TOWER_PLAN";
/// Closed plans kept in the store (the newest).
const KEEP_CLOSED: usize = 20;
/// A chat message, at most (a pasted log is cut).
const MSG_CHARS: usize = 4_000;
/// A chat thread, at most this many messages (the oldest go).
const THREAD_MAX: usize = 200;

/// The planner is a terminal of this name: a `tower-…` terminal that is no
/// space's brain.
pub fn is_planner(name: &str) -> bool {
    name == PLANNER
}

/// The form a plan is published in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Form {
    /// An HTML page Artifact.
    #[default]
    Page,
    /// A Whiteboard Artifact (from the account's Artifact type).
    Whiteboard,
}

impl Form {
    pub fn parse(word: &str) -> Result<Form> {
        match word.trim().to_lowercase().as_str() {
            "page" | "html" => Ok(Form::Page),
            "whiteboard" | "board" => Ok(Form::Whiteboard),
            other => bail!("--form is page or whiteboard, not {other:?}"),
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Form::Page => "page",
            Form::Whiteboard => "whiteboard",
        }
    }
}

/// Where a plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The planner writes it (or is starting).
    #[default]
    Writing,
    /// Published: its artifact has a link.
    Ready,
    /// The planner couldn't (its words in `why`), or ended before it published.
    Failed,
    /// Ended: Start over, another plan.
    Closed,
    /// Launched: the plan of its space while the work goes on, revised as
    /// the sessions learn things (`findings`, `revisions`).
    Live,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Writing => "writing",
            Status::Ready => "ready",
            Status::Failed => "failed",
            Status::Closed => "closed",
            Status::Live => "live",
        }
    }
}

/// Who said a line of the chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    User,
    Planner,
}

impl Who {
    pub fn parse(word: &str) -> Result<Who> {
        match word.trim().to_lowercase().as_str() {
            "user" | "me" | "you" => Ok(Who::User),
            "planner" | "tower" => Ok(Who::Planner),
            other => bail!("--as is user or planner, not {other:?}"),
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Who::User => "user",
            Who::Planner => "planner",
        }
    }
}

/// One line of the chat about a plan.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Msg {
    /// `user` or `planner`.
    pub from: String,
    pub text: String,
    pub at: u64,
}

/// A plan, as the store keeps it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Plan {
    /// `p1`, `p2`, …
    pub id: String,
    pub form: Form,
    pub status: Status,
    /// The ticket as the user pasted it.
    pub ticket: String,
    /// The draft (the receipt `tower launch --draft --json` prints), as the
    /// planner last revised it.
    pub draft: Value,
    /// How many times the planner revised the draft.
    pub revision: u32,
    /// The planner's terminal (None while it starts, and once it is gone).
    pub address: Option<String>,
    /// How the planner stands, as Herdr says: `starting`, `working`,
    /// `idle`, `blocked` (a dialog in its terminal), `gone`.
    pub planner: String,
    /// Its folder.
    pub dir: String,
    /// The artifact, once published.
    pub url: Option<String>,
    /// Why it failed, or what the user should know (the planner ended).
    pub why: String,
    /// The chat, oldest first.
    pub thread: Vec<Msg>,
    pub created_ms: u64,
    pub ready_ms: Option<u64>,
    pub updated_ms: u64,
    pub closed_ms: Option<u64>,
    /// The space launched with it.
    pub launched: Option<String>,
    /// The pull requests it predicts, each with its review score (`tower plan prs`).
    pub prs: Vec<crate::prscore::Predicted>,
    /// Its milestones, in order (`tower plan stages`: see `docs/planning-v2.md`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<Stage>,
    /// What it takes for granted, each with how and when it is checked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<Assumption>,
    /// What it can't decide alone: each answered by the tower or the user, and written back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
    /// What the sessions learned while working that bears on the plan.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// Every change to its structure after the first, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub revisions: Vec<Revision>,
    /// Its version: 1 once it has stages, one more for each revision applied.
    #[serde(skip_serializing_if = "is_zero")]
    pub version: u32,
    /// The critic's read of the draft, before its questions go out.
    #[serde(skip_serializing_if = "Critic::is_none")]
    pub critic: Critic,
    /// The account its planner runs on (none: the first one not at its limit).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub account: String,
    /// Spike stages done whose findings wait for a revision (or the
    /// planner's word that nothing changes): the stages after them wait.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spikes_pending: Vec<String>,
    /// Where each stage stood when last looked at, to tell what changed.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub stage_seen: std::collections::BTreeMap<String, String>,
}

/// A separate reviewer's read of a draft plan, before the user sees it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Critic {
    /// "" (not yet), `reading`, `done`, `failed`.
    pub status: String,
    #[serde(skip_serializing_if = "is_zero_ms")]
    pub at_ms: u64,
    /// The gaps it found: a stage without real validation, an assumption
    /// nobody checks, a question the plan should ask, …
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
}

fn is_zero_ms(n: &u64) -> bool {
    *n == 0
}

impl Critic {
    pub fn is_none(&self) -> bool {
        self.status.is_empty()
    }

    /// Its questions may go out: it has read the plan (or couldn't).
    pub fn over(&self) -> bool {
        self.status == "done" || self.status == "failed"
    }
}

/// The prompt for the critic: the draft plan's markdown and its
/// structure; it answers JSON only.
pub fn critic_prompt(p: &Plan, md: &str) -> String {
    let structure = json!({ "stages": p.stages, "assumptions": p.assumptions, "questions": p.questions, "prs": p.prs.iter().map(|x| json!({ "n": x.n, "repo": x.repo, "title": x.title, "checks": x.checks.len() })).collect::<Vec<_>>() });
    let md: String = md.chars().take(24_000).collect();
    format!(
        "You review a software plan before the user sees it, as a critic: you find what is missing, you don't rewrite it. The plan has stages (each with what counts as done, pre-checks before its work and post-checks after), assumptions (each with how it is checked) and questions for the user.\n\n\
         Find only real gaps, at most 8, most important first:\n\
         - a stage whose checks wouldn't really show it is done (vague, untestable, no evidence);\n\
         - risky or unknown work that should be a spike stage first and isn't;\n\
         - an assumption the plan relies on that nobody checks, or that is missing;\n\
         - a question the user must answer that the plan doesn't ask, or a question it asks that it could settle itself;\n\
         - a PR no stage delivers, or a stage with no PR where code changes.\n\
         Say each in one sentence, concretely (name the stage, check, assumption or question). No praise, no style notes. None found: an empty list.\n\n\
         Answer with JSON only: {{\"gaps\": [\"…\"]}}\n\n## The goal\n\n{}\n\n## The structure\n\n{}\n\n## The plan\n\n{}\n",
        p.draft["goal"].as_str().unwrap_or(&p.ticket),
        serde_json::to_string_pretty(&structure).unwrap_or_default(),
        md
    )
}

/// The critic's gaps, from its answer.
pub fn parse_critic(stdout: &str) -> Result<Vec<String>> {
    let v = crate::ticket::parse_reply(stdout)?;
    let Some(gaps) = v["gaps"].as_array() else { bail!("the critic's answer has no gaps list") };
    Ok(gaps.iter().filter_map(Value::as_str).map(|g| g.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|g| !g.is_empty()).take(8).collect())
}

/// What the planner is told of the critic's gaps.
pub fn critic_note(p: &Plan, gaps: &[String], tower: &str) -> String {
    let t = tower_word(tower);
    let list: String = gaps.iter().enumerate().map(|(i, g)| format!("\n{}. {g}", i + 1)).collect();
    format!(
        "[plan {}] A critic read the plan before the user sees it and found {} gap{}:{list}\n\nFix what is real: change plan.md, send the stages again (`{t} plan stages {} --json-file stages.json`) when they change (add a question there for one the user must answer), republish the same artifact and run `{t} plan published {} --url <its link>`. What you leave as it is, say why in a line with `{t} plan say {} \"…\"`. The questions go out to the user once you have sent the stages again, or now if nothing changes.",
        p.id,
        gaps.len(),
        if gaps.len() == 1 { "" } else { "s" },
        p.id,
        p.id,
        p.id
    )
}

/// A session's Handoff block: how it reports back on a plan's task, at the
/// end of its turn.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Handoff {
    pub task: String,
    pub plan: String,
    pub stage: String,
    /// `done`, `partial`, `blocked`, `failed`.
    pub status: String,
    pub done: String,
    pub evidence: String,
    pub left: String,
    pub uncertain: String,
    /// (kind, text): what it found that bears on the plan.
    pub findings: Vec<(String, String)>,
    pub needs: String,
    pub next: String,
}

/// The block a session ends its turn with, as it is asked for.
pub const HANDOFF_FORMAT: &str = "Handoff <task> · plan <plan> · stage <stage>\nStatus: done | partial | blocked | failed\nDone: <what you did>\nEvidence: <commands run and their result, test output, a PR, a screenshot's path>\nLeft: <what is left of this task>\nUncertain: <what you are not sure of; none>\nFindings: <one per line, \"- <kind>: <what>\", kind one of assumption-wrong, split-pr, risk, check-cant-run, scope, other; none>\nNeeds: <what you need from the user; nothing>\nNext: <the next action>";

/// Reads a Handoff block from the end of a message, if it has one that
/// names its plan and stage.
pub fn parse_handoff(text: &str) -> Option<Handoff> {
    let start = text.rfind("Handoff ")?;
    let block = &text[start..];
    let mut lines = block.lines();
    let head = lines.next()?.trim().trim_matches(['*', '#', '`', ' ']);
    let mut h = Handoff::default();
    for part in head.trim_start_matches("Handoff").split('·') {
        let w: Vec<&str> = part.split_whitespace().collect();
        match w.as_slice() {
            ["plan", p] => h.plan = p.trim_matches(['*', '`']).to_string(),
            ["stage", st] => h.stage = st.trim_matches(['*', '`']).to_string(),
            [t] => h.task = t.trim_matches(['*', '`']).to_string(),
            _ => {}
        }
    }
    if h.plan.is_empty() || h.stage.is_empty() {
        return None;
    }
    let mut key = String::new();
    for line in lines {
        let l = line.trim().trim_start_matches(['*', '>']).trim();
        if l.starts_with("```") {
            break;
        }
        let none = |v: &str| matches!(v.trim().trim_end_matches('.').to_lowercase().as_str(), "" | "none" | "nothing" | "n/a" | "-");
        if let Some((k, v)) = l.split_once(':')
            && !k.contains(' ')
            && ["status", "done", "evidence", "left", "uncertain", "findings", "needs", "next"].contains(&k.trim_matches('*').to_lowercase().as_str())
        {
            key = k.trim_matches('*').to_lowercase();
            let v = v.trim().trim_matches('*').trim().to_string();
            match key.as_str() {
                "status" => h.status = v.split_whitespace().next().unwrap_or("").trim_matches(['.', ',']).to_lowercase(),
                "done" => h.done = v,
                "evidence" => h.evidence = v,
                "left" => h.left = if none(&v) { String::new() } else { v },
                "uncertain" => h.uncertain = if none(&v) { String::new() } else { v },
                "needs" => h.needs = if none(&v) { String::new() } else { v },
                "next" => h.next = v,
                "findings" if !none(&v) => h.findings.push(finding_of(&v)),
                _ => {}
            }
        } else if key == "findings" && l.starts_with('-') {
            let v = l.trim_start_matches('-').trim();
            if !v.is_empty() && !matches!(v.to_lowercase().as_str(), "none" | "nothing") {
                h.findings.push(finding_of(v));
            }
        } else if l.is_empty() {
            continue;
        }
    }
    Some(h)
}

/// "split-pr: the backfill is its own PR" → (kind, text); a kind it doesn't know is `other`.
fn finding_of(v: &str) -> (String, String) {
    match v.split_once(':') {
        Some((k, t)) if FINDING_KINDS.contains(&k.trim().to_lowercase().as_str()) => (k.trim().to_lowercase(), t.trim().to_string()),
        _ => ("other".to_string(), v.trim().to_string()),
    }
}

/// The structured handoff of a task sent from a plan: its goal, its
/// stage and what counts as done, what is known so far, the constraints,
/// what is uncertain, and the exact next action; then the stage's checks
/// and how to report back.
pub struct TaskHandoff<'a> {
    pub task: &'a str,
    pub goal: &'a str,
    pub so_far: &'a str,
    pub constraints: &'a str,
    pub uncertain: &'a str,
    pub next: &'a str,
}

pub fn handoff_text(p: &Plan, stage: &str, h: &TaskHandoff) -> Result<String> {
    let Some(st) = p.stages.iter().find(|s| s.id.eq_ignore_ascii_case(stage.trim())) else {
        bail!("plan {} has no stage {stage:?}: its stages are {}", p.id, p.stages.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", "));
    };
    let n = p.stages.iter().position(|s| s.id == st.id).unwrap_or(0) + 1;
    let line = |k: &str, v: &str| if v.trim().is_empty() { String::new() } else { format!("{k}: {}\n", v.trim()) };
    let goal = if h.goal.trim().is_empty() { if st.goal.is_empty() { st.title.as_str() } else { st.goal.as_str() } } else { h.goal };
    let mut s = format!("[tower] Plan {} · stage {} of {} ({}, {}): {}{}\n", p.id, n, p.stages.len(), st.id, st.kind, st.title, if h.task.is_empty() { String::new() } else { format!(" · task {}", h.task) });
    s += &line("Goal", goal);
    s += &line("Done when", &st.done_when);
    s += &line("So far", h.so_far);
    s += &line("Constraints", h.constraints);
    s += &line("Uncertain", h.uncertain);
    s += &line("Next", h.next);
    let checks: Vec<String> = st.checks().map(|(w, c)| format!("- {w} {}: {} (how: {}; evidence: {})", c.id, c.what, c.how, c.evidence)).collect();
    if !checks.is_empty() {
        s += &format!("\nThis stage is done when these pass, each reported with `tower plan check {} {} <check> passed|failed|not-run --evidence \"<…>\"`:\n{}\n", p.id, st.id, checks.join("\n"));
    }
    s += &format!("\nWhen your turn ends, end your last message with this block, filled in (it is how the tower and the plan hear what you found):\n\n{}\n", HANDOFF_FORMAT.replace("<task>", if h.task.is_empty() { "-" } else { h.task }).replace("<plan>", &p.id).replace("<stage>", &st.id));
    Ok(s)
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// What a finding is about.
pub const FINDING_KINDS: &[&str] = &["assumption-wrong", "split-pr", "risk", "check-cant-run", "scope", "other"];

/// Something a session learned while working that bears on the plan.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Finding {
    /// `f1`, `f2`, …
    pub id: String,
    pub at_ms: u64,
    /// One of `FINDING_KINDS`.
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stage: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub assumption: String,
    /// The session that found it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session: String,
    /// The revision that took it in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
}

/// A change to a plan's stages, assumptions or questions.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Revision {
    /// Its number, from 1; the plan's version once applied is n + 1.
    pub n: u32,
    pub at_ms: u64,
    /// `planner`, `tower` or `you`.
    pub by: String,
    pub why: String,
    /// The findings it takes in.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
    /// What changes, a line each.
    pub changes: Vec<String>,
    /// `small` (applied by itself, noted) or `big` (the user decides).
    pub size: String,
    /// `applied`, `proposed` (a card in the user's queue) or `rejected`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// The structure it proposes, until it is applied or rejected.
    #[serde(skip_serializing_if = "Value::is_null")]
    pub proposed: Value,
}

/// What changes from one structure to another, a line each, and whether
/// any of it is big: a stage added or removed, a stage's title, kind or
/// done-when changed, a check or an assumption removed. Small: a check
/// added or reworded, a stage's PRs or order changed (a PR split), an
/// assumption or a question added.
pub fn diff_structure(old: &[Stage], old_a: &[Assumption], new: &[Stage], new_a: &[Assumption], new_q: &[Question], old_q: &[Question]) -> (Vec<String>, bool) {
    let mut out: Vec<String> = vec![];
    let mut big = false;
    for st in new {
        let Some(was) = old.iter().find(|o| o.id == st.id) else {
            out.push(format!("Stage {} added: {} (done when: {})", st.id, st.title, st.done_when));
            big = true;
            continue;
        };
        if was.title != st.title || was.kind != st.kind {
            out.push(format!("Stage {} is now \"{}\" ({}), was \"{}\" ({})", st.id, st.title, st.kind, was.title, was.kind));
            big = true;
        }
        if was.done_when.trim() != st.done_when.trim() {
            out.push(format!("Stage {} is done when: {} (was: {})", st.id, st.done_when, was.done_when));
            big = true;
        }
        if was.prs != st.prs {
            out.push(format!("Stage {} delivers {} (was {})", st.id, nums(&st.prs), nums(&was.prs)));
        }
        if was.after != st.after {
            out.push(format!("Stage {} waits for {} (was {})", st.id, list_or_none(&st.after), list_or_none(&was.after)));
        }
        for (when, now_c, was_c) in [("pre", &st.pre, &was.pre), ("post", &st.post, &was.post)] {
            for c in now_c {
                match was_c.iter().find(|o| o.id == c.id) {
                    None => out.push(format!("Stage {} gets a {when} check {}: {}", st.id, c.id, c.what)),
                    Some(o) if o.what != c.what || o.how != c.how || o.evidence != c.evidence => out.push(format!("Stage {} {when} check {} now: {} (how: {})", st.id, c.id, c.what, c.how)),
                    _ => {}
                }
            }
            for o in was_c.iter().filter(|o| !now_c.iter().any(|c| c.id == o.id)) {
                out.push(format!("Stage {} drops its {when} check {}: {}", st.id, o.id, o.what));
                big = true;
            }
        }
    }
    for was in old.iter().filter(|o| !new.iter().any(|s| s.id == o.id)) {
        out.push(format!("Stage {} removed: {}", was.id, was.title));
        big = true;
    }
    for a in new_a {
        match old_a.iter().find(|o| o.id == a.id) {
            None => out.push(format!("Assumption {} added: {}", a.id, a.text)),
            Some(o) if o.text != a.text || o.check != a.check => out.push(format!("Assumption {} now: {} (checked: {})", a.id, a.text, a.check)),
            _ => {}
        }
    }
    for o in old_a.iter().filter(|o| !new_a.iter().any(|a| a.id == o.id)) {
        out.push(format!("Assumption {} dropped: {}", o.id, o.text));
        big = true;
    }
    for q in new_q.iter().filter(|q| !old_q.iter().any(|o| o.id == q.id)) {
        out.push(format!("Question {} added: {}", q.id, q.text));
    }
    (out, big)
}

fn nums(n: &[u32]) -> String {
    if n.is_empty() { "no PR".into() } else { n.iter().map(|x| format!("PR {x}")).collect::<Vec<_>>().join(", ") }
}

fn list_or_none(v: &[String]) -> String {
    if v.is_empty() { "nothing".into() } else { v.join(", ") }
}

/// A stage's kind: an unknown settled first, the work, or its release.
pub const STAGE_KINDS: &[&str] = &["spike", "build", "release"];

/// One milestone of a plan: what it delivers, what counts as done, and how
/// that is known before (`pre`) and after (`post`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stage {
    /// `s1`, `s2`, …
    pub id: String,
    pub title: String,
    /// `spike`, `build` or `release`.
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub goal: String,
    pub done_when: String,
    /// The stages it waits for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// The plan's PRs (their `n`) it delivers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prs: Vec<u32>,
    /// Checked before its work starts: assumptions, access, data, environments.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pre: Vec<crate::prscore::Check>,
    /// Checked after: the exact tests, commands and live checks, and the evidence each gives.
    pub post: Vec<crate::prscore::Check>,
}

impl Stage {
    /// Its pre and post checks, each with which it is.
    pub fn checks(&self) -> impl Iterator<Item = (&'static str, &crate::prscore::Check)> {
        self.pre.iter().map(|c| ("pre", c)).chain(self.post.iter().map(|c| ("post", c)))
    }
}

/// Where a stage stands (computed: `Plan::stage_states`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    /// A stage it waits for isn't done.
    Waiting,
    /// Its pre-checks haven't all passed: its work shouldn't start.
    Ready,
    /// Pre-checks passed (or none): its work goes on.
    Active,
    /// A check of it failed or wasn't run, or one of its PRs' checks did.
    Failing,
    /// Every check passed with evidence, and every PR of it that opened has all its checks passed.
    Done,
}

impl StageState {
    pub fn word(self) -> &'static str {
        match self {
            StageState::Waiting => "waiting",
            StageState::Ready => "ready",
            StageState::Active => "active",
            StageState::Failing => "failing",
            StageState::Done => "done",
        }
    }
}

/// Something a plan takes for granted.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Assumption {
    /// `a1`, `a2`, …
    pub id: String,
    pub text: String,
    /// How it is checked.
    pub check: String,
    /// The stage it must hold by.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub by: String,
    /// `open`, `holds` or `broken`.
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub proof: String,
}

/// One answer a question offers, and why.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QOption {
    pub label: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// Where a plan's question is: just written, weighed by the tower's brain,
/// in the user's queue, or answered.
pub const Q_OPEN: &str = "open";
pub const Q_WITH_TOWER: &str = "with-tower";
pub const Q_ASKED: &str = "asked";
pub const Q_ANSWERED: &str = "answered";

/// Something a plan can't decide alone.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Question {
    /// `q1`, `q2`, …
    pub id: String,
    pub text: String,
    /// Why it matters: what turns on it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QOption>,
    /// The planner's pick (an index of `options`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
    /// The planner says only the user can answer it.
    pub theirs: bool,
    /// The stage it holds up.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stage: String,
    /// `open`, `with-tower`, `asked`, `answered`.
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub answer: String,
    /// `you` or `tower`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub by: String,
    /// Its card in the user's queue, once it is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
}

impl Question {
    pub fn is_open(&self) -> bool {
        self.status != Q_ANSWERED
    }

    /// What the question-set rules see of it (`questions::must_escalate`):
    /// its words, its why and its options.
    pub fn as_set(&self, plan: &str) -> crate::questions::QuestionSet {
        let options = self.options.iter().map(|o| crate::questions::Opt { label: o.label.clone(), description: o.why.clone(), ..Default::default() }).collect();
        crate::questions::QuestionSet {
            id: format!("{plan}-{}", self.id),
            kind: "plan".into(),
            title: self.text.clone(),
            detail: self.why.clone(),
            tabs: vec![crate::questions::Tab { title: "Plan".into(), question: self.text.clone(), options, allows_other: true, ..Default::default() }],
            ..Default::default()
        }
    }
}

impl Plan {
    pub fn is_open(&self) -> bool {
        self.status != Status::Closed
    }

    /// Its questions not answered yet.
    pub fn open_questions(&self) -> Vec<&Question> {
        self.questions.iter().filter(|q| q.is_open()).collect()
    }

    /// Where each stage stands, in order. `pr_checks`: a PR's planned
    /// checks as the tower has them, by its key (`owner/name#12`), when it
    /// opened; a stage's PR that hasn't opened yet holds it from done.
    pub fn stage_states(&self, pr_checks: &dyn Fn(&str) -> Option<crate::prscore::Validation>) -> Vec<(String, StageState)> {
        let mut out: Vec<(String, StageState)> = vec![];
        for st in &self.stages {
            let waits = st.after.iter().any(|a| out.iter().find(|(id, _)| id == a).is_none_or(|(_, s)| *s != StageState::Done) || self.spikes_pending.iter().any(|x| x == a));
            let bad = |c: &crate::prscore::Check| c.status == crate::prscore::FAILED || c.status == crate::prscore::NOT_RUN;
            let pre_ok = st.pre.iter().all(crate::prscore::Check::passed);
            let post_ok = !st.post.is_empty() && st.post.iter().all(crate::prscore::Check::passed);
            // Its PRs: each must have opened and passed every planned check.
            let mut prs_ok = true;
            let mut prs_bad = false;
            for n in &st.prs {
                let key = self.prs.iter().find(|p| p.n == *n).and_then(|p| p.pr.clone());
                match key.and_then(|k| pr_checks(&k)) {
                    Some(v) => {
                        prs_ok &= v.checks.is_empty() || v.all_passed();
                        prs_bad |= v.any_bad();
                    }
                    None => prs_ok = false,
                }
            }
            let state = if st.checks().any(|(_, c)| bad(c)) || prs_bad {
                StageState::Failing
            } else if pre_ok && post_ok && prs_ok {
                StageState::Done
            } else if waits {
                StageState::Waiting
            } else if !pre_ok {
                StageState::Ready
            } else {
                StageState::Active
            };
            out.push((st.id.clone(), state));
        }
        out
    }

    /// Its markdown copy, in the planner's folder.
    pub fn plan_md(&self) -> PathBuf {
        Path::new(&self.dir).join("plan.md")
    }

    /// The user's last line has no answer yet.
    pub fn answering(&self) -> bool {
        self.thread.last().is_some_and(|m| m.from == Who::User.word()) && self.status != Status::Failed && self.address.is_some()
    }

    /// The plan as `state.json` (`plan`) and `tower plan show --json` carry it.
    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "form": self.form.word(),
            "status": self.status.word(),
            "ticket": self.ticket,
            "draft": self.draft,
            "revision": self.revision,
            "address": self.address,
            "planner": self.planner,
            "blocked": self.planner == "blocked",
            "answering": self.answering(),
            "dir": self.dir,
            "plan_md": self.plan_md().to_string_lossy(),
            "url": self.url,
            "why": self.why,
            "thread": self.thread,
            "created_ms": self.created_ms,
            "ready_ms": self.ready_ms,
            "updated_ms": self.updated_ms,
            "closed_ms": self.closed_ms,
            "launched": self.launched,
            "prs": self.prs,
            "stages": self.stages,
            "assumptions": self.assumptions,
            "questions": self.questions,
            "open_questions": self.open_questions().len(),
            "findings": self.findings,
            "revisions": self.revisions,
            "version": self.version,
            "critic": self.critic,
            "spikes_pending": self.spikes_pending,
            "account": self.account,
        })
    }

    /// `view`, with where each stage stands (`stage_states`) and how far
    /// the plan is: `stage_states` on each stage, and `progress`.
    pub fn view_with(&self, pr_checks: &dyn Fn(&str) -> Option<crate::prscore::Validation>) -> Value {
        let mut v = self.view();
        let states = self.stage_states(pr_checks);
        if let Some(stages) = v["stages"].as_array_mut() {
            for (st, (_, state)) in stages.iter_mut().zip(&states) {
                st["state"] = json!(state.word());
            }
        }
        let done = states.iter().filter(|(_, s)| *s == StageState::Done).count();
        // The stage at work: the first that isn't done.
        let at = states.iter().position(|(_, s)| *s != StageState::Done).map(|i| i + 1);
        v["progress"] = json!({ "stages": states.len(), "done": done, "at": at });
        v
    }
}

/// Every plan: the open one (at most one) and the latest closed ones.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub plans: Vec<Plan>,
    /// The last id's number.
    pub next: u64,
}

/// A plan's id: `p` and a number.
pub fn is_id(word: &str) -> bool {
    word.strip_prefix('p').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// A line of the chat: its spaces kept, cut to `MSG_CHARS`.
fn tidy_msg(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= MSG_CHARS {
        return t.to_string();
    }
    let mut cut: String = t.chars().take(MSG_CHARS).collect();
    cut.push('…');
    cut
}

impl Book {
    /// The open plan (the newest, should there be more).
    pub fn current(&self) -> Option<&Plan> {
        self.plans.iter().rev().find(|p| p.is_open() && p.status != Status::Live)
    }

    /// The plans launched and live, newest first.
    pub fn live(&self) -> Vec<&Plan> {
        self.plans.iter().rev().filter(|p| p.status == Status::Live).collect()
    }

    pub fn get(&self, id: &str) -> Option<&Plan> {
        self.plans.iter().find(|p| p.id == id)
    }

    /// `id`, or the open plan when no id is given.
    pub fn pick(&self, id: Option<&str>) -> Result<&Plan> {
        match id.map(str::trim).filter(|i| !i.is_empty()) {
            Some(i) => self.get(i).ok_or_else(|| anyhow::anyhow!("no plan {i}")),
            None => self.current().ok_or_else(|| anyhow::anyhow!("no plan is open")),
        }
    }

    fn open_mut(&mut self, id: &str) -> Result<&mut Plan> {
        let Some(p) = self.plans.iter_mut().find(|p| p.id == id) else { bail!("no plan {id}") };
        if !p.is_open() {
            bail!("plan {id} is closed");
        }
        Ok(p)
    }

    /// A new plan of `draft` for `ticket`, in the folder `dir_of` gives its
    /// id. The plans open before it are closed (one at a time): they come
    /// back with it, for their planners to be ended.
    pub fn add(&mut self, form: Form, ticket: &str, draft: Value, dir_of: impl Fn(&str) -> PathBuf, now: u64) -> (Plan, Vec<Plan>) {
        let mut ended = vec![];
        for p in self.plans.iter_mut().filter(|p| p.is_open() && p.status != Status::Live) {
            p.status = Status::Closed;
            p.closed_ms = Some(now);
            p.updated_ms = now;
            ended.push(p.clone());
        }
        self.next += 1;
        let id = format!("p{}", self.next);
        let plan = Plan {
            dir: dir_of(&id).to_string_lossy().into_owned(),
            id,
            form,
            status: Status::Writing,
            ticket: ticket.trim().to_string(),
            draft,
            planner: "starting".into(),
            created_ms: now,
            updated_ms: now,
            ..Default::default()
        };
        self.plans.push(plan.clone());
        self.prune();
        (plan, ended)
    }

    /// The planner's terminal runs: its address.
    pub fn started(&mut self, id: &str, address: &str, now: u64) -> Result<Plan> {
        let p = self.open_mut(id)?;
        p.address = Some(address.to_string());
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// A line of the chat.
    pub fn say(&mut self, id: &str, who: Who, text: &str, now: u64) -> Result<Plan> {
        let text = tidy_msg(text);
        if text.is_empty() {
            bail!("nothing to say: give the words after the plan's id");
        }
        let p = self.open_mut(id)?;
        p.thread.push(Msg { from: who.word().into(), text, at: now });
        if p.thread.len() > THREAD_MAX {
            let drop = p.thread.len() - THREAD_MAX;
            p.thread.drain(..drop);
        }
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// The planner revised the draft.
    pub fn set_draft(&mut self, id: &str, draft: Value, now: u64) -> Result<Plan> {
        check_draft(&draft)?;
        let p = self.open_mut(id)?;
        p.draft = draft;
        p.revision += 1;
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// The PRs the planner predicts (checked and scored by `prscore::check`).
    pub fn set_prs(&mut self, id: &str, prs: Vec<crate::prscore::Predicted>, now: u64) -> Result<Plan> {
        let p = self.open_mut(id)?;
        // A PR that opened already stays matched to the prediction of the same number.
        let opened: Vec<(u32, String)> = p.prs.iter().filter_map(|x| Some((x.n, x.pr.clone()?))).collect();
        p.prs = prs;
        for (n, k) in opened {
            if let Some(x) = p.prs.iter_mut().find(|x| x.n == n) {
                x.pr = Some(k);
            }
        }
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// The planner's structure (`tower plan stages`): its stages,
    /// assumptions and questions, checked (`structure`). What was known of
    /// the same ids stays: a check's result, an assumption's status, a
    /// question's answer and where it is (unless its words changed).
    pub fn set_structure(&mut self, id: &str, v: &Value, now: u64) -> Result<Plan> {
        let p = self.open_mut(id)?;
        let (mut stages, mut assumptions, mut questions) = structure(v, &p.prs)?;
        for st in stages.iter_mut() {
            if let Some(old) = p.stages.iter().find(|o| o.id == st.id) {
                for (now_c, was) in [(&mut st.pre, &old.pre), (&mut st.post, &old.post)] {
                    for c in now_c.iter_mut() {
                        if let Some(o) = was.iter().find(|o| o.id == c.id && o.status != crate::prscore::PLANNED) {
                            (c.status, c.proof, c.by, c.at_ms) = (o.status.clone(), o.proof.clone(), o.by.clone(), o.at_ms);
                        }
                    }
                }
            }
        }
        for a in assumptions.iter_mut() {
            if let Some(o) = p.assumptions.iter().find(|o| o.id == a.id && o.text.trim() == a.text.trim()) {
                (a.status, a.proof) = (o.status.clone(), o.proof.clone());
            }
        }
        for q in questions.iter_mut() {
            if let Some(o) = p.questions.iter().find(|o| o.id == q.id && (o.text.trim() == q.text.trim() || o.status == Q_ANSWERED)) {
                (q.status, q.answer, q.by, q.card, q.at_ms) = (o.status.clone(), o.answer.clone(), o.by.clone(), o.card.clone(), o.at_ms);
            }
        }
        (p.stages, p.assumptions, p.questions) = (stages, assumptions, questions);
        if p.version == 0 {
            p.version = 1;
        }
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// A finding reported on a plan: kept, and an assumption it says is
    /// wrong marked broken.
    #[allow(clippy::too_many_arguments)]
    pub fn finding(&mut self, id: &str, kind: &str, text: &str, stage: &str, assumption: &str, session: &str, now: u64) -> Result<Finding> {
        let p = self.plans.iter_mut().find(|p| p.id == id && p.status != Status::Closed).ok_or_else(|| anyhow::anyhow!("no open or live plan {id}"))?;
        let kind = kind.trim().to_lowercase();
        if !FINDING_KINDS.contains(&kind.as_str()) {
            bail!("a finding's kind is {}, not {kind:?}", FINDING_KINDS.join(", "));
        }
        let text = tidy_msg(text);
        if text.is_empty() {
            bail!("say what was found: tower plan finding {id} --kind {kind} \"<what>\"");
        }
        if !stage.trim().is_empty() && !p.stages.iter().any(|s| s.id.eq_ignore_ascii_case(stage.trim())) {
            bail!("plan {id} has no stage {stage:?}");
        }
        if !assumption.trim().is_empty() {
            let Some(a) = p.assumptions.iter_mut().find(|a| a.id.eq_ignore_ascii_case(assumption.trim())) else { bail!("plan {id} has no assumption {assumption:?}") };
            if kind == "assumption-wrong" {
                (a.status, a.proof) = ("broken".into(), text.clone());
            }
        }
        let f = Finding {
            id: format!("f{}", p.findings.len() + 1),
            at_ms: now,
            kind,
            text,
            stage: stage.trim().to_string(),
            assumption: assumption.trim().to_string(),
            session: session.trim().to_string(),
            revision: None,
        };
        p.findings.push(f.clone());
        p.updated_ms = now;
        Ok(f)
    }

    /// An assumption checked: it holds, or it is broken (then it is a finding too).
    pub fn assume(&mut self, id: &str, a: &str, holds: bool, proof: &str, session: &str, now: u64) -> Result<(Assumption, Option<Finding>)> {
        if proof.trim().is_empty() {
            bail!("give its evidence: --evidence \"<what showed it>\"");
        }
        if !holds {
            let f = self.finding(id, "assumption-wrong", proof, "", a, session, now)?;
            let p = self.plans.iter().find(|p| p.id == id).ok_or_else(|| anyhow::anyhow!("no plan {id}"))?;
            let x = p.assumptions.iter().find(|x| x.id.eq_ignore_ascii_case(a.trim())).cloned().unwrap_or_default();
            return Ok((x, Some(f)));
        }
        let p = self.plans.iter_mut().find(|p| p.id == id && p.status != Status::Closed).ok_or_else(|| anyhow::anyhow!("no open or live plan {id}"))?;
        let x = p.assumptions.iter_mut().find(|x| x.id.eq_ignore_ascii_case(a.trim())).ok_or_else(|| anyhow::anyhow!("plan {id} has no assumption {a:?}"))?;
        (x.status, x.proof) = ("holds".into(), tidy_msg(proof));
        let x = x.clone();
        p.updated_ms = now;
        Ok((x, None))
    }

    /// A revision of the structure (`v`, as `tower plan stages` takes it),
    /// for `why`, taking in `findings`: sized by what changes. A small one
    /// is applied now; a big one is proposed, for the user to decide.
    pub fn revise(&mut self, id: &str, v: &Value, why: &str, findings: &[String], by: &str, now: u64) -> Result<Revision> {
        let p = self.plans.iter().find(|p| p.id == id && p.status != Status::Closed).ok_or_else(|| anyhow::anyhow!("no open or live plan {id}"))?;
        if p.stages.is_empty() {
            bail!("plan {id} has no stages yet: send them with `tower plan stages`");
        }
        if why.trim().is_empty() {
            bail!("say why: --why \"<what made it change>\"");
        }
        for f in findings {
            if !p.findings.iter().any(|x| x.id.eq_ignore_ascii_case(f.trim())) {
                bail!("plan {id} has no finding {f:?}");
            }
        }
        if p.revisions.iter().any(|r| r.status == "proposed") {
            bail!("plan {id} has a revision waiting for the user's word: one at a time");
        }
        let (stages, assumptions, questions) = structure(v, &p.prs)?;
        let (changes, big) = diff_structure(&p.stages, &p.assumptions, &stages, &assumptions, &questions, &p.questions);
        if changes.is_empty() {
            bail!("nothing changes: the stages, assumptions and questions are as they are");
        }
        let r = Revision {
            n: p.revisions.len() as u32 + 1,
            at_ms: now,
            by: by.to_string(),
            why: tidy_msg(why),
            findings: findings.iter().map(|f| f.trim().to_string()).collect(),
            changes,
            size: if big { "big".into() } else { "small".into() },
            status: if big { "proposed".into() } else { "applied".into() },
            card: None,
            proposed: if big { v.clone() } else { Value::Null },
        };
        if !big {
            self.set_structure(id, v, now)?;
        }
        let p = self.plans.iter_mut().find(|p| p.id == id).expect("found above");
        if !big {
            p.version += 1;
            for f in p.findings.iter_mut().filter(|f| r.findings.iter().any(|x| x.eq_ignore_ascii_case(&f.id))) {
                f.revision = Some(r.n);
            }
        }
        // A revision takes in what the spikes found: the stages after them go on.
        if !big {
            p.spikes_pending.clear();
        }
        p.revisions.push(r.clone());
        p.updated_ms = now;
        Ok(r)
    }

    /// The planner's word that a spike's findings change nothing: noted as
    /// a revision of no change, and the stages after it go on.
    pub fn keep(&mut self, id: &str, why: &str, now: u64) -> Result<Revision> {
        let p = self.plans.iter_mut().find(|p| p.id == id && p.status != Status::Closed).ok_or_else(|| anyhow::anyhow!("no open or live plan {id}"))?;
        if why.trim().is_empty() {
            bail!("say why nothing changes: --why \"…\"");
        }
        let taken: Vec<String> = p.findings.iter().filter(|f| f.revision.is_none()).map(|f| f.id.clone()).collect();
        let n = p.revisions.len() as u32 + 1;
        for f in p.findings.iter_mut().filter(|f| f.revision.is_none()) {
            f.revision = Some(n);
        }
        let r = Revision { n, at_ms: now, by: "planner".into(), why: tidy_msg(why), findings: taken, changes: vec![], size: "none".into(), status: "kept".into(), card: None, proposed: Value::Null };
        p.spikes_pending.clear();
        p.revisions.push(r.clone());
        p.updated_ms = now;
        Ok(r)
    }

    /// Where each stage of a plan stands now (`states`), against where it
    /// stood: the stages that just became done. A spike that did waits for
    /// its findings to be taken in (`spikes_pending`).
    pub fn stage_moves(&mut self, id: &str, states: &[(String, StageState)]) -> Vec<String> {
        let Some(p) = self.plans.iter_mut().find(|p| p.id == id) else { return vec![] };
        let mut done = vec![];
        for (sid, st) in states {
            let was = p.stage_seen.insert(sid.clone(), st.word().to_string());
            if *st == StageState::Done && was.as_deref() != Some("done") && was.is_some() {
                done.push(sid.clone());
                if p.stages.iter().any(|s| &s.id == sid && s.kind == "spike") && !p.spikes_pending.contains(sid) {
                    p.spikes_pending.push(sid.clone());
                }
            }
        }
        done
    }

    /// The critic's read, kept.
    pub fn criticised(&mut self, id: &str, status: &str, gaps: Vec<String>, why: &str, now: u64) -> Result<Plan> {
        let p = self.plans.iter_mut().find(|p| p.id == id).ok_or_else(|| anyhow::anyhow!("no plan {id}"))?;
        p.critic = Critic { status: status.to_string(), at_ms: now, gaps, why: why.to_string() };
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// The user's word on a big revision: applied, or not.
    pub fn decide_revision(&mut self, id: &str, n: u32, apply: bool, now: u64) -> Result<Revision> {
        let p = self.plans.iter().find(|p| p.id == id).ok_or_else(|| anyhow::anyhow!("no plan {id}"))?;
        let Some(r) = p.revisions.iter().find(|r| r.n == n).cloned() else { bail!("plan {id} has no revision {n}") };
        if r.status != "proposed" {
            bail!("revision {n} of plan {id} is {} already", r.status);
        }
        if apply {
            self.set_structure(id, &r.proposed, now)?;
        }
        let p = self.plans.iter_mut().find(|p| p.id == id).expect("found above");
        if apply {
            p.version += 1;
            for f in p.findings.iter_mut().filter(|f| r.findings.iter().any(|x| x.eq_ignore_ascii_case(&f.id))) {
                f.revision = Some(n);
            }
        }
        let x = p.revisions.iter_mut().find(|x| x.n == n).expect("found above");
        x.status = if apply { "applied".into() } else { "rejected".into() };
        x.proposed = Value::Null;
        let x = x.clone();
        p.updated_ms = now;
        Ok(x)
    }

    /// A revision's card in the user's queue.
    pub fn revision_card(&mut self, id: &str, n: u32, card: &str) {
        if let Some(r) = self.plans.iter_mut().find(|p| p.id == id).and_then(|p| p.revisions.iter_mut().find(|r| r.n == n)) {
            r.card = Some(card.to_string());
        }
    }

    /// A question of the plan, by its id.
    fn question_mut(&mut self, id: &str, q: &str) -> Result<&mut Question> {
        let p = self.plans.iter_mut().find(|p| p.id == id).ok_or_else(|| anyhow::anyhow!("no plan {id}"))?;
        let ids: Vec<String> = p.questions.iter().map(|q| q.id.clone()).collect();
        p.questions.iter_mut().find(|x| x.id.eq_ignore_ascii_case(q.trim())).ok_or_else(|| anyhow::anyhow!("plan {id} has no question {q:?}: its questions are {}", ids.join(", ")))
    }

    /// Where a question is now (`with-tower`, `asked`), and its card.
    pub fn question_at(&mut self, id: &str, q: &str, status: &str, card: Option<&str>, now: u64) -> Result<Question> {
        let x = self.question_mut(id, q)?;
        if x.status == Q_ANSWERED {
            bail!("{q} of plan {id} is answered already: {}", x.answer);
        }
        x.status = status.to_string();
        if let Some(c) = card {
            x.card = Some(c.to_string());
        }
        let x = x.clone();
        if let Some(p) = self.plans.iter_mut().find(|p| p.id == id) {
            p.updated_ms = now;
        }
        Ok(x)
    }

    /// A question answered (`by`: `you` or `tower`): written into the plan.
    pub fn answer(&mut self, id: &str, q: &str, answer: &str, by: &str, now: u64) -> Result<Question> {
        let answer = tidy_msg(answer);
        if answer.is_empty() {
            bail!("give the answer: tower plan answer {id} {q} \"<the answer>\"");
        }
        let x = self.question_mut(id, q)?;
        if x.status == Q_ANSWERED && x.answer == answer {
            return Ok(x.clone());
        }
        (x.status, x.answer, x.by, x.at_ms) = (Q_ANSWERED.to_string(), answer, if by == "tower" { "tower".into() } else { "you".into() }, Some(now));
        let x = x.clone();
        if let Some(p) = self.plans.iter_mut().find(|p| p.id == id) {
            p.updated_ms = now;
        }
        Ok(x)
    }

    /// A stage's check reported (`passed`, `failed`, `not-run`), with its evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn check(&mut self, id: &str, stage: &str, check: &str, status: &str, proof: &str, by: &str, now: u64) -> Result<crate::prscore::Check> {
        let p = self.plans.iter_mut().find(|p| p.id == id).ok_or_else(|| anyhow::anyhow!("no plan {id}"))?;
        let ids: Vec<String> = p.stages.iter().map(|s| s.id.clone()).collect();
        let Some(st) = p.stages.iter_mut().find(|s| s.id.eq_ignore_ascii_case(stage.trim())) else {
            bail!("plan {id} has no stage {stage:?}: its stages are {}", ids.join(", "));
        };
        // The pre and post checks as one list, to report as a PR's are.
        let mut v = crate::prscore::Validation { checks: st.pre.iter().chain(st.post.iter()).cloned().collect(), edge_cases: vec![] };
        let c = v.report(check, status, proof, by, now)?;
        let n = st.pre.len();
        st.pre = v.checks[..n].to_vec();
        st.post = v.checks[n..].to_vec();
        p.updated_ms = now;
        Ok(c)
    }

    /// The prediction a real PR is, in the plan its space was launched
    /// with (the latest): matched once (`prscore::best_match`), and marked
    /// with the PR's key. (The plan's id, the prediction.)
    pub fn match_pr(&mut self, space: &str, repo: &str, base: &str, title: &str, key: &str, now: u64) -> Option<(String, crate::prscore::Predicted)> {
        let space = space.trim();
        if space.is_empty() {
            return None;
        }
        let p = self.plans.iter_mut().rev().find(|p| p.launched.as_deref().is_some_and(|l| l.trim().eq_ignore_ascii_case(space)) && !p.prs.is_empty())?;
        // Seen before (the same PR found again): the same prediction.
        if let Some(x) = p.prs.iter().find(|x| x.pr.as_deref() == Some(key)) {
            return Some((p.id.clone(), x.clone()));
        }
        let i = crate::prscore::best_match(&p.prs, repo, base, title)?;
        p.prs[i].pr = Some(key.to_string());
        p.updated_ms = now;
        Some((p.id.clone(), p.prs[i].clone()))
    }

    /// Published (again): its link, and it is ready.
    pub fn published(&mut self, id: &str, url: &str, now: u64) -> Result<Plan> {
        let url = url.trim();
        if !url.starts_with("https://") || url.contains(char::is_whitespace) {
            bail!("--url is the artifact's link (https://claude.ai/…), not {url:?}");
        }
        let p = self.open_mut(id)?;
        p.url = Some(url.to_string());
        p.status = Status::Ready;
        p.why.clear();
        p.ready_ms.get_or_insert(now);
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// The planner couldn't: why.
    pub fn failed(&mut self, id: &str, why: &str, now: u64) -> Result<Plan> {
        let p = self.open_mut(id)?;
        p.status = Status::Failed;
        p.why = match why.trim() {
            "" => "the planner couldn't publish the plan".to_string(),
            w => tidy_msg(w),
        };
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// Ended, or launched (`launched`: the space launched with it): a
    /// launched plan with stages stays live, the plan of its space. Closing
    /// a closed plan is no error.
    pub fn close(&mut self, id: &str, launched: Option<&str>, now: u64) -> Result<Plan> {
        let Some(p) = self.plans.iter_mut().find(|p| p.id == id) else { bail!("no plan {id}") };
        let launched = launched.map(str::trim).filter(|s| !s.is_empty());
        if launched.is_some() && !p.stages.is_empty() && p.status != Status::Closed {
            p.status = Status::Live;
            p.address = None;
        } else if p.is_open() {
            p.status = Status::Closed;
            p.closed_ms = Some(now);
        }
        if let Some(space) = launched {
            p.launched = Some(space.to_string());
        }
        p.updated_ms = now;
        Ok(p.clone())
    }

    /// How the planner's terminal stands now (`word`: Herdr's status;
    /// `gone`: it has ended). True when that changed the plan.
    pub fn seen(&mut self, id: &str, word: &str, now: u64) -> bool {
        let Ok(p) = self.open_mut(id) else { return false };
        if p.planner == word {
            return false;
        }
        p.planner = word.to_string();
        if word == "gone" {
            p.address = None;
            if p.status == Status::Writing {
                p.status = Status::Failed;
                p.why = "the planner's terminal ended before it published the plan".into();
            } else if p.status == Status::Ready {
                p.why = "The planner's terminal ended: the plan stays, and Plan again starts one to talk it over.".into();
            }
        }
        p.updated_ms = now;
        true
    }

    /// Only the latest closed plans are kept.
    fn prune(&mut self) {
        let closed = self.plans.iter().filter(|p| !p.is_open()).count();
        if closed <= KEEP_CLOSED {
            return;
        }
        let mut drop = closed - KEEP_CLOSED;
        self.plans.retain(|p| {
            if drop > 0 && !p.is_open() {
                drop -= 1;
                return false;
            }
            true
        });
    }
}

/// The planner's structure, checked: `{"stages": [...], "assumptions":
/// [...], "questions": [...]}`. Every stage has an id, a title, a kind
/// (`spike`, `build`, `release`), what counts as done and at least one
/// post-check (each with what, how and the evidence); its PRs are the
/// plan's; it waits only for stages before it. Every check, assumption and
/// question has an id and words.
pub fn structure(v: &Value, prs: &[crate::prscore::Predicted]) -> Result<(Vec<Stage>, Vec<Assumption>, Vec<Question>)> {
    let word = |x: &Value, k: &str| x[k].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(raw) = v["stages"].as_array().filter(|a| !a.is_empty()) else {
        bail!("no stages: give {{\"stages\": [{{\"id\": \"s1\", \"title\", \"kind\": \"spike|build|release\", \"done_when\", \"pre\": [checks], \"post\": [checks], \"prs\": [1], \"after\": []}}], \"assumptions\": [...], \"questions\": [...]}}");
    };
    let checks_of = |st: &Value, k: &str, sid: &str| -> Result<Vec<crate::prscore::Check>> {
        let mut out: Vec<crate::prscore::Check> = vec![];
        for (i, c) in st[k].as_array().into_iter().flatten().enumerate() {
            let id = match word(c, "id") {
                i if !i.is_empty() => i,
                _ => format!("{}{}", if k == "pre" { "p" } else { "c" }, i + 1),
            };
            let (what, how, evidence) = (word(c, "what"), word(c, "how"), word(c, "evidence"));
            if what.is_empty() || how.is_empty() || evidence.is_empty() {
                bail!("stage {sid}, {k} check {id}: give what it shows, how (the test, the command or the live step) and the evidence it gives");
            }
            if out.iter().any(|o| o.id.eq_ignore_ascii_case(&id)) {
                bail!("stage {sid}: two checks are called {id}");
            }
            out.push(crate::prscore::Check { id, what, how, evidence, status: crate::prscore::PLANNED.into(), ..Default::default() });
        }
        Ok(out)
    };
    let mut stages: Vec<Stage> = vec![];
    for (i, st) in raw.iter().enumerate() {
        let id = match word(st, "id") {
            x if !x.is_empty() => x,
            _ => format!("s{}", i + 1),
        };
        if stages.iter().any(|s| s.id.eq_ignore_ascii_case(&id)) {
            bail!("two stages are called {id}");
        }
        let title = word(st, "title");
        let done_when = st["done_when"].as_str().unwrap_or("").trim().to_string();
        if title.is_empty() || done_when.is_empty() {
            bail!("stage {id}: give its title and what counts as done (done_when)");
        }
        let kind = match word(st, "kind").to_lowercase() {
            k if k.is_empty() => "build".to_string(),
            k if STAGE_KINDS.contains(&k.as_str()) => k,
            k => bail!("stage {id}: its kind is spike, build or release, not {k:?}"),
        };
        let (pre, post) = (checks_of(st, "pre", &id)?, checks_of(st, "post", &id)?);
        if post.is_empty() {
            bail!("stage {id}: give at least one post check (how it is known to be done, and the evidence)");
        }
        if let Some(dup) = pre.iter().find(|c| post.iter().any(|o| o.id.eq_ignore_ascii_case(&c.id))) {
            bail!("stage {id}: a pre and a post check are both called {}", dup.id);
        }
        let mut after: Vec<String> = vec![];
        for a in st["after"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !stages.iter().any(|s| s.id.eq_ignore_ascii_case(a.trim())) {
                bail!("stage {id} waits for {a:?}: a stage waits only for one before it");
            }
            after.push(a.trim().to_string());
        }
        let mut nums: Vec<u32> = vec![];
        for n in st["prs"].as_array().into_iter().flatten() {
            let Some(n) = n.as_u64().map(|n| n as u32) else { bail!("stage {id}: its prs are the plan's PR numbers (1, 2, …)") };
            if !prs.iter().any(|p| p.n == n) {
                bail!("stage {id}: the plan has no PR {n} (run `tower plan prs` first; it has {})", if prs.is_empty() { "none".to_string() } else { prs.iter().map(|p| p.n.to_string()).collect::<Vec<_>>().join(", ") });
            }
            nums.push(n);
        }
        stages.push(Stage { id, title, kind, goal: word(st, "goal"), done_when, after, prs: nums, pre, post });
    }
    let mut assumptions: Vec<Assumption> = vec![];
    for (i, a) in v["assumptions"].as_array().into_iter().flatten().enumerate() {
        let id = match word(a, "id") {
            x if !x.is_empty() => x,
            _ => format!("a{}", i + 1),
        };
        let (text, check, by) = (word(a, "text"), word(a, "check"), word(a, "by"));
        if text.is_empty() || check.is_empty() {
            bail!("assumption {id}: give its text and how it is checked (check)");
        }
        if !by.is_empty() && !stages.iter().any(|s| s.id.eq_ignore_ascii_case(&by)) {
            bail!("assumption {id} must hold by {by:?}: no such stage");
        }
        assumptions.push(Assumption { id, text, check, by, status: "open".into(), proof: String::new() });
    }
    let mut questions: Vec<Question> = vec![];
    for (i, q) in v["questions"].as_array().into_iter().flatten().enumerate() {
        let id = match word(q, "id") {
            x if !x.is_empty() => x,
            _ => format!("q{}", i + 1),
        };
        let text = word(q, "text");
        if text.is_empty() {
            bail!("question {id}: give its words (text)");
        }
        if questions.iter().any(|x| x.id.eq_ignore_ascii_case(&id)) {
            bail!("two questions are called {id}");
        }
        let options: Vec<QOption> = q["options"].as_array().into_iter().flatten().map(|o| QOption { label: word(o, "label"), why: word(o, "why") }).filter(|o| !o.label.is_empty()).collect();
        let recommended = q["recommended"].as_u64().map(|n| n as usize).filter(|n| *n < options.len());
        let stage = word(q, "stage");
        if !stage.is_empty() && !stages.iter().any(|s| s.id.eq_ignore_ascii_case(&stage)) {
            bail!("question {id} holds up {stage:?}: no such stage");
        }
        questions.push(Question { id, text, why: word(q, "why"), options, recommended, theirs: q["theirs"] == true, stage, status: Q_OPEN.into(), ..Default::default() });
    }
    Ok((stages, assumptions, questions))
}

/// What the brain is told of a plan's question it may answer itself: the
/// question, its options and the planner's pick, and the two ways out.
pub fn question_note(plan: &str, space: &str, q: &Question, tower: &str) -> String {
    let t = tower_word(tower);
    let body = json!({
        "plan": plan,
        "space": space,
        "question": q.id,
        "text": q.text,
        "why": q.why,
        "options": q.options,
        "recommended": q.recommended.and_then(|i| q.options.get(i)).map(|o| o.label.clone()),
        "stage": q.stage,
    });
    format!(
        "[plan-question] {plan} {} {} Answer it yourself only when it is routine (the 90/10 rule: the planner's pick or the obvious answer, nothing the user would want a say in): `{t} plan answer {plan} {} \"<the answer, and why in a few words>\" --by tower`. Otherwise give it to the user: `{t} plan ask {plan} {}`. Say nothing to the user about it either way.",
        q.id,
        serde_json::to_string(&body).unwrap_or_default(),
        q.id,
        q.id
    )
}

/// A plan's question as its card in the user's queue asks it.
pub fn card_of(plan: &str, space: Option<&str>, q: &Question) -> crate::decisions::PlanAsk {
    let what = space.filter(|s| !s.trim().is_empty()).map(|s| format!("The plan of {s}")).unwrap_or_else(|| format!("Plan {plan}"));
    let mut detail = format!("{what} asks this before its work starts.");
    if !q.why.trim().is_empty() {
        detail += &format!(" Why it matters: {}", q.why.trim());
    }
    if !q.stage.trim().is_empty() {
        detail += &format!(" It holds up stage {}.", q.stage.trim());
    }
    crate::decisions::PlanAsk {
        plan: plan.to_string(),
        question: q.id.clone(),
        space: space.map(String::from),
        title: q.text.trim().to_string(),
        detail,
        options: q.options.iter().enumerate().map(|(i, o)| (o.label.clone(), o.why.clone(), q.recommended == Some(i))).collect(),
    }
}

/// What the planner is told when one of its questions is answered: put it
/// in the plan and republish, and send the stages again if it changes them.
pub fn answered_note(p: &Plan, q: &Question, tower: &str) -> String {
    let t = tower_word(tower);
    let who = if q.by == "tower" { "the tower's brain, as routine" } else { "the user" };
    let left = p.open_questions().len();
    let rest = match left {
        0 => "No question is open now.".to_string(),
        n => format!("{n} question{} still open: the tower asks {}; you'll be told each answer.", if n == 1 { " is" } else { "s are" }, if n == 1 { "it" } else { "them" }),
    };
    format!(
        "[plan {}] {} is answered by {who}: {}\n\nWrite it into plan.md and the artifact: the question moves to \"Answered\" with its answer, and what it changes in the plan changes with it. If it changes the stages, assumptions or checks, send them again with `{t} plan stages {} --json-file stages.json`. Then republish the same artifact and run `{t} plan published {} --url <its link>`. {rest}",
        p.id, q.id, q.answer, p.id, p.id
    )
}

/// What the planner of a live plan is told of a finding.
pub fn finding_note(p: &Plan, f: &Finding, tower: &str) -> String {
    let t = tower_word(tower);
    let mut about = vec![];
    if !f.stage.is_empty() {
        about.push(format!("stage {}", f.stage));
    }
    if !f.assumption.is_empty() {
        about.push(format!("assumption {}", f.assumption));
    }
    let about = if about.is_empty() { String::new() } else { format!(" (about {})", about.join(", ")) };
    let who = if f.session.is_empty() { String::new() } else { format!(" from {}", f.session) };
    format!(
        "[plan {}] Finding {}{who}, {}{about}: {}\n\nRevise the plan for it as instructions.md says (\"6. A live plan\"): `{t} plan revise {} --json-file stages.json --why \"…\" --finding {}`, then plan.md and the artifact. If it changes nothing, say so with `{t} plan say {} \"…\"`.",
        p.id, f.id, f.kind, f.text, p.id, f.id, p.id
    )
}

/// A big revision as its card in the user's queue asks it.
pub fn revision_card(p: &Plan, r: &Revision) -> crate::decisions::PlanAsk {
    let space = p.launched.clone().or_else(|| p.draft["space"].as_str().map(String::from));
    let of = space.as_deref().map(|s| format!("The plan of {s}")).unwrap_or_else(|| format!("Plan {}", p.id));
    let mut detail = format!("{of}, version {} → {}. Why: {}\n", p.version, p.version + 1, r.why.trim_end_matches('.'));
    for c in &r.changes {
        detail += &format!("\n- {c}");
    }
    if !r.findings.is_empty() {
        detail += &format!("\n\nFrom finding {}.", r.findings.join(", "));
    }
    crate::decisions::PlanAsk {
        plan: p.id.clone(),
        question: format!("rev:{}", r.n),
        space,
        title: format!("Change the plan: {}?", r.why.trim().trim_end_matches(['.', '?'])),
        detail,
        options: vec![
            ("Apply it".into(), "the plan changes as listed; its sessions follow the new version".into(), false),
            ("Keep the plan as it is".into(), "the planner is told, and finds another way".into(), false),
        ],
    }
}

/// A draft a plan can be made of: the receipt `tower launch --draft`
/// prints, with its space and at least one session's repo.
pub fn check_draft(v: &Value) -> Result<()> {
    if v["space"].as_str().is_none_or(|s| s.trim().is_empty()) {
        bail!("the draft has no space: give the JSON `tower launch \"<ticket>\" --draft --json` printed");
    }
    let repos = v["sessions"].as_array().map(|s| s.iter().filter(|x| x["repo"].as_str().is_some_and(|r| !r.trim().is_empty())).count()).unwrap_or(0);
    if repos == 0 {
        bail!("the draft has no session: give the JSON `tower launch \"<ticket>\" --draft --json` printed");
    }
    Ok(())
}

/// The draft the planner wrote (in the draft's own shape, or the shape a
/// draft is asked in: `{"project", "account", "repos": [{"folder", "why"}],
/// "space", "goal", "done_when", "notes"}`), checked as a draft is
/// (`ticket::check`): its receipt.
pub fn revised(text: &str, ticket: &str, w: &crate::launch::World) -> Result<Value> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|e| anyhow::anyhow!("--json-file: not JSON ({e})"))?;
    let brief = if v["sessions"].is_array() { crate::ticket::prior_brief(&v) } else { v };
    let d = crate::ticket::check(&brief, ticket, w, None)?;
    let mut r = crate::ticket::receipt(&d, ticket, w)?;
    r["assumed"]["setup_why"] = json!("the planner's revision");
    r["assumed"]["name_why"] = json!("the planner's revision");
    Ok(r)
}

// ---------------------------------------------------------------------------
// The planner: its folder, its instructions, how it is started.

/// The folder plans are written in.
pub fn plans_dir() -> PathBuf {
    crate::config::state_dir().join("plans")
}

/// The tower as the planner runs it: its whole path (the `tower` on PATH
/// may be another).
fn tower_word(tower: &str) -> String {
    if tower.chars().any(|c| c.is_whitespace() || c == '\'') { format!("'{}'", tower.replace('\'', "'\\''")) } else { tower.to_string() }
}

/// A repo of the draft, by its folder's name.
fn repo_name(folder: &str) -> &str {
    folder.trim_end_matches('/').rsplit('/').next().unwrap_or(folder)
}

/// The draft's repos, whole.
pub fn repos_of(draft: &Value) -> Vec<String> {
    draft["sessions"].as_array().into_iter().flatten().filter_map(|s| s["repo"].as_str()).map(|r| crate::dispatch::expand(r.trim()).trim_end_matches('/').to_string()).filter(|r| !r.is_empty()).collect()
}

/// The planner's instructions (`instructions.md`, and its `CLAUDE.md`).
/// `tower`: the program it runs `tower plan …` with.
pub fn instructions(p: &Plan, tower: &str) -> String {
    let t = tower_word(tower);
    let id = &p.id;
    let space = p.draft["space"].as_str().unwrap_or("the space");
    let sessions: Vec<String> = p.draft["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let repo = s["repo"].as_str().unwrap_or("");
            match s["why"].as_str().filter(|w| !w.trim().is_empty()) {
                Some(why) => format!("- `{}` ({repo}): {why}", repo_name(repo)),
                None => format!("- `{}` ({repo})", repo_name(repo)),
            }
        })
        .collect();
    let mut s = String::new();
    s += "# You are the tower's planner\n\n";
    s += &format!(
        "The user is about to launch the space \"{space}\": one Claude Code session per repository below, each with the ticket as its brief. Before anything opens, you write the full plan of it, publish it for the user to see, and talk it over with them. You never launch anything: the user does, from the tower's window, when they are ready.\n\n"
    );
    s += &format!("Your plan is `{id}`. Every `tower plan` command below names it.\n\n");
    s += "## What is in this folder\n\n";
    s += "- `ticket.md`: the ticket, as the user pasted it.\n";
    s += "- `draft.json`: the tower's draft of the space: its name (`space`), its goal (the ticket's title), what counts as done (`done_when`), one session per repository (`sessions[].repo` and why it is in it, `sessions[].why`), the project and the account.\n";
    s += "- `plan.md`: your plan, in markdown. You write it, and keep it in step with what you publish.\n";
    s += "- `instructions.md` (this file, also `CLAUDE.md`).\n\n";
    s += "The sessions of the draft:\n\n";
    s += &sessions.join("\n");
    s += "\n\nThe repositories are on this Mac at those paths. Read them (Read, Glob, Grep, `git log`) to plan well; never change them.\n\n";
    s += "## 1. Write the plan\n\n";
    s += "The full plan of the draft, concrete and short enough to read in five minutes:\n\n";
    s += "1. **The sessions**: for each one, what it does, and in what order: which starts first, what waits for what, what each hands the others.\n";
    s += "2. **How the repositories fit together**: the APIs, the data and the contracts between them (endpoints, payloads, events, schemas, settings), and which repository owns each.\n";
    s += "3. **The risks.**\n";
    s += "4. **The open questions**: never left on the page for the user to find. Each goes to the tower (see 3), which gets it answered: by its brain when it is routine, else as a card in the user's queue, on the Mac and the phone. The page lists each question with its answer, or as \"asked in the tower\" until it has one.\n";
    s += "5. **How it will be validated, for sure**: for each session (each task) and each pull request, the exact tests to add or run (the test file and the test's name), the commands, and the live checks (run it, open it, check the deployed state on dev), and the evidence each produces (test output, screenshots, logs); then the edge cases to cover, each tied to the check that covers it. This is what gives the user the confidence to merge and release: make it concrete enough that someone else could run every check and see its evidence.\n";
    s += "6. **The pull requests** it will open, when it changes code, each with its validation and edge cases (see 2).\n";
    s += "7. **The stages**: the milestones of the work, in order, each with what counts as done, how that is checked before its work starts (pre: the assumptions it rests on, access, data, environments) and after (post: the exact tests, commands and live checks, and the evidence each gives), and the PRs it delivers. What is unknown or risky comes first, as a short `spike` stage whose findings shape the rest. The last stage is the release, when there is one.\n";
    s += "8. **The assumptions**: what the plan takes for granted, each with how it is checked and by which stage.\n\n";
    s += "In `plan.md`, put each session under a `### <its repository's folder name>` heading inside a `## Sessions` section (a launched session is handed its own part), with a `#### Validation` (each check: what it shows, how, the evidence), a `#### Edge cases` (each case, and the check that covers it) and a `#### Who depends on it` (what outside uses what it changes, and whether it still works) inside it, and the rest under `##` headings of their own.\n\n";
    s += "## 2. The pull requests\n\n";
    s += "When the plan changes code, say what the future pull requests will look like: one entry per PR. Write them to `prs.json` in this folder:\n\n";
    s += "```json\n{\"prs\": [{\"repo\": \"<the repository's folder name>\", \"base\": \"dev\", \"title\": \"<the PR's title>\", \"changes\": \"<what it changes, in a sentence or two>\", \"files\": [\"<roughly which files>\"], \"migration\": false, \"breaking\": \"\", \"escalate\": [], \"after\": [],\n  \"checks\": [{\"id\": \"c1\", \"what\": \"<what it shows, concretely>\", \"how\": \"<the exact test to add or run, the command, or the live step>\", \"evidence\": \"<what it produces: test output, a screenshot, a log, the deployed state>\"}],\n  \"edge_cases\": [{\"case\": \"<the edge case>\", \"checks\": [\"c1\"]}],\n  \"dependents\": [{\"what\": \"<what it changes that something outside uses>\", \"used_by\": \"<repo, file, function>\", \"breaks\": false}],\n  \"outside\": \"<how you checked who depends on it; when nothing does, say so>\"}]}\n```\n\n";
    s += "- `base`: `dev`. The user's release rule: everything goes into dev first; promoting dev to main (or prod) is a release PR of its own, which the user merges.\n";
    s += "- `migration`: true when it adds or changes a database migration (a schema change, a new table or column, a backfill).\n";
    s += "- `breaking`: what it breaks, in a few words, when it changes an API, a contract or a schema others rely on, or removes or renames something public (an endpoint, a field, an event, an exported function, a setting); empty when nothing.\n";
    s += "- `escalate`: anything else on the tower's escalate list it touches, a few words each: secrets or credentials, infrastructure (AWS, servers, DNS, CI, Docker, Terraform), deploys or releases, auth and permissions, deleting or overwriting data, money or billing, new dependencies that matter, security.\n";
    s += "- `changes` says what it does, plainly; the flags above say what a person must look at. `after`: the numbers (from 1, in the list's order) of the PRs it waits for.\n";
    s += "- `checks` (at least one): how its implementation is validated, for sure. Each says `what` it shows, `how` (the exact test to add or run, with its file and name; the command; or the live step: run it, open it, check the deployed state on dev) and the `evidence` it produces (test output, a screenshot, a log). Give every check its own id (`c1`, `c2`, …). A migration gets a check that it applies and rolls back; a breaking change one for each side of the contract; a UI change a live check with a screenshot.\n";
    s += "- `edge_cases` (at least one): the edge cases to cover (empty input, the first and the last, failure and retry, permissions, concurrency, old data, a slow or missing service, …), each with the `checks` that cover it.\n";
    s += "- The session that opens the PR reports each check with its evidence (`tower validate`); a PR whose planned checks have not all passed is never merged without the user, whatever its score.\n";
    s += "- `dependents` and `outside`: whether it breaks other people's functionality. Find what outside the changed code depends on what it changes: callers of changed or removed functions, API routes, request and response shapes, DB columns, events, config keys, shared types, and the other repos or services that consume them (the dashboard calling a serverless API, the SDK, the mobile app). Search the user's repos under ~/Work (rg, git grep) and read the callers. List each with what it uses and who (repo, file, function), and `breaks: true` when the change would break it (then the PR is \"Needs review: a breaking change\", with that warning). Say in `outside` how you checked; when nothing outside depends on it, say so there and leave `dependents` empty. Give a breaking PR a check that compiles or runs the dependents' tests.\n\n";
    s += &format!(
        "Then run `{t} plan prs {id} --json-file prs.json`. The tower checks each PR and gives it a review score: **Needs review** (a migration, a breaking change, or something on the escalate list, from your flags, its files and its words) or **Low risk**, and why. It refuses a PR without checks, with an edge case no check covers, or that doesn't say who depends on it. When something outside would break, it is a breaking change: say what breaks and who uses it as plainly as the score. It prints them: put a `## Pull requests` section in `plan.md` and in what you publish, one entry per PR (its repository, into which branch, its title, what it changes, roughly which files) with its score and the reason exactly as the tower printed them, then its validation (each check: what, how, the evidence) and its edge cases (each with the checks that cover it), shown as plainly as the score. Be honest in the flags: a PR scored low risk may be merged into dev by the tower without a person reading it, when the user has turned that on.\n\n"
    );
    s += "## 3. The stages, the assumptions and the questions\n\n";
    s += "Write them to `stages.json` in this folder (after `tower plan prs`, so a stage can name its PRs by number):\n\n";
    s += "```json\n{\"stages\": [{\"id\": \"s1\", \"title\": \"<a few words>\", \"kind\": \"spike|build|release\", \"goal\": \"<what it delivers>\", \"done_when\": \"<what counts as done, concretely>\", \"after\": [], \"prs\": [1],\n  \"pre\": [{\"id\": \"p1\", \"what\": \"<what must hold before the work starts>\", \"how\": \"<how it is checked>\", \"evidence\": \"<what shows it>\"}],\n  \"post\": [{\"id\": \"c1\", \"what\": \"<what it shows>\", \"how\": \"<the exact test, command or live step>\", \"evidence\": \"<test output, a screenshot, a log, the deployed state>\"}]}],\n \"assumptions\": [{\"id\": \"a1\", \"text\": \"<what the plan takes for granted>\", \"check\": \"<how it is checked>\", \"by\": \"s1\"}],\n \"questions\": [{\"id\": \"q1\", \"text\": \"<the question>\", \"why\": \"<what turns on it>\", \"options\": [{\"label\": \"<an answer>\", \"why\": \"<why>\"}], \"recommended\": 0, \"theirs\": false, \"stage\": \"s2\"}]}\n```\n\n";
    s += "- Every stage has `done_when` and at least one `post` check; a stage's `prs` are the numbers `tower plan prs` gave; `after` names stages before it. A stage is done only when every check of it passed with evidence, and the PRs it delivers passed theirs.\n";
    s += "- Spikes first: when an API, a data shape, a library, access or performance is not known, make it a `spike` stage at the start whose post-checks say what it must find out; the build stages wait for it (`after`).\n";
    s += "- `questions`: each with two or three `options` and the one you recommend (`recommended`, an index). `theirs: true` when only the user can decide it (scope, product, money, who does what, anything they'd want a say in); else the tower's brain answers it when it is routine and passes it on when it isn't. Secrets, deploys, deletes, production data and money always go to the user.\n";
    s += &format!("- Then run `{t} plan stages {id} --json-file stages.json`. It refuses a stage without what counts as done or a post check, a PR the plan doesn't have, or a stage waiting for one after it, and says why.\n");
    s += &format!(
        "- The answers come back to you as `[plan {id}] q2 is answered by …`: write each into `plan.md` and the artifact, change what it changes (and send `stages.json` again when the stages change), and republish.\n\n"
    );
    s += "Once you publish, a critic reads the plan before the user sees it: its gaps reach you as `[plan <id>] A critic read the plan …`. Fix what is real and send the stages again; the questions go out to the user after that, so they see the plan once, with only the real questions.\n\n";
    s += "## 4. Publish it\n\n";
    match p.form {
        Form::Page => {
            s += &format!(
                "Publish it with your Artifact tool as an **HTML page** Artifact: one self-contained page titled \"Plan · {space}\", with the stages first (stage N of M, each with its kind, what counts as done, its pre and post checks, each check's what, how and evidence, and its PRs), the sessions in order (one card each), how the repositories fit together, the assumptions (each with its check and the stage it must hold by), the risks, the questions (each answered one with its answer and who gave it; an open one as \"asked in the tower\"), and the pull requests, each with its score, its validation (every check: what, how, the evidence) and its edge cases, as plainly as the score. It is private to the user's account. Publish one artifact only; every change later republishes this same one (its `url`).\n\n"
            );
        }
        Form::Whiteboard => {
            s += &format!(
                "Publish it with your Artifact tool as a **Whiteboard** Artifact, made from the account's Artifact type `{WHITEBOARD_TYPE}` (publish with that `type_url` and the title \"Plan · {space}\", then fill it as the type's instructions say): a block per session (its repository, what it does, its place in the order), arrows for the dependencies between them (who waits for whom, who calls whom), a lane of stages in order (each with what counts as done and its pre and post checks), a section for the assumptions, the risks and the questions (with their answers, or \"asked in the tower\"), and a block per pull request with its score, its checks and its edge cases. It is private to the user's account. Make one whiteboard only; every change later updates this same one (its `url`).\n\n"
            );
        }
    }
    s += &format!("Then say so: `{t} plan published {id} --url <the artifact's link>`.\n\n");
    s += &format!("If you can't publish it (no Artifact tool, an error that stays), say why and stop: `{t} plan failed {id} \"<why, in a line>\"`.\n\n");
    s += "## 5. Then: the chat\n\n";
    s += &format!(
        "After that you wait. What the user says about the plan reaches you as a prompt that begins `[plan {id}] The user says:`. Answer it in a few sentences with `{t} plan say {id} \"<your answer>\"`: that is the only way your answer reaches them (nobody reads this terminal). Never ask anything in this terminal; ask with `plan say`.\n\n"
    );
    s += "- A question: answer it from the plan and the repositories.\n";
    s += "- A change: say what you would change, and ask whether to. Once the user agrees (or asked for it outright), make it, then say what changed:\n";
    s += &format!(
        "  - **the draft** (which repositories get a session, the space's name, its goal, what counts as done): write the new draft to `draft.json` (the same shape; keep what you don't change; a repository must be a project folder on this Mac, see `{t} setups`) and run `{t} plan draft {id} --json-file draft.json`. It says what it took.\n"
    );
    s += &format!(
        "  - **the plan**: change `plan.md`, republish the same artifact (your Artifact tool, with its `url`), and run `{t} plan published {id} --url <its link>` again.\n"
    );
    s += &format!("  - **the pull requests**: change `prs.json`, run `{t} plan prs {id} --json-file prs.json` again, and put the new scores in `plan.md` and the artifact.\n\n");
    s += "## 6. A live plan\n\n";
    s += "Once the space is launched the plan stays live: its sessions report what they find (an assumption that proved wrong, a PR that has to split, a new risk, a check that can't run), and you revise the plan for it. A finding reaches you as `[plan <id>] Finding f2 …`.\n\n";
    s += &format!("- Change `stages.json` for it and run `{t} plan revise {id} --json-file stages.json --why \"<what made it change, in a sentence>\" --finding f2`. The tower compares it with the plan as it is: a small change (a check added or reworded, a PR split inside a stage, an assumption or a question added) is applied at once and noted; a big one (a stage added or removed, a stage's done-when changed, a check or an assumption dropped) goes to the user as one card with what changes, and is applied only on their yes. It says which.\n");
    s += "- Put it in `plan.md` and republish the same artifact: the version at the top, and a \"What changed\" section, newest first, each revision with what changed, why, and the finding that caused it.\n";
    s += &format!("- A finding that changes nothing: say why with `{t} plan keep {id} --why \"…\"` (it is noted as a revision of no change), and leave the plan as it is.\n");
    s += &format!("- A spike stage done: what it found reaches you as a finding. The stages after it wait until you take it in: `{t} plan revise …` when the plan changes, `{t} plan keep {id} --why \"…\"` when it doesn't.\n");
    s += "- Sessions end their turns on the plan's tasks with a Handoff block; what it says is uncertain, what failed or blocked, and the findings it lists reach you as findings too.\n\n";
    s += "## Never\n\n";
    s += "- Never launch the space, open or start a session, queue or send anything to a session, or open or close a space: no `tower launch`, `tower start`, `tower queue`, `tower dispatch`, `tower space`, no `hc`. The user launches it from the tower's window.\n";
    s += "- Never edit, commit or push in a repository. Write only in this folder.\n";
    s += "- Never publish a second artifact: republish this plan's own.\n";
    s
}

/// The planner's first task: write it, publish it, say so. For a live
/// plan: take in the findings that wait, and revise.
pub fn first_task(p: &Plan, tower: &str) -> String {
    let t = tower_word(tower);
    if p.status == Status::Live {
        let waiting: Vec<String> = p.findings.iter().filter(|f| f.revision.is_none()).map(|f| format!("{} ({}): {}", f.id, f.kind, f.text)).collect();
        return format!(
            "[plan {}] The plan is live: the space {} was launched with it, and its sessions are at work. Read instructions.md (\"6. A live plan\"), plan.md and stages.json. Findings waiting: {}. Revise the plan for them with `{t} plan revise {} --json-file stages.json --why \"<what made it change>\" --finding <ids>`, put it in plan.md, republish the same artifact with what changed, and run `{t} plan published {} --url <its link>`. Then wait for more.",
            p.id,
            p.launched.as_deref().unwrap_or("of it"),
            if waiting.is_empty() { "none yet".to_string() } else { waiting.join("; ") },
            p.id,
            p.id
        );
    }
    let form = match p.form {
        Form::Page => "an HTML page Artifact".to_string(),
        Form::Whiteboard => format!("a Whiteboard Artifact (from the Artifact type {WHITEBOARD_TYPE})"),
    };
    format!(
        "[plan {}] Read instructions.md in this folder, then ticket.md and draft.json. Write the full plan of the draft into plan.md; when it changes code, list its pull requests in prs.json and score them with `{t} plan prs {} --json-file prs.json`; write its stages, assumptions and questions in stages.json and send them with `{t} plan stages {} --json-file stages.json`; publish it as {form}, and say so with `{t} plan published {} --url <its link>`. Then wait for the user's messages and the answers to its questions.",
        p.id, p.id, p.id, p.id
    )
}

/// What the user said, as the planner gets it.
pub fn chat_prompt(p: &Plan, text: &str, tower: &str) -> String {
    let t = tower_word(tower);
    format!(
        "[plan {}] The user says: {}\n\nAnswer briefly with `{t} plan say {} \"…\"`. A change they agree to: as instructions.md says (draft.json and `plan draft`, or plan.md and the same artifact republished).",
        p.id,
        text.trim(),
        p.id
    )
}

/// The planner's Claude Code settings: what it may do without asking, and
/// what it may never do. It reads anywhere (the repos too), writes in its
/// own folder only, runs `tower plan …` (and `tower setups`), publishes
/// its artifact, and never asks in its terminal.
pub fn settings(tower: &str, repos: &[String]) -> Value {
    let mut allow: Vec<String> = vec!["Read".into(), "Glob".into(), "Grep".into(), "Edit(./**)".into(), "Write(./**)".into(), "Artifact".into()];
    for t in [tower, "tower"] {
        allow.extend([format!("Bash({t} plan *)"), format!("Bash({t} setups*)")]);
    }
    allow.extend(["git log", "git show", "git diff", "git status", "git ls-files", "ls", "cat", "head", "wc", "rg", "grep", "find"].iter().map(|c| format!("Bash({c} *)")));
    let mut deny: Vec<String> = vec!["AskUserQuestion".into()];
    for t in [tower, "tower"] {
        deny.extend(["launch", "start", "queue", "dispatch", "space", "answer", "agenda", "undo"].iter().map(|c| format!("Bash({t} {c}*)")));
    }
    deny.extend(["hc", "herdr-core", "herdr-app", "cca", "git commit", "git push", "git checkout", "git switch", "git reset", "git merge", "git rebase", "git stash", "git worktree", "git branch -D"].iter().map(|c| format!("Bash({c} *)")));
    // No edit in a repo of the draft: deny wins over every allow.
    deny.extend(repos.iter().map(|r| format!("Edit(/{}/**)", r.trim_end_matches('/'))));
    json!({ "permissions": { "allow": allow, "deny": deny, "additionalDirectories": repos } })
}

/// Writes the planner's folder: the ticket, the draft, its instructions
/// (`instructions.md` and `CLAUDE.md`), and its settings.
pub fn write_folder(p: &Plan, tower: &str) -> Result<PathBuf> {
    let dir = PathBuf::from(&p.dir);
    std::fs::create_dir_all(dir.join(".claude"))?;
    std::fs::write(dir.join("ticket.md"), format!("{}\n", p.ticket.trim()))?;
    std::fs::write(dir.join("draft.json"), serde_json::to_string_pretty(&p.draft)?)?;
    let text = instructions(p, tower);
    std::fs::write(dir.join("instructions.md"), &text)?;
    std::fs::write(dir.join("CLAUDE.md"), &text)?;
    std::fs::write(dir.join(".claude/settings.json"), serde_json::to_string_pretty(&settings(tower, &repos_of(&p.draft)))?)?;
    Ok(dir)
}

/// The tower's own folders, as this process has them, when they are not
/// the usual ones (a tower run beside the real one): the planner's `tower
/// plan …` reaches this daemon, not another.
pub fn tower_env() -> Vec<(String, String)> {
    ["HERDER_TOWER_HOME", "HERDER_TOWER_STATE", "HERDER_TOWER_DATA", "HERDER_TOWER_BIN"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
        .collect()
}

/// The planner's command line: `env` (this plan's id, and the tower's
/// folders when they are not the usual ones) then `cca <the default
/// account>` (or plain `claude` without the accounts mod), named
/// `tower-planner`, on the brains' model and effort, with its first task.
pub fn argv(p: &Plan, cfg: &crate::config::Config, cca: Option<&str>, account: Option<&str>, tower: &str, env: &[(String, String)]) -> Result<Vec<String>> {
    let mut a: Vec<String> = vec!["env".into(), "-u".into(), "CLAUDECODE".into(), "-u".into(), "CLAUDE_CODE_ENTRYPOINT".into()];
    if cca.is_none() || account.is_none() {
        // Plain `claude` is the default account: no other's folder.
        a.extend(["-u", "CLAUDE_CONFIG_DIR", "-u", "CCA_ACCOUNT"].map(String::from));
    }
    a.push(format!("{ENV_PLAN}={}", p.id));
    a.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    // Its model and effort are the brains' (`tower model`), as a brain's are.
    let brain = crate::brains::argv("claude", PLANNER, &p.dir, tower, cfg)?;
    match (cca, account) {
        (Some(cca), Some(acc)) => a.extend([cca.to_string(), acc.to_string()]),
        _ => a.push("claude".into()),
    }
    a.extend(brain.into_iter().skip(1));
    a.push(first_task(p, tower));
    Ok(a)
}

// ---------------------------------------------------------------------------
// A launch with a plan.

/// What a launched session is given of the plan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanBrief {
    pub id: String,
    pub url: Option<String>,
    /// `plan.md` as the planner left it.
    pub md: String,
    /// The PRs it predicts, with their checks and edge cases.
    pub prs: Vec<crate::prscore::Predicted>,
    /// Its stages, with what counts as done and their checks.
    pub stages: Vec<Stage>,
}

/// A markdown heading's level and words, if the line is one.
fn heading(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    let level = t.chars().take_while(|c| *c == '#').count();
    (level > 0 && level <= 6 && t[level..].starts_with(' ')).then(|| (level, t[level..].trim()))
}

/// A heading names this repo: one of its words is the repo's name.
fn names_repo(words: &str, repo: &str) -> bool {
    !repo.is_empty() && words.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '.')).any(|w| w.trim_matches('.').eq_ignore_ascii_case(repo))
}

/// The part of `md` for the session of `mine`: the whole plan when it fits
/// in `max` characters; else without the sections of the other sessions
/// (a heading that names one of `others` and not `mine`, to the next
/// heading of its level or above), cut at a line to `max`.
pub fn part_for(md: &str, mine: &str, others: &[String], max: usize) -> (String, bool) {
    let md = md.trim();
    if md.chars().count() <= max {
        return (md.to_string(), false);
    }
    let mut kept: Vec<&str> = vec![];
    let mut skip_under: Option<usize> = None;
    for line in md.lines() {
        if let Some((level, words)) = heading(line) {
            if skip_under.is_some_and(|l| level <= l) {
                skip_under = None;
            }
            if skip_under.is_none() && !names_repo(words, mine) && others.iter().any(|o| names_repo(words, o)) {
                skip_under = Some(level);
            }
        }
        if skip_under.is_none() {
            kept.push(line);
        }
    }
    let mut out = String::new();
    let mut cut = false;
    for line in kept {
        if out.chars().count() + line.chars().count() + 1 > max {
            cut = true;
            // A line longer than all of it: its start.
            if out.is_empty() {
                out = line.chars().take(max.saturating_sub(1)).collect::<String>() + "…";
            }
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    (out.trim_end().to_string(), cut)
}

/// What a launched session's first instruction says of the plan: its link,
/// and its part of `plan.md`.
pub fn brief_text(b: &PlanBrief, mine: &str, others: &[String]) -> String {
    let (part, cut) = part_for(&b.md, mine, others, BRIEF_CHARS);
    let link = b.url.as_deref().map(|u| format!(" ({u})")).unwrap_or_default();
    let mut s = format!("The space's plan{link}, as the tower's planner wrote it with the user: follow it, and say so when you must leave it.");
    if !part.is_empty() {
        s += "\n\n";
        s += &part;
    }
    if cut {
        s += &match &b.url {
            Some(u) => format!("\n\n(The plan goes on: all of it is at {u}.)"),
            None => "\n\n(The plan goes on past this.)".to_string(),
        };
    }
    s += &checks_text(&b.prs, mine);
    s += &stages_text(&b.id, &b.stages, &b.prs, mine);
    s
}

/// What a launched session is told of the plan's stages: each with what
/// counts as done and its checks (its own PRs marked), and how a stage's
/// check is reported. A stage is done only when every check of it passed
/// with evidence.
pub fn stages_text(id: &str, stages: &[Stage], prs: &[crate::prscore::Predicted], mine: &str) -> String {
    if stages.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "\n\nThe plan's stages, in order. Spike stages come first: their findings may change the plan before the build stages start, so a build stage waits until the spikes it follows are done and taken in (the tower says when). A stage is done only when every check of it passed with evidence; work on a stage starts once its pre-checks passed. Report a stage's check you ran with `tower plan check {id} <stage> <check> passed|failed|not-run --evidence \"<the test output, a screenshot's path, a log, or why it wasn't run>\"`, and what you find that bears on the plan (an assumption that proved wrong, a PR that has to split, a new risk, a check that can't run) with `tower plan finding {id} --kind <kind> \"<what>\" --stage <stage>`. End each turn on the plan's work with this block:\n\n{}",
        HANDOFF_FORMAT.replace("<plan>", id)
    );
    for st in stages {
        let own: Vec<String> = st.prs.iter().filter_map(|n| prs.iter().find(|p| p.n == *n)).filter(|p| !mine.is_empty() && p.repo.eq_ignore_ascii_case(mine)).map(|p| format!("PR {}", p.n)).collect();
        s += &format!("\n\n{} ({}): {}. Done when: {}", st.id, st.kind, st.title, st.done_when.trim_end_matches('.'));
        if !own.is_empty() {
            s += &format!(". Yours: {}", own.join(", "));
        }
        for (when, c) in st.checks() {
            s += &format!("\n- {when} {}: {} (how: {}; evidence: {})", c.id, c.what, c.how, c.evidence);
        }
    }
    s
}

/// What a launched session is told of the PRs the plan predicts for its
/// repo: each one's checks and edge cases, and how to report each check.
pub fn checks_text(prs: &[crate::prscore::Predicted], mine: &str) -> String {
    let own: Vec<&crate::prscore::Predicted> = prs.iter().filter(|p| !mine.is_empty() && p.repo.eq_ignore_ascii_case(mine) && !p.checks.is_empty()).collect();
    if own.is_empty() {
        return String::new();
    }
    let mut s = "\n\nThe PRs the plan predicts for you, and how each is validated. Run every check; once the PR is open, report each one with its evidence: `tower validate <the PR's link> <check> passed|failed|not-run --evidence \"<the test output, a screenshot's path, a log, or why it wasn't run>\"`. Report them before you finish your turn on the PR: what isn't reported then counts as not run, and a PR whose planned checks haven't all passed is never merged without the user.".to_string();
    for p in own {
        s += &format!("\n\nPR {} into {}: {}", p.n, p.base, p.title);
        for c in &p.checks {
            s += &format!("\n- {}: {} (how: {}; evidence: {})", c.id, c.what, c.how, c.evidence);
        }
        for e in &p.edge_cases {
            s += &format!("\n- edge case: {} (covered by {})", e.case, e.checks.join(", "));
        }
    }
    s
}

/// The plan `id`, as the daemon has it (or `plans.json`, without one).
pub fn show(id: Option<&str>) -> Result<Value> {
    if crate::protocol::is_running() {
        return request(&Args { action: "show".into(), id: id.map(String::from), ..Default::default() });
    }
    let book: Book = std::fs::read_to_string(crate::config::state_dir().join("plans.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    Ok(book.pick(id)?.view())
}

/// No plan goes out with questions on its page: a launch with one whose
/// questions aren't all answered is refused, and says which.
pub fn no_open_questions(id: &str, v: &Value) -> Result<()> {
    let open: Vec<String> = v["questions"].as_array().into_iter().flatten().filter(|q| q["status"] != Q_ANSWERED).map(|q| format!("{} ({})", q["id"].as_str().unwrap_or(""), q["text"].as_str().unwrap_or(""))).collect();
    if open.is_empty() {
        return Ok(());
    }
    bail!(
        "--plan {id}: {} question{} still open: {}. Answer {} in the tower's queue (or `tower plan answer {id} <question> \"<answer>\"`) first",
        open.len(),
        if open.len() == 1 { " is" } else { "s are" },
        open.join("; "),
        if open.len() == 1 { "it" } else { "them" }
    )
}

/// What a launch with the plan `id` hands its sessions: its link and its markdown.
pub fn brief_of(id: &str) -> Result<PlanBrief> {
    let v = show(Some(id))?;
    let md = v["plan_md"].as_str().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let url = v["url"].as_str().map(String::from);
    if md.trim().is_empty() && url.is_none() {
        bail!("--plan {id}: the plan isn't written yet (no plan.md, no link)");
    }
    no_open_questions(id, &v)?;
    let prs = serde_json::from_value(v["prs"].clone()).unwrap_or_default();
    let stages = serde_json::from_value(v["stages"].clone()).unwrap_or_default();
    Ok(PlanBrief { id: id.to_string(), url, md, prs, stages })
}

// ---------------------------------------------------------------------------
// The CLI.

/// `tower plan <action> …` as given.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Args {
    /// new | say | draft | prs | published | failed | show | close | list
    pub action: String,
    pub id: Option<String>,
    /// What `say` and `failed` say.
    pub words: String,
    pub form: Option<Form>,
    /// `new`: the draft's JSON.
    pub draft: Option<String>,
    /// `new`: the file the ticket is in.
    pub ticket_file: Option<String>,
    /// `draft`, `prs`: the file the revised draft, or the predicted PRs, are in.
    pub json_file: Option<String>,
    /// `published`: the artifact's link.
    pub url: Option<String>,
    /// `say`: whose words (none: the planner's from its own terminal, else the user's).
    pub who: Option<Who>,
    /// `close`: the space launched with it.
    pub launched: Option<String>,
    /// `answer`: who answers (`tower`: the brain; else the user).
    pub by: Option<String>,
    /// `check`, `assume`: the evidence of its result.
    pub evidence: Option<String>,
    /// `finding`: what it is about; its stage, assumption and session.
    pub kind: Option<String>,
    pub stage: Option<String>,
    pub assumption: Option<String>,
    pub session: Option<String>,
    /// `revise`: why, and the findings it takes in.
    pub why: Option<String>,
    /// `new`: the account its planner runs on.
    pub account: Option<String>,
    pub finding: Option<String>,
    pub json: bool,
}

const USAGE: &str = "tower plan finding <id> --kind <kind> \"<what was found>\" [--stage s] [--assumption a] | assume <id> <a> holds|broken --evidence \"<…>\" | revise <id> --json-file stages.json --why \"<…>\" [--finding f1] | new --form page|whiteboard --draft '<draft JSON>' --ticket-file <file> | say <id> \"<words>\" [--as user|planner] | draft <id> --json-file <file> | prs <id> --json-file <file> | stages <id> --json-file <file> | answer <id> <question> \"<answer>\" [--by tower|you] | ask <id> <question> | check <id> <stage> <check> passed|failed|not-run --evidence \"<…>\" | published <id> --url <link> | failed <id> \"<why>\" | show [<id>] [--json] | close [<id>] | list";

/// Reads `tower plan …`'s words (after `plan`): the action, its flags, the id, the words.
pub fn parse_args(raw: &[String]) -> Result<Args> {
    let mut a = Args::default();
    let mut words: Vec<String> = vec![];
    let mut it = raw.iter().peekable();
    let mut rest = false;
    while let Some(w) = it.next() {
        if rest {
            words.push(w.clone());
            continue;
        }
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (w.as_str(), None),
        };
        let mut value = |name: &str| -> Result<String> {
            match inline.clone().or_else(|| it.next().cloned()) {
                Some(v) => Ok(v),
                None => bail!("{name} needs a value: {USAGE}"),
            }
        };
        match flag {
            "--" => rest = true,
            "--form" => a.form = Some(Form::parse(&value("--form")?)?),
            "--draft" => a.draft = Some(value("--draft")?),
            "--ticket-file" => a.ticket_file = Some(value("--ticket-file")?),
            "--json-file" => a.json_file = Some(value("--json-file")?),
            "--url" => a.url = Some(value("--url")?),
            "--as" => a.who = Some(Who::parse(&value("--as")?)?),
            "--launched" => a.launched = Some(value("--launched")?),
            "--by" => a.by = Some(value("--by")?),
            "--evidence" => a.evidence = Some(value("--evidence")?),
            "--kind" => a.kind = Some(value("--kind")?),
            "--stage" => a.stage = Some(value("--stage")?),
            "--assumption" => a.assumption = Some(value("--assumption")?),
            "--session" => a.session = Some(value("--session")?),
            "--why" => a.why = Some(value("--why")?),
            "--account" => a.account = Some(value("--account")?),
            "--finding" => a.finding = Some(value("--finding")?),
            "--json" => a.json = true,
            f if f.starts_with("--") && f.len() > 2 => bail!("tower plan: no flag {f}: {USAGE}"),
            _ => words.push(w.clone()),
        }
    }
    let mut words = words.into_iter();
    a.action = words.next().unwrap_or_else(|| "show".into()).to_lowercase();
    let mut words: Vec<String> = words.collect();
    if !words.is_empty() && is_id(words[0].trim()) {
        a.id = Some(words.remove(0).trim().to_string());
    }
    a.words = words.join(" ").trim().to_string();
    match a.action.as_str() {
        "new" => {
            if a.form.is_none() || a.draft.is_none() || a.ticket_file.is_none() {
                bail!("tower plan new --form page|whiteboard --draft '<the draft's JSON>' --ticket-file <the ticket's file>");
            }
        }
        "say" | "failed" if a.id.is_none() || a.words.is_empty() => bail!("tower plan {} <id> \"<words>\"", a.action),
        "draft" if a.id.is_none() || a.json_file.is_none() => bail!("tower plan draft <id> --json-file <the revised draft>"),
        "prs" if a.id.is_none() || a.json_file.is_none() => bail!("tower plan prs <id> --json-file <the predicted PRs: {{\"prs\":[{{\"repo\",\"base\",\"title\",\"changes\",\"files\",\"migration\",\"breaking\",\"escalate\",\"after\"}}]}}>"),
        "published" if a.id.is_none() || a.url.is_none() => bail!("tower plan published <id> --url <the artifact's link>"),
        "stages" if a.id.is_none() || a.json_file.is_none() => bail!("tower plan stages <id> --json-file <the plan's stages, assumptions and questions>"),
        "answer" if a.id.is_none() || a.words.split_whitespace().count() < 2 => bail!("tower plan answer <id> <question> \"<the answer>\" [--by tower|you]"),
        "ask" if a.id.is_none() || a.words.split_whitespace().count() != 1 => bail!("tower plan ask <id> <question>: it goes to the user's queue"),
        "check" if a.id.is_none() || a.words.split_whitespace().count() != 3 || a.evidence.is_none() => {
            bail!("tower plan check <id> <stage> <check> passed|failed|not-run --evidence \"<the test output, a screenshot's path, a log, or why it wasn't run>\"")
        }
        "finding" if a.id.is_none() || a.kind.is_none() || a.words.is_empty() => {
            bail!("tower plan finding <id> --kind {} \"<what was found>\" [--stage <s>] [--assumption <a>]", FINDING_KINDS.join("|"))
        }
        "assume" if a.id.is_none() || a.words.split_whitespace().count() != 2 || a.evidence.is_none() => bail!("tower plan assume <id> <assumption> holds|broken --evidence \"<what showed it>\""),
        "revise" if a.id.is_none() || a.json_file.is_none() || a.why.is_none() => bail!("tower plan revise <id> --json-file stages.json --why \"<what made it change>\" [--finding f1,f2]"),
        "keep" if a.id.is_none() || a.why.is_none() => bail!("tower plan keep <id> --why \"<why what was found changes nothing>\""),
        "say" | "failed" | "draft" | "prs" | "stages" | "answer" | "ask" | "check" | "finding" | "assume" | "revise" | "keep" | "published" | "show" | "close" | "list" => {}
        other => bail!("tower plan {other}: {USAGE}"),
    }
    if !a.words.is_empty() && !matches!(a.action.as_str(), "say" | "failed" | "answer" | "ask" | "check" | "finding" | "assume") {
        bail!("tower plan {}: what is {:?}? {USAGE}", a.action, a.words);
    }
    Ok(a)
}

/// `say`'s words are the planner's when they come from its own terminal
/// (its environment names the plan), the user's otherwise.
pub fn who_says(asked: Option<Who>, env_plan: Option<&str>, id: &str) -> Who {
    asked.unwrap_or(if env_plan.is_some_and(|p| p.trim() == id) { Who::Planner } else { Who::User })
}

/// The daemon's answer to `a` (its `state`).
pub fn request(a: &Args) -> Result<Value> {
    use crate::protocol::{self, Request};
    let r = protocol::send(&Request::Plan {
        action: a.action.clone(),
        id: a.id.clone().unwrap_or_default(),
        form: a.form.map(|f| f.word().to_string()).unwrap_or_default(),
        ticket: String::new(),
        draft: None,
        text: if a.action == "keep" { a.why.clone().unwrap_or_default() } else { a.words.clone() },
        url: a.url.clone().unwrap_or_default(),
        who: a.who.map(|w| w.word().to_string()).unwrap_or_default(),
        launched: a.launched.clone(),
        prs: None,
        body: body_of(a),
    })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_else(|| "the tower said no".into()));
    }
    Ok(r.state.unwrap_or_default())
}

/// What `answer`, `ask` and `check` name, for the daemon.
fn body_of(a: &Args) -> Option<Value> {
    let w: Vec<&str> = a.words.split_whitespace().collect();
    match a.action.as_str() {
        "answer" => Some(json!({ "question": w.first().copied().unwrap_or(""), "answer": w.get(1..).map(|r| r.join(" ")).unwrap_or_default(), "by": a.by.clone().unwrap_or_default() })),
        "ask" => Some(json!({ "question": w.first().copied().unwrap_or("") })),
        "check" => Some(json!({ "stage": w.first(), "check": w.get(1), "status": w.get(2), "evidence": a.evidence.clone().unwrap_or_default() })),
        "finding" => Some(json!({ "kind": a.kind, "text": a.words, "stage": a.stage.clone().unwrap_or_default(), "assumption": a.assumption.clone().unwrap_or_default(), "session": a.session.clone().unwrap_or_default() })),
        "assume" => Some(json!({ "assumption": w.first(), "status": w.get(1), "evidence": a.evidence.clone().unwrap_or_default(), "session": a.session.clone().unwrap_or_default() })),
        _ => None,
    }
}

/// A plan in words.
pub fn said(v: &Value) -> String {
    let id = v["id"].as_str().unwrap_or("");
    let space = v["draft"]["space"].as_str().unwrap_or("");
    let mut out = format!("Plan {id} of \"{space}\" ({}): {}", v["form"].as_str().unwrap_or(""), v["status"].as_str().unwrap_or(""));
    if let Some(u) = v["url"].as_str() {
        out += &format!("\n  {u}");
    }
    if v["blocked"] == true {
        out += "\n  The planner waits on a question in its terminal (tower show tower-planner).";
    }
    if let Some(w) = v["why"].as_str().filter(|w| !w.is_empty()) {
        out += &format!("\n  {w}");
    }
    let prs: Vec<crate::prscore::Predicted> = serde_json::from_value(v["prs"].clone()).unwrap_or_default();
    if !prs.is_empty() {
        out += "\n  Pull requests:";
        for line in crate::prscore::said(&prs).lines() {
            out += &format!("\n    {line}");
        }
    }
    for m in v["thread"].as_array().into_iter().flatten() {
        out += &format!("\n  {}: {}", if m["from"] == "user" { "You" } else { "Planner" }, m["text"].as_str().unwrap_or(""));
    }
    out
}

/// `tower plan …`.
pub fn cmd(raw: &[String]) -> Result<()> {
    let mut a = parse_args(raw)?;
    let v = match a.action.as_str() {
        "new" => {
            let draft: Value = serde_json::from_str(a.draft.as_deref().unwrap_or("")).map_err(|e| anyhow::anyhow!("--draft: not a draft's JSON ({e})"))?;
            check_draft(&draft)?;
            let file = a.ticket_file.clone().unwrap_or_default();
            let ticket = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("--ticket-file {file}: {e}"))?;
            if ticket.trim().is_empty() {
                bail!("--ticket-file {file}: the ticket is empty");
            }
            let r = crate::protocol::send(&crate::protocol::Request::Plan {
                action: "new".into(),
                id: String::new(),
                form: a.form.unwrap_or_default().word().into(),
                ticket,
                draft: Some(draft),
                text: String::new(),
                url: String::new(),
                who: String::new(),
                launched: None,
                prs: None,
                body: a.account.clone().map(|acc| json!({ "account": acc })),
            })?;
            if !r.ok {
                bail!("{}", r.error.unwrap_or_default());
            }
            r.state.unwrap_or_default()
        }
        "say" => {
            let id = a.id.clone().unwrap_or_default();
            a.who = Some(who_says(a.who, std::env::var(ENV_PLAN).ok().as_deref(), &id));
            request(&a)?
        }
        "draft" => {
            let id = a.id.clone().unwrap_or_default();
            let file = a.json_file.clone().unwrap_or_default();
            let text = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("--json-file {file}: {e}"))?;
            let plan = show(Some(&id))?;
            let ticket = plan["ticket"].as_str().unwrap_or("").to_string();
            let r = revised(&text, &ticket, &crate::launch::world_now()?)?;
            let sent = crate::protocol::send(&crate::protocol::Request::Plan {
                action: "draft".into(),
                id,
                form: String::new(),
                ticket: String::new(),
                draft: Some(r.clone()),
                text: String::new(),
                url: String::new(),
                who: String::new(),
                launched: None,
                prs: None,
                body: None,
            })?;
            if !sent.ok {
                bail!("{}", sent.error.unwrap_or_default());
            }
            // What it took, for the planner to tell the user.
            if !(a.json || !std::io::stdout().is_terminal()) {
                println!("{}", crate::ticket::said(&r));
                return Ok(());
            }
            let mut v = sent.state.unwrap_or_default();
            v["took"] = json!(crate::ticket::said(&r));
            v
        }
        "stages" => {
            let id = a.id.clone().unwrap_or_default();
            let file = a.json_file.clone().unwrap_or_default();
            let text = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("--json-file {file}: {e}"))?;
            let v: Value = serde_json::from_str(text.trim()).map_err(|e| anyhow::anyhow!("--json-file {file}: not JSON ({e})"))?;
            // Checked here first, to say what's wrong before the daemon is asked.
            let plan = show(Some(&id))?;
            let prs: Vec<crate::prscore::Predicted> = serde_json::from_value(plan["prs"].clone()).unwrap_or_default();
            structure(&v, &prs)?;
            let sent = crate::protocol::send(&crate::protocol::Request::Plan {
                action: "stages".into(),
                id,
                form: String::new(),
                ticket: String::new(),
                draft: None,
                text: String::new(),
                url: String::new(),
                who: String::new(),
                launched: None,
                prs: None,
                body: Some(v),
            })?;
            if !sent.ok {
                bail!("{}", sent.error.unwrap_or_default());
            }
            sent.state.unwrap_or_default()
        }
        "revise" => {
            let id = a.id.clone().unwrap_or_default();
            let file = a.json_file.clone().unwrap_or_default();
            let text = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("--json-file {file}: {e}"))?;
            let v: Value = serde_json::from_str(text.trim()).map_err(|e| anyhow::anyhow!("--json-file {file}: not JSON ({e})"))?;
            let findings: Vec<String> = a.finding.clone().unwrap_or_default().split([',', ' ']).map(str::trim).filter(|f| !f.is_empty()).map(String::from).collect();
            let sent = crate::protocol::send(&crate::protocol::Request::Plan {
                action: "revise".into(),
                id,
                form: String::new(),
                ticket: String::new(),
                draft: None,
                text: a.why.clone().unwrap_or_default(),
                url: String::new(),
                who: a.who.map(|w| w.word().to_string()).unwrap_or_default(),
                launched: None,
                prs: None,
                body: Some(json!({ "structure": v, "findings": findings })),
            })?;
            if !sent.ok {
                bail!("{}", sent.error.unwrap_or_default());
            }
            sent.state.unwrap_or_default()
        }
        "prs" => {
            let id = a.id.clone().unwrap_or_default();
            let file = a.json_file.clone().unwrap_or_default();
            let text = std::fs::read_to_string(&file).map_err(|e| anyhow::anyhow!("--json-file {file}: {e}"))?;
            let v: Value = serde_json::from_str(text.trim()).map_err(|e| anyhow::anyhow!("--json-file {file}: not JSON ({e})"))?;
            // Checked here first, to say what's wrong before the daemon is asked.
            let plan = show(Some(&id))?;
            crate::prscore::check(&v, &repos_of(&plan["draft"]))?;
            let sent = crate::protocol::send(&crate::protocol::Request::Plan {
                action: "prs".into(),
                id,
                form: String::new(),
                ticket: String::new(),
                draft: None,
                text: String::new(),
                url: String::new(),
                who: String::new(),
                launched: None,
                prs: Some(v),
                body: None,
            })?;
            if !sent.ok {
                bail!("{}", sent.error.unwrap_or_default());
            }
            let v = sent.state.unwrap_or_default();
            // The scores, for the planner to put in the plan as they are.
            if !(a.json || !std::io::stdout().is_terminal()) {
                let prs: Vec<crate::prscore::Predicted> = serde_json::from_value(v["prs"].clone()).unwrap_or_default();
                println!("{}", crate::prscore::said(&prs));
                return Ok(());
            }
            let mut v = v;
            v["took"] = json!(crate::prscore::said(&serde_json::from_value::<Vec<crate::prscore::Predicted>>(v["prs"].clone()).unwrap_or_default()));
            v
        }
        "show" => show(a.id.as_deref())?,
        _ => request(&a)?,
    };
    if a.json || !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if a.action == "list" {
        for p in v["plans"].as_array().into_iter().flatten() {
            println!("{}", said(p));
        }
    } else {
        println!("{}", said(&v));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Value {
        json!({
            "space": "msg-translation", "goal": "EYZFlow Support – Translate messages", "done_when": "Messages show translated",
            "draft": true, "project": "EYZFlow",
            "sessions": [
                { "name": "msg-translation-eyz-customer-support", "repo": "/w/eyz/eyz-customer-support", "account": "default", "why": "the inbox service" },
                { "name": "msg-translation-eyzflow-admin-dashboard", "repo": "/w/eyz/eyzflow-admin-dashboard", "account": "default", "why": "the agents' UI" },
            ],
            "assumed": { "setup": "EYZFlow", "account": "default" },
        })
    }

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_plan_is_added_said_revised_published_and_closed_one_at_a_time() {
        let mut b = Book::default();
        let dir = |id: &str| PathBuf::from(format!("/s/plans/{id}"));
        let (p, ended) = b.add(Form::Page, "  The ticket \n", draft(), dir, 100);
        assert_eq!((p.id.as_str(), p.status, p.ticket.as_str(), p.dir.as_str(), p.planner.as_str()), ("p1", Status::Writing, "The ticket", "/s/plans/p1", "starting"));
        assert!(ended.is_empty());
        assert_eq!(p.plan_md(), PathBuf::from("/s/plans/p1/plan.md"));
        b.started("p1", "t9", 110).unwrap();
        assert_eq!(b.current().unwrap().address.as_deref(), Some("t9"));
        // The chat: the user's line waits for an answer until the planner's comes.
        b.say("p1", Who::User, "  which session starts first? ", 120).unwrap();
        assert!(b.current().unwrap().answering());
        b.say("p1", Who::Planner, "The inbox service: the UI reads its API.", 130).unwrap();
        let p = b.current().unwrap();
        assert!(!p.answering());
        assert_eq!(p.thread.iter().map(|m| (m.from.as_str(), m.text.as_str(), m.at)).collect::<Vec<_>>(), [("user", "which session starts first?", 120), ("planner", "The inbox service: the UI reads its API.", 130)]);
        assert!(b.say("p1", Who::User, "  ", 140).is_err(), "nothing to say");
        // A revised draft; a draft with no session is refused.
        let mut d = draft();
        d["sessions"].as_array_mut().unwrap().truncate(1);
        assert_eq!(b.set_draft("p1", d.clone(), 150).unwrap().revision, 1);
        assert!(b.set_draft("p1", json!({ "space": "x", "sessions": [] }), 151).is_err());
        assert_eq!(b.current().unwrap().draft, d);
        // Published: its link, and ready; a link that isn't one is refused.
        assert!(b.published("p1", "not a link", 160).is_err());
        let p = b.published("p1", "https://claude.ai/artifact/abc", 160).unwrap();
        assert_eq!((p.status, p.url.as_deref(), p.ready_ms), (Status::Ready, Some("https://claude.ai/artifact/abc"), Some(160)));
        // Republished: the same plan, the first moment it was ready kept.
        assert_eq!(b.published("p1", "https://claude.ai/artifact/abc", 170).unwrap().ready_ms, Some(160));
        // A second plan closes the first: it comes back, for its planner to be ended.
        let (p2, ended) = b.add(Form::Whiteboard, "The ticket", draft(), dir, 200);
        assert_eq!((p2.id.as_str(), ended.len(), ended[0].id.as_str(), ended[0].address.as_deref()), ("p2", 1, "p1", Some("t9")));
        assert_eq!(b.get("p1").unwrap().status, Status::Closed);
        assert_eq!(b.current().unwrap().id, "p2");
        assert!(b.say("p1", Who::User, "hello?", 210).unwrap_err().to_string().contains("closed"));
        // Failed: why, said; closed with the space it launched.
        let p = b.failed("p2", "no Artifact tool", 220).unwrap();
        assert_eq!((p.status, p.why.as_str(), p.answering()), (Status::Failed, "no Artifact tool", false));
        let p = b.close("p2", Some("msg-translation"), 230).unwrap();
        assert_eq!((p.status, p.closed_ms, p.launched.as_deref()), (Status::Closed, Some(230), Some("msg-translation")));
        assert!(b.current().is_none());
        assert!(b.close("p2", None, 240).is_ok(), "closing a closed plan is no error");
        assert!(b.close("p9", None, 240).is_err());
        assert_eq!(b.pick(None).unwrap_err().to_string(), "no plan is open");
        assert_eq!(b.pick(Some("p1")).unwrap().id, "p1");
    }

    #[test]
    fn the_planners_terminal_is_followed() {
        let mut b = Book::default();
        let (p, _) = b.add(Form::Page, "t", draft(), |id| PathBuf::from(id), 1);
        b.started(&p.id, "t9", 2).unwrap();
        assert!(b.seen(&p.id, "working", 3));
        assert!(!b.seen(&p.id, "working", 4), "no change");
        assert!(b.seen(&p.id, "blocked", 5));
        assert_eq!(b.current().unwrap().view()["blocked"], true);
        // Gone before it published: failed, and why.
        assert!(b.seen(&p.id, "gone", 6));
        let p = b.current().unwrap().clone();
        assert_eq!((p.status, p.address.as_deref()), (Status::Failed, None));
        assert!(p.why.contains("ended before it published"), "{}", p.why);
        // Gone after: the plan stays ready.
        let (q, _) = b.add(Form::Page, "t", draft(), |id| PathBuf::from(id), 7);
        b.started(&q.id, "t10", 8).unwrap();
        b.published(&q.id, "https://claude.ai/artifact/x", 9).unwrap();
        b.seen(&q.id, "gone", 10);
        let q = b.current().unwrap();
        assert_eq!(q.status, Status::Ready);
        assert!(q.why.contains("Plan again"), "{}", q.why);
    }

    #[test]
    fn the_store_is_kept_as_json_and_read_back() {
        let mut b = Book::default();
        let (p, _) = b.add(Form::Whiteboard, "The ticket", draft(), |id| PathBuf::from(format!("/s/{id}")), 1);
        b.started(&p.id, "t9", 2).unwrap();
        b.say(&p.id, Who::User, "which first?", 3).unwrap();
        b.published(&p.id, "https://claude.ai/artifact/wb", 4).unwrap();
        // As the daemon writes plans.json, and reads it after a restart.
        let text = serde_json::to_string_pretty(&b).unwrap();
        let back: Book = serde_json::from_str(&text).unwrap();
        assert_eq!(back, b);
        assert!(text.contains("\"form\": \"whiteboard\"") && text.contains("\"status\": \"ready\""), "{text}");
        // A store from before a field was added reads, the rest defaulted.
        let old: Book = serde_json::from_str(r#"{"plans":[{"id":"p1","form":"page","status":"writing","ticket":"x"}],"next":1}"#).unwrap();
        assert_eq!((old.plans[0].id.as_str(), old.plans[0].thread.len(), old.next), ("p1", 0, 1));
        // The next id goes on from the last.
        let mut back = back;
        assert_eq!(back.add(Form::Page, "t", draft(), |id| PathBuf::from(id), 5).0.id, "p2");
        // The view the panel reads.
        let v = back.get("p1").unwrap().view();
        assert_eq!((v["form"].as_str(), v["status"].as_str(), v["url"].as_str(), v["plan_md"].as_str()), (Some("whiteboard"), Some("closed"), Some("https://claude.ai/artifact/wb"), Some("/s/p1/plan.md")));
        assert_eq!(v["thread"][0]["from"], "user");
    }

    #[test]
    fn a_plan_with_a_migration_pr_and_a_ui_pr_scores_both_and_the_real_prs_carry_them() {
        let mut b = Book::default();
        let (p, _) = b.add(Form::Page, "Translate support messages", draft(), |id| PathBuf::from(format!("/s/{id}")), 1);
        let prs = crate::prscore::check(&crate::prscore::tests::two_prs(), &repos_of(&p.draft)).unwrap();
        let p = b.set_prs(&p.id, prs, 2).unwrap();
        // Both predictions, each with its score and why, as the state shows them.
        let v = p.view();
        assert_eq!(v["prs"].as_array().unwrap().len(), 2);
        assert_eq!((v["prs"][0]["repo"].as_str(), v["prs"][0]["base"].as_str()), (Some("eyz-customer-support"), Some("dev")));
        assert_eq!((v["prs"][0]["score"]["needs_review"].as_bool(), v["prs"][0]["score"]["flags"][0].as_str()), (Some(true), Some("migration")));
        assert_eq!(v["prs"][0]["score"]["line"], "Needs review: a database migration (db/migrations/0042_message_translations.sql)");
        assert_eq!((v["prs"][1]["repo"].as_str(), v["prs"][1]["score"]["needs_review"].as_bool()), (Some("eyzflow-admin-dashboard"), Some(false)));
        assert_eq!(v["prs"][1]["score"]["line"], crate::prscore::LOW_LINE);
        let text = said(&v);
        assert!(text.contains("Pull requests:\n    PR 1 · eyz-customer-support → dev: Store each message's translation\n      Needs review: a database migration"), "{text}");
        assert!(text.contains("PR 2 · eyzflow-admin-dashboard → dev: Show translated messages in the inbox\n      Low risk"), "{text}");
        // Launched as its space: a PR a session opens there is matched to its prediction, once.
        b.close(&p.id, Some("msg-translation"), 3).unwrap();
        let (id, ui) = b.match_pr("Msg-Translation", "eyzhub/eyzflow-admin-dashboard", "dev", "feat: translated messages in the inbox", "eyzhub/eyzflow-admin-dashboard#7", 4).unwrap();
        assert_eq!((id.as_str(), ui.n, ui.score.needs_review), ("p1", 2, false));
        assert_eq!(b.get("p1").unwrap().prs[1].pr.as_deref(), Some("eyzhub/eyzflow-admin-dashboard#7"));
        let again = b.match_pr("msg-translation", "eyzhub/eyzflow-admin-dashboard", "dev", "anything", "eyzhub/eyzflow-admin-dashboard#7", 5).unwrap();
        assert_eq!(again.1.n, 2, "the same PR found again: the same prediction");
        let (_, migration) = b.match_pr("msg-translation", "eyzhub/eyz-customer-support", "dev", "Store translations", "eyzhub/eyz-customer-support#3", 6).unwrap();
        assert!(migration.score.needs_review);
        assert!(b.match_pr("msg-translation", "eyzhub/eyz-customer-support", "dev", "one more", "eyzhub/eyz-customer-support#4", 7).is_none(), "every prediction of that repo is taken");
        assert!(b.match_pr("another-space", "eyzhub/eyzflow-admin-dashboard", "dev", "x", "eyzhub/eyzflow-admin-dashboard#9", 8).is_none());
        assert!(b.match_pr("", "eyzhub/eyzflow-admin-dashboard", "dev", "x", "eyzhub/eyzflow-admin-dashboard#9", 8).is_none(), "no space: no plan");
        // A plan revised while open keeps the PRs that opened with their predictions.
        let (q, _) = b.add(Form::Page, "t", draft(), |id| PathBuf::from(id), 9);
        let prs = crate::prscore::check(&crate::prscore::tests::two_prs(), &repos_of(&q.draft)).unwrap();
        b.set_prs(&q.id, prs.clone(), 10).unwrap();
        b.plans.iter_mut().find(|x| x.id == q.id).unwrap().prs[0].pr = Some("o/r#1".into());
        let q = b.set_prs(&q.id, prs, 11).unwrap();
        assert_eq!(q.prs[0].pr.as_deref(), Some("o/r#1"));
        // The CLI reads it.
        let a = parse_args(&words(&["prs", "p3", "--json-file", "prs.json"])).unwrap();
        assert_eq!((a.action.as_str(), a.id.as_deref(), a.json_file.as_deref()), ("prs", Some("p3"), Some("prs.json")));
        assert!(parse_args(&words(&["prs", "p3"])).is_err(), "prs needs its file");
    }

    #[test]
    fn a_launched_session_is_told_its_prs_checks_and_how_to_report_them() {
        let prs = crate::prscore::check(&crate::prscore::tests::two_prs(), &repos_of(&draft())).unwrap();
        let b = PlanBrief { id: "p1".into(), url: Some("https://claude.ai/artifact/x".into()), md: "# Plan\n\nShort.".into(), prs, stages: vec![] };
        let text = brief_text(&b, "eyzflow-admin-dashboard", &["eyz-customer-support".into()]);
        assert!(text.contains("tower validate <the PR's link> <check> passed|failed|not-run --evidence"), "{text}");
        assert!(text.contains("what isn't reported then counts as not run"), "{text}");
        assert!(text.contains("PR 2 into dev: Show translated messages in the inbox\n- c1: Each message shows its translation"), "{text}");
        assert!(text.contains("- edge case: A very long message wraps, nothing is cut (covered by c2)"), "{text}");
        assert!(!text.contains("Store each message's translation"), "only its own PRs: {text}");
        // A session whose repo has no predicted PR is told nothing of checks.
        assert!(!brief_text(&b, "other-repo", &[]).contains("tower validate"));
    }

    #[test]
    fn a_handoff_block_is_read_from_the_end_of_a_turn() {
        let said = "I added the endpoint and its tests.\n\n**Handoff a12.1 · plan p1 · stage s2**\nStatus: partial\nDone: POST /messages/:id/translate, with a cache\nEvidence: npm test -- translate.spec.ts: 6 passed\nLeft: the backfill\nUncertain: whether the provider's rate limit holds at 50 rps\nFindings:\n- split-pr: the backfill is a PR of its own\n- the dashboard needs the language list from the API\nNeeds: nothing\nNext: open the backfill PR";
        let h = parse_handoff(said).unwrap();
        assert_eq!((h.task.as_str(), h.plan.as_str(), h.stage.as_str(), h.status.as_str()), ("a12.1", "p1", "s2", "partial"));
        assert_eq!(h.evidence, "npm test -- translate.spec.ts: 6 passed");
        assert_eq!(h.uncertain, "whether the provider's rate limit holds at 50 rps");
        assert_eq!(h.findings, [("split-pr".to_string(), "the backfill is a PR of its own".to_string()), ("other".into(), "the dashboard needs the language list from the API".into())]);
        assert!(h.needs.is_empty(), "nothing is no need");
        // One finding on its own line; none; no block, or one with no plan.
        let one = parse_handoff("Handoff - · plan p2 · stage s1\nStatus: done\nFindings: risk: the API is slow\nUncertain: none").unwrap();
        assert_eq!((one.findings.len(), one.uncertain.as_str()), (1, ""));
        assert!(parse_handoff("All done, tests pass.").is_none());
        assert!(parse_handoff("Handoff a1\nStatus: done").is_none());
    }

    #[test]
    fn a_task_from_a_plan_goes_out_as_the_structured_handoff() {
        let prs = crate::prscore::check(&crate::prscore::tests::two_prs(), &repos_of(&draft())).unwrap();
        let v = json!({ "stages": [
            { "id": "s1", "title": "Spike", "kind": "spike", "done_when": "We know the API.", "post": [{ "id": "c1", "what": "w1", "how": "h1", "evidence": "e1" }] },
            { "id": "s2", "title": "Build", "goal": "The inbox translates", "after": ["s1"], "prs": [1], "done_when": "It works on dev", "post": [{ "id": "c1", "what": "w2", "how": "h2", "evidence": "e2" }] } ] });
        let mut b = Book::default();
        let (p, _) = b.add(Form::Page, "T", draft(), |id| PathBuf::from(format!("/s/{id}")), 1);
        b.plans[0].prs = prs;
        b.set_structure(&p.id, &v, 2).unwrap();
        let p = b.plans[0].clone();
        let h = TaskHandoff { task: "b", goal: "", so_far: "s1 found the API answers in 300 ms", constraints: "PRs into dev only", uncertain: "", next: "Add the endpoint" };
        let text = handoff_text(&p, "s2", &h).unwrap();
        assert!(text.starts_with("[tower] Plan p1 · stage 2 of 2 (s2, build): Build · task b\nGoal: The inbox translates\nDone when: It works on dev\nSo far: s1 found the API answers in 300 ms\nConstraints: PRs into dev only\nNext: Add the endpoint\n"), "{text}");
        assert!(!text.split("\n\n").next().unwrap().contains("Uncertain:"), "an empty field is left out");
        assert!(text.contains("`tower plan check p1 s2 <check> passed|failed|not-run") && text.contains("- post c1: w2"), "{text}");
        assert!(text.contains("Handoff b · plan p1 · stage s2\nStatus: done | partial | blocked | failed"), "{text}");
        assert!(handoff_text(&p, "s9", &h).unwrap_err().to_string().contains("no stage \"s9\""));
    }

    #[test]
    fn a_change_to_the_plan_is_sized_small_or_big() {
        let base = json!({ "stages": [
            { "id": "s1", "title": "Spike", "kind": "spike", "done_when": "Known", "post": [{ "id": "c1", "what": "w", "how": "h", "evidence": "e" }] },
            { "id": "s2", "title": "Build", "after": ["s1"], "done_when": "Works", "post": [{ "id": "c1", "what": "w", "how": "h", "evidence": "e" }] } ],
            "assumptions": [{ "id": "a1", "text": "t", "check": "c" }] });
        let (st, asm, q) = structure(&base, &[]).unwrap();
        let size = |v: &Value| {
            let (s2, a2, q2) = structure(v, &[]).unwrap();
            diff_structure(&st, &asm, &s2, &a2, &q2, &q)
        };
        assert_eq!(size(&base), (vec![], false), "no change");
        let mut small = base.clone();
        small["stages"][1]["post"].as_array_mut().unwrap().push(json!({ "id": "c2", "what": "w2", "how": "h2", "evidence": "e2" }));
        small["stages"][1]["post"][0]["how"] = json!("h, better");
        small["assumptions"].as_array_mut().unwrap().push(json!({ "id": "a2", "text": "t2", "check": "c2" }));
        small["questions"] = json!([{ "id": "q1", "text": "Which?" }]);
        let (changes, big) = size(&small);
        assert!(!big, "{changes:?}");
        assert_eq!(changes, ["Stage s2 post check c1 now: w (how: h, better)", "Stage s2 gets a post check c2: w2", "Assumption a2 added: t2", "Question q1 added: Which?"]);
        for (what, f) in [
            ("a stage added", (|v: &mut Value| v["stages"].as_array_mut().unwrap().push(json!({ "id": "s3", "title": "Release", "done_when": "Live", "post": [{ "what": "w", "how": "h", "evidence": "e" }] }))) as fn(&mut Value)),
            ("a stage removed", |v: &mut Value| { v["stages"].as_array_mut().unwrap().pop(); }),
            ("a done-when changed", |v: &mut Value| v["stages"][0]["done_when"] = json!("Known and priced")),
            ("a check dropped", |v: &mut Value| v["stages"][0]["post"][0]["id"] = json!("c9")),
            ("an assumption dropped", |v: &mut Value| v["assumptions"] = json!([])),
        ] {
            let mut v = base.clone();
            f(&mut v);
            assert!(size(&v).1, "{what} is big");
        }
    }

    #[test]
    fn a_launched_session_is_told_the_stages_its_prs_belong_to_and_how_to_report_a_stage_check() {
        let prs = crate::prscore::check(&crate::prscore::tests::two_prs(), &repos_of(&draft())).unwrap();
        let v = json!({ "stages": [
            { "id": "s1", "title": "Spike", "kind": "spike", "done_when": "We know the API.", "post": [{ "id": "c1", "what": "w1", "how": "h1", "evidence": "e1" }] },
            { "id": "s2", "title": "Build", "after": ["s1"], "prs": [1, 2], "done_when": "It works on dev", "pre": [{ "id": "p1", "what": "access", "how": "a call", "evidence": "its reply" }], "post": [{ "id": "c1", "what": "w2", "how": "h2", "evidence": "e2" }] } ] });
        let (stages, _, _) = structure(&v, &prs).unwrap();
        let b = PlanBrief { id: "p4".into(), prs, stages, ..Default::default() };
        let text = brief_text(&b, "eyzflow-admin-dashboard", &[]);
        assert!(text.contains("A stage is done only when every check of it passed with evidence"), "{text}");
        assert!(text.contains("`tower plan check p4 <stage> <check> passed|failed|not-run --evidence"), "{text}");
        assert!(text.contains("s1 (spike): Spike. Done when: We know the API\n- post c1: w1 (how: h1; evidence: e1)"), "{text}");
        assert!(text.contains("s2 (build): Build. Done when: It works on dev. Yours: PR 2\n- pre p1: access"), "{text}");
        assert!(!brief_text(&PlanBrief::default(), "x", &[]).contains("stages"), "no stages: nothing said");
    }

    #[test]
    fn the_planner_is_told_to_predict_the_prs_dev_first_and_flag_what_needs_review() {
        let mut b = Book::default();
        let (p, _) = b.add(Form::Page, "The ticket", draft(), |id| PathBuf::from(format!("/s/{id}")), 1);
        let text = instructions(&p, "/bin/tower");
        for part in [
            "## 2. The pull requests",
            "one entry per PR",
            "/bin/tower plan prs p1 --json-file prs.json",
            "everything goes into dev first",
            "`migration`: true",
            "`breaking`: what it breaks",
            "`escalate`: anything else on the tower's escalate list",
            "**Needs review**",
            "**Low risk**",
            "## Pull requests",
            "its score and the reason exactly as the tower printed them",
            "## 4. Publish it",
            "## 5. Then: the chat",
            "**the pull requests**: change `prs.json`",
            "How it will be validated, for sure",
            "the exact tests to add or run",
            "the live checks (run it, open it, check the deployed state on dev)",
            "#### Validation",
            "#### Edge cases",
            "`checks` (at least one)",
            "`edge_cases` (at least one)",
            "each with the `checks` that cover it",
            "It refuses a PR without checks",
            "then its validation (each check: what, how, the evidence) and its edge cases",
            "never merged without the user, whatever its score",
            "whether it breaks other people's functionality",
            "the dashboard calling a serverless API, the SDK, the mobile app",
            "Search the user's repos under ~/Work",
            "`breaks: true` when the change would break it",
            "#### Who depends on it",
            "or that doesn't say who depends on it",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(first_task(&p, "/bin/tower").contains("/bin/tower plan prs p1 --json-file prs.json"));
    }

    #[test]
    fn only_the_latest_closed_plans_are_kept() {
        let mut b = Book::default();
        for i in 0..30 {
            b.add(Form::Page, "t", draft(), |id| PathBuf::from(id), i);
        }
        assert_eq!(b.plans.len(), KEEP_CLOSED + 1);
        assert_eq!(b.current().unwrap().id, "p30");
        assert_eq!(b.plans[0].id, "p10");
    }

    #[test]
    fn the_cli_reads_each_action_and_its_flags() {
        let a = parse_args(&words(&["new", "--form", "whiteboard", "--draft", "{}", "--ticket-file", "/tmp/t.md", "--json"])).unwrap();
        assert_eq!((a.action.as_str(), a.form, a.draft.as_deref(), a.ticket_file.as_deref(), a.json), ("new", Some(Form::Whiteboard), Some("{}"), Some("/tmp/t.md"), true));
        assert!(parse_args(&words(&["new", "--form", "page"])).is_err(), "new needs its draft and ticket");
        assert!(parse_args(&words(&["new", "--form", "slides", "--draft", "{}", "--ticket-file", "f"])).unwrap_err().to_string().contains("page or whiteboard"));
        // say: the id, then the words (one argument or several), and whose they are.
        let a = parse_args(&words(&["say", "p3", "which session starts first?"])).unwrap();
        assert_eq!((a.id.as_deref(), a.words.as_str(), a.who), (Some("p3"), "which session starts first?", None));
        let a = parse_args(&words(&["say", "--as", "user", "p3", "leave", "the", "dashboard", "out"])).unwrap();
        assert_eq!((a.words.as_str(), a.who), ("leave the dashboard out", Some(Who::User)));
        let a = parse_args(&words(&["say", "p3", "--", "--json is a word here"])).unwrap();
        assert_eq!(a.words, "--json is a word here");
        assert!(parse_args(&words(&["say", "p3"])).is_err() && parse_args(&words(&["say", "hello"])).is_err(), "say needs the id and words");
        // The planner's own reports.
        let a = parse_args(&words(&["published", "p3", "--url=https://claude.ai/artifact/x"])).unwrap();
        assert_eq!((a.action.as_str(), a.url.as_deref()), ("published", Some("https://claude.ai/artifact/x")));
        assert!(parse_args(&words(&["published", "p3"])).is_err());
        let a = parse_args(&words(&["draft", "p3", "--json-file", "draft.json"])).unwrap();
        assert_eq!(a.json_file.as_deref(), Some("draft.json"));
        assert_eq!(parse_args(&words(&["failed", "p3", "no", "Artifact", "tool"])).unwrap().words, "no Artifact tool");
        // show and close take the open plan when no id is given.
        assert_eq!(parse_args(&[]).unwrap().action, "show");
        assert_eq!(parse_args(&words(&["close"])).unwrap().id, None);
        let a = parse_args(&words(&["close", "p2", "--launched", "msg-translation"])).unwrap();
        assert_eq!((a.id.as_deref(), a.launched.as_deref()), (Some("p2"), Some("msg-translation")));
        assert!(parse_args(&words(&["show", "p2", "extra"])).is_err());
        assert!(parse_args(&words(&["launch", "p2"])).unwrap_err().to_string().contains("tower plan launch"));
        assert!(parse_args(&words(&["say", "p2", "hi", "--bogus"])).unwrap_err().to_string().contains("no flag --bogus"));
        // Whose words: the planner's from its own terminal, else the user's; --as says it outright.
        assert_eq!(who_says(None, Some("p3"), "p3"), Who::Planner);
        assert_eq!(who_says(None, Some("p2"), "p3"), Who::User);
        assert_eq!(who_says(None, None, "p3"), Who::User);
        assert_eq!(who_says(Some(Who::User), Some("p3"), "p3"), Who::User);
        assert!(is_id("p12") && !is_id("p") && !is_id("plan") && !is_id("a3"));
    }

    #[test]
    fn the_planners_instructions_say_how_to_publish_each_form() {
        let mut b = Book::default();
        let (page, _) = b.add(Form::Page, "The ticket", draft(), |id| PathBuf::from(format!("/s/{id}")), 1);
        let text = instructions(&page, "/bin/tower");
        assert!(text.contains("HTML page") && text.contains("Plan · msg-translation") && !text.contains(WHITEBOARD_TYPE), "{text}");
        assert!(text.contains("`eyz-customer-support` (/w/eyz/eyz-customer-support): the inbox service"), "{text}");
        assert!(text.contains("/bin/tower plan published p1 --url"), "{text}");
        assert!(text.contains("/bin/tower plan say p1"), "{text}");
        assert!(text.contains("/bin/tower plan draft p1 --json-file draft.json"), "{text}");
        assert!(text.contains("/bin/tower plan failed p1"), "{text}");
        assert!(text.contains("the same artifact") && text.contains("Never launch the space") && text.contains("Never edit, commit or push"), "{text}");
        for part in ["what it does, and in what order", "APIs, the data and the contracts", "risks", "open questions", "How it will be validated, for sure", "### <its repository's folder name>"] {
            assert!(text.contains(part), "{part}: {text}");
        }
        // Stages with what counts as done and their checks; assumptions; questions that go to the tower, never left on the page.
        for part in [
            "## 3. The stages, the assumptions and the questions",
            "/bin/tower plan stages p1 --json-file stages.json",
            "Spikes first",
            "\"pre\": [{",
            "\"post\": [{",
            "`theirs: true` when only the user can decide it",
            "Secrets, deploys, deletes, production data and money always go to the user",
            "never left on the page",
            "\"asked in the tower\"",
            "[plan p1] q2 is answered by",
            "the stages first (stage N of M",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(first_task(&page, "/bin/tower").contains("/bin/tower plan stages p1 --json-file stages.json"));
        let (board, _) = b.add(Form::Whiteboard, "The ticket", draft(), |id| PathBuf::from(format!("/s/{id}")), 2);
        let text = instructions(&board, "/bin/tower");
        assert!(text.contains("**Whiteboard**") && text.contains(WHITEBOARD_TYPE) && text.contains("a block per session") && text.contains("arrows") && text.contains("a section for the assumptions, the risks and the questions") && text.contains("a lane of stages"), "{text}");
        assert!(first_task(&board, "/bin/tower").contains(WHITEBOARD_TYPE));
        let first = first_task(&page, "/bin/tower");
        assert!(first.starts_with("[plan p1] Read instructions.md") && first.contains("/bin/tower plan published p1 --url"), "{first}");
        // A path with a space is quoted for its shell.
        assert!(instructions(&page, "/Applications/My Tower/tower").contains("'/Applications/My Tower/tower' plan say p1"));
        // What the user says, as the planner gets it.
        let said = chat_prompt(&page, " which session starts first? ", "/bin/tower");
        assert!(said.starts_with("[plan p1] The user says: which session starts first?\n\n") && said.contains("/bin/tower plan say p1"), "{said}");
    }

    #[test]
    fn the_planner_may_read_anywhere_write_in_its_folder_and_never_launch() {
        let s = settings("/bin/tower", &["/w/eyz/eyzflow".into()]);
        let allow: Vec<&str> = s["permissions"]["allow"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        let deny: Vec<&str> = s["permissions"]["deny"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        for a in ["Read", "Edit(./**)", "Artifact", "Bash(/bin/tower plan *)", "Bash(tower plan *)", "Bash(git log *)"] {
            assert!(allow.contains(&a), "{a}: {allow:?}");
        }
        for d in ["Bash(/bin/tower launch*)", "Bash(tower start*)", "Bash(tower queue*)", "Bash(hc *)", "Bash(git push *)", "Edit(//w/eyz/eyzflow/**)", "AskUserQuestion"] {
            assert!(deny.contains(&d), "{d}: {deny:?}");
        }
        assert_eq!(s["permissions"]["additionalDirectories"], json!(["/w/eyz/eyzflow"]));
        // Its command line: the plan's id and the tower's own folders in its environment, the default account, the brains' model.
        let mut b = Book::default();
        let (p, _) = b.add(Form::Page, "t", draft(), |id| PathBuf::from(format!("/s/{id}")), 1);
        let mut cfg = crate::config::Config::default();
        cfg.brains.model = "claude-opus-5-5".into();
        cfg.brains.effort = "high".into();
        let env = vec![("HERDER_TOWER_STATE".to_string(), "/sb/state".to_string())];
        let a = argv(&p, &cfg, Some("/bin/cca"), Some("default"), "/bin/tower", &env).unwrap();
        let at = a.iter().position(|w| w == "/bin/cca").unwrap();
        assert_eq!(&a[at..at + 8], ["/bin/cca", "default", "-n", PLANNER, "--model", "claude-opus-5-5", "--effort", "high"]);
        assert!(a[..at].contains(&"HERDER_TOWER_PLAN=p1".to_string()) && a[..at].contains(&"HERDER_TOWER_STATE=/sb/state".to_string()), "{a:?}");
        assert!(!a.contains(&"CLAUDE_CONFIG_DIR".to_string()), "cca picks the account's folder itself");
        assert!(a.last().unwrap().starts_with("[plan p1] Read instructions.md"));
        // No accounts mod: plain claude, on the default account's folder.
        let a = argv(&p, &cfg, None, None, "/bin/tower", &[]).unwrap();
        assert!(a.contains(&"CLAUDE_CONFIG_DIR".to_string()) && a.contains(&"claude".to_string()), "{a:?}");
    }

    #[test]
    fn a_launched_session_gets_its_part_of_the_plan() {
        let md = "# Plan · msg-translation\n\nOverview line.\n\n## Sessions\n\n### eyz-customer-support\n\nBuild the translate endpoint.\n\n### eyzflow-admin-dashboard\n\nShow translated messages.\n\n## Risks\n\nRate limits.\n";
        // It fits: the whole plan.
        let (all, cut) = part_for(md, "eyz-customer-support", &["eyzflow-admin-dashboard".into()], 6_000);
        assert_eq!((all.as_str(), cut), (md.trim(), false));
        // Too long: the other session's own section goes, the rest stays.
        let (mine, _) = part_for(md, "eyz-customer-support", &["eyzflow-admin-dashboard".into()], 150);
        assert!(mine.contains("Build the translate endpoint") && !mine.contains("Show translated messages"), "{mine}");
        let long = format!("{md}\n## Tests\n\n{}", "A test line.\n".repeat(50));
        let (theirs, cut) = part_for(&long, "eyzflow-admin-dashboard", &["eyz-customer-support".into()], 300);
        assert!(theirs.contains("Show translated messages") && !theirs.contains("Build the translate endpoint") && cut, "{theirs}");
        assert!(theirs.chars().count() <= 300);
        // A heading that names both (the seam between them) is everyone's.
        let both = format!("{}\n### eyz-customer-support and eyzflow-admin-dashboard\n\nThe contract.\n", "x\n".repeat(200));
        assert!(part_for(&both, "eyz-customer-support", &["eyzflow-admin-dashboard".into()], 1_000).0.contains("The contract."));
        // In the instruction: the link, and what is cut is said.
        let b = PlanBrief { id: "p1".into(), url: Some("https://claude.ai/artifact/x".into()), md: long.clone(), ..Default::default() };
        let text = brief_text(&b, "eyz-customer-support", &["eyzflow-admin-dashboard".into()]);
        assert!(text.starts_with("The space's plan (https://claude.ai/artifact/x)") && text.contains("Build the translate endpoint"), "{text}");
        let short = PlanBrief { md: "One line.".into(), ..b.clone() };
        assert!(!brief_text(&short, "x", &[]).contains("goes on"));
        let huge = PlanBrief { md: "word ".repeat(3_000), ..b };
        let text = brief_text(&huge, "x", &[]);
        assert!(text.contains("word word") && text.ends_with("(The plan goes on: all of it is at https://claude.ai/artifact/x.)"), "{text}");
    }
}
