//! Answering what sessions ask, through the voice daemon (which reads the
//! dialog again and types the answers in, see `questions`): `tower answer`,
//! `tower dismiss`, `tower policy` and the orb's panel all go through here.

use anyhow::{Result, bail};
use serde_json::Value;

use crate::protocol::{self, Request};

/// The policies `tower policy` takes.
pub const POLICIES: [&str; 3] = ["normal", "keep-moving", "ask-everything"];

/// Answers a question set (`target`: its id, or a session's address) with
/// `answers` (`[{tab, choice, attachments?}]`); `submit` sends them (else
/// they are only filled in). The set as it is now.
pub fn answer(target: &str, answers: Value, submit: bool) -> Result<Value> {
    let r = protocol::send(&Request::Answer { target: target.into(), answers, submit })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    Ok(r.state.unwrap_or_default())
}

/// The user doesn't want to answer it here.
pub fn dismiss(id: &str) -> Result<()> {
    let r = protocol::send(&Request::Dismiss { id: id.into() })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    Ok(())
}

/// How questions are handled from now on: kept in config.toml, and told to
/// the daemon if it runs.
pub fn set_policy(policy: &str) -> Result<()> {
    if !POLICIES.contains(&policy) {
        bail!("tower policy normal|keep-moving|ask-everything");
    }
    crate::phone::set_question_policy(policy)?;
    let _ = protocol::send(&Request::Policy { policy: policy.into() });
    Ok(())
}
