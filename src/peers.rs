//! Sessions talk to each other without the tower in between.
//!
//! `tower queue <address> "<message>"` run by an agent session (not a brain,
//! not the user) is a message between sessions, not work handed out. The
//! send queue delivers it with its usual guarantees (it waits for a busy
//! receiver, in order, once, kept across restarts) and with the sender's
//! name on it; it makes no agenda task, no job, no watch, and no note to any
//! brain. It is kept in the history as kind `peer`, and a space's `peer`
//! entries are its thread (`tower thread`, `threads` in state.json).
//!
//! That holds at the quiet levels (`tell`). While the tower tells
//! everything, a session's instruction is handled as it always was (a task,
//! watched, told to the brain); it still carries the sender's name.
//!
//! A message for a brain (`tower queue tower "…"`) goes in tagged
//! `[session]`: a note from that session, so the brain's turn on it is a
//! note turn (`brain::is_note_prompt`), not an answer to the user.

use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hc;
use crate::history::Entry;

/// Messages kept per space in the state (the history has them all).
pub const KEEP: usize = 50;
/// A message's text in the state is cut to this many characters.
const STATE_TEXT: usize = 400;

/// The session that sent a message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sender {
    /// Its address (`t7h`, `vps/t2`).
    pub address: String,
    /// Its name, as Herdr shows it.
    pub name: String,
    /// The Herdr space it is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
}

/// The terminal a command runs in, when it runs in Herdr (`HC_TERMINAL_ID`):
/// who called `tower queue`. None in a plain shell, the panel, the phone.
pub fn caller() -> Option<String> {
    caller_in(std::env::var("HC_TERMINAL_ID").ok())
}

pub fn caller_in(terminal: Option<String>) -> Option<String> {
    terminal.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The terminal `t` as a sender, when it is an agent session other than a
/// brain: a Claude Code or Codex session Herdr follows. A brain (`tower`,
/// `tower-<space>`), the daemon's terminal, a plain shell or program are
/// not: what they queue is an instruction, as before.
pub fn sender_of(t: &Value) -> Option<Sender> {
    let address = t["address"].as_str().filter(|a| !a.is_empty())?;
    hc::agent_app(t).filter(|a| !a.is_empty())?;
    let name = hc::name(t).unwrap_or("");
    if crate::brain::brain_of(t).is_some() || name == crate::brain::VOICE_NAME {
        return None;
    }
    Some(Sender { address: address.to_string(), name: hc::spoken_name(t), space: crate::spaces::space_of(t) })
}

/// What `tower queue <session> "<text>"` is, by who ran it.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Work handed out (by the user, a brain, or, while the tower tells
    /// everything, anyone): into the agenda, watched, told to the brain.
    /// `text` is what goes in (with a session's name on it, when one sent it).
    Instruction { text: String },
    /// A message from one session to another: only delivered and kept in
    /// their thread. No agenda task, no job, no watch, no note to a brain.
    Message { from: Sender },
}

/// How an instruction from `sender` (None: the user or a brain) goes at
/// `level`: a session's is a message between sessions at the quiet levels.
pub fn route(sender: Option<Sender>, level: crate::tell::Level, text: &str) -> Route {
    match sender {
        Some(from) if level.quiet() => Route::Message { from },
        Some(from) => Route::Instruction { text: signed(&from, text) },
        None => Route::Instruction { text: text.to_string() },
    }
}

/// The terminal `t` as the sender of a message to another brain, when it
/// is a brain itself: the main one (no space), or a space's own.
pub fn brain_sender_of(t: &Value) -> Option<Sender> {
    let address = t["address"].as_str().filter(|a| !a.is_empty())?;
    let name = crate::brain::brain_of(t)?;
    let space = if crate::brains::is_space_brain(name) { crate::brains::space_of_terminal(t) } else { None };
    Some(Sender { address: address.to_string(), name: hc::spoken_name(t), space })
}

