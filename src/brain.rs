//! The brain: a Claude Code or Codex session in a Herdr terminal named
//! `tower`, with the tower's instructions, driving every other session
//! through `hc`.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::config::{self, Config};
use crate::hc;

pub const NAME: &str = "tower";
pub const VOICE_NAME: &str = "tower-voice";

/// The brain's terminal: `tower`, or `HERDER_TOWER_BRAIN` (a second tower
/// beside the first, for trying changes out).
pub fn name() -> String {
    std::env::var("HERDER_TOWER_BRAIN").ok().filter(|n| !n.is_empty()).unwrap_or_else(|| NAME.into())
}

/// The brains' instructions, generated from the core and the enabled
/// behaviors: the main brain's, and every space brain's that has a folder
/// (its own section stays as its card has it, see `brains`).
pub fn write_prompt() -> Result<()> {
    let dir = config::brain_dir();
    std::fs::create_dir_all(&dir)?;
    let text = crate::behaviors::Store::open().prompt();
    std::fs::write(dir.join("CLAUDE.md"), &text)?;
    std::fs::write(dir.join("AGENTS.md"), &text)?;
    crate::brains::rewrite_all();
    Ok(())
}

/// Writes the brain's folder: its instructions (CLAUDE.md for Claude Code,
/// AGENTS.md for Codex) and Claude Code's hooks and permissions.
pub fn write_files() -> Result<()> {
    let dir = config::brain_dir();
    std::fs::create_dir_all(dir.join(".claude"))?;
    // The tower's own behaviors, first time (and unedited ones that changed);
    // the brain's memory notes that are standing preferences become behaviors.
    let store = crate::behaviors::Store::open();
    let seeded = store.seed()?;
    if !seeded.is_empty() {
        eprintln!("behaviors: seeded {}", seeded.join(", "));
    }
    let moved = crate::behaviors::migrate_memory(&store, &crate::behaviors::brain_memory_dir())?;
    if !moved.is_empty() {
        eprintln!("behaviors: from the brain's memory: {}", moved.join(", "));
    }
    write_prompt()?;
    let tower = config::tower_bin().display().to_string();
    std::fs::write(dir.join(".claude/settings.json"), serde_json::to_string_pretty(&crate::brains::settings(&tower, None))?)?;
    Ok(())
}

/// Starts a brain's program in a hidden Herdr terminal called `name`, in
/// its folder. Returns its address.
fn run(name: &str, dir: &str, argv: &[String], labels: &[String]) -> Result<String> {
    // A finished brain keeps its name until removed.
    let _ = hc::run(&["rm", name]);
    let mut args: Vec<&str> = vec!["run", "--bg", "--json", "--name", name, "--no-notify", "--no-parent", "--cwd", dir];
    for l in labels {
        args.extend(["--label", l.as_str()]);
    }
    args.push("--");
    args.extend(argv.iter().map(String::as_str));
    let out = hc::run(&args)?;
    if out.code != 0 {
        bail!("starting the brain: {}{}", out.stderr.trim(), out.stdout.trim());
    }
    let v: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap_or_default();
    Ok(v["address"].as_str().unwrap_or(name).to_string())
}

/// Starts the brain unless it runs. Returns its address.
pub fn launch(cfg: &Config, which: &str) -> Result<String> {
    if let Some(t) = hc::get(NAME).filter(|t| t["state"] == "live") {
        return Ok(t["address"].as_str().unwrap_or(NAME).to_string());
    }
    write_files()?;
    let dir = config::brain_dir().display().to_string();
    let tower = config::tower_bin().display().to_string();
    // Its model and effort are the brains' setting (`tower model`); every
    // step the brain takes is silence for the user.
    let argv = crate::brains::argv(which, NAME, &dir, &tower, cfg)?;
    let address = run(NAME, &dir, &argv, &[])?;
    settle(&address);
    Ok(address)
}

