//! herdr-core (`hc`), run as a program.
//!
//! Every call has a deadline (a hung `hc`, or an ssh link to another machine
//! that went quiet, never holds a thread forever), and typing into a terminal
//! goes one writer at a time (`typing`).

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::config;

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// How long a call may take: typing waits for a person to pause, and another
/// machine is reached over ssh.
fn deadline_for(args: &[&str]) -> Duration {
    match args.first().copied() {
        Some("send" | "keys" | "exec" | "interrupt" | "put") => Duration::from_secs(60),
        Some("machine") => Duration::from_secs(90),
        _ => Duration::from_secs(30),
    }
}

pub fn run(args: &[&str]) -> Result<Output> {
    run_for(args, deadline_for(args))
}

/// Runs hc, killing it if it takes longer than `limit`.
pub fn run_for(args: &[&str], limit: Duration) -> Result<Output> {
    let mut cmd = Command::new(config::hc_bin());
    cmd.args(args);
    output_within(cmd, &format!("hc {}", args.first().copied().unwrap_or("")), limit)
        .with_context(|| format!("running hc {}", args.join(" ")))
}

/// Runs a program to its end, or kills it after `limit`.
pub fn output_within(mut cmd: Command, what: &str, limit: Duration) -> Result<Output> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    // Read both pipes meanwhile, so a chatty hc never blocks on a full pipe.
    let drain = |p: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{what} took longer than {}s", limit.as_secs());
        }
        thread::sleep(Duration::from_millis(15));
    };
    Ok(Output {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned(),
        stderr: String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned(),
    })
}

/// Held while typing into a terminal (a prompt, an answer, keys), so two
/// writers (the send queue, an answer from the phone, a note for the brain)
/// never interleave their keys there.
pub struct Typing {
    _guard: std::sync::MutexGuard<'static, ()>,
}

pub fn typing(address: &str) -> Typing {
    static LOCKS: OnceLock<Mutex<HashMap<String, &'static Mutex<()>>>> = OnceLock::new();
    let lock: &'static Mutex<()> = {
        let mut all = LOCKS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        // One small lock per address ever typed into: kept for the process's life.
        all.entry(address.to_string()).or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
    };
    Typing { _guard: lock.lock().unwrap_or_else(|e| e.into_inner()) }
}

/// Runs hc with `--json` and parses what it prints.
pub fn json(args: &[&str]) -> Result<Value> {
    let mut all: Vec<&str> = args.to_vec();
    all.push("--json");
    let out = run(&all)?;
    let value: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
    if out.code != 0 && value.is_null() {
        bail!("hc {} failed: {}{}", args.join(" "), out.stderr.trim(), out.stdout.trim());
    }
    Ok(value)
}

/// One terminal's facts, or None when there is no such terminal (or hc
/// couldn't say: see `lookup` to tell those apart).
pub fn get(terminal: &str) -> Option<Value> {
    match lookup(terminal) {
        Lookup::Found(v) => Some(v),
        _ => None,
    }
}

/// What hc says about a terminal.
#[derive(Debug, Clone)]
pub enum Lookup {
    /// Its facts (live or finished: see `state`).
    Found(Value),
    /// hc answered: there is no such terminal.
    Gone,
    /// hc couldn't say (not running, a machine out of reach, too slow).
    Unreachable(String),
}

pub fn lookup(terminal: &str) -> Lookup {
    let out = match run(&["get", "--json", "--", terminal]) {
        Ok(o) => o,
        Err(e) => return Lookup::Unreachable(format!("{e:#}")),
    };
    classify(&out)
}

fn classify(out: &Output) -> Lookup {
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
    if v.get("address").is_some() {
        return Lookup::Found(v);
    }
    // A failing hc prints its error (as JSON) on stderr.
    let v = if v.is_null() { serde_json::from_str(out.stderr.trim()).unwrap_or(Value::Null) } else { v };
    match v["error"]["kind"].as_str() {
        Some("not_found") => Lookup::Gone,
        Some(kind) => Lookup::Unreachable(format!("{kind}: {}", v["error"]["message"].as_str().unwrap_or(""))),
        None => Lookup::Unreachable(format!("hc get: {}{}", out.stderr.trim(), out.stdout.trim())),
    }
}

