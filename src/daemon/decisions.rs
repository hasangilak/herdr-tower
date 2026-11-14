//! The decisions in the daemon (see `crate::decisions`): held in memory,
//! written to `decisions.json` after every change, and published in the
//! state (`decisions`, in queue order, and `decisions_count` for the orb).
//!
//! Its sources feed it: the question sets that are the user's (`asks`), the
//! PRs that wait for a merge (the PR store's changes), `tower raise`, and
//! the agenda ledger's tasks that failed twice. The radar says when a
//! session works again, or is gone.
//!
//! Who is told: a decision is said once, when it first appears (`tells`).
//! The same key again says nothing at a quiet level (`tell`); while the
//! tower tells everything, the alerts it always made stay. A muted space's
//! decisions (`tower space mute`) are out of the main queue and the count,
//! and are not said.
//!
//! On hold (`decisions::Hold`: `tower hold`, the card's Hold, the phone):
//! out of the queue and the count (the state has them apart, in
//! `decisions_held`), and never said. One that comes back (updated, what
//! it waited for done, let go) is in the queue again and said once
//! (`came_back`). A PR that merges in a held decision's space is read
//! against the user's words once (`weigh`): the tower's model says
//! whether it is what they waited for.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use super::asks::Asks;

use super::state::{Shared, lock};
use crate::decisions::{self, Book, Changes, Decision, Failure, Raise, Raised, State};
use crate::hc;
use crate::prs::Pr;
use crate::questions::QuestionSet;
use crate::tell::Level;

/// How a decision back from hold is said (`Decisions::on_back`).
type Say = Box<dyn Fn(&Decision, &str) + Send + Sync>;

#[derive(Clone)]
pub struct Decisions {
    book: Arc<Mutex<Book>>,
    /// `decisions.json`, when they are kept (the daemon's; tests keep none unless they say where).
    path: Option<Arc<PathBuf>>,
    shared: Shared,
    /// The muted spaces now (`spaces.json`).
    muted: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    /// How much the tower tells the user now.
    level: fn() -> Level,
    /// Says a decision that came back from hold (the daemon's: as a raise
    /// is said, by its first session). Unset: written down only.
    back: Arc<std::sync::OnceLock<Say>>,
    /// Whether a PR that merged is what a hold's words wait for (the
    /// daemon's: one model call, see `judge_merged`); none in tests.
    judge: Option<fn(&str, &Pr) -> Option<bool>>,
}

fn in_muted(d: &Decision, muted: &[String]) -> bool {
    muted.iter().any(|m| d.in_space(m))
}

impl Decisions {
    /// Decisions kept nowhere, with no space muted.
    pub fn new(shared: Shared) -> Decisions {
        Decisions { book: Arc::default(), path: None, shared, muted: Arc::new(Vec::new), level: crate::tell::level, back: Arc::default(), judge: None }
    }

    /// The daemon's: `decisions.json` in the state folder, as the last run left it.
    pub fn open(shared: Shared) -> Decisions {
        let mut d = Decisions::at(shared, Self::path());
        d.muted = Arc::new(|| crate::spaces::load().mute.into_iter().collect());
        d.judge = Some(judge_merged);
        d.refresh();
        d
    }

    pub fn at(shared: Shared, path: PathBuf) -> Decisions {
        let book = Book::load(&path);
        if !book.open.is_empty() {
            eprintln!("[decisions] {} open from before", book.open.len());
        }
        let d = Decisions { book: Arc::new(Mutex::new(book)), path: Some(Arc::new(path)), ..Decisions::new(shared) };
        d.refresh();
        d
    }

    /// Publishes the queue as it stands now: its order moves with time, and
    /// a space may be muted or unmuted meanwhile.
    pub fn refresh(&self) {
        self.change(|_, _| ());
    }

    pub fn path() -> PathBuf {
        crate::config::state_dir().join("decisions.json")
    }

    fn save(&self, book: &Book) {
        if let Some(path) = &self.path
            && let Err(e) = book.save(path)
        {
            eprintln!("[decisions] can't keep them in {}: {e:#}", path.display());
        }
    }

    /// The open decisions in queue order as the state carries them (each
    /// says whether its space is muted), how many are in the main queue,
    /// and those on hold apart (newest hold first).
    fn views(&self, book: &Book, now: u64) -> (Vec<Value>, usize, Vec<Value>) {
        let muted = (self.muted)();
        let list: Vec<Value> = book.ordered(now).iter().filter(|d| !d.held()).map(|d| d.view(in_muted(d, &muted))).collect();
        let count = list.iter().filter(|v| v["muted"] != true).count();
        (list, count, held_views(book, &muted))
    }

    /// Changes the book, keeps it, and publishes the queue as it stands
    /// now. The book is held until the state has it, so two changes at once
    /// never leave the older queue in the state.
    fn change<R>(&self, f: impl FnOnce(&mut Book, u64) -> R) -> R {
        let now = crate::history::now_ms();
        let (r, back) = {
            let mut b = lock(&self.book);
            let before = b.clone();
            let r = f(&mut b, now);
            // On hold and updated since: back in the queue.
            b.wake(now);
            let back = std::mem::take(&mut b.came_back);
            if *b != before {
                self.save(&b);
            }
            let (list, count, held) = self.views(&b, now);
            self.shared.update(|s| {
                s.decisions = list;
                s.decisions_count = count;
                s.decisions_held = held;
            });
            // One opened, closed, held or back: each mission counts what its
            // space needs (`needs`), so the missions are looked at now, not
            // at their next round.
            let open = |b: &Book| b.open.iter().filter(|d| !d.held()).map(|d| (d.id.clone(), d.space.clone())).collect::<Vec<_>>();
            if open(&b) != open(&before) {
                super::missions::kick();
            }
            (r, back)
        };
        for (id, why) in back {
            self.came_back(&id, &why);
        }
        r
    }

    /// A decision on hold came back (`why`): written down, and said once,
    /// as a new one is (`tells`: not in a muted space).
    fn came_back(&self, id: &str, why: &str) {
        let Some(d) = self.get(id) else { return };
        eprintln!("[decisions] {id} is back from hold ({why}): {}", d.title);
        record_hold(&d, why);
        if self.tells(std::slice::from_ref(&d.id), true) {
            let line = format!("Back from hold: {}. {}.", d.title.trim_end_matches('.'), capital(why.trim_end_matches('.')));
            if let Some(say) = self.back.get() {
                say(&d, &line);
            }
        }
    }

    /// A session (`address`, `name`, in `space`) ended its turn with
    /// `message`: a merge on hold there whose PR it names comes back (the
    /// session posted something new about it).
    pub fn posted(&self, space: Option<&str>, address: &str, name: &str, message: &str) {
        let mut named = decisions::pr_numbers(message);
        named.extend(crate::prs::links(message).into_iter().map(|(_, n)| n));
        if named.is_empty() {
            return;
        }
        let ids: Vec<String> = lock(&self.book)
            .open
            .iter()
            .filter(|d| d.held() && d.kind == decisions::Kind::Merge)
            .filter(|d| d.sessions.iter().any(|w| w.address == address) || space.is_some_and(|s| d.in_space(s)))
            .filter(|d| d.prs.iter().any(|p| named.contains(&p.number)))
            .map(|d| d.id.clone())
            .collect();
        let line: String = message.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(100).collect();
        for id in ids {
            let _ = self.unhold(&id, &format!("{name} posted about it: {line}"));
        }
    }

    /// How a decision that comes back from hold is said (the daemon's).
    pub fn on_back(&self, say: impl Fn(&Decision, &str) + Send + Sync + 'static) {
        let _ = self.back.set(Box::new(say));
    }

    /// Puts `id` on hold (`reason`: the user's words, may be empty; `by`:
    /// who): out of the queue and the count, never said, until it comes back.
    pub fn hold(&self, id: &str, reason: &str, by: &str) -> Result<Decision> {
        let d = self.change(|b, now| b.hold(id, reason, by, now))?;
        eprintln!("[decisions] {id} is on hold ({by}){}: {}", if d.hold.as_ref().is_some_and(|h| !h.reason.is_empty()) { format!(", \"{}\"", d.hold.as_ref().unwrap().reason) } else { String::new() }, d.title);
        record_hold(&d, "");
        Ok(d)
    }

    /// Lets `id` go from hold (`why`: what brought it back): in the queue
    /// again, counted, and said once.
    pub fn unhold(&self, id: &str, why: &str) -> Result<Decision> {
        self.change(|b, now| b.unhold(id, why, now))
    }