/// Starts a space's own brain unless it runs: its folder is written first
/// (`mission`: the space as the state's `missions` has it), and its
/// terminal says which space it is for. Returns its address; `settle` then
/// sees it past its first-run question.
pub fn launch_space(space: &str, mission: &serde_json::Value, cfg: &Config) -> Result<String> {
    let name = crate::brains::name_for(space);
    if let Some(t) = hc::get(&name).filter(|t| t["state"] == "live") {
        return Ok(t["address"].as_str().unwrap_or(&name).to_string());
    }
    let (dir, _) = crate::brains::write_space_files(space, mission)?;
    let dir = dir.display().to_string();
    let tower = config::tower_bin().display().to_string();
    let argv = crate::brains::argv(&cfg.brain, &name, &dir, &tower, cfg)?;
    run(&name, &dir, &argv, &[format!("{}={space}", crate::brains::LABEL_SPACE)])
}

/// Gets past a first-run folder trust question, which is ours to answer:
/// the folder is the tower's own.
pub fn settle(address: &str) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(25) {
        thread::sleep(Duration::from_millis(700));
        let Some(t) = hc::get(address) else { return };
        if t["state"] != "live" {
            return;
        }
        let screen = hc::run(&["read", address, "--lines", "0"]).map(|o| o.stdout).unwrap_or_default();
        if let Some(keys) = trust_answer(&screen) {
            let mut args = vec!["keys", address];
            args.extend(keys);
            let _ = hc::run(&args);
            continue;
        }
        if matches!(hc::status_word(&t).as_deref(), Some("idle") | Some("working")) {
            return;
        }
    }
}

/// The keys that answer "yes" to a folder-trust question on screen, if one
/// is showing: Claude Code's ("Yes, I trust this folder", sometimes below a
/// highlighted "No, exit") and Codex's.
fn trust_answer(screen: &str) -> Option<Vec<&'static str>> {
    let lower = screen.to_lowercase();
    if !lower.contains("trust") {
        return None;
    }
    let lines: Vec<&str> = screen.lines().collect();
    let yes = lines.iter().position(|l| {
        let l = l.to_lowercase();
        l.contains("yes") && (l.contains("trust") || l.contains("proceed") || l.contains("allow"))
    })?;
    let cursor =
        lines.iter().position(|l| l.trim_start().starts_with('❯') || l.trim_start().starts_with('›') || l.trim_start().starts_with('>'));
    match cursor {
        Some(c) if c < yes => Some(std::iter::repeat_n("down", yes - c).chain(["enter"]).collect()),
        Some(c) if c > yes => Some(std::iter::repeat_n("up", c - yes).chain(["enter"]).collect()),
        _ => Some(vec!["enter"]),
    }
}

/// A brain to hand something to.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// Its terminal's name (`tower`, `tower-<space>`): whose turn it is.
    pub name: String,
    /// How Herdr finds it: its address, or its name.
    pub terminal: String,
    /// Its space (None: the main brain).
    pub space: Option<String>,
}

impl Target {
    /// The main brain.
    pub fn main() -> Target {
        Target { name: name(), terminal: name(), space: None }
    }
}

/// Hands what the user said to the brain, with the radar's view of every
/// agent (`radar`, may be empty), so a status question needs no lookup.
/// `source`: `mac`, `voice` or `phone:<device id>`, for the chat.
pub fn ask(text: &str, radar: &str, source: &str) -> Result<()> {
    ask_as(&Target::main(), text, radar, source)
}

/// `ask`, of the brain `to`. What is asked of a space's own brain is kept
/// in the history as that space's request.
pub fn ask_as(to: &Target, text: &str, radar: &str, source: &str) -> Result<()> {
    let mut msg = if radar.is_empty() { format!("[voice] {text}") } else { format!("[voice] {text} {radar}") };
    if let Some(hint) = split_hint(text, &crate::dispatch::known_projects()) {
        msg.push(' ');
        msg.push_str(&hint);
    }
    note_to(to, &msg, false)?;
    set_turn(&to.name, Turn { question: false, ..turn_of(&to.name) });
    // The orb's working line is the main brain's.
    if to.space.is_none() {
        crate::activity::start();
    }
    let (text, source, space) = (text.to_string(), source.to_string(), to.space.clone());
    crate::history::later(move || crate::history::record_request(&text, &source, space));
    Ok(())
}

