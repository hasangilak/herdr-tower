//! The tower's brains: the main one (the terminal `tower`) and, with
//! `[brains] per_space`, one per open space (`tower-<space-slug>`). The
//! contract is `docs/quiet-tower.md`, "Step 7" and "The brains' model".
//!
//! - **Names and folders.** A space's brain is a Herdr terminal named
//!   `tower-<space-slug>` (the slug `worktrees::slug` gives), run in its own
//!   folder, `brains/<space-slug>/` beside the main brain's `brain/`. The
//!   folder holds its instructions (the main brain's core and behaviors,
//!   and a section for that space only), its hooks, and `brain.json` (which
//!   space it is, and the space as its instructions describe it).
//! - **What a brain is started with** (`argv`): the program, and for Claude
//!   Code `--model` and `--effort` from `[brains]`, unless `claude_args`
//!   sets them. Nothing is typed into a running brain to change them, and
//!   nothing is put in its environment: a change applies at its next start.
//! - **The model** (`report`, `set`): the setting, what each running brain
//!   was started with (read off its command line), and whether a restart
//!   is needed.
//! - **When a space has one** (`step`): from its first live agent session
//!   until it is closed, archived, gone, or has had no live session for a
//!   few minutes. One that dies is started again, each time later.
//! - **Who gets what** (`route`): a note or a request for a space goes to
//!   that space's brain when it can take it, else to the main one.
//!
//! The daemon runs all of it (`crate::daemon::brains`).

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::brain;
use crate::config::{self, Config};
use crate::hc::SessionStatus;

/// What a brain runs on when config.toml says nothing.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_EFFORT: &str = "high";

/// The models the picker offers: the name the program takes, and what to call it.
pub const MODELS: [(&str, &str); 4] =
    [("claude-fable-5-2", "Fable 5.2"), ("claude-opus-5-5", "Opus 5.5"), ("claude-sonnet-5-5", "Sonnet 5.5"), ("claude-haiku-5-5", "Haiku 5.5")];

/// The effort levels, lowest first.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// A space's brain is stopped once its space has had no live session this long.
pub const IDLE_GRACE_MS: u64 = 3 * 60_000;
/// A brain that has run this long started well: its next restart is at once again.
pub const STEADY_MS: u64 = 5 * 60_000;
/// The longest wait between two tries to start a brain that keeps dying.
pub const BACKOFF_MAX_MS: u64 = 10 * 60_000;

/// The label a space's brain carries: its space, by name.
pub const LABEL_SPACE: &str = "tower.space";

// ---- names and folders ----

/// The longest name Herdr gives a terminal.
const NAME_MAX: usize = 32;

/// The terminal name of a space's own brain: `tower-<space-slug>`. A space
/// called "voice" would collide with the daemon's own terminal, so its
/// brain is `tower-voice-space`. A name past what Herdr allows is cut, with
/// a few letters of its own at the end so two long names stay apart.
pub fn name_for(space: &str) -> String {
    let slug = crate::worktrees::slug(space);
    let name = format!("tower-{slug}");
    // (Nor the planner's: a space called "planner" is `tower-planner-space`.)
    if name == brain::VOICE_NAME || crate::plans::is_planner(&name) {
        return format!("{name}-space");
    }
    if name.len() <= NAME_MAX {
        return name;
    }
    let hash = slug.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    // (The slug is a-z 0-9 - _ only: cut anywhere.)
    format!("{}-{:04x}", name[..NAME_MAX - 5].trim_end_matches('-'), hash & 0xffff)
}

/// The folder a space's brain runs in.
pub fn dir_for(space: &str) -> PathBuf {
    dir_of(&name_for(space))
}

/// The folder of the space brain called `name`.
pub fn dir_of(name: &str) -> PathBuf {
    config::brains_dir().join(name.strip_prefix("tower-").unwrap_or(name))
}

/// A terminal by this name is a space's own brain: `tower-…`, but neither
/// the daemon's terminal, the main brain, nor the planner (`crate::plans`:
/// a brain of no space, which no note or request is ever routed to).
pub fn is_space_brain(name: &str) -> bool {
    name.starts_with("tower-") && name != brain::VOICE_NAME && name != brain::NAME && name != brain::name() && !crate::plans::is_planner(name)
}

/// A folder is a brain's own (the main one's, or a space's): what runs
/// there is the tower, not a session of the work.
pub fn is_brain_folder(folder: &str) -> bool {
    !folder.is_empty() && (Path::new(folder).starts_with(config::brain_dir()) || Path::new(folder).starts_with(config::brains_dir()))
}

/// A terminal called `name` is a brain this tower runs: the main one, or
/// a space's own started from this tower's folders. A `tower-…` terminal
/// run from anywhere else (named by hand, another tower's) is left to
/// whoever started it: never routed to, never stopped.
pub fn is_ours(name: &str, t: &Value) -> bool {
    // The planner is the tower's, but no brain of a space nor the main one: the plans follow it.
    !crate::plans::is_planner(name) && (!is_space_brain(name) || is_brain_folder(t["cwd"].as_str().unwrap_or("")))
}

/// What a space brain's folder says about it (`brain.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Card {
    /// Its terminal's name.
    pub name: String,
    /// Its space, as Herdr spells it.
    pub space: String,
    /// The space as its instructions describe it (`missions[]` in the state).
    pub mission: Value,
}

/// The card of the space brain called `name`, if it has a folder.
pub fn card_of(name: &str) -> Option<Card> {
    let card: Card = serde_json::from_str(&std::fs::read_to_string(dir_of(name).join("brain.json")).ok()?).ok()?;
    (!card.space.is_empty()).then_some(card)
}

/// Every space brain that has a folder.
pub fn cards() -> Vec<Card> {
    let Ok(rd) = std::fs::read_dir(config::brains_dir()) else { return vec![] };
    let mut out: Vec<Card> = rd
        .flatten()
        .filter_map(|e| serde_json::from_str::<Card>(&std::fs::read_to_string(e.path().join("brain.json")).ok()?).ok())
        .filter(|c| !c.space.is_empty() && !c.name.is_empty())
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ---- a space brain's instructions ----

/// One line of the section per session, request.
fn session_line(s: &Value) -> String {
    let word = |k: &str| s[k].as_str().unwrap_or("").trim().to_string();
    let mut line = format!("  - {} \"{}\": {}", word("address"), word("name"), if word("status").is_empty() { "starting".into() } else { word("status") });
    if !word("worktree").is_empty() {
        line += &format!(", in {}", word("worktree"));
    }
    if let Some(p) = s["port"].as_u64() {
        line += &format!(", dev port {p}");
    }
    line
}

/// The part of a space brain's instructions that is about its space only:
/// what the space is (`mission`: its row of the state's `missions`), and
/// what is this brain's to handle and what is the main tower's.
pub fn space_section(space: &str, name: &str, dir: &Path, mission: &Value) -> String {
    let word = |k: &str| mission[k].as_str().unwrap_or("").trim().to_string();
    let mut s = format!("# Your space: \"{space}\"\n\n");
    s += &format!(
        "You are not the main tower. You are the tower of one space, \"{space}\": your terminal is `{name}`, and everything above holds for you inside this space only. Where this part and the text above disagree, this part wins.\n\n"
    );
    s += "## What this space is\n\n";
    match (word("goal").is_empty(), mission["standing"] == true) {
        (_, true) => s += "- A standing space: the user keeps it. It is never ready to close, and sessions opened here work in the projects' own checkouts.\n",
        (true, false) => s += "- It has no goal: it is the user's own place, with no end.\n",
        (false, false) => {
            s += &format!("- Its goal: {}\n", word("goal"));
            if !word("done_when").is_empty() {
                s += &format!("- Done when: {}\n", word("done_when"));
            }
        }
    }
    if mission["standing"] == true && !word("goal").is_empty() {
        s += &format!("- What it is for: {}\n", word("goal"));
    }
    let sessions: Vec<String> = mission["sessions"].as_array().into_iter().flatten().map(session_line).collect();
    match sessions.is_empty() {
        true => s += "- Its sessions: none when this was written.\n",
        false => s += &format!("- Its sessions when this was written:\n{}\n", sessions.join("\n")),
    }
    let requests: Vec<String> = mission["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| format!("  - {} \"{}\": {}, {} of {} tasks settled", r["id"].as_str().unwrap_or(""), r["title"].as_str().unwrap_or(""), r["state"].as_str().unwrap_or(""), r["done"].as_u64().unwrap_or(0), r["total"].as_u64().unwrap_or(0)))
        .collect();
    match requests.is_empty() {
        true => s += "- Its requests: none open when this was written.\n",
        false => s += &format!("- Its requests when this was written:\n{}\n", requests.join("\n")),
    }
    s += &format!("- How it stands now: `tower missions` (its row is \"{space}\"), `tower agenda` for its requests, `tower decisions --space \"{space}\"` for what waits for the user here.\n");
    s += &format!("- What its sessions wrote to each other: `tower thread \"{space}\"`.\n\n");
    s += "## What is yours, and what is the main tower's\n\n";
    s += "- You handle this space and nothing else: its sessions, its requests, its decisions. The notes you get (`[watch]`, `[queue]`, `[agenda]`, `[question]`, `[session]`, `[space]`) are about this space, and so is what the user asks you (typed in this space's view; it arrives as `[voice]`, and its `[radar]` line lists this space's sessions only).\n";
    s += "- Anything that crosses spaces is the main tower's: a session of another space, work for a project no session here has, another space to open or close, how the tower behaves (behaviors, `tower tell`, `tower model`). Hand it over in one line: `tower queue tower \"<what it is, and what you need back>\"`. It reaches the main tower as a note from you. When the user asked, say in your answer that you passed it on.\n";
    s += "- A `[session]` note from `tower` is the main tower handing you something for this space: act on it, and answer it with `tower queue tower \"<the answer>\"` only if it asks for one.\n";
    s += &format!("- Open sessions only in this space: always `tower start --space \"{space}\" …`. Never close this space, and never start or stop a tower.\n");
    s += &format!("- A decision you raise is this space's: `tower raise <session> …` as above. Sessions here reach you with `tower queue {name} \"…\"`.\n");
    s += &format!("- Your instructions are this file, `{}`. A `[behaviors]` note means: read it again, and write nothing.\n", dir.join("CLAUDE.md").display());
    s
}

