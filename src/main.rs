//! herder-tower: a voice control tower for Herdr.
//!
//! `tower up` starts two Herdr terminals: `tower-voice` (this program's
//! daemon: Whisper ears, Kokoro mouth, a radar on every session) and `tower`
//! (a Claude Code or Codex session that steers the others through `hc`).

// GPUI comes through GPUI Kit; its macros expect the crate to be called `gpui`.
extern crate gpui_kit as gpui;

mod accounts;
mod activity;
mod brain;
mod config;
mod daemon;
mod draft;
mod hc;
mod history;
mod hooks;
mod install;
mod orb;
mod phone;
mod protocol;
mod screenshots;
mod selftest;
mod spaces;
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
        /// Its working folder (default: here; with --on, that machine's home).
        #[arg(long)]
        cwd: Option<String>,
        /// Another machine (`hc machine ls`).
        #[arg(long)]
        on: Option<String>,
        /// The Herdr space to put it in, by its name or a part of it (default:
        /// a new tab in the space the window shows).
        #[arg(long)]
        space: Option<String>,
        /// Don't watch it (by default you get a [watch] message when it stops working).
        #[arg(long)]
        no_watch: bool,
        /// Its first task.
        task: Vec<String>,
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
        Cmd::Start { account, name, cwd, on, space, no_watch, task } => {
            start(account.as_deref(), &name, cwd, on.as_deref(), space.as_deref(), no_watch, &task.join(" "))
        }
        Cmd::Accounts { on } => list_accounts(on.as_deref()),
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
                "done" | "failed" => history::job_update(&terminal, if action == "done" { "done" } else { "failed" }, &text),
                "update" => history::job_update(&terminal, status.as_deref().unwrap_or("working"), &text),
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
fn start(account: Option<&str>, name: &str, cwd: Option<String>, on: Option<&str>, space: Option<&str>, no_watch: bool, task: &str) -> Result<()> {
    // The space first: it may say which machine the session runs on.
    let spaces = if space.is_some() { spaces::windows() } else { vec![] };
    let target = match space {
        Some(want) => Some(spaces::pick_window(want, &spaces)?.clone()),
        None => None,
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
    let cca = cca.unwrap_or_else(|| "cca".into());
    let how = accounts::Start { name, cwd: cwd.as_deref(), on, task, account: account.as_deref(), cca: &cca };
    let (address, placed) = match &target {
        Some(sp) => (start_in_space(sp, &how, &spaces)?, Some(sp.name.clone())),
        None => {
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
    if std::io::stdout().is_terminal() || !watched {
        println!(
            "Started {name} as {address}{in_space}{on_account}.{}",
            if watched { " Watching it: you'll get a [watch] message when it stops working. End your turn now." } else { "" }
        );
    } else {
        print_json(&json!({ "address": address, "name": name, "space": placed, "account": account, "watching": watched }));
    }
    Ok(())
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