/// A note after a request that reads as separate tasks for different
/// projects: `[split?] 1) eyzflow: "..." 2) herder tower: "..."`.
pub fn split_hint(text: &str, projects: &[crate::split::Project]) -> Option<String> {
    let parts = crate::split::separate(text, projects)?;
    let list: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(i, (folder, part))| {
            let name = projects.iter().find(|p| &p.folder == folder).map(|p| p.name().to_string()).unwrap_or_else(|| folder.clone());
            format!("{}) {name}: \"{part}\"", i + 1)
        })
        .collect();
    Some(format!("[split?] {}", list.join(" ")))
}

/// What a brain's turn is, by how it began. Each brain has its own: set as
/// a note goes in, and by its transcript as the turn starts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Turn {
    /// A watch reporting back, or the send queue's news (not a reply to
    /// the user): its words come after the bell.
    pub watch: bool,
    /// A note from the tower itself (`[watch]`, `[queue]`, `[agenda]`,
    /// `[session]`, `[space]`): at a quiet level nothing it writes is
    /// spoken or kept in the chat (see `tell`).
    pub note: bool,
    /// A `[question]` note (it recommends answers with `tower recommend`):
    /// nothing it writes is spoken or kept in the chat.
    pub question: bool,
}

impl Turn {
    /// The turn a prompt begins.
    pub fn of_prompt(prompt: &str) -> Turn {
        Turn { watch: is_news_prompt(prompt), note: is_note_prompt(prompt), question: is_question_prompt(prompt) }
    }
}

static TURNS: std::sync::Mutex<std::collections::BTreeMap<String, Turn>> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// The turn the brain called `brain` is on.
pub fn turn_of(brain: &str) -> Turn {
    TURNS.lock().unwrap_or_else(|e| e.into_inner()).get(brain).copied().unwrap_or_default()
}

pub fn set_turn(brain: &str, turn: Turn) {
    TURNS.lock().unwrap_or_else(|e| e.into_inner()).insert(brain.to_string(), turn);
}

/// A brain is gone: nothing is kept of its turn.
pub fn forget_turn(brain: &str) {
    TURNS.lock().unwrap_or_else(|e| e.into_inner()).remove(brain);
}

/// A note that is news for the user, not a request: a watch's report, or
/// the send queue saying what it delivered. (`contains`: `hc send` from
/// inside Herdr wraps a note in `<hc from="...">`.)
pub fn is_news_prompt(text: &str) -> bool {
    !from_the_user(text) && (text.contains("[watch]") || text.contains("[queue]"))
}

/// The user said or typed it: the prompt, or a line of it (what waited
/// while the brain worked may come in as one), begins with `[voice]`, as
/// the tower sends a request (`hc send` from inside Herdr wraps it in `<hc
/// from="...">`). A note that only has the word somewhere in it isn't the
/// user's: a session of this very project writes "the [voice] test passes",
/// and its note must stay a note.
fn from_the_user(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        let line = match line.strip_prefix("<hc ") {
            Some(rest) => rest.split_once('>').map(|(_, r)| r).unwrap_or(line),
            None => line,
        };
        line.trim_start().starts_with("[voice]")
    })
}

/// A turn's prompt is a note, not something the user said: from the tower
/// itself (a watch's report, the send queue's news, the agenda ledger's, a
/// space that is ready to close), or from a session that wrote to the brain
/// (`[session]`, see `peers`). A turn that starts with one is a note turn.
pub fn is_note_prompt(text: &str) -> bool {
    !from_the_user(text) && NOTE_TAGS.iter().any(|tag| text.contains(tag))
}

/// What a note starts with (`[space]`: a space is ready to close and wants
/// its closing report, see `missions`).
const NOTE_TAGS: [&str; 5] = ["[watch]", "[queue]", "[agenda]", "[session]", "[space]"];