/// A space brain's whole instructions: the main brain's (`base`: the core
/// and the behaviors), told at the top whose they are, and its space's
/// section at the end.
pub fn space_prompt(base: &str, space: &str, name: &str, dir: &Path, mission: &Value) -> String {
    let lead = format!("> You are the tower of the space \"{space}\", not the main tower. Read \"Your space\" at the end of this file first: it says what is yours, and it wins over the rest.");
    // Under the "generated" line the main brain's file starts with.
    let (head, rest) = base.split_once("\n\n").unwrap_or(("", base));
    let top = if head.starts_with("<!--") { format!("{head}\n\n{lead}\n\n{rest}") } else { format!("{lead}\n\n{base}") };
    format!("{}\n\n{}", top.trim_end(), space_section(space, name, dir, mission))
}

/// A brain's Claude Code settings: its hooks and what it may run. `brain`:
/// a space brain's name, so its hooks say whose words they bring (None: the
/// main brain, which also reports what it is doing for the orb).
pub fn settings(tower: &str, brain: Option<&str>) -> Value {
    let as_brain = brain.map(|b| format!(" {b}")).unwrap_or_default();
    let command = |hook: &str| json!({ "type": "command", "command": format!("'{tower}' hook {hook}") });
    let mut hooks = json!({
        "Stop": [{ "hooks": [{ "type": "command", "command": format!("'{tower}' hook claude{as_brain}") }] }],
        "Notification": [{ "hooks": [{ "type": "command", "command": format!("'{tower}' hook claude-notify{as_brain}") }] }],
        "PreToolUse": [
            // One hand-off per task: a bundle for several projects is turned back.
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": format!("'{tower}' hook brain-guard"), "timeout": 5 }] },
        ],
    });
    if brain.is_none() {
        // What the main brain is doing, for the orb's working line.
        hooks["UserPromptSubmit"] = json!([{ "hooks": [command("activity-prompt")] }]);
        hooks["PreToolUse"] = json!([
            { "matcher": "*", "hooks": [command("activity-pre")] },
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": format!("'{tower}' hook brain-guard"), "timeout": 5 }] },
        ]);
        hooks["PostToolUse"] = json!([{ "matcher": "*", "hooks": [command("activity-post")] }]);
    }
    json!({
        "hooks": hooks,
        "permissions": {
            "allow": ["Bash(hc *)", "Bash(herdr-core *)", "Bash(tower *)", "Bash(herdr-app *)", format!("Bash({tower} *)")],
        },
    })
}

/// Writes a file only when what it holds would change (a brain's
/// instructions are written again at every look).
fn write_if_changed(path: &Path, text: &str) -> Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|was| was == text) {
        return Ok(false);
    }
    std::fs::write(path, text)?;
    Ok(true)
}

/// Writes a space brain's folder: its instructions (CLAUDE.md for Claude
/// Code, AGENTS.md for Codex), its hooks and permissions, and its card.
/// `mission`: the space as the state's `missions` has it. Returns the
/// folder, and whether its instructions changed.
pub fn write_space_files(space: &str, mission: &Value) -> Result<(PathBuf, bool)> {
    let dir = dir_for(space);
    let base = crate::behaviors::Store::open().prompt();
    let changed = write_folder(&dir, &base, &config::tower_bin().display().to_string(), space, mission)?;
    Ok((dir, changed))
}

/// `write_space_files`, into `dir`, from the main brain's instructions
/// (`base`) and this program's path (`tower`, for the hooks).
fn write_folder(dir: &Path, base: &str, tower: &str, space: &str, mission: &Value) -> Result<bool> {
    let name = name_for(space);
    std::fs::create_dir_all(dir.join(".claude"))?;
    let text = space_prompt(base, space, &name, dir, mission);
    let changed = write_if_changed(&dir.join("CLAUDE.md"), &text)?;
    write_if_changed(&dir.join("AGENTS.md"), &text)?;
    write_if_changed(&dir.join(".claude/settings.json"), &serde_json::to_string_pretty(&settings(tower, Some(&name)))?)?;
    let card = Card { name, space: space.to_string(), mission: mission.clone() };
    write_if_changed(&dir.join("brain.json"), &serde_json::to_string_pretty(&card)?)?;
    Ok(changed)
}

/// Every space brain's instructions and hooks, written again from its card
/// (the behaviors, the level or the tower itself changed). Returns the
/// names of those whose instructions changed.
pub fn rewrite_all() -> Vec<String> {
    cards().into_iter().filter(|c| write_space_files(&c.space, &c.mission).is_ok_and(|(_, changed)| changed)).map(|c| c.name).collect()
}

// ---- what a brain is started with ----

/// Whether `args` set `flag` themselves (`--model x` or `--model=x`), and to what.
fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let mut found = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            found = it.clone().next().cloned().or(found);
        } else if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            found = Some(v.to_string());
        }
    }
    found
}

/// The command a brain called `name` is started with, by which program
/// `which` is (`claude` or `codex`). `dir`: its folder; `tower`: this
/// program, for Codex's hook. For Claude Code, the model and effort of
/// `[brains]` go in as `--model` and `--effort` unless `claude_args` has
/// its own.
pub fn argv(which: &str, name: &str, dir: &str, tower: &str, cfg: &Config) -> Result<Vec<String>> {
    match which {
        "claude" => {
            // Its instructions are CLAUDE.md in its folder (read again when they change): not a second copy here.
            let mut a: Vec<String> = vec!["claude".into(), "-n".into(), name.into()];
            if arg_value(&cfg.claude_args, "--model").is_none() && !cfg.brains.model.trim().is_empty() {
                a.extend(["--model".into(), cfg.brains.model.trim().to_string()]);
            }
            if arg_value(&cfg.claude_args, "--effort").is_none() && !cfg.brains.effort.trim().is_empty() {
                a.extend(["--effort".into(), cfg.brains.effort.trim().to_string()]);
            }
            a.extend(cfg.claude_args.iter().cloned());
            Ok(a)
        }
        "codex" => {
            let notify = if is_space_brain(name) { format!("notify=[\"{tower}\", \"hook\", \"codex\", \"{name}\"]") } else { format!("notify=[\"{tower}\", \"hook\", \"codex\"]") };
            let mut a: Vec<String> = vec![
                "codex".into(),
                "-C".into(),
                dir.into(),
                "-c".into(),
                notify,
                // hc talks to herdr-core over a Unix socket outside the workspace.
                "--sandbox".into(),
                "danger-full-access".into(),
                "--ask-for-approval".into(),
                "on-request".into(),
            ];
            if !cfg.codex_model.is_empty() {
                a.extend(["-m".into(), cfg.codex_model.clone()]);
            }
            if !cfg.codex_effort.is_empty() {
                a.extend(["-c".into(), format!("model_reasoning_effort=\"{}\"", cfg.codex_effort)]);
            }
            if !cfg.codex_service_tier.is_empty() {
                a.extend(["-c".into(), format!("service_tier=\"{}\"", cfg.codex_service_tier)]);
            }
            a.extend(cfg.codex_args.iter().cloned());
            Ok(a)
        }
        other => bail!("unknown brain {other:?}: use claude or codex"),
    }
}