/// When a terminal was made: with its address, who it is (an address can
/// come back for another terminal after the first is removed).
pub fn created_ms(t: &Value) -> Option<u64> {
    t["created_ms"].as_u64()
}

/// How many turns the agent in it has taken (Herdr counts them).
pub fn turn(t: &Value) -> Option<u64> {
    t["turn"].as_u64()
}

pub fn is_live(terminal: &str) -> bool {
    get(terminal).is_some_and(|t| t["state"] == "live")
}

/// What a session is doing, as the tower understands it: Herdr's status
/// word for it, or its agent's hooks, in one vocabulary (see
/// docs/state-machines.md, section 1). On disk and on the wire it is its
/// word, as before; older words fold in (the hooks' `waiting` is blocked,
/// `ended` is exited), and a word Herdr adds later is kept as it is.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(from = "String", into = "String")]
pub enum SessionStatus {
    /// Live, no status reported yet (starting, a first-run question, or a plain program). Its word is "".
    #[default]
    Starting,
    Idle,
    Working,
    /// Waiting on a person: a dialog (question, permission, plan, menu) or
    /// other (`block_kind` says which).
    Blocked,
    Done,
    Error,
    /// The terminal is there; its program ended.
    Exited,
    /// Herdr doesn't know the address any more (or it is another terminal now).
    Gone,
    /// A machine that doesn't answer: not gone, unknown.
    Unreachable,
    Other(String),
}

impl SessionStatus {
    pub fn parse(s: &str) -> SessionStatus {
        match s {
            "" => SessionStatus::Starting,
            "idle" => SessionStatus::Idle,
            "working" => SessionStatus::Working,
            "blocked" | "waiting" => SessionStatus::Blocked,
            "done" => SessionStatus::Done,
            "error" => SessionStatus::Error,
            "exited" | "ended" => SessionStatus::Exited,
            "gone" => SessionStatus::Gone,
            "unreachable" => SessionStatus::Unreachable,
            other => SessionStatus::Other(other.to_string()),
        }
    }

    pub fn word(&self) -> &str {
        match self {
            SessionStatus::Starting => "",
            SessionStatus::Idle => "idle",
            SessionStatus::Working => "working",
            SessionStatus::Blocked => "blocked",
            SessionStatus::Done => "done",
            SessionStatus::Error => "error",
            SessionStatus::Exited => "exited",
            SessionStatus::Gone => "gone",
            SessionStatus::Unreachable => "unreachable",
            SessionStatus::Other(s) => s,
        }
    }

    /// Takes an instruction: not working, not waiting on a question.
    pub fn ready(&self) -> bool {
        matches!(self, SessionStatus::Idle | SessionStatus::Done)
    }

    /// Goes back to ready by itself: an instruction can wait it out.
    pub fn passing(&self) -> bool {
        matches!(self, SessionStatus::Working | SessionStatus::Blocked)
    }

    /// Stopped for someone to look at it: finished, failed or waiting on a person.
    pub fn stopped(&self) -> bool {
        matches!(self, SessionStatus::Idle | SessionStatus::Done | SessionStatus::Error | SessionStatus::Blocked)
    }

    /// Its program is over, one way or another.
    pub fn ended(&self) -> bool {
        matches!(self, SessionStatus::Exited | SessionStatus::Gone)
    }
}

impl From<String> for SessionStatus {
    fn from(s: String) -> SessionStatus {
        SessionStatus::parse(&s)
    }
}

impl From<&str> for SessionStatus {
    fn from(s: &str) -> SessionStatus {
        SessionStatus::parse(s)
    }
}

impl From<SessionStatus> for String {
    fn from(s: SessionStatus) -> String {
        s.word().to_string()
    }
}

impl std::fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.word())
    }
}

/// What a blocked session waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Question,
    Permission,
    Other,
}

