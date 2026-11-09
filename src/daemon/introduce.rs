//! Introducing related sessions, in the daemon (see `crate::introduce`):
//! looks at Herdr's terminals every 20 seconds (and at once when a Claude
//! session starts or `tower start` made one), and when a space's Claude
//! sessions have a new member, hands each of them the introduction through
//! the send queue as a quiet note: it waits until that session is free, and
//! nobody is interrupted. What was sent is kept in `introductions.json`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::introduce::{self, Book, Member, Settling};

#[derive(Clone)]
pub struct Introducer {
    book: Arc<Mutex<Book>>,
    path: PathBuf,
    settling: Arc<Mutex<BTreeMap<String, Settling>>>,
    /// `hc get` per address (labels, creation time), dropped when the address leaves `hc ls`.
    full: Arc<Mutex<HashMap<String, Value>>>,
    wake: Arc<(Mutex<bool>, Condvar)>,
    sendq: super::sendq::SendQueue,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn path() -> PathBuf {
    crate::config::state_dir().join("introductions.json")
}

/// `[sessions] introduce` (read each time: switching it needs no restart).
fn enabled() -> bool {
    crate::config::Config::load().map(|c| c.sessions.introduce).unwrap_or(true)
}

/// What a session is on now: the agenda's task for it, else the prompt its
/// hooks last saw (unwrapped; an introduction is none).
fn task_now(address: &str) -> Option<String> {
    super::agendas::task_of(address)
        .and_then(|t| introduce::task_from_prompt(&t))
        .or_else(|| super::sessions::registry().get(address).and_then(|s| introduce::task_from_prompt(&s.prompt)))
}

impl Introducer {
    pub fn new(sendq: super::sendq::SendQueue) -> Introducer {
        let path = path();
        Introducer {
            book: Arc::new(Mutex::new(Book::load_from(&path))),
            path,
            settling: Arc::default(),
            full: Arc::default(),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
            sendq,
        }
    }

    pub fn start(&self) {
        let me = self.clone();
        thread::Builder::new()
            .name("introduce".into())
            .spawn(move || {
                // The radar's first look settles first.
                thread::sleep(Duration::from_secs(8));
                loop {
                    if enabled() {
                        me.round();
                    }
                    let (m, cv) = &*me.wake;
                    let woken = lock(m);
                    let mut woken = cv.wait_timeout_while(woken, Duration::from_secs(20), |w| !*w).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
                    *woken = false;
                }
            })
            .ok();
    }

