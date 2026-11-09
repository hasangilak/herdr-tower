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
    },
    /// Stop speaking, and drop what was queued.
    Hush,
    /// The tower-wide mute: `on`, `off`, `toggle` or `status`.
    Mute {
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
    /// Send text to the brain as if it were spoken.
    Ask {
        text: String,
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
    /// The send queue: `add` (`session`, `text`, `after`), `list`, `cancel`
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
    /// Answer a question set (`target`: its id, or a session's address).
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
    /// The behaviors changed (`change`: `behaviors::Change::view`; `by`: user or brain).
    BehaviorsChanged { by: String, change: serde_json::Value },
    /// The user dismissed a question set.
    Dismiss { id: String },
    /// `normal`, `keep-moving` or `ask-everything`.
    Policy { policy: String },
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
        Request::Queue { .. } => Duration::from_secs(90),
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
