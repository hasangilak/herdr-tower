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
/// A question with the brain this long without its word goes to the user.
const BRAIN_MS: u64 = 2 * 60_000;

/// A PR's planned checks as the tower has them, by its key.
pub type PrChecks = Arc<dyn Fn(&str) -> Option<crate::prscore::Validation> + Send + Sync>;
/// Hands a note to the tower's brain.
pub type BrainNote = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct Plans {
    book: Arc<Mutex<Book>>,
    path: PathBuf,
    shared: Shared,
    sendq: SendQueue,
    /// One start or end of a planner at a time (they share a name).
    hands: Arc<Mutex<()>>,
    /// The user's queue: a plan's questions that are theirs become cards there.
    decisions: Option<super::decisions::Decisions>,
    /// The PRs' checks, for a stage's state (none: no PR has opened).
    pr_checks: Option<PrChecks>,
    /// The brain, for the questions it may answer itself.
    brain: Option<BrainNote>,
    /// How long the brain has before a question goes to the user.
    brain_ms: u64,
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
        let plans = Plans { book: Arc::new(Mutex::new(book)), path, shared, sendq, hands: Arc::default(), decisions: None, pr_checks: None, brain: None, brain_ms: BRAIN_MS };
        plans.publish(&lock(&plans.book));
        plans
    }

    /// With the user's queue, the PRs' checks and the brain: questions are
    /// routed, stages see their PRs.
    pub fn wired(mut self, decisions: super::decisions::Decisions, pr_checks: PrChecks, brain: BrainNote) -> Plans {
        (self.decisions, self.pr_checks, self.brain) = (Some(decisions), Some(pr_checks), Some(brain));
        self.publish(&lock(&self.book));
        self
    }

    /// How long the brain has before a question goes to the user (tests).
    #[cfg(test)]
    pub fn brain_within(mut self, ms: u64) -> Plans {
        self.brain_ms = ms;
        self
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
        let view = b.current().map(|p| self.view_of(p));
        let live: Vec<Value> = b.live().into_iter().map(|p| self.view_of(p)).collect();
        self.shared.update(|s| {
            s.plan = view;
            s.plans_live = live;
        });
    }

    /// A plan as the windows and the phone get it: with where its stages stand.
    fn view_of(&self, p: &Plan) -> Value {
        let none = |_: &str| None;
        match &self.pr_checks {
            Some(f) => p.view_with(&|k| f(k)),
            None => p.view_with(&none),
        }
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
        // The open plan's planner, or a live plan's (revising it).
        let Some((id, address)) = lock(&self.book).plans.iter().rev().filter(|p| p.is_open()).find_map(|p| Some((p.id.clone(), p.address.clone()?))) else { return };
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
        let Request::Plan { action, id, form, ticket, draft, text, url, who, launched, prs, body } = req else { return Response::err("not a plan request") };
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
            "stages" => done(self.pick_id(id).and_then(|id| self.structure(&id, body.clone().unwrap_or_default())).map(|p| self.view_of(&p))),
            "answer" => {
                let b = body.clone().unwrap_or_default();
                let by = if b["by"].as_str().is_some_and(|w| w.trim().eq_ignore_ascii_case("tower")) { "tower" } else { "you" };
                done(self.pick_id(id).and_then(|id| self.answer(&id, b["question"].as_str().unwrap_or(""), b["answer"].as_str().unwrap_or(""), by)))
            }
            "ask" => {
                let q = body.as_ref().and_then(|b| b["question"].as_str()).unwrap_or("").to_string();
                done(self.pick_id(id).and_then(|id| self.ask_user(&id, &q, "the tower's brain says it is the user's")))
            }
            "check" => {
                let b = body.clone().unwrap_or_default();
                let w = |k: &str| b[k].as_str().unwrap_or("").to_string();
                done(self.pick_id(id).and_then(|id| self.check(&id, &w("stage"), &w("check"), &w("status"), &w("evidence"), &w("by"))))
            }
            "finding" => {
                let b = body.clone().unwrap_or_default();
                let w = |k: &str| b[k].as_str().unwrap_or("").to_string();
                done(self.pick_id(id).and_then(|id| self.found(&id, &w("kind"), &w("text"), &w("stage"), &w("assumption"), &w("session"))))
            }
            "assume" => {
                let b = body.clone().unwrap_or_default();
                let w = |k: &str| b[k].as_str().unwrap_or("").to_string();
                let holds = match w("status").to_lowercase().as_str() {
                    "holds" | "hold" | "ok" | "true" => Ok(true),
                    "broken" | "wrong" | "false" => Ok(false),
                    other => Err(anyhow::anyhow!("an assumption holds or is broken, not {other:?}")),
                };
                done(holds.and_then(|holds| self.pick_id(id).and_then(|id| self.assume(&id, &w("assumption"), holds, &w("evidence"), &w("session")))))
            }
            "revise" => {
                let b = body.clone().unwrap_or_default();
                let findings: Vec<String> = b["findings"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
                let by = if who.trim().is_empty() { "planner" } else { who.as_str() };
                done(self.pick_id(id).and_then(|id| self.revise_structure(&id, &b["structure"], text, &findings, by)))
            }
            "show" | "" => done(lock(&self.book).pick(id).map(|p| self.view_of(p))),
            "list" => done(Ok(json!({ "plans": lock(&self.book).plans.iter().rev().map(|p| self.view_of(p)).collect::<Vec<_>>() }))),
            other => Response::err(format!("plan is new, say, draft, prs, stages, answer, ask, check, published, failed, show, close or list, not {other:?}")),
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

    /// The planner's stages, assumptions and questions: kept, and each new
    /// open question routed (`route`).
    fn structure(&self, id: &str, v: Value) -> Result<Plan> {
        let p = self.change(|b| b.set_structure(id, &v, now()))?;
        let _ = std::fs::write(PathBuf::from(&p.dir).join("stages.json"), serde_json::to_string_pretty(&v).unwrap_or_default());
        eprintln!("[plans] {id}: {} stage(s), {} assumption(s), {} question(s) ({} open)", p.stages.len(), p.assumptions.len(), p.questions.len(), p.open_questions().len());
        self.route(id);
        Ok(lock(&self.book).get(id).cloned().unwrap_or(p))
    }

    /// Each question just written goes where it is answered: the user's
    /// queue when the planner says it is theirs, or the tower's rules do
    /// (`questions::must_escalate`: secrets, deploys, deletes, prod data,
    /// money); else the brain, which answers what is routine (the 90/10
    /// rule) and hands on the rest, and the user gets it when the brain
    /// says nothing in time.
    pub fn route(&self, id: &str) {
        let (fresh, space) = {
            let b = lock(&self.book);
            let Some(p) = b.get(id) else { return };
            (p.questions.iter().filter(|q| q.status == plans::Q_OPEN).cloned().collect::<Vec<_>>(), p.draft["space"].as_str().unwrap_or("").to_string())
        };
        for q in fresh {
            let rule = crate::questions::must_escalate(&q.as_set(id));
            if q.theirs || rule.is_some() || self.brain.is_none() {
                let why = if q.theirs { "the planner says it is yours".to_string() } else { rule.map(|r| format!("the tower's rule: {r}")).unwrap_or_else(|| "no brain to weigh it".into()) };
                if let Err(e) = self.ask_user(id, &q.id, &why) {
                    eprintln!("[plans] {id} {}: couldn't ask the user: {e:#}", q.id);
                }
                continue;
            }
            if self.change(|b| b.question_at(id, &q.id, plans::Q_WITH_TOWER, None, now()).map(|_| b.get(id).cloned().unwrap_or_default())).is_err() {
                continue;
            }
            let note = plans::question_note(id, &space, &q, &config::tower_bin().display().to_string());
            if let Some(brain) = &self.brain {
                let (brain, note) = (brain.clone(), note.clone());
                thread::spawn(move || brain(&note));
            }
            eprintln!("[plans] {id} {}: with the brain: {}", q.id, q.text);
            // No word in time: the user's.
            let (me, id, qid, wait) = (self.clone(), id.to_string(), q.id.clone(), self.brain_ms);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(wait));
                let still = lock(&me.book).get(&id).and_then(|p| p.questions.iter().find(|x| x.id == qid).map(|x| x.status == plans::Q_WITH_TOWER)).unwrap_or(false);
                if still && let Err(e) = me.ask_user(&id, &qid, "no word from the tower's brain in time") {
                    eprintln!("[plans] {id} {qid}: couldn't ask the user: {e:#}");
                }
            });
        }
    }

    /// A question goes to the user's queue as a card (why: what sent it there).
    pub fn ask_user(&self, id: &str, q: &str, why: &str) -> Result<Value> {
        let (x, space) = {
            let b = lock(&self.book);
            let p = b.get(id).ok_or_else(|| anyhow::anyhow!("no plan {id}"))?;
            let x = p.questions.iter().find(|x| x.id.eq_ignore_ascii_case(q.trim())).cloned().ok_or_else(|| anyhow::anyhow!("plan {id} has no question {q:?}"))?;
            (x, p.draft["space"].as_str().map(String::from))
        };
        if !x.is_open() {
            bail!("{} of plan {id} is answered already: {}", x.id, x.answer);
        }
        let Some(decisions) = &self.decisions else { bail!("the tower has no queue to ask in") };
        let ask = plans::card_of(id, space.as_deref(), &x);
        let (card, told) = decisions.plan_question(ask);
        self.change(|b| b.question_at(id, &x.id, plans::Q_ASKED, Some(&card), now()).map(|_| b.get(id).cloned().unwrap_or_default()))?;
        eprintln!("[plans] {id} {}: asked the user ({card}): {why}", x.id);
        Ok(json!({ "plan": id, "question": x.id, "card": card, "told": told, "why": why }))
    }

    /// A question answered (`by`: `you` or `tower`): written into the
    /// plan, its card closed, and the planner told to put it in the plan
    /// and its artifact.
    pub fn answer(&self, id: &str, q: &str, answer: &str, by: &str) -> Result<Value> {
        let x = self.change(|b| b.answer(id, q, answer, by, now()).map(|_| b.get(id).cloned().unwrap_or_default()))?;
        let qx = x.questions.iter().find(|y| y.id.eq_ignore_ascii_case(q.trim())).cloned().unwrap_or_default();
        if let Some(d) = &self.decisions {
            d.plan_answered(id, &qx.id, &qx.answer);
        }
        eprintln!("[plans] {id} {}: answered by {by}: {}", qx.id, qx.answer);
        if let Some(address) = x.address.clone().filter(|_| x.is_open() && !cfg!(test)) {
            let note = plans::answered_note(&x, &qx, &config::tower_bin().display().to_string());
            if let Err(e) = self.sendq.add_note(&address, &note) {
                eprintln!("[plans] {id}: couldn't tell the planner: {e:#}");
            }
        }
        Ok(json!({ "plan": id, "question": qx.id, "answer": qx.answer, "by": qx.by, "open_questions": x.open_questions().len() }))
    }

    /// A finding on a plan: kept, and told to whoever revises the plan.
    fn found(&self, id: &str, kind: &str, text: &str, stage: &str, assumption: &str, session: &str) -> Result<Value> {
        let mut f = None;
        let p = self.change(|b| {
            f = Some(b.finding(id, kind, text, stage, assumption, session, now())?);
            Ok(b.get(id).cloned().unwrap_or_default())
        })?;
        let f = f.expect("set above");
        eprintln!("[plans] {id} {}: {} ({})", f.id, f.text, f.kind);
        let to = self.tell_reviser(&p, &f);
        Ok(json!({ "plan": id, "finding": f, "told": to, "version": p.version }))
    }

    /// An assumption checked; a broken one is a finding, told as one.
    fn assume(&self, id: &str, a: &str, holds: bool, evidence: &str, session: &str) -> Result<Value> {
        let mut out = None;
        let p = self.change(|b| {
            out = Some(b.assume(id, a, holds, evidence, session, now())?);
            Ok(b.get(id).cloned().unwrap_or_default())
        })?;
        let (x, f) = out.expect("set above");
        let told = f.as_ref().map(|f| self.tell_reviser(&p, f));
        Ok(json!({ "plan": id, "assumption": x, "finding": f, "told": told }))
    }

    /// Who revises the plan for a finding: its planner when it runs; else,
    /// for a live plan, a planner started for it (when no other plan's
    /// planner runs); else the brain, to start one or say it to the user.
    fn tell_reviser(&self, p: &Plan, f: &plans::Finding) -> &'static str {
        let tower = config::tower_bin().display().to_string();
        if let Some(address) = &p.address {
            match self.sendq.add_note(address, &plans::finding_note(p, f, &tower)) {
                Ok(_) => return "the planner",
                Err(e) => eprintln!("[plans] {}: couldn't tell its planner: {e:#}", p.id),
            }
        }
        let busy = lock(&self.book).plans.iter().any(|x| x.id != p.id && x.is_open() && x.address.is_some());
        // (A test tower never starts a real planner in the user's Herdr.)
        if p.status == plans::Status::Live && !busy && !cfg!(test) && !super::watch::test_daemon() {
            let (me, p) = (self.clone(), p.clone());
            thread::spawn(move || me.start_planner(&p, &[]));
            return "a planner started for it";
        }
        if let Some(brain) = &self.brain {
            let note = format!(
                "[plan-finding] {} {} {}: no planner can take it now. Tell the user in a sentence what was found and that the plan waits for its revision.",
                p.id,
                f.id,
                serde_json::to_string(f).unwrap_or_default()
            );
            let brain = brain.clone();
            thread::spawn(move || brain(&note));
            return "the brain";
        }
        "nobody"
    }

    /// A revision of a plan's structure: small, applied (and new questions
    /// routed); big, a card in the user's queue with what changes.
    fn revise_structure(&self, id: &str, v: &Value, why: &str, findings: &[String], by: &str) -> Result<Value> {
        let r = self.change(|b| b.revise(id, v, why, findings, by, now()).map(|_| b.get(id).cloned().unwrap_or_default()))?;
        let rev = r.revisions.last().cloned().unwrap_or_default();
        eprintln!("[plans] {id}: revision {} ({}, {}): {}", rev.n, rev.size, rev.status, rev.changes.join("; "));
        if rev.status == "applied" {
            self.route(id);
            return Ok(json!({ "plan": id, "revision": rev, "version": r.version, "said": format!("Applied by itself (a small change): the plan is version {} now. Put it in plan.md and republish with what changed.", r.version) }));
        }
        let Some(decisions) = &self.decisions else { bail!("the tower has no queue to ask in") };
        let (card, _) = decisions.plan_question(plans::revision_card(&r, &rev));
        let _ = self.change(|b| {
            b.revision_card(id, rev.n, &card);
            Ok(b.get(id).cloned().unwrap_or_default())
        });
        Ok(json!({ "plan": id, "revision": rev, "card": card, "version": r.version, "said": "A big change: it is a card in the user's queue, applied only on their yes. You'll be told their word." }))
    }

    /// A plan's card answered by the user: a question (`q1`), or a big
    /// revision (`rev:2`: applied when they chose to apply it).
    pub fn from_card(&self, id: &str, what: &str, answer: &str) -> Result<Value> {
        let Some(n) = what.strip_prefix("rev:").and_then(|n| n.parse::<u32>().ok()) else {
            return self.answer(id, what, answer, "you");
        };
        let apply = answer.trim().to_lowercase().starts_with("apply");
        let p = self.change(|b| b.decide_revision(id, n, apply, now()).map(|_| b.get(id).cloned().unwrap_or_default()))?;
        eprintln!("[plans] {id}: revision {n} {}", if apply { "applied by the user" } else { "kept out by the user" });
        if apply {
            self.route(id);
        }
        if let Some(address) = p.address.clone().filter(|_| !cfg!(test)) {
            let note = if apply {
                format!("[plan {id}] The user applied revision {n}: the plan is version {} now. Put it in plan.md and republish the same artifact with what changed.", p.version)
            } else {
                format!("[plan {id}] The user kept the plan as it is: revision {n} is not applied ({answer}). Find another way for its findings, or say with `plan say` why there is none.")
            };
            let _ = self.sendq.add_note(&address, &note);
        }
        Ok(json!({ "plan": id, "revision": n, "applied": apply, "version": p.version }))
    }

    /// A stage's check reported.
    fn check(&self, id: &str, stage: &str, check: &str, status: &str, evidence: &str, by: &str) -> Result<Value> {
        let mut c = None;
        let p = self.change(|b| {
            c = Some(b.check(id, stage, check, status, evidence, by, now())?);
            Ok(b.get(id).cloned().unwrap_or_default())
        })?;
        let v = self.view_of(&p);
        let st = v["stages"].as_array().into_iter().flatten().find(|s| s["id"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(stage.trim()))).cloned().unwrap_or_default();
        Ok(json!({ "plan": id, "stage": st["id"], "state": st["state"], "check": c, "progress": v["progress"] }))
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
        Request::Plan { action: action.into(), id: id.into(), form: String::new(), ticket: String::new(), draft: None, text: String::new(), url: String::new(), who: String::new(), launched: None, prs: None, body: None }
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
                { "repo": "eyz-customer-support", "title": "Translations table", "files": ["db/migrations/0042_translations.sql"], "migration": true,
                  "checks": [{ "what": "It applies and rolls back", "how": "npm run db:migrate && npm run db:rollback", "evidence": "the output" }],
                  "edge_cases": [{ "case": "Rows exist on rollback", "checks": ["c1"] }], "outside": "nothing outside reads the table yet" },
                { "repo": "/w/eyz/eyz-customer-support", "title": "Translate button styling", "files": ["src/ui/Button.tsx"],
                  "checks": [{ "what": "The button looks as designed", "how": "open it on dev", "evidence": "a screenshot" }],
                  "edge_cases": [{ "case": "A narrow screen", "check": "c1" }], "outside": "only this repo uses the button" },
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

    /// A realistic multi-PR task: translate support messages, across the
    /// API and the dashboard. Its plan has a spike first, two build stages
    /// and a release, three PRs, and four questions.
    fn translation_plan(plans: &Plans, dir: &std::path::Path) {
        let draft = json!({ "space": "msg-translation", "goal": "Translate support messages", "sessions": [
            { "repo": "/w/eyz/eyz-customer-support-api", "why": "the messages and the translation endpoint" },
            { "repo": "/w/eyz/eyz-customer-support-dashboard", "why": "the inbox's translate toggle" } ] });
        plans.change(|b| Ok(b.add(Form::Page, "Translate support messages into the agent's language", draft, |id| dir.join(id), 1).0)).unwrap();
        std::fs::create_dir_all(dir.join("p1")).unwrap();
        let mut predicted = ask("prs", "p1");
        if let Request::Plan { prs, .. } = &mut predicted {
            *prs = Some(json!({ "prs": [
                { "repo": "eyz-customer-support-api", "title": "Translations table", "files": ["db/migrations/0042_translations.sql"], "migration": true,
                  "checks": [{ "what": "It applies and rolls back", "how": "npm run db:migrate && npm run db:rollback", "evidence": "the output" }],
                  "edge_cases": [{ "case": "Rows exist on rollback", "checks": ["c1"] }], "outside": "nothing reads the table yet" },
                { "repo": "eyz-customer-support-api", "title": "POST /messages/:id/translate", "files": ["src/routes/translate.ts"], "after": [1],
                  "checks": [{ "what": "It translates and caches", "how": "npm test -- translate.spec.ts", "evidence": "the test output" }],
                  "edge_cases": [{ "case": "The provider times out", "checks": ["c1"] }], "outside": "only the dashboard will call it" },
                { "repo": "eyz-customer-support-dashboard", "title": "Translate toggle in the inbox", "files": ["src/inbox/Message.tsx"], "after": [2],
                  "checks": [{ "what": "The toggle shows the translation", "how": "open /inbox on dev and toggle a German message", "evidence": "a screenshot" }],
                  "edge_cases": [{ "case": "A message already in English", "checks": ["c1"] }], "outside": "a new component" } ] }));
        }
        let r = plans.request(&predicted);
        assert!(r.ok, "{:?}", r.error);
    }

    fn structure_of() -> Value {
        json!({
            "stages": [
                { "id": "s1", "title": "Provider spike", "kind": "spike", "done_when": "We know the provider's latency and cost for 1,000 messages",
                  "post": [{ "id": "c1", "what": "p95 latency under 800 ms", "how": "node scripts/spike-latency.js --n 1000", "evidence": "its p50/p95 output" }] },
                { "id": "s2", "title": "The API translates", "kind": "build", "after": ["s1"], "prs": [1, 2], "done_when": "POST /messages/:id/translate returns and caches a translation on dev",
                  "pre": [{ "id": "p1", "what": "The provider's key is in dev's secrets", "how": "aws secretsmanager describe-secret --secret-id dev/translate", "evidence": "its ARN" }],
                  "post": [{ "id": "c1", "what": "The endpoint answers on dev", "how": "curl -X POST https://api-dev/messages/42/translate", "evidence": "the JSON reply" }] },
                { "id": "s3", "title": "The inbox shows it", "kind": "build", "after": ["s2"], "prs": [3], "done_when": "An agent toggles a German message to English in the inbox on dev",
                  "post": [{ "id": "c1", "what": "The toggle works on dev", "how": "open /inbox on dev", "evidence": "a screenshot" }] },
                { "id": "s4", "title": "Release", "kind": "release", "after": ["s3"], "done_when": "dev is promoted to main and the toggle works on prod",
                  "post": [{ "id": "c1", "what": "It works on prod", "how": "open /inbox on prod", "evidence": "a screenshot" }] }
            ],
            "assumptions": [{ "id": "a1", "text": "Messages carry a language code", "check": "select count(*) from messages where lang is null", "by": "s2" }],
            "questions": [
                { "id": "q1", "text": "Cache translations for 30 days or forever?", "why": "storage against cost", "options": [{ "label": "30 days", "why": "bounded storage" }, { "label": "Forever", "why": "no repeat cost" }], "recommended": 0 },
                { "id": "q2", "text": "Which languages first?", "why": "the dashboard's language list", "options": [{ "label": "German and French" }, { "label": "All the provider has" }], "recommended": 0, "theirs": true, "stage": "s3" },
                { "id": "q3", "text": "Put the provider's API key in prod's secrets now?", "options": [{ "label": "Yes, now" }, { "label": "At release" }], "recommended": 1 },
                { "id": "q4", "text": "Name the toggle 'Translate' or 'Show in English'?", "options": [{ "label": "Translate" }, { "label": "Show in English" }], "recommended": 0 }
            ]
        })
    }

    #[test]
    fn a_plans_questions_are_answered_through_the_tower_and_written_back_and_a_stage_is_done_only_when_its_checks_pass() {
        use crate::daemon::decisions::{Carry, Decisions};
        let dir = std::env::temp_dir().join(format!("tower-plans-v2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Shared::new(State::new("off", "test", "test"));
        let decisions = Decisions::new(shared.clone());
        let notes: Arc<Mutex<Vec<String>>> = Arc::default();
        let heard = notes.clone();
        let brain: BrainNote = Arc::new(move |t: &str| heard.lock().unwrap().push(t.to_string()));
        // PR 1 opened as #7, all its checks passed; PR 2 as #8, one check not reported yet.
        let pr_checks: PrChecks = Arc::new(|key: &str| {
            let c = |status: &str| crate::prscore::Check { id: "c1".into(), status: status.into(), ..Default::default() };
            match key {
                "eyz/api#7" => Some(crate::prscore::Validation { checks: vec![c(crate::prscore::PASSED)], edge_cases: vec![] }),
                "eyz/api#8" => Some(crate::prscore::Validation { checks: vec![c(crate::prscore::PLANNED)], edge_cases: vec![] }),
                _ => None,
            }
        });
        let plans = Plans::at(dir.join("plans.json"), shared.clone(), SendQueue::open()).wired(decisions.clone(), pr_checks, brain).brain_within(300);
        translation_plan(&plans, &dir);

        // A stage that waits for one after it, or names a PR the plan hasn't, is refused.
        let mut bad = ask("stages", "p1");
        if let Request::Plan { body, .. } = &mut bad {
            let mut v = structure_of();
            v["stages"][1]["prs"] = json!([9]);
            *body = Some(v);
        }
        assert!(plans.request(&bad).error.unwrap().contains("no PR 9"));

        // The structure: q2 is the user's (the planner says so), q3 by the tower's rule (prod, secrets);
        // q1 and q4 go to the brain.
        let mut st = ask("stages", "p1");
        if let Request::Plan { body, .. } = &mut st {
            *body = Some(structure_of());
        }
        let r = plans.request(&st);
        assert!(r.ok, "{:?}", r.error);
        let shown = shared.get().plan.unwrap();
        let status = |v: &Value, q: usize| v["questions"][q]["status"].as_str().unwrap_or("").to_string();
        assert_eq!((status(&shown, 0), status(&shown, 1), status(&shown, 2), status(&shown, 3)), ("with-tower".into(), "asked".into(), "asked".into(), "with-tower".into()));
        assert_eq!(shown["open_questions"], 4);
        let queue = decisions.list(None, false);
        let cards: Vec<&str> = queue["decisions"].as_array().unwrap().iter().map(|d| d["title"].as_str().unwrap()).collect();
        assert_eq!(cards, ["Which languages first?", "Put the provider's API key in prod's secrets now?"]);
        let q2_card = shown["questions"][1]["card"].as_str().unwrap().to_string();
        let d = decisions.get(&q2_card).unwrap();
        assert_eq!((d.why.as_str(), d.space.as_deref(), d.options.iter().filter(|o| o.pick).map(|o| o.label.as_str()).collect::<Vec<_>>()), ("plan", Some("msg-translation"), vec!["German and French"]));
        assert!(d.detail.contains("It holds up stage s3"));
        let said = notes.lock().unwrap().clone();
        assert_eq!(said.len(), 2, "q1 and q4 to the brain");
        assert!(said[0].starts_with("[plan-question] p1 q1 ") && said[0].contains("\"recommended\":\"30 days\"") && said[0].contains("plan answer p1 q1"));

        // The brain answers q1 as routine: written back, no card.
        let mut a1 = ask("answer", "p1");
        if let Request::Plan { body, .. } = &mut a1 {
            *body = Some(json!({ "question": "q1", "answer": "30 days: the planner's pick, storage stays bounded", "by": "tower" }));
        }
        let r = plans.request(&a1);
        assert!(r.ok, "{:?}", r.error);
        // The user answers q2 on its card (Mac or phone): written into the plan, the card closed.
        let asks = crate::daemon::asks::Asks::new(shared.clone(), Arc::new(crate::daemon::lan::hub::Hub::default()), "normal");
        let send = |_: &str, _: &str| -> Result<Value> { panic!("nothing is sent to a session") };
        let agenda = |_: &crate::agenda::Cmd| crate::protocol::Response { ok: true, error: None, state: None };
        let reopen = |_: &crate::reopen::Pick| -> Result<Value> { panic!("no reopen") };
        let accounts = |_: bool| -> Result<Value> { panic!("no accounts") };
        let planned = |plan: &str, q: &str, a: &str| plans.answer(plan, q, a, "you");
        let carry = Carry { asks: &asks, send: &send, agenda: &agenda, reopen: &reopen, accounts: &accounts, plan: &planned };
        let v = decisions.answer_with(&q2_card, &json!({ "choice": 0 }), true, &carry).unwrap();
        assert_eq!((v["state"].as_str(), v["plan"]["answer"].as_str()), (Some("answered"), Some("German and French")));
        // q3 in the user's own words.
        let q3_card = shared.get().plan.unwrap()["questions"][2]["card"].as_str().unwrap().to_string();
        decisions.answer_with(&q3_card, &json!({ "other": "At release, and I'll add it myself" }), true, &carry).unwrap();
        // q4: no word from the brain in time: the user's card.
        let mut waited = 0;
        while status(&shared.get().plan.unwrap(), 3) != "asked" && waited < 50 {
            std::thread::sleep(Duration::from_millis(50));
            waited += 1;
        }
        let shown = shared.get().plan.unwrap();
        assert_eq!(status(&shown, 3), "asked", "the brain said nothing in time");
        assert_eq!(shown["open_questions"], 1);
        // Launch waits for it.
        assert!(plans::no_open_questions("p1", &shown).unwrap_err().to_string().contains("q4 (Name the toggle"));
        // The brain hands it on instead of answering after all: no second card; then the CLI answers it.
        let mut a4 = ask("answer", "p1");
        if let Request::Plan { body, .. } = &mut a4 {
            *body = Some(json!({ "question": "q4", "answer": "Show in English" }));
        }
        assert!(plans.request(&a4).ok);
        let shown = shared.get().plan.unwrap();
        let answered: Vec<(String, String, String)> = shown["questions"].as_array().unwrap().iter().map(|q| (q["id"].as_str().unwrap().into(), q["by"].as_str().unwrap().into(), q["answer"].as_str().unwrap().into())).collect();
        assert_eq!(
            answered,
            [
                ("q1".to_string(), "tower".to_string(), "30 days: the planner's pick, storage stays bounded".to_string()),
                ("q2".into(), "you".into(), "German and French".into()),
                ("q3".into(), "you".into(), "At release, and I'll add it myself".into()),
                ("q4".into(), "you".into(), "Show in English".into())
            ]
        );
        assert_eq!(decisions.list(None, false)["count"], 0, "every card closed with its answer");
        assert!(plans::no_open_questions("p1", &shown).is_ok());
        // A rewrite of the stages keeps the answers.
        assert!(plans.request(&st).ok);
        assert_eq!(shared.get().plan.unwrap()["open_questions"], 0);

        // Stages: s1 active; done only once its check passed with evidence.
        let stage = |n: usize| shared.get().plan.unwrap()["stages"][n]["state"].as_str().unwrap().to_string();
        assert_eq!((stage(0), stage(1), stage(2), stage(3)), ("active".into(), "waiting".into(), "waiting".into(), "waiting".into()));
        let check = |sid: &str, c: &str, status: &str, ev: &str| {
            let mut r = ask("check", "p1");
            if let Request::Plan { body, .. } = &mut r {
                *body = Some(json!({ "stage": sid, "check": c, "status": status, "evidence": ev }));
            }
            plans.request(&r)
        };
        assert!(check("s1", "c1", "passed", "").error.unwrap().contains("give its evidence"), "no evidence, no pass");
        assert!(check("s1", "c9", "passed", "x").error.unwrap().contains("no planned check"));
        let r = check("s1", "c1", "passed", "p50 310 ms, p95 640 ms over 1,000 messages");
        let v = r.state.unwrap();
        assert_eq!((v["state"].as_str(), v["progress"]["done"].as_u64()), (Some("done"), Some(1)));
        // s2: its pre-check first; then its own check passed, but PR 2's checks haven't: not done.
        assert_eq!(stage(1), "ready");
        check("s2", "p1", "passed", "arn:aws:secretsmanager:eu-central-1:dev/translate");
        assert_eq!(stage(1), "active");
        check("s2", "c1", "passed", "{\"text\":\"Hello\"}");
        assert_eq!(stage(1), "active", "its PRs haven't opened");
        plans
            .change(|b| {
                let p = b.plans.iter_mut().find(|p| p.id == "p1").unwrap();
                (p.prs[0].pr, p.prs[1].pr) = (Some("eyz/api#7".into()), Some("eyz/api#8".into()));
                Ok(p.clone())
            })
            .unwrap();
        assert_eq!(stage(1), "active", "PR #8's check isn't reported yet");
        // A failed check: failing.
        check("s3", "c1", "failed", "the toggle shows the original text");
        assert_eq!(stage(2), "failing");
        let shown = shared.get().plan.unwrap();
        assert_eq!((shown["progress"]["stages"].as_u64(), shown["progress"]["done"].as_u64(), shown["progress"]["at"].as_u64()), (Some(4), Some(1), Some(2)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_plan_is_revised_from_findings_small_changes_by_themselves_big_ones_on_the_users_word() {
        use crate::daemon::decisions::{Carry, Decisions};
        let dir = std::env::temp_dir().join(format!("tower-plans-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = Shared::new(State::new("off", "test", "test"));
        let decisions = Decisions::new(shared.clone());
        let notes: Arc<Mutex<Vec<String>>> = Arc::default();
        let heard = notes.clone();
        let brain: BrainNote = Arc::new(move |t: &str| heard.lock().unwrap().push(t.to_string()));
        let plans = Plans::at(dir.join("plans.json"), shared.clone(), SendQueue::open()).wired(decisions.clone(), Arc::new(|_: &str| None), brain).brain_within(60_000);
        translation_plan(&plans, &dir);
        let mut st = ask("stages", "p1");
        if let Request::Plan { body, .. } = &mut st {
            let mut v = structure_of();
            v["questions"] = json!([]);
            *body = Some(v);
        }
        assert!(plans.request(&st).ok);
        // Launched: the plan stays, live, as its space's.
        let mut close = ask("close", "p1");
        if let Request::Plan { launched, .. } = &mut close {
            *launched = Some("msg-translation".into());
        }
        assert!(plans.request(&close).ok);
        let state = shared.get();
        assert!(state.plan.is_none(), "no plan being written");
        assert_eq!((state.plans_live.len(), state.plans_live[0]["status"].as_str(), state.plans_live[0]["version"].as_u64()), (1, Some("live"), Some(1)));

        // A session finds an assumption wrong: the assumption is broken, the finding kept and told.
        let mut wrong = ask("assume", "p1");
        if let Request::Plan { body, .. } = &mut wrong {
            *body = Some(json!({ "assumption": "a1", "status": "broken", "evidence": "1,204 messages have no lang: imported tickets", "session": "t5" }));
        }
        let r = plans.request(&wrong);
        assert!(r.ok, "{:?}", r.error);
        let v = r.state.unwrap();
        assert_eq!((v["assumption"]["status"].as_str(), v["finding"]["id"].as_str(), v["finding"]["kind"].as_str(), v["told"].as_str()), (Some("broken"), Some("f1"), Some("assumption-wrong"), Some("the brain")));
        assert!(notes.lock().unwrap().iter().any(|n| n.starts_with("[plan-finding] p1 f1 ")));
        // And a PR that has to split.
        let mut split = ask("finding", "p1");
        if let Request::Plan { body, .. } = &mut split {
            *body = Some(json!({ "kind": "split-pr", "text": "The endpoint and the language backfill are two PRs", "stage": "s2" }));
        }
        assert_eq!(plans.request(&split).state.unwrap()["finding"]["id"], "f2");

        // A small revision: s2 gets a check for the backfill. Applied by itself, version 2, f2 taken in.
        let revise = |v: Value, why: &str, findings: &[&str]| {
            let mut r = ask("revise", "p1");
            if let Request::Plan { body, text, .. } = &mut r {
                *body = Some(json!({ "structure": v, "findings": findings }));
                *text = why.into();
            }
            plans.request(&r)
        };
        let mut v = structure_of();
        v["questions"] = json!([]);
        v["stages"][1]["post"].as_array_mut().unwrap().push(json!({ "id": "c2", "what": "Every message has a language", "how": "select count(*) from messages where lang is null", "evidence": "0" }));
        let r = revise(v.clone(), "The backfill gets its own check", &["f2"]);
        assert!(r.ok, "{:?}", r.error);
        let out = r.state.unwrap();
        assert_eq!((out["revision"]["size"].as_str(), out["revision"]["status"].as_str(), out["version"].as_u64()), (Some("small"), Some("applied"), Some(2)));
        assert_eq!(out["revision"]["changes"][0], "Stage s2 gets a post check c2: Every message has a language");
        let live = shared.get().plans_live[0].clone();
        assert_eq!((live["findings"][1]["revision"].as_u64(), live["stages"][1]["post"].as_array().map(Vec::len)), (Some(1), Some(2)));
        assert!(revise(v.clone(), "again", &[]).error.unwrap().contains("nothing changes"));

        // A big one: s2's done-when grows with the backfill (f1). A card with the diff; nothing applied yet.
        let mut big = v.clone();
        big["stages"][1]["done_when"] = json!("The endpoint answers on dev and every message has a language");
        let out = revise(big.clone(), "Imported tickets have no language: backfill them first", &["f1"]).state.unwrap();
        assert_eq!((out["revision"]["size"].as_str(), out["revision"]["status"].as_str(), out["version"].as_u64()), (Some("big"), Some("proposed"), Some(2)));
        let card = out["card"].as_str().unwrap().to_string();
        let d = decisions.get(&card).unwrap();
        assert_eq!(d.title, "Change the plan: Imported tickets have no language: backfill them first?");
        assert!(d.detail.contains("version 2 → 3") && d.detail.contains("Stage s2 is done when: The endpoint answers on dev and every message has a language") && d.detail.contains("From finding f1"), "{}", d.detail);
        assert!(revise(v.clone(), "another", &[]).error.unwrap().contains("one at a time"));
        assert_eq!(shared.get().plans_live[0]["stages"][1]["done_when"], "POST /messages/:id/translate returns and caches a translation on dev");
        // The user applies it on the card: version 3.
        let asks = crate::daemon::asks::Asks::new(shared.clone(), Arc::new(crate::daemon::lan::hub::Hub::default()), "normal");
        let send = |_: &str, _: &str| -> Result<Value> { panic!("nothing is sent to a session") };
        let agenda = |_: &crate::agenda::Cmd| crate::protocol::Response { ok: true, error: None, state: None };
        let reopen = |_: &crate::reopen::Pick| -> Result<Value> { panic!("no reopen") };
        let accounts = |_: bool| -> Result<Value> { panic!("no accounts") };
        let planned = |plan: &str, what: &str, a: &str| plans.from_card(plan, what, a);
        let carry = Carry { asks: &asks, send: &send, agenda: &agenda, reopen: &reopen, accounts: &accounts, plan: &planned };
        let v3 = decisions.answer_with(&card, &json!({ "choice": 0 }), true, &carry).unwrap();
        assert_eq!((v3["plan"]["applied"].as_bool(), v3["plan"]["version"].as_u64()), (Some(true), Some(3)));
        let live = shared.get().plans_live[0].clone();
        assert_eq!((live["version"].as_u64(), live["stages"][1]["done_when"].as_str(), live["revisions"][1]["status"].as_str()), (Some(3), Some("The endpoint answers on dev and every message has a language"), Some("applied")));
        assert_eq!(live["findings"][0]["revision"], 2);
        // Another big one (dropping s4's check), kept out by the user: the plan stays at version 3.
        let mut drop = big.clone();
        drop["stages"][3]["post"][0]["id"] = json!("c9");
        let out = revise(drop, "Release check by hand", &[]).state.unwrap();
        let card = out["card"].as_str().unwrap().to_string();
        let kept = decisions.answer_with(&card, &json!({ "choice": 1 }), true, &carry).unwrap();
        assert_eq!((kept["plan"]["applied"].as_bool(), kept["plan"]["version"].as_u64()), (Some(false), Some(3)));
        assert_eq!(shared.get().plans_live[0]["revisions"][2]["status"], "rejected");
        assert_eq!(decisions.list(None, false)["count"], 0);
        // A new plan being written leaves the live one live.
        plans.change(|b| Ok(b.add(Form::Page, "Another", draft(), |id| dir.join(id), 9).0)).unwrap();
        assert_eq!((shared.get().plans_live.len(), shared.get().plan.unwrap()["id"].as_str()), (1, Some("p2")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Starts the planner of `p` in a hidden Herdr terminal: its folder written
/// first. Returns its address.
fn run_planner(p: &Plan) -> Result<String> {
    let cfg = config::Config::live();
    let tower = config::tower_bin().display().to_string();
    plans::write_folder(p, &tower)?;
    let account = crate::accounts::default_of(&crate::accounts::list().unwrap_or_default());
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