    /// Look now (a session started); a space still settles before it's introduced.
    pub fn kick(&self) {
        let (m, cv) = &*self.wake;
        *lock(m) = true;
        cv.notify_all();
        // Look again once the settle time has passed, so a new pair isn't left for the next round.
        let me = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(introduce::SETTLE_MS + 1_000));
            let (m, cv) = &*me.wake;
            *lock(m) = true;
            cv.notify_all();
        });
    }

    /// Every space's Claude sessions here, as members (None: Herdr couldn't be asked).
    fn scan(&self) -> Option<BTreeMap<String, Vec<Member>>> {
        let list = match crate::hc::list_on(None) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[introduce] hc ls: {e:#}");
                return None;
            }
        };
        let mut full = lock(&self.full);
        let here: std::collections::HashSet<String> = list.iter().filter_map(|t| t["address"].as_str().map(String::from)).collect();
        full.retain(|a, _| here.contains(a));
        let mut members = vec![];
        for t in &list {
            // Only those that may be members get a closer look.
            if introduce::member_of(t, None).is_none() {
                continue;
            }
            let Some(address) = t["address"].as_str() else { continue };
            if !full.contains_key(address)
                && let Some(f) = crate::hc::get(address)
            {
                full.insert(address.to_string(), f);
            }
            let f = full.get(address);
            // The real tower and a test one never introduce each other's sessions.
            if f.is_some_and(super::watch::left_alone) || (f.is_none() && super::watch::left_alone(t)) {
                continue;
            }
            if let Some(m) = introduce::member_of(t, f) {
                members.push(m);
            }
        }
        drop(full);
        let mut groups = introduce::group(members);
        // Each one's task: what it's on now, else the last real task it had.
        let mut b = lock(&self.book);
        if b.tasks_for(&mut groups, |m| task_now(&m.address)) {
            self.save(&b);
        }
        Some(groups)
    }

    fn save(&self, b: &Book) {
        if let Err(e) = b.save_to(&self.path) {
            eprintln!("[introduce] saving {}: {e:#}", self.path.display());
        }
    }

    /// One round: what changed, and the introductions due.
    fn round(&self) {
        let Some(groups) = self.scan() else { return };
        let due = {
            let mut b = lock(&self.book);
            let (due, changed) = introduce::decide(&mut b, &mut lock(&self.settling), &groups, crate::history::now_ms(), introduce::SETTLE_MS);
            if changed {
                self.save(&b);
            }
            due
        };
        for space in due {
            if let Some(ms) = groups.get(&space) {
                self.send(&space, ms);
            }
        }
    }

    /// Hands the introduction to every member (queued: each gets it once it's free), and books it.
    fn send(&self, space: &str, members: &[Member]) -> Vec<Value> {
        let mut out = vec![];
        let mut reached = vec![];
        for m in members {
            let text = introduce::compose(space, m, members);
            match self.sendq.add_note(&m.address, &text) {
                Ok((added, ..)) => {
                    let (id, sent) = match added {
                        super::sendq::Added::Sent { id } => (id, true),
                        super::sendq::Added::Queued { id, .. } => (id, false),
                    };
                    reached.push(m.clone());
                    out.push(json!({ "to": m.address, "name": m.label(), "queue_id": id, "sent": sent }));
                }
                Err(e) => {
                    eprintln!("[introduce] {space}: couldn't queue it for {}: {e:#}", m.address);
                    out.push(json!({ "to": m.address, "name": m.label(), "error": format!("{e:#}") }));
                }
            }
        }
        if !reached.is_empty() {
            let mut b = lock(&self.book);
            b.sent(space, members, crate::history::now_ms());
            self.save(&b);
            let names: Vec<String> = members.iter().map(|m| format!("{} ({})", m.label(), m.address)).collect();
            eprintln!("[introduce] {space}: introduced {}", names.join(", "));
            let (sp, text) = (space.to_string(), format!("Introduced the Claude sessions in {space} to each other: {}", names.join(", ")));
            crate::history::later(move || crate::history::record_space("introduce", &sp, None, &text));
        }
        out
    }

    /// `tower introduce`: every space's Claude sessions and what was sent
    /// (no space); one space's introduction, shown (`dry_run`) or sent again.
    pub fn request(&self, space: &str, dry_run: bool) -> Result<Value> {
        let Some(groups) = self.scan() else { bail!("Herdr couldn't be asked (hc ls)") };
        if space.trim().is_empty() {
            let b = lock(&self.book);
            let spaces: Vec<Value> = groups
                .iter()
                .map(|(sp, ms)| {
                    let k = b.spaces.get(sp).cloned().unwrap_or_default();
                    let now: std::collections::BTreeSet<String> = ms.iter().map(Member::key).collect();
                    json!({
                        "space": sp,
                        "sessions": ms.iter().map(|m| json!({ "address": m.address, "name": m.label(), "folder": m.folder, "task": m.task })).collect::<Vec<_>>(),
                        "introduced_ms": k.sent_ms,
                        "introduced_to": k.sent_to,
                        "up_to_date": now == k.members,
                    })
                })
                .collect();
            return Ok(json!({ "enabled": enabled(), "spaces": spaces }));
        }
        let names: Vec<String> = groups.keys().cloned().collect();
        let Some(sp) = crate::spaces::resolve(space, &names) else {
            bail!("no space {space} with Claude sessions (spaces with some: {})", if names.is_empty() { "none".into() } else { names.join(", ") })
        };
        let members = &groups[&sp];
        if members.len() < 2 {
            bail!("{sp} has one Claude session: nobody to introduce it to");
        }
        if dry_run {
            let messages: Vec<Value> = members.iter().map(|m| json!({ "to": m.address, "name": m.label(), "text": introduce::compose(&sp, m, members) })).collect();
            return Ok(json!({ "space": sp, "dry_run": true, "messages": messages }));
        }
        Ok(json!({ "space": sp, "dry_run": false, "sent": self.send(&sp, members) }))
    }
}
