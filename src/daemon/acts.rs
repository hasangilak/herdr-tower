//! The acts the daemon keeps (`crate::acts`): what `tower start`, `tower
//! launch` and `tower dispatch` opened for the user, with what was assumed,
//! tied to the request it answered, and undone on their word.
//!
//! They are kept in `acts.json` (the state folder) across restarts, shown
//! in `state.json` (`acts`: the latest, newest first), written to the
//! history (kind `act`), and sent to the phones (`act` events).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::lan::hub::Hub;
use super::state::{Shared, lock};
use crate::acts::{Act, Book, Hands, Opened, Refused};
use crate::hc;
use crate::worktrees::{Place, Removal};

#[derive(Clone)]
pub struct Acts {
    book: Arc<Mutex<Book>>,
    path: PathBuf,
    shared: Shared,
    hub: Arc<Hub>,
}

static ACTS: OnceLock<Acts> = OnceLock::new();

/// The running daemon's acts (None in a command, and in tests).
pub fn get() -> Option<&'static Acts> {
    ACTS.get()
}

/// A session took a prompt, or was sent an instruction: more work than its
/// first one holds the act that opened it.
pub fn given(address: &str, text: &str) {
    if let Some(a) = get() {
        a.given(address, text);
    }
}

impl Acts {
    pub fn path() -> PathBuf {
        crate::config::state_dir().join("acts.json")
    }

    /// The acts as the last run left them.
    pub fn open(shared: Shared, hub: Arc<Hub>) -> Acts {
        let acts = Acts::at(Self::path(), shared, hub);
        let _ = ACTS.set(acts.clone());
        acts
    }

    pub fn at(path: PathBuf, shared: Shared, hub: Arc<Hub>) -> Acts {
        let book: Book = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let acts = Acts { book: Arc::new(Mutex::new(book)), path, shared, hub };
        acts.publish(&lock(&acts.book));
        acts
    }

    fn save(&self, b: &Book) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec_pretty(b).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    fn publish(&self, b: &Book) {
        let views = b.views();
        self.shared.update(|s| s.acts = views);
    }

    /// One act changed: kept, shown, and told to the phones.
    fn changed(&self, b: &Book, act: &Act) {
        self.save(b);
        self.publish(b);
        self.hub.broadcast("act", act.view());
    }

