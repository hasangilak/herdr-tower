//! herder-tower: a voice control tower for Herdr.
//!
//! `tower up` starts two Herdr terminals: `tower-voice` (this program's
//! daemon: Whisper ears, Kokoro mouth, a radar on every session) and `tower`
//! (a Claude Code or Codex session that steers the others through `hc`).
//! The daemon then starts a `tower-<space>` beside them for each open space
//! (see `brains`).

// GPUI comes through GPUI Kit; its macros expect the crate to be called `gpui`.
extern crate gpui_kit as gpui;

mod accounts;
mod acts;
mod agenda;
mod answers;
mod activity;
mod behaviors;
mod brain;
mod brains;
mod browsers;
mod chat;
mod choices;
mod config;
mod covered;
mod daemon;
mod decisions;
mod describe;
mod dispatch;
mod failover;
mod goals;
mod draft;
mod fixes;
mod hc;
mod history;
mod hooks;
mod impact;
mod install;
mod introduce;
mod launch;
mod merges;
mod missions;
mod names;
mod orb;
mod peers;
mod phone;
mod plans;
mod protocol;
mod projects;
mod releases;
mod reopen;
mod prs;
mod prscore;
mod questions;
mod reviews;
mod running;
mod screenshots;
mod selftest;
mod setups;
mod space_drafts;
mod spaces;
mod split;
mod tell;
mod text;
mod unsent;
mod usage;
mod ticket;
mod worktrees;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use protocol::Request;

