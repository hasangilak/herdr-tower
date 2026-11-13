//! The daemon's socket: one line of JSON in, one line out.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Speak text. `interrupt` cuts off what is being said. `kind`: empty
    /// for the brain's words, `answer` for its final answer (its turn is
    /// over), `alert` for news nobody asked for (it waits its turn).
    Say {
        text: String,
        #[serde(default)]
        interrupt: bool,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        kind: String,
        /// The terminal that ran `tower say` (its `HC_TERMINAL_ID`), when it
        /// ran in one: a brain's words on a note turn are not for the user
        /// at a quiet level (see `tell`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// Whose words they are, when a space's own brain's hook brings
        /// them (`tower-<space>`; none: the main brain's).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brain: Option<String>,
    },
    /// Stop speaking, and drop what was queued.
    Hush,
    /// The tower-wide mute: `on`, `off`, `toggle` or `status`.
    Mute {
        action: String,
    },
    /// Where speech plays: `on` (always this Mac), `off` (the phones as
    /// routed), `toggle` or `status`. Kept in config.toml.
    MacVoice {
        action: String,
    },
    /// Press-to-talk: `toggle`, `start` or `stop` (stop transcribes what was heard).
    Talk {
        action: String,
    },
    /// Cancel a take without sending it.
    Cancel,
    /// `ptt`, `handsfree` or `off`.
    Mode {
        mode: String,
    },
    /// Send text to the brain as if it were spoken. `space`: asked in that
    /// space's view, so its own brain gets it when it has one that can
    /// take it (else the main brain, with the space named).
    Ask {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<String>,
    },
    /// Take back the request at `at` (its `at` in the history): its brain
    /// is stopped if it is still on it, and the chat says no answer comes.
    Withdraw {
        at: u64,
    },
    /// Tell the brain when this terminal stops working.
    Watch {
        terminal: String,
        #[serde(default)]
        note: String,
        /// The Claude account it was started on (`tower start`), for its job.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// The send queue: `add` (`session`, `text`, `after`, `from`), `list`, `cancel`
    /// (`id`), `retarget` (`id` to `session`), `track` (`session`, `text`:
    /// a session started with that task, for `after`).
    Queue {
        action: String,
        #[serde(default)]
        session: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        id: Option<u64>,
        /// Ids that must be done first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        after: Vec<u64>,
        /// `start`: the `tower start` arguments for a session started once `after` is done.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        start: Vec<String>,
        /// How it goes into the agenda ledger (`add`, `start`, `track`): which agenda, its title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<crate::agenda::Tag>,
        /// `add`: the terminal that ran `tower queue` (its `HC_TERMINAL_ID`),
        /// when it ran in one. An agent session other than a brain makes it
        /// a message between sessions (see `peers`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// How much the tower tells the user (`tower tell`): `level` sets it
    /// (`decisions`, `results`, `everything`), `voice` what it says out
    /// loud (`off`, `decisions`, `replies`); neither asks what they are.
    Tell {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        voice: Option<String>,
    },
    /// What the tower opened for the user (see `crate::acts`): `record`
    /// (`act`: what `tower start`, `tower launch` or `tower dispatch` just
    /// opened and assumed; `from`: the terminal the command ran in, so the
    /// daemon knows whose request it answers), `undo` (`id`, or the latest
    /// when none; `force`), `list`.
    Act {
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default)]
        force: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        act: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// A decision for the user (`tower raise`): about `session`, of kind
    /// `why`, in one line, with the choices offered (`options`) when given.
    Raise {
        session: String,
        #[serde(default)]
        why: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<serde_json::Value>,
        /// What it is about, more than the line says (`--detail`): the
        /// command, the plan. Shown with it, cut to about 600 characters.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        detail: String,
        /// The terminal that ran `tower raise`: what a space's own brain
        /// raises is that space's, whatever session it names.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// A PR's review (`tower review`, see `crate::reviews`): `list` (no
    /// `pr`) or `show`; the reviewer's `report` (with `critical`) and
    /// `agree`, the owner's `fixed`, each with its line (`text`); `sweep`
    /// looks at every review now (a space's review was turned off).
    /// `from`: the terminal that ran it.
    Review {
        #[serde(default)]
        action: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        pr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        critical: Option<u32>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// A planned check of a PR, reported by its session (`tower validate`):
    /// `pr` (its key, its link, or `#12`: the caller's own), `check` (its id;
    /// empty: show the PR's checks), `status` (`passed`, `failed`,
    /// `not-run`), `evidence`. A PR the tower doesn't know yet is looked up
    /// first, as the caller's.
    Validate {
        pr: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        check: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        status: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        evidence: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// Who depends on a PR's changes (`tower impact`): `pr` (its key, its
    /// link, or `#12`), `verdict` (`breaks` or `clean`; empty: show it),
    /// `text` (what breaks and who uses it, or what was compiled or run).
    Impact {
        pr: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        verdict: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// What only the user can decide, in queue order (`tower decisions`).
    /// No `space`: the main queue (muted spaces left out). `space`: that
    /// space's own. `all`: the last ones that closed too.
    Decisions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        space: Option<String>,
        #[serde(default)]
        all: bool,
    },
    /// Every space as a mission (`tower missions`): the daemon looks at
    /// the sessions again and answers `{"spaces": [Mission], "quiet":
    /// [name], "closed_today": [name]}`, as the state's `missions` has it.
    Missions,
    /// The one line for a finished request (`tower receipt`), and the
    /// agenda it closes when known.
    Receipt {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agenda: Option<String>,
        /// The terminal that ran `tower receipt`: a space's own brain's
        /// receipt is that space's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// The brains' model (`tower model`): none of them asks what it is;
    /// `model` and `effort` set it (kept in config.toml, `[brains]`);
    /// `restart` restarts the brains that run, so they start on it. The
    /// answer is the state's `brain_model`, with `restarted` (their names).
    Model {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default)]
        restart: bool,
    },
    /// The brains (see `crate::brains`): `list` (those that run), `close`
    /// (`space` is being closed: its own brain is ended and held off),
    /// `restart` (every one that runs).
    Brains {
        #[serde(default)]
        action: String,
        #[serde(default)]
        space: String,
    },
    /// The agenda ledger: `list`, `tasks`, `show`, `open`, `close`, `abandon`,
    /// `verify`, `fail`, `requeue`, `judge` (`tower job done|failed`).
    Agenda {
        #[serde(flatten)]
        ledger: crate::agenda::Cmd,
    },
    /// An agent session's hook fired (`tower hook agent`).
    Agent {
        event: crate::daemon::sessions::AgentEvent,
    },
    /// What the agent sessions' hooks said (`tower sessions`).
    Sessions,
    /// The phone API: `on`, `off`, `pair`, `revoked` (arg: device id), `status`.
    Phone {
        action: String,
        #[serde(default)]
        arg: String,
    },
    /// The open question sets.
    Questions,
    /// Answer a question set (`target`: its id, or a session's address) with
    /// `answers` (`[{"tab", "choice"}]`); or a decision (`target`: its id,
    /// `d…`) with `{"choice": n}`, `{"other": "text"}`, or its question's answers.
    Answer {
        target: String,
        answers: serde_json::Value,
        #[serde(default = "yes")]
        submit: bool,
    },
    /// The brain's recommended answers for a question set.
    Recommend {
        id: String,
        #[serde(default)]
        recs: serde_json::Value,
        /// `auto` or `escalate`.
        #[serde(default)]
        decision: Option<String>,
        #[serde(default)]
        reason: String,
        /// A text question set that asks nothing after all.
        #[serde(default)]
        none: bool,
    },
    /// The behaviors changed (`change`: `behaviors::Change::view`; `by`:
    /// user or brain; `from`: the terminal the command ran in, so the
    /// brain that changed them isn't told to read them again).
    BehaviorsChanged {
        by: String,
        change: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// The user dismissed a question set, or a decision (`d…`).
    Dismiss { id: String },
    /// A decision put on hold (`reason`: the user's words, may be empty),
    /// or let go from hold (`unhold`; `reason`: what brought it back). `by`:
    /// who (`mac`, `phone:<device>`); none: the caller's terminal (`from`)
    /// says (a brain's name), else `cli`.
    Hold {
        id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unhold: bool,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// The user merged a PR (`pr`: `owner/repo#n`; `by`: `mac` or
    /// `phone:<device>`), from its chip. Only a report: the daemon asks
    /// GitHub, and only a merge it sees moves the chip and tells the session
    /// and the brain. Nothing merges through the socket.
    PrMerged { pr: String, by: String },
    /// `normal`, `keep-moving` or `ask-everything`.
    Policy { policy: String },
    /// Introducing related sessions (`tower introduce`): `status` (every
    /// space's Claude sessions and what was sent), `run` (`space`, shown with
    /// `dry_run` or sent again), `kick` (a session was just started: look now).
    Introduce {
        #[serde(default)]
        action: String,
        #[serde(default)]
        space: String,
        #[serde(default)]
        dry_run: bool,
    },
    /// "Merge all N" on a merge card (see `crate::merges`): `start` merges
    /// `prs` (keys, in order) of the merge decision `card` one after the
    /// other, by itself, as `by` (`mac`, `phone:<device>`) confirmed them;
    /// `stop` (`id`, or the card's running one) merges nothing more; `list`.
    MergeChain {
        action: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        card: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prs: Vec<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        by: String,
    },
    /// What a restart of the Mac or Herdr closed (`tower reopen`, see
    /// `crate::reopen`): `list`, `go` (all, or `spaces`; `dry_run`: only
    /// how), `forget` (all, or `spaces`).
    Reopen {
        #[serde(default)]
        action: String,
        #[serde(default)]
        spaces: Vec<String>,
        #[serde(default)]
        dry_run: bool,
    },
    /// A space's plan before it is launched (`tower plan`, see
    /// `crate::plans`): `new` (`form`, `ticket`, `draft`: the planner is
    /// started), `say` (`id`, `text`, `who`: `user` or `planner`; the
    /// user's is queued to the planner), `draft` (`id`, `draft`: the
    /// planner's revision, checked), `published` (`id`, `url`), `failed`
    /// (`id`, `text`), `prs` (`id`, `prs`: the PRs the plan predicts, which
    /// the tower checks and scores), `show` (`id`, or the open one), `close`
    /// (`id`, or the open one; `launched`: the space it was launched as), `list`.
    Plan {
        action: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        form: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ticket: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        url: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        who: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launched: Option<String>,
        /// `prs`: the PRs the plan predicts, as the planner wrote them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prs: Option<serde_json::Value>,
    },
    /// A screenshot was added (`tower screenshot`): the state's list is re-read.
    Screenshots,
    Status,
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<serde_json::Value>,
}

impl Response {
    pub fn err(e: impl ToString) -> Self {
        Self { ok: false, error: Some(e.to_string()), state: None }
    }
}

pub fn send(req: &Request) -> Result<Response> {
    send_within(req, wait_for(req))
}

/// How long an answer may take: typing a dialog's answers, a queue that
/// reaches another machine, and a dispatch take longer than the rest.
fn wait_for(req: &Request) -> Duration {
    match req {
        Request::Answer { .. } => Duration::from_secs(180),
        Request::Queue { .. } | Request::Agenda { .. } => Duration::from_secs(90),
        // It asks Herdr about every session first.
        Request::Missions => Duration::from_secs(60),
        // A report queues a note to the owner; a sweep may end reviewers.
        Request::Review { .. } => Duration::from_secs(60),
        // An undo ends terminals and takes worktrees away.
        Request::Act { action, .. } if action == "undo" => Duration::from_secs(120),
        // Ending a brain's terminal, and Herdr's answers on the way (a plan's planner too).
        Request::Model { .. } | Request::Brains { .. } | Request::Plan { .. } => Duration::from_secs(45),
        // A PR the tower doesn't know yet is looked up on GitHub first.
        Request::Validate { .. } | Request::Impact { .. } => Duration::from_secs(30),
        _ => Duration::from_secs(10),
    }
}

pub fn send_within(req: &Request, wait: Duration) -> Result<Response> {
    let path = config::socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("the tower's voice isn't running (no {}); start it with `tower up`", path.display()))?;
    stream.set_read_timeout(Some(wait))?;
    stream.set_write_timeout(Some(wait))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    reader.read_line(&mut answer)?;
    if answer.trim().is_empty() {
        bail!("the daemon closed the connection without answering");
    }
    Ok(serde_json::from_str(&answer)?)
}

pub fn is_running() -> bool {
    UnixStream::connect(config::socket_path()).is_ok()
}

fn yes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::Request;

    #[test]
    fn older_callers_and_the_new_requests_are_understood() {
        // A `tower queue` from before the sender was carried, and one that names it.
        let old: Request = serde_json::from_str(r#"{"cmd":"queue","action":"add","session":"t5","text":"x"}"#).unwrap();
        assert!(matches!(old, Request::Queue { from: None, .. }));
        let new: Request = serde_json::from_str(r#"{"cmd":"queue","action":"add","session":"t5","text":"x","from":"t7h"}"#).unwrap();
        assert!(matches!(new, Request::Queue { from: Some(ref f), .. } if f == "t7h"));
        // Without a sender it is written as before.
        let sent = serde_json::to_string(&Request::Queue { action: "list".into(), session: String::new(), text: String::new(), id: None, after: vec![], start: vec![], tag: None, from: None }).unwrap();
        assert!(!sent.contains("from"), "{sent}");
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"say","text":"hi"}"#).unwrap(), Request::Say { from: None, brain: None, .. }));
        // A space's own brain's hook says whose words it brings; the main brain's is written as before.
        let own: Request = serde_json::from_str(r#"{"cmd":"say","text":"hi","kind":"answer","brain":"tower-sandbox"}"#).unwrap();
        assert!(matches!(own, Request::Say { brain: Some(ref b), .. } if b == "tower-sandbox"));
        assert_eq!(serde_json::to_string(&Request::Say { text: "hi".into(), interrupt: false, kind: String::new(), from: None, brain: None }).unwrap(), r#"{"cmd":"say","text":"hi","interrupt":false}"#);
        // A request asked of the tower, and one asked in a space's view.
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"ask","text":"what's left?"}"#).unwrap(), Request::Ask { space: None, .. }));
        let there: Request = serde_json::from_str(r#"{"cmd":"ask","text":"what's left?","space":"Sandbox"}"#).unwrap();
        assert!(matches!(there, Request::Ask { space: Some(ref s), .. } if s == "Sandbox"));
        assert_eq!(serde_json::to_string(&Request::Ask { text: "x".into(), space: None }).unwrap(), r#"{"cmd":"ask","text":"x"}"#);
        // The brains' model: asked, set, set and restarted.
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"model"}"#).unwrap(), Request::Model { model: None, effort: None, restart: false }));
        let set: Request = serde_json::from_str(r#"{"cmd":"model","model":"claude-fable-5-2","effort":"high","restart":true}"#).unwrap();
        assert!(matches!(set, Request::Model { model: Some(ref m), effort: Some(ref e), restart: true } if m == "claude-fable-5-2" && e == "high"));
        assert_eq!(serde_json::to_string(&Request::Model { model: None, effort: None, restart: false }).unwrap(), r#"{"cmd":"model","restart":false}"#);
        let close: Request = serde_json::from_str(r#"{"cmd":"brains","action":"close","space":"Sandbox"}"#).unwrap();
        assert!(matches!(close, Request::Brains { ref action, ref space } if action == "close" && space == "Sandbox"));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"brains"}"#).unwrap(), Request::Brains { ref action, .. } if action.is_empty()));
        // Older callers of the requests that now say who ran them.
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"receipt","text":"Done."}"#).unwrap(), Request::Receipt { agenda: None, from: None, .. }));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"behaviors_changed","by":"user","change":{}}"#).unwrap(), Request::BehaviorsChanged { from: None, .. }));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"tell"}"#).unwrap(), Request::Tell { level: None, voice: None }));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"tell","level":"results"}"#).unwrap(), Request::Tell { level: Some(ref l), voice: None } if l == "results"));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"tell","voice":"decisions"}"#).unwrap(), Request::Tell { level: None, voice: Some(ref v) } if v == "decisions"));
        let raise: Request = serde_json::from_str(r#"{"cmd":"raise","session":"t5","why":"merge","text":"Merge PR 42?","options":[{"label":"Merge"}]}"#).unwrap();
        assert!(matches!(raise, Request::Raise { ref why, options: Some(ref o), from: None, .. } if why == "merge" && o.is_array()));
        let receipt: Request = serde_json::from_str(r#"{"cmd":"receipt","text":"The login is fixed.","agenda":"a3"}"#).unwrap();
        assert!(matches!(receipt, Request::Receipt { agenda: Some(ref a), .. } if a == "a3"));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"decisions"}"#).unwrap(), Request::Decisions { space: None, all: false }));
        assert!(matches!(serde_json::from_str::<Request>(r#"{"cmd":"missions"}"#).unwrap(), Request::Missions));
        assert_eq!(serde_json::to_string(&Request::Missions).unwrap(), r#"{"cmd":"missions"}"#);
        let report: Request = serde_json::from_str(r#"{"cmd":"review","action":"report","pr":"o/app#12","critical":2,"text":"two","from":"t9"}"#).unwrap();
        assert!(matches!(report, Request::Review { ref action, critical: Some(2), from: Some(ref f), .. } if action == "report" && f == "t9"));
        assert_eq!(
            serde_json::to_string(&Request::Review { action: "list".into(), pr: String::new(), critical: None, text: String::new(), from: None }).unwrap(),
            r#"{"cmd":"review","action":"list"}"#
        );
        let own: Request = serde_json::from_str(r#"{"cmd":"decisions","space":"Shop","all":true}"#).unwrap();
        assert!(matches!(own, Request::Decisions { space: Some(ref s), all: true } if s == "Shop"));
        // What the tower opened: recorded by the command that opened it, undone by its id or the latest.
        let act: Request = serde_json::from_str(r#"{"cmd":"act","action":"record","act":{"kind":"start"},"from":"t7"}"#).unwrap();
        assert!(matches!(act, Request::Act { ref action, id: None, force: false, act: Some(ref a), from: Some(ref f) } if action == "record" && a["kind"] == "start" && f == "t7"));
        let undo: Request = serde_json::from_str(r#"{"cmd":"act","action":"undo","id":"r0123456789ab","force":true}"#).unwrap();
        assert!(matches!(undo, Request::Act { ref action, id: Some(ref i), force: true, act: None, .. } if action == "undo" && crate::acts::is_id(i)));
        assert_eq!(serde_json::to_string(&Request::Act { action: "undo".into(), id: None, force: false, act: None, from: None }).unwrap(), r#"{"cmd":"act","action":"undo","force":false}"#);
        // A plan: the planner's answer, the user's message; a plan's start carries its draft.
        let say: Request = serde_json::from_str(r#"{"cmd":"plan","action":"say","id":"p3","text":"which first?","who":"user"}"#).unwrap();
        assert!(matches!(say, Request::Plan { ref action, ref id, ref who, draft: None, .. } if action == "say" && id == "p3" && who == "user"));
        let new: Request = serde_json::from_str(r#"{"cmd":"plan","action":"new","form":"page","ticket":"T","draft":{"space":"x"}}"#).unwrap();
        assert!(matches!(new, Request::Plan { ref form, draft: Some(ref d), .. } if form == "page" && d["space"] == "x"));
        let show = Request::Plan { action: "show".into(), id: String::new(), form: String::new(), ticket: String::new(), draft: None, text: String::new(), url: String::new(), who: String::new(), launched: None, prs: None };
        assert_eq!(serde_json::to_string(&show).unwrap(), r#"{"cmd":"plan","action":"show"}"#);
        // A decision is answered through the request a question set is, with its own body.
        let answer: Request = serde_json::from_str(r#"{"cmd":"answer","target":"d0123456789ab","answers":{"choice":1}}"#).unwrap();
        assert!(matches!(answer, Request::Answer { ref target, ref answers, submit: true } if crate::decisions::is_id(target) && answers["choice"] == 1));
    }
}