/// The session a note is about: the first address in it (`t5`, `vps/t2`).
/// None for a note about no one session.
pub fn note_session(prompt: &str) -> Option<String> {
    let start = NOTE_TAGS.iter().filter_map(|tag| prompt.find(tag).map(|i| i + tag.len())).min()?;
    prompt[start..]
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != '-' && c != '_'))
        .find(|w| is_address(w))
        .map(String::from)
}

/// `t5`, `t7h`, `vps/t2`: a Herdr terminal's address.
fn is_address(word: &str) -> bool {
    let local = word.rsplit('/').next().unwrap_or(word);
    let mut c = local.chars();
    c.next() == Some('t') && c.next().is_some_and(|d| d.is_ascii_digit()) && local.len() <= 8 && local.chars().all(|c| c.is_ascii_alphanumeric())
}

/// A terminal by this name is a brain: `tower`, the one this tower uses, or
/// a space's own (`tower-<space>`). Never `tower-voice`, the daemon's.
pub fn is_brain_name(name: &str) -> bool {
    name == NAME || name == self::name() || (name.starts_with("tower-") && name != VOICE_NAME)
}

/// Why a session can't be given `name` (None: it can): `tower` and
/// `tower-…` name the towers' own terminals, and a session called so would
/// be taken for one (no watch, no question, no decision). Says a name that
/// can be used instead.
pub fn not_a_sessions_name(name: &str) -> Option<String> {
    let n = name.trim();
    if !is_brain_name(n) && n != VOICE_NAME {
        return None;
    }
    let rest = n.strip_prefix("tower").unwrap_or("").trim_matches('-');
    let instead = if rest.is_empty() { "work-on-tower".to_string() } else { format!("{rest}-tower") };
    Some(format!("a session can't be named \"{n}\": \"tower\" and \"tower-…\" are the names of the towers' own terminals. Give it another name, for example \"{instead}\"."))
}

/// The terminal `t` (as `hc get` or `hc ls` has it) as a brain: its own name,
/// the one it was started with (`hc run --name`), says so. A session in a
/// space's tab has no name of its own, only the label `tower.name`: it is
/// never a brain, whatever it is called (`tower-pr-app` in a space named
/// "tower-pr", or a session someone called `tower-fix`).
pub fn brain_of(t: &serde_json::Value) -> Option<&str> {
    t["name"].as_str().filter(|n| is_brain_name(n))
}

/// A turn's prompt is the tower's own quiet note (`[question]`,
/// `[question-text]`, `[behaviors]`), not something the user said.
pub fn is_question_prompt(prompt: &str) -> bool {
    (prompt.contains("[question]") || prompt.contains("[question-text]") || prompt.contains("[behaviors]")) && !from_the_user(prompt)
}

/// The session a note of any kind names (a `[question]` too): who it is about, to find its space.
pub fn session_in(text: &str) -> Option<String> {
    const TAGS: [&str; 7] = ["[watch]", "[queue]", "[agenda]", "[session]", "[space]", "[question]", "[question-text]"];
    let start = TAGS.iter().filter_map(|tag| text.find(tag).map(|i| i + tag.len())).min()?;
    text[start..]
        .split_whitespace()
        .take(12)
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '/' && c != '-' && c != '_'))
        .find(|w| is_address(w))
        .map(String::from)
}

/// A message from the tower itself (a watch firing), for the main brain.
pub fn note(text: &str) -> Result<()> {
    note_as(text, false)
}

/// `note`; `record`: the report is written down only, not a chat message
/// (a finish at a quiet level: see `tell`).
pub fn note_as(text: &str, record: bool) -> Result<()> {
    note_to(&Target::main(), text, record)
}

