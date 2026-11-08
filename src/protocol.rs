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
    },
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
    let path = config::socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("the tower's voice isn't running (no {}); start it with `tower up`", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
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
