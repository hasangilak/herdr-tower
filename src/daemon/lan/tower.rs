//! The rest of the tower as the phone API reaches it, and what it starts
//! that takes a while.
//!
//! A phone does what the Mac's windows and the CLI do, through the same
//! functions: the daemon is a `Tower` (`crate::daemon::desk`), so a decision
//! answered on the phone goes where `tower answer` sends it, a goal set
//! there is the one `tower space goal` sets, and so on. Tests give a tower
//! of their own, so nothing of the user's is touched.
//!
//! Launching a space and closing one open and end sessions, make and remove
//! worktrees: minutes, sometimes. They are *actions*: the call answers at
//! once with one (`{id, kind, state, …}`), the work goes on on a thread of
//! its own, every step is an `action` event, and `GET /v1/actions/{id}`
//! says where it stands (the last `KEEP`, until the daemon restarts).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use super::hub::Hub;
use crate::daemon::decisions::Refused;
use crate::launch::Plan;

pub trait Tower: Send + Sync {
    /// The queue in order (`tower decisions`): the main one (muted spaces
    /// left out), or the own of the space with exactly that name; `all`
    /// adds the last that closed.
    fn decisions(&self, space: Option<&str>, all: bool) -> Value;
    /// One decision as the queue carries it: open, or closed lately.
    fn decision(&self, id: &str) -> Option<Value>;
    /// `tower answer <decision>`: `{"choice": n}`, `{"other": "…"}`, or its question's answers.
    fn answer(&self, id: &str, body: &Value, submit: bool) -> Result<Value, Refused>;
    /// `tower dismiss <decision>`.
    fn dismiss(&self, id: &str) -> Result<Value, Refused>;
    /// Every space as a mission, as the state has it (`tower missions`).
    fn missions(&self) -> Value;
    /// Every space the tower knows: Herdr's, the history's, those it keeps.
    fn spaces(&self) -> Vec<String>;
    /// The spaces the tower keeps (`spaces.json`), closed ones too.
    fn remembered(&self) -> Vec<String>;
    /// `tower space goal` and `tower space standing`, each part when given:
    /// what the command prints (`{space, goal, done_when, standing, state, report}`).
    fn goal(&self, space: &str, goal: Option<&str>, done_when: Option<&str>, standing: Option<bool>) -> Result<Value>;
    /// `tower space follow|unfollow`.
    fn follow(&self, space: &str, on: bool) -> Result<()>;
    /// `tower space mute|unmute`.
    fn mute(&self, space: &str, on: bool) -> Result<()>;
    /// The sessions that hold a space open (`{address, name, status}`):
    /// closing it without `force` is refused over them.
    fn holding(&self, space: &str) -> Result<Vec<Value>>;
    /// `tower space close`: its receipt.
    fn close(&self, space: &str, force: bool) -> Result<Value>;
    /// `tower space resolve`: its goal is achieved; closed as `close` closes it. Its receipt.
    fn resolve(&self, space: &str, force: bool) -> Result<Value>;
    /// `tower space resolve --dry-run`: what closing it would end and remove.
    fn closing(&self, space: &str) -> Result<Value>;
    /// `tower setups`: `{"setups": [{name, account, repos}]}`.
    fn setups(&self) -> Result<Value>;
    /// What `tower launch` would do, as things stand.
    fn plan(&self, goal: &str, setup: Option<&str>, name: Option<&str>) -> Result<Plan>;
    /// Opens a plan for `by` (`phone:<device>`); `each` gets the receipt so
    /// far after every session. The receipt names the act kept for it (`act`).
    fn launch(&self, plan: &Plan, by: &str, each: &mut dyn FnMut(&Value)) -> Result<Value>;
    /// `tower tell`: `{"level", "voice"}`; with a level, sets it (and says
    /// `"was"`); with a voice setting, sets that (`"voice_was"`).
    fn tell(&self, level: Option<&str>, voice: Option<&str>) -> Result<Value>;
    /// `tower model`: the report (`brain_model`), after setting what is given.
    fn model(&self, model: Option<&str>, effort: Option<&str>, restart: bool) -> Result<Value>;
    /// What the tower opened lately, newest first: `{"acts": [Act]}` (`tower acts`).
    fn acts(&self) -> Value;
    /// One act, as it is now.
    fn act(&self, id: &str) -> Option<Value>;
    /// `tower undo <id> [--force]`: what it came to, with the act as it is now.
    fn undo(&self, id: &str, force: bool) -> Result<Value, crate::acts::Refused>;
    /// The request that makes an answered decision's answer a standing
    /// rule, and the space it is asked in (`answer`: what it was answered
    /// with, when the caller knows it).
    fn rule(&self, id: &str, answer: Option<&str>) -> Result<(String, Option<String>), Refused>;
    /// What the user asked, to the tower (or, with `space`, in that
    /// space's view). `phone`: the device that hears the reply alone.
    fn ask(&self, text: &str, phone: Option<String>, source: &str, space: Option<&str>) -> Result<()>;
}