    /// What a command just opened (`what`: `{kind, space, space_made, goal,
    /// sessions: [{address, name, worktree, fresh, first, task}], assumed,
    /// by}`), kept as an act. `from`: the terminal the command ran in. Run
    /// by a brain on a turn the user began, it answers their request; on a
    /// note turn, or run by anyone else, it answers none (and is still
    /// kept, to be undone by its id).
    pub fn record(&self, what: &Value, from: Option<&str>) -> Result<Value> {
        let now = crate::history::now_ms();
        let sessions: Vec<Opened> = serde_json::from_value(what["sessions"].clone()).unwrap_or_default();
        let kind = what["kind"].as_str().unwrap_or("start").to_string();
        if !matches!(kind.as_str(), "start" | "launch" | "dispatch") {
            bail!("an act is a start, a launch or a dispatch");
        }
        if sessions.is_empty() && kind != "launch" {
            bail!("an act with no sessions");
        }
        let brain = from.map(str::trim).filter(|f| !f.is_empty()).and_then(|f| {
            super::brains::name_at(f).or_else(|| hc::get(f).and_then(|t| crate::brain::brain_of(&t).map(String::from)))
        });
        let (by, request) = match &brain {
            Some(name) => {
                let turn = crate::brain::turn_of(name);
                let space = super::brains::space_of(name);
                let asked = (!turn.note && !turn.question).then(|| crate::acts::request_for(&crate::history::load(), space.as_deref(), now)).flatten();
                (name.clone(), asked)
            }
            // The daemon's own terminal: the tower's queue started it (a dispatch's task that waited for another).
            None if from.and_then(hc::get).is_some_and(|t| hc::name(&t) == Some(crate::brain::VOICE_NAME)) => (crate::brain::NAME.to_string(), None),
            None => (what["by"].as_str().map(String::from).or_else(|| from.map(String::from)).filter(|b| !b.trim().is_empty()).unwrap_or_else(|| "cli".into()), None),
        };
        let assumed = match &what["assumed"] {
            Value::Array(_) => serde_json::from_value(what["assumed"].clone()).unwrap_or_default(),
            a if kind == "launch" => crate::acts::chips_of_launch(a),
            a => crate::acts::chips_of_start(a),
        };
        let act = Act {
            id: crate::acts::new_id(now),
            // A dispatch's sessions are opened as a start's are: for one request they are one act.
            kind: if kind == "dispatch" { "start".into() } else { kind },
            at_ms: now,
            by,
            space: what["space"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
            space_made: what["space_made"].as_bool().unwrap_or(false),
            goal: what["goal"].as_str().unwrap_or("").to_string(),
            sessions,
            assumed,
            request,
            state: "done".into(),
            ..Default::default()
        };
        let kept = {
            let mut b = lock(&self.book);
            let kept = b.record(act);
            self.changed(&b, &kept);
            kept
        };
        eprintln!("[acts] {}: {} ({}{})", kept.id, kept.line(), kept.by, if kept.request.is_some() { ", in answer to a request" } else { "" });
        keep_in_history(&kept);
        Ok(kept.view())
    }

    /// `given`, for these acts.
    pub fn given(&self, address: &str, text: &str) {
        let mut b = lock(&self.book);
        if b.given(address, text) {
            eprintln!("[acts] {address} was given more work: its act is undone only with --force");
            self.save(&b);
            self.publish(&b);
            if let Some(act) = b.acts.iter().rev().find(|a| a.sessions.iter().any(|s| s.address == address)) {
                self.hub.broadcast("act", act.view());
            }
        }
    }

    /// Every act kept, newest first.
    pub fn list(&self) -> Value {
        json!({ "acts": lock(&self.book).acts.iter().rev().map(Act::view).collect::<Vec<_>>() })
    }

    pub fn view(&self, id: &str) -> Option<Value> {
        lock(&self.book).get(id).map(Act::view)
    }

    /// `tower undo [<id>]` (none: the latest, when it is recent): ends what
    /// the act opened, through `hands`. The answer: `{"act": its view,
    /// "closing", "sessions_ended", "worktrees_removed", "kept_worktrees",
    /// "space_closed"}`.
    pub fn undo(&self, id: Option<&str>, force: bool, hands: &dyn Hands) -> Result<Value, Refused> {
        let now = crate::history::now_ms();
        let act = {
            let b = lock(&self.book);
            match id.map(str::trim).filter(|i| !i.is_empty()) {
                Some(id) => b.get(id).cloned().ok_or_else(|| Refused::Unknown(id.to_string()))?,
                None => b.latest(now).cloned().ok_or_else(|| Refused::Unknown(String::new()))?,
            }
        };
        // Outside the lock: it ends terminals and waits for them.
        let out = crate::acts::undo(&act, force, hands)?;
        let kept = {
            let mut b = lock(&self.book);
            let Some(a) = b.acts.iter_mut().find(|a| a.id == act.id) else { return Err(Refused::Unknown(act.id.clone())) };
            a.state = "undone".into();
            a.undone_ms = Some(now);
            a.closing = Some(out.closing.clone());
            let kept = a.clone();
            self.changed(&b, &kept);
            kept
        };
        eprintln!("[acts] {} undone{}: {}", kept.id, if force { " (forced)" } else { "" }, out.closing);
        keep_in_history(&kept);
        let mut v = out.value();
        v["act"] = kept.view();
        Ok(v)
    }
}

/// An act as it stands, into the history (kind `act`).
fn keep_in_history(act: &Act) {
    // Tests never touch the user's history.
    if cfg!(test) {
        return;
    }
    let entry = act.entry();
    crate::history::later(move || crate::history::append(entry));
}

/// Undoing with the daemon's own hands: Herdr's terminals, git, and what
/// the tower holds for a session (its task, its queue, its watch).
pub struct Real<'a> {
    pub sendq: &'a super::sendq::SendQueue,
    pub agendas: &'a super::agendas::Agendas,
    pub watches: &'a super::watch::Watches,
    pub decisions: &'a super::decisions::Decisions,
    pub missions: &'a super::missions::Missions,
}