/// `note_as`, for the brain `to` (the main one, or a space's own).
pub fn note_to(to: &Target, text: &str, record: bool) -> Result<()> {
    let Some(t) = hc::get(&to.terminal).filter(|t| t["state"] == "live") else {
        bail!("brain-absent");
    };
    let status = hc::status_word(&t);
    if status.as_deref() == Some("blocked") {
        bail!("brain-blocked");
    }
    hc::send(&to.terminal, text)?;
    if let Some(report) = text.strip_prefix("[watch] ") {
        // Without what only the brain is told (that it is for the record).
        let report = report.strip_suffix(crate::tell::NOTE_TAIL).unwrap_or(report).to_string();
        let space = to.space.clone();
        crate::history::later(move || {
            crate::history::append(crate::history::Entry { kind: "watch".into(), text: report, record, space, ..Default::default() })
        });
    }
    // A busy brain takes it after its turn; the transcript says when.
    if status.as_deref() != Some("working") {
        set_turn(&to.name, Turn { watch: is_news_prompt(text), note: is_note_prompt(text), question: text.starts_with("[question") || text.starts_with("[behaviors]") });
    }
    Ok(())
}

pub fn spoken_error(msg: &str) -> String {
    if msg.contains("brain-absent") {
        "The tower isn't running. Start it with tower up.".into()
    } else if msg.contains("brain-blocked") {
        "I'm waiting on a permission question on my own screen. Have a look at the tower terminal.".into()
    } else {
        "I couldn't pass that on. The error is on screen.".into()
    }
}

/// Starts the voice daemon in a Herdr terminal (so it lives on after this
/// command, and macOS asks once for the microphone on Herdr Core's behalf).
pub fn launch_voice() -> Result<()> {
    if crate::protocol::is_running() {
        return Ok(());
    }
    if hc::is_live(VOICE_NAME) {
        let _ = hc::run(&["kill", VOICE_NAME]);
        let _ = hc::run(&["rm", VOICE_NAME]);
    }
    let tower = config::tower_bin().display().to_string();
    let home = config::home().display().to_string();
    let out =
        hc::run(&["run", "--bg", "--json", "--name", VOICE_NAME, "--no-notify", "--no-parent", "--cwd", &home, "--", &tower, "daemon"])?;
    if out.code != 0 {
        bail!("starting the voice: {}{}", out.stderr.trim(), out.stdout.trim());
    }
    // Models load before the socket opens: Whisper and Kokoro take a few seconds.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(600) {
        if crate::protocol::is_running() {
            return Ok(());
        }
        if !hc::is_live(VOICE_NAME) {
            let screen = hc::run(&["read", VOICE_NAME, "--lines", "40"]).map(|o| o.stdout).unwrap_or_default();
            bail!("the voice stopped while starting:\n{}", screen.trim());
        }
        thread::sleep(Duration::from_millis(300));
    }
    bail!("the voice took more than ten minutes to start (downloading models?); see `hc read {VOICE_NAME}`")
}

pub fn stop(name: &str) -> bool {
    if hc::get(name).is_some() {
        let _ = hc::run(&["kill", name]);
        let _ = hc::run(&["rm", name]);
        true
    } else {
        false
    }
}

pub fn brain_dir_exists() -> bool {
    Path::new(&config::brain_dir()).join("CLAUDE.md").exists()
}

#[cfg(test)]
mod tests {
    use super::trust_answer;