/// What a brain runs on, as its command line says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Started {
    /// `claude`, `codex`, or whatever else the terminal runs.
    pub program: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// What a command line starts a brain with (the last word wins, as the
/// programs read them).
pub fn started_with(argv: &[String]) -> Started {
    let program = argv.first().map(|p| p.rsplit('/').next().unwrap_or(p).to_string()).unwrap_or_default();
    let rest = argv.get(1..).unwrap_or_default();
    match program.as_str() {
        "codex" => {
            let effort = rest.iter().filter_map(|a| a.strip_prefix("model_reasoning_effort=")).next_back().map(|v| v.trim_matches('"').to_string());
            Started { model: arg_value(rest, "-m").or_else(|| arg_value(rest, "--model")), effort, program }
        }
        _ => Started { model: arg_value(rest, "--model"), effort: arg_value(rest, "--effort"), program },
    }
}

// ---- the model ----

/// What to call a model: the picker's name for one it offers, else the name itself.
pub fn label(model: &str) -> String {
    MODELS.iter().find(|(id, _)| *id == model).map(|(_, l)| l.to_string()).unwrap_or_else(|| model.to_string())
}

/// A model as typed: one the picker offers by its name ("opus 5.5"), or any
/// name the program takes (an alias, a full name).
pub fn parse_model(word: &str) -> Result<String> {
    let w = word.trim();
    if let Some((id, _)) = MODELS.iter().find(|(id, l)| id.eq_ignore_ascii_case(w) || l.eq_ignore_ascii_case(w)) {
        return Ok(id.to_string());
    }
    if w.is_empty() || w.starts_with('-') || w.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\') {
        bail!("{word:?} isn't a model's name: use {} (or another name the program takes)", MODELS.map(|(id, _)| id).join(", "));
    }
    Ok(w.to_string())
}

pub fn parse_effort(word: &str) -> Result<String> {
    let w = word.trim().to_lowercase();
    if !EFFORTS.contains(&w.as_str()) {
        bail!("effort is {}: not {word:?}", EFFORTS.join(", "));
    }
    Ok(w)
}

/// A config.toml's text with the brains' model and effort set (those
/// given), the rest as it is. Refused when the file wouldn't read after it.
pub fn with_setting(text: &str, model: Option<&str>, effort: Option<&str>) -> Result<String> {
    let quoted = |v: &str| toml::Value::String(v.to_string()).to_string();
    let mut out = text.to_string();
    if let Some(m) = model {
        out = config::with_table_key(&out, "brains", "model", &quoted(m));
    }
    if let Some(e) = effort {
        out = config::with_table_key(&out, "brains", "effort", &quoted(e));
    }
    if let Err(e) = Config::from_text(&out) {
        bail!("config.toml wouldn't read after that change, so it is left as it is: {e}");
    }
    Ok(out)
}

/// Keeps the brains' model and effort in config.toml (`tower model`).
pub fn set(model: Option<&str>, effort: Option<&str>) -> Result<()> {
    Config::write_default_if_missing()?;
    let path = config::config_file();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, with_setting(&text, model, effort)?)?;
    Ok(())
}

/// A brain that runs, for the report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Running {
    pub brain: String,
    pub space: Option<String>,
    pub address: String,
    /// Its command line.
    pub argv: Vec<String>,
}

/// "Opus 5.5 at high effort", "sonnet", "its program's own model".
fn on(model: Option<&str>, effort: Option<&str>) -> String {
    match (model.map(label), effort) {
        (Some(m), Some(e)) => format!("{m} at {e} effort"),
        (Some(m), None) => m,
        (None, Some(e)) => format!("its program's own model at {e} effort"),
        (None, None) => "its program's own model".into(),
    }
}

/// The brains' model as `state.json` (`brain_model`), `tower model --json`
/// and the socket carry it: the setting, the choices, what each running
/// brain was started with, and whether a restart is needed for the setting
/// to apply.
pub fn report(cfg: &Config, running: &[Running]) -> Value {
    let program = if cfg.brain == "codex" { "codex" } else { "claude" };
    // What a brain started now would run on: the setting, or what the user's own arguments say.
    let would = argv(program, brain::NAME, "", "tower", cfg).map(|a| started_with(&a)).unwrap_or_default();
    let (args_model, args_effort) = match program {
        "claude" => (arg_value(&cfg.claude_args, "--model"), arg_value(&cfg.claude_args, "--effort")),
        _ => (None, None),
    };
    let rows: Vec<(Value, bool)> = running
        .iter()
        .map(|r| {
            let s = started_with(&r.argv);
            let stale = s.program != would.program || s.model != would.model || s.effort != would.effort;
            (json!({ "brain": r.brain, "space": r.space, "address": r.address, "model": s.model, "effort": s.effort, "restart": stale }), stale)
        })
        .collect();
    let restart_needed = rows.iter().any(|(_, stale)| *stale);
    let locked = args_model.is_some() || args_effort.is_some();
    let now = on(would.model.as_deref(), would.effort.as_deref());
    // The main brain speaks for them all; else the first that runs.
    let lead = running.iter().find(|r| r.brain == brain::NAME || r.brain == brain::name()).or(running.first()).map(|r| started_with(&r.argv));
    let mut says = match (&lead, program) {
        (_, "codex") => "The tower's brain is Codex: its model comes from codex_model, codex_effort and codex_args in config.toml, and the choice here is kept for a Claude Code brain.".to_string(),
        (None, _) => format!("No brain is running. The next one starts on {now}."),
        (Some(s), _) if !restart_needed => format!("The tower's brain runs on {}.", on(s.model.as_deref(), s.effort.as_deref())),
        (Some(s), _) => format!("The tower's brain runs on {}. {} applies when it restarts.", on(s.model.as_deref(), s.effort.as_deref()), {
            let mut n = now.clone();
            if let Some(f) = n.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            n
        }),
    };
    if program == "claude" {
        let named: Vec<&str> = [args_model.as_ref().map(|_| "the model"), args_effort.as_ref().map(|_| "the effort")].into_iter().flatten().collect();
        if !named.is_empty() {
            says += &format!(" claude_args in config.toml sets {} itself, and that wins over the choice here.", named.join(" and "));
        }
    }
    json!({
        "program": program,
        "model": cfg.brains.model,
        "effort": cfg.brains.effort,
        "choices": {
            "models": MODELS.iter().map(|(id, label)| json!({ "id": id, "label": label })).collect::<Vec<_>>(),
            "efforts": EFFORTS,
        },
        "running": rows.into_iter().map(|(v, _)| v).collect::<Vec<_>>(),
        "restart_needed": restart_needed,
        "locked_by_args": locked,
        "locked": { "model": args_model, "effort": args_effort },
        "says": says,
    })
}

/// The brains Herdr has (no daemon to ask): the main one, and every
/// space's own (a `tower-…` terminal run from a brain's folder).
pub fn known_from_herdr() -> Vec<Known> {
    let mut out = vec![];
    for row in crate::hc::list_on(None).unwrap_or_default() {
        let Some(name) = crate::hc::name(&row).map(String::from) else { continue };
        let main = name == brain::NAME || name == brain::name();
        if !main && !(is_space_brain(&name) && is_brain_folder(row["cwd"].as_str().unwrap_or(""))) {
            continue;
        }
        if let Some(t) = crate::hc::get(row["address"].as_str().unwrap_or(&name)) {
            out.push(crate::daemon::brains::known_of(&name, &t, None));
        }
    }
    out
}

/// The brains that run, as Herdr has them, for the model's report.
pub fn running_from_herdr() -> Vec<Running> {
    crate::daemon::brains::running_of(&known_from_herdr())
}

/// The space a brain's terminal is for: its label, else its card.
pub fn space_of_terminal(t: &Value) -> Option<String> {
    t["labels"][LABEL_SPACE]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| crate::hc::name(t).and_then(card_of).map(|c| c.space))
}