    /// The card's Hold, the phone's, `tower hold`: on hold with `words`
    /// (the user's), or (`unhold`) let go, `words` saying what brought it
    /// back (none: the user did). The decision as the state carries it.
    pub fn hold_with(&self, id: &str, words: &str, unhold: bool, by: &str) -> Result<Value, Refused> {
        if self.get(id).is_none() {
            return Err(Refused::Unknown(id.to_string()));
        }
        let r = if unhold {
            let why = match words.trim() {
                "" if by.starts_with("tower") => "the tower let it go from hold",
                "" => "you let it go from hold",
                w => w,
            };
            self.unhold(id, why)
        } else {
            self.hold(id, words, by)
        };
        match r {
            Ok(d) => Ok(d.view(in_muted(&d, &(self.muted)()))),
            Err(e) => Err(match self.get(id) {
                Some(d) if d.closed_ms.is_some() => Refused::Closed { id: id.to_string(), state: d.state },
                _ => Refused::Bad(format!("{e:#}")),
            }),
        }
    }

    /// A round of one source: what opened and closed is logged, and what
    /// closed is written down.
    fn round(&self, source: &str, f: impl FnOnce(&mut Book, u64) -> Changes) -> Changes {
        let ch = self.change(f);
        for id in &ch.fresh {
            if let Some(d) = self.get(id) {
                eprintln!("[decisions] {id} ({}, {source}) for the user: {}", d.kind.word(), d.title);
            }
        }
        for d in &ch.closed {
            eprintln!("[decisions] {} is {}: {}", d.id, d.state.word(), d.title);
            record(d);
        }
        ch
    }

    /// The question sets as they are now: those that are the user's are
    /// decisions, one per key (`ended`: how a set that is over ended).
    pub fn sync_questions(&self, sets: &[QuestionSet], ended: &dyn Fn(&str) -> State) {
        let asked: Vec<decisions::Asked> = sets.iter().filter_map(decisions::asked).collect();
        // Nothing asked and nothing open that asks: the usual case, left alone.
        if asked.is_empty() && !lock(&self.book).open.iter().any(|d| d.kind.asks()) {
            return;
        }
        self.round("a question", |b, now| b.sync_questions(&asked, ended, now));
    }

    /// The PRs as they are now: those waiting for a merge are one decision per repo and space.
    pub fn sync_prs(&self, prs: &[Pr]) {
        self.round("a merge", |b, now| b.sync_merges(prs, now));
        self.weigh(prs);
    }

    /// The PRs that merged in a held decision's space since it was held
    /// (not its own, not one its words name: those bring it back anyway),
    /// each read once against the user's words (`judge`), in the background:
    /// the one that is what they waited for brings it back.
    fn weigh(&self, prs: &[Pr]) {
        let Some(judge) = self.judge else { return };
        let todo: Vec<(String, String, Pr)> = {
            let b = lock(&self.book);
            let mut todo = vec![];
            for d in &b.open {
                let Some(h) = d.hold.as_ref().filter(|h| !h.reason.is_empty()) else { continue };
                for p in prs {
                    let key = p.key();
                    if p.state == "merged"
                        && p.closed_ms.unwrap_or(0) >= h.since_ms
                        && p.space.as_deref().is_some_and(|s| d.in_space(s))
                        && !d.prs.iter().any(|r| r.key == key)
                        && !h.weighed.contains(&key)
                        && !h.until_prs.contains(&key)
                    {
                        todo.push((d.id.clone(), h.reason.clone(), p.clone()));
                    }
                }
            }
            todo
        };
        if todo.is_empty() {
            return;
        }
        // Weighed once, whatever comes of it.
        self.change(|b, _| {
            for (id, _, p) in &todo {
                if let Some(h) = b.open.iter_mut().find(|d| d.id == *id).and_then(|d| d.hold.as_mut()) {
                    h.weighed.push(p.key());
                }
            }
        });
        for (id, reason, p) in todo {
            let me = self.clone();
            thread::spawn(move || {
                let done = judge(&reason, &p);
                eprintln!("[decisions] {id} on hold (\"{reason}\"): {} merged, read as {}", p.key(), match done {
                    Some(true) => "what it waited for",
                    Some(false) => "something else",
                    None => "nothing (the model couldn't say)",
                });
                if done == Some(true) {
                    let title: String = p.title.chars().take(80).collect();
                    let _ = me.unhold(&id, &format!("PR #{} \"{title}\" merged, which looks like what it waited for", p.number));
                }
            });
        }
    }

    /// `tower raise`: the decision it is, as it stands, and whether the user
    /// is told now (the first time; every time while the tower tells everything).
    pub fn raise(&self, r: Raise) -> (Raised, Decision, bool) {
        let raised = self.change(|b, now| b.raise(r, now));
        let d = self.get(&raised.id).unwrap_or_default();
        let told = self.tells(std::slice::from_ref(&raised.id), true);
        (raised, d, told)
    }

    /// A task failed again after its retry. Told the first time, at a quiet
    /// level (while the tower tells everything, the brain says it as it always did).
    pub fn failed(&self, f: Failure) -> (Raised, Decision, bool) {
        let raised = self.change(|b, now| b.failed(f, now));
        let d = self.get(&raised.id).unwrap_or_default();
        let told = self.tells(std::slice::from_ref(&raised.id), false);
        (raised, d, told)
    }

    /// The tasks that still stand failed after a retry (the others' decisions are over).
    pub fn sync_failed(&self, still: &[String]) {
        if !lock(&self.book).open.iter().any(|d| d.kind == decisions::Kind::Failed) {
            return;
        }
        self.round("a failed task", |b, now| b.sync_failed(still, now));
    }

    /// A session is gone: it leaves its decisions, and one left with no
    /// session is over. (A question's decision follows its question sets; a
    /// merge stays while its PRs wait, with that session no longer waiting.)
    pub fn session_gone(&self, address: &str) {
        let about = lock(&self.book)
            .open
            .iter()
            .any(|d| !d.kind.asks() && d.sessions.iter().any(|w| w.address == address && (d.kind != decisions::Kind::Merge || w.waiting)));
        if about {
            self.round("a session gone", |b, now| b.session_gone(address, now));
        }
    }

    /// A space has no session left (`merges` false), or was closed or
    /// archived (`merges` true): what was open there is over, its merges
    /// only when it is closed or archived.
    pub fn space_over(&self, space: &str, merges: bool) -> usize {
        let any = lock(&self.book).open.iter().any(|d| d.in_space(space) && !d.kind.asks() && (merges || d.kind != decisions::Kind::Merge));
        if !any {
            return 0;
        }
        self.round(if merges { "its space is closed" } else { "its space is quiet" }, |b, now| b.space_over(space, merges, now)).closed.len()
    }

    /// A session is blocked on something the tower can't read: a decision
    /// (it counts on the orb and stands in the queue), and whether the user
    /// is told now (`tells`: the first time at a quiet level, every time
    /// while the tower tells everything, as before there were decisions).
    pub fn blocked(&self, space: Option<String>, who: decisions::Who, title: &str) -> bool {
        let raised = self.change(|b, now| b.blocked(space, who, title, now));
        if raised.fresh {
            eprintln!("[decisions] {} (blocked) for the user: {title}", raised.id);
        }
        self.tells(std::slice::from_ref(&raised.id), true)
    }

    /// A session's turn ended waiting on the user: the open decision the
    /// brain raised about it, if there is one, which that wait joins
    /// (`Book::waits_on_raised`). Its id.
    pub fn waits_on_raised(&self, address: &str) -> Option<String> {
        let id = self.change(|b, now| b.waits_on_raised(address, now))?;
        eprintln!("[decisions] {address} waits on {id}, the decision raised about it: no second card");
        Some(id)
    }

    /// A session as the radar sees it, blocked or not: what it was blocked
    /// on is over once it is not.
    pub fn session_blocked(&self, address: &str, blocked: bool) {
        if blocked {
            return;
        }
        let about =
            lock(&self.book).open.iter().any(|d| d.kind == decisions::Kind::Blocked && d.sessions.iter().any(|w| w.address == address));
        if about {
            self.round("a session no longer blocked", |b, now| b.sync_blocked(address, false, now));
        }
    }

    /// The question set `set` was answered in its session's own terminal: the
    /// decision it stood in closes now, as answered (or, while other sessions
    /// still ask the same, its session leaves it), and the state carries it
    /// at once (the Mac's windows, the phone's `decisions` event). Not written
    /// down here: the caller writes the line once it knows what was chosen.
    /// The decisions it was in: (id, over now).
    pub fn answered_in_terminal(&self, set: &str) -> Vec<(String, bool)> {
        if !lock(&self.book).open.iter().any(|d| d.sessions.iter().any(|w| w.question_id.as_deref() == Some(set))) {
            return vec![];
        }
        let out = self.change(|b, now| b.answered_in_terminal(set, now));
        for (id, over) in &out {
            eprintln!("[decisions] {id}: {set} was answered in its terminal{}", if *over { "; the decision is over" } else { "" });
        }
        out
    }

