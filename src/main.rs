//! herder-tower: a voice control tower for Herdr.
//!
//! `tower up` starts two Herdr terminals: `tower-voice` (this program's
//! daemon: Whisper ears, Kokoro mouth, a radar on every session) and `tower`
//! (a Claude Code or Codex session that steers the others through `hc`).

// GPUI comes through GPUI Kit; its macros expect the crate to be called `gpui`.
extern crate gpui_kit as gpui;

mod accounts;
mod agenda;
mod answers;
mod activity;
mod behaviors;
mod brain;
mod chat;
mod config;
mod daemon;
mod dispatch;
mod draft;
mod hc;
mod history;
mod hooks;
mod install;
mod introduce;
mod orb;
mod phone;
mod protocol;
mod projects;
mod prs;
mod questions;
mod screenshots;
mod selftest;
mod spaces;
mod split;
mod text;

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
    /// Stop the voice and the brain.
    Down {
        /// Leave the brain running.
        #[arg(long)]
        keep_brain: bool,
    },
    /// Stop and start the voice (after a config change). The brain keeps running.
    Restart,
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
    /// `fail <task> "<why>"`, `requeue <task> [<session>]`.
    Agenda {
        /// list | tasks | show | new | close | abandon | verify | fail | requeue
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
        /// Its first task.
        task: Vec<String>,
    },
    /// The folder a project's name means ("sandbox project", "mediaroom"): the
    /// best match under ~/Work (and one level below), and what else came close.
    Project {
        /// The name as the user said it.
        name: Vec<String>,
    },
    /// The Claude accounts the claude-accounts mod (`cca`) knows; the first is the default.
    Accounts {
        /// Another machine's accounts.
        #[arg(long)]
        on: Option<String>,
    },
    /// The phone API for the Android app (docs/mobile-api.md): enable, disable, pair, devices, route <id> phone|mac, revoke <id|all>.
    Phone {
        /// enable | disable | pair | devices | route | revoke
        #[arg(default_value = "devices")]
        action: String,
        /// revoke: a device id (`tower phone devices`), or `all`. route: a device id.
        id: Option<String>,
        /// route: `phone` (replies to what it sends are heard on the phone only) or `mac`.
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
    /// Answer a question set: `tower answer <id|session> --json '[{"tab":0,"choice":1}, {"tab":1,"choice":{"other":"…"}}]'`.
    Answer {
        target: String,
        #[arg(long)]
        json: String,
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
    /// Dismiss a question set (it shows no more).
    Dismiss { id: String },
    /// How session questions are handled: normal (the tower answers routine
    /// ones, the user decides the rest), keep-moving, or ask-everything.
    Policy { policy: Option<String> },
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
    /// Send text to the brain as if spoken.
    Ask { text: Vec<String> },
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
    /// A space's agenda: `tower space agenda b2c "Answer support tickets"` ("" clears it).
    Space { what: String, space: String, text: Vec<String> },
    /// Every Herdr space: focus, agenda, unread news, sessions (JSON when piped).
    Spaces,
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
    /// Called by the brain's agent when it finishes a turn.
    #[command(hide = true)]
    Hook { kind: String, arg: Option<String> },
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
                if show {
                    show_terminal(&address)?;
                }
                let _ = protocol::send(&Request::Say { text: "Tower online.".into(), interrupt: true, kind: String::new() });
            }
            Ok(())
        }
        Cmd::Down { keep_brain } => {
            let _ = protocol::send(&Request::Quit);
            brain::stop(brain::VOICE_NAME);
            if !keep_brain {
                brain::stop(brain::NAME);
            }
            println!("Tower stopped.");
            Ok(())
        }
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
        Cmd::Restart => {
            let _ = protocol::send(&Request::Quit);
            brain::stop(brain::VOICE_NAME);
            brain::launch_voice()?;
            println!("Voice restarted.");
            Ok(())
        }
        Cmd::Status => {
            let r = protocol::send(&Request::Status)?;
            print_json(&r.state.unwrap_or_default());
            Ok(())
        }
        Cmd::Agents { all } => agents(all),
        Cmd::Start { account, name, cwd, on, space, new_space, no_watch, agenda, title, plan_id, no_track, task } => {
            let tag = (!no_track).then(|| agenda::Tag { agenda, title: title.unwrap_or_default(), plan_id: plan_id.unwrap_or_default(), ..Default::default() });
            let space = space.map(|s| spaces::Asked { name: s, new: new_space });
            start(account.as_deref(), &name, cwd, on.as_deref(), space.as_ref(), no_watch, tag, &task.join(" "))
        }
        Cmd::Agenda { action, args, all, force, title, space } => agenda_cmd(&action, &args, all, force, title, space),
        Cmd::Introduce { space, dry_run } => introduce_cmd(&space.join(" "), dry_run),
        Cmd::Accounts { on } => list_accounts(on.as_deref()),
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
        Cmd::Answer { target, json, no_submit, here } => {
            let answers: Value = serde_json::from_str(&json).map_err(|e| anyhow::anyhow!("--json: {e}"))?;
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
            answers::answer(&target, answers, !no_submit)?;
            println!("Answered{}.", if no_submit { " (filled in, not sent)" } else { "" });
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
            ok(Request::Say { text: text.join(" "), interrupt, kind: if alert { "alert".into() } else { String::new() } })
        }
        Cmd::Hush => ok(Request::Hush),
        Cmd::Talk { action } => ok(Request::Talk { action }),
        Cmd::Cancel => ok(Request::Cancel),
        Cmd::Mode { mode } => ok(Request::Mode { mode }),
        Cmd::Ask { text } => ok(Request::Ask { text: text.join(" ") }),
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
        Cmd::Space { what, space, text } => match what.as_str() {
            "agenda" => spaces::cmd_agenda(space, text.join(" ")),
            other => bail!("tower space {other}: use `tower space agenda <space> \"<text>\"`"),
        },
        Cmd::Spaces => spaces::cmd_spaces(),
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
        Cmd::Hook { kind, arg } => {
            match kind.as_str() {
                "claude" => hooks::claude_stop(),
                "claude-notify" => hooks::claude_notify(),
                "codex" => hooks::codex(arg),
                "activity-prompt" => activity::hook("prompt"),
                "activity-pre" => activity::hook("pre"),
                "activity-post" => activity::hook("post"),
                // Every agent session's hooks: `tower hook agent claude|codex`.
                "agent" => hooks::agent(arg.as_deref().unwrap_or("claude")),
                // The brain's Bash calls: one hand-off per task.
                "brain-guard" => hooks::brain_guard(),
                _ => {}
            }
            Ok(())
        }
    }
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

