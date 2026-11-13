//! The plans in the daemon (see `crate::plans`): kept in `plans.json` (the
//! state folder) across restarts, the open one shown in `state.json`
//! (`plan`), each with its planner: a Claude Code terminal the daemon
//! starts, follows and ends.
//!
//! - `new` closes the plan that is open (one at a time), writes the new
//!   one's folder and starts its planner on a thread of its own (the
//!   answer doesn't wait for it); its first-run folder question is the
//!   tower's to answer (`brain::settle`).
//! - What the user says is kept in the plan's thread and queued to the
//!   planner as a quiet note (`sendq::add_note`: it waits for the
//!   planner's turn to end, and is no task); the planner's answers come
//!   back through `tower plan say`.
//! - Every couple of seconds the open plan's planner is looked at: a
//!   dialog in its terminal (`blocked`, for the window's "Show in Herdr"),
//!   at work, at rest, or gone (a plan it hadn't published yet fails).
//! - `close` ends the planner (`hc kill`, the terminal removed).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::sendq::SendQueue;
use super::state::{Shared, lock};
use crate::plans::{self, Book, Form, Plan, Who};
use crate::protocol::{Request, Response};
use crate::{brain, config, hc};

/// How often the open plan's planner is looked at.
const LOOK: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Plans {
    book: Arc<Mutex<Book>>,
    path: PathBuf,
    shared: Shared,
    sendq: SendQueue,
    /// One start or end of a planner at a time (they share a name).
    hands: Arc<Mutex<()>>,
}

fn now() -> u64 {
    crate::history::now_ms()
}

impl Plans {
    pub fn path() -> PathBuf {
        config::state_dir().join("plans.json")
    }

    /// The plans as the last run left them.
    pub fn open(shared: Shared, sendq: SendQueue) -> Plans {
        Plans::at(Self::path(), shared, sendq)
    }

    pub fn at(path: PathBuf, shared: Shared, sendq: SendQueue) -> Plans {
        let book: Book = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let plans = Plans { book: Arc::new(Mutex::new(book)), path, shared, sendq, hands: Arc::default() };
        plans.publish(&lock(&plans.book));
        plans
    }

    /// Follows the open plan's planner from now on.
    pub fn start(&self) {
        let me = self.clone();
        let _ = thread::Builder::new().name("plans".into()).spawn(move || {
            loop {
                me.look();
                thread::sleep(LOOK);
            }
        });
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
        let view = b.current().map(Plan::view);
        self.shared.update(|s| s.plan = view);
    }

    /// One change to the store: kept and shown.
    fn change(&self, f: impl FnOnce(&mut Book) -> Result<Plan>) -> Result<Plan> {
        let mut b = lock(&self.book);
        let p = f(&mut b)?;
        self.save(&b);
        self.publish(&b);
        Ok(p)
    }

    /// The open plan's planner, as Herdr has it now.
    fn look(&self) {
        let Some((id, address)) = lock(&self.book).current().and_then(|p| Some((p.id.clone(), p.address.clone()?))) else { return };
        let word = match hc::lookup(&address) {
            hc::Lookup::Found(t) if t["state"].as_str().is_none_or(|s| s == "live") => hc::status_word(&t).unwrap_or_else(|| "starting".into()),
            hc::Lookup::Found(_) | hc::Lookup::Gone => "gone".into(),
            hc::Lookup::Unreachable(_) => return,
        };
        let mut b = lock(&self.book);
        if b.seen(&id, &word, now()) {
            if word == "gone" {
                eprintln!("[plans] {id}: the planner ({address}) is gone");
            }
            self.save(&b);
            self.publish(&b);
        }
    }