/// Actions kept to be asked about.
pub const KEEP: usize = 50;

/// A launch or a close under way, or over.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub id: String,
    /// `launch` or `close`.
    pub kind: &'static str,
    pub space: String,
    /// `running`, `done` or `failed`.
    pub state: &'static str,
    /// A launch: what it set out to do (the dry-run receipt).
    pub plan: Value,
    /// The receipt: a launch's so far while it runs (sessions that have
    /// opened carry their address), the whole one when it is done.
    pub receipt: Value,
    pub error: Option<String>,
    /// Who started it (`phone:<device>`).
    pub by: String,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

impl Action {
    pub fn view(&self) -> Value {
        json!({
            "id": self.id, "kind": self.kind, "space": self.space, "state": self.state,
            "plan": self.plan, "receipt": self.receipt, "error": self.error,
            "by": self.by, "started_ms": self.started_ms, "finished_ms": self.finished_ms,
        })
    }
}

/// The actions the phones started, newest last, and the streams to tell.
#[derive(Clone)]
pub struct Actions {
    list: Arc<Mutex<VecDeque<Action>>>,
    hub: Arc<Hub>,
}

impl Actions {
    pub fn new(hub: Arc<Hub>) -> Actions {
        Actions { list: Arc::default(), hub }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Action>> {
        self.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, id: &str) -> Option<Action> {
        self.lock().iter().find(|a| a.id == id).cloned()
    }

    /// Newest first.
    pub fn all(&self) -> Vec<Action> {
        self.lock().iter().rev().cloned().collect()
    }

    /// Begins an action, unless one like it runs (`same`): then that one.
    /// Checked and added under one lock, so two taps start one.
    pub fn begin(&self, kind: &'static str, space: &str, plan: Value, by: &str, same: impl Fn(&Action) -> bool) -> Result<Action, Box<Action>> {
        let a = {
            let mut list = self.lock();
            if let Some(running) = list.iter().find(|a| a.state == "running" && same(a)) {
                return Err(Box::new(running.clone()));
            }
            let a = Action {
                id: format!("act_{}", super::auth::hex(&super::auth::random_bytes::<4>())),
                kind,
                space: space.to_string(),
                state: "running",
                plan,
                receipt: Value::Null,
                error: None,
                by: by.to_string(),
                started_ms: crate::history::now_ms(),
                finished_ms: None,
            };
            list.push_back(a.clone());
            // The oldest that is over goes; one that runs is never forgotten.
            while list.len() > KEEP {
                match list.iter().position(|a| a.state != "running") {
                    Some(i) => {
                        list.remove(i);
                    }
                    None => break,
                }
            }
            a
        };
        self.hub.broadcast("action", a.view());
        Ok(a)
    }

    /// Changes an action and tells every stream.
    pub fn change(&self, id: &str, f: impl FnOnce(&mut Action)) {
        let changed = {
            let mut list = self.lock();
            list.iter_mut().find(|a| a.id == id).map(|a| {
                f(a);
                a.clone()
            })
        };
        if let Some(a) = changed {
            self.hub.broadcast("action", a.view());
        }
    }

    /// It is over: with its receipt, or with why not.
    pub fn finish(&self, id: &str, result: Result<Value, String>) {
        self.change(id, |a| {
            a.finished_ms = Some(crate::history::now_ms());
            match result {
                Ok(receipt) => (a.state, a.receipt) = ("done", receipt),
                Err(e) => (a.state, a.error) = ("failed", Some(e)),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_action_runs_once_and_says_each_step() {
        let hub = Arc::new(Hub::default());
        let (_, rx) = hub.subscribe("d_1", false, true);
        let actions = Actions::new(hub);
        let a = actions.begin("launch", "fix-login", json!({ "space": "fix-login" }), "phone:d_1", |a| a.kind == "launch").unwrap();
        assert!(a.id.starts_with("act_") && a.id.len() == 12, "{}", a.id);
        assert_eq!((a.state, a.receipt.is_null(), a.finished_ms), ("running", true, None));
        // A second one while it runs is that one; another kind is its own.
        assert_eq!(actions.begin("launch", "other", Value::Null, "phone:d_2", |a| a.kind == "launch").unwrap_err().id, a.id);
        let close = actions.begin("close", "Shop", Value::Null, "phone:d_1", |a| a.kind == "close" && a.space == "Shop").unwrap();
        actions.change(&a.id, |a| a.receipt = json!({ "sessions": [{ "address": "t5" }] }));
        actions.finish(&a.id, Ok(json!({ "space": "fix-login", "sessions": [{ "address": "t5" }, { "address": "t6" }] })));
        actions.finish(&close.id, Err("\"Shop\" isn't closed: api (t7) is working".into()));
        let done = actions.get(&a.id).unwrap();
        assert_eq!((done.state, done.receipt["sessions"][1]["address"].as_str(), done.finished_ms.is_some()), ("done", Some("t6"), true));
        let failed = actions.get(&close.id).unwrap().view();
        assert_eq!((failed["state"].as_str(), failed["error"].as_str().map(|e| e.contains("is working"))), (Some("failed"), Some(true)));
        assert_eq!(actions.all().iter().map(|a| a.id.clone()).collect::<Vec<_>>(), [close.id.clone(), a.id.clone()], "newest first");
        // Over: another may begin.
        assert!(actions.begin("launch", "next", Value::Null, "phone:d_1", |a| a.kind == "launch").is_ok());
        // Every step went to the streams: begun, begun, changed, done, failed, begun.
        let events: Vec<(String, String)> = rx.try_iter().map(|o| (o.event, o.data["state"].as_str().unwrap_or("").to_string())).collect();
        assert_eq!(
            events.iter().map(|(e, s)| (e.as_str(), s.as_str())).collect::<Vec<_>>(),
            [
                ("action", "running"),
                ("action", "running"),
                ("action", "running"),
                ("action", "done"),
                ("action", "failed"),
                ("action", "running"),
            ]
        );
        assert!(actions.get("act_nope").is_none());
    }

    #[test]
    fn only_actions_that_are_over_are_forgotten() {
        let actions = Actions::new(Arc::new(Hub::default()));
        let first = actions.begin("close", "s0", Value::Null, "phone:d_1", |_| false).unwrap();
        for n in 1..KEEP + 5 {
            let a = actions.begin("close", &format!("s{n}"), Value::Null, "phone:d_1", |_| false).unwrap();
            actions.finish(&a.id, Ok(Value::Null));
        }
        assert_eq!(actions.all().len(), KEEP);
        assert_eq!(actions.get(&first.id).map(|a| a.state), Some("running"), "the one that still runs is kept, however old");
    }
}