    #[test]
    fn a_brain_is_known_by_its_own_name_never_by_a_sessions_label() {
        use serde_json::json;
        // The towers' own terminals are started with their names.
        assert_eq!(super::brain_of(&json!({ "address": "t1", "name": "tower" })), Some("tower"));
        assert_eq!(super::brain_of(&json!({ "address": "t12", "name": "tower-sandbox", "labels": { "tower.space": "sandbox" } })), Some("tower-sandbox"));
        assert_eq!(super::brain_of(&json!({ "address": "t2", "name": "tower-voice" })), None);
        // A session in a tab carries its name as a label: `tower launch "Tower PR chips"` once made
        // `tower-pr-herder-tower`, and an older tower let any session be called `tower-…`.
        let session = json!({ "address": "t7", "name": null, "labels": { "where": "tower-pr / x", "tower.name": "tower-pr-herder-tower" }, "status": { "app": "claude-code", "state": "blocked" } });
        assert_eq!(crate::hc::name(&session), Some("tower-pr-herder-tower"));
        assert_eq!(super::brain_of(&session), None, "it is watched, asked about and counted like any session");
        assert_eq!(super::brain_of(&json!({ "address": "t8", "where": "M5 Pro / x", "status": "working" })), None);
        // A new session is never given such a name.
        let why = super::not_a_sessions_name("tower-pr-chips").unwrap();
        assert!(why.contains("can't be named \"tower-pr-chips\"") && why.contains("\"pr-chips-tower\""), "{why}");
        assert!(super::not_a_sessions_name("tower").unwrap().contains("work-on-tower"));
        assert!(super::not_a_sessions_name("tower-voice").is_some());
        assert!(super::not_a_sessions_name("herder-tower").is_none() && super::not_a_sessions_name("towering").is_none() && super::not_a_sessions_name("fix-login").is_none());
    }

    #[test]
    fn separate_tasks_get_a_hint() {
        let projects: Vec<crate::split::Project> =
            ["/w/eyzflow", "/w/herder-tower"].iter().filter_map(|f| crate::split::Project::of_folder(f)).collect();
        let h = super::split_hint("fix the login in eyzflow and also add a mute button to herder tower", &projects).unwrap();
        assert!(h.starts_with("[split?] 1) eyzflow: ") && h.contains("2) herder tower: "), "{h}");
        assert!(super::split_hint("fix the login in eyzflow", &projects).is_none());
    }

    #[test]
    fn news_is_watch_and_queue_notes_only() {
        assert!(super::is_news_prompt("<hc from=\"t3\">[queue] delivered to t5 (api): x"));
        assert!(super::is_news_prompt("[watch] t5 is now done."));
        assert!(!super::is_news_prompt("[voice] what did the [watch] say"));
    }

    #[test]
    fn a_note_turn_is_any_note_from_the_tower() {
        use super::is_note_prompt;
        assert!(is_note_prompt("[watch] t5 (api) is now done."));
        assert!(is_note_prompt("<hc from=\"t3\">[queue] delivered to t5 (api): x"));
        assert!(is_note_prompt("[agenda] a3 is done: every task is settled"));
        assert!(is_note_prompt("[session] From Shop › api (t5): the build is green"), "a session writing to the brain");
        assert!(!is_note_prompt("[voice] what's left? [agendas] unfinished: a3"), "the user asked");
        assert!(!is_note_prompt("[voice] what did the [agenda] note say"));
        assert!(!is_note_prompt("[question] t5 q1 {}"), "a question turn has its own rules");
        assert!(!is_note_prompt("just typed"));
        // A note that only quotes the word (a session of this project writes about it) is still a note,
        // and nothing the brain writes on it is shown or said.
        assert!(is_note_prompt("[session] From Shop › api (t5): the [voice] prefix test passes"));
        assert!(is_note_prompt("<hc from=\"tower-voice\">[watch] t5 (api) is now done. It said: \"[voice] turns are spoken\"."));
        assert!(super::is_question_prompt("[question-text] t5 q1 {\"message\":\"should the [voice] tag stay?\"}"));
        // What the user asked is theirs wherever it stands: wrapped by hc, or come in behind a note that waited.
        assert!(!is_note_prompt("<hc from=\"tower-voice\">[voice] what did the [watch] say"));
        assert!(!is_note_prompt("[watch] t5 (api) is now done.\n[voice] what is t5 doing?"));
        assert!(!super::is_question_prompt("  [voice] answer the [question] for me"));
    }