    /// A `plan` request (see `Request::Plan`).
    pub fn request(&self, req: &Request) -> Response {
        let Request::Plan { action, id, form, ticket, draft, text, url, who, launched, prs } = req else { return Response::err("not a plan request") };
        let id = id.trim();
        let id = (!id.is_empty()).then_some(id);
        let done = |r: Result<Value>| match r {
            Ok(v) => Response { ok: true, error: None, state: Some(v) },
            Err(e) => Response::err(format!("{e:#}")),
        };
        let view = |r: Result<Plan>| done(r.map(|p| p.view()));
        match action.as_str() {
            "new" => view(Form::parse(form).and_then(|f| self.new_plan(f, ticket, draft.clone().unwrap_or_default()))),
            "say" => view(self.pick_id(id).and_then(|id| {
                let who = if who.trim().is_empty() { Ok(Who::User) } else { Who::parse(who) };
                who.and_then(|w| self.say(&id, w, text))
            })),
            "draft" => view(self.pick_id(id).and_then(|id| self.revise(&id, draft.clone().unwrap_or_default()))),
            "prs" => view(self.pick_id(id).and_then(|id| self.predict(&id, prs.clone().unwrap_or_default()))),
            "published" => view(self.pick_id(id).and_then(|id| self.change(|b| b.published(&id, url, now())))),
            "failed" => view(self.pick_id(id).and_then(|id| self.change(|b| b.failed(&id, text, now())))),
            "close" => view(self.pick_id(id).and_then(|id| self.close(&id, launched.as_deref()))),
            "show" | "" => done(lock(&self.book).pick(id).map(Plan::view)),
            "list" => done(Ok(json!({ "plans": lock(&self.book).plans.iter().rev().map(Plan::view).collect::<Vec<_>>() }))),
            other => Response::err(format!("plan is new, say, draft, prs, published, failed, show, close or list, not {other:?}")),
        }
    }

    /// `id`, or the open plan's.
    fn pick_id(&self, id: Option<&str>) -> Result<String> {
        Ok(lock(&self.book).pick(id)?.id.clone())
    }

    /// A new plan: the open one is closed, the new one's planner started.
    fn new_plan(&self, form: Form, ticket: &str, draft: Value) -> Result<Plan> {
        plans::check_draft(&draft)?;
        if ticket.trim().is_empty() {
            bail!("the plan has no ticket");
        }
        let (plan, ended) = {
            let mut b = lock(&self.book);
            let (plan, ended) = b.add(form, ticket, draft, |id| plans::plans_dir().join(id), now());
            self.save(&b);
            self.publish(&b);
            (plan, ended)
        };
        eprintln!("[plans] {} ({}) for \"{}\"", plan.id, plan.form.word(), plan.draft["space"].as_str().unwrap_or(""));
        let me = self.clone();
        let p = plan.clone();
        thread::spawn(move || me.start_planner(&p, &ended));
        Ok(plan)
    }

    /// Starts `p`'s planner, once those of the plans it `ended` are gone.
    fn start_planner(&self, p: &Plan, ended: &[Plan]) {
        let _hands = lock(&self.hands);
        for e in ended {
            if let Some(a) = &e.address {
                brain::stop(a);
            }
        }
        // A planner left from before (finished, or of a run that stopped) has the new one's name.
        if hc::get(plans::PLANNER).is_some() {
            brain::stop(plans::PLANNER);
        }
        match run_planner(p) {
            Ok(address) => {
                eprintln!("[plans] {}: the planner runs ({address})", p.id);
                if self.change(|b| b.started(&p.id, &address, now())).is_err() {
                    // Closed while it started: it goes again.
                    brain::stop(&address);
                    return;
                }
                brain::settle(&address);
            }
            Err(e) => {
                eprintln!("[plans] {}: the planner didn't start: {e:#}", p.id);
                let _ = self.change(|b| b.failed(&p.id, &format!("the planner didn't start: {e:#}"), now()));
            }
        }
    }

    /// A line of the chat. The user's goes to the planner (queued until its turn is over).
    fn say(&self, id: &str, who: Who, text: &str) -> Result<Plan> {
        if who == Who::User {
            let p = lock(&self.book).get(id).cloned().ok_or_else(|| anyhow::anyhow!("no plan {id}"))?;
            if !p.is_open() {
                bail!("plan {id} is closed");
            }
            let Some(address) = p.address.clone() else {
                bail!("the planner isn't running{}: Plan again", if p.planner == "starting" { " yet" } else { "" });
            };
            if text.trim().is_empty() {
                bail!("nothing to say");
            }
            let prompt = plans::chat_prompt(&p, text, &config::tower_bin().display().to_string());
            self.sendq.add_note(&address, &prompt).map_err(|e| anyhow::anyhow!("couldn't reach the planner: {e:#}"))?;
        }
        self.change(|b| b.say(id, who, text, now()))
    }

    /// The planner's revised draft: kept, and in its folder too.
    fn revise(&self, id: &str, draft: Value) -> Result<Plan> {
        let p = self.change(|b| b.set_draft(id, draft, now()))?;
        let _ = std::fs::write(PathBuf::from(&p.dir).join("draft.json"), serde_json::to_string_pretty(&p.draft).unwrap_or_default());
        Ok(p)
    }

