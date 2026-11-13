//! The agenda ledger in the daemon (see `crate::agenda`): held in memory,
//! written to `agendas.json` after every change, and moved by a reconciler
//! that looks every 20 seconds (and at once when a hook fires or a task
//! registers) at what the send queue, Herdr, the agents' hooks and the
//! question cards say about each open task.
//!
//! What it has to say goes to the brain through the radar's outbox, like a
//! watch's report: it waits for a brain that's away (said out loud once
//! meanwhile), and a space out of focus marks it, as work the user asked for.
//!
//! A task that failed: at a quiet level (`tell`) the tower sends it once
//! more by itself (unless `[agenda] retry_failed = false` in config.toml);
//! one that failed again after a retry is the user's to decide
//! (`decisions`, kind `failed`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::agenda::{self, Action, Facts, FailedTask, Heard, HookFact, Ledger, Live, Probe, Reg, TaskEv};
use crate::hc;
use crate::protocol::Response;

/// What the reconciler reads and acts through (set once at the daemon's start).
#[derive(Clone)]
pub struct Ctx {
    pub sendq: super::sendq::SendQueue,
    pub watches: super::watch::Watches,
    pub asks: super::asks::Asks,
    pub shared: super::state::Shared,
    pub decisions: super::decisions::Decisions,
}

#[derive(Clone)]
pub struct Agendas {
    ledger: Arc<Mutex<Ledger>>,
    path: PathBuf,
    wake: Arc<(Mutex<bool>, Condvar)>,
    ctx: Arc<OnceLock<Ctx>>,
}

static RUNNING: OnceLock<Agendas> = OnceLock::new();

/// What is still open, for the brain's `[radar]` line (None: nothing, or no daemon here).
pub fn radar_note() -> Option<String> {
    RUNNING.get()?.lock().radar_note()
}

/// The user answered `session`'s question on a card with words of their own
/// (`questions::users_words`), and the tower typed them in: like what they
/// type in its terminal, their request there (an agenda of origin
/// `terminal`, and in the record), unless it only answers ("yes", "2").
pub fn answered_on_card(session: &str, words: &str) {
    let Some(words) = crate::introduce::users_task(words) else { return };
    let (session, at_ms) = (session.to_string(), now_ms());
    std::thread::spawn(move || {
        let Some(agendas) = RUNNING.get() else { return };
        let t = crate::hc::get(&session);
        let (name, space) = t.as_ref().map(|t| (crate::hc::spoken_name(t), crate::spaces::space_of(t))).unwrap_or_default();
        let r = agendas.typed(agenda::Typed {
            session: session.clone(),
            name: name.clone(),
            created_ms: t.as_ref().and_then(crate::hc::created_ms),
            space: space.clone(),
            words: words.clone(),
            at_ms,
        });
        crate::history::append(crate::history::Entry {
            kind: "request".into(),
            text: words,
            source: Some(format!("answer:{session}")),
            session: Some(session),
            name: Some(name).filter(|n| !n.is_empty()),
            space,
            agenda: Some(r.agenda),
            record: true,
            ..Default::default()
        });
    });
}

/// What the user has `session` on: the instruction of its unfinished task,
/// unless that is a message from another session (`Task::is_message`).
pub fn users_task_of(session: &str) -> Option<String> {
    let l = RUNNING.get()?.lock();
    l.unfinished_users_on(session).map(|t| t.instruction.clone())
}

/// What the user last asked `session` for, done or not: the newest task on
/// it that is no message from another session and passes `is_request`.
pub fn users_last_task_of(session: &str, is_request: impl Fn(&str) -> bool) -> Option<String> {
    let l = RUNNING.get()?.lock();
    l.latest_users_on(session, is_request).map(|t| t.instruction.clone())
}

/// What was asked of `session` lately: the instruction of the task it is
/// on, or was until a minute ago (None: nothing, or no daemon here).
pub fn working_on(session: &str) -> Option<String> {
    let l = RUNNING.get()?.lock();
    l.at_work_on(session, now_ms(), 60_000).map(|t| t.instruction.clone())
}