/// `tower model [<model>] [--effort <level>] [--restart] [--json]`.
pub fn cmd_model(model: Option<String>, effort: Option<String>, restart: bool, as_json: bool) -> Result<()> {
    use crate::protocol::{self, Request};
    let model = model.as_deref().map(parse_model).transpose()?;
    let effort = effort.as_deref().map(parse_effort).transpose()?;
    let changing = model.is_some() || effort.is_some();
    if changing && Config::load()?.brain == "codex" {
        bail!("the tower's brain is Codex here (brain = \"codex\"): its model comes from codex_model, codex_effort and codex_args in config.toml. `tower model` chooses for a Claude Code brain.");
    }
    let v = if protocol::is_running() {
        // The voice keeps it, shows it, and restarts the brains (it outlives them).
        let r = protocol::send(&Request::Model { model, effort, restart })?;
        if !r.ok {
            bail!("{}", r.error.unwrap_or_default());
        }
        r.state.unwrap_or_default()
    } else {
        if changing {
            set(model.as_deref(), effort.as_deref())?;
        }
        let cfg = Config::load()?;
        let mut v = report(&cfg, &running_from_herdr());
        if restart {
            // The voice isn't running: the main brain here and now; the spaces' own when it starts.
            let stopped = stop_space_brains();
            let main = crate::hc::is_live(brain::NAME);
            if main {
                brain::stop(brain::NAME);
                brain::launch(&cfg, &cfg.brain)?;
            }
            v = report(&cfg, &running_from_herdr());
            v["restarted"] = json!(stopped.into_iter().chain(main.then(|| brain::NAME.to_string())).collect::<Vec<_>>());
        }
        v
    };
    if as_json || !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("Model: {} ({}), effort {}.", label(v["model"].as_str().unwrap_or("")), v["model"].as_str().unwrap_or(""), v["effort"].as_str().unwrap_or(""));
    for r in v["running"].as_array().into_iter().flatten() {
        println!(
            "  {:<24} {}{}",
            r["brain"].as_str().unwrap_or(""),
            on(r["model"].as_str(), r["effort"].as_str()),
            if r["restart"] == true { "  (restart needed)" } else { "" }
        );
    }
    println!("{}", v["says"].as_str().unwrap_or(""));
    match (v["restarted"].as_array().map(Vec::len), v["restart_needed"] == true) {
        (Some(n), _) if n > 0 => println!("Restarting {n} brain(s) now: each starts a new conversation on the setting above."),
        (_, true) => println!("`tower model --restart` restarts the brains now (each starts a new conversation)."),
        _ => {}
    }
    Ok(())
}

/// Ends every space's own brain Herdr has (those run from a brain folder).
/// Returns their names.
pub fn stop_space_brains() -> Vec<String> {
    let mut out = vec![];
    for row in crate::hc::list_on(None).unwrap_or_default() {
        let Some(name) = crate::hc::name(&row) else { continue };
        if is_space_brain(name) && is_brain_folder(row["cwd"].as_str().unwrap_or("")) && brain::stop(row["address"].as_str().unwrap_or(name)) {
            out.push(name.to_string());
        }
    }
    out
}

// ---- when a space has a brain of its own ----

/// How long to wait before the `n`th try to start a brain in a row.
pub fn backoff_ms(tries: u32) -> u64 {
    (15_000u64 << tries.saturating_sub(1).min(10)).min(BACKOFF_MAX_MS)
}

/// What the tower keeps about a space's brain between looks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Life {
    /// When the space last had a live session (or when this was first kept).
    pub last_live_ms: u64,
    /// Starts in a row without a steady run between them.
    pub tries: u32,
    /// Not started again before this.
    pub next_try_ms: u64,
    /// Since when it has been up, while it is.
    pub up_since_ms: Option<u64>,
    /// Held off until then (its space is being closed).
    pub held_until_ms: u64,
}

/// A space at one look.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpaceNow {
    /// Its live agent sessions.
    pub sessions: usize,
    /// Closed or archived.
    pub over: bool,
    /// Herdr's window is open and no longer has it.
    pub gone: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Do {
    Start(String),
    Stop { space: String, why: &'static str },
}

/// One look at every space: which brains to start and which to stop.
/// `spaces`: those the tower knows now; `up`: the spaces whose own brain
/// runs; `main_up`: the main brain runs (a space's brain is only started
/// beside it: it hands the main one what crosses spaces).
pub fn step(lives: &mut BTreeMap<String, Life>, spaces: &BTreeMap<String, SpaceNow>, up: &[String], per_space: bool, main_up: bool, now: u64) -> Vec<Do> {
    let mut names: Vec<String> = spaces.keys().cloned().collect();
    for n in up.iter().chain(lives.keys()) {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    let mut out = vec![];
    for name in names {
        let s = spaces.get(&name).cloned().unwrap_or_default();
        let runs = up.contains(&name);
        if !per_space || s.over {
            if runs {
                out.push(Do::Stop { space: name.clone(), why: if s.over { "its space is closed" } else { "one brain for everything ([brains] per_space = false)" } });
            }
            lives.remove(&name);
            continue;
        }
        let life = lives.entry(name.clone()).or_insert_with(|| Life { last_live_ms: now, ..Default::default() });
        let wanted = s.sessions > 0;
        if wanted {
            life.last_live_ms = now;
        }
        let idle_long = now.saturating_sub(life.last_live_ms) >= IDLE_GRACE_MS;
        if runs {
            let since = *life.up_since_ms.get_or_insert(now);
            if now.saturating_sub(since) >= STEADY_MS {
                (life.tries, life.next_try_ms) = (0, 0);
            }
            if !wanted && (s.gone || idle_long) {
                out.push(Do::Stop { space: name.clone(), why: if s.gone { "its space is gone" } else { "no session there for a few minutes" } });
                lives.remove(&name);
            }
            continue;
        }
        life.up_since_ms = None;
        if wanted && main_up && now >= life.held_until_ms && now >= life.next_try_ms {
            life.tries += 1;
            life.next_try_ms = now + backoff_ms(life.tries);
            out.push(Do::Start(name));
        } else if !wanted && idle_long {
            lives.remove(&name);
        }
    }
    out
}

// ---- who gets what ----

/// A brain as the tower sees it now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Known {
    pub name: String,
    /// Its space (None: the main brain).
    pub space: Option<String>,
    pub address: String,
    pub live: bool,
    pub status: SessionStatus,
    /// Its command line.
    pub argv: Vec<String>,
    /// When its terminal was made.
    pub created_ms: Option<u64>,
}

impl Known {
    /// It can take a note now: it runs, and isn't starting, stuck on a
    /// question on its own screen, or in an error.
    pub fn takes(&self) -> bool {
        self.live && matches!(self.status, SessionStatus::Idle | SessionStatus::Working | SessionStatus::Done)
    }

    /// How a space's view says its tower stands.
    pub fn word(&self) -> &'static str {
        match (&self.status, self.live) {
            (_, false) => "off",
            (SessionStatus::Starting, _) => "starting",
            (SessionStatus::Working, _) => "working",
            (SessionStatus::Idle | SessionStatus::Done, _) => "ready",
            _ => "blocked",
        }
    }
}

/// Where a note or a request goes.
#[derive(Debug, Clone, PartialEq)]
pub enum To {
    /// The main brain.
    Main,
    /// A space's own.
    Space { brain: String, address: String, space: String },
}

/// The brain that takes what is about `space`: that space's own when
/// `[brains] per_space` is on and it can take it now; the main one for
/// everything else (no space, a space without a brain, one that is
/// starting, stuck or gone).
pub fn route(space: Option<&str>, per_space: bool, known: &[Known]) -> To {
    let Some(space) = space.map(str::trim).filter(|s| !s.is_empty() && per_space) else { return To::Main };
    match known.iter().find(|k| k.takes() && k.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(space))) {
        Some(k) => To::Space { brain: k.name.clone(), address: k.address.clone(), space: k.space.clone().unwrap_or_default() },
        None => To::Main,
    }
}

/// The space a note is about: the one it comes with (the radar knows its
/// session's), else the space of the session it names (`space_of`: a
/// session's space by its address). None: about no one space.
pub fn note_space(given: Option<&str>, text: &str, space_of: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    given.map(str::trim).filter(|s| !s.is_empty()).map(String::from).or_else(|| brain::session_in(text).and_then(|a| space_of(&a)))
}