    /// The PRs the planner predicts: checked against the plan's draft, scored, kept.
    fn predict(&self, id: &str, v: Value) -> Result<Plan> {
        let repos = {
            let b = lock(&self.book);
            plans::repos_of(&b.get(id).ok_or_else(|| anyhow::anyhow!("no plan {id}"))?.draft)
        };
        let prs = crate::prscore::check(&v, &repos)?;
        let p = self.change(|b| b.set_prs(id, prs, now()))?;
        eprintln!("[plans] {id}: {} PR(s) predicted, {} needing review", p.prs.len(), p.prs.iter().filter(|x| x.score.needs_review).count());
        Ok(p)
    }

    /// The prediction a PR just found is, in the plan its space was
    /// launched with: (the plan's id, the prediction), marked as opened.
    pub fn match_pr(&self, pr: &crate::prs::Pr) -> Option<(String, crate::prscore::Predicted)> {
        let mut b = lock(&self.book);
        let found = b.match_pr(pr.space.as_deref().unwrap_or(""), &pr.repo, &pr.base, &pr.title, &pr.key(), now())?;
        self.save(&b);
        self.publish(&b);
        Some(found)
    }

    /// Closed, and its planner ended.
    fn close(&self, id: &str, launched: Option<&str>) -> Result<Plan> {
        let p = self.change(|b| b.close(id, launched, now()))?;
        eprintln!("[plans] {id} closed{}", launched.map(|s| format!(": launched as {s}")).unwrap_or_default());
        // (A test never ends a terminal: Herdr's are the user's.)
        if cfg!(test) {
            return Ok(p);
        }
        let me = self.clone();
        let (address, id) = (p.address.clone(), p.id.clone());
        thread::spawn(move || {
            let _hands = lock(&me.hands);
            if let Some(a) = address {
                brain::stop(&a);
            }
            // One still called so, of this plan (it started meanwhile).
            if hc::get(plans::PLANNER).is_some_and(|t| t["labels"][plans::LABEL_PLAN].as_str() == Some(id.as_str())) {
                brain::stop(plans::PLANNER);
            }
        });
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::State;

    fn draft() -> Value {
        json!({ "space": "msg-translation", "goal": "Translate support messages", "sessions": [{ "repo": "/w/eyz/eyz-customer-support", "why": "the inbox" }] })
    }

    fn ask(action: &str, id: &str) -> Request {
        Request::Plan { action: action.into(), id: id.into(), form: String::new(), ticket: String::new(), draft: None, text: String::new(), url: String::new(), who: String::new(), launched: None, prs: None }
    }

    #[test]
    fn a_plan_is_kept_across_a_restart_and_shown_in_the_state() {
        let dir = std::env::temp_dir().join(format!("tower-plans-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("plans.json");
        let shared = Shared::new(State::new("off", "test", "test"));
        let plans = Plans::at(path.clone(), shared.clone(), SendQueue::open());
        assert!(shared.get().plan.is_none(), "no plan: none in the state");
        // (Its planner isn't started here: the book is written as `new` writes it.)
        plans.change(|b| Ok(b.add(Form::Whiteboard, "The ticket", draft(), |id| dir.join(id), 1).0)).unwrap();
        plans.change(|b| b.started("p1", "t9", 2)).unwrap();
        std::fs::create_dir_all(dir.join("p1")).unwrap();
        assert_eq!(shared.get().plan.as_ref().unwrap()["status"], "writing");
        // The planner reports: an answer, a published link, a revised draft.
        let mut say = ask("say", "p1");
        if let Request::Plan { text, who, .. } = &mut say {
            (*text, *who) = ("The inbox service starts first.".into(), "planner".into());
        }
        assert!(plans.request(&say).ok);
        let mut published = ask("published", "p1");
        if let Request::Plan { url, .. } = &mut published {
            *url = "https://claude.ai/artifact/wb".into();
        }
        let r = plans.request(&published);
        assert!(r.ok, "{:?}", r.error);
        let mut revised = ask("draft", "");
        if let Request::Plan { draft: d, .. } = &mut revised {
            let mut v = draft();
            v["space"] = "msg-translate".into();
            *d = Some(v);
        }
        assert!(plans.request(&revised).ok, "no id: the open plan");
        // The planner's predicted PRs: scored and shown; one for a repo the draft hasn't is refused.
        let mut predicted = ask("prs", "p1");
        if let Request::Plan { prs, .. } = &mut predicted {
            *prs = Some(json!({ "prs": [
                { "repo": "eyz-customer-support", "title": "Translations table", "files": ["db/migrations/0042_translations.sql"], "migration": true },
                { "repo": "/w/eyz/eyz-customer-support", "title": "Translate button styling", "files": ["src/ui/Button.tsx"] },
            ]}));
        }
        let r = plans.request(&predicted);
        assert!(r.ok, "{:?}", r.error);
        let shown = shared.get().plan.unwrap();
        assert_eq!((shown["prs"][0]["score"]["needs_review"].as_bool(), shown["prs"][1]["score"]["needs_review"].as_bool()), (Some(true), Some(false)));
        let mut wrong = ask("prs", "p1");
        if let Request::Plan { prs, .. } = &mut wrong {
            *prs = Some(json!([{ "repo": "elsewhere", "title": "x" }]));
        }
        assert!(plans.request(&wrong).error.unwrap().contains("isn't one of the draft's"));
        let state = shared.get().plan.unwrap();
        assert_eq!((state["status"].as_str(), state["url"].as_str(), state["revision"].as_u64()), (Some("ready"), Some("https://claude.ai/artifact/wb"), Some(1)));
        assert_eq!((state["thread"][0]["from"].as_str(), state["draft"]["space"].as_str()), (Some("planner"), Some("msg-translate")));
        assert!(std::fs::read_to_string(dir.join("p1/draft.json")).unwrap().contains("msg-translate"), "the planner's folder has it too");
        // A restart: the same plan, from plans.json.
        let again = Plans::at(path.clone(), Shared::new(State::new("off", "test", "test")), SendQueue::open());
        let shown = again.request(&ask("show", ""));
        assert_eq!(shown.state.unwrap()["url"], "https://claude.ai/artifact/wb");
        // Closed: out of the state, still listed.
        let mut close = ask("close", "p1");
        if let Request::Plan { launched, .. } = &mut close {
            *launched = Some("msg-translate".into());
        }
        assert!(again.request(&close).ok);
        assert!(again.shared.get().plan.is_none());
        let list = again.request(&ask("list", "")).state.unwrap();
        assert_eq!((list["plans"][0]["status"].as_str(), list["plans"][0]["launched"].as_str()), (Some("closed"), Some("msg-translate")));
        // What can't be: a plan with no draft, an unknown action, the user's words with no planner.
        let mut bad = ask("new", "");
        if let Request::Plan { form, ticket, draft: d, .. } = &mut bad {
            (*form, *ticket, *d) = ("page".into(), "T".into(), Some(json!({})));
        }
        assert!(!again.request(&bad).ok);
        assert!(!again.request(&ask("launch", "p1")).ok);
        assert!(again.request(&ask("show", "")).error.unwrap().contains("no plan is open"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Starts the planner of `p` in a hidden Herdr terminal: its folder written
/// first. Returns its address.
fn run_planner(p: &Plan) -> Result<String> {
    let cfg = config::Config::live();
    let tower = config::tower_bin().display().to_string();
    plans::write_folder(p, &tower)?;
    let account = crate::accounts::list().unwrap_or_default().first().map(|a| a.name.clone());
    let cca = crate::accounts::cca_bin().map(|c| c.to_string_lossy().into_owned());
    let argv = plans::argv(p, &cfg, cca.as_deref().filter(|_| account.is_some()), account.as_deref(), &tower, &plans::tower_env())?;
    let labels = [format!("{}=planner", plans::LABEL_ROLE), format!("{}={}", plans::LABEL_PLAN, p.id)];
    let mut args: Vec<&str> = vec!["run", "--bg", "--json", "--name", plans::PLANNER, "--no-notify", "--no-parent", "--cwd", &p.dir];
    for l in &labels {
        args.extend(["--label", l.as_str()]);
    }
    args.push("--");
    args.extend(argv.iter().map(String::as_str));
    let out = hc::run(&args)?;
    if out.code != 0 {
        bail!("hc run: {}{}", out.stderr.trim(), out.stdout.trim());
    }
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap_or_default();
    Ok(v["address"].as_str().unwrap_or(plans::PLANNER).to_string())
}