    /// The user's answer to the question set `set` was passed on to its
    /// session by the tower (`with`: how): that session leaves the decision
    /// it stood in, which closes as answered with it when nobody else asks
    /// the same (and is written down). The decisions it was in: (id, over now).
    pub fn passed_on(&self, set: &str, with: &str) -> Vec<(String, bool)> {
        if !lock(&self.book).open.iter().any(|d| d.sessions.iter().any(|w| w.question_id.as_deref() == Some(set))) {
            return vec![];
        }
        let out = self.change(|b, now| b.answered_with(set, with, now));
        for (id, over) in &out {
            eprintln!("[decisions] {id}: {set} was answered through the tower{}", if *over { "; the decision is over" } else { "; others still wait on it" });
            if *over {
                self.record_closed(id);
            }
        }
        out
    }

    /// A question set was settled elsewhere (a PR it asked about merged): the
    /// decision it stood in is answered with `what`.
    pub fn settled(&self, set: &str, what: &str) {
        let ids: Vec<String> = lock(&self.book).open.iter().filter(|d| d.sessions.iter().any(|w| w.question_id.as_deref() == Some(set))).map(|d| d.id.clone()).collect();
        for id in ids {
            let w = what.to_string();
            self.change(|b, now| b.answered(&id, &w, now));
            eprintln!("[decisions] {id}: {what}");
        }
    }

    /// What a decision was answered with, kept on it (open or closed lately).
    pub fn note_answer(&self, id: &str, with: &str) {
        self.change(|b, _| b.note_answer(id, with));
    }

    /// A decision that closed without its line in the record: written now, as any other.
    pub fn record_closed(&self, id: &str) {
        if let Some(d) = self.get(id).filter(|d| d.closed_ms.is_some()) {
            record(&d);
        }
    }

    /// A session as the radar sees it: working on something else (`waiting`
    /// false), or waiting again.
    pub fn session_seen(&self, address: &str, waiting: bool) {
        let moved =
            lock(&self.book).open.iter().any(|d| !d.kind.asks() && d.sessions.iter().any(|w| w.address == address && w.waiting != waiting));
        if moved {
            self.change(|b, _| b.session_waits(address, waiting));
        }
    }

    /// The open decisions that stand for a question set.
    pub fn for_question(&self, set: &str) -> Vec<String> {
        lock(&self.book)
            .open
            .iter()
            .filter(|d| d.sessions.iter().any(|w| w.question_id.as_deref() == Some(set)))
            .map(|d| d.id.clone())
            .collect()
    }

    /// The open merge decisions that list any of these PRs.
    pub fn for_prs(&self, prs: &[Pr]) -> Vec<String> {
        let keys: Vec<String> = prs.iter().map(Pr::key).collect();
        lock(&self.book).open.iter().filter(|d| d.prs.iter().any(|p| keys.contains(&p.key))).map(|d| d.id.clone()).collect()
    }

    /// Whether the user is told now of what the decisions `ids` stand for,
    /// and marks them told. At a quiet level: only when one of them hasn't
    /// been told yet (the same key again says nothing). While the tower
    /// tells everything: `loud`, what it did before there were decisions.
    /// Never when their spaces are muted, nor when they are on hold. No
    /// decision stands for it: `loud`.
    pub fn tells(&self, ids: &[String], loud: bool) -> bool {
        let spaces = (self.muted)();
        let (known, first, muted) = {
            let mut b = lock(&self.book);
            let (mut known, mut first, mut muted) = (false, false, true);
            for id in ids {
                // On hold: not said, and not marked told (it is told once it comes back).
                if b.find(id).is_some_and(|d| d.held() && d.closed_ms.is_none()) {
                    known = true;
                    continue;
                }
                if let Some(f) = b.claim_told(id) {
                    known = true;
                    first |= f;
                    muted &= b.find(id).is_some_and(|d| in_muted(d, &spaces));
                }
            }
            if first {
                self.save(&b);
            }
            (known, first, muted)
        };
        if !known {
            return loud;
        }
        if muted {
            return false;
        }
        if (self.level)().quiet() { first } else { loud }
    }

    /// Whether the user is told now that these PRs wait for their merge
    /// (`tells`, for their merge decisions). PRs no decision stands for (the
    /// user dismissed theirs) are said only while the tower tells everything.
    pub fn tells_merge(&self, prs: &[Pr], loud: bool) -> bool {
        let ids = self.for_prs(prs);
        if ids.is_empty() {
            return loud && !(self.level)().quiet();
        }
        self.tells(&ids, loud)
    }

    /// At start: what is open was told by the run before (or comes from
    /// before there were decisions). Nothing is said again for it.
    pub fn told_all(&self) {
        self.change(|b, _| b.open.iter_mut().for_each(|d| d.told = true));
    }

    /// A decision by its id: an open one, else one that closed lately.
    pub fn get(&self, id: &str) -> Option<Decision> {
        lock(&self.book).find(id).cloned()
    }

    /// The user answered it (`with`: the option's label, or their words).
    pub fn answered(&self, id: &str, with: &str) -> Option<Decision> {
        let d = self.change(|b, now| b.answered(id, with, now))?;
        eprintln!("[decisions] {id} answered: {with}");
        record(&d);
        Some(d)
    }

    /// The request that makes an answered decision's answer a standing rule
    /// (`decisions::rule_for`), and the space to ask it in. `answer`: what it
    /// was answered with, when the caller knows better than the decision
    /// does (a question answered through its dialog's own steps).
    pub fn rule_of(&self, id: &str, answer: Option<&str>) -> Result<(String, Option<String>), Refused> {
        let Some(d) = self.get(id) else { return Err(Refused::Unknown(id.to_string())) };
        if d.closed_ms.is_none() && d.state == State::Open {
            return Err(Refused::Bad(format!("{id} isn't answered yet: answer it first")));
        }
        let answer = answer.map(str::trim).filter(|a| !a.is_empty()).unwrap_or(d.answer.trim());
        match decisions::rule_for(d.kind, &d.why, &d.title, d.space.as_deref(), answer) {
            Some(text) => Ok((text, d.space.clone())),
            None if answer.is_empty() && matches!(d.kind, decisions::Kind::Question | decisions::Kind::Choice) => {
                Err(Refused::Failed("the tower doesn't know what it was answered with: send {\"answer\": \"<the answer>\"}".into()))
            }
            None => Err(Refused::Failed("no rule is made of this one: only of a question or a choice, and never of a deploy, a delete, production data, secrets or money".into())),
        }
    }

    /// The user dismissed it.
    pub fn dismiss(&self, id: &str) -> Result<Decision> {
        let d = self.change(|b, now| b.dismiss(id, now))?;
        eprintln!("[decisions] {id} dismissed: {}", d.title);
        record(&d);
        Ok(d)
    }

    /// `tower decisions`: the queue in order. No space: the main queue (muted
    /// spaces left out). `space` (its name, or a part): that space's, muted
    /// or not. `all`: the last ones that closed too, newest first.
    pub fn list(&self, space: Option<&str>, all: bool) -> Value {
        let space = space.map(str::trim).filter(|s| !s.is_empty()).map(|want| {
            let b = lock(&self.book);
            let known: Vec<String> = b.open.iter().chain(b.closed.iter()).filter_map(|d| d.space.clone()).collect();
            crate::spaces::resolve(want, &known).unwrap_or_else(|| want.to_string())
        });
        self.listed(space.as_deref(), all)
    }

    /// `list`, for the space of exactly that name (any case): the phone
    /// names a space whole, and never gets another one's that starts alike.
    pub fn listed(&self, space: Option<&str>, all: bool) -> Value {
        let now = crate::history::now_ms();
        let b = lock(&self.book);
        let muted = (self.muted)();
        // Its name as the decisions there spell it, when any is known.
        let space = space.map(str::trim).filter(|s| !s.is_empty()).map(|s| {
            let known = b.open.iter().chain(b.closed.iter()).find(|d| d.in_space(s)).and_then(|d| d.space.clone());
            known.unwrap_or_else(|| s.to_string())
        });
        let wanted = |d: &Decision| match &space {
            Some(s) => d.in_space(s),
            None => !in_muted(d, &muted),
        };
        let list: Vec<Value> = b.ordered(now).into_iter().filter(|d| wanted(d) && !d.held()).map(|d| d.view(in_muted(d, &muted))).collect();
        // On hold: listed apart, not counted.
        let held: Vec<Value> = held_views(&b, &muted).into_iter().filter(|v| b.find(v["id"].as_str().unwrap_or("")).is_some_and(wanted)).collect();
        let mut v = json!({ "space": space, "count": list.len(), "decisions": list, "on_hold": held });
        if all {
            v["closed"] = Value::Array(b.closed.iter().rev().filter(|d| wanted(d)).map(|d| d.view(in_muted(d, &muted))).collect());
        }
        v
    }