fn queue_cmd(args: &[String]) -> Result<()> {
    let q = |action: &str, session: &str, text: String, id: Option<u64>, after: Vec<u64>| Request::Queue {
        action: action.into(),
        session: session.into(),
        text,
        id,
        after,
        start: vec![],
        tag: None,
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
            if let Request::Queue { tag, .. } = &mut req {
                *tag = Some(agenda::Tag { agenda: agenda_id, ..Default::default() });
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
                    if name == brain::NAME || name == brain::VOICE_NAME {
                        continue;
                    }
                    if status.is_none() && !all {
                        continue;
                    }
                    // `hc ls` has the reporting app (or else the program) as `agent`.
                    let app = hc::agent_app(&t).or_else(|| t["agent"].as_str().map(String::from));
                    let account = if machine.is_none() { accounts::of_address(t["address"].as_str().unwrap_or("")) } else { None };
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
fn start(account: Option<&str>, name: &str, cwd: Option<String>, on: Option<&str>, space: Option<&spaces::Asked>, no_watch: bool, tag: Option<agenda::Tag>, task: &str) -> Result<()> {
    // The space first: it may say which machine the session runs on. One
    // that doesn't exist yet is made below; of several, the closest is taken.
    let spaces = if space.is_some() { spaces::windows() } else { vec![] };
    let mut notes: Vec<String> = vec![];
    let (target, new_space) = match space {
        Some(want) => match spaces::best_window(want, &spaces) {
            spaces::Placing::One(w) | spaces::Placing::Closest(w, _) if want.new && !w.name.eq_ignore_ascii_case(want.name.trim()) => (None, Some(want.name.trim().to_string())),
            spaces::Placing::One(w) => (Some(w.clone()), None),
            spaces::Placing::Closest(w, others) => {
                notes.push(format!("\"{}\" matched several spaces ({}): took \"{}\"", want.name, others.join(", "), w.name));
                (Some(w.clone()), None)
            }
            spaces::Placing::New(name) => (None, Some(name)),
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
                chosen = Some((m.folder.clone(), others));
                Some(m.path.to_string_lossy().into_owned())
            }
            None => bail!("no folder {c}, and no project folder fits that name under {}", projects::roots().iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")),
        },
        c => c,
    };
    let cca = cca.unwrap_or_else(|| "cca".into());
    let how = accounts::Start { name, cwd: cwd.as_deref(), on, task, account: account.as_deref(), cca: &cca };
    let (address, placed) = match (&target, &new_space) {
        (Some(sp), _) => (start_in_space(sp, &how, &spaces)?, Some(sp.name.clone())),
        (None, Some(name)) => {
            notes.push(format!("made the space \"{name}\""));
            (new_space_with(name, &how, &spaces)?, Some(name.clone()))
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
            tag.project = cwd.clone().unwrap_or_default();
            protocol::send(&Request::Queue {
                action: "track".into(),
                session: address.clone(),
                text: task.to_string(),
                id: None,
                after: vec![],
                start: vec![],
                tag: Some(tag),
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
    if std::io::stdout().is_terminal() {
        println!(
            "Started {name} as {address}{in_space}{on_account}.{}{}{}",
            notes.iter().map(|n| format!(" {}{}.", n[..1].to_uppercase(), &n[1..])).collect::<String>(),
            if watched { " Watching it: you'll get a [watch] message when it stops working. End your turn now." } else { "" },
            agenda_said(&tracked)
        );
    } else {
        let mut v = json!({
            "address": address, "name": name, "space": placed, "account": account, "watching": watched,
            "queue_id": tracked["id"], "agenda": tracked["agenda"], "task": tracked["task"],
        });
        if new_space.is_some() {
            v["space_made"] = json!(true);
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
        "show" | "close" | "abandon" | "verify" | "fail" => {
            c.target = need("agenda or task")?;
            c.text = args[1..].join(" ");
        }
        "requeue" => {
            c.target = need("task")?;
            c.session = args.get(1).cloned().unwrap_or_default();
            c.text = args.get(2..).map(|r| r.join(" ")).unwrap_or_default();
        }
        "new" => c.text = args.join(" "),
        other => bail!("tower agenda {other}: use list, tasks, show, new, close, abandon, verify, fail or requeue"),
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
    let out = match std::process::Command::new("herdr-app").args(&args).stdin(std::process::Stdio::null()).output() {
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
        let _ = std::process::Command::new("herdr-app").args(["do", "space.select", "--id", &was.id]).output();
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
    let out = match std::process::Command::new("herdr-app").args(&args).stdin(std::process::Stdio::null()).output() {
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
        let _ = std::process::Command::new("herdr-app").args(["do", "space.select", "--id", &was.id]).output();
    }
    Ok(address)
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
    let _ = protocol::send(&Request::BehaviorsChanged { by: by.into(), change: view.clone() });
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

pub fn show_terminal(address: &str) -> Result<()> {
    let Some(t) = hc::get(address) else { bail!("no terminal {address}") };
    let addr = t["address"].as_str().unwrap_or(address).to_string();
    let shown = t["labels"].get("herdr.space").is_some();
    let args: Vec<&str> = if shown {
        vec!["do", "agent.focus", "--terminal", &addr]
    } else {
        vec!["do", "terminal.keep", "--terminal", &addr, "--place", "tab"]
    };
    let out = std::process::Command::new("herdr-app").args(&args).output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => bail!("herdr-app: {}{}", String::from_utf8_lossy(&o.stdout).trim(), String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => bail!("herdr-app: {e}"),
    }
}