fn now_ms() -> u64 {
    crate::history::now_ms()
}

impl Agendas {
    pub fn open() -> Agendas {
        Self::at(agenda::path())
    }

    pub fn at(path: PathBuf) -> Agendas {
        Agendas {
            ledger: Arc::new(Mutex::new(Ledger::load_from(&path))),
            path,
            wake: Arc::new((Mutex::new(false), Condvar::new())),
            ctx: Arc::new(OnceLock::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, l: &Ledger) {
        if let Err(e) = l.save_to(&self.path) {
            eprintln!("[agenda] {e:#}");
        }
    }

    /// The ledger as it stands (for the missions: each space's requests).
    pub fn ledger(&self) -> Ledger {
        self.lock().clone()
    }

    /// The reconciler, from now on.
    pub fn start(&self, ctx: Ctx) {
        let _ = self.ctx.set(ctx);
        let _ = RUNNING.set(self.clone());
        let me = self.clone();
        thread::Builder::new()
            .name("agendas".into())
            .spawn(move || {
                // The radar's and the queue's own look at what the last run left comes first.
                thread::sleep(Duration::from_secs(6));
                {
                    let l = me.lock();
                    let open = l.open_agendas();
                    if !open.is_empty() {
                        let tasks: usize = open.iter().map(|a| a.unfinished().len()).sum();
                        eprintln!("[agenda] {} open agenda(s), {tasks} unfinished task(s), from before", open.len());
                    }
                }
                let mut round: u64 = 0;
                loop {
                    me.tick(round);
                    round += 1;
                    let (m, cv) = &*me.wake;
                    let woken = m.lock().unwrap_or_else(|e| e.into_inner());
                    let mut woken =
                        cv.wait_timeout_while(woken, Duration::from_secs(20), |w| !*w).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
                    *woken = false;
                }
            })
            .ok();
    }

    /// Something happened (a hook, a registration): look now.
    pub fn kick(&self) {
        let (m, cv) = &*self.wake;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    /// The user's latest request, as the daemon heard it.
    fn heard(&self) -> Heard {
        match self.ctx.get() {
            Some(c) => c.shared.with(|s| Heard { text: s.heard.clone(), at_ms: s.heard_ms }),
            None => Heard::default(),
        }
    }

    /// A task for what the send queue took on (`tower queue`, `dispatch`, `start`).
    pub fn register(&self, reg: Reg) -> Option<agenda::Registered> {
        if reg.queue_id == 0 || reg.text.trim().is_empty() {
            return None;
        }
        // The real tower and a test one never follow each other's sessions.
        let session = (!reg.session.is_empty()).then(|| hc::get(&reg.session)).flatten();
        if session.as_ref().is_some_and(super::watch::left_alone) {
            return None;
        }
        // What goes to a brain (the main one, a space's own) is no task of anyone's.
        if reg.session == crate::brain::name() || super::brains::is_brain_address(&reg.session) {
            return None;
        }
        let heard = self.heard();
        let (r, request) = {
            let mut l = self.lock();
            let r = l.register(&reg, &heard, now_ms())?;
            self.save(&l);
            // A request of the user's handed out into a space: it may add to what that space is for.
            let request = l.request_in_space(&r, session.as_ref().and_then(crate::spaces::space_of).as_deref());
            (r, request)
        };
        if let Some((space, words, at_ms)) = request {
            super::goals::heard(&space, &words, at_ms, "a dispatch into it");
        }
        eprintln!("[agenda] #{} is {} in {} ({})", reg.queue_id, r.task, r.agenda, r.how);
        self.kick();
        Some(r)
    }

    /// The user typed a request in a session's own terminal: an agenda of
    /// its own (origin `terminal`), or more of the one typed there before.
    /// Kept, never told to a brain.
    pub fn typed(&self, t: agenda::Typed) -> agenda::Registered {
        let r = {
            let mut l = self.lock();
            let r = l.typed(&t, now_ms());
            self.save(&l);
            r
        };
        eprintln!("[agenda] {}: typed in its terminal: {} in {} ({})", t.session, r.task, r.agenda, r.how);
        self.kick();
        r
    }

    /// One round: facts for every open task, moves, re-prompts, news.
    fn tick(&self, round: u64) {
        let Some(ctx) = self.ctx.get() else { return };
        let now = now_ms();
        let (probes, wants_jobs) = {
            let l = self.lock();
            (l.probes(now), l.wants_jobs(now, 15 * 60_000))
        };
        // The history only now and then (it can be large), while a new task has no job yet.
        let jobs: Vec<(String, String, u64)> = if wants_jobs && round.is_multiple_of(3) {
            crate::history::jobs(&crate::history::load()).into_iter().filter(|j| now.saturating_sub(j.started) < 3_600_000).map(|j| (j.session, j.id, j.started)).collect()
        } else {
            vec![]
        };
        let facts = if probes.is_empty() { BTreeMap::new() } else { gather(ctx, &probes) };
        let quiet = crate::tell::level().quiet();
        // `[agenda] retry_failed`, read each round: switching it needs no restart.
        let retry = retries(quiet, &crate::config::Config::live().agenda);
        let (actions, failures, notes) = {
            let mut l = self.lock();
            let before = l.clone();
            let actions = l.step(&facts, now);
            l.attach_jobs(&jobs);
            // Before the news: a failure the tower handles itself isn't told as one.
            let failures = l.failures(retry, now);
            let notes = l.news(now);
            let pruned = l.prune(now);
            if pruned > 0 {
                eprintln!("[agenda] {pruned} settled agenda(s) older than a month dropped");
            }
            if *l != before {
                self.save(&l);
            }
            (actions, failures, notes)
        };
        for a in actions {
            self.reprompt(ctx, a);
        }
        for f in failures {
            self.failed(ctx, f, quiet);
        }
        for n in notes {
            eprintln!("[agenda] telling the brain: {}", agenda::clip(&n.msg, 200));
            ctx.watches.tell_agenda(n);
        }
        // A failure's decision is over once its task is sent again, verified or dropped.
        let still = self.lock().failed_twice();
        ctx.decisions.sync_failed(&still);
    }

    /// A task failed. The first time, the tower sends it once more by itself
    /// (a quiet level only); again after a retry, it is the user's to decide.
    fn failed(&self, ctx: &Ctx, f: FailedTask, quiet: bool) {
        let now = now_ms();
        match f {
            FailedTask::Retry { task, session, text, why } => {
                // Settled meanwhile (the brain said it's done, or dropped it): nothing to send.
                if self.lock().task(&task).is_none_or(|t| t.state != agenda::TaskState::Failed) {
                    return;
                }
                // Its address has another terminal now: the tower sends nothing of this task in there by itself.
                let given = self.lock().task(&task).and_then(|t| t.created_ms);
                let there = match look(&session) {
                    Live::Here { created_ms, .. } => created_ms,
                    _ => None,
                };
                if let Some(stranger) = agenda::another_terminal(&session, given, there) {
                    eprintln!("[agenda] {task} failed, and isn't sent again: {stranger}");
                    let mut l = self.lock();
                    l.retry_unsent(&task, &stranger);
                    self.save(&l);
                    return;
                }
                // Other words than the first time: the queue takes the same instruction again for a retry of its own.
                let again = format!("That didn't work ({}). Try it once more: {text}", agenda::clip(why.trim_end_matches('.'), 200));
                match ctx.sendq.add(&session, &again, &[]) {
                    Ok((added, address, name)) => {
                        let id = match added {
                            super::sendq::Added::Sent { id } | super::sendq::Added::Queued { id, .. } => id,
                        };
                        let note = {
                            let mut l = self.lock();
                            l.requeue(&task, id, &again, now);
                            self.save(&l);
                            let t = l.task(&task).cloned().unwrap_or_default();
                            agenda::Note {
                                session: address.clone(),
                                name: name.clone(),
                                space: t.space.clone(),
                                msg: format!(
                                    "[agenda] {task} \"{}\" on {address} ({name}) failed ({}), so the tower sent it once more by itself (#{id}). Nothing to do: if it fails again, the tower puts it to the user as a decision.",
                                    t.title,
                                    agenda::clip(&why, 200)
                                ),
                                speech: String::new(),
                            }
                        };
                        eprintln!("[agenda] {task} failed: sent once more to {address} as #{id}");
                        ctx.watches.tell_agenda(note);
                    }
                    Err(e) => {
                        eprintln!("[agenda] {task} failed, and couldn't be sent again: {e:#}");
                        let mut l = self.lock();
                        l.retry_unsent(&task, &format!("{e:#}"));
                        self.save(&l);
                        drop(l);
                        self.kick();
                    }
                }
            }
            FailedTask::Decide { task, title, session, name, space, why } => {
                // A session that has ended is still what it is about, but nobody waits there.
                let live = !session.is_empty() && hc::get(&session).is_some_and(|t| t["state"] == "live");
                let who = live.then(|| crate::decisions::Who::new(&session, &name));
                let (raised, d, told) =
                    ctx.decisions.failed(crate::decisions::Failure { task: task.clone(), title: title.clone(), space: space.clone(), who, why: why.clone() });
                {
                    let mut l = self.lock();
                    l.decided(&task, quiet, now);
                    self.save(&l);
                }
                eprintln!("[agenda] {task} failed again after a retry: the user's to decide ({})", raised.id);
                if told {
                    let line = crate::decisions::said(&name, &format!("{}. Try it again, or drop it?", d.title), None);
                    ctx.watches.raise(&session, &line);
                }
                if quiet {
                    ctx.watches.tell_agenda(agenda::Note {
                        session: session.clone(),
                        name: name.clone(),
                        space,
                        msg: format!(
                            "[agenda] {task} \"{title}\" on {session} ({name}) failed again after a retry: {}. The tower has put it to the user as a decision ({}): try it again, or drop it. Don't raise it again, and don't requeue it yourself.",
                            agenda::clip(&why, 300),
                            raised.id
                        ),
                        speech: String::new(),
                    });
                }
            }
        }
    }

    /// The session never took its instruction: it goes in once more, through the queue.
    fn reprompt(&self, ctx: &Ctx, a: Action) {
        let Action::Reprompt { task, session, text } = a;
        let now = now_ms();
        match ctx.sendq.add(&session, &text, &[]) {
            Ok((added, address, name)) => {
                let id = match added {
                    super::sendq::Added::Sent { id } | super::sendq::Added::Queued { id, .. } => id,
                };
                let note = {
                    let mut l = self.lock();
                    l.reprompted(&task, id, now);
                    self.save(&l);
                    let t = l.task(&task).cloned().unwrap_or_default();
                    agenda::Note {
                        session: address.clone(),
                        name: name.clone(),
                        space: t.space.clone(),
                        msg: format!(
                            "[agenda] {task} \"{}\": {address} ({name}) hadn't taken its instruction {} min after it went in, so the tower sent it again (#{id}). Nothing to tell the user unless it stalls.",
                            t.title,
                            agenda::REPROMPT_MS / 60_000
                        ),
                        speech: String::new(),
                    }
                };
                eprintln!("[agenda] {task}: sent again to {address} as #{id}");
                ctx.watches.tell_agenda(note);
            }
            Err(e) => {
                let mut l = self.lock();
                let _ = l.apply(&task, TaskEv::Stall { why: format!("it never took the instruction, and sending it again failed: {e:#}") }, now);
                // Told once by the next round's news, so it isn't sent again.
                if let Some(t) = l.agendas.iter_mut().flat_map(|a| a.tasks.iter_mut()).find(|t| t.id == task) {
                    t.reprompts += 1;
                }
                self.save(&l);
            }
        }
    }

    /// `tower agenda …` (and `tower job done|failed`, as `judge`).
    pub fn request(&self, c: &agenda::Cmd) -> Response {
        let ok = |v: serde_json::Value| Response { ok: true, error: None, state: Some(v) };
        let now = now_ms();
        if c.action == "requeue" {
            return match self.requeue(c, now) {
                Ok(v) => ok(v),
                Err(e) => Response::err(format!("{e:#}")),
            };
        }
        let heard = self.heard();
        let acted = {
            let mut l = self.lock();
            match agenda::act(&mut l, c, &heard, now) {
                Ok(a) => {
                    if a.changed {
                        self.save(&l);
                    }
                    a
                }
                Err(e) => return Response::err(format!("{e:#}")),
            }
        };
        // Dropped work leaves the queue too, so it doesn't go in after all.
        let mut value = acted.value;
        if let Some(ctx) = self.ctx.get() {
            let mut left = vec![];
            for id in &acted.cancel {
                if let Err(e) = ctx.sendq.cancel(*id) {
                    left.push(format!("#{id}: {e:#}"));
                }
            }
            if !left.is_empty() {
                value["not_cancelled"] = json!(left);
            }
        }
        if acted.changed {
            self.kick();
        }
        ok(value)
    }

    /// `tower agenda requeue <task> [<session>]`: its instruction (or its latest follow-up) again.
    fn requeue(&self, c: &agenda::Cmd, now: u64) -> anyhow::Result<serde_json::Value> {
        let Some(ctx) = self.ctx.get() else { anyhow::bail!("the reconciler isn't running yet") };
        let (task, session, text) = {
            let l = self.lock();
            let agenda::Target::Task(id) = l.resolve(&c.target)? else {
                anyhow::bail!("requeue takes a task (a3.2), not an agenda: `tower agenda show {}` lists its tasks", c.target)
            };
            let t = l.task(&id).cloned().unwrap_or_default();
            let session = if c.session.trim().is_empty() { t.session.clone() } else { c.session.trim().to_string() };
            if session.is_empty() {
                anyhow::bail!("{id} never had a session: give one (`tower agenda requeue {id} <session>`)");
            }
            let text = if c.text.trim().is_empty() { t.followups.last().cloned().unwrap_or(t.instruction.clone()) } else { c.text.trim().to_string() };
            // No session named: the one it had, and only while that address is still that terminal.
            if c.session.trim().is_empty() {
                let there = match look(&session) {
                    Live::Here { created_ms, .. } => created_ms,
                    _ => None,
                };
                if let Some(stranger) = agenda::another_terminal(&session, t.created_ms, there) {
                    anyhow::bail!("{id} isn't sent again: {stranger}. Name the session it should go to (`tower agenda requeue {id} <session>`), or drop it");
                }
            }
            (id, session, text)
        };
        let (added, address, name) = ctx.sendq.add(&session, &text, &[])?;
        let (id, sent) = match added {
            super::sendq::Added::Sent { id } => (id, true),
            super::sendq::Added::Queued { id, .. } => (id, false),
        };
        {
            let mut l = self.lock();
            l.requeue(&task, id, if c.text.trim().is_empty() { "" } else { &text }, now);
            self.save(&l);
        }
        self.kick();
        Ok(json!({ "task": task, "queue_id": id, "session": address, "name": name, "sent": sent }))
    }
}

/// Whether a task that just failed is sent to its session once more by the
/// tower itself: at a quiet level (`tell`), unless `[agenda] retry_failed`
/// is off. Then nothing is sent again unasked: the brain hears of the
/// failure in the ledger's note and raises it, or requeues it itself.
fn retries(quiet: bool, cfg: &crate::config::Agenda) -> bool {
    quiet && cfg.retry_failed
}

/// What Herdr says about a session now.
fn look(session: &str) -> Live {
    match hc::lookup(session) {
        hc::Lookup::Found(t) if t["state"] == "live" => Live::Here {
            status: hc::status(&t),
            created_ms: hc::created_ms(&t),
            turn: hc::turn(&t),
            name: hc::spoken_name(&t),
            space: crate::spaces::space_of(&t),
        },
        hc::Lookup::Found(_) => Live::Exited,
        hc::Lookup::Gone => Live::Gone,
        hc::Lookup::Unreachable(_) => Live::Unreachable,
    }
}

/// The facts for each probed task: one look per session, outside the ledger's lock.
fn gather(ctx: &Ctx, probes: &[Probe]) -> BTreeMap<String, Facts> {
    let mut lives: BTreeMap<String, Live> = BTreeMap::new();
    let questions = ctx.asks.list();
    let mut out = BTreeMap::new();
    for p in probes {
        let queue = p.queue_id.map(|id| ctx.sendq.fact(id)).unwrap_or_default();
        let session = match &queue {
            agenda::QueueFact::Delivered { session, .. } => session.clone(),
            agenda::QueueFact::Waiting { session, .. } if !session.is_empty() => session.clone(),
            _ => p.session.clone(),
        };
        if session.is_empty() {
            out.insert(p.task.clone(), Facts { queue, ..Default::default() });
            continue;
        }
        let live = lives.entry(session.clone()).or_insert_with(|| look(&session)).clone();
        let hook = super::sessions::registry().get(&session).map(|s| HookFact {
            state: s.state,
            event: s.event,
            event_ms: s.event_ms,
            prompt_ms: s.prompt_ms,
            said: s.said,
            said_ms: s.said_ms,
            notice: s.notice,
            started_ms: s.started_ms,
        });
        let question = questions.iter().find(|q| q.session == session).map(|q| {
            if !q.title.trim().is_empty() { q.title.clone() } else { q.tabs.first().map(|t| t.question.clone()).unwrap_or_default() }
        });
        let working = matches!(&live, Live::Here { status: hc::SessionStatus::Working, .. });
        let screen = (p.look && working)
            .then(|| hc::run(&["read", &session, "--lines", "30"]).ok().filter(|o| o.code == 0).map(|o| agenda::screen_hash(&o.stdout)))
            .flatten();
        out.insert(p.task.clone(), Facts { queue, live: Some(live), hook, question, screen });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::retries;
    use crate::agenda::{FailedTask, Ledger, Reg, Tag, TaskEv};

    /// `[agenda] retry_failed = false`: a failed task is not sent again by
    /// the tower; the brain hears of it from the ledger's note instead.
    #[test]
    fn the_retry_of_a_failed_task_can_be_turned_off() {
        let on = crate::config::Config::default();
        assert!(on.agenda.retry_failed, "on unless the user says otherwise");
        let off: crate::config::Config = toml::from_str("[agenda]\nretry_failed = false\n").unwrap();
        assert!(!off.agenda.retry_failed);
        assert!(retries(true, &on.agenda) && !retries(false, &on.agenda), "never while the tower tells everything");
        assert!(!retries(true, &off.agenda) && !retries(false, &off.agenda));
        // A task that just failed.
        let now = 1_000_000;
        let mut l = Ledger::default();
        let a = l.open("fix the login", "", "user", 0, &[], now);
        let reg = Reg { queue_id: 1, session: "t5".into(), name: "api".into(), text: "fix the login".into(), tag: Tag { agenda: Some(a), ..Default::default() } };
        let task = l.register(&reg, &Default::default(), now).unwrap().task;
        l.apply(&task, TaskEv::Fail { why: "the tests fail".into() }, now).unwrap();
        // On: the tower sends it once more.
        assert!(matches!(l.failures(retries(true, &on.agenda), now + 1_000).as_slice(), [FailedTask::Retry { task: t, .. }] if *t == task));
        // Off: nothing is sent again, and it is no decision either (it wasn't retried)...
        assert!(l.failures(retries(true, &off.agenda), now + 1_000).is_empty());
        // ... so the ledger's note tells the brain, once its grace has passed.
        let notes = l.news(now + crate::agenda::GRACE_TROUBLE_MS);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].msg.starts_with("[agenda] ") && notes[0].msg.contains("failed"), "{}", notes[0].msg);
        // Tried again by hand and failed again: the user's to decide, as always.
        l.requeue(&task, 2, "", now + 60_000);
        l.apply(&task, TaskEv::Fail { why: "still failing".into() }, now + 120_000).unwrap();
        assert!(matches!(l.failures(retries(true, &off.agenda), now + 121_000).as_slice(), [FailedTask::Decide { task: t, .. }] if *t == task));
    }
}