    #[test]
    fn a_note_names_its_session() {
        use super::note_session;
        assert_eq!(note_session("[watch] t3 (api) is now done. You had asked it: x.").as_deref(), Some("t3"));
        assert_eq!(note_session("<hc from=\"t1\">[queue] delivered to vps/t12 (web): run the tests").as_deref(), Some("vps/t12"));
        assert_eq!(note_session("[queue] #4 for t7h (orb) waited for an instruction that was cancelled").as_deref(), Some("t7h"));
        assert_eq!(note_session("[agenda] a3.2 \"fix login\": t5 (api) hadn't taken its instruction").as_deref(), Some("t5"));
        assert_eq!(note_session("[agenda] a3 is done: every task is settled (2/2 verified): the tests (verified)"), None);
        assert_eq!(note_session("[voice] ask t3 about it"), None, "not a note");
    }

    #[test]
    fn each_brain_has_a_turn_of_its_own() {
        use super::{Turn, forget_turn, set_turn, turn_of};
        // (Names no other test uses: the turns are the process's.)
        let (main, own) = ("tower-test-main", "tower-test-space");
        assert_eq!(turn_of(own), Turn::default(), "nothing known: an answer to the user");
        set_turn(own, Turn::of_prompt("[watch] t5 (app) is now done. You had asked it: x."));
        set_turn(main, Turn::of_prompt("[voice] what's left? [radar] t5 app: done"));
        assert_eq!(turn_of(own), Turn { watch: true, note: true, question: false });
        assert_eq!(turn_of(main), Turn::default(), "the other brain's note doesn't make this one's turn a note");
        set_turn(main, Turn::of_prompt("[question] t5 q1 {}"));
        assert_eq!((turn_of(main).question, turn_of(own).question), (true, false));
        set_turn(own, Turn::of_prompt("[session] From Shop › api (t5): the build is green"));
        assert_eq!(turn_of(own), Turn { watch: false, note: true, question: false });
        forget_turn(own);
        forget_turn(main);
        assert_eq!((turn_of(own), turn_of(main)), (Turn::default(), Turn::default()));
    }

    #[test]
    fn a_note_of_any_kind_names_its_session_for_routing() {
        use super::session_in;
        assert_eq!(session_in("[watch] t3 (api) is now done. You had asked it: x.").as_deref(), Some("t3"));
        assert_eq!(session_in("[queue] #4 for t7h (orb) waited for an instruction that was cancelled").as_deref(), Some("t7h"));
        assert_eq!(session_in("[agenda] a3.2 \"fix login\": t5 (api) hadn't taken its instruction").as_deref(), Some("t5"));
        assert_eq!(session_in("[session] From Shop › api (vps/t12): the build is green").as_deref(), Some("vps/t12"));
        // A question's note starts with its session; `note_session` (the record's) doesn't read those.
        assert_eq!(session_in("[question] t34 q0123456789ab {\"kind\":\"menu\"} policy=normal").as_deref(), Some("t34"));
        assert_eq!(session_in("[question-text] t9 q1 {\"tabs\":[]}").as_deref(), Some("t9"));
        assert_eq!(super::note_session("[question] t34 q1 {}"), None);
        // About no one session: the main brain's.
        assert_eq!(session_in("[space] \"Shop\" looks finished: every request there is done"), None);
        assert_eq!(session_in("[agenda] a3 is done: every task is settled (2/2 verified)"), None);
        assert_eq!(session_in("[behaviors] edit \"Talk plainly\": read it again"), None);
        assert_eq!(session_in("typed by the user about t5"), None);
    }

    #[test]
    fn brains_are_known_by_name() {
        use super::is_brain_name;
        assert!(is_brain_name("tower") && is_brain_name("tower-b2c-tickets"));
        assert!(!is_brain_name("tower-voice"), "the daemon's terminal is no brain");
        assert!(!is_brain_name("towering") && !is_brain_name("api") && !is_brain_name(""));
    }

    #[test]
    fn answers_claude_trust() {
        let screen = " Quick safety check: Is this a project you created or one you trust?\n ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm";
        assert_eq!(trust_answer(screen), Some(vec!["down", "enter"]));
        let screen = " Do you trust the files in this folder?\n ❯ 1. Yes, proceed\n   2. No, exit";
        assert_eq!(trust_answer(screen), Some(vec!["enter"]));
        assert_eq!(trust_answer("Welcome to Claude Code"), None);
    }
}