#[derive(Parser)]
#[command(
    name = "tower",
    version,
    about = "A voice control tower for Herdr: talk to every Claude Code and Codex session on every machine."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the voice and the brain (Claude Code by default).
    Up {
        /// claude or codex (default: config.toml's `brain`).
        #[arg(long)]
        brain: Option<String>,
        /// Only the voice; no brain.
        #[arg(long)]
        no_brain: bool,
        /// Show the brain in a tab of the Herdr window.
        #[arg(long)]
        show: bool,
        /// Don't start the orb.
        #[arg(long)]
        no_orb: bool,
    },
    /// Stop the voice and the brains (the main one, and each space's own).
    Down {
        /// Leave the brains running.
        #[arg(long)]
        keep_brain: bool,
    },
    /// Stop and start the voice (after a config change). The brains keep running, and the new
    /// voice finds them again. `--brain`: restart the brains too (each starts a new conversation).
    Restart {
        /// The brains too: the main one, and each space's own.
        #[arg(long)]
        brain: bool,
    },
    /// The model the tower's brains run on (the main one, and each space's own), and their effort:
    /// `tower model` (the setting, what each running brain was started with, whether a restart is
    /// needed), `tower model claude-fable-5-2 --effort high`, `tower model --effort xhigh`. Models:
    /// claude-fable-5-2, claude-opus-5-5 (the default, at high effort), claude-sonnet-5-5,
    /// claude-haiku-5-5, or any name the program takes. Kept in config.toml (`[brains]`); a change
    /// applies when a brain starts. `--restart` restarts the brains now.
    Model {
        /// The model.
        model: Option<String>,
        /// low | medium | high | xhigh | max
        #[arg(long)]
        effort: Option<String>,
        /// Restart the brains now, so they start on it (each starts a new conversation).
        #[arg(long)]
        restart: bool,
        #[arg(long)]
        json: bool,
    },
    /// Send a session an instruction now if it's free, or queue it until it stops working
    /// (delivered in order, kept across restarts). `tower queue list`, `tower queue cancel <id>`.
    Queue {
        /// <session> "<instruction>", or list, or cancel <id>.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Hand out a request with several tasks: each to the session in its project's folder, or a
    /// new one, each watched, `after` for one that waits for another:
    /// `tower dispatch --json '{"tasks":[{"id":"a","project":"~/w/api","instruction":"…"},{"id":"b","project":"~/w/web","instruction":"…","after":["a"]}]}'`
    Dispatch {
        /// The plan (tasks: id, title, project, instruction, after, session, name, space, account, on, isolate).
        #[arg(long)]
        json: String,
        /// Only say where each task would go.
        #[arg(long)]
        dry_run: bool,
    },
    /// The agenda ledger: every request of the user's and the tasks handed out for it, until each
    /// is verified or dropped. `tower agenda` (open ones), `tasks` (what's unfinished), `show <id>`,
    /// `new "<the user's words>"`, `close <id> [--force]`, `abandon <id|task>`, `verify <task>`,
    /// `fail <task> "<why>"`, `requeue <task> [<session>]`, `forget <id>` (takes a finished one out of
    /// the ledger, as if it never was: a test, a mistake).
    Agenda {
        /// list | tasks | show | new | close | abandon | verify | fail | requeue | forget
        #[arg(default_value = "list")]
        action: String,
        /// The agenda or task (`a3`, `a3.2`), then words (why, a note) or, for requeue, a session.
        args: Vec<String>,
        /// list: closed and done ones too.
        #[arg(long)]
        all: bool,
        /// close: abandon what's unfinished and close it anyway.
        #[arg(long)]
        force: bool,
        /// new: a few words for it.
        #[arg(long)]
        title: Option<String>,
        /// new: the spaces it's about.
        #[arg(long)]
        space: Vec<String>,
    },
    /// What a restart of the Mac (or of Herdr) closed, and putting it back: each session resumes its
    /// own conversation where it was (its own terminal when Herdr kept it as a shell, else a new tab
    /// of its space, else the space made again). The tower offers it on a card once Herdr is back.
    /// No action: what is waiting. `go [<space>…]`: reopen them all, or those spaces' (`--dry-run`:
    /// only say how). `forget [<space>…]`: let them go.
    Reopen {
        /// list, go or forget.
        action: Option<String>,
        /// The spaces (their names).
        spaces: Vec<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// A job this session runs in the background beyond its turn (a training run, a long script),
    /// said to the tower so it shows the session as running a job, not done, and tells when it ends:
    /// `start '<what>' [--pid <pid>]` (with a pid it ends by itself when that process does),
    /// `progress ['<how far>']`, `end`; `wait '<job>' --until '<what>' [--for <minutes>]` (no stuck check
    /// until then; `--session <address>` for another session's job). Background shells are seen without it; this is for jobs the
    /// tower can't see (another machine, a process started some other way). `look <session>`: what
    /// the tower sees running under a session now (it reads only).
    JobRunning {
        /// start, progress, end, wait or look.
        action: String,
        /// What the job is (start), how far it is (progress), which job (wait: its task id, pid or words), a session (look).
        what: Option<String>,
        /// Its process.
        #[arg(long)]
        pid: Option<u32>,
        /// wait: what the job waits for (no stuck check meanwhile).
        #[arg(long)]
        until: Option<String>,
        /// wait: for at most this many minutes (default 360).
        #[arg(long = "for")]
        minutes: Option<u64>,
        /// The session the job is in (else this terminal's): for a tower speaking of another session's job.
        #[arg(long)]
        session: Option<String>,
    },
    /// A Claude account at its usage limit, and the work moved off it: each session or tower that hits
    /// its account's limit is resumed on the next account with usage (the default first), in its own
    /// terminal, with its conversation and what it was doing. No action: what is out, what moved, what
    /// waits. `move <session> [--to <account>]`: move one now. `clear`: forget what is out and what waits.
    Failover {
        /// status, move or clear.
        action: Option<String>,
        /// The session to move (its address or name).
        target: Option<String>,
        /// Where to move it (else the next account with usage).
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Introduce the Claude sessions of a Herdr space to each other (the tower does it by itself when
    /// one joins a space with others; `[sessions] introduce = false` turns that off). No space: each
    /// space's Claude sessions and when they were introduced. `<space> --dry-run`: the messages it
    /// would send. `<space>`: send them again now (queued: each session gets it once it's free).
    Introduce {
        /// The space (its name or a part of it).
        space: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// What every agent session's hooks told the tower: its terminal, session id, prompt, last answer.
    Sessions {
        /// One terminal.
        terminal: Option<String>,
    },
    /// The agent hooks that let the tower see every Claude Code and Codex session: install, remove or status.
    Hooks {
        #[arg(default_value = "status")]
        action: String,
    },
    /// Mute the tower: nothing is said on the Mac or the phones (text and chat go on). Kept across restarts.
    Mute {
        /// on, off, toggle or status (default).
        #[arg(default_value = "status")]
        action: String,
    },
    /// Play everything the tower says on this Mac, never on a phone (`on`), or
    /// let paired phones play it as they route it (`off`, the default). Kept across restarts.
    MacVoice {
        /// on, off, toggle or status (default).
        #[arg(default_value = "status")]
        action: String,
    },
    /// Whether the tower merges low-risk PRs into dev itself (`[prs] auto_merge_low_risk`, off
    /// unless you turn it on): a PR a plan predicted, scored low risk on its own files too, its
    /// checks passing and its reviewer agreeing without finding anything. Anything that needs
    /// review, and every merge into main or prod, stays yours.
    Automerge {
        /// on, off or status (default).
        #[arg(default_value = "status")]
        action: String,
    },
    /// Where each space's changes stand, repo by repo: each PR open, merged into dev, on dev
    /// (deployed) or on prod, with when, and each release to prod with what it carried. From
    /// GitHub, never from what sessions say. `tower releases [<space>]` (JSON when piped).
    Releases {
        /// A space (a part of its name is enough).
        space: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Who outside depends on what a PR changes (callers in other repos, readers of an API field,
    /// …), as the tower found it: `tower impact <pr>`. The reviewer or the PR's session reports
    /// what compiling, type-checking or running the dependents' tests showed: `tower impact <pr>
    /// breaks "<what breaks, and who uses it: repo, file, function>"` or `tower impact <pr> clean
    /// "<what you compiled or ran>"`. A break makes it a breaking change, for the user to review.
    Impact {
        /// The PR: owner/repo#12, its link, or #12.
        pr: String,
        /// breaks or clean.
        verdict: Option<String>,
        /// What breaks and who uses it, or what was compiled or run.
        text: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// About your PR the tower follows: `tower pr cant-fix <pr> "<why>"` when you were asked to
    /// resolve its conflicts with its base (or bring it up to date) and can't. It then goes to
    /// the user, with your words. A PR is its key (owner/repo#12), its link, or #12 (yours).
    Pr {
        /// cant-fix.
        action: String,
        /// The PR: owner/repo#12, its link, or #12.
        pr: String,
        /// Why it can't be resolved.
        why: Vec<String>,
    },
    /// Report one of the planned checks of your PR (its plan lists them: what it shows, how, the
    /// evidence): `tower validate <pr> <check> passed|failed|not-run --evidence "<test output, a
    /// screenshot's path, a log, or why it wasn't run>"`. `tower validate <pr>` shows its checks.
    /// A PR is its key (owner/repo#12), its link, or #12 (yours). Until every planned check
    /// passed, the PR is never merged without the user.
    Validate {
        /// The PR: owner/repo#12, its link, or #12.
        pr: String,
        /// The check's id (c1, c2, …).
        check: Option<String>,
        /// passed, failed or not-run.
        status: Option<String>,
        /// What shows it: the test output, a screenshot's path, a log, or why it wasn't run.
        #[arg(long)]
        evidence: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// What the tower is doing.
    Status,
    /// Every agent on every machine (JSON when piped).
    Agents {
        /// Plain programs and shells too.
        #[arg(long)]
        all: bool,
    },
    /// Start a Claude Code session on a Claude account, with its first task,
    /// and watch it: `tower start --name fix-login --cwd ~/w/app "look at the login bug"`.
    Start {
        /// One of the claude-accounts mod's accounts (`tower accounts`); default: the first it lists.
        #[arg(long)]
        account: Option<String>,
        /// The session's name (a-z 0-9 - _), unique among live terminals.
        #[arg(long)]
        name: String,
        /// Its working folder, or a project's name ("sandbox"): a folder that
        /// doesn't exist is matched to the best project folder, and the answer
        /// says which (default: here; with --on, that machine's home).
        #[arg(long)]
        cwd: Option<String>,
        /// Another machine (`hc machine ls`).
        #[arg(long)]
        on: Option<String>,
        /// The Herdr space to put it in, by its name or a part of it; made when
        /// none has that name (default: a new tab in the space the window shows).
        #[arg(long)]
        space: Option<String>,
        /// The user asked for a new space: --space is its name, made unless a
        /// space has exactly that name (a space it's only part of doesn't count).
        #[arg(long)]
        new_space: bool,
        /// Don't watch it (by default you get a [watch] message when it stops working).
        #[arg(long)]
        no_watch: bool,
        /// The agenda its task belongs to (`a3`; default: the user's latest request's).
        #[arg(long)]
        agenda: Option<String>,
        /// Its task's title in the agenda.
        #[arg(long, hide = true)]
        title: Option<String>,
        /// Its task's id in a dispatch plan.
        #[arg(long, hide = true)]
        plan_id: Option<String>,
        /// Not into the ledger: a queued start stands for it already.
        #[arg(long, hide = true)]
        no_track: bool,
        /// Work in the folder itself, not in the space's worktree of it (a session opened for a
        /// space with a goal that isn't standing gets a git worktree and a dev port of its own
        /// otherwise; a space with no goal never does).
        #[arg(long)]
        no_worktree: bool,
        /// Not kept as an act of its own: `tower launch` keeps one for the whole space.
        #[arg(long, hide = true)]
        no_act: bool,
        /// Its first task.
        task: Vec<String>,
    },
    /// Open a space for a goal, in one go: `tower launch "Fix the login redirect"`. It names the
    /// space from the goal, makes it, opens a session per repo (those the goal's first line names,
    /// else a project's: `tower setups`) on its account, each in its own git worktree with a dev
    /// port, sets the goal (the ticket's title) and hands the whole text out as the first request.
    /// It asks nothing, and prints what it assumed. `--dry-run`: only the plan. `--draft`: the
    /// tower's brain reads the ticket and drafts the space (repos, name, goal, done-when) for you
    /// to confirm; `--repo`, `--title` and `--done-when` launch exactly what was confirmed.
    Launch {
        /// The goal, in a sentence, or a whole ticket.
        #[arg(required = true)]
        goal: Vec<String>,
        /// The project to open it with (default: the only one, else the first). With
        /// `--draft`: the project its repos come from. With `--repo`: the project it is for.
        #[arg(long)]
        setup: Option<String>,
        /// The space's name (default: the goal's first two words that carry meaning, made unique).
        #[arg(long)]
        name: Option<String>,
        /// Print the plan and do nothing.
        #[arg(long)]
        dry_run: bool,
        /// Have the tower's brain read the ticket and draft the space, and print the draft
        /// (as `--dry-run` does its plan). Nothing is opened.
        #[arg(long, conflicts_with_all = ["repo", "title", "done_when", "dry_run"])]
        draft: bool,
        /// With `--draft`: what to change about the draft `--prior` holds, in words.
        #[arg(long, requires = "draft")]
        adjust: Option<String>,
        /// With `--draft`: the draft before (its JSON, as `--draft --json` printed it).
        #[arg(long, requires = "draft")]
        prior: Option<String>,
        /// Open a session in this folder (once per repo), and in no other.
        #[arg(long)]
        repo: Vec<String>,
        /// The space's goal, in a line (default: the ticket's title).
        #[arg(long)]
        title: Option<String>,
        /// What counts as done (default: the ticket's acceptance criteria, if it has any).
        #[arg(long)]
        done_when: Option<String>,
        /// The Claude account its sessions run on (default: the project's, else the default one).
        #[arg(long)]
        account: Option<String>,
        /// The plan it was planned with (`tower plan`, `p3`): each session's first instruction
        /// carries the plan's link and its part of the plan, and the plan is closed.
        #[arg(long, conflicts_with = "draft")]
        plan: Option<String>,
        /// Each PR its sessions open gets a reviewer session, for critical issues only (the default).
        #[arg(long, conflicts_with_all = ["no_review", "draft"])]
        review: bool,
        /// Its PRs get no reviewer.
        #[arg(long, conflicts_with = "draft")]
        no_review: bool,
        /// The conversation that made the space (the full window's New space: the ticket, the
        /// drafts, the changes asked for, the planner's answers), as a JSON file of
        /// `[{"who": "you"|"tower"|"planner", "text": "…", "at": <ms>}]`: once the space is
        /// launched it is kept in the history as the space's own, how it was created.
        #[arg(long, conflicts_with_all = ["draft", "dry_run"])]
        story_file: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// A space's plan before it is launched: the tower's planner (a Claude Code terminal of the
    /// tower's, `tower-planner`) writes the full plan of a draft, publishes it as an Artifact on
    /// your claude.ai account (an HTML page or a whiteboard) and talks it over with you.
    /// `tower plan new --form page|whiteboard --draft '<draft JSON>' --ticket-file <file>`,
    /// `say <id> "<words>" [--as user|planner]` (yours is queued to the planner; the planner's
    /// is its answer), `draft <id> --json-file <file>` (the planner's revised draft),
    /// `published <id> --url <link>`, `failed <id> "<why>"`, `show [<id>] [--json]`,
    /// `close [<id>]` (its planner ends), `list`.
    Plan {
        /// new | say | draft | published | failed | show | close | list, its id and flags.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// The projects (setups) `tower launch` opens a space with: a name, a Claude account, what it
    /// is, and the repos that get a session each with what each is in it (`[[setups]]` in
    /// config.toml). The first is the default.
    Setups {
        #[arg(long)]
        json: bool,
    },
    /// `tower setup add "<name>" --account <account> [--about "<what it is>"] --repo
    /// "<folder>=<its role>" [--repo …]` (the same name again changes it; `--was "<old name>"`
    /// renames one in its place), `tower setup remove "<name>"`.
    Setup {
        /// add | remove
        action: String,
        /// The project's name.
        name: Vec<String>,
        /// add: the Claude account its sessions run on (`tower accounts`; default: the first).
        #[arg(long)]
        account: Option<String>,
        /// add: what the project is, in a line.
        #[arg(long)]
        about: Option<String>,
        /// add: a repo's folder (or a project folder's name), with what it is in the project after
        /// a `=` (or ` — `): `--repo "~/Work/eyz/eyzflow=the web app"`. Once per repo.
        #[arg(long)]
        repo: Vec<String>,
        /// add: the project's name before, to rename it in its place.
        #[arg(long)]
        was: Option<String>,
    },
    /// Every space as a mission: its goal and state (open, quiet, ready to close), what needs you
    /// there, its sessions (with worktree and port) and its requests. JSON when piped.
    Missions {
        #[arg(long)]
        json: bool,
    },
    /// The folder a project's name means ("sandbox project", "mediaroom"): the
    /// best match under ~/Work (and one level below), and what else came close.
    Project {
        /// The name as the user said it.
        name: Vec<String>,
    },
    /// The Claude accounts the claude-accounts mod (`cca`) knows; the first is the default. `--usage`:
    /// each account's 5-hour and 7-day usage (as cca recorded it) and whether the tower found it out of
    /// usage, then every tower and session by space with the account it runs on, and those the failover moved.
    Accounts {
        /// Another machine's accounts.
        #[arg(long)]
        on: Option<String>,
        /// Usage, and who runs on which account.
        #[arg(long)]
        usage: bool,
        /// As JSON (also when not printed to a terminal).
        #[arg(long)]
        json: bool,
    },
    /// The phone API for the Android app (docs/mobile-api.md): enable, disable, pair, devices, network [name "<name>"], rename <id> "<name>", route <id> phone|mac, revoke <id|all>.
    Phone {
        /// enable | disable | pair | devices | network | rename | route | revoke
        #[arg(default_value = "devices")]
        action: String,
        /// revoke: a device id (`tower phone devices`), or `all`. route, rename: a device id. network: `name`.
        id: Option<String>,
        /// route: `phone` (replies to what it sends are heard on the phone only) or `mac`. rename, network name: the name.
        value: Option<String>,
        /// pair: print the pairing details as JSON (for the orb).
        #[arg(long)]
        json: bool,
    },
    /// Show a visual result in the tower (the panel, the phone): `tower screenshot shot.png --caption "the new login page"`.
    Screenshot {
        /// One or more images (PNG, JPEG, WebP, GIF; 15 MB each at most).
        #[arg(required = true)]
        images: Vec<PathBuf>,
        /// The session it's from (default: the terminal this runs in).
        #[arg(long)]
        session: Option<String>,
        /// What it shows.
        #[arg(long)]
        caption: Option<String>,
    },
    /// The latest screenshots sessions sent (JSON when piped).
    Screenshots {
        #[arg(long, default_value_t = 10)]
        last: usize,
        #[arg(long)]
        session: Option<String>,
    },
    /// The user's behaviors (how the tower works): list, show <id>, add, edit <id>,
    /// enable <id>, disable <id>, remove <id>, undo, history.
    Behavior {
        /// list | show | add | edit | enable | disable | remove | undo | history | prompt
        #[arg(default_value = "list")]
        action: String,
        /// The behavior's id (show, edit, enable, disable, remove); for add, its text.
        arg: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        category: Option<String>,
        /// One plain sentence for the user.
        #[arg(long)]
        summary: Option<String>,
        /// edit: the whole rule, rewritten.
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// What blocked sessions ask, as question sets (JSON). `--session t3`: read that session's dialog now.
    Questions {
        #[arg(long)]
        session: Option<String>,
    },
    /// What only you can decide, in queue order (heaviest first: sessions waiting x how long): what
    /// sessions ask, PRs waiting for a merge, what the tower raised, tasks that failed twice. JSON when
    /// piped. `--space <s>`: that space's own (a muted space's too). `--all`: the last ones that closed too.
    Decisions {
        /// One space's decisions (its name or a part of it).
        #[arg(long)]
        space: Option<String>,
        #[arg(long)]
        json: bool,
        /// Those that closed lately too.
        #[arg(long)]
        all: bool,
    },
    /// Answer a question set: `tower answer <id|session> --json '[{"tab":0,"choice":1}, {"tab":1,"choice":{"other":"…"}}]'`.
    /// Answer a decision (`tower decisions`): `tower answer <id> --choice 0` (an option, by its number) or
    /// `--other "<your words>"`; for one that asks a question, `--json` with its answers works too.
    Answer {
        target: String,
        /// A question set's answers.
        #[arg(long, default_value = "")]
        json: String,
        /// An option, by its number (from 0).
        #[arg(long)]
        choice: Option<usize>,
        /// Your own words.
        #[arg(long)]
        other: Option<String>,
        /// Fill in the answers but don't send them.
        #[arg(long)]
        no_submit: bool,
        /// Drive the dialog from this process, not the voice (for testing).
        #[arg(long, hide = true)]
        here: bool,
    },
    /// The tower's brain: three recommended answers per question, and whether
    /// to answer it itself (`--decision auto`) or ask the user (`escalate`).
    Recommend {
        id: String,
        /// `[{"tab":0,"options":[{"label":"…","why":"…","choice":1}, …]}, …]`; for a
        /// text question also each tab's `"question"`.
        #[arg(long, default_value = "")]
        json: String,
        /// `auto` (routine: the tower answers with recommendation 1) or `escalate` (the user decides).
        #[arg(long)]
        decision: Option<String>,
        /// Why, in a few words.
        #[arg(long, default_value = "")]
        reason: String,
        /// A finished message that asks the user nothing after all.
        #[arg(long)]
        none: bool,
    },
    /// Dismiss a question set, or a decision (it shows no more; nothing is sent).
    Dismiss { id: String },
    /// Put a decision on hold (`tower decisions` has its id): it stays listed, under On hold with
    /// your words, but is not counted, not said and not reminded until it is updated (new commits
    /// or checks on its PRs, its session raises it again), what it waits for looks done (a PR
    /// `--reason` names merges), or `tower unhold`. `--reason`: the user's words, "wait for the
    /// ingest filters, then we go for prod".
    Hold {
        id: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Let a decision go from hold: it waits for you again, counted, and is said once.
    /// `--why`: what brought it back ("the ingest filters are merged").
    Unhold {
        id: String,
        #[arg(long, default_value = "")]
        why: String,
    },
    /// How session questions are handled: normal (the tower answers routine
    /// ones, the user decides the rest), keep-moving, or ask-everything.
    Policy { policy: Option<String> },
    /// How much the tower tells you: `decisions` (only what you must decide, and answers to what
    /// you ask), `results` (also one receipt when a request is finished) or `everything` (every
    /// delivery, finish and note, as before). `--voice`: what it says out loud: `off` (nothing:
    /// the mute), `decisions` (a decision, once, in a sentence, and nothing else) or `replies`
    /// (the default: its answers too). Neither: what they are. Kept in config.toml.
    Tell {
        /// decisions | results | everything
        level: Option<String>,
        /// off | decisions | replies
        #[arg(long)]
        voice: Option<String>,
    },
    /// Undo what the tower opened for you: `tower undo` (the latest, when it was opened in the
    /// last ten minutes) or `tower undo <id>` (`tower acts` lists them). It ends the sessions that
    /// act opened and removes the worktrees it made for them (each only when nothing in it would
    /// be lost), or closes the space it launched. Refused, saying why, when a session was given
    /// more work since or its worktree holds changes; `--force` undoes it anyway.
    Undo {
        /// The act (`r…`).
        id: Option<String>,
        /// Though work would be lost: the sessions are ended, a worktree with changes is kept.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// What the tower opened for you lately, newest first, with what it assumed and whether it
    /// can be undone (JSON when piped).
    Acts {
        #[arg(long)]
        json: bool,
    },
    /// Put a decision in front of the user (the tower's brain calls it):
    /// `tower raise t5 --why merge "Merge PR #42 into main: the checks pass"`.
    Raise {
        /// The session it is about.
        session: String,
        /// The kind of decision: merge, deploy, delete, data, secrets, money, design, failed, waiting.
        #[arg(long)]
        why: String,
        /// The choices to offer, the recommended one first: `[{"label":"…","why":"…"}]`.
        #[arg(long)]
        options: Option<String>,
        /// What it is about, more than the line says: the command it would run, the plan, the
        /// change (shown with it, cut to about 600 characters).
        #[arg(long)]
        detail: Option<String>,
        /// The decision, in one line.
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// The one line for a finished request (the tower's brain calls it). Shown and said when you
    /// want results (`tower tell results`, or you follow its space); kept in the record otherwise.
    Receipt {
        /// The agenda it closes (`a3`): refused while its tasks are open, and a second time.
        #[arg(long)]
        agenda: Option<String>,
        /// What was done, in one line.
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// What the sessions of a space wrote to each other (`tower queue` from one session to
    /// another), newest last. No space: every space's.
    Thread {
        /// The space (its name or a part of it).
        space: Vec<String>,
        #[arg(long)]
        json: bool,
        /// How many messages.
        #[arg(short, default_value_t = 50)]
        n: usize,
    },
    /// Speak text.
    Say {
        text: Vec<String>,
        /// Cut off what is being said.
        #[arg(long)]
        interrupt: bool,
        /// News, not an answer: it waits its turn behind replies, after a bell.
        #[arg(long)]
        alert: bool,
    },
    /// Stop talking.
    Hush,
    /// Press-to-talk: start or stop a take (default: toggle).
    Talk {
        #[arg(default_value = "toggle")]
        action: String,
    },
    /// Drop the current take without sending it.
    Cancel,
    /// Microphone mode: ptt, handsfree or off.
    Mode { mode: String },
    /// Send text to the brain as if spoken. `--space <space>`: to that space's own brain, when it
    /// has one that can take it (else to the main brain, with the space named).
    Ask {
        /// The space it is asked in (its name or a part of it).
        #[arg(long)]
        space: Option<String>,
        text: Vec<String>,
    },
    /// Tell the brain when a terminal stops working.
    Watch { terminal: String, note: Vec<String> },
    /// Record a job in the tower's history: `start`, `done`, `failed` or `update`.
    Job {
        /// start | done | failed | update
        action: String,
        /// The session doing it (`t3`, `vps/t2`).
        terminal: String,
        /// start: what it was asked. done/failed: what came of it (one or two sentences).
        text: Vec<String>,
        /// update: where it stands (`working`, `blocked`, ...).
        #[arg(long)]
        status: Option<String>,
    },
    /// Record a summary of what a session has done so far: `tower summary t3 "…"`,
    /// or for a whole space: `tower summary --space b2c "…"` (with `--session t3` to name one).
    Summary {
        /// The session, then the summary (without `--space`); the summary alone with it.
        words: Vec<String>,
        /// Tag it with this Herdr space.
        #[arg(long)]
        space: Option<String>,
        /// The session, with `--space`.
        #[arg(long)]
        session: Option<String>,
    },
    /// Focus one Herdr space: its news is spoken, other spaces' is held for
    /// later (`tower digest`). No space: say which one is focused.
    Focus {
        space: Vec<String>,
        /// Speak every space's news again.
        #[arg(long)]
        clear: bool,
    },
    /// A space is a mission with an end. `tower space goal b2c "Answer support tickets" [--done-when
    /// "the queue is empty"]` ("" clears it; `agenda` is the same; `--by you`: set by hand). A goal
    /// follows the user's requests there: one that adds scope rewrites it, and its history is kept.
    /// `tower space goal-request <space> "<words>" [--dry-run]` weighs words as such a request by hand.
    /// A space with no goal that isn't standing (opened by a chat) gets its first goal from the
    /// user's requests; `tower space goal-backfill <space>|all [--dry-run]` writes it from those so far.
    /// `tower space standing <space>
    /// on|off`: a space you keep (never ready, never closed by the tower, no worktrees).
    /// `tower space report <space> "<text>"`: its closing report. `tower space close <space>
    /// [--force]`: ends its sessions, removes its clean worktrees, archives it (refused while a
    /// session there works or waits for you, unless --force). `tower space resolve <space>
    /// [--force] [--dry-run]`: its goal is achieved; marked so, then closed the same way
    /// (`--dry-run`: what it would end and remove).
    /// `tower space follow|unfollow <space>`: its finished requests' receipts reach you as they land.
    /// `tower space mute|unmute <space>`: what needs you there stays in its own view (`tower decisions
    /// --space <space>`), off the orb's count and out of the main queue.
    /// `tower space review <space> on|off`: whether the PRs its sessions open get a reviewer session
    /// (on by default; off ends one that is open there).
    Space {
        /// goal | agenda | goal-request | goal-backfill | standing | report | close | resolve | follow | unfollow | mute | unmute | review
        what: String,
        space: String,
        text: Vec<String>,
        /// goal: what counts as done.
        #[arg(long)]
        done_when: Option<String>,
        /// goal: who sets it: `you` (the user, by hand: a window, the phone); else the tower's brain.
        #[arg(long)]
        by: Option<String>,
        /// close, resolve: though a session there still works or waits for you.
        #[arg(long)]
        force: bool,
        /// resolve, goal-request, goal-backfill: say what it would do, and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// The PRs' reviews: a PR a session opens in a space with review on gets a reviewer session,
    /// which looks for critical issues only and agrees with the PR's session before you merge.
    /// `tower review [list]`, `show <pr>` (JSON when piped, or `--json`). The reviewer's `report
    /// <pr> --critical <n> "<one line>"` and `agree <pr> ["<one line>"]`; the owner's `fixed <pr>
    /// "<one line>"`. A PR is its key (owner/repo#12), its link, or #12.
    Review {
        /// list | show | report | agree | fixed
        action: Option<String>,
        /// The PR: owner/repo#12, its link, or #12.
        pr: Option<String>,
        /// report: how many critical issues it found (0: none).
        #[arg(long)]
        critical: Option<u32>,
        #[arg(long)]
        json: bool,
        /// The one line.
        text: Vec<String>,
    },
    /// Every Herdr space: focus, goal, state, unread news, sessions (JSON when piped). A space
    /// quiet for a day, or closed, is left out unless `--all`.
    Spaces {
        #[arg(long)]
        all: bool,
    },
    /// Catch up on held news, one space at a time, the most pressing first;
    /// what is printed is marked read.
    Digest {
        /// Only list the spaces with news, in order.
        #[arg(long)]
        list: bool,
        /// Pass over the next space for now (it stays unread).
        #[arg(long)]
        skip: bool,
        /// This space instead of the next.
        #[arg(long)]
        space: Option<String>,
        /// Print it without marking it read.
        #[arg(long)]
        peek: bool,
    },
    /// Record a line of activity in the tower's history.
    Log {
        text: Vec<String>,
        /// The session it is about.
        #[arg(long)]
        session: Option<String>,
    },
    /// The tower's history: jobs and activity (JSON when piped).
    History {
        /// Only jobs: `open` (working or blocked) or `finished`.
        #[arg(long)]
        jobs: Option<String>,
        /// Only this session.
        #[arg(long)]
        session: Option<String>,
        /// How many entries.
        #[arg(short, default_value_t = 30)]
        n: usize,
    },
    /// Bring a terminal (default: the brain) on screen in Herdr.
    Show { terminal: Option<String> },
    /// Install: this binary to ~/.local/bin, config and the brain's folder.
    Install {
        /// No longer needed: the orb has the tower's keys (⌘⇧Space, ⌘⇧Esc, ⌘⇧K).
        #[arg(long, hide = true)]
        keys: bool,
    },
    /// The orb: a small floating window that shows what the tower is doing (`tower up` starts it).
    Orb {
        /// Read state.json from this folder instead of the daemon's (for trying it out).
        #[arg(long, hide = true)]
        ui_dir: Option<PathBuf>,
    },
    /// Check the setup.
    Doctor,
    /// Kokoro speaks a command and Whisper writes it down (no microphone needed).
    Selftest,
    /// Run the voice daemon in the foreground (`tower up` does this in a Herdr terminal).
    Daemon,
    /// Called by the brain's agent when it finishes a turn (a space's own brain names itself).
    #[command(hide = true)]
    Hook {
        kind: String,
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("tower: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Up { brain: which, no_brain, show, no_orb } => {
            let cfg = config::Config::load()?;
            config::Config::write_default_if_missing()?;
            if !no_orb {
                match orb::spawn() {
                    Ok(true) => println!("Orb started (⌘⇧K shows or hides it)."),
                    Ok(false) => {}
                    Err(e) => eprintln!("tower: the orb didn't start: {e:#}"),
                }
            }
            println!("Starting the voice (Whisper and Kokoro load in a few seconds; the first run downloads Kokoro) ...");
            brain::launch_voice()?;
            println!("Voice ready. Microphone mode: {}.", cfg.mode);
            if !no_brain {
                let which = which.unwrap_or_else(|| cfg.brain.clone());
                let address = brain::launch(&cfg, &which)?;
                println!("Brain ({which}) running as {address}.");
                if cfg.brains.per_space {
                    println!("Each open space gets a tower of its own (tower-<space>) with its first session: tower status lists them.");
                }
                if show {
                    show_terminal(&address)?;
                }
                let _ = protocol::send(&Request::Say { text: "Tower online.".into(), interrupt: true, kind: String::new(), from: None, brain: None });
            }
            Ok(())
        }
        Cmd::Down { keep_brain } => {
            let _ = protocol::send(&Request::Quit);
            brain::stop(brain::VOICE_NAME);
            if !keep_brain {
                // The spaces' own first: nothing is left to start them again.
                let own = brains::stop_space_brains();
                brain::stop(brain::NAME);
                if !own.is_empty() {
                    println!("Stopped the spaces' own brains: {}.", own.join(", "));
                }
            }
            println!("Tower stopped.");
            Ok(())
        }
        Cmd::Model { model, effort, restart, json } => brains::cmd_model(model, effort, restart, json),
        Cmd::Queue { args } => queue_cmd(&args),
        Cmd::Dispatch { json, dry_run } => dispatch::run(&json, dry_run),
        Cmd::Sessions { terminal } => {
            let v = protocol::send(&Request::Sessions)?.state.unwrap_or_default();
            let all = v["sessions"].as_array().cloned().unwrap_or_default();
            // A name means its terminal's address.
            let want = terminal.as_deref().map(|t| hc::get(t).and_then(|v| v["address"].as_str().map(String::from)).unwrap_or_else(|| t.to_string()));
            let mine: Vec<Value> = all.into_iter().filter(|s| want.as_deref().is_none_or(|t| s["terminal"] == t)).collect();
            if std::io::stdout().is_terminal() {
                for s in &mine {
                    let said = s["said"].as_str().unwrap_or("");
                    let said: String = said.chars().take(100).collect();
                    println!(
                        "{:<6} {:<12} {:<8} {}  prompt: {}  said: {}",
                        s["terminal"].as_str().unwrap_or(""),
                        s["app"].as_str().unwrap_or(""),
                        s["state"].as_str().unwrap_or(""),
                        s["cwd"].as_str().unwrap_or(""),
                        s["prompt"].as_str().unwrap_or("").chars().take(60).collect::<String>(),
                        said
                    );
                }
            } else {
                print_json(&json!(mine));
            }
            Ok(())
        }
        Cmd::Hooks { action } => install::agent_hooks(&action),
        Cmd::Mute { action } => {
            let r = protocol::send(&Request::Mute { action })?;
            if !r.ok {
                bail!("{}", r.error.unwrap_or_default());
            }
            let muted = r.state.as_ref().and_then(|s| s["muted"].as_bool()).unwrap_or(false);
            println!("{}", if muted { "Muted: the tower says nothing (tower mute off to hear it again)." } else { "Not muted: the tower talks." });
            Ok(())
        }
        Cmd::Automerge { action } => automerge_cmd(&action),
        Cmd::Releases { space, json } => releases_cmd(space.as_deref(), json),
        Cmd::Impact { pr, verdict, text, json } => impact_cmd(&pr, verdict.as_deref(), &text.join(" "), json),
        Cmd::Pr { action, pr, why } => pr_cmd(&action, &pr, &why.join(" ")),
        Cmd::Validate { pr, check, status, evidence, json } => validate_cmd(&pr, check.as_deref(), status.as_deref(), evidence.as_deref(), json),
        Cmd::MacVoice { action } => {
            let on = if protocol::is_running() {
                let r = protocol::send(&Request::MacVoice { action })?;
                if !r.ok {
                    bail!("{}", r.error.unwrap_or_default());
                }
                r.state.as_ref().and_then(|s| s["voice_on_mac"].as_bool()).unwrap_or(false)
            } else {
                // The voice isn't running: kept for when it starts.
                let now = config::Config::load()?.voice_on_mac;
                let on = match action.as_str() {
                    "on" => true,
                    "off" => false,
                    "toggle" => !now,
                    "status" | "" => now,
                    _ => bail!("tower mac-voice on|off|toggle|status"),
                };
                if on != now {
                    phone::set_voice_on_mac(on)?;
                }
                on
            };
            println!(
                "{}",
                if on { "Voice on this Mac: everything the tower says plays here, never on a phone." } else { "Voice as routed: a phone that is the voice plays it (this Mac otherwise)." }
            );
            Ok(())
        }
        Cmd::Restart { brain: brains_too } => {
            let _ = protocol::send(&Request::Quit);
            brain::stop(brain::VOICE_NAME);
            brain::launch_voice()?;
            println!("Voice restarted.");
            if brains_too {
                // By the voice, which outlives them (this command may run in a brain's own terminal).
                let r = protocol::send(&Request::Brains { action: "restart".into(), space: String::new() })?;
                if !r.ok {
                    bail!("{}", r.error.unwrap_or_default());
                }
                let names: Vec<String> = r.state.unwrap_or_default()["restarted"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
                match names.is_empty() {
                    true => println!("No brain was running: nothing to restart (tower up starts the main one)."),
                    false => println!("Restarting the brains: {}.", names.join(", ")),
                }
            }
            Ok(())
        }
        Cmd::Status => {
            let r = protocol::send(&Request::Status)?;
            print_json(&r.state.unwrap_or_default());
            Ok(())
        }
        Cmd::Agents { all } => agents(all),
        Cmd::Start { account, name, cwd, on, space, new_space, no_watch, agenda, title, plan_id, no_track, no_worktree, no_act, task } => {
            let tag = (!no_track).then(|| agenda::Tag { agenda, title: title.unwrap_or_default(), plan_id: plan_id.unwrap_or_default(), ..Default::default() });
            let space = space.map(|s| spaces::Asked { name: s, new: new_space });
            start(account.as_deref(), &name, cwd, on.as_deref(), space.as_ref(), no_watch, tag, &task.join(" "), no_worktree, !no_act)
        }
        Cmd::Launch { goal, setup, name, dry_run, draft, adjust, prior, repo, title, done_when, account, plan, review, no_review, story_file, json } => {
            let goal = goal.join(" ");
            if draft {
                ticket::cmd(&goal, setup.as_deref(), adjust.as_deref(), prior.as_deref(), json)
            } else {
                let review = match (review, no_review) {
                    (true, _) => Some(true),
                    (_, true) => Some(false),
                    _ => None,
                };
                let ask = launch::Ask {
                    setup: setup.as_deref(),
                    name: name.as_deref(),
                    repos: &repo,
                    title: title.as_deref(),
                    done_when: done_when.as_deref(),
                    account: account.as_deref(),
                    plan: plan.as_deref(),
                    review,
                    story: story_file.as_deref(),
                };
                launch::cmd(&goal, &ask, dry_run, json)
            }
        }
        Cmd::Plan { args } => plans::cmd(&args),
        Cmd::Setups { json } => setups::cmd_list(json),
        Cmd::Setup { action, name, account, about, repo, was } => match action.as_str() {
            "add" => setups::cmd_add(&name.join(" "), account.as_deref(), about.as_deref(), &repo, was.as_deref()),
            "remove" | "rm" | "delete" => setups::cmd_remove(&name.join(" ")),
            other => bail!("tower setup {other}: use `tower setup add \"<name>\" --account <account> --repo <folder>` or `tower setup remove \"<name>\"` (`tower setups` lists them)"),
        },
        Cmd::Missions { json } => missions::cmd_missions(json),
        Cmd::Agenda { action, args, all, force, title, space } => agenda_cmd(&action, &args, all, force, title, space),
        Cmd::Introduce { space, dry_run } => introduce_cmd(&space.join(" "), dry_run),
        Cmd::Reopen { action, spaces, dry_run, json } => reopen_cmd(action.as_deref().unwrap_or("list"), &spaces, dry_run, json),
        Cmd::Accounts { usage: true, json, .. } => accounts_usage(json),
        Cmd::Accounts { on, .. } => list_accounts(on.as_deref()),
        Cmd::JobRunning { action, what, pid, until, minutes, session } => {
            job_running(&action, what.as_deref().unwrap_or(""), pid, until.as_deref().unwrap_or(""), minutes, session.as_deref())
        }
        Cmd::Failover { action, target, to, json } => failover_cmd(action.as_deref().unwrap_or("status"), target.as_deref().unwrap_or(""), to.as_deref().unwrap_or(""), json),
        Cmd::Project { name } => {
            let asked = name.join(" ");
            match projects::best(&asked) {
                Some((m, others)) => {
                    print_json(&json!({ "folder": m.folder, "others": others.iter().map(|o| &o.folder).collect::<Vec<_>>() }));
                    Ok(())
                }
                None => bail!("no project folder fits {asked:?} under {}", projects::roots().iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")),
            }
        }
        Cmd::Behavior { action, arg, title, category, summary, text, json } => behavior_cmd(&action, &arg, title, category, summary, text, json),
        Cmd::Questions { session } => {
            let v = match session {
                Some(t) => match daemon::asks::detect(&t) {
                    Some(set) => serde_json::to_value(set)?,
                    None => bail!("{t} isn't blocked on a dialog the tower can read"),
                },
                None => protocol::send(&Request::Questions)?.state.unwrap_or_default(),
            };
            print_json(&v);
            Ok(())
        }
        Cmd::Decisions { space, json, all } => decisions_cmd(space, json, all),
        Cmd::Answer { target, json, choice, other, no_submit, here } => {
            let decision = decisions::is_id(&target);
            let answers: Value = match (choice, other, json.trim()) {
                // A decision takes its option's number or the user's words; a question set, the same for its one question.
                (Some(n), _, _) if decision => json!({ "choice": n }),
                (None, Some(text), _) if decision => json!({ "other": text }),
                (Some(n), _, _) => json!([{ "tab": 0, "choice": n }]),
                (None, Some(text), _) => json!([{ "tab": 0, "choice": { "other": text } }]),
                (None, None, "") => bail!(
                    "tower answer <id> --json '[{{\"tab\":0,\"choice\":1}}]' (a question set), or --choice <n> / --other \"<words>\" (a decision: tower decisions)"
                ),
                (None, None, j) => serde_json::from_str(j).map_err(|e| anyhow::anyhow!("--json: {e}"))?,
            };
            if here {
                let Some(set) = daemon::asks::detect(&target) else { bail!("{target} isn't blocked on a dialog the tower can read") };
                let answers: Vec<questions::Answer> = serde_json::from_value(answers)?;
                let mut term = questions::Hc { address: set.session.clone() };
                return match questions::drive(&mut term, &set, &answers, !no_submit) {
                    Ok(()) => {
                        println!("Answered {} ({}).", set.id, set.kind);
                        Ok(())
                    }
                    Err(stop) => bail!("stopped at {stop}"),
                };
            }
            let v = answers::answer(&target, answers, !no_submit)?;
            if !decision {
                println!("Answered{}.", if no_submit { " (filled in, not sent)" } else { "" });
            } else if std::io::stdout().is_terminal() {
                let n = |k: &str| v[k].as_array().map(Vec::len).unwrap_or(0);
                match v["state"].as_str() {
                    Some("answering") => println!("Answering {target}: its answer is going in to {} session(s).", n("answered")),
                    _ if !v["task"].is_null() => println!("Answered {target}: its task {} is {}.", v["task"].as_str().unwrap_or(""), if v["abandoned"].is_null() { "sent again" } else { "dropped" }),
                    _ => println!("Answered {target}: \"{}\" went to {} session(s).", v["answer"].as_str().unwrap_or(""), n("sent")),
                }
            } else {
                print_json(&v);
            }
            Ok(())
        }
        Cmd::Recommend { id, json, decision, reason, none } => {
            let recs: Value = if none || json.trim().is_empty() { Value::Null } else { serde_json::from_str(&json).map_err(|e| anyhow::anyhow!("--json: {e}"))? };
            if !none && recs.is_null() {
                bail!("give --json with the recommendations, or --none");
            }
            if let Some(d) = &decision
                && d != "auto"
                && d != "escalate"
            {
                bail!("--decision is auto or escalate");
            }
            let r = protocol::send(&Request::Recommend { id: id.clone(), recs, decision, reason, none })?;
            if !r.ok {
                bail!("{}", r.error.unwrap_or_default());
            }
            let v = r.state.unwrap_or_default();
            match (none, v["decision"].as_str()) {
                (true, _) => println!("{id}: closed, it asks nothing."),
                (_, Some("auto")) => println!("{id}: answering it for the user ({}).", v["reason"].as_str().unwrap_or("")),
                (_, d) => println!("{id}: for the user to decide ({}).", if d.is_some() { v["reason"].as_str().unwrap_or("") } else { "" }),
            }
            Ok(())
        }
        Cmd::Dismiss { id } => {
            answers::dismiss(&id)?;
            println!("Dismissed {id}.");
            Ok(())
        }
        Cmd::Hold { id, reason } => hold_cmd(&id, &reason, false),
        Cmd::Unhold { id, why } => hold_cmd(&id, &why, true),
        Cmd::Policy { policy } => {
            let Some(p) = policy else {
                let now = protocol::send(&Request::Status).ok().and_then(|r| r.state).and_then(|s| s["question_policy"].as_str().map(String::from));
                println!("{}", now.unwrap_or_else(|| config::Config::load().map(|c| c.question_policy).unwrap_or_else(|_| "normal".into())));
                return Ok(());
            };
            answers::set_policy(&p)?;
            println!("Questions: {p}.");
            Ok(())
        }
        Cmd::Screenshot { images, session, caption } => screenshots::cmd(&images, session, caption),
        Cmd::Screenshots { last, session } => screenshots::cmd_list(last, session.as_deref()),
        Cmd::Phone { action, id, value, json } => phone::cmd(&action, id.as_deref(), value.as_deref(), json),
        Cmd::Say { text, interrupt, alert } => {
            ok(Request::Say { text: text.join(" "), interrupt, kind: if alert { "alert".into() } else { String::new() }, from: peers::caller(), brain: None })
        }
        Cmd::Tell { level, voice } => tell_cmd(level.as_deref(), voice.as_deref()),
        Cmd::Undo { id, force, json } => undo_cmd(id.as_deref(), force, json),
        Cmd::Acts { json } => acts_cmd(json),
        Cmd::Raise { session, why, options, detail, text } => {
            let options: Option<Value> = match options.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
                Some(o) => match serde_json::from_str::<Value>(o) {
                    Ok(v) if v.is_array() => Some(v),
                    Ok(_) => bail!("--options is a list: '[{{\"label\":\"…\",\"why\":\"…\"}}]'"),
                    Err(e) => bail!("--options: {e}"),
                },
                None => None,
            };
            let detail = detail.map(|d| d.trim().to_string()).unwrap_or_default();
            let r = protocol::send(&Request::Raise { session, why, text: text.join(" "), options, detail, from: peers::caller() })?;
            if !r.ok {
                bail!("{}", r.error.unwrap_or_default());
            }
            let v = r.state.unwrap_or_default();
            if std::io::stdout().is_terminal() {
                let (id, name, session) = (v["id"].as_str().unwrap_or(""), v["name"].as_str().unwrap_or(""), v["session"].as_str().unwrap_or(""));
                if v["new"] == true {
                    println!("Raised for the user as {id}, about {name} ({session}). It is shown, and said once: don't say or raise it again.");
                } else {
                    println!(
                        "Already in front of the user as {id} ({}; raised {} time(s), {} session(s) waiting): nothing new is shown or said.",
                        v["title"].as_str().unwrap_or(""),
                        v["count"],
                        v["waiting"]
                    );
                }
            } else {
                print_json(&v);
            }
            Ok(())
        }
        Cmd::Receipt { agenda, text } => {
            let r = protocol::send(&Request::Receipt { text: text.join(" "), agenda, from: peers::caller() })?;
            if !r.ok {
                bail!("{}", r.error.unwrap_or_default());
            }
            let v = r.state.unwrap_or_default();
            if std::io::stdout().is_terminal() {
                println!("{}", if v["told"] == true { "Receipt shown and said to the user." } else { "Receipt kept in the record: nothing is shown or said (`tower tell results`, or `tower space follow <space>`, to hear them)." });
            } else {
                print_json(&v);
            }
            Ok(())
        }
        Cmd::Thread { space, json, n } => peers::cmd(&space.join(" "), json, n),
        Cmd::Hush => ok(Request::Hush),
        Cmd::Talk { action } => ok(Request::Talk { action }),
        Cmd::Cancel => ok(Request::Cancel),
        Cmd::Mode { mode } => ok(Request::Mode { mode }),
        Cmd::Ask { space, text } => ok(Request::Ask { text: text.join(" "), space: space.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) }),
        Cmd::Job { action, terminal, text, status } => {
            let text = text.join(" ");
            match action.as_str() {
                "start" => println!("{}", history::job_start(&terminal, &text)),
                "done" => {
                    history::job_update(&terminal, history::JobState::Done, &text);
                    agenda_judged(&terminal, true, &text);
                }
                "failed" => {
                    history::job_update(&terminal, history::JobState::Failed, &text);
                    agenda_judged(&terminal, false, &text);
                }
                "update" => match history::JobState::parse(status.as_deref().unwrap_or("working")) {
                    history::JobState::Other(s) => bail!("tower job update --status {s}: use working, blocked, done, failed, error or exited"),
                    st => history::job_update(&terminal, st, &text),
                },
                other => bail!("tower job {other}: use start, done, failed or update"),
            }
            Ok(())
        }
        Cmd::Summary { words, space, session } => {
            match space {
                Some(sp) => {
                    let names = spaces::known(&spaces::live(), &history::load(), &spaces::load());
                    let sp = spaces::resolve(&sp, &names).unwrap_or(sp);
                    history::record_space("summary", &sp, session.as_deref(), &words.join(" "));
                }
                None => {
                    let Some((terminal, text)) = words.split_first() else {
                        bail!("tower summary <terminal> \"<text>\" (or --space <space>)")
                    };
                    history::record_session("summary", terminal, &text.join(" "), None);
                }
            }
            Ok(())
        }
        Cmd::Focus { space, clear } => spaces::cmd_focus(Some(space.join(" ")).filter(|s| !s.is_empty()), clear),
        Cmd::Space { what, space, text, done_when, by, force, dry_run } => match what.as_str() {
            "goal" | "agenda" => missions::cmd_goal(space, text.join(" "), done_when, by),
            "goal-request" => goals::cmd_request(&space, &text.join(" "), dry_run),
            // The space's name may be several words; `all`: every open space with no goal.
            "goal-backfill" => goals::cmd_backfill(&std::iter::once(space).chain(text).collect::<Vec<_>>().join(" "), dry_run),
            "report" => missions::cmd_report(space, text.join(" ")),
            // The space's name may be several words.
            "standing" => missions::cmd_standing(std::iter::once(space).chain(text).collect()),
            "close" => missions::cmd_close(std::iter::once(space).chain(text).collect::<Vec<_>>().join(" "), force),
            "resolve" => missions::cmd_resolve(std::iter::once(space).chain(text).collect::<Vec<_>>().join(" "), force, dry_run),
            "follow" | "unfollow" => spaces::cmd_follow(std::iter::once(space).chain(text).collect::<Vec<_>>().join(" "), what == "follow"),
            "mute" | "unmute" => spaces::cmd_mute(std::iter::once(space).chain(text).collect::<Vec<_>>().join(" "), what == "mute"),
            "review" => missions::cmd_review(std::iter::once(space).chain(text).collect()),
            other => bail!(
                "tower space {other}: use `tower space goal <space> \"<goal>\" [--done-when \"<…>\"] [--by you]`, `goal-request <space> \"<the user's words>\" [--dry-run]`, `goal-backfill <space>|all [--dry-run]`, `standing <space> on|off`, `review <space> on|off`, `report <space> \"<text>\"`, `close <space> [--force]`, `resolve <space> [--force] [--dry-run]`, or `follow|unfollow|mute|unmute <space>`"
            ),
        },
        Cmd::Review { action, pr, critical, json, text } => review_cmd(action.as_deref().unwrap_or("list"), pr.as_deref().unwrap_or(""), critical, &text.join(" "), json),
        Cmd::Spaces { all } => spaces::cmd_spaces(all),
        Cmd::Digest { list, skip, space, peek } => spaces::cmd_digest(list, skip, space, peek),
        Cmd::Log { text, session } => {
            match session {
                Some(s) => history::record_session("note", &s, &text.join(" "), None),
                None => history::record("note", &text.join(" ")),
            }
            Ok(())
        }
        Cmd::History { jobs, session, n } => show_history(jobs.as_deref(), session.as_deref(), n),
        Cmd::Watch { terminal, note } => {
            ok(Request::Watch { terminal: terminal.clone(), note: note.join(" "), account: None })?;
            println!("Watching {terminal}: you'll get a [watch] message when it stops working. End your turn now.");
            Ok(())
        }
        Cmd::Show { terminal } => show_terminal(terminal.as_deref().unwrap_or(brain::NAME)),
        Cmd::Install { keys } => install::install(keys),
        Cmd::Orb { ui_dir } => orb::run(ui_dir),
        Cmd::Doctor => install::doctor(),
        Cmd::Daemon => daemon::run(),
        Cmd::Selftest => selftest::run(),
        Cmd::Hook { kind, args } => {
            let arg = args.first().map(String::as_str);
            match kind.as_str() {
                // A space's own brain's hooks say whose they are: `tower hook claude tower-<space>`.
                "claude" => hooks::claude_stop(arg),
                "claude-notify" => hooks::claude_notify(arg),
                // Its turn ended on an error: the request it began with gets no answer.
                "claude-failed" => hooks::claude_failed(arg),
                "codex" => hooks::codex(&args),
                "activity-prompt" => activity::hook("prompt"),
                "activity-pre" => activity::hook("pre"),
                "activity-post" => activity::hook("post"),
                // Every agent session's hooks: `tower hook agent claude|codex`.
                "agent" => hooks::agent(arg.unwrap_or("claude")),
                // The brain's Bash calls: one hand-off per task.
                "brain-guard" => hooks::brain_guard(),
                _ => {}
            }
            Ok(())
        }
    }
}

/// `tower review …`: what the daemon keeps of the PRs' reviews (`list` and
/// `show` read `reviews.json` when it isn't running), and the reviewer's
/// and the owner's word on one.
fn review_cmd(action: &str, pr: &str, critical: Option<u32>, text: &str, as_json: bool) -> Result<()> {
    let action = action.trim().to_lowercase();
    if !["list", "show", "report", "agree", "fixed"].contains(&action.as_str()) {
        bail!("tower review {action}: use list, show <pr>, report <pr> --critical <n> \"<one line>\", agree <pr> [\"<one line>\"] or fixed <pr> \"<one line>\"");
    }
    if action != "list" && pr.trim().is_empty() {
        bail!("tower review {action} <pr>: the PR's key (owner/repo#12), its link, or #12");
    }
    if action == "report" && critical.is_none() {
        bail!("tower review report <pr> --critical <n> \"<one line>\" (0 when nothing is critical)");
    }
    if action == "fixed" && text.trim().is_empty() {
        bail!("tower review fixed <pr> \"<one line: what you fixed>\"");
    }
    let v = if protocol::is_running() {
        let r = protocol::send(&Request::Review { action: action.clone(), pr: pr.trim().to_string(), critical, text: text.to_string(), from: peers::caller() })?;
        if !r.ok {
            bail!("{}", r.error.unwrap_or_default());
        }
        r.state.unwrap_or_default()
    } else {
        let book = reviews::Book::load_from(&reviews::path());
        match action.as_str() {
            "list" => json!({ "reviews": book.reviews.iter().rev().map(reviews::Review::view).collect::<Vec<_>>() }),
            "show" => json!({ "review": book.find(pr, None)?.view() }),
            _ => bail!("the tower's voice isn't running (tower up): a review's word goes to it"),
        }
    };
    if as_json || !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    let read = |r: &Value| serde_json::from_value::<reviews::Review>(r.clone()).unwrap_or_default();
    match action.as_str() {
        "list" => {
            let list = v["reviews"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("No reviews yet: a PR a session opens in a space with review on gets one.");
            }
            for r in &list {
                println!("{}", reviews::row(&read(r)));
            }
        }
        "show" => {
            let r = read(&v["review"]);
            println!("{}", reviews::row(&r));
            println!("  {} · owner {} ({}) · reviewer {} ({}) · in {}", r.url, r.owner_name, r.owner, r.reviewer_name, r.reviewer, r.folder);
            for s in &r.steps {
                let n = s.critical.map(|n| format!(" {n} critical")).unwrap_or_default();
                let by = if s.by.is_empty() { String::new() } else { format!(" by {}", s.by) };
                println!("  {} {}{n}{by}{}", phone::ago(s.at_ms), s.what, if s.text.is_empty() { String::new() } else { format!(": {}", s.text) });
            }
        }
        "report" => {
            let r = read(&v["review"]);
            match (r.status, v["noted_owner"] == true, v["decision"].as_str()) {
                (reviews::Status::Stuck, _, Some(id)) => println!("{}: round {} and no agreement. It is the user's now ({id}); their word comes to you both.", r.short(), r.rounds),
                (_, true, _) => println!("{}: round {}, {} critical. The tower told its owner who you are and what to do; the comments themselves are yours to send (tower queue {}).", r.short(), r.rounds, r.critical, r.owner),
                _ if r.critical == 0 => println!("{}: nothing critical. Approve it and agree: tower review agree {}.", r.short(), r.key),
                _ => println!("{}: round {}, {} critical, kept.", r.short(), r.rounds, r.critical),
            }
        }
        "agree" => println!("{}: agreed. The review is over; its reviewer session ends now.", read(&v["review"]).short()),
        _ => {
            let r = read(&v["review"]);
            println!("{}: kept. Tell the reviewer yourself (tower queue {} \"…\"): it checks again.", r.short(), r.reviewer);
        }
    }
    Ok(())
}

fn show_history(jobs: Option<&str>, session: Option<&str>, n: usize) -> Result<()> {
    let entries = history::load();
    let mine = |s: &str| session.is_none_or(|want| s == want);
    if let Some(which) = jobs {
        let open = match which {
            "open" => true,
            "finished" | "done" => false,
            other => bail!("--jobs {other}: use open or finished"),
        };
        let list: Vec<_> = history::jobs(&entries).into_iter().filter(|j| j.open() == open && mine(&j.session)).take(n).collect();
        if std::io::stdout().is_terminal() {
            for j in &list {
                println!(
                    "{:<9} {:<10} {:<16} {:<10} {}{}",
                    j.status,
                    j.session,
                    j.name,
                    j.account.as_deref().unwrap_or("-"),
                    j.title,
                    if j.summary.is_empty() { String::new() } else { format!(" — {}", j.summary) }
                );
            }
        } else {
            let rows: Vec<Value> = list.iter().map(|j| j.row()).collect();
            print_json(&json!(rows));
        }
        return Ok(());
    }
    let list: Vec<_> = entries.iter().rev().filter(|e| mine(e.session.as_deref().unwrap_or(""))).take(n).collect();
    if std::io::stdout().is_terminal() {
        for e in list.iter().rev() {
            let account = e.account.as_deref().map(|a| format!("[{a}] ")).unwrap_or_default();
            println!("{:<8} {:<10} {account}{}", e.kind, e.session.as_deref().unwrap_or(""), e.text);
        }
    } else {
        print_json(&serde_json::to_value(&list)?);
    }
    Ok(())
}

/// `tower tell [decisions|results|everything] [--voice off|decisions|replies]`:
/// how much it tells and what it says out loud, or set them.
/// `tower releases [<space>]`: the release tracks as the daemon last worked them out.
fn releases_cmd(space: Option<&str>, as_json: bool) -> Result<()> {
    let state: serde_json::Value = std::fs::read_to_string(config::ui_dir().join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let all = state["releases"].as_object().cloned().unwrap_or_default();
    let want = space.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let picked: Vec<(&String, &serde_json::Value)> = all.iter().filter(|(k, _)| want.as_deref().is_none_or(|w| k.to_lowercase().contains(w))).collect();
    if as_json || !std::io::stdout().is_terminal() {
        print_json(&serde_json::json!(picked.iter().map(|(k, v)| ((*k).clone(), (*v).clone())).collect::<serde_json::Map<_, _>>()));
        return Ok(());
    }
    if picked.is_empty() {
        println!("{}", if want.is_some() { "No release track for that space: it has no PR the tower tracks." } else { "No space has PRs the tower tracks." });
    }
    for (name, t) in picked {
        let summary = t["summary"].as_str().unwrap_or("");
        println!("{name}{}", if summary.is_empty() { String::new() } else { format!(": {summary}") });
        for r in t["repos"].as_array().into_iter().flatten() {
            println!("  {}", r["repo"].as_str().unwrap_or(""));
            for l in r["lines"].as_array().into_iter().flatten().filter_map(|l| l.as_str()) {
                println!("    {l}");
            }
            for c in r["changes"].as_array().into_iter().flatten() {
                println!("    #{} {}: {}", c["number"], c["title"].as_str().unwrap_or(""), c["stage_line"].as_str().unwrap_or(""));
            }
        }
    }
    Ok(())
}

/// `tower pr cant-fix <pr> "<why>"`.
fn pr_cmd(action: &str, pr: &str, why: &str) -> Result<()> {
    if action != "cant-fix" {
        bail!("tower pr cant-fix <pr> \"<why you can't resolve its conflicts>\"");
    }
    if why.trim().is_empty() {
        bail!("say why: tower pr cant-fix {pr} \"<why you can't resolve its conflicts>\"");
    }
    let r = protocol::send(&Request::CantFix { pr: pr.trim().to_string(), why: why.trim().to_string(), from: peers::caller() })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    println!("{}: {}", v["pr"].as_str().unwrap_or(pr), v["line"].as_str().unwrap_or("it goes to the user"));
    Ok(())
}

/// `tower impact <pr> [breaks|clean "<words>"]`.
fn impact_cmd(pr: &str, verdict: Option<&str>, text: &str, as_json: bool) -> Result<()> {
    let verdict = verdict.unwrap_or("").trim();
    if !verdict.is_empty() && text.trim().is_empty() {
        bail!("tower impact <pr> {verdict} \"<what breaks and who uses it (repo, file, function), or what you compiled or ran>\"");
    }
    let r = protocol::send(&Request::Impact { pr: pr.trim().to_string(), verdict: verdict.to_string(), text: text.trim().to_string(), from: peers::caller() })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if as_json || !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    println!("{}: {}", v["pr"].as_str().unwrap_or(""), v["line"].as_str().unwrap_or("who depends on it wasn't checked"));
    for d in v["impact"]["dependents"].as_array().into_iter().flatten() {
        let within = d["within"].as_str().filter(|w| !w.is_empty()).map(|w| format!(" in {w}")).unwrap_or_default();
        println!("  {} uses {}: {} {}:{}{within}", d["repo"].as_str().unwrap_or(""), d["uses"].as_str().unwrap_or(""), d["repo"].as_str().unwrap_or(""), d["file"].as_str().unwrap_or(""), d["line"]);
    }
    Ok(())
}

/// `tower validate <pr> [<check> <status> --evidence "…"]`.
fn validate_cmd(pr: &str, check: Option<&str>, status: Option<&str>, evidence: Option<&str>, as_json: bool) -> Result<()> {
    let (check, status) = (check.unwrap_or("").trim(), status.unwrap_or("").trim());
    if !check.is_empty() && status.is_empty() {
        bail!("tower validate <pr> {check} passed|failed|not-run --evidence \"<what shows it>\"");
    }
    if !check.is_empty() && evidence.is_none_or(|e| e.trim().is_empty()) {
        bail!("give its evidence: --evidence \"<the test output, a screenshot's path, a log, or why it wasn't run>\"");
    }
    let r = protocol::send(&Request::Validate {
        pr: pr.trim().to_string(),
        check: check.to_string(),
        status: status.to_string(),
        evidence: evidence.unwrap_or("").to_string(),
        from: peers::caller(),
    })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if as_json || !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    println!("{}: {}", v["pr"].as_str().unwrap_or(""), v["line"].as_str().unwrap_or("no planned checks"));
    for c in v["validation"]["checks"].as_array().into_iter().flatten() {
        println!("  {} {}: {}", c["id"].as_str().unwrap_or(""), c["status"].as_str().unwrap_or(""), c["what"].as_str().unwrap_or(""));
    }
    Ok(())
}

/// `tower automerge on|off|status`: `[prs] auto_merge_low_risk` in config.toml,
/// which the daemon reads as it goes (no restart).
fn automerge_cmd(action: &str) -> Result<()> {
    let path = config::config_file();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let now = config::Config::from_text(&text)?.prs.auto_merge_low_risk;
    let on = match action.trim() {
        "on" => true,
        "off" => false,
        "status" | "" => now,
        _ => bail!("tower automerge on|off|status"),
    };
    if on != now {
        let new = config::with_table_key(&text, "prs", "auto_merge_low_risk", if on { "true" } else { "false" });
        // Kept only once it reads back as asked.
        if config::Config::from_text(&new)?.prs.auto_merge_low_risk != on {
            bail!("couldn't set [prs] auto_merge_low_risk in {}", path.display());
        }
        std::fs::create_dir_all(config::config_dir())?;
        std::fs::write(&path, new)?;
    }
    if !std::io::stdout().is_terminal() {
        println!("{}", serde_json::json!({ "auto_merge_low_risk": on, "changed": on != now }));
    } else if on {
        println!("Auto-merge is on: the tower merges a low-risk PR into dev itself once a plan predicted it, its checks pass and its reviewer found nothing. Anything that needs review, and every merge into main or prod, is still yours.");
    } else {
        println!("Auto-merge is off: every PR is yours to merge.");
    }
    Ok(())
}

fn tell_cmd(level: Option<&str>, voice: Option<&str>) -> Result<()> {
    let level = level.map(tell::parse).transpose()?;
    let voice = voice.map(tell::parse_voice).transpose()?;
    let (level, voice) = if protocol::is_running() {
        // The voice keeps them, shows them, and has the brain read its instructions again.
        let r = protocol::send(&Request::Tell { level: level.map(|l| l.word().into()), voice: voice.map(|v| v.word().into()) })?;
        if !r.ok {
            bail!("{}", r.error.unwrap_or_default());
        }
        let v = r.state.unwrap_or_default();
        (
            v["level"].as_str().and_then(tell::Level::parse).unwrap_or_else(tell::level),
            v["voice"].as_str().and_then(tell::Voice::parse).unwrap_or_else(tell::voice),
        )
    } else {
        // The voice isn't running: kept for when it starts.
        if let Some(l) = level {
            tell::set(l)?;
        }
        if let Some(v) = voice {
            tell::set_voice(v)?;
        }
        (level.unwrap_or_else(tell::level), voice.unwrap_or_else(tell::voice))
    };
    if std::io::stdout().is_terminal() {
        println!("{level}. {}", level.says());
        println!("Voice: {voice}. {}", voice.says());
    } else {
        print_json(&json!({ "level": level.word(), "voice": voice.word() }));
    }
    Ok(())
}

/// `tower undo [<id>] [--force]`: the daemon ends what that act opened.
fn undo_cmd(id: Option<&str>, force: bool, as_json: bool) -> Result<()> {
    let r = protocol::send(&Request::Act { action: "undo".into(), id: id.map(String::from), force, act: None, from: peers::caller() })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if as_json || !std::io::stdout().is_terminal() {
        print_json(&v);
    } else {
        println!("{} ({}: {})", v["closing"].as_str().unwrap_or("Undone."), v["act"]["id"].as_str().unwrap_or(""), v["act"]["line"].as_str().unwrap_or(""));
    }
    Ok(())
}

/// `tower acts`: what the tower opened lately.
fn acts_cmd(as_json: bool) -> Result<()> {
    let r = protocol::send(&Request::Act { action: "list".into(), id: None, force: false, act: None, from: None })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if as_json || !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    let list = v["acts"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("Nothing was opened lately.");
    }
    for a in list {
        let how = match (a["state"].as_str(), a["undo"]["possible"].as_bool()) {
            (Some("undone"), _) => "undone".to_string(),
            (_, Some(true)) => "can be undone".to_string(),
            _ => format!("undo needs --force: {}", a["undo"]["why"].as_str().unwrap_or("")),
        };
        let assumed: Vec<String> = a["assumed"].as_array().into_iter().flatten().map(|c| format!("{} {} ({})", c["what"].as_str().unwrap_or(""), c["value"].as_str().unwrap_or(""), c["why"].as_str().unwrap_or(""))).collect();
        println!("{}  {}  [{how}]", a["id"].as_str().unwrap_or(""), a["line"].as_str().unwrap_or(""));
        if !assumed.is_empty() {
            println!("    assumed: {}", assumed.join("; "));
        }
    }
    Ok(())
}

fn ok(req: Request) -> Result<()> {
    let r = protocol::send(&req)?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    Ok(())
}

/// An id as `tower queue list` prints it (`#3` or `3`).
fn queue_id(s: &str) -> Result<u64> {
    s.trim_start_matches('#').parse().map_err(|_| anyhow::anyhow!("{s} isn't an id (tower queue list shows them)"))
}

/// `tower decisions`: what only the user can decide, in queue order.
/// `tower hold` and `tower unhold`.
fn hold_cmd(id: &str, words: &str, unhold: bool) -> Result<()> {
    let r = protocol::send(&Request::Hold { id: id.trim().into(), reason: words.trim().into(), unhold, by: String::new(), from: peers::caller() })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    let title = v["title"].as_str().unwrap_or("");
    if unhold {
        println!("{id} waits for you again: {title}. It is counted and said once.");
    } else {
        let reason = v["hold"]["reason"].as_str().filter(|r| !r.is_empty()).map(|r| format!(" (\"{r}\")")).unwrap_or_default();
        let until: Vec<&str> = v["hold"]["until_prs"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        println!(
            "{id} is on hold{reason}: {title}. Not counted or said until it is updated{}, or `tower unhold {id}`.",
            if until.is_empty() { String::new() } else { format!(" or {} merges", until.join(" or ")) }
        );
    }
    Ok(())
}

fn decisions_cmd(space: Option<String>, as_json: bool, all: bool) -> Result<()> {
    let r = protocol::send(&Request::Decisions { space, all })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if as_json || !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    let rows = |key: &str| -> Vec<decisions::Decision> { v[key].as_array().into_iter().flatten().filter_map(|d| serde_json::from_value(d.clone()).ok()).collect() };
    let list = rows("decisions");
    if list.is_empty() {
        match v["space"].as_str() {
            Some(s) => println!("Nothing needs you in {s}."),
            None => println!("Nothing needs you."),
        }
    }
    for d in &list {
        println!("{}", d.row());
        for (i, o) in d.options.iter().enumerate() {
            println!("      {i}. {}{}{}", o.label, if o.pick { " (the tower's pick)" } else { "" }, if o.why.is_empty() { String::new() } else { format!(": {}", o.why) });
        }
    }
    if !list.is_empty() {
        println!("(tower answer <id> --choice <n> | --other \"<words>\"; tower dismiss <id>; tower hold <id> --reason \"<words>\"; a merge is its PR's Merge button)");
    }
    let held = rows("on_hold");
    if !held.is_empty() {
        println!("On hold (not counted, not said):");
        for d in &held {
            let reason = d.hold.as_ref().map(|h| if h.reason.is_empty() { "no reason given".to_string() } else { format!("\"{}\"", h.reason) }).unwrap_or_default();
            println!("{}  {:<10} {}  {}  [{reason}]", d.id, d.kind.word(), phone::ago(d.hold.as_ref().map(|h| h.since_ms).unwrap_or(d.last_ms)), d.title);
        }
        println!("(tower unhold <id>: it waits for you again)");
    }
    let closed = rows("closed");
    if !closed.is_empty() {
        println!("Closed lately:");
        for d in &closed {
            println!("{}  {:<10} {}  {}", d.id, d.state.word(), phone::ago(d.closed_ms.unwrap_or(d.last_ms)), d.title);
        }
    }
    Ok(())
}

fn queue_cmd(args: &[String]) -> Result<()> {
    let q = |action: &str, session: &str, text: String, id: Option<u64>, after: Vec<u64>| Request::Queue {
        action: action.into(),
        session: session.into(),
        text,
        id,
        after,
        start: vec![],
        tag: None,
        from: None,
    };
    let usage = "usage: tower queue <session> [--after <id>[,<id>]] [--agenda <id>] \"<instruction>\" | list | cancel <id> | retarget <id> <session>";
    let req = match args.first().map(String::as_str) {
        Some("list") if args.len() == 1 => q("list", "", String::new(), None, vec![]),
        Some("cancel") if args.len() == 2 => q("cancel", "", String::new(), Some(queue_id(&args[1])?), vec![]),
        Some("retarget") if args.len() == 3 => q("retarget", &args[2], String::new(), Some(queue_id(&args[1])?), vec![]),
        Some(session) if args.len() >= 2 => {
            let mut rest = &args[1..];
            let mut after = vec![];
            let mut agenda_id = None;
            loop {
                match rest.first().map(String::as_str) {
                    Some("--agenda") if rest.len() >= 3 => {
                        agenda_id = Some(rest[1].clone());
                        rest = &rest[2..];
                    }
                    Some("--after") if rest.len() >= 3 => {
                        for id in rest[1].split(',').filter(|s| !s.is_empty()) {
                            after.push(queue_id(id)?);
                        }
                        rest = &rest[2..];
                    }
                    // One task, though it reads like several (the brain's guard lets it through).
                    Some("--whole") => rest = &rest[1..],
                    _ => break,
                }
            }
            let mut req = q("add", session, rest.join(" "), None, after);
            if let Request::Queue { tag, from, .. } = &mut req {
                *tag = Some(agenda::Tag { agenda: agenda_id, ..Default::default() });
                // Who is asking: the terminal this runs in, when it runs in Herdr.
                *from = peers::caller();
            }
            req
        }
        _ => bail!("{usage}"),
    };
    let action = match &req {
        Request::Queue { action, .. } => action.clone(),
        _ => unreachable!(),
    };
    let r = protocol::send(&req)?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    match action.as_str() {
        // One session writing to another: delivered, and nothing more happens.
        "add" if v["peer"] == true => println!(
            "{} {} ({}): the tower put your name on it.{}",
            if v["sent"] == true { "Message sent to" } else { "Message queued for" },
            v["session"].as_str().unwrap_or(""),
            v["name"].as_str().unwrap_or(""),
            if v["while_working"] == true {
                " It was typed in while that session works: it reads it when its turn ends."
            } else if v["sent"] == true {
                ""
            } else {
                " It goes in when that session stops working (or, once a Claude Code session has worked a long while, is typed in to be read when its turn ends); you are told only if it can't be delivered."
            }
        ),
        "add" if v["sent"] == true => println!(
            "Sent #{} to {} ({}): it was free. Watching it: you'll get a [watch] message when it stops working.{}",
            v["id"],
            v["session"].as_str().unwrap_or(""),
            v["name"].as_str().unwrap_or(""),
            agenda_said(&v)
        ),
        "retarget" => println!("#{} now waits for {}.", v["retargeted"]["id"], v["retargeted"]["session"].as_str().unwrap_or("")),
        "add" => println!(
            "Queued #{} for {} ({}, {}): position {}. It goes in when the session stops working and is watched then; the brain hears \"[queue] delivered to {}\".{}",
            v["id"],
            v["session"].as_str().unwrap_or(""),
            v["name"].as_str().unwrap_or(""),
            v["status"].as_str().unwrap_or(""),
            v["position"],
            v["session"].as_str().unwrap_or(""),
            agenda_said(&v)
        ),
        "cancel" => println!("Cancelled #{}: {}", v["cancelled"]["id"], v["cancelled"]["text"].as_str().unwrap_or("")),
        _ => {
            let items: Vec<daemon::sendq::Item> = serde_json::from_value(v["items"].clone()).unwrap_or_default();
            if items.is_empty() {
                println!("Nothing queued.");
            }
            let status = |s: &str| hc::get(s).filter(|t| t["state"] == "live").map(|t| hc::status_word(&t).unwrap_or_default());
            for line in daemon::sendq::describe(&items, status) {
                println!("{line}");
            }
        }
    }
    Ok(())
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// Every agent, here and on each machine, in one list.
fn agents(all: bool) -> Result<()> {
    let (rows, errors) = agent_rows(all);
    if std::io::stdout().is_terminal() {
        for r in &rows {
            println!(
                "{:<12} {:<9} {:<12} {:<10} {}  {}",
                r["address"].as_str().unwrap_or(""),
                r["status"].as_str().unwrap_or("-"),
                r["agent"].as_str().unwrap_or(""),
                r["account"].as_str().unwrap_or("-"),
                r["folder"].as_str().unwrap_or(""),
                r["title"].as_str().unwrap_or("")
            );
            if let Some(line) = r["job"]["line"].as_str() {
                println!("{:<12} {line}", "");
            }
            for b in r["browsers"].as_array().into_iter().flatten() {
                if let Some(line) = b["line"].as_str() {
                    println!("{:<12} {line}", "");
                }
            }
        }
        for e in &errors {
            println!("! {}: {}", e["machine"].as_str().unwrap_or(""), e["error"].as_str().unwrap_or(""));
        }
    } else {
        print_json(&json!({ "agents": rows, "unreachable": errors }));
    }
    Ok(())
}

/// `tower agents` as JSON (the phone API serves it too).
pub fn agents_value(all: bool) -> Value {
    let (rows, errors) = agent_rows(all);
    json!({ "agents": rows, "unreachable": errors })
}

fn agent_rows(all: bool) -> (Vec<Value>, Vec<Value>) {
    // The jobs and servers sessions left running, as the daemon last saw them.
    let state: Value = std::fs::read_to_string(config::ui_dir().join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let running = |address: &str| -> (Vec<running::Job>, Value) {
        let a = state["agents"].as_array().into_iter().flatten().find(|a| a["address"] == address);
        let jobs: Vec<running::Job> = a.and_then(|a| serde_json::from_value(a["jobs"].clone()).ok()).unwrap_or_default();
        (jobs, a.map(|a| a["servers"].clone()).filter(|s| s.is_array()).unwrap_or(json!([])))
    };
    let browsers = |address: &str| -> Value {
        state["agents"].as_array().into_iter().flatten().find(|a| a["address"] == address).map(|a| a["browsers"].clone()).filter(|b| b.is_array()).unwrap_or(json!([]))
    };
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut lists = vec![(None, hc::list_on(None))];
    for m in hc::machines() {
        let l = hc::list_on(Some(&m));
        lists.push((Some(m), l));
    }
    for (machine, list) in lists {
        match list {
            Ok(list) => {
                for t in list {
                    let status = hc::status_word(&t);
                    let name = t["name"].as_str().unwrap_or("");
                    // The tower's own terminals (its brains, its voice) are no agents to steer.
                    if name == brain::VOICE_NAME || (machine.is_none() && brain::is_brain_name(name)) {
                        continue;
                    }
                    if status.is_none() && !all {
                        continue;
                    }
                    // `hc ls` has the reporting app (or else the program) as `agent`.
                    let app = hc::agent_app(&t).or_else(|| t["agent"].as_str().map(String::from));
                    let account = if machine.is_none() { accounts::of_address(t["address"].as_str().unwrap_or("")) } else { None };
                    let (jobs, servers) = if machine.is_none() { running(t["address"].as_str().unwrap_or("")) } else { (vec![], json!([])) };
                    rows.push(json!({
                        "address": t["address"],
                        "account": account,
                        "machine": machine.clone().unwrap_or_else(|| "this mac".into()),
                        "name": hc::name(&t),
                        "spoken": hc::spoken_name(&t),
                        "agent": app.or_else(|| t["foreground"]["argv"][0].as_str().map(String::from)),
                        "status": status,
                        "doing": t["status"]["msg"],
                        "title": t["title"],
                        "folder": t["foreground"]["cwd"].as_str().or(t["cwd"].as_str()),
                        "where": t["labels"].get("where").unwrap_or(&t["where"]),
                        // Its turn may be done while a job it started runs: shown, with every job and server.
                        "job": running::shown(&jobs, history::now_ms()),
                        "jobs": jobs,
                        "servers": servers,
                        // A browser it uses now: Chrome, the built-in one, a headless one (the latest first).
                        "browsers": if machine.is_none() { browsers(t["address"].as_str().unwrap_or("")) } else { json!([]) },
                    }));
                }
            }
            Err(e) => errors.push(json!({ "machine": machine, "error": format!("{e:#}") })),
        }
    }
    (rows, errors)
}

/// `tower start`: a Claude Code session on an account, through `cca <account>`.
#[allow(clippy::too_many_arguments)]
fn start(
    account: Option<&str>,
    name: &str,
    cwd: Option<String>,
    on: Option<&str>,
    space: Option<&spaces::Asked>,
    no_watch: bool,
    tag: Option<agenda::Tag>,
    task: &str,
    no_worktree: bool,
    keep_act: bool,
) -> Result<()> {
    // `tower` and `tower-…` are the towers' own terminals: a session called so would be taken for one.
    if let Some(why) = brain::not_a_sessions_name(name) {
        bail!("{why}");
    }
    // What it settles by itself, and why: it asks nothing, and says so afterwards (`assumed`).
    let asked_account = account.map(str::trim).filter(|a| !a.is_empty()).map(String::from);
    // The space first: it may say which machine the session runs on. One
    // that doesn't exist yet is made below; of several, the closest is taken.
    let spaces = if space.is_some() { spaces::windows() } else { vec![] };
    let mut notes: Vec<String> = vec![];
    let mut space_why = "the one the window shows: none was named".to_string();
    let (target, new_space) = match space {
        Some(want) => match spaces::best_window(want, &spaces) {
            spaces::Placing::One(w) | spaces::Placing::Closest(w, _) if want.new && !w.name.eq_ignore_ascii_case(want.name.trim()) => {
                space_why = "made: a new space was asked for".into();
                (None, Some(want.name.trim().to_string()))
            }
            spaces::Placing::One(w) => {
                space_why = if w.name.eq_ignore_ascii_case(want.name.trim()) { "as asked".into() } else { format!("the one \"{}\" names", want.name.trim()) };
                (Some(w.clone()), None)
            }
            spaces::Placing::Closest(w, others) => {
                notes.push(format!("\"{}\" matched several spaces ({}): took \"{}\"", want.name, others.join(", "), w.name));
                space_why = format!("the closest to \"{}\" (it also matched {})", want.name.trim(), others.join(", "));
                (Some(w.clone()), None)
            }
            spaces::Placing::New(name) => {
                space_why = "made: no space had that name".into();
                (None, Some(name))
            }
        },
        None => (None, None),
    };
    let machine: Option<String> = match (on, target.as_ref().and_then(|t| t.machine.clone())) {
        (Some(m), Some(sm)) if m != sm => bail!("space \"{}\" runs on {sm}, not {m}", target.as_ref().map(|t| t.name.as_str()).unwrap_or("")),
        (Some(m), _) => Some(m.to_string()),
        (None, sm) => sm,
    };
    let on = machine.as_deref();
    let (known, cca) = match on {
        None => (accounts::list()?, accounts::cca_bin().map(|p| p.to_string_lossy().into_owned())),
        Some(m) => {
            let l = accounts::list_on(m);
            let cca = if l.is_empty() { None } else { Some("cca".to_string()) };
            (l, cca)
        }
    };
    let account = accounts::resolve(account, &known)?;
    let account_why = match (&account, &asked_account) {
        (None, _) => "none: no accounts are known here, so it runs plain claude".to_string(),
        (Some(a), Some(asked)) if a.eq_ignore_ascii_case(asked) => "as asked".to_string(),
        (Some(_), Some(asked)) => format!("what \"{asked}\" names"),
        (Some(_), None) => "the default one: none was named".to_string(),
    };
    let mut folder_why = match (&cwd, on) {
        (Some(_), _) => "as asked".to_string(),
        (None, Some(m)) => format!("{m}'s home: none was named"),
        (None, None) => "the folder the command ran in: none was named".to_string(),
    };
    // `~` in --cwd, as a shell would have it.
    let cwd = cwd.map(|c| match c.strip_prefix("~/") {
        Some(rest) if on.is_none() => dirs::home_dir().unwrap_or_default().join(rest).to_string_lossy().into_owned(),
        _ => c,
    });
    // A folder that isn't there is a project's name ("sandbox", or a guessed
    // path): take the best-matching project folder and say so, never stop.
    let mut chosen: Option<(String, Vec<String>)> = None;
    let cwd = match cwd {
        Some(c) if on.is_none() && !std::path::Path::new(&c).is_dir() => match projects::best(&c) {
            Some((m, others)) => {
                let others: Vec<String> = others.into_iter().map(|o| o.folder).collect();
                notes.push(format!(
                    "no folder {c}: took {}, the best match{}",
                    m.folder,
                    if others.is_empty() { String::new() } else { format!(" (also close: {})", others.join(", ")) }
                ));
                folder_why = format!("the best match for \"{c}\"{}", if others.is_empty() { String::new() } else { format!(" (also close: {})", others.join(", ")) });
                chosen = Some((m.folder.clone(), others));
                Some(m.path.to_string_lossy().into_owned())
            }
            None => bail!("no folder {c}, and no project folder fits that name under {}", projects::roots().iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")),
        },
        c => c,
    };
    // A session opened for a mission (a space with a goal, or one `tower
    // launch` made, that isn't standing) works in that space's git worktree
    // of its repo, with a dev port of its own (see `worktrees`). Not in a
    // space with no goal, not on another machine, not in a folder that is no
    // git checkout.
    let space_name = target.as_ref().map(|t| t.name.clone()).or_else(|| new_space.clone());
    let project = cwd.clone();
    let mut made: Option<worktrees::Made> = None;
    let mut port: Option<u16> = None;
    let mut cwd = cwd;
    if let Some(sp) = space_name.as_deref().filter(|_| on.is_none() && !no_worktree) {
        let rules = config::Config::load().map(|c| c.spaces).unwrap_or_default();
        let folder = cwd.clone().or_else(|| target.as_ref().map(|t| t.folder.clone()).filter(|f| !f.is_empty()));
        if let Some(folder) = folder.filter(|_| rules.worktrees && missions::is_mission(sp)) {
            match worktrees::ensure(std::path::Path::new(&folder), sp) {
                Ok(Some(m)) => {
                    port = worktrees::pick_port(&rules.port_range, &worktrees::handed_ports());
                    notes.push(format!(
                        "it works in {} worktree of {} at {} (branch {}){}",
                        if m.fresh { "a fresh" } else { "the space's" },
                        m.place.repo,
                        m.place.path,
                        m.place.branch,
                        port.map(|p| format!(", with port {p} for a dev server")).unwrap_or_default()
                    ));
                    // Remembered with the space before the session opens: closing the space finds it either way.
                    if worktrees::place(std::path::Path::new(&m.place.repo), sp).path == m.place.path {
                        missions::opened(sp, Some(&m.place));
                    }
                    cwd = Some(m.cwd.clone());
                    made = Some(m);
                }
                Ok(None) => {}
                Err(e) => notes.push(format!("no worktree ({e:#}): it works in {folder} itself")),
            }
        }
    }
    // Where it works and its port are said after its task.
    let first = match (made.as_ref().map(|m| worktrees::briefing(m, port)), task.trim().is_empty()) {
        (Some(b), false) => format!("{task}\n\n{b}"),
        (Some(b), true) => b,
        (None, _) => task.to_string(),
    };
    let cca = cca.unwrap_or_else(|| "cca".into());
    let how = accounts::Start { name, cwd: cwd.as_deref(), on, task: &first, account: account.as_deref(), cca: &cca };
    let (address, placed) = match (&target, &new_space) {
        (Some(sp), _) => (start_in_space(sp, &how, &spaces)?, Some(sp.name.clone())),
        (None, Some(name)) => {
            notes.push(format!("made the space \"{name}\""));
            // In a worktree: the space opens on a home tab beside its worktrees, so
            // Herdr shows it as a space of its own, never nested under the repo's.
            let home = made.as_ref().filter(|_| on.is_none()).and_then(|m| worktrees::home_of(&m.place.path));
            let address = match home {
                Some(home) => new_space_at_home(name, &home, &how, &spaces)?,
                None => new_space_with(name, &how, &spaces)?,
            };
            (address, Some(name.clone()))
        }
        (None, None) => {
            let args = accounts::hc_run_args(&how);
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = hc::run(&refs)?;
            if out.code != 0 {
                bail!("hc run: {}{}", out.stderr.trim(), out.stdout.trim());
            }
            let v: Value = serde_json::from_str(out.stdout.trim()).unwrap_or_default();
            let address = v["address"].as_str().or_else(|| v["terminal"]["address"].as_str()).map(String::from).unwrap_or_else(|| name.to_string());
            let address = match on {
                Some(m) if !address.contains('/') => format!("{m}/{address}"),
                _ => address,
            };
            // The voice puts it in a tab of the space the window shows; say which.
            let placed = placed_space(&address);
            (address, placed)
        }
    };
    // What it was given stays on its terminal, and its space is a mission the tower knows (open again, if it was closed).
    worktrees::label(&address, made.as_ref().map(|m| &m.place), port);
    if space.is_some()
        && let Some(sp) = &placed
    {
        missions::opened(sp, None);
    }
    let on_account = account.as_deref().map(|a| format!(" on the {a} account")).unwrap_or_default();
    let in_space = match &placed {
        Some(sp) => format!(" in space \"{sp}\""),
        None => " (not in a space yet: no Herdr window, or the voice isn't running)".into(),
    };
    let watched = !no_watch
        && protocol::send(&Request::Watch { terminal: address.clone(), note: task.to_string(), account: account.clone() })
            .is_ok_and(|r| r.ok);
    if !watched && !task.is_empty() {
        // No voice running (or no watch wanted): keep the job in the history all the same.
        history::job_start_on(&address, task, account.clone());
    }
    // Into the agenda ledger, with an id later instructions can wait for (`after`).
    let tracked = match tag {
        Some(mut tag) if !task.trim().is_empty() && protocol::is_running() => {
            tag.space = placed.clone();
            // The project is the repo, not the space's worktree of it.
            tag.project = project.clone().or_else(|| cwd.clone()).unwrap_or_default();
            protocol::send(&Request::Queue {
                action: "track".into(),
                session: address.clone(),
                text: task.to_string(),
                id: None,
                after: vec![],
                start: vec![],
                tag: Some(tag),
                from: None,
            })
            .ok()
            .filter(|r| r.ok)
            .and_then(|r| r.state)
        }
        _ => None,
    };
    let tracked = tracked.unwrap_or_default();
    // A new session in a space with others: they're introduced to each other (once it settles).
    if protocol::is_running() {
        let _ = protocol::send(&Request::Introduce { action: "kick".into(), space: String::new(), dry_run: false });
    }
    let given = made.as_ref().map(|m| m.place.clone());
    // What was settled without asking, for the receipt and for `tower undo`.
    let folder = project.clone().or_else(|| on.is_none().then(|| std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned())).flatten());
    let assumed = json!({
        "account": account, "account_why": account_why,
        "model": "default model", "model_why": "none was asked for: a session starts on its account's model",
        "folder": folder, "folder_why": folder_why,
        "worktree": given.as_ref().map(|p| p.path.clone()),
        "worktree_why": match &made {
            Some(m) if m.fresh => format!("made for this space, on the branch {}", m.place.branch),
            Some(m) => format!("this space's worktree of the repo, which was there already (branch {})", m.place.branch),
            None => "none: it works in the folder itself".to_string(),
        },
        "space": placed, "space_why": if placed.is_some() { space_why } else { "none: no Herdr window shows one".to_string() },
        "name": name, "name_why": "as asked",
    });
    // Which terminal it is: an address can come back for another one.
    let made_ms = hc::get(&address).and_then(|t| hc::created_ms(&t));
    let act = match keep_act && protocol::is_running() {
        true => {
            let opened = acts::Opened {
                address: address.clone(),
                name: name.to_string(),
                created_ms: made_ms,
                worktree: given.clone(),
                fresh: made.as_ref().is_some_and(|m| m.fresh),
                first: first.clone(),
                task: tracked["task"].as_str().map(String::from),
                more: None,
            };
            let what = json!({ "kind": "start", "space": placed, "space_made": new_space.is_some(), "sessions": [opened], "assumed": assumed });
            protocol::send(&Request::Act { action: "record".into(), id: None, force: false, act: Some(what), from: peers::caller() })
                .ok()
                .filter(|r| r.ok)
                .and_then(|r| r.state)
                .and_then(|v| v["id"].as_str().map(String::from))
        }
        false => None,
    };
    if std::io::stdout().is_terminal() {
        println!(
            "Started {name} as {address}{in_space}{on_account}.{}{}{}{}",
            notes.iter().map(|n| format!(" {}{}.", n[..1].to_uppercase(), &n[1..])).collect::<String>(),
            if watched { " Watching it: you'll get a [watch] message when it stops working. End your turn now." } else { "" },
            agenda_said(&tracked),
            act.as_deref().map(|id| format!(" To take it back: tower undo {id}.")).unwrap_or_default()
        );
    } else {
        let mut v = json!({
            "address": address, "name": name, "space": placed, "account": account, "watching": watched,
            "queue_id": tracked["id"], "agenda": tracked["agenda"], "task": tracked["task"],
            "assumed": assumed, "act": act,
        });
        if new_space.is_some() {
            v["space_made"] = json!(true);
        }
        if let Some(ms) = made_ms {
            v["created_ms"] = json!(ms);
        }
        if let Some(p) = &given {
            v["worktree"] = json!(p.path);
            v["branch"] = json!(p.branch);
            v["repo"] = json!(p.repo);
            v["worktree_fresh"] = json!(made.as_ref().is_some_and(|m| m.fresh));
        }
        if let Some(p) = port {
            v["port"] = json!(p);
        }
        if let Some((folder, others)) = chosen {
            v["folder"] = json!(folder);
            v["other_folders"] = json!(others);
        }
        if !notes.is_empty() {
            v["chose"] = json!(notes.join("; "));
        }
        print_json(&v);
    }
    Ok(())
}

/// " Agenda a3, task a3.2." when the answer says where its task went.
fn agenda_said(v: &Value) -> String {
    match (v["agenda"].as_str(), v["task"].as_str()) {
        (Some(a), Some(t)) => format!(" Agenda {a}, task {t}."),
        _ => String::new(),
    }
}

/// `tower job done|failed <t>`: the brain's word moves the session's agenda task too.
fn agenda_judged(terminal: &str, ok: bool, text: &str) {
    let session = hc::get(terminal).and_then(|t| t["address"].as_str().map(String::from)).unwrap_or_else(|| terminal.to_string());
    let cmd = agenda::Cmd { action: "judge".into(), session, ok, text: text.into(), ..Default::default() };
    let v = match agenda_do(&cmd) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("tower: the agenda ledger: {e:#}");
            return;
        }
    };
    if let Some(t) = v["task"].as_str() {
        println!("Agenda task {t} is {}.", if ok { "verified" } else { "failed" });
    }
    // It failed a moment ago and the tower sent it once more by itself: this word is about the try before.
    if let Some(t) = v["retried"].as_str() {
        println!("Agenda task {t} failed before, and the tower sent it once more by itself: it is on its way again. Nothing to do; if it fails again, the tower asks the user.");
    }
    // The request is finished: its one receipt is due (the brain's own words say it when the tower tells everything).
    if let Some(a) = v["agenda_done"].as_str().filter(|_| tell::level().quiet()) {
        println!("That was the last of {a}: the request is finished. Give its receipt once: tower receipt --agenda {a} \"<one line: what was done>\"");
    }
}

/// A `tower agenda` command: by the voice's ledger when it runs, else on the file.
fn agenda_do(cmd: &agenda::Cmd) -> Result<Value> {
    if protocol::is_running() {
        let r = protocol::send(&Request::Agenda { ledger: cmd.clone() })?;
        if !r.ok {
            bail!("{}", r.error.unwrap_or_default());
        }
        return Ok(r.state.unwrap_or_default());
    }
    if cmd.action == "requeue" {
        bail!("requeue needs the voice running (`tower up`): it goes through the send queue");
    }
    let path = agenda::path();
    let mut l = agenda::Ledger::load_from(&path);
    let acted = agenda::act(&mut l, cmd, &agenda::Heard::default(), history::now_ms())?;
    if acted.changed {
        l.save_to(&path)?;
    }
    let mut v = acted.value;
    if !acted.cancel.is_empty() {
        v["not_cancelled"] = json!(acted
            .cancel
            .iter()
            .map(|id| format!("#{id}: the voice isn't running; `tower queue cancel {id}` once it is"))
            .collect::<Vec<_>>());
    }
    Ok(v)
}

/// `tower reopen [list|go|forget] [<space>…] [--dry-run]`.
fn job_running(action: &str, what: &str, pid: Option<u32>, until: &str, minutes: Option<u64>, session: Option<&str>) -> Result<()> {
    // One look at a session now, without the daemon (it reads only).
    if action == "look" {
        print_json(&daemon::running::look_at(what.trim())?);
        return Ok(());
    }
    if !protocol::is_running() {
        bail!("the tower's voice isn't running (tower up): it keeps the jobs");
    }
    let terminal = session.map(String::from).or_else(peers::caller).unwrap_or_default();
    let r = protocol::send(&Request::JobRunning { terminal, action: action.into(), what: what.into(), pid, until: until.into(), minutes })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    print_json(&r.state.unwrap_or_default());
    Ok(())
}

fn failover_cmd(action: &str, target: &str, to: &str, json_out: bool) -> Result<()> {
    if !protocol::is_running() {
        bail!("the tower's voice isn't running (tower up): it moves sessions between accounts");
    }
    if action == "move" && target.trim().is_empty() {
        bail!("tower failover move <session> [--to <account>]");
    }
    let r = protocol::send(&Request::Failover { action: action.into(), target: target.into(), account: to.into() })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if json_out || !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    match action {
        "move" => println!("Moving {} from the {} account to {}: its conversation resumes in its own terminal.", s(&v, "moving"), s(&v, "from"), s(&v, "to")),
        "clear" => println!("Forgot {} account(s) out of usage and {} session(s) waiting for one.", v["forgot_out"], v["forgot_waiting"]),
        _ => {
            for a in v["accounts"].as_array().into_iter().flatten() {
                let out = match a["out_until"].as_str() {
                    Some(u) => format!("out of usage until {u}"),
                    None => "has usage".to_string(),
                };
                println!("{} ({}): {out}", s(a, "name"), s(a, "email"));
            }
            for w in v["waiting"].as_array().into_iter().flatten() {
                println!("Waiting for an account: {} ({}) on {}", s(w, "name"), s(w, "address"), s(w, "account"));
            }
            for m in v["moves"].as_array().into_iter().flatten().rev().take(10) {
                let when = phone::ago(m["at_ms"].as_u64().unwrap_or(0));
                match s(m, "how").as_str() {
                    "moved" => println!("{when}: {} ({}) moved from {} to {}", s(m, "name"), s(m, "address"), s(m, "from"), s(m, "to")),
                    _ => println!("{when}: {} ({}) couldn't move from {} to {}: {}", s(m, "name"), s(m, "address"), s(m, "from"), s(m, "to"), s(m, "why")),
                }
            }
        }
    }
    Ok(())
}

fn reopen_cmd(action: &str, spaces: &[String], dry_run: bool, json_out: bool) -> Result<()> {
    if !protocol::is_running() {
        bail!("the tower's voice isn't running (tower up): it keeps what was open");
    }
    let r = protocol::send(&Request::Reopen { action: action.into(), spaces: spaces.to_vec(), dry_run })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if json_out || !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    match action {
        "list" | "ls" | "" => {
            let lost = v["lost"].as_array().cloned().unwrap_or_default();
            if lost.is_empty() {
                println!("Nothing a restart closed is waiting: {} session(s) open now are kept in case one does.", v["open"]);
            }
            let mut last = String::new();
            for k in &lost {
                if s(k, "space") != last {
                    last = s(k, "space");
                    println!("{last}:");
                }
                println!("  {} ({}, {}), last seen {}", s(k, "name"), s(k, "app"), s(k, "folder"), phone::ago(k["seen_ms"].as_u64().unwrap_or(0)));
            }
            if !lost.is_empty() {
                println!("tower reopen go [<space>…] puts them back; tower reopen forget lets them go.");
            }
            for k in v["reopening"].as_array().into_iter().flatten() {
                println!("Reopening {} in {} ({}): not seen back yet.", s(k, "name"), s(k, "space"), s(k, "address"));
            }
        }
        _ if v["forgot"].is_number() => println!("{} {} session(s): the tower stops offering them.", if dry_run { "Would forget" } else { "Forgot" }, v["forgot"]),
        _ if dry_run => {
            for st in v["steps"].as_array().into_iter().flatten() {
                let step = &st["step"];
                let how = match step["how"].as_str().unwrap_or("") {
                    "typed" => format!("typed into its terminal {}: {}", s(step, "terminal"), s(step, "line")),
                    "tab" => format!("a new tab \"{}\" in its space, in {}", s(step, "tab"), s(step, "folder")),
                    "space" => format!("the space made again (in {}), its tab \"{}\"", s(step, "folder"), s(step, "tab")),
                    _ => format!("not: {}", s(step, "why")),
                };
                println!("{} in {}: {how}", s(st, "name"), s(st, "space"));
            }
            if v["brain"].is_string() {
                println!("The tower's own brain isn't running: it would start again.");
            }
            println!("(dry run: nothing opened)");
        }
        _ => {
            for d in v["reopened"].as_array().into_iter().flatten() {
                match s(d, "how").as_str() {
                    "failed" => println!("{} in {}: not reopened: {}", s(d, "name"), s(d, "space"), s(d, "why")),
                    how => println!("{} in {}: resumed as {} ({how}).", s(d, "name"), s(d, "space"), s(d, "address")),
                }
            }
            if v["brain"].is_string() {
                println!("The tower's own brain is starting again.");
            }
        }
    }
    Ok(())
}

/// `tower introduce [<space>] [--dry-run]`.
fn introduce_cmd(space: &str, dry_run: bool) -> Result<()> {
    if !protocol::is_running() {
        bail!("the tower's voice isn't running (tower up): it keeps what was introduced");
    }
    let action = if space.trim().is_empty() { "status" } else { "run" };
    let r = protocol::send(&Request::Introduce { action: action.into(), space: space.trim().into(), dry_run })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    if !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    if action == "status" {
        if v["enabled"] == false {
            println!("Introducing sessions is off ([sessions] introduce = false in config.toml).");
        }
        let spaces = v["spaces"].as_array().cloned().unwrap_or_default();
        if spaces.is_empty() {
            println!("No Claude sessions in any space.");
        }
        for sp in spaces {
            let names: Vec<String> = sp["sessions"]
                .as_array()
                .map(|a| a.iter().map(|m| format!("{} ({})", m["name"].as_str().unwrap_or(""), m["address"].as_str().unwrap_or(""))).collect())
                .unwrap_or_default();
            let when = match sp["introduced_ms"].as_u64() {
                Some(t) if t > 0 => format!("introduced {}", phone::ago(t)),
                _ => "never introduced".into(),
            };
            let note = if names.len() < 2 { "alone".to_string() } else if sp["up_to_date"] == true { when } else { format!("{when}; changed since") };
            println!("{}: {} ({note})", sp["space"].as_str().unwrap_or(""), names.join(", "));
        }
        return Ok(());
    }
    if dry_run {
        for m in v["messages"].as_array().cloned().unwrap_or_default() {
            println!("── to {} ({}):\n{}\n", m["name"].as_str().unwrap_or(""), m["to"].as_str().unwrap_or(""), m["text"].as_str().unwrap_or(""));
        }
        println!("(dry run: nothing sent; `tower introduce {}` sends it)", v["space"].as_str().unwrap_or(space));
        return Ok(());
    }
    for m in v["sent"].as_array().cloned().unwrap_or_default() {
        match m["error"].as_str() {
            Some(e) => println!("{} ({}): not queued: {e}", m["name"].as_str().unwrap_or(""), m["to"].as_str().unwrap_or("")),
            None => println!(
                "{} ({}): {} (#{})",
                m["name"].as_str().unwrap_or(""),
                m["to"].as_str().unwrap_or(""),
                if m["sent"] == true { "sent" } else { "queued until it's free" },
                m["queue_id"]
            ),
        }
    }
    Ok(())
}

/// `tower agenda …`.
fn agenda_cmd(action: &str, args: &[String], all: bool, force: bool, title: Option<String>, space: Vec<String>) -> Result<()> {
    let action = match action {
        "ls" => "list",
        "open" => "new",
        "drop" => "abandon",
        a => a,
    };
    let mut c = agenda::Cmd { action: action.into(), all, force, title: title.unwrap_or_default(), spaces: space, ..Default::default() };
    let need = |what: &str| -> Result<String> {
        match args.first() {
            Some(t) => Ok(t.clone()),
            None => bail!("tower agenda {action} <{what}> (`tower agenda` lists them)"),
        }
    };
    match action {
        "list" | "tasks" => {}
        "show" | "close" | "abandon" | "verify" | "fail" | "forget" => {
            c.target = need("agenda or task")?;
            c.text = args[1..].join(" ");
        }
        "requeue" => {
            c.target = need("task")?;
            c.session = args.get(1).cloned().unwrap_or_default();
            c.text = args.get(2..).map(|r| r.join(" ")).unwrap_or_default();
        }
        "new" => c.text = args.join(" "),
        other => bail!("tower agenda {other}: use list, tasks, show, new, close, abandon, verify, fail, requeue or forget"),
    }
    let v = agenda_do(&c)?;
    if !std::io::stdout().is_terminal() {
        print_json(&v);
        return Ok(());
    }
    let agendas = |key: &str| -> Vec<agenda::Agenda> { serde_json::from_value(v[key].clone()).unwrap_or_default() };
    match action {
        "list" => {
            let list = agendas("agendas");
            if list.is_empty() {
                println!("{}", if all { "No agendas yet." } else { "No open agendas: everything the user asked for is done, closed or dropped." });
            }
            for a in &list {
                print!("{}", agenda::render(a, false));
            }
        }
        "tasks" => {
            let rows = v["tasks"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("Nothing unfinished.");
            }
            for r in rows {
                let t: agenda::Task = serde_json::from_value(r["task"].clone()).unwrap_or_default();
                let why = match t.state {
                    agenda::TaskState::Blocked => t.question.clone(),
                    agenda::TaskState::DoneClaimed => format!("said: {}", t.said),
                    _ => t.why.clone(),
                };
                println!(
                    "{} {:<12} {} on {} ({}): \"{}\"{}",
                    t.id,
                    t.state.word(),
                    t.title,
                    if t.name.is_empty() { t.session.clone() } else { format!("{} {}", t.name, t.session) },
                    r["agenda"].as_str().unwrap_or(""),
                    agenda::clip(r["words"].as_str().unwrap_or(""), 80),
                    if why.is_empty() { String::new() } else { format!(": {}", agenda::clip(&why, 140)) }
                );
            }
        }
        "show" => match serde_json::from_value::<agenda::Agenda>(v["agenda"].clone()) {
            Ok(a) => print!("{}", agenda::render(&a, true)),
            Err(_) => println!("Nothing to show."),
        },
        "new" => println!("Opened {}: \"{}\". Hand out its tasks with --agenda {}.", v["id"].as_str().unwrap_or(""), v["words"].as_str().unwrap_or(""), v["id"].as_str().unwrap_or("")),
        "close" => {
            println!("Closed {}.", v["closed"].as_str().unwrap_or(""));
            list_line("Abandoned with it", &v["abandoned"]);
            list_line("Cancelled in the queue", &v["cancelled"]);
        }
        "abandon" => {
            list_line("Abandoned", &v["abandoned"]);
            list_line("Cancelled in the queue", &v["cancelled"]);
        }
        "verify" | "fail" => list_line(if action == "verify" { "Verified" } else { "Failed" }, &v["moved"]),
        "forget" => println!("Forgot {}: it is out of the ledger (the record keeps what was said).", v["forgotten"].as_str().unwrap_or("")),
        "requeue" => println!(
            "{} {} as #{} for {} ({}).",
            v["task"].as_str().unwrap_or(""),
            if v["sent"] == true { "sent again" } else { "queued again" },
            v["queue_id"],
            v["session"].as_str().unwrap_or(""),
            v["name"].as_str().unwrap_or("")
        ),
        _ => print_json(&v),
    }
    if let Some(left) = v["not_cancelled"].as_array() {
        for l in left {
            println!("Not cancelled: {}", l.as_str().unwrap_or(""));
        }
    }
    Ok(())
}

fn list_line(what: &str, v: &Value) {
    let items: Vec<String> = v.as_array().map(|a| a.iter().map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect()).unwrap_or_default();
    if !items.is_empty() {
        println!("{what}: {}.", items.join(", "));
    }
}

/// A new tab in `sp` running the session (Herdr's `tab.new`), and the window
/// left on the space it showed. Returns the terminal's address.
fn start_in_space(sp: &spaces::Window, how: &accounts::Start, all: &[spaces::Window]) -> Result<String> {
    let argv = serde_json::to_string(&accounts::command(how))?;
    let folder = how.cwd.map(String::from).unwrap_or_else(|| sp.folder.clone());
    let mut args = vec!["do", "tab.new", "--space", &sp.id, "--name", how.name, "--argv", &argv];
    if !folder.is_empty() {
        args.extend(["--folder", &folder]);
    }
    let out = match std::process::Command::new(config::herdr_app_bin()).args(&args).stdin(std::process::Stdio::null()).output() {
        Ok(o) => o,
        Err(e) => bail!("herdr-app: {e}"),
    };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let Some(terminal) = v["terminal"].as_str() else {
        bail!("herdr-app tab.new: {}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim());
    };
    let address = match &sp.machine {
        Some(m) if !terminal.contains('/') => format!("{m}/{terminal}"),
        _ => terminal.to_string(),
    };
    // A tab's terminal has no hc name: the tower's own label stands in (see `hc::name`).
    let label = format!("tower.name={}", how.name);
    let _ = hc::run(&["label", &address, &label]);
    // tab.new shows the new tab; give the window back to the space it showed.
    if let Some(was) = all.iter().find(|s| s.active)
        && was.id != sp.id
    {
        let _ = std::process::Command::new(config::herdr_app_bin()).args(["do", "space.select", "--id", &was.id]).output();
    }
    Ok(address)
}

/// A new space called `name`, its first tab the session (as `tab.new` would
/// make it in a space that exists): its address.
fn new_space_with(name: &str, how: &accounts::Start, all: &[spaces::Window]) -> Result<String> {
    let argv = serde_json::to_string(&accounts::command(how))?;
    let folder = how.cwd.map(String::from).unwrap_or_default();
    let mut args = vec!["do", "space.new", "--name", name, "--tab", how.name, "--argv", &argv];
    if !folder.is_empty() {
        args.extend(["--folder", &folder]);
    }
    if let Some(m) = how.on {
        args.extend(["--machine", m]);
    }
    let out = match std::process::Command::new(config::herdr_app_bin()).args(&args).stdin(std::process::Stdio::null()).output() {
        Ok(o) => o,
        Err(e) => bail!("herdr-app: {e}"),
    };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let Some(terminal) = v["terminal"].as_str() else {
        bail!("herdr-app space.new: {}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim());
    };
    let address = match how.on {
        Some(m) if !terminal.contains('/') => format!("{m}/{terminal}"),
        _ => terminal.to_string(),
    };
    let label = format!("tower.name={}", how.name);
    let _ = hc::run(&["label", &address, &label]);
    // space.new shows the new space; give the window back to the one it showed.
    if let Some(was) = all.iter().find(|s| s.active) {
        let _ = std::process::Command::new(config::herdr_app_bin()).args(["do", "space.select", "--id", &was.id]).output();
    }
    Ok(address)
}

/// A new space called `name` that opens on its home tab (a shell in `home`,
/// beside the space's worktrees: see `worktrees::home_of`), the session a
/// tab after it: the session's address. Herdr's sidebar places a space by
/// its first terminal's folder and nests one in a worktree under the
/// space that holds the repo's own checkout; the home tab is in none.
fn new_space_at_home(name: &str, home: &str, how: &accounts::Start, all: &[spaces::Window]) -> Result<String> {
    let args = ["do", "space.new", "--name", name, "--tab", name, "--folder", home];
    let out = match std::process::Command::new(config::herdr_app_bin()).args(args).stdin(std::process::Stdio::null()).output() {
        Ok(o) => o,
        Err(e) => bail!("herdr-app: {e}"),
    };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let Some(terminal) = v["terminal"].as_str() else {
        bail!("herdr-app space.new: {}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim());
    };
    // The tower's own tab, never taken for a session.
    let _ = hc::run(&["label", terminal, worktrees::LABEL_HOME, &format!("tower.name={name}")]);
    let made = spaces::windows().into_iter().find(|w| w.name == name && w.machine.is_none());
    let address = match made {
        Some(sp) => start_in_space(&sp, how, all),
        None => Err(anyhow::anyhow!("herdr-app made the space \"{name}\", but its window doesn't list it")),
    };
    // space.new shows the new space; give the window back to the one it showed.
    if let Some(was) = all.iter().find(|s| s.active) {
        let _ = std::process::Command::new(config::herdr_app_bin()).args(["do", "space.select", "--id", &was.id]).output();
    }
    address
}

/// The space a session landed in, once the voice has given it a tab (up to 3 s).
fn placed_space(address: &str) -> Option<String> {
    for _ in 0..12 {
        let t = hc::get(address)?;
        if let Some(sp) = t["labels"]["where"].as_str().and_then(|w| w.split(" / ").next()).filter(|s| !s.is_empty()) {
            return Some(sp.to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    None
}

/// `tower behavior ...`.
fn behavior_cmd(action: &str, arg: &[String], title: Option<String>, category: Option<String>, summary: Option<String>, text: Option<String>, json: bool) -> Result<()> {
    use behaviors::{Op, Patch, Store};
    let store = Store::open();
    let id = arg.first().cloned().unwrap_or_default();
    let need_id = || -> Result<String> {
        if id.is_empty() {
            bail!("tower behavior {action} <id> (ids: tower behavior list)");
        }
        Ok(id.clone())
    };
    let op = match action {
        "list" | "ls" => {
            if json || !std::io::stdout().is_terminal() {
                print_json(&behaviors::listing());
                return Ok(());
            }
            let mut last = String::new();
            for b in store.list() {
                if b.category != last {
                    println!("\n{}", b.category);
                    last = b.category.clone();
                }
                println!("  {} {:<34} {}", if b.enabled { "●" } else { "○" }, b.id, b.summary);
            }
            return Ok(());
        }
        "show" => {
            let Some(b) = store.get(&need_id()?) else { bail!("no behavior {id}") };
            if json || !std::io::stdout().is_terminal() {
                print_json(&serde_json::to_value(&b)?);
            } else {
                println!("{} ({}, {}, {})\n{}\n\n{}", b.title, b.category, b.source, if b.enabled { "on" } else { "off" }, b.summary, b.text.trim());
            }
            return Ok(());
        }
        "history" => {
            for c in store.history().iter().rev().take(20) {
                println!("{} {:<8} {:<30} by {}", c.at, c.op, c.title, c.by);
            }
            return Ok(());
        }
        "prompt" => {
            print!("{}", store.prompt());
            return Ok(());
        }
        "add" => {
            let body = text.unwrap_or_else(|| arg.join(" "));
            let Some(title) = title else { bail!("tower behavior add --title \"…\" [--category …] [--summary \"…\"] \"<the rule>\"") };
            Op::Add { title, category: category.unwrap_or_else(|| "Misc".into()), summary: summary.unwrap_or_default(), text: body }
        }
        "edit" => Op::Edit { id: need_id()?, patch: Patch { title, category, summary, text, enabled: None } },
        "enable" | "disable" => Op::Edit { id: need_id()?, patch: Patch { enabled: Some(action == "enable"), ..Default::default() } },
        "remove" | "rm" | "delete" => Op::Remove { id: need_id()? },
        "undo" => Op::Undo,
        other => bail!("tower behavior {other}: use list, show, add, edit, enable, disable, remove, undo, history"),
    };
    let by = behaviors::caller();
    let (b, c) = behaviors::apply(op, by)?;
    let view = c.view();
    let _ = protocol::send(&Request::BehaviorsChanged { by: by.into(), change: view.clone(), from: peers::caller() });
    if json || !std::io::stdout().is_terminal() {
        print_json(&json!({ "behavior": b, "change": view }));
    } else {
        println!("{} {} ({}).", c.op, c.title, c.id);
        let d = view["diff"].as_str().unwrap_or("");
        if !d.is_empty() {
            println!("{d}");
        }
    }
    Ok(())
}

/// `tower accounts`.
fn list_accounts(on: Option<&str>) -> Result<()> {
    let list = match on {
        None => accounts::list()?,
        Some(m) => accounts::list_on(m),
    };
    if std::io::stdout().is_terminal() {
        if list.is_empty() {
            println!(
                "No Claude accounts: the claude-accounts mod (cca) isn't installed{}.",
                on.map(|m| format!(" on {m}")).unwrap_or_default()
            );
        }
        for (i, a) in list.iter().enumerate() {
            println!("{} {:<12} {}", if i == 0 { "*" } else { " " }, a.name, a.email);
        }
    } else {
        print_json(&json!({ "default": list.first().map(|a| &a.name), "accounts": list }));
    }
    Ok(())
}

/// `tower accounts --usage`: the board (`usage::board`, read now from cca
/// and the failover's book, as the daemon reads it) and every seat in the
/// state, with its account.
fn accounts_usage(json_out: bool) -> Result<()> {
    let now = history::now_ms();
    let (list, used) = accounts::list_with_usage()?;
    let mut state: Value = std::fs::read_to_string(config::ui_dir().join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    // Who is moving only the daemon knows: as its board says.
    let moving = usage::of_state(&state).map(|b| b.moving).unwrap_or_default();
    let board = usage::board(&list, &used, &failover::load_from(&failover::path()), &moving, now);
    // A daemon that doesn't say a tower's account: asked of its terminal here.
    for b in state["brains"].as_array_mut().into_iter().flatten() {
        if b["account"].as_str().is_none_or(str::is_empty)
            && let Some(a) = b["address"].as_str().and_then(accounts::of_address)
        {
            b["account"] = json!(a);
        }
    }
    let groups = usage::seats(&state, Some(&board));
    if json_out || !std::io::stdout().is_terminal() {
        print_json(&json!({ "accounts": board, "groups": groups }));
        return Ok(());
    }
    if board.list.is_empty() {
        println!("No Claude accounts: the claude-accounts mod (cca) isn't installed.");
    }
    for l in &board.list {
        let mut tags = vec![];
        if l.default {
            tags.push("the default".to_string());
        }
        if board.starts_on.as_deref() == Some(l.name.as_str()) {
            tags.push("new sessions start here".to_string());
        }
        let tags = if tags.is_empty() { String::new() } else { format!("  ({})", tags.join(", ")) };
        println!("{}  {}{tags}", l.name, l.email);
        // The 5-hour window only for an account that has one (`Line::rows`).
        let windows: Vec<String> = l.rows().into_iter().map(|(label, w)| usage::window_words(if label == "5-hour" { "5h" } else { "7d" }, w, now)).collect();
        println!("  {}", windows.join("   "));
        let out = usage::out_words(l, now);
        if !out.is_empty() {
            println!("  {out}");
        }
        let (sessions, towers) = usage::on(&groups, &l.name);
        println!("  runs {sessions} session(s) and {towers} tower(s)");
    }
    for g in &groups {
        println!("\n{}", g.title());
        for s in &g.seats {
            let mut line = format!("  {} ({}){}  {}", s.name, s.address, if s.tower { " tower" } else { "" }, s.account.as_deref().unwrap_or("–"));
            if let Some(m) = &s.moved {
                line.push_str(&format!("  · {}", usage::moved_words(m, now)));
            }
            if s.moving {
                line.push_str("  · moving now");
            }
            if s.waiting {
                line.push_str("  · waits for an account");
            }
            println!("{line}");
        }
    }
    Ok(())
}

pub fn show_terminal(address: &str) -> Result<()> {
    let Some(t) = hc::get(address) else { bail!("no terminal {address}") };
    let addr = t["address"].as_str().unwrap_or(address).to_string();
    let shown = t["labels"].get("herdr.space").is_some();
    let args: Vec<&str> = if shown {
        vec!["do", "agent.focus", "--terminal", &addr]
    } else {
        vec!["do", "terminal.keep", "--terminal", &addr, "--place", "tab"]
    };
    let out = std::process::Command::new(config::herdr_app_bin()).args(&args).output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => bail!("herdr-app: {}{}", String::from_utf8_lossy(&o.stdout).trim(), String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => bail!("herdr-app: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_says_whether_its_prs_get_a_reviewer() {
        let flags = |args: &[&str]| match Cli::try_parse_from([&["tower", "launch", "Fix the login"], args].concat()).map(|c| c.cmd) {
            Ok(Cmd::Launch { review, no_review, .. }) => Ok((review, no_review)),
            Ok(_) => Err("another command".to_string()),
            Err(e) => Err(e.kind().to_string()),
        };
        assert_eq!(flags(&[]), Ok((false, false)), "neither: as the space has it (on)");
        assert_eq!(flags(&["--review"]), Ok((true, false)));
        assert_eq!(flags(&["--no-review"]), Ok((false, true)));
        assert!(flags(&["--review", "--no-review"]).is_err(), "one or the other");
        assert!(flags(&["--draft", "--no-review"]).is_err(), "a draft opens nothing to review");
        // The space's own switch, and the reviews' commands.
        assert!(matches!(Cli::try_parse_from(["tower", "space", "review", "fix-login", "off"]).map(|c| c.cmd), Ok(Cmd::Space { ref what, .. }) if what == "review"));
        let report = Cli::try_parse_from(["tower", "review", "report", "acme/shop#12", "--critical", "2", "a crash", "on empty carts"]).map(|c| c.cmd);
        assert!(matches!(report, Ok(Cmd::Review { ref action, ref pr, critical: Some(2), ref text, .. }) if action.as_deref() == Some("report") && pr.as_deref() == Some("acme/shop#12") && text.join(" ") == "a crash on empty carts"));
    }
}