/// What a terminal's agent says it is doing (Starting for none: see `status_word`).
pub fn status(t: &Value) -> SessionStatus {
    status_word(t).map(|w| SessionStatus::parse(&w)).unwrap_or_default()
}

/// What a terminal is doing, its program's end included: Exited when it isn't live.
pub fn status_live(t: &Value) -> SessionStatus {
    if t["state"].as_str().is_some_and(|s| s != "live") { SessionStatus::Exited } else { status(t) }
}

/// What a blocked terminal waits on (herdr-core's `status.kind`).
pub fn block_kind(t: &Value) -> BlockKind {
    match t["status"]["kind"].as_str() {
        Some("question") => BlockKind::Question,
        Some("permission") => BlockKind::Permission,
        _ => BlockKind::Other,
    }
}

/// The agent status word a terminal reports (`idle`, `working`, `blocked`,
/// `done`, `error`), or None for a plain program.
pub fn status_word(t: &Value) -> Option<String> {
    match &t["status"] {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("state").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

/// The program that reported the status (`claude-code`, `codex`, ...).
pub fn agent_app(t: &Value) -> Option<String> {
    t["status"].get("app").and_then(Value::as_str).map(String::from)
}

/// A terminal's name: hc's, or the one `tower start --space` gave it (a tab
/// made in a space can't carry an hc name, so it's the label `tower.name`).
pub fn name(t: &Value) -> Option<&str> {
    t["name"].as_str().filter(|n| !n.is_empty()).or_else(|| t["labels"]["tower.name"].as_str().filter(|n| !n.is_empty()))
}

/// What a terminal's session is called within its space: its own name, else
/// its project (see `crate::names`, which also says it with its space).
pub fn spoken_name(t: &Value) -> String {
    crate::names::short_of(t)
}

/// The machines added with `hc machine add`.
pub fn machines() -> Vec<String> {
    let Ok(v) = json(&["machine", "ls"]) else { return vec![] };
    v["machines"].as_array().map(|a| a.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect()).unwrap_or_default()
}

/// The live terminals on one machine (`None`: this one), each with its full
/// address (`vps/t3` for another machine).
pub fn list_on(machine: Option<&str>) -> Result<Vec<Value>> {
    let v = match machine {
        None => json(&["ls"])?,
        Some(m) => {
            let out = run(&["exec", "--on", m, "--", "hc", "ls", "--json"])?;
            if out.code != 0 {
                bail!("{m}: {}", out.stderr.trim());
            }
            serde_json::from_str(out.stdout.trim()).with_context(|| format!("{m}: hc ls printed {}", out.stdout.trim()))?
        }
    };
    let mut list = v["terminals"].as_array().cloned().unwrap_or_default();
    if let Some(m) = machine {
        for t in &mut list {
            if let Some(a) = t["address"].as_str() {
                t["address"] = Value::String(format!("{m}/{a}"));
            }
            t["machine"] = Value::String(m.to_string());
        }
    }
    Ok(list)
}

/// Pastes text into a terminal and presses Enter (one writer at a time; text
/// that starts with `-` is text, not an option). hc refuses a terminal that
/// is blocked on a dialog. Enter is all it presses (no Esc, no interrupt):
/// a Claude Code session that is working keeps what is typed and sends it
/// as its next message when its turn ends.
pub fn send(terminal: &str, text: &str) -> Result<()> {
    send_if(terminal, text, None)
}

/// `send`, only while the terminal's status is exactly `status` (`idle`,
/// `working`): hc looks again as it types, and refuses otherwise.
pub fn send_if(terminal: &str, text: &str, status: Option<&str>) -> Result<()> {
    let text = clean(text);
    let _typing = typing(terminal);
    let mut args = vec!["send", "--json"];
    if let Some(s) = status {
        args.extend(["--if", s]);
    }
    args.extend(["--", terminal, &text]);
    let out = run(&args)?;
    if out.code != 0 {
        let msg = if out.stderr.trim().is_empty() { out.stdout.trim() } else { out.stderr.trim() };
        bail!("{msg}");
    }
    Ok(())
}

/// Text fit to type into a terminal: no control characters (an escape or a
/// carriage return would drive the program, not say something), but line
/// breaks and tabs.
pub fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_vocabulary_for_a_session() {
        use SessionStatus as S;
        for (w, st) in [("", S::Starting), ("idle", S::Idle), ("working", S::Working), ("blocked", S::Blocked), ("done", S::Done), ("error", S::Error), ("exited", S::Exited), ("gone", S::Gone)] {
            assert_eq!(S::parse(w), st);
            assert_eq!(st.word(), w, "the words on disk and on the wire stay as they were");
        }
        // The hooks' words fold in.
        assert_eq!(S::parse("waiting"), S::Blocked);
        assert_eq!(S::parse("ended"), S::Exited);
        assert_eq!(S::parse("thinking"), S::Other("thinking".into()), "a word Herdr adds later is kept");
        assert!(S::Idle.ready() && S::Done.ready() && !S::Blocked.ready() && !S::Other("x".into()).ready());
        assert!(S::Working.passing() && S::Blocked.passing() && !S::Error.passing());
        assert!(S::Error.stopped() && S::Blocked.stopped() && !S::Working.stopped() && !S::Exited.stopped());
        assert!(S::Gone.ended() && S::Exited.ended() && !S::Unreachable.ended(), "out of reach isn't gone");
        let t = serde_json::json!({"state": "finished", "status": {"state": "idle", "kind": "question"}});
        assert_eq!((status(&t), status_live(&t)), (S::Idle, S::Exited));
        assert_eq!(block_kind(&t), BlockKind::Question);
        assert_eq!(status(&serde_json::json!({"state": "live"})), S::Starting, "a plain program: no status");
        let v: Vec<S> = serde_json::from_str(r#"["", "waiting", "idle"]"#).unwrap();
        assert_eq!(v, [S::Starting, S::Blocked, S::Idle]);
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"["","blocked","idle"]"#);
    }

    fn out(code: i32, stdout: &str) -> Output {
        Output { code, stdout: stdout.into(), stderr: String::new() }
    }

    #[test]
    fn tells_gone_from_unreachable() {
        assert!(matches!(classify(&out(0, r#"{"address":"t5","state":"live"}"#)), Lookup::Found(_)));
        assert!(matches!(classify(&out(3, r#"{"error":{"kind":"not_found","message":"no terminal \"t9\""}}"#)), Lookup::Gone));
        assert!(matches!(
            classify(&out(4, r#"{"error":{"kind":"machine_unknown","message":"unknown machine"}}"#)),
            Lookup::Unreachable(_)
        ));
        assert!(matches!(classify(&out(1, "")), Lookup::Unreachable(_)));
        // As the real hc does it: the error on stderr.
        let real = Output { code: 3, stdout: String::new(), stderr: r#"{"error":{"hint":"see `hc ls --all`","kind":"not_found","message":"no terminal \"t48\""}}"#.into() };
        assert!(matches!(classify(&real), Lookup::Gone));
    }

    #[test]
    fn typed_text_has_no_control_keys() {
        assert_eq!(clean("yes\u{1b}[A\rmore\n\tok\u{9b}"), "yes[Amore\n\tok");
    }

    #[test]
    fn one_writer_per_terminal() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static INSIDE: AtomicU32 = AtomicU32::new(0);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                thread::spawn(|| {
                    let _t = typing("t-test-typing");
                    assert_eq!(INSIDE.fetch_add(1, Ordering::SeqCst), 0, "two writers at once");
                    thread::sleep(Duration::from_millis(20));
                    INSIDE.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn a_slow_call_is_cut_off() {
        // `sleep` stands in for a hung hc.
        let start = Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let r = output_within(cmd, "sleep", Duration::from_millis(200));
        assert!(r.is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        let mut cmd = Command::new("echo");
        cmd.arg("hi");
        assert_eq!(output_within(cmd, "echo", Duration::from_secs(5)).unwrap().stdout, "hi\n");
    }
}