    /// A decision as the state carries it: an open one, else one that closed lately.
    pub fn view(&self, id: &str) -> Option<Value> {
        let muted = (self.muted)();
        lock(&self.book).find(id).map(|d| d.view(in_muted(d, &muted)))
    }

    /// From now on: the queue's order moves with time (and a space may be
    /// muted meanwhile), and a session that ended without the radar seeing
    /// it (while the tower was down) leaves its decisions.
    pub fn start(&self) {
        let me = self.clone();
        thread::Builder::new()
            .name("decisions".into())
            .spawn(move || {
                let mut round: u64 = 0;
                loop {
                    thread::sleep(Duration::from_secs(30));
                    if round.is_multiple_of(4) {
                        me.look();
                    }
                    round += 1;
                    me.refresh();
                }
            })
            .ok();
    }

    /// Asks Herdr about every session a decision names.
    fn look(&self) {
        let mut sessions: Vec<String> =
            lock(&self.book).open.iter().filter(|d| !d.kind.asks()).flat_map(|d| d.sessions.iter().map(|w| w.address.clone())).collect();
        sessions.sort();
        sessions.dedup();
        for a in sessions {
            match hc::lookup(&a) {
                hc::Lookup::Found(t) if t["state"] == "live" => self.session_seen(&a, hc::status(&t) != hc::SessionStatus::Working),
                hc::Lookup::Found(_) | hc::Lookup::Gone => self.session_gone(&a),
                hc::Lookup::Unreachable(_) => {}
            }
        }
    }
}

#[cfg(test)]
impl Decisions {
    /// Decisions kept nowhere, for a tower that tells this much.
    pub fn telling(shared: Shared, level: fn() -> Level) -> Decisions {
        Decisions { level, ..Decisions::new(shared) }
    }

    /// With these spaces muted (as `spaces.json` would say).
    pub fn muted_by(mut self, muted: impl Fn() -> Vec<String> + Send + Sync + 'static) -> Decisions {
        self.muted = Arc::new(muted);
        self
    }
}

/// What an answer to a decision is carried out with, beyond the decisions
/// themselves: the daemon's question sets, its send queue and its agenda
/// ledger (tests give their own, so nothing is typed anywhere).
pub struct Carry<'a> {
    pub asks: &'a Asks,
    /// Queues an instruction for a session, as work handed out:
    /// `{"session", "name", "queue_id", "sent"}`.
    pub send: &'a dyn Fn(&str, &str) -> Result<Value>,
    /// A `tower agenda` command, by the daemon's ledger.
    pub agenda: &'a dyn Fn(&crate::agenda::Cmd) -> crate::protocol::Response,
    /// Puts back (or lets go) what a restart closed: what was done.
    pub reopen: &'a dyn Fn(&crate::reopen::Pick) -> Result<Value>,
    /// The sessions waiting for an account: on at the reset (true), or let go.
    pub accounts: &'a dyn Fn(bool) -> Result<Value>,
    /// A plan's question answered by the user (plan, question, answer): written into the plan.
    pub plan: &'a dyn Fn(&str, &str, &str) -> Result<Value>,
}

/// Whether the answer just given can be made a rule, and the request that
/// would make it one: `{"can": bool, "text": "<the request>" or null}`.
fn rule_view(d: &Decision, answer: &str) -> Value {
    let text = decisions::rule_for(d.kind, &d.why, &d.title, d.space.as_deref(), answer);
    json!({ "can": text.is_some(), "text": text })
}

/// Why an answer to a decision (or its dismissal) was refused. As a
/// sentence it is what `tower answer` prints; the phone API gives each its
/// own status and code.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    // (`rule_of` says `Bad` for one that isn't answered yet, `Failed` for one no rule is made of.)
    /// No decision has that id.
    Unknown(String),
    /// It is over already (`state`: answered, dismissed or stopped).
    Closed { id: String, state: State },
    /// A merge is its PR's own flow: nothing merges by an answer.
    Merge(String),
    /// The answer doesn't fit the decision (no such option, no words, answers that don't read).
    Bad(String),
    /// None of its question sets took the answer: (session, why) each.
    Questions(Vec<(String, String)>),
    /// It fits, and couldn't be carried out (the queue, the agenda ledger).
    Failed(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Unknown(id) => write!(f, "no decision {id} (`tower decisions` lists them)"),
            Refused::Closed { id, state } => write!(f, "already answered: {id} is {} already", state.word()),
            Refused::Merge(why) | Refused::Bad(why) | Refused::Failed(why) => f.write_str(why),
            Refused::Questions(errors) => f.write_str(&errors.iter().map(|(session, why)| format!("{session}: {why}")).collect::<Vec<_>>().join("; ")),
        }
    }
}

impl Decisions {
    /// `tower answer <decision>` (the Mac's cards, the phone): a question's
    /// decision answers its question sets (every session that asks it); a
    /// raised one sends the chosen words to its waiting sessions through
    /// the queue, as an instruction; a failed task is sent again or
    /// dropped. A merge isn't answered here.
    pub fn answer_with(&self, id: &str, body: &Value, submit: bool, with: &Carry) -> Result<Value, Refused> {
        use decisions::Act;
        let Some(d) = self.get(id) else { return Err(Refused::Unknown(id.to_string())) };
        if d.closed_ms.is_some() {
            return Err(Refused::Closed { id: id.to_string(), state: d.state });
        }
        let act = match decisions::act(&d, body, submit) {
            Ok(a) => a,
            Err(e) if d.kind == decisions::Kind::Merge => return Err(Refused::Merge(format!("{e:#}"))),
            Err(e) => return Err(Refused::Bad(format!("{e:#}"))),
        };
        // An option by its number, from a sender that says what it showed there: still that one.
        if let (Some(n), Some(label)) = (body.get("choice").and_then(Value::as_u64), body.get("label").and_then(Value::as_str))
            && d.options.get(n as usize).is_some_and(|o| o.label.trim() != label.trim())
        {
            return Err(Refused::Bad(format!("{id} changed since it was shown: its answer {} is \"{}\" now. Look at the card again", n + 1, d.options[n as usize].label.trim())));
        }
        match act {
            Act::Questions { sets, answers, submit } => {
                let answers: Vec<crate::questions::Answer> = match serde_json::from_value(answers) {
                    Ok(a) => a,
                    Err(e) => return Err(Refused::Bad(format!("answers: {e}"))),
                };
                // Only where it means what the card showed: another session may ask the same in a dialog that offers other answers.
                let open: Vec<crate::questions::QuestionSet> = sets.iter().filter_map(|(_, set)| with.asks.get(set)).collect();
                let (take, skipped) = match decisions::takers(&d, body, &answers, &open) {
                    Ok(t) => t,
                    Err(why) => return Err(Refused::Bad(why)),
                };
                let (mut answered, mut errors) = (vec![], skipped);
                for (session, set) in sets.iter().filter(|(_, set)| take.iter().any(|t| &t.id == set) || !open.iter().any(|o| &o.id == set)) {
                    match with.asks.answer(set, &answers, submit) {
                        Ok(s) => answered.push(json!({ "session": session, "question": s.id, "status": s.status })),
                        Err(e) => errors.push((session.clone(), format!("{e:#}"))),
                    }
                }
                if answered.is_empty() {
                    return Err(Refused::Questions(errors));
                }
                // Its sets close as their dialogs go, and the decision with them.
                let state = self.get(id).map(|d| d.state).filter(|s| *s != State::Open).unwrap_or(State::Answering);
                let errors: Vec<String> = errors.iter().map(|(session, why)| format!("{session}: {why}")).collect();
                // What it was answered with, in words, when the answer says so: kept, to be made a rule.
                let said = match (body["choice"].as_u64(), body["other"].as_str()) {
                    (Some(n), _) => d.options.get(n as usize).map(|o| o.label.clone()),
                    (None, Some(words)) => Some(words.trim().to_string()),
                    _ => None,
                }
                .filter(|l| !l.is_empty());
                if let Some(label) = &said {
                    self.change(|b, _| b.note_answer(id, label));
                }
                let rule = rule_view(&d, said.as_deref().unwrap_or(""));
                Ok(json!({ "ok": true, "id": id, "state": state.word(), "answered": answered, "errors": errors, "rule": rule }))
            }
            Act::Send { to, text, label } => {
                let (mut sent, mut errors) = (vec![], vec![]);
                for w in &to {
                    match (with.send)(&w.address, &text) {
                        Ok(v) => sent.push(v),
                        Err(e) => errors.push(format!("{}: {e:#}", w.address)),
                    }
                }
                if sent.is_empty() && !to.is_empty() {
                    return Err(Refused::Failed(format!("its answer couldn't be sent: {}", errors.join("; "))));
                }
                self.answered(id, &label);
                let rule = rule_view(&d, &label);
                Ok(json!({ "ok": true, "id": id, "state": "answered", "answer": label, "sent": sent, "errors": errors, "rule": rule }))
            }
            Act::Requeue { task, text } => {
                // In the user's words: the queue takes the same instruction again for a retry of its own.
                let words = if text.is_empty() { "The user says: try it once more.".to_string() } else { format!("The user says: {text}") };
                let r = (with.agenda)(&crate::agenda::Cmd { action: "requeue".into(), target: task.clone(), text: words, ..Default::default() });
                if !r.ok {
                    return Err(Refused::Failed(r.error.unwrap_or_default()));
                }
                self.answered(id, if text.is_empty() { "try again" } else { &text });
                Ok(json!({ "ok": true, "id": id, "state": "answered", "task": task, "requeued": r.state }))
            }
            Act::Abandon { task } => {
                let r = (with.agenda)(&crate::agenda::Cmd {
                    action: "abandon".into(),
                    target: task.clone(),
                    text: "the user dropped it after it failed again".into(),
                    ..Default::default()
                });
                if !r.ok {
                    return Err(Refused::Failed(r.error.unwrap_or_default()));
                }
                self.answered(id, "dropped");
                Ok(json!({ "ok": true, "id": id, "state": "answered", "task": task, "abandoned": r.state }))
            }
            Act::Reopen { pick, label } => {
                let done = match (with.reopen)(&pick) {
                    Ok(v) => v,
                    Err(e) => return Err(Refused::Failed(format!("{e:#}"))),
                };
                self.answered(id, &label);
                Ok(json!({ "ok": true, "id": id, "state": "answered", "answer": label, "reopen": done }))
            }
            Act::Plan { plan, question, answer, label } => {
                let done = match (with.plan)(&plan, &question, &answer) {
                    Ok(v) => v,
                    Err(e) => return Err(Refused::Failed(format!("{e:#}"))),
                };
                self.answered(id, &label);
                Ok(json!({ "ok": true, "id": id, "state": "answered", "answer": label, "plan": done }))
            }
            Act::Accounts { wait, label } => {
                self.answered(id, &label);
                let done = match (with.accounts)(wait) {
                    Ok(v) => v,
                    Err(e) => return Err(Refused::Failed(format!("{e:#}"))),
                };
                Ok(json!({ "ok": true, "id": id, "state": "answered", "answer": label, "accounts": done }))
            }
        }
    }