/// `route`, by the terminals of who ran `tower queue` (`from`: none in a
/// plain shell, the panel, the phone) and who gets it (`to`). One brain
/// writing to another is a message at every level: a space's own handing
/// the main one what crosses spaces, the main one handing a space's
/// something for it. Neither is the user asking, nor work handed out.
pub fn route_between(from: Option<&Value>, to: Option<&Value>, level: crate::tell::Level, text: &str) -> Route {
    if let (Some(f), Some(t)) = (from, to)
        && crate::brain::brain_of(t).is_some()
        && f["address"] != t["address"]
        && let Some(sender) = brain_sender_of(f)
    {
        return Route::Message { from: sender };
    }
    route(from.and_then(sender_of), level, text)
}

/// "From <space> › <name> (<address>): ".
pub fn header(from: &Sender) -> String {
    match from.space.as_deref().filter(|s| !s.is_empty()) {
        Some(space) => format!("From {space} › {} ({}): ", from.name, from.address),
        None => format!("From {} ({}): ", from.name, from.address),
    }
}

/// The message as its receiver gets it: with the sender's name on it,
/// unless it says who it is from already.
pub fn signed(from: &Sender, text: &str) -> String {
    let text = text.trim();
    if text.starts_with("From ") || text.starts_with(TO_BRAIN) { text.to_string() } else { format!("{}{text}", header(from)) }
}

/// Starts a session's message to a brain: a note from that session, not
/// something the user said.
pub const TO_BRAIN: &str = "[session] ";

/// The session a text says it comes from, by the way a message between
/// sessions opens: the tower's own signature (`header`: "From Shop › web
/// (t7h): …", "From web (t7h): …"), or as sessions sign their own ("From
/// t7h: …", "From t7h (web). …", "From t7p, at the user's request: …"). An
/// address is `t`, a digit, then letters or digits (`t7`, `t7h`, `t58`),
/// maybe after its machine (`vps/t2`). Any case; a `<pasted_content …>` (or
/// `[session] `) in front is passed over. None: it opens no such way (the
/// user's own words, a brain's instruction, "From the orb session: …").
pub fn sender_in(text: &str) -> Option<String> {
    let mut t = text.trim_start();
    t = t.strip_prefix(TO_BRAIN).unwrap_or(t).trim_start();
    if t.starts_with("<pasted_content")
        && let Some(end) = t.find('>')
    {
        t = t[end + 1..].trim_start();
    }
    if !t.get(..5).is_some_and(|h| h.eq_ignore_ascii_case("from ")) {
        return None;
    }
    let rest = &t[5..];
    if let Some(a) = address_at(rest) {
        return Some(a);
    }
    // "From <name> (t7h)": the address in brackets, on the first line.
    let line: String = rest.lines().next().unwrap_or("").chars().take(160).collect();
    let open = line.find('(')?;
    let close = open + line[open..].find(')')?;
    let inside = line[open + 1..close].trim();
    address_at(inside).filter(|a| a.len() == inside.len())
}

/// A session's address at the start of `s` (`t7h`, `vps/t2`), as a whole
/// word: "t7p's" is no address, nor is "the".
fn address_at(s: &str) -> Option<String> {
    let token: String = s.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.')).collect();
    let token = token.trim_end_matches('.');
    let next = s[token.len()..].chars().next();
    if next.is_some_and(|c| c == '\'' || c == '’' || c.is_alphanumeric()) {
        return None;
    }
    let (machine, address) = match token.rsplit_once('/') {
        Some((m, a)) => (Some(m), a),
        None => (None, token),
    };
    if machine.is_some_and(|m| m.is_empty() || m.contains('/')) {
        return None;
    }
    let mut c = address.chars();
    let ok = matches!(c.next(), Some('t' | 'T')) && c.next().is_some_and(|d| d.is_ascii_digit()) && c.all(|x| x.is_ascii_alphanumeric());
    ok.then(|| token.to_lowercase())
}

/// Ends a session's message to a brain: what a turn on it is, and how it is answered.
pub const BRAIN_TAIL: &str = " (A note from that session, not from the user: nothing you write on this turn is shown or spoken. If it needs an answer, `tower queue` it to that session; `tower raise` what only the user can decide.)";

/// A session's message as a brain gets it: tagged as a note, with the
/// sender's name on it.
pub fn for_brain(from: &Sender, text: &str) -> String {
    format!("{TO_BRAIN}{}{BRAIN_TAIL}", signed(from, text))
}