impl Hands for Real<'_> {
    fn runs(&self, s: &Opened) -> bool {
        hc::get(&s.address).is_some_and(|t| t["state"] == "live" && crate::acts::same_terminal(s, &t))
    }

    fn dirty(&self, place: &Place) -> Option<String> {
        if !std::path::Path::new(&place.path).exists() {
            return None;
        }
        match crate::worktrees::dirty(place) {
            Ok(why) => why,
            Err(e) => Some(format!("{e:#}")),
        }
    }

    fn in_space(&self, space: &str) -> Vec<String> {
        let get = |a: &str| hc::get(a);
        crate::missions::scan(&get, &|_| None)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.space.trim().eq_ignore_ascii_case(space.trim()))
            .map(|s| s.address)
            .collect()
    }

    fn shared(&self, place: &Place, mine: &[String]) -> bool {
        hc::list_on(None)
            .unwrap_or_default()
            .iter()
            .filter(|t| t["state"].as_str().is_none_or(|s| s == "live"))
            .filter_map(|t| t["address"].as_str())
            .filter(|a| !mine.iter().any(|m| m == a))
            .filter_map(hc::get)
            .any(|t| crate::worktrees::given(&t).worktree.as_deref() == Some(place.path.as_str()))
    }

    fn forget(&self, s: &Opened) {
        // Its task is dropped before its session ends: nothing is told as stalled or failed.
        if let Some(task) = &s.task {
            let r = self.agendas.request(&crate::agenda::Cmd { action: "abandon".into(), target: task.clone(), text: "undone".into(), ..Default::default() });
            if !r.ok {
                eprintln!("[acts] {task} stays as it is: {}", r.error.unwrap_or_default());
            }
        }
        for item in self.sendq.items().into_iter().filter(|i| i.session == s.address) {
            let _ = self.sendq.cancel(item.id);
        }
        self.watches.forget(&s.address);
    }

    fn end(&self, address: &str) -> Result<(), String> {
        let killed = hc::run(&["kill", address]);
        let _ = hc::run(&["rm", address]);
        match killed {
            Ok(o) if o.code == 0 || !hc::is_live(address) => Ok(()),
            Ok(o) => Err(o.stderr.trim().to_string()),
            Err(e) => Err(format!("{e:#}")),
        }
    }

    fn remove(&self, space: Option<&str>, place: &Place) -> Removal {
        // Its program lets go of the folder first.
        let until = Instant::now() + Duration::from_secs(8);
        let held = |p: &Place| {
            hc::list_on(None).unwrap_or_default().iter().any(|t| {
                t["state"].as_str().is_none_or(|s| s == "live") && t["cwd"].as_str().is_some_and(|c| c == p.path || c.starts_with(&format!("{}/", p.path)))
            })
        };
        while held(place) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(300));
        }
        let done = crate::worktrees::remove(place);
        // The space no longer keeps a worktree that is gone.
        if let (Removal::Removed | Removal::Gone, Some(space)) = (&done, space) {
            crate::spaces::update(|s| {
                if let Some(name) = crate::missions::of(s, space).map(|(k, _)| k.clone())
                    && let Some(m) = s.missions.get_mut(&name)
                {
                    m.worktrees.retain(|w| w.path != place.path);
                }
            });
        }
        done
    }

    fn close_space(&self, space: &str) -> Result<Value, String> {
        let v = crate::missions::close_named(space, true, false).map_err(|e| format!("{e:#}"))?;
        if let Some(name) = v["closed"].as_str() {
            self.decisions.space_over(name, true);
        }
        self.missions.now();
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acts::tests::opened;
    use crate::daemon::state::State;

    fn acts(name: &str) -> (Acts, Shared, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tower-acts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Shared::new(State::new("ptt", "test", "test"));
        (Acts::at(dir.join("acts.json"), shared.clone(), Arc::new(Hub::default())), shared, dir)
    }

    /// Hands that end whatever they are asked to.
    struct Easy;
    impl Hands for Easy {
        fn runs(&self, _: &Opened) -> bool {
            true
        }
        fn dirty(&self, _: &Place) -> Option<String> {
            None
        }
        fn in_space(&self, _: &str) -> Vec<String> {
            vec![]
        }
        fn shared(&self, _: &Place, _: &[String]) -> bool {
            false
        }
        fn forget(&self, _: &Opened) {}
        fn end(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn remove(&self, _: Option<&str>, _: &Place) -> Removal {
            Removal::Removed
        }
        fn close_space(&self, space: &str) -> Result<Value, String> {
            Ok(json!({ "closed": space, "sessions_ended": [], "sessions_left": [] }))
        }
    }

    #[test]
    fn an_act_is_kept_shown_and_undone_once() {
        let (a, shared, dir) = acts("kept");
        let (_, events) = a.hub.subscribe("d_1", false, true);
        let what = json!({
            "kind": "start", "space": "M5 Pro",
            "sessions": [serde_json::to_value(opened("t9", "fix-login", "look at the login bug")).unwrap()],
            "assumed": { "account": "orca", "account_why": "the default one: none was named", "folder": "/w/herder-tower", "folder_why": "as asked", "name": "fix-login", "name_why": "as asked" },
        });
        // Run from a plain shell: kept, shown, answering no request.
        let v = a.record(&what, None).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!((v["kind"].as_str(), v["by"].as_str(), v["line"].as_str(), v["request"].is_null()), (Some("start"), Some("cli"), Some("Opened fix login in M5 Pro."), true));
        assert_eq!(v["assumed"].as_array().unwrap().iter().map(|c| c["value"].as_str().unwrap()).collect::<Vec<_>>(), ["orca", "herder-tower", "fix-login"]);
        assert_eq!(shared.get().acts, vec![v.clone()], "in the state, as the windows read it");
        assert_eq!(a.view(&id), Some(v.clone()));
        // It survives a restart.
        let again = Acts::at(dir.join("acts.json"), Shared::new(State::new("ptt", "test", "test")), Arc::new(Hub::default()));
        assert_eq!(again.list()["acts"][0]["id"], id.as_str());
        // More work for its session: undo is refused, and says why; forced, it goes.
        a.given("t9", "also fix the logout redirect");
        assert_eq!(shared.get().acts[0]["undo"]["possible"], false);
        let refused = a.undo(None, false, &Easy).unwrap_err();
        assert!(matches!(refused, Refused::Holds(ref h) if h[0].address == "t9"), "{refused:?}");
        let done = a.undo(Some(&id), true, &Easy).unwrap();
        assert_eq!((done["closing"].as_str(), done["act"]["state"].as_str(), done["act"]["closing"].as_str()), (Some("Closed it. Nothing else changed."), Some("undone"), Some("Closed it. Nothing else changed.")));
        assert_eq!(shared.get().acts[0]["state"], "undone");
        // Once: a second undo, an id nobody has, and nothing recent.
        assert_eq!(a.undo(Some(&id), false, &Easy).unwrap_err(), Refused::Undone(id.clone()));
        assert_eq!(a.undo(Some("r000000000000"), false, &Easy).unwrap_err(), Refused::Unknown("r000000000000".into()));
        assert_eq!(a.undo(None, false, &Easy).unwrap_err(), Refused::Unknown(String::new()));
        // Every step went to the phones: recorded, held, undone.
        let states: Vec<(String, bool)> = events.try_iter().filter(|o| o.event == "act").map(|o| (o.data["state"].as_str().unwrap_or("").to_string(), o.data["undo"]["possible"] == true)).collect();
        assert_eq!(states, [("done".to_string(), true), ("done".to_string(), false), ("undone".to_string(), false)]);
        // What isn't an act is refused.
        assert!(a.record(&json!({ "kind": "start", "sessions": [] }), None).is_err());
        assert!(a.record(&json!({ "kind": "merge", "sessions": [{ "address": "t1" }] }), None).is_err());
        // A dispatch's sessions are a start's; a launch keeps its goal and its chips.
        assert_eq!(a.record(&json!({ "kind": "dispatch", "sessions": [{ "address": "t3", "name": "api" }], "by": "phone:d_1" }), None).unwrap()["kind"], "start");
        let l = a.record(&json!({ "kind": "launch", "space": "fix-login", "space_made": true, "goal": "Fix the login", "sessions": [{ "address": "t5", "name": "fix-login-eyzflow" }], "by": "phone:d_1", "assumed": { "setup": "eyzflow on orca", "setup_why": "the only one", "worktrees": true, "worktrees_why": "one per repo" } }), None).unwrap();
        assert_eq!((l["by"].as_str(), l["line"].as_str(), l["assumed"][1]["what"].as_str()), (Some("phone:d_1"), Some("Launched fix-login: 1 session, eyzflow."), Some("worktrees")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