    /// A plan's question in the user's queue (`decisions::PlanAsk`): its
    /// card's id, and whether the user is told now (the first time).
    pub fn plan_question(&self, a: decisions::PlanAsk) -> (String, bool) {
        let raised = self.change(|b, now| b.plan_question(a, now));
        if raised.fresh {
            eprintln!("[decisions] {} (plan question) for the user", raised.id);
        }
        let told = raised.fresh && self.tells(std::slice::from_ref(&raised.id), true);
        (raised.id, told)
    }

    /// A plan question's card is over: answered elsewhere (the brain, the
    /// CLI), with `with`. Nothing when it isn't open.
    pub fn plan_answered(&self, plan: &str, question: &str, with: &str) -> Option<Decision> {
        let key = decisions::plan_key(plan, question);
        let id = lock(&self.book).open.iter().find(|d| d.key == key).map(|d| d.id.clone())?;
        self.answered(&id, with)
    }

    /// A plan is closed: its cards still open (its questions, a revision) are over.
    pub fn plan_over(&self, plan: &str) -> usize {
        let prefix = format!("{}{}:", decisions::PLAN_KEY, plan.trim());
        let ids: Vec<String> = lock(&self.book).open.iter().filter(|d| d.key.starts_with(&prefix)).map(|d| d.id.clone()).collect();
        for id in &ids {
            if let Some(d) = self.change(|b, now| b.close(id, State::Stopped, now)) {
                record(&d);
            }
        }
        ids.len()
    }

    /// The card offering back what a restart closed, as it should stand
    /// now (None: nothing left, an open one stops). Its id, and whether
    /// the user is told now (the first time, as a raise is).
    pub fn offer_reopen(&self, offer: Option<crate::reopen::Offer>) -> Option<(String, bool)> {
        let had = lock(&self.book).open.iter().find(|d| d.key == decisions::REOPEN_KEY).map(|d| (d.title.clone(), d.detail.clone(), d.options.clone()));
        let same = match (&had, &offer) {
            (None, None) => return None,
            (Some((title, detail, options)), Some(o)) => {
                *title == o.title && *detail == decisions::detail_cut(&o.detail, false) && options.iter().map(|x| &x.label).eq(o.options.iter().map(|x| &x.0))
            }
            _ => false,
        };
        let raised = if same {
            lock(&self.book).open.iter().find(|d| d.key == decisions::REOPEN_KEY).map(|d| Raised { id: d.id.clone(), fresh: false, folded: false, count: d.count, waiting: 0 })
        } else {
            self.change(|b, now| b.reopen(offer, now))
        };
        let raised = raised?;
        if raised.fresh {
            eprintln!("[decisions] {} (reopen) for the user: what the restart closed", raised.id);
        }
        let told = raised.fresh && self.tells(std::slice::from_ref(&raised.id), true);
        Some((raised.id, told))
    }

    /// The card saying every Claude account is out (`crate::failover`), as
    /// it should stand now (None: nothing waits, an open one stops). Its id,
    /// and whether the user is told now (the first time, as a raise is).
    pub fn offer_accounts(&self, card: Option<crate::failover::Card>) -> Option<(String, bool)> {
        let had = lock(&self.book).open.iter().find(|d| d.key == decisions::ACCOUNTS_KEY).map(|d| (d.title.clone(), d.detail.clone(), d.options.clone()));
        let same = match (&had, &card) {
            (None, None) => return None,
            (Some((title, detail, options)), Some(c)) => {
                *title == c.title && *detail == decisions::detail_cut(&c.detail, false) && options.iter().map(|x| &x.label).eq(c.options.iter().map(|x| &x.0))
            }
            _ => false,
        };
        let raised = if same {
            lock(&self.book).open.iter().find(|d| d.key == decisions::ACCOUNTS_KEY).map(|d| Raised { id: d.id.clone(), fresh: false, folded: false, count: d.count, waiting: 0 })
        } else {
            self.change(|b, now| b.accounts_out(card, now))
        };
        let raised = raised?;
        if raised.fresh {
            eprintln!("[decisions] {} (accounts) for the user: every Claude account is out of usage", raised.id);
        }
        let told = raised.fresh && self.tells(std::slice::from_ref(&raised.id), true);
        Some((raised.id, told))
    }

    /// `tower dismiss <decision>`: out of the queue, with nothing sent. A
    /// question's sets are dismissed with it. One that is over already
    /// says how it ended.
    pub fn dismiss_with(&self, id: &str, asks: &Asks) -> Result<Value, Refused> {
        let Some(d) = self.get(id) else { return Err(Refused::Unknown(id.to_string())) };
        if d.closed_ms.is_some() {
            return Ok(json!({ "id": id, "state": d.state.word() }));
        }
        if d.kind.asks() {
            let errors: Vec<String> = d
                .sessions
                .iter()
                .filter_map(|w| w.question_id.as_deref().map(|q| (w, q)))
                .filter_map(|(w, q)| asks.dismiss(q).err().map(|e| format!("{}: {e:#}", w.address)))
                .collect();
            // Its sets are gone, and the decision with them; one that couldn't go keeps it.
            return match self.get(id).filter(|d| d.closed_ms.is_none()) {
                None => Ok(json!({ "id": id, "state": "dismissed" })),
                Some(_) if errors.is_empty() => match self.dismiss(id) {
                    Ok(d) => Ok(json!({ "id": id, "state": d.state.word() })),
                    Err(e) => Err(Refused::Failed(format!("{e:#}"))),
                },
                Some(_) => Err(Refused::Failed(errors.join("; "))),
            };
        }
        match self.dismiss(id) {
            Ok(d) => Ok(json!({ "id": id, "state": d.state.word() })),
            Err(e) => Err(Refused::Failed(format!("{e:#}"))),
        }
    }
}