/// Whom a session's message to "the tower" reaches. Written to the main
/// brain by a session of a space that has a brain of its own: that one (a
/// session's own tower is its space's). Written to a brain by name
/// (`tower-<space>`), by a brain, or from a space without one: as addressed.
pub fn session_to(sender_space: Option<&str>, sender_is_brain: bool, to_main: bool, per_space: bool, known: &[Known]) -> To {
    if sender_is_brain || !to_main {
        return To::Main;
    }
    route(sender_space, per_space, known)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_space_brain_is_named_after_its_space() {
        assert_eq!(name_for("Sandbox"), "tower-sandbox");
        assert_eq!(name_for("B2C support"), "tower-b2c-support", "the slug worktrees and launch use");
        assert_eq!(name_for("fix-login"), "tower-fix-login");
        assert_eq!(name_for("Voice"), "tower-voice-space", "never the daemon's own terminal");
        assert!(name_for("日本").starts_with("tower-space-"));
        // Herdr takes names of up to 32 letters: a long one is cut, and stays its own.
        let (a, b) = (name_for("Rebuild the checkout flow for the spring campaign"), name_for("Rebuild the checkout flow for the summer campaign"));
        assert!(a.len() <= 32 && b.len() <= 32 && a != b && a.starts_with("tower-rebuild-the-checkout-"), "{a} {b}");
        assert_eq!(name_for("a-name-of-exactly-26-chars").len(), 32);
        assert_eq!(dir_for("Rebuild the checkout flow for the spring campaign"), dir_of(&a));
        // Its folder is beside the main brain's, under the tower's settings folder.
        assert!(dir_for("B2C support").ends_with("brains/b2c-support"));
        assert_eq!(dir_for("Sandbox"), dir_of("tower-sandbox"));
        assert_eq!(dir_for("Sandbox").parent(), Some(config::brains_dir().as_path()));
        assert_ne!(config::brains_dir(), config::brain_dir());
        // Which terminals are a space's brain.
        assert!(is_space_brain("tower-sandbox") && is_space_brain("tower-voice-space"));
        assert!(!is_space_brain("tower") && !is_space_brain("tower-voice") && !is_space_brain("towering") && !is_space_brain("sandbox-app"));
        // Its folder, and the main one's, are the tower's own.
        assert!(is_brain_folder(&dir_for("Sandbox").to_string_lossy()) && is_brain_folder(&config::brain_dir().join("x").to_string_lossy()));
        assert!(!is_brain_folder("/w/sandbox") && !is_brain_folder("") && !is_brain_folder(&config::config_dir().join("behaviors").to_string_lossy()));
        // The tower runs the main brain, and the spaces' own it started from its folders; not a `tower-…` someone else runs.
        let at = |cwd: &Path| json!({ "address": "t9", "cwd": cwd.to_string_lossy() });
        assert!(is_ours("tower", &at(Path::new("/anywhere"))) && is_ours("tower-sandbox", &at(&dir_for("Sandbox"))));
        assert!(!is_ours("tower-sandbox", &at(Path::new("/w/sandbox"))) && !is_ours("tower-sandbox", &json!({ "address": "t9" })));
        // The planner (`crate::plans`) is a brain by its name (no session of the user's), but no
        // space's brain and not the main one: nothing is routed to it, and no space's brain is called so.
        assert!(crate::brain::is_brain_name(crate::plans::PLANNER) && !is_space_brain(crate::plans::PLANNER));
        assert!(!is_ours(crate::plans::PLANNER, &at(&crate::plans::plans_dir().join("p1"))) && !is_ours(crate::plans::PLANNER, &at(&dir_for("planner"))));
        assert_eq!(name_for("Planner"), "tower-planner-space");
        assert_eq!(crate::brain::brain_of(&json!({ "address": "t9", "name": crate::plans::PLANNER })), Some(crate::plans::PLANNER), "never introduced, never a member, never asked about");
    }

    #[test]
    fn a_brain_starts_on_the_setting_unless_the_users_arguments_say() {
        let run = |cfg: &Config, name: &str| argv("claude", name, "/b", "/bin/tower", cfg).unwrap();
        // Nothing said: Opus 5.5 at high effort, for the main brain and a space's alike.
        let cfg = Config::default();
        assert_eq!(run(&cfg, "tower"), strings(&["claude", "-n", "tower", "--model", "claude-opus-5-5", "--effort", "high"]));
        assert_eq!(run(&cfg, "tower-sandbox"), strings(&["claude", "-n", "tower-sandbox", "--model", "claude-opus-5-5", "--effort", "high"]));
        // A model chosen, an effort chosen.
        let mut picked = Config::default();
        picked.brains.model = "claude-fable-5-2".into();
        assert_eq!(run(&picked, "tower"), strings(&["claude", "-n", "tower", "--model", "claude-fable-5-2", "--effort", "high"]));
        picked.brains.effort = "max".into();
        assert_eq!(run(&picked, "tower"), strings(&["claude", "-n", "tower", "--model", "claude-fable-5-2", "--effort", "max"]));
        // The user's own arguments win, each on its own, and theirs are passed as they are.
        let mut own = Config::default();
        own.claude_args = strings(&["--model", "sonnet", "--verbose"]);
        assert_eq!(run(&own, "tower"), strings(&["claude", "-n", "tower", "--effort", "high", "--model", "sonnet", "--verbose"]));
        own.claude_args = strings(&["--effort=low"]);
        assert_eq!(run(&own, "tower"), strings(&["claude", "-n", "tower", "--model", "claude-opus-5-5", "--effort=low"]));
        own.claude_args = strings(&["--model=haiku", "--effort", "medium"]);
        assert_eq!(run(&own, "tower"), strings(&["claude", "-n", "tower", "--model=haiku", "--effort", "medium"]));
        // Codex: the picker's model is not its business; a space's brain says whose hook it is.
        let mut codex = Config::default();
        codex.codex_model = "gpt-6.1-terra".into();
        let a = argv("codex", "tower", "/b", "/bin/tower", &codex).unwrap();
        assert!(!a.iter().any(|x| x.contains("claude-opus")), "{a:?}");
        assert!(a.contains(&"notify=[\"/bin/tower\", \"hook\", \"codex\"]".to_string()) && a.windows(2).any(|w| w == ["-m", "gpt-6.1-terra"]), "{a:?}");
        let a = argv("codex", "tower-sandbox", "/b", "/bin/tower", &codex).unwrap();
        assert!(a.contains(&"notify=[\"/bin/tower\", \"hook\", \"codex\", \"tower-sandbox\"]".to_string()), "{a:?}");
        assert!(argv("gemini", "tower", "/b", "/bin/tower", &codex).is_err());
    }

    #[test]
    fn what_a_brain_runs_on_is_read_off_its_command_line() {
        let s = started_with(&strings(&["claude", "-n", "tower", "--model", "sonnet", "--effort", "medium"]));
        assert_eq!((s.program.as_str(), s.model.as_deref(), s.effort.as_deref()), ("claude", Some("sonnet"), Some("medium")));
        // The last one wins; `=` works; a path to the program is the program.
        let s = started_with(&strings(&["/opt/bin/claude", "--model", "a", "--model=b", "--effort=xhigh"]));
        assert_eq!((s.program.as_str(), s.model.as_deref(), s.effort.as_deref()), ("claude", Some("b"), Some("xhigh")));
        let s = started_with(&strings(&["claude", "-n", "tower"]));
        assert_eq!((s.model, s.effort), (None, None), "started by something that named neither");
        let s = started_with(&strings(&["codex", "-C", "/b", "-m", "gpt-6.1-terra", "-c", "model_reasoning_effort=\"high\""]));
        assert_eq!((s.program.as_str(), s.model.as_deref(), s.effort.as_deref()), ("codex", Some("gpt-6.1-terra"), Some("high")));
        assert_eq!(started_with(&[]), Started::default());
    }

    fn runs(brain: &str, space: Option<&str>, argv: &[&str]) -> Running {
        Running { brain: brain.into(), space: space.map(String::from), address: "t1".into(), argv: strings(argv) }
    }

    #[test]
    fn the_report_says_the_setting_what_runs_and_whether_to_restart() {
        // An install from before the picker: its brain was started on sonnet at medium.
        let cfg = Config::default();
        let old = runs("tower", None, &["claude", "-n", "tower", "--model", "sonnet", "--effort", "medium"]);
        let v = report(&cfg, &[old.clone()]);
        assert_eq!((v["program"].as_str(), v["model"].as_str(), v["effort"].as_str()), (Some("claude"), Some("claude-opus-5-5"), Some("high")));
        assert_eq!(v["choices"]["models"].as_array().unwrap().iter().map(|m| (m["id"].as_str().unwrap(), m["label"].as_str().unwrap())).collect::<Vec<_>>(), MODELS.to_vec());
        assert_eq!(v["choices"]["efforts"], json!(["low", "medium", "high", "xhigh", "max"]));
        assert_eq!(v["running"], json!([{ "brain": "tower", "space": null, "address": "t1", "model": "sonnet", "effort": "medium", "restart": true }]));
        assert_eq!((v["restart_needed"].as_bool(), v["locked_by_args"].as_bool()), (Some(true), Some(false)));
        assert_eq!(v["says"], "The tower's brain runs on sonnet at medium effort. Opus 5.5 at high effort applies when it restarts.");
        // Started on the setting: nothing to do. A space's brain that wasn't: still a restart.
        let new = runs("tower", None, &["claude", "-n", "tower", "--model", "claude-opus-5-5", "--effort", "high"]);
        let v = report(&cfg, &[new.clone()]);
        assert_eq!((v["restart_needed"].as_bool(), v["says"].as_str()), (Some(false), Some("The tower's brain runs on Opus 5.5 at high effort.")));
        let v = report(&cfg, &[new.clone(), runs("tower-sandbox", Some("Sandbox"), &["claude", "-n", "tower-sandbox", "--model", "claude-opus-5-5", "--effort", "low"])]);
        assert_eq!((v["restart_needed"].as_bool(), v["running"][0]["restart"].as_bool(), v["running"][1]["restart"].as_bool(), v["running"][1]["space"].as_str()), (Some(true), Some(false), Some(true), Some("Sandbox")));
        // Nothing runs: nothing to restart.
        let v = report(&cfg, &[]);
        assert_eq!((v["restart_needed"].as_bool(), v["says"].as_str()), (Some(false), Some("No brain is running. The next one starts on Opus 5.5 at high effort.")));
        // The user's own arguments win: the report says so, and compares with what a start would really give.
        let mut own = Config::default();
        own.claude_args = strings(&["--model", "sonnet"]);
        let v = report(&own, &[runs("tower", None, &["claude", "-n", "tower", "--effort", "high", "--model", "sonnet"])]);
        assert_eq!((v["locked_by_args"].as_bool(), v["locked"]["model"].as_str(), v["locked"]["effort"].is_null(), v["restart_needed"].as_bool()), (Some(true), Some("sonnet"), true, Some(false)));
        assert!(v["says"].as_str().unwrap().ends_with("claude_args in config.toml sets the model itself, and that wins over the choice here."), "{}", v["says"]);
        assert_eq!(v["model"], "claude-opus-5-5", "the setting is still what the picker shows");
        // A Codex brain: the picker says where its model comes from.
        let mut codex = Config::default();
        codex.brain = "codex".into();
        let v = report(&codex, &[runs("tower", None, &argv("codex", "tower", "", "tower", &codex).unwrap().iter().map(String::as_str).collect::<Vec<_>>())]);
        assert_eq!((v["program"].as_str(), v["restart_needed"].as_bool(), v["locked_by_args"].as_bool()), (Some("codex"), Some(false), Some(false)));
        assert!(v["says"].as_str().unwrap().starts_with("The tower's brain is Codex"));
    }

    #[test]
    fn a_model_and_an_effort_as_typed() {
        assert_eq!(parse_model("claude-fable-5-2").unwrap(), "claude-fable-5-2");
        assert_eq!(parse_model(" Opus 5.5 ").unwrap(), "claude-opus-5-5", "by the picker's own name");
        assert_eq!(parse_model("opus").unwrap(), "opus", "an alias of the program is passed as it is");
        assert_eq!(parse_model("claude-opus-4-8[1m]").unwrap(), "claude-opus-4-8[1m]");
        for bad in ["", "  ", "--restart", "two words", "a\"b"] {
            assert!(parse_model(bad).is_err(), "{bad:?}");
        }
        assert_eq!((parse_effort("High").unwrap(), parse_effort("xhigh").unwrap()), ("high".to_string(), "xhigh".to_string()));
        assert!(parse_effort("extreme").is_err() && parse_effort("").is_err());
        assert_eq!((label("claude-haiku-5-5"), label("opus")), ("Haiku 5.5".to_string(), "opus".to_string()));
    }

    #[test]
    fn the_setting_is_kept_without_losing_the_rest_of_the_file() {
        let text = "# mine\nvoice = \"af_sky\"\nbrain = \"claude\"\nclaude_args = [\"--verbose\"]\n\n[tell]\nlevel = \"results\"\n\n[[setups]]\nname = \"web\"\naccount = \"orca\"\nrepos = [\"~/w/web\"]\n";
        let a = with_setting(text, Some("claude-sonnet-5-5"), None).unwrap();
        let cfg = Config::from_text(&a).unwrap();
        assert_eq!((cfg.brains.model.as_str(), cfg.brains.effort.as_str(), cfg.brains.per_space), ("claude-sonnet-5-5", "high", true));
        let b = with_setting(&a, None, Some("xhigh")).unwrap();
        let c = with_setting(&b, Some("claude-fable-5-2"), Some("max")).unwrap();
        let cfg = Config::from_text(&c).unwrap();
        assert_eq!((cfg.brains.model.as_str(), cfg.brains.effort.as_str()), ("claude-fable-5-2", "max"));
        assert_eq!((cfg.voice.as_str(), cfg.brain.as_str(), cfg.claude_args.clone(), cfg.tell.level, cfg.setups.len()), ("af_sky", "claude", strings(&["--verbose"]), crate::tell::Level::Results, 1));
        assert!(c.starts_with("# mine\n") && c.matches("[brains]").count() == 1 && c.matches("model = ").count() == 1, "{c}");
        // Per space off, set by hand, stays as it is.
        let off = with_setting("[brains]\nper_space = false\n", Some("claude-haiku-5-5"), None).unwrap();
        assert_eq!(off, "[brains]\nper_space = false\nmodel = \"claude-haiku-5-5\"\n");
        assert!(!Config::from_text(&off).unwrap().brains.per_space);
        // A file that doesn't read is left alone.
        assert!(with_setting("voice = [", Some("x"), None).is_err());
    }

    fn mission() -> Value {
        json!({
            "name": "Sandbox", "goal": "Fix the login redirect", "done_when": "the redirect test passes", "standing": false, "state": "open",
            "sessions": [
                { "address": "t5", "name": "sandbox eyzflow", "status": "working", "worktree": "/w/.tower-worktrees/sandbox/eyzflow", "port": 3001 },
                { "address": "t6", "name": "sandbox mediaroom", "status": "", "worktree": null, "port": null },
            ],
            "requests": [{ "id": "a7", "title": "Fix the login redirect", "state": "open", "done": 1, "total": 2 }],
        })
    }

    #[test]
    fn a_space_brains_instructions_are_the_main_ones_and_its_space_only() {
        let dir = Path::new("/h/brains/sandbox");
        let base = "<!-- Generated by the tower. -->\n\n# You are the tower\n\nThe core.\n\n# Behaviors\n\n### Talk plainly\n\nShort sentences.\n";
        let text = space_prompt(base, "Sandbox", "tower-sandbox", dir, &mission());
        // The same core and behaviors, under a line that says whose file it is.
        assert!(text.starts_with("<!-- Generated by the tower. -->\n\n> You are the tower of the space \"Sandbox\", not the main tower."), "{text}");
        assert!(text.contains("# You are the tower\n\nThe core.") && text.contains("### Talk plainly\n\nShort sentences."));
        let section = &text[text.find("# Your space: \"Sandbox\"").expect("its section")..];
        assert!(text.find("# Behaviors").unwrap() < text.find("# Your space").unwrap(), "its section comes last");
        // Its name, goal and done-when, its sessions, its requests, its thread.
        for has in [
            "your terminal is `tower-sandbox`",
            "- Its goal: Fix the login redirect",
            "- Done when: the redirect test passes",
            "  - t5 \"sandbox eyzflow\": working, in /w/.tower-worktrees/sandbox/eyzflow, dev port 3001",
            "  - t6 \"sandbox mediaroom\": starting",
            "  - a7 \"Fix the login redirect\": open, 1 of 2 tasks settled",
            "`tower thread \"Sandbox\"`",
            "`tower decisions --space \"Sandbox\"`",
            // Only its own space, and the way to the main tower for the rest.
            "You handle this space and nothing else",
            "`tower queue tower \"<what it is, and what you need back>\"`",
            "always `tower start --space \"Sandbox\" …`",
            "`tower queue tower-sandbox \"…\"`",
            "`/h/brains/sandbox/CLAUDE.md`",
        ] {
            assert!(section.contains(has), "missing {has:?} in:\n{section}");
        }
        // Another space's sessions and requests are not in it.
        let other = json!({ "name": "Docs", "goal": "Write the guide", "sessions": [{ "address": "t9", "name": "docs site", "status": "idle" }], "requests": [{ "id": "a9", "title": "Write the guide", "state": "open", "done": 0, "total": 1 }] });
        let docs = space_section("Docs", "tower-docs", Path::new("/h/brains/docs"), &other);
        assert!(docs.contains("t9 \"docs site\"") && docs.contains("a9 ") && !docs.contains("t5") && !docs.contains("a7") && !docs.contains("Sandbox"), "{docs}");
        assert!(!section.contains("t9") && !section.contains("a9") && !section.contains("Docs"));
        // A space with no goal, a standing one, one with nothing in it yet.
        let own = space_section("M5 Pro", "tower-m5-pro", dir, &json!({ "name": "M5 Pro", "goal": "", "sessions": [], "requests": [] }));
        assert!(own.contains("It has no goal: it is the user's own place") && own.contains("Its sessions: none") && own.contains("Its requests: none open"), "{own}");
        let kept = space_section("Ops", "tower-ops", dir, &json!({ "name": "Ops", "goal": "Keep prod healthy", "standing": true }));
        assert!(kept.contains("A standing space") && kept.contains("- What it is for: Keep prod healthy") && !kept.contains("Its goal:"), "{kept}");
        // A base without the generated line: the lead still comes first.
        assert!(space_prompt("# Core\n\nx", "Docs", "tower-docs", dir, &other).starts_with("> You are the tower of the space \"Docs\""));
    }

    #[test]
    fn a_space_brains_folder_holds_its_instructions_its_hooks_and_its_card() {
        let dir = std::env::temp_dir().join(format!("tower-brains-folder-{}", std::process::id())).join("brains/sandbox");
        let _ = std::fs::remove_dir_all(&dir);
        let base = "<!-- Generated by the tower. -->\n\n# You are the tower\n\nThe core.\n\n# Behaviors\n";
        assert!(write_folder(&dir, base, "/bin/tower", "Sandbox", &mission()).unwrap(), "new: its instructions are written");
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        // Claude Code and Codex read the same instructions.
        assert_eq!(read("CLAUDE.md"), read("AGENTS.md"));
        assert_eq!(read("CLAUDE.md"), space_prompt(base, "Sandbox", "tower-sandbox", &dir, &mission()));
        assert!(read("CLAUDE.md").contains(&format!("`{}`", dir.join("CLAUDE.md").display())), "it is told where its own file is");
        let hooks: Value = serde_json::from_str(&read(".claude/settings.json")).unwrap();
        assert_eq!(hooks, settings("/bin/tower", Some("tower-sandbox")));
        let card: Card = serde_json::from_str(&read("brain.json")).unwrap();
        assert_eq!(card, Card { name: "tower-sandbox".into(), space: "Sandbox".into(), mission: mission() });
        // Written again as it is: nothing changed, so there is nothing to read again.
        assert!(!write_folder(&dir, base, "/bin/tower", "Sandbox", &mission()).unwrap());
        // Its space moved on (a session finished), or the behaviors changed: the instructions follow.
        let mut later = mission();
        later["sessions"][0]["status"] = json!("done");
        assert!(write_folder(&dir, base, "/bin/tower", "Sandbox", &later).unwrap());
        assert!(read("CLAUDE.md").contains("t5 \"sandbox eyzflow\": done"));
        assert!(write_folder(&dir, &format!("{base}\n### A new rule\n\nBe brief.\n"), "/bin/tower", "Sandbox", &later).unwrap());
        assert!(read("CLAUDE.md").contains("### A new rule") && read("CLAUDE.md").contains("# Your space: \"Sandbox\""));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_space_brains_hooks_say_whose_they_are() {
        let own = settings("/bin/tower", Some("tower-sandbox"));
        assert_eq!(own["hooks"]["Stop"][0]["hooks"][0]["command"], "'/bin/tower' hook claude tower-sandbox");
        assert_eq!(own["hooks"]["Notification"][0]["hooks"][0]["command"], "'/bin/tower' hook claude-notify tower-sandbox");
        // The orb's working line is the main brain's: a space's brain reports none.
        assert!(own["hooks"]["UserPromptSubmit"].is_null() && own["hooks"]["PostToolUse"].is_null());
        assert_eq!(own["hooks"]["PreToolUse"].as_array().map(Vec::len), Some(1));
        assert_eq!(own["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "'/bin/tower' hook brain-guard");
        // The main brain's are as they were.
        let main = settings("/bin/tower", None);
        assert_eq!(main["hooks"]["Stop"][0]["hooks"][0]["command"], "'/bin/tower' hook claude");
        assert_eq!(main["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "'/bin/tower' hook activity-prompt");
        assert_eq!(main["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "'/bin/tower' hook activity-pre");
        assert_eq!(main["hooks"]["PreToolUse"][1]["hooks"][0]["command"], "'/bin/tower' hook brain-guard");
        assert_eq!(main["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "'/bin/tower' hook activity-post");
        assert_eq!(main["permissions"], own["permissions"]);
        assert!(main["permissions"]["allow"].as_array().unwrap().contains(&json!("Bash(/bin/tower *)")));
    }

    fn space(sessions: usize) -> SpaceNow {
        SpaceNow { sessions, ..Default::default() }
    }

    fn look(lives: &mut BTreeMap<String, Life>, spaces: &[(&str, SpaceNow)], up: &[&str], now: u64) -> Vec<Do> {
        let spaces: BTreeMap<String, SpaceNow> = spaces.iter().map(|(n, s)| (n.to_string(), s.clone())).collect();
        step(lives, &spaces, &strings(up), true, true, now)
    }

    #[test]
    fn a_space_gets_its_brain_with_its_first_session_and_keeps_it_through_a_short_gap() {
        const S: u64 = 1_000;
        let mut lives = BTreeMap::new();
        let t0 = 1_000_000;
        // A space with no session yet (a goal set ahead): no brain.
        assert!(look(&mut lives, &[("Sandbox", space(0))], &[], t0).is_empty());
        // Its first live session: started, once.
        assert_eq!(look(&mut lives, &[("Sandbox", space(1))], &[], t0 + 20 * S), [Do::Start("Sandbox".into())]);
        assert!(look(&mut lives, &[("Sandbox", space(2))], &["Sandbox"], t0 + 40 * S).is_empty());
        // Its only session restarts: for two minutes there is none, and the brain stays.
        assert!(look(&mut lives, &[("Sandbox", space(0))], &["Sandbox"], t0 + 60 * S).is_empty());
        assert!(look(&mut lives, &[("Sandbox", space(0))], &["Sandbox"], t0 + 160 * S).is_empty());
        assert!(look(&mut lives, &[("Sandbox", space(1))], &["Sandbox"], t0 + 180 * S).is_empty(), "back before the grace ran out");
        // Then the space empties for good: after the grace, stopped.
        assert!(look(&mut lives, &[("Sandbox", space(0))], &["Sandbox"], t0 + 200 * S).is_empty());
        assert!(look(&mut lives, &[("Sandbox", space(0))], &["Sandbox"], t0 + 180 * S + IDLE_GRACE_MS - S).is_empty());
        assert_eq!(look(&mut lives, &[("Sandbox", space(0))], &["Sandbox"], t0 + 180 * S + IDLE_GRACE_MS), [Do::Stop { space: "Sandbox".into(), why: "no session there for a few minutes" }]);
        assert!(lives.is_empty(), "nothing is kept of a space that is over");
        // A standing space gets one like any other; one without a goal too (`step` doesn't ask).
        assert_eq!(look(&mut lives, &[("Ops", space(3)), ("Docs", space(0))], &[], t0), [Do::Start("Ops".into())]);
    }

    #[test]
    fn a_closed_archived_or_vanished_space_loses_its_brain_at_once() {
        let mut lives = BTreeMap::new();
        let t0 = 1_000_000;
        assert_eq!(look(&mut lives, &[("Shop", space(1))], &[], t0), [Do::Start("Shop".into())]);
        // Closed (or archived) with a session still listed for a moment: stopped, and not started again.
        let over = SpaceNow { sessions: 1, over: true, gone: false };
        assert_eq!(look(&mut lives, &[("Shop", over.clone())], &["Shop"], t0 + 1), [Do::Stop { space: "Shop".into(), why: "its space is closed" }]);
        assert!(look(&mut lives, &[("Shop", over)], &[], t0 + 2).is_empty());
        // Herdr closed the space itself: its sessions are gone and so is the space.
        assert_eq!(look(&mut lives, &[("Docs", space(1))], &[], t0), [Do::Start("Docs".into())]);
        let gone = SpaceNow { sessions: 0, over: false, gone: true };
        assert_eq!(look(&mut lives, &[("Docs", gone)], &["Docs"], t0 + 5_000), [Do::Stop { space: "Docs".into(), why: "its space is gone" }]);
        // A brain that runs for a space the tower no longer knows at all: after the grace.
        assert!(look(&mut lives, &[], &["Old"], t0).is_empty());
        assert_eq!(look(&mut lives, &[], &["Old"], t0 + IDLE_GRACE_MS), [Do::Stop { space: "Old".into(), why: "no session there for a few minutes" }]);
    }

    #[test]
    fn one_brain_for_everything_stops_the_spaces_own() {
        let mut lives = BTreeMap::new();
        let spaces: BTreeMap<String, SpaceNow> = [("Shop".to_string(), space(2))].into();
        assert_eq!(step(&mut lives, &spaces, &[], true, true, 10), [Do::Start("Shop".into())]);
        // `[brains] per_space = false`: what runs is stopped, and nothing starts.
        assert_eq!(step(&mut lives, &spaces, &strings(&["Shop"]), false, true, 20), [Do::Stop { space: "Shop".into(), why: "one brain for everything ([brains] per_space = false)" }]);
        assert!(step(&mut lives, &spaces, &[], false, true, 30).is_empty());
        // Without the main brain, a space's own isn't started (one that runs stays).
        assert!(step(&mut lives, &spaces, &[], true, false, 40).is_empty());
        assert!(step(&mut lives, &spaces, &strings(&["Shop"]), true, false, 50).is_empty());
        assert_eq!(step(&mut lives, &spaces, &[], true, true, 60), [Do::Start("Shop".into())]);
    }

    #[test]
    fn a_brain_that_keeps_dying_is_started_later_each_time() {
        assert_eq!([1, 2, 3, 4, 5, 6, 7, 20].map(backoff_ms), [15_000, 30_000, 60_000, 120_000, 240_000, 480_000, BACKOFF_MAX_MS, BACKOFF_MAX_MS]);
        let mut lives = BTreeMap::new();
        let shop = [("Shop", space(1))];
        let mut now = 1_000_000;
        // It never comes up: looked at every 20 seconds for half an hour.
        let mut starts = vec![];
        for _ in 0..90 {
            if !look(&mut lives, &shop, &[], now).is_empty() {
                starts.push(now - 1_000_000);
            }
            now += 20_000;
        }
        assert_eq!(starts, [0, 20_000, 60_000, 120_000, 240_000, 480_000, 960_000, 1_560_000], "not in a tight loop: the gaps grow to ten minutes");
        // It comes up and runs steadily: the next death is answered at once again.
        assert!(look(&mut lives, &shop, &["Shop"], now).is_empty());
        assert!(look(&mut lives, &shop, &["Shop"], now + STEADY_MS).is_empty());
        assert_eq!((lives["Shop"].tries, lives["Shop"].next_try_ms), (0, 0));
        assert_eq!(look(&mut lives, &shop, &[], now + STEADY_MS + 20_000), [Do::Start("Shop".into())]);
        // Held off while its space is being closed.
        let mut lives = BTreeMap::new();
        lives.insert("Shop".to_string(), Life { last_live_ms: now, held_until_ms: now + 60_000, ..Default::default() });
        assert!(look(&mut lives, &shop, &[], now + 30_000).is_empty());
        assert_eq!(look(&mut lives, &shop, &[], now + 60_000), [Do::Start("Shop".into())]);
    }

    fn known(name: &str, space: Option<&str>, status: &str, live: bool) -> Known {
        Known { name: name.into(), space: space.map(String::from), address: format!("t-{name}"), live, status: SessionStatus::parse(status), ..Default::default() }
    }

    #[test]
    fn what_is_about_a_space_goes_to_its_brain_and_the_rest_to_the_main_one() {
        let brains = [known("tower", None, "idle", true), known("tower-sandbox", Some("Sandbox"), "working", true), known("tower-docs", Some("Docs"), "", true), known("tower-shop", Some("Shop"), "blocked", true), known("tower-old", Some("Old"), "idle", false)];
        let own = To::Space { brain: "tower-sandbox".into(), address: "t-tower-sandbox".into(), space: "Sandbox".into() };
        // Its space's brain, busy or not (a busy one takes it after its turn); the name in any case.
        assert_eq!(route(Some("Sandbox"), true, &brains), own);
        assert_eq!(route(Some("sandbox "), true, &brains), own);
        // No space, or a space with no brain of its own: the main one.
        assert_eq!(route(None, true, &brains), To::Main);
        assert_eq!(route(Some(""), true, &brains), To::Main);
        assert_eq!(route(Some("Elsewhere"), true, &brains), To::Main);
        // Every way a space's brain can't take it falls back to the main one: starting, stuck on its own screen, ended.
        assert_eq!(route(Some("Docs"), true, &brains), To::Main, "still starting");
        assert_eq!(route(Some("Shop"), true, &brains), To::Main, "waiting on a question of its own");
        assert_eq!(route(Some("Old"), true, &brains), To::Main, "its terminal ended");
        assert_eq!(route(Some("Sandbox"), true, &[known("tower-sandbox", Some("Sandbox"), "error", true)]), To::Main);
        // One brain for everything: always the main one.
        assert_eq!(route(Some("Sandbox"), false, &brains), To::Main);
        // How a space's view says its tower stands.
        assert_eq!(brains.iter().map(Known::word).collect::<Vec<_>>(), ["ready", "working", "starting", "blocked", "off"]);
        assert_eq!(known("tower-x", Some("X"), "done", true).word(), "ready");
    }

    #[test]
    fn every_kind_of_note_goes_to_the_brain_of_its_sessions_space() {
        let brains = [known("tower", None, "idle", true), known("tower-sandbox", Some("Sandbox"), "idle", true), known("tower-docs", Some("Docs"), "", true)];
        let own = To::Space { brain: "tower-sandbox".into(), address: "t-tower-sandbox".into(), space: "Sandbox".into() };
        // Where each session is, as Herdr says: t5 in Sandbox (it has a brain), t6 in Docs (its brain is starting), t7 in Shop (none), t8 in no space.
        let space_of = |a: &str| match a {
            "t5" => Some("Sandbox".to_string()),
            "t6" => Some("Docs".to_string()),
            "t7" => Some("Shop".to_string()),
            _ => None,
        };
        let to = |given: Option<&str>, text: &str| route(note_space(given, text, &space_of).as_deref(), true, &brains);
        // The radar's reports come with their session's space; a question's note names its session.
        let notes = [
            (Some("Sandbox"), "[watch] t5 (sandbox app) is now done. You had asked it: fix the login."),
            (Some("Sandbox"), "[queue] t5 (sandbox app) is gone, with 1 instruction(s) queued for it (#4)"),
            (Some("Sandbox"), "[agenda] a3.2 \"fix login\": t5 (sandbox app) hadn't taken its instruction"),
            (Some("Sandbox"), "[space] \"Sandbox\" looks finished: every request there is done and nothing waits for the user."),
            (None, "[question] t5 q0123456789ab {\"kind\":\"menu\"} policy=normal"),
            (None, "[question-text] t5 q1 {\"tabs\":[]} policy=normal"),
            (None, "[watch] t5 (sandbox app) is waiting on its first-run question"),
        ];
        for (given, text) in notes {
            assert_eq!(to(given, text), own, "{text}");
            // One brain for everything: the main one, as before.
            assert_eq!(route(note_space(given, text, &space_of).as_deref(), false, &brains), To::Main, "{text}");
        }
        // The same notes about a session whose space has no brain that can take them: the main one, never dropped.
        for text in ["[question] t6 q1 {} policy=normal", "[question] t7 q1 {} policy=normal", "[question] t8 q1 {} policy=normal", "[watch] t9 (gone) has exited."] {
            assert_eq!(to(None, text), To::Main, "{text}");
        }
        assert_eq!(to(Some("Docs"), "[watch] t6 (docs site) is now done."), To::Main, "its space's brain is still starting");
        assert_eq!(to(Some("Shop"), "[agenda] a4.1: t7 (web) shows no progress"), To::Main);
        // About no one session and no space (a request with several tasks is all done): the main one.
        assert_eq!(to(None, "[agenda] a3 is done: every task is settled (2/2 verified)"), To::Main);
        assert_eq!(to(None, "[behaviors] edit \"Talk plainly\""), To::Main);
        // The space a note comes with wins over what its words name.
        assert_eq!(to(Some("Shop"), "[watch] t5 (sandbox app) is now done."), To::Main);

        // A session's own message (`tower queue tower "…"`) reaches its space's tower.
        assert_eq!(session_to(Some("Sandbox"), false, true, true, &brains), own);
        // From a space without one (or whose own can't take it), or with one brain for everything: the main tower.
        assert_eq!(session_to(Some("Shop"), false, true, true, &brains), To::Main);
        assert_eq!(session_to(Some("Docs"), false, true, true, &brains), To::Main);
        assert_eq!(session_to(None, false, true, true, &brains), To::Main);
        assert_eq!(session_to(Some("Sandbox"), false, true, false, &brains), To::Main);
        // A space's own brain writing to the main tower hands over what crosses spaces: it goes where it says.
        assert_eq!(session_to(Some("Sandbox"), true, true, true, &brains), To::Main);
        // Written to a space's tower by name: as addressed (`Main` here means: not moved).
        assert_eq!(session_to(Some("Sandbox"), false, false, true, &brains), To::Main);
    }

    #[test]
    fn a_request_goes_to_the_space_it_was_asked_in() {
        let brains = [known("tower", None, "working", true), known("tower-sandbox", Some("Sandbox"), "idle", true), known("tower-shop", Some("Shop"), "blocked", true)];
        // Asked of the tower (no space): the main brain, whatever runs.
        assert_eq!(route(None, true, &brains), To::Main);
        // Asked in a space's view: that space's own.
        assert!(matches!(route(Some("Sandbox"), true, &brains), To::Space { ref brain, .. } if brain == "tower-sandbox"));
        // Its own can't take it (stuck, not running, never started), or there is one brain for everything: the main brain.
        for space in ["Shop", "Docs", "sandbox-2"] {
            assert_eq!(route(Some(space), true, &brains), To::Main, "{space}");
        }
        assert_eq!(route(Some("Sandbox"), false, &brains), To::Main);
        assert_eq!(route(Some("Sandbox"), true, &brains[..1]), To::Main);
    }
}