/// `signed` (and `for_brain`) undone: the message as its sender wrote it.
pub fn unsigned<'a>(from: &Sender, text: &'a str) -> &'a str {
    let text = text.strip_prefix(TO_BRAIN).map(|t| t.strip_suffix(BRAIN_TAIL).unwrap_or(t)).unwrap_or(text);
    text.strip_prefix(header(from).as_str()).unwrap_or(text)
}

/// A message between sessions, as the history keeps it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Message {
    pub at_ms: u64,
    pub from: Sender,
    /// The receiver's address and name.
    pub to: String,
    pub to_name: String,
    /// The receiver's space, when known.
    pub space: Option<String>,
    /// As the sender wrote it.
    pub text: String,
    /// False: it could not be delivered (its receiver is gone).
    pub delivered: bool,
}

impl Message {
    /// Its history entry: `session` is the receiver, `from` the sender, in
    /// the receiver's space (the sender's when the receiver has none).
    pub fn entry(&self) -> Entry {
        Entry {
            at: self.at_ms,
            kind: "peer".into(),
            session: Some(self.to.clone()),
            name: Some(self.to_name.clone()).filter(|n| !n.is_empty()),
            space: self.space.clone().or_else(|| self.from.space.clone()),
            status: Some(if self.delivered { "delivered" } else { "undelivered" }.into()),
            text: self.text.clone(),
            from: Some(self.from.address.clone()),
            from_name: Some(self.from.name.clone()).filter(|n| !n.is_empty()),
            record: true,
            ..Default::default()
        }
    }
}

/// A `peer` entry as the thread carries it (`tower thread --json`, the
/// state): who wrote to whom, when, and whether it arrived.
pub fn row(e: &Entry) -> Value {
    json!({
        "at": e.at, "from": e.from, "from_name": e.from_name, "to": e.session, "to_name": e.name,
        "text": e.text, "space": e.space, "delivered": e.status.as_deref() != Some("undelivered"),
    })
}

fn in_space(e: &Entry, space: Option<&str>) -> bool {
    e.kind == "peer" && space.is_none_or(|s| e.space.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(s)))
}

/// A space's thread (None: every space's), newest last, at most `limit`.
pub fn thread<'a>(entries: &'a [Entry], space: Option<&str>, limit: usize) -> Vec<&'a Entry> {
    let all: Vec<&Entry> = entries.iter().filter(|e| in_space(e, space)).collect();
    let from = all.len().saturating_sub(limit.max(1));
    all[from..].to_vec()
}

/// A `peer` entry as the phone carries it: `row`, with its id (`h<line>`,
/// its line in the history, as a chat message's).
pub fn message(line: usize, e: &Entry) -> Value {
    json!({
        "id": format!("h{line}"), "at": e.at, "from": e.from, "from_name": e.from_name, "to": e.session, "to_name": e.name,
        "text": e.text, "space": e.space, "delivered": e.status.as_deref() != Some("undelivered"),
    })
}

/// A page of a space's thread (None: every space's), for the phone: up to
/// `limit` messages before message `before` (its `id`), newest last, and
/// whether older ones remain. `lines`: the history with its line numbers.
pub fn page(lines: &[(usize, Entry)], space: Option<&str>, before: Option<&str>, limit: usize) -> (Vec<Value>, bool) {
    let cut = before.and_then(|b| b.strip_prefix('h')).and_then(|n| n.parse::<usize>().ok()).unwrap_or(usize::MAX);
    let older: Vec<&(usize, Entry)> = lines.iter().filter(|(n, e)| *n < cut && in_space(e, space)).collect();
    let start = older.len().saturating_sub(limit.max(1));
    (older[start..].iter().map(|(n, e)| message(*n, e)).collect(), start > 0)
}