/// A decision that is over, written down (kind `decision`, for the record).
/// The open decisions on hold as the state carries them, newest hold first.
fn held_views(book: &Book, muted: &[String]) -> Vec<Value> {
    let mut held: Vec<&Decision> = book.open.iter().filter(|d| d.held()).collect();
    held.sort_by_key(|d| std::cmp::Reverse(d.hold.as_ref().map(|h| h.since_ms).unwrap_or(0)));
    held.into_iter().map(|d| d.view(in_muted(d, muted))).collect()
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The record's line for a hold (`why` empty), or a decision back from one.
fn record_hold(d: &Decision, why: &str) {
    if cfg!(test) {
        return;
    }
    let who = d.sessions.first().cloned();
    let (status, text) = match &d.hold {
        Some(h) if why.is_empty() => ("on hold", if h.reason.is_empty() { format!("On hold: {}", d.title) } else { format!("On hold: {} (\"{}\")", d.title, h.reason) }),
        _ => ("back", format!("Back from hold: {} ({why})", d.title)),
    };
    let entry = crate::history::Entry {
        kind: "decision".into(),
        id: Some(d.id.clone()),
        session: who.as_ref().map(|w| w.address.clone()),
        name: who.map(|w| w.name).filter(|n| !n.is_empty()),
        space: d.space.clone(),
        status: Some(status.into()),
        why: Some(d.why.clone()).filter(|w| !w.is_empty()),
        text,
        record: true,
        ..Default::default()
    };
    crate::history::later(move || crate::history::append(entry));
}

/// Whether the PR `pr`, which merged, is what a hold's words (`reason`)
/// wait for: one call to the model (the default account, `[brains] model`,
/// as `covered` reads). None: it couldn't say.
fn judge_merged(reason: &str, pr: &Pr) -> Option<bool> {
    let prompt = format!(
        "The user put a decision on hold with these words: <hold>{}</hold>\nThis pull request just merged: <pr>#{} \"{}\" ({} into {}, branch {})</pr>\n\
         Is this pull request what the user said to wait for, so the wait is over? Only when it plainly is. \
         Answer with JSON only: {{\"done\": true|false, \"why\": \"a few words\"}}",
        reason, pr.number, pr.title, pr.repo, pr.base, pr.head
    );
    let out = crate::covered::ask(&prompt).ok()?;
    crate::ticket::parse_reply(&out).ok()?["done"].as_bool()
}

fn record(d: &Decision) {
    // Tests never write the history.
    if cfg!(test) {
        return;
    }
    let entry = entry_of(d);
    crate::history::later(move || crate::history::append(entry));
}

/// The record's line for a decision that is over.
fn entry_of(d: &Decision) -> crate::history::Entry {
    let who = d.sessions.first().cloned();
    // Blocked on what the tower couldn't read, and no longer: the user answered it in its terminal.
    let there = d.kind == decisions::Kind::Blocked && d.state == State::Answered;
    crate::history::Entry {
        source: who.as_ref().filter(|_| there).map(|w| format!("terminal:{}", w.address)),
        kind: "decision".into(),
        id: Some(d.id.clone()),
        session: who.as_ref().map(|w| w.address.clone()),
        name: who.map(|w| w.name).filter(|n| !n.is_empty()),
        space: d.space.clone(),
        status: Some(d.state.word().to_string()),
        why: Some(d.why.clone()).filter(|w| !w.is_empty()),
        text: if d.answer.is_empty() { d.title.clone() } else { format!("{} -> {}", d.title, d.answer) },
        record: true,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decisions::{Kind, Who};

    fn shared() -> Shared {
        Shared::new(super::super::state::State::new("ptt", "test", "test"))
    }

    fn raise(space: &str, session: &str, text: &str) -> Raise {
        Raise {
            detail: String::new(),
            space: Some(space.into()),
            who: Some(Who::new(session, &format!("s-{session}"))),
            why: "design".into(),
            text: text.into(),
            options: vec![],
        }
    }

    fn pr(n: u64, base: &str, session: &str) -> Pr {
        Pr {
            repo: "o/app".into(),
            number: n,
            base: base.into(),
            state: "open".into(),
            mergeable: "mergeable".into(),
            checks: crate::prs::Checks { state: "passing".into(), ..Default::default() },
            session: session.into(),
            name: format!("s-{session}"),
            space: Some("Shop".into()),
            waiting: true,
            found_ms: 1_000,
            ..Default::default()
        }
    }

    /// On hold: listed apart (the state's `decisions_held`, `tower
    /// decisions`' `on_hold`), out of the count, never told; back (updated,
    /// or let go), it is counted again and told once.
    #[test]
    fn a_decision_on_hold_is_listed_apart_and_not_counted_or_told() {
        let s = shared();
        let d = Decisions::telling(s.clone(), || Level::Decisions);
        let said: Arc<Mutex<Vec<String>>> = Arc::default();
        let heard = said.clone();
        d.on_back(move |_, line| heard.lock().unwrap().push(line.to_string()));
        let (r, _, told) = d.raise(raise("Shop", "t1", "Ship the release?"));
        assert!(told, "a new one is told");
        d.sync_prs(&[pr(830, "main", "t2")]);
        assert_eq!(s.get().decisions_count, 2);
        let held = d.hold(&r.id, "wait for the ingest filters", "tower-shop").unwrap();
        assert!(held.held());
        let st = s.get();
        assert_eq!((st.decisions.len(), st.decisions_count, st.decisions_held.len()), (1, 1, 1), "out of the queue and the count");
        assert_eq!((st.decisions_held[0]["id"].as_str(), st.decisions_held[0]["hold"]["reason"].as_str()), (Some(r.id.as_str()), Some("wait for the ingest filters")));
        let listed = d.list(Some("Shop"), false);
        assert_eq!((listed["count"].as_u64(), listed["decisions"].as_array().map(Vec::len), listed["on_hold"][0]["id"].as_str()), (Some(1), Some(1), Some(r.id.as_str())));
        assert_eq!(d.list(None, false)["count"], 1, "the main queue's count too");
        // Never told while held, whatever asks.
        assert!(!d.tells(std::slice::from_ref(&r.id), true));
        // Raised again (its session posted something new): back in the queue, counted, and told once.
        d.raise(raise("Shop", "t1", "ship the release"));
        let st = s.get();
        assert_eq!((st.decisions_count, st.decisions_held.len()), (2, 0));
        assert_eq!(st.decisions.iter().find(|v| v["id"] == r.id.as_str()).unwrap()["back"]["why"], "it came up again");
        assert_eq!(said.lock().unwrap().as_slice(), ["Back from hold: Ship the release?. It came up again."]);
        assert!(!d.tells(std::slice::from_ref(&r.id), true), "told once, not again");
        // A merge held, its session posting about its PR: back.
        let merge = s.get().decisions.iter().find(|v| v["kind"] == "merge").unwrap()["id"].as_str().unwrap().to_string();
        d.hold(&merge, "", "mac").unwrap();
        assert_eq!(s.get().decisions_count, 1);
        d.posted(Some("Shop"), "t9", "s-t9", "Nothing about the release here.");
        assert_eq!(s.get().decisions_held.len(), 1, "it doesn't name the PR");
        d.posted(Some("Shop"), "t2", "s-t2", "Rebased PR #830 on main; it is ready again.\nMore below.");
        assert_eq!(s.get().decisions_count, 2);
        assert_eq!(d.get(&merge).unwrap().back.unwrap().why, "s-t2 posted about it: Rebased PR #830 on main; it is ready again.");
        // The phone and the CLI hold and let go the same way; one that is over can't be.
        let v = d.hold_with(&merge, "after lunch", false, "phone:p1").unwrap();
        assert_eq!((v["hold"]["by"].as_str(), v["hold"]["reason"].as_str()), (Some("phone:p1"), Some("after lunch")));
        let v = d.hold_with(&merge, "", true, "phone:p1").unwrap();
        assert_eq!((v["hold"].is_null(), v["back"]["why"].as_str()), (true, Some("you let it go from hold")));
        assert!(matches!(d.hold_with(&merge, "", true, "mac"), Err(Refused::Bad(_))), "not on hold");
        assert!(matches!(d.hold_with("d000000000000", "", false, "mac"), Err(Refused::Unknown(_))));
        d.dismiss(&r.id).unwrap();
        assert!(matches!(d.hold_with(&r.id, "", false, "mac"), Err(Refused::Closed { .. })));
    }

    /// A PR merged in a held merge's space is read against the user's
    /// words once (the judge here a stand-in for the model): the one that
    /// is what they waited for brings it back.
    #[test]
    fn a_merged_pr_that_is_what_a_hold_waited_for_brings_it_back() {
        let s = shared();
        let mut d = Decisions::telling(s.clone(), || Level::Decisions);
        d.judge = Some(|reason, pr| Some(reason.contains("ingest filters") && pr.title.contains("filters")));
        d.sync_prs(&[pr(830, "main", "t1")]);
        let id = s.get().decisions[0]["id"].as_str().unwrap().to_string();
        d.hold(&id, "wait for the ingest filters then we go for prod", "tower-shop").unwrap();
        let since = d.get(&id).unwrap().hold.unwrap().since_ms;
        let merged = |n: u64, title: &str, at: u64| Pr { state: "merged".into(), waiting: false, title: title.into(), closed_ms: Some(at), ..pr(n, "dev", "t2") };
        // The judge reads on a thread of its own: back as soon as it is, else after `ms`.
        let wait = |ms: u64| {
            for _ in 0..ms / 10 {
                if !d.get(&id).unwrap().held() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        // Merged before it was held, and one that is something else: it stays on hold.
        d.sync_prs(&[pr(830, "main", "t1"), merged(828, "Ingest filters, the first try", since - 1), merged(829, "Fix a typo", since + 1)]);
        wait(500);
        assert!(d.get(&id).unwrap().held());
        assert_eq!(d.get(&id).unwrap().hold.unwrap().weighed, ["o/app#829"], "each read once; one merged before the hold isn't");
        // The filters merge: back, why said.
        d.sync_prs(&[pr(830, "main", "t1"), merged(829, "Fix a typo", since + 1), merged(831, "Ingest page list filters", since + 2)]);
        wait(5_000);
        let back = d.get(&id).unwrap();
        assert!(!back.held());
        assert_eq!(back.back.unwrap().why, "PR #831 \"Ingest page list filters\" merged, which looks like what it waited for");
    }

    #[test]
    fn the_state_carries_the_queue_and_the_orbs_count() {
        let s = shared();
        let d = Decisions::new(s.clone());
        assert_eq!((s.get().decisions.len(), s.get().decisions_count), (0, 0));
        let (r, made, _) = d.raise(raise("Shop", "t1", "Tabs or a sidebar?"));
        assert!(r.fresh && made.kind == Kind::Raised);
        d.raise(raise("Shop", "t2", "tabs or a sidebar"));
        d.sync_prs(&[pr(89, "main", "t3"), pr(88, "dev", "t4")]);
        let st = s.get();
        assert_eq!((st.decisions.len(), st.decisions_count), (2, 2));
        // The merge is older (its PRs were found long ago) and two sessions wait on each: it is first.
        assert_eq!(st.decisions[0]["kind"], "merge");
        let v = &st.decisions[1];
        assert_eq!(
            (v["id"].as_str(), v["kind"].as_str(), v["space"].as_str(), v["state"].as_str()),
            (Some(r.id.as_str()), Some("raised"), Some("Shop"), Some("open"))
        );
        assert_eq!(
            (v["waiting"].as_u64(), v["count"].as_u64(), v["muted"].as_bool(), v["pick"].is_null()),
            (Some(2), Some(2), Some(false), true)
        );
        assert_eq!(
            v["sessions"],
            json!([{ "address": "t1", "name": "s-t1", "waiting": true }, { "address": "t2", "name": "s-t2", "waiting": true }])
        );
        let m = &st.decisions[0];
        assert_eq!(
            m["prs"].as_array().map(|a| a
                .iter()
                .map(|p| (
                    p["repo"].as_str().unwrap(),
                    p["number"].as_u64().unwrap(),
                    p["base"].as_str().unwrap(),
                    p["checks"].as_str().unwrap()
                ))
                .collect::<Vec<_>>()),
            Some(vec![("o/app", 88, "dev", "pass"), ("o/app", 89, "main", "pass")])
        );
        assert_eq!(m["options"][0], json!({ "label": "Merge PR #88 into dev", "why": "checks passing", "pick": true }));
        // The state's JSON has them under these names.
        let json = s.json();
        assert!(json["decisions"].is_array() && json["decisions_count"] == 2);
        // A session works on something else: it no longer waits on the raise.
        d.session_seen("t1", false);
        assert_eq!(s.get().decisions[1]["waiting"], 1);
        // Answered, and the PRs merged: the queue is empty.
        assert_eq!(d.answered(&r.id, "Tabs").map(|d| (d.state, d.answer)), Some((State::Answered, "Tabs".to_string())));
        d.sync_prs(&[Pr { state: "merged".into(), ..pr(89, "main", "t3") }, Pr { state: "merged".into(), ..pr(88, "dev", "t4") }]);
        assert_eq!((s.get().decisions.len(), s.get().decisions_count), (0, 0));
        assert_eq!(d.get(&r.id).map(|d| d.state), Some(State::Answered));
        assert_eq!(d.list(None, true)["closed"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn a_muted_spaces_decisions_stay_out_of_the_main_queue_and_the_count() {
        let s = shared();
        let mut d = Decisions::new(s.clone());
        d.muted = Arc::new(|| vec!["docs".to_string()]);
        let (shop, ..) = d.raise(raise("Shop", "t1", "Tabs or a sidebar?"));
        let (docs, _, told) = d.raise(raise("Docs", "t2", "Rename the guide?"));
        assert!(!told, "a muted space's decision is not said");
        let st = s.get();
        assert_eq!(st.decisions_count, 1, "the orb counts what is outside muted spaces");
        // The state holds them all, each saying whether its space is muted: the space's own view shows its.
        assert_eq!(
            st.decisions.iter().map(|v| (v["id"].as_str().unwrap().to_string(), v["muted"].as_bool().unwrap())).collect::<Vec<_>>().len(),
            2
        );
        assert!(st.decisions.iter().any(|v| v["id"] == docs.id.as_str() && v["muted"] == true));
        assert!(st.decisions.iter().any(|v| v["id"] == shop.id.as_str() && v["muted"] == false));
        // The main list leaves it out; asked for by its space, it is there.
        let main = d.list(None, false);
        assert_eq!((main["count"].as_u64(), main["decisions"][0]["id"].as_str()), (Some(1), Some(shop.id.as_str())));
        let own = d.list(Some("doc"), false);
        assert_eq!(
            (own["space"].as_str(), own["count"].as_u64(), own["decisions"][0]["id"].as_str()),
            (Some("Docs"), Some(1), Some(docs.id.as_str()))
        );
        assert_eq!(d.list(Some("Shop"), false)["count"], 1);
        assert_eq!(d.list(Some("nowhere"), false)["count"], 0);
        // Unmuted: back in the queue and the count.
        d.muted = Arc::new(Vec::new);
        d.change(|_, _| ());
        assert_eq!(s.get().decisions_count, 2);
    }

    #[test]
    fn a_decision_is_told_once_and_a_repeat_only_when_the_tower_tells_everything() {
        let quiet = Decisions::new(shared());
        let (first, _, told) = quiet.raise(raise("Shop", "t1", "Deploy the tickets change?"));
        assert!(first.fresh && told, "the first time: one alert, one sentence");
        // Two more sessions raise the same thing: one decision, three waiting, nothing said again.
        let (again, _, told_again) = quiet.raise(raise("Shop", "t2", "deploy the tickets change"));
        let (third, d, told_third) = quiet.raise(raise("Shop", "t3", "Deploy the tickets change?!"));
        assert_eq!((again.id.as_str(), third.id.as_str(), d.waiting(), d.count), (first.id.as_str(), first.id.as_str(), 3, 3));
        assert!(!told_again && !told_third, "a repeat says nothing at `decisions`");
        assert!(!quiet.tells(std::slice::from_ref(&first.id), true), "nor does a question or a merge that joins it");
        // Something no decision stands for is told as it was before.
        assert!(quiet.tells(&[], true) && !quiet.tells(&[], false) && quiet.tells(&["d000000000000".into()], true));
        // The tower tells everything: the old alert each time, as before there were decisions.
        let loud = Decisions::telling(shared(), || Level::Everything);
        assert!(loud.raise(raise("Shop", "t1", "Deploy the tickets change?")).2);
        let (same, _, told) = loud.raise(raise("Shop", "t2", "Deploy the tickets change?"));
        assert!(!same.fresh && told, "still one decision, and the alert it always made");
        assert_eq!(loud.shared.get().decisions.len(), 1);
        // A failure is the brain's to say then; at a quiet level the tower says it, once.
        let f = || Failure {
            task: "a3.2".into(),
            title: "fix the login".into(),
            space: None,
            who: Some(Who::new("t5", "api")),
            why: "the tests fail".into(),
        };
        assert!(!loud.failed(f()).2);
        assert!(quiet.failed(f()).2 && !quiet.failed(f()).2);
    }

    #[test]
    fn three_sessions_prs_of_one_repo_are_one_merge_said_once() {
        let s = shared();
        let d = Decisions::new(s.clone());
        // Each session's turn ends on its PR, one after the other.
        let mut prs = vec![pr(89, "main", "t1")];
        d.sync_prs(&prs);
        assert!(d.tells_merge(&prs, true), "the first: one alert, one sentence");
        for (n, session) in [(88, "t2"), (90, "t3")] {
            prs.push(pr(n, "dev", session));
            d.sync_prs(&prs);
            assert!(!d.tells_merge(&prs[prs.len() - 1..], true), "another PR of the repo joins it and says nothing");
        }
        let st = s.get();
        assert_eq!((st.decisions.len(), st.decisions_count), (1, 1));
        let v = &st.decisions[0];
        assert_eq!((v["kind"].as_str(), v["waiting"].as_u64(), v["count"].as_u64()), (Some("merge"), Some(3), Some(3)));
        assert_eq!(v["prs"].as_array().map(|a| a.iter().map(|p| p["number"].as_u64().unwrap()).collect::<Vec<_>>()), Some(vec![88, 90, 89]), "into dev before into main");
        assert_eq!(v["title"], "Merge PR #88 into dev, then PR #90 into dev, then PR #89 into main (o/app)");
        // While the tower tells everything, each turn that ends on a PR is said, as before; still one decision.
        let loud = Decisions::telling(shared(), || Level::Everything);
        loud.sync_prs(&prs);
        assert!(loud.tells_merge(&prs[..1], true) && loud.tells_merge(&prs[1..], true) && !loud.tells_merge(&prs, false));
        assert_eq!(loud.shared.get().decisions.len(), 1);
    }

    /// A space goes quiet: what was raised there is over, the PR that waits
    /// stays the user's; closed or archived, the merge goes too.
    #[test]
    fn a_quiet_space_keeps_only_its_merges_and_a_closed_one_nothing() {
        let s = shared();
        let d = Decisions::new(s.clone());
        let (raised, ..) = d.raise(raise("Shop", "t1", "Tabs or a sidebar?"));
        let (other, ..) = d.raise(raise("Docs", "t5", "Rename the guide?"));
        let prs = [pr(88, "dev", "t2")];
        d.sync_prs(&prs);
        let merge = d.for_prs(&prs)[0].clone();
        assert_eq!(s.get().decisions_count, 3);
        // Its sessions end, one by one: the raise goes with its session, the merge stays with nobody waiting.
        d.session_gone("t1");
        d.session_gone("t2");
        assert_eq!(d.get(&raised.id).map(|d| d.state), Some(State::Stopped));
        assert_eq!(d.get(&merge).map(|d| (d.state, d.waiting(), d.closed_ms)), Some((State::Open, 0, None)));
        d.session_gone("t2");
        assert_eq!(s.get().decisions.len(), 2, "told again that it is gone: nothing more");
        // The space is quiet: nothing else was open there, and the merge still is.
        let (late, ..) = d.raise(Raise { detail: String::new(), who: None, ..raise("shop", "t1", "Delete the old staging database?") });
        assert_eq!(d.space_over("Shop", false), 1);
        assert_eq!(d.get(&late.id).map(|d| d.state), Some(State::Stopped));
        assert!(s.get().decisions.iter().any(|v| v["id"] == merge.as_str()) && s.get().decisions_count == 2);
        assert_eq!(d.space_over("Shop", false), 0);
        // Archived or closed: the merge goes too, and doesn't come back while its PR waits.
        assert_eq!(d.space_over("Shop", true), 1);
        assert_eq!(d.get(&merge).map(|d| d.state), Some(State::Stopped));
        d.sync_prs(&prs);
        assert_eq!(s.get().decisions.iter().map(|v| v["id"].as_str().unwrap().to_string()).collect::<Vec<_>>(), std::slice::from_ref(&other.id), "the other space's is untouched");
    }

    #[test]
    fn decisions_come_back_after_a_restart_without_being_said_again() {
        let dir = std::env::temp_dir().join(format!("tower-decisions-daemon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("decisions.json");
        let before = Decisions::at(shared(), path.clone());
        let (r, _, told) = before.raise(raise("Shop", "t1", "Tabs or a sidebar?"));
        before.sync_prs(&[pr(88, "dev", "t2")]);
        assert!(told && path.exists());
        // Another run: the same decisions, in the state at once, and nothing told twice.
        let s = shared();
        let after = Decisions::at(s.clone(), path.clone());
        assert_eq!((s.get().decisions.len(), s.get().decisions_count), (2, 2));
        assert_eq!(after.get(&r.id).map(|d| (d.title, d.told)), Some(("Tabs or a sidebar?".to_string(), true)));
        let (same, _, told) = after.raise(raise("Shop", "t3", "Tabs or a sidebar?"));
        assert_eq!((same.id.as_str(), same.fresh, same.waiting, told), (r.id.as_str(), false, 2, false));
        // What was open at the start was told by the run before: the merge too.
        after.told_all();
        assert!(!after.tells_merge(&[pr(88, "dev", "t2")], true));
        // Its session is gone: the raise is over; the dismissal is kept.
        after.session_gone("t1");
        after.session_gone("t3");
        assert_eq!(after.get(&r.id).map(|d| d.state), Some(State::Stopped));
        let merge = after.for_prs(&[pr(88, "dev", "t2")])[0].clone();
        assert_eq!(after.dismiss(&merge).map(|d| d.state).ok(), Some(State::Dismissed));
        let last = Decisions::at(shared(), path);
        assert!(last.shared.get().decisions.is_empty());
        assert_eq!(last.get(&merge).map(|d| d.state), Some(State::Dismissed));
        last.sync_prs(&[pr(88, "dev", "t2")]);
        assert!(last.shared.get().decisions.is_empty(), "a dismissed merge stays away while its PR waits");
        assert!(!last.tells_merge(&[pr(88, "dev", "t2")], true), "and its session's next turn on it says nothing");
        assert!(
            Decisions::telling(shared(), || Level::Everything).tells_merge(&[pr(88, "dev", "t2")], true),
            "telling everything, as before"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blocked_session_the_tower_cant_read_is_told_once_and_counted() {
        let s = shared();
        let quiet = Decisions::telling(s.clone(), || Level::Decisions);
        assert!(quiet.blocked(Some("Shop".into()), Who::new("t4", "api"), "Asks to use Frobnicate"), "the first time: told");
        assert!(!quiet.blocked(Some("Shop".into()), Who::new("t4", "api"), "Asks to use Frobnicate: step 2"), "a new status line says nothing again");
        let st = s.get();
        assert_eq!((st.decisions.len(), st.decisions_count), (1, 1), "on the orb's count, as the red dot was");
        assert_eq!(st.decisions[0]["kind"], "blocked");
        quiet.session_blocked("t4", true);
        assert_eq!(s.get().decisions_count, 1);
        quiet.session_blocked("t4", false);
        assert_eq!((s.get().decisions.len(), s.get().decisions_count), (0, 0), "answered in its terminal: gone");
        // Answered there, as its line in the record says (the Mac's Record: "You answered").
        let over = quiet.list(None, true)["closed"][0]["id"].as_str().map(String::from).and_then(|id| quiet.get(&id)).unwrap();
        let e = entry_of(&over);
        assert_eq!((e.status.as_deref(), e.source.as_deref(), e.session.as_deref()), (Some("answered"), Some("terminal:t4"), Some("t4")));
        assert_eq!(e.text, "Asks to use Frobnicate: step 2 -> in its own terminal");
        // The tower tells everything: the plain notice each time, as before.
        let loud = Decisions::telling(shared(), || Level::Everything);
        assert!(loud.blocked(None, Who::new("t4", "api"), "Asks to use Frobnicate"));
        assert!(loud.blocked(None, Who::new("t4", "api"), "Asks to use Frobnicate"));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// One real call each (`cargo test live_hold_judge -- --ignored`): a
    /// merged PR read against the words a merge was held with.
    #[test]
    #[ignore]
    fn live_hold_judge() {
        let pr = |n: u64, title: &str, head: &str| Pr { repo: "eyzhub/eyzflow".into(), number: n, title: title.into(), base: "dev".into(), head: head.into(), state: "merged".into(), ..Default::default() };
        let words = "wait for the ingest filters then we go for prod";
        let started = std::time::Instant::now();
        let yes = judge_merged(words, &pr(831, "Ingest page: list filters (Published, Ready to publish, Empty)", "feat/ingest-list-filters"));
        let no = judge_merged(words, &pr(832, "Fix a typo in the README", "chore/readme-typo"));
        eprintln!("took {} ms: the filters PR {yes:?}, the typo PR {no:?}", started.elapsed().as_millis());
        assert_eq!((yes, no), (Some(true), Some(false)));
    }
}
