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
    /// Ended: Start over, a launch, another plan.
    Closed,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Writing => "writing",
            Status::Ready => "ready",
            Status::Failed => "failed",
            Status::Closed => "closed",
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
}

impl Plan {
    pub fn is_open(&self) -> bool {
        self.status != Status::Closed
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
        })
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
        self.plans.iter().rev().find(|p| p.is_open())
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
        for p in self.plans.iter_mut().filter(|p| p.is_open()) {
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

    /// Ended (`launched`: the space launched with it). Closing a closed plan is no error.
    pub fn close(&mut self, id: &str, launched: Option<&str>, now: u64) -> Result<Plan> {
        let Some(p) = self.plans.iter_mut().find(|p| p.id == id) else { bail!("no plan {id}") };
        if p.is_open() {
            p.status = Status::Closed;
            p.closed_ms = Some(now);
        }
        if let Some(space) = launched.map(str::trim).filter(|s| !s.is_empty()) {
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
    s += "4. **The open questions** for the user.\n";
    s += "5. **How it will be tested**: in each repository, and end to end.\n\n";
    s += "In `plan.md`, put each session under a `### <its repository's folder name>` heading inside a `## Sessions` section (a launched session is handed its own part), and the rest under `##` headings of their own.\n\n";
    s += "## 2. Publish it\n\n";
    match p.form {
        Form::Page => {
            s += &format!(
                "Publish it with your Artifact tool as an **HTML page** Artifact: one self-contained page titled \"Plan · {space}\", with the sessions in order (one card each), how the repositories fit together, the risks, the open questions and the tests. It is private to the user's account. Publish one artifact only; every change later republishes this same one (its `url`).\n\n"
            );
        }
        Form::Whiteboard => {
            s += &format!(
                "Publish it with your Artifact tool as a **Whiteboard** Artifact, made from the account's Artifact type `{WHITEBOARD_TYPE}` (publish with that `type_url` and the title \"Plan · {space}\", then fill it as the type's instructions say): a block per session (its repository, what it does, its place in the order), arrows for the dependencies between them (who waits for whom, who calls whom), and a section for the risks and the open questions. It is private to the user's account. Make one whiteboard only; every change later updates this same one (its `url`).\n\n"
            );
        }
    }
    s += &format!("Then say so: `{t} plan published {id} --url <the artifact's link>`.\n\n");
    s += &format!("If you can't publish it (no Artifact tool, an error that stays), say why and stop: `{t} plan failed {id} \"<why, in a line>\"`.\n\n");
    s += "## 3. Then: the chat\n\n";
    s += &format!(
        "After that you wait. What the user says about the plan reaches you as a prompt that begins `[plan {id}] The user says:`. Answer it in a few sentences with `{t} plan say {id} \"<your answer>\"`: that is the only way your answer reaches them (nobody reads this terminal). Never ask anything in this terminal; ask with `plan say`.\n\n"
    );
    s += "- A question: answer it from the plan and the repositories.\n";
    s += "- A change: say what you would change, and ask whether to. Once the user agrees (or asked for it outright), make it, then say what changed:\n";
    s += &format!(
        "  - **the draft** (which repositories get a session, the space's name, its goal, what counts as done): write the new draft to `draft.json` (the same shape; keep what you don't change; a repository must be a project folder on this Mac, see `{t} setups`) and run `{t} plan draft {id} --json-file draft.json`. It says what it took.\n"
    );
    s += &format!(
        "  - **the plan**: change `plan.md`, republish the same artifact (your Artifact tool, with its `url`), and run `{t} plan published {id} --url <its link>` again.\n\n"
    );
    s += "## Never\n\n";
    s += "- Never launch the space, open or start a session, queue or send anything to a session, or open or close a space: no `tower launch`, `tower start`, `tower queue`, `tower dispatch`, `tower space`, no `hc`. The user launches it from the tower's window.\n";
    s += "- Never edit, commit or push in a repository. Write only in this folder.\n";
    s += "- Never publish a second artifact: republish this plan's own.\n";
    s
}

/// The planner's first task: write it, publish it, say so.
pub fn first_task(p: &Plan, tower: &str) -> String {
    let t = tower_word(tower);
    let form = match p.form {
        Form::Page => "an HTML page Artifact".to_string(),
        Form::Whiteboard => format!("a Whiteboard Artifact (from the Artifact type {WHITEBOARD_TYPE})"),
    };
    format!(
        "[plan {}] Read instructions.md in this folder, then ticket.md and draft.json. Write the full plan of the draft into plan.md, publish it as {form}, and say so with `{t} plan published {} --url <its link>`. Then wait for the user's messages.",
        p.id, p.id
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

/// What a launch with the plan `id` hands its sessions: its link and its markdown.
pub fn brief_of(id: &str) -> Result<PlanBrief> {
    let v = show(Some(id))?;
    let md = v["plan_md"].as_str().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    let url = v["url"].as_str().map(String::from);
    if md.trim().is_empty() && url.is_none() {
        bail!("--plan {id}: the plan isn't written yet (no plan.md, no link)");
    }
    Ok(PlanBrief { id: id.to_string(), url, md })
}

// ---------------------------------------------------------------------------
// The CLI.

/// `tower plan <action> …` as given.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Args {
    /// new | say | draft | published | failed | show | close | list
    pub action: String,
    pub id: Option<String>,
    /// What `say` and `failed` say.
    pub words: String,
    pub form: Option<Form>,
    /// `new`: the draft's JSON.
    pub draft: Option<String>,
    /// `new`: the file the ticket is in.
    pub ticket_file: Option<String>,
    /// `draft`: the file the revised draft is in.
    pub json_file: Option<String>,
    /// `published`: the artifact's link.
    pub url: Option<String>,
    /// `say`: whose words (none: the planner's from its own terminal, else the user's).
    pub who: Option<Who>,
    /// `close`: the space launched with it.
    pub launched: Option<String>,
    pub json: bool,
}

const USAGE: &str = "tower plan new --form page|whiteboard --draft '<draft JSON>' --ticket-file <file> | say <id> \"<words>\" [--as user|planner] | draft <id> --json-file <file> | published <id> --url <link> | failed <id> \"<why>\" | show [<id>] [--json] | close [<id>] | list";

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
        "published" if a.id.is_none() || a.url.is_none() => bail!("tower plan published <id> --url <the artifact's link>"),
        "say" | "failed" | "draft" | "published" | "show" | "close" | "list" => {}
        other => bail!("tower plan {other}: {USAGE}"),
    }
    if !a.words.is_empty() && !matches!(a.action.as_str(), "say" | "failed") {
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
        text: a.words.clone(),
        url: a.url.clone().unwrap_or_default(),
        who: a.who.map(|w| w.word().to_string()).unwrap_or_default(),
        launched: a.launched.clone(),
    })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_else(|| "the tower said no".into()));
    }
    Ok(r.state.unwrap_or_default())
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
        for part in ["what it does, and in what order", "APIs, the data and the contracts", "risks", "open questions", "How it will be tested", "### <its repository's folder name>"] {
            assert!(text.contains(part), "{part}: {text}");
        }
        let (board, _) = b.add(Form::Whiteboard, "The ticket", draft(), |id| PathBuf::from(format!("/s/{id}")), 2);
        let text = instructions(&board, "/bin/tower");
        assert!(text.contains("**Whiteboard**") && text.contains(WHITEBOARD_TYPE) && text.contains("a block per session") && text.contains("arrows") && text.contains("a section for the risks"), "{text}");
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
        let b = PlanBrief { id: "p1".into(), url: Some("https://claude.ai/artifact/x".into()), md: long.clone() };
        let text = brief_text(&b, "eyz-customer-support", &["eyzflow-admin-dashboard".into()]);
        assert!(text.starts_with("The space's plan (https://claude.ai/artifact/x)") && text.contains("Build the translate endpoint"), "{text}");
        let short = PlanBrief { md: "One line.".into(), ..b.clone() };
        assert!(!brief_text(&short, "x", &[]).contains("goes on"));
        let huge = PlanBrief { md: "word ".repeat(3_000), ..b };
        let text = brief_text(&huge, "x", &[]);
        assert!(text.contains("word word") && text.ends_with("(The plan goes on: all of it is at https://claude.ai/artifact/x.)"), "{text}");
    }
}