/// The spaces that have a thread.
pub fn spaces(entries: &[Entry]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for s in entries.iter().filter(|e| e.kind == "peer").filter_map(|e| e.space.clone()) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// One message for the state: its text cut short (the history has it whole).
fn state_row(e: &Entry) -> Value {
    let mut v = row(e);
    if e.text.chars().count() > STATE_TEXT {
        v["text"] = json!(format!("{}…", e.text.chars().take(STATE_TEXT).collect::<String>().trim_end()));
    }
    v
}

/// Every space's thread for the state: the last `KEEP` messages each,
/// newest last. Messages in no space are under "".
pub fn threads(entries: &[Entry]) -> BTreeMap<String, Vec<Value>> {
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.kind == "peer") {
        push(&mut out, e);
    }
    out
}

/// Adds one message to the state's threads, dropping the oldest past `KEEP`.
pub fn push(threads: &mut BTreeMap<String, Vec<Value>>, e: &Entry) {
    if e.kind != "peer" {
        return;
    }
    let list = threads.entry(e.space.clone().unwrap_or_default()).or_default();
    list.push(state_row(e));
    if list.len() > KEEP {
        let n = list.len() - KEEP;
        list.drain(..n);
    }
}

fn line(e: &Entry) -> String {
    let who = |name: &Option<String>, address: &Option<String>| match (name.as_deref(), address.as_deref()) {
        (Some(n), Some(a)) => format!("{n} ({a})"),
        (Some(n), None) => n.to_string(),
        (None, a) => a.unwrap_or("?").to_string(),
    };
    format!(
        "{}  {} → {}{}: {}",
        crate::phone::ago(e.at),
        who(&e.from_name, &e.from),
        who(&e.name, &e.session),
        if e.status.as_deref() == Some("undelivered") { " (not delivered)" } else { "" },
        e.text
    )
}

/// `tower thread [<space>] [--json]`: what the sessions of a space wrote to
/// each other, newest last. No space: every space's.
pub fn cmd(space: &str, as_json: bool, limit: usize) -> Result<()> {
    let entries = crate::history::load();
    let want = space.trim();
    let space = if want.is_empty() {
        None
    } else {
        let mut names = spaces(&entries);
        for n in crate::spaces::known(&crate::spaces::live(), &[], &crate::spaces::load()) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        // A space nobody wrote in yet has an empty thread, not an error.
        Some(crate::spaces::resolve(want, &names).unwrap_or_else(|| want.to_string()))
    };
    let list = thread(&entries, space.as_deref(), limit);
    if as_json || !std::io::stdout().is_terminal() {
        let rows: Vec<Value> = list.iter().map(|e| row(e)).collect();
        println!("{}", serde_json::to_string_pretty(&json!({ "space": space, "messages": rows })).unwrap_or_default());
        return Ok(());
    }
    if list.is_empty() {
        match &space {
            Some(s) => println!("No messages between the sessions of {s} yet."),
            None => println!("No messages between sessions yet."),
        }
    }
    for e in list {
        match (&space, e.space.as_deref()) {
            (None, Some(sp)) if !sp.is_empty() => println!("[{sp}] {}", line(e)),
            _ => println!("{}", line(e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(address: &str, name: &str, space: &str) -> Value {
        json!({
            "address": address, "name": name, "state": "live",
            "status": { "state": "working", "app": "claude-code" },
            "labels": { "where": format!("{space} / {name}") },
        })
    }

    #[test]
    fn the_caller_is_the_terminal_it_runs_in() {
        assert_eq!(caller_in(Some("t7h".into())).as_deref(), Some("t7h"));
        assert_eq!(caller_in(Some(" vps/t2 \n".into())).as_deref(), Some("vps/t2"));
        assert_eq!(caller_in(Some(String::new())), None, "outside Herdr");
        assert_eq!(caller_in(None), None, "a plain shell, the panel, the phone");
    }

    #[test]
    fn an_agent_session_is_a_sender_and_a_brain_is_not() {
        let s = sender_of(&agent("t7h", "orb-rust", "herder tower")).unwrap();
        assert_eq!(s, Sender { address: "t7h".into(), name: "orb rust".into(), space: Some("herder tower".into()) });
        // The brain, a space's own brain, and the daemon's terminal: their instructions stay instructions.
        assert_eq!(sender_of(&agent("t1", "tower", "herder tower")), None);
        assert_eq!(sender_of(&agent("t2", "tower-b2c-tickets", "B2C tickets")), None);
        assert_eq!(sender_of(&agent("t3", "tower-voice", "")), None);
        // A shell or a plain program the user types in: no agent, so the user's own instruction.
        assert_eq!(sender_of(&json!({ "address": "t9", "name": "zsh", "state": "live" })), None);
        assert_eq!(sender_of(&json!({ "address": "t9", "name": "htop", "status": "idle" })), None);
        // A tab made in a space carries its name as a label.
        let tab = json!({ "address": "t4", "status": { "state": "idle", "app": "codex" }, "labels": { "tower.name": "api", "where": "Shop / api" } });
        assert_eq!(sender_of(&tab).unwrap().name, "api");
        // One that is only called like a brain (a label, not the name it was started with) is a session.
        let called =
            json!({ "address": "t5", "status": { "state": "idle", "app": "claude-code" }, "labels": { "tower.name": "tower-shop" } });
        assert_eq!(sender_of(&called).map(|s| s.name), Some("shop tower".to_string()));
    }

    #[test]
    fn a_sessions_instruction_is_a_message_only_at_a_quiet_level() {
        use crate::tell::Level::*;
        let s = sender_of(&agent("t7h", "orb-rust", "herder tower"));
        // A message: nothing registers it as work (only an `Instruction` goes into the agenda and is watched).
        for level in [Decisions, Results] {
            assert_eq!(route(s.clone(), level, "which port?"), Route::Message { from: s.clone().unwrap() });
        }
        // Telling everything: an instruction as before, with who it is from.
        assert_eq!(
            route(s, Everything, "which port?"),
            Route::Instruction { text: "From herder tower › orb rust (t7h): which port?".into() }
        );
        // The user's or a brain's: an instruction at every level, word for word.
        for level in [Decisions, Results, Everything] {
            assert_eq!(route(None, level, "fix the login"), Route::Instruction { text: "fix the login".into() });
        }
    }

    #[test]
    fn one_brain_writing_to_another_is_a_message_at_every_level() {
        use crate::tell::Level::*;
        // A space's own brain (hidden: no `where`, its space on a label), the main brain, a session, the user's shell.
        let own = json!({ "address": "t12", "name": "tower-sandbox", "state": "live", "status": { "state": "working", "app": "claude-code" }, "labels": { "tower.space": "Sandbox" } });
        let main = json!({ "address": "t7", "name": "tower", "state": "live", "status": { "state": "idle", "app": "claude-code" }, "labels": {} });
        let session = agent("t5", "sandbox-app", "Sandbox");
        let shell = json!({ "address": "t9", "name": "zsh", "state": "live" });
        let from_own = Sender { address: "t12".into(), name: "tower sandbox".into(), space: Some("Sandbox".into()) };
        assert_eq!(brain_sender_of(&own), Some(from_own.clone()));
        assert_eq!(brain_sender_of(&main), Some(Sender { address: "t7".into(), name: "tower".into(), space: None }));
        assert_eq!(brain_sender_of(&session), None);
        for level in [Decisions, Results, Everything] {
            // A space's brain hands the main one what crosses spaces; the main one hands a space's something for it.
            assert_eq!(route_between(Some(&own), Some(&main), level, "open a session for billing"), Route::Message { from: from_own.clone() }, "{level}");
            assert!(matches!(route_between(Some(&main), Some(&own), level, "billing is t9"), Route::Message { ref from } if from.address == "t7" && from.space.is_none()));
            // A brain handing a session work is work, as it always was; and so is what the user queues to a brain.
            assert_eq!(route_between(Some(&own), Some(&session), level, "run the tests"), Route::Instruction { text: "run the tests".into() });
            assert_eq!(route_between(Some(&shell), Some(&own), level, "status?"), Route::Instruction { text: "status?".into() });
            assert_eq!(route_between(None, Some(&main), level, "status?"), Route::Instruction { text: "status?".into() });
            // A brain doesn't write to itself.
            assert_eq!(route_between(Some(&own), Some(&own), level, "note to self"), Route::Instruction { text: "note to self".into() });
        }
        // A session writing to a brain: a message at the quiet levels, as between sessions.
        assert!(matches!(route_between(Some(&session), Some(&main), Decisions, "the build is green"), Route::Message { ref from } if from.address == "t5" && from.space.as_deref() == Some("Sandbox")));
        assert_eq!(route_between(Some(&session), Some(&main), Everything, "x"), Route::Instruction { text: "From Sandbox › app (t5): x".into() });
        // As a brain gets it: a note from who wrote it, never the user asking.
        let note = for_brain(&from_own, "open a session for billing");
        assert!(note.starts_with("[session] From Sandbox › tower sandbox (t12): open a session for billing"), "{note}");
        assert!(crate::brain::is_note_prompt(&note) && crate::brain::session_in(&note).as_deref() == Some("t12"));
    }

    /// How a message between sessions opens says who sent it, as the tower
    /// signs it and as sessions sign their own; the user's words don't.
    #[test]
    fn a_message_says_who_it_is_from_by_how_it_opens() {
        for (text, from) in [
            ("From Shop › orb rust (t7h): which port?", "t7h"),
            ("From orb rust (t7h): which port?", "t7h"),
            ("From t7h: thanks, all of it is in", "t7h"),
            ("From t7h, from t7k, one small addition", "t7h"),
            ("From t7h (sandbox-eyzflow). New steer from the user", "t7h"),
            ("From t7h. Thanks, all four objections", "t7h"),
            ("From t7p (Claude in ~/Work/herder-tower, M5 Pro space). The Mac side", "t7p"),
            ("from t58 the build is green", "t58"),
            ("From vps/t2: the migration ran", "vps/t2"),
            ("[session] From Docs › guide (t9): renamed", "t9"),
            ("<pasted_content id=\"1\">\nFrom t7r (Android). Status", "t7r"),
        ] {
            assert_eq!(sender_in(text).as_deref(), Some(from), "{text}");
        }
        for text in [
            "From the orb session: ready when you are",
            "From now on, ask me first",
            "From t7p's branch, cherry-pick the fix",
            "From the user (t5 asked): x",
            "fix the login",
            "Wait for instructions from the user. Do nothing until told.",
            "About \"Phone check\": the user decided: It arrived",
            "From tower-sandbox: x",
        ] {
            assert_eq!(sender_in(text), None, "{text}");
        }
        // What the tower signs, it reads back.
        let s = Sender { address: "t12".into(), name: "tower sandbox".into(), space: Some("Sandbox".into()) };
        assert_eq!(sender_in(&signed(&s, "hello")).as_deref(), Some("t12"));
    }

    #[test]
    fn the_tower_puts_the_senders_name_on_it() {
        let from = Sender { address: "t7h".into(), name: "orb rust".into(), space: Some("herder tower".into()) };
        let text = signed(&from, "  which port does the api use?  ");
        assert_eq!(text, "From herder tower › orb rust (t7h): which port does the api use?");
        assert_eq!(unsigned(&from, &text), "which port does the api use?");
        // It says who it is from already: left as it is.
        assert_eq!(signed(&from, "From t7h (orb): the build is green"), "From t7h (orb): the build is green");
        assert_eq!(unsigned(&from, "From t7h (orb): the build is green"), "From t7h (orb): the build is green");
        let nowhere = Sender { address: "t2".into(), name: "api".into(), space: None };
        assert_eq!(signed(&nowhere, "ready"), "From api (t2): ready");
    }

    /// A session's `tower queue tower "…"`: the brain's turn on it is a note
    /// turn about that session, not an answer shown and spoken to the user.
    #[test]
    fn a_sessions_message_to_the_brain_is_a_note_from_that_session() {
        let from = Sender { address: "t7h".into(), name: "orb rust".into(), space: Some("herder tower".into()) };
        let note = for_brain(&from, "the build is green, PR #12 is open");
        assert!(note.starts_with("[session] From herder tower › orb rust (t7h): the build is green, PR #12 is open"), "{note}");
        assert!(note.ends_with(BRAIN_TAIL) && note.contains("tower raise"));
        assert!(crate::brain::is_note_prompt(&note), "a note turn");
        assert!(crate::brain::is_note_prompt(&format!("<hc from=\"tower-voice\">{note}")), "as hc wraps it");
        assert_eq!(crate::brain::note_session(&note).as_deref(), Some("t7h"), "kept in the record as about its sender");
        assert!(!crate::brain::is_news_prompt(&note) && !crate::brain::is_question_prompt(&note));
        // The queue leaves it as it is (no second name on it), and the thread keeps what the session wrote.
        assert_eq!(signed(&from, &note), note);
        assert_eq!(unsigned(&from, &note), "the build is green, PR #12 is open");
        // Untagged, it would be a request of the user's: answered to them.
        assert!(!crate::brain::is_note_prompt(&signed(&from, "the build is green")));
    }

    fn msg(at: u64, from: &str, to: &str, space: &str, text: &str) -> Entry {
        Message {
            at_ms: at,
            from: Sender { address: from.into(), name: format!("s-{from}"), space: Some(space.into()) },
            to: to.into(),
            to_name: format!("s-{to}"),
            space: Some(space.into()),
            text: text.into(),
            delivered: true,
        }
        .entry()
    }

    #[test]
    fn a_spaces_peer_messages_are_its_thread() {
        let mut entries =
            vec![msg(1, "t1", "t2", "Shop", "first"), msg(2, "t5", "t6", "Docs", "elsewhere"), msg(3, "t2", "t1", "Shop", "second")];
        entries.push(Entry { at: 4, kind: "queue".into(), space: Some("Shop".into()), text: "not a message".into(), ..Default::default() });
        let e = &entries[0];
        assert_eq!(
            (e.kind.as_str(), e.session.as_deref(), e.from.as_deref(), e.space.as_deref()),
            ("peer", Some("t2"), Some("t1"), Some("Shop"))
        );
        assert!(e.record && !crate::chat::shown(e), "kept in the record, never a chat message");
        let shop: Vec<&str> = thread(&entries, Some("shop"), 50).iter().map(|e| e.text.as_str()).collect();
        assert_eq!(shop, ["first", "second"], "newest last, its own space only");
        assert_eq!(thread(&entries, Some("Shop"), 1)[0].text, "second");
        assert_eq!(thread(&entries, None, 50).len(), 3);
        assert_eq!(spaces(&entries), ["Shop", "Docs"]);
        let r = row(&entries[2]);
        assert_eq!(
            (r["from"].as_str(), r["from_name"].as_str(), r["to"].as_str(), r["to_name"].as_str()),
            (Some("t2"), Some("s-t2"), Some("t1"), Some("s-t1"))
        );
        assert_eq!((r["text"].as_str(), r["at"].as_u64(), r["delivered"].as_bool()), (Some("second"), Some(3), Some(true)));
        // One that never arrived is in the thread too, marked.
        let mut lost = Message { at_ms: 5, to: "t9".into(), text: "hello?".into(), ..Default::default() };
        lost.from.space = Some("Shop".into());
        let lost = lost.entry();
        assert_eq!((lost.space.as_deref(), row(&lost)["delivered"].as_bool()), (Some("Shop"), Some(false)), "in its sender's space");
    }

    #[test]
    fn the_state_keeps_the_last_fifty_of_each_space() {
        let mut entries: Vec<Entry> = (0..KEEP as u64 + 10).map(|i| msg(i, "t1", "t2", "Shop", &format!("m{i}"))).collect();
        entries.push(msg(999, "t5", "t6", "Docs", &"x".repeat(STATE_TEXT + 50)));
        let mut t = threads(&entries);
        assert_eq!(t["Shop"].len(), KEEP);
        assert_eq!((t["Shop"][0]["text"].as_str(), t["Shop"][KEEP - 1]["text"].as_str()), (Some("m10"), Some("m59")));
        assert_eq!(t["Docs"][0]["text"].as_str().unwrap().chars().count(), STATE_TEXT + 1, "a long one is cut short");
        push(&mut t, &msg(1000, "t2", "t1", "Shop", "newest"));
        assert_eq!((t["Shop"].len(), t["Shop"][KEEP - 1]["text"].as_str()), (KEEP, Some("newest")));
    }
}
