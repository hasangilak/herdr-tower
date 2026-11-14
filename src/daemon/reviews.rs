//! The reviews, in the daemon (see `crate::reviews`): a PR found in a space
//! with review on opens its reviewer (`tower start`, in a checkout of the
//! PR's head of its own), the reviewer's and the owner's `tower review`
//! commands move it, and every 20 seconds (and at once when a space's
//! review is turned off) each review not over is looked at: its PR merged
//! or closed, its space closed or its review off, its reviewer gone. What
//! ends a review ends its reviewer session (`hc kill`, `hc rm`) and takes
//! its checkout away; the record stays in `reviews.json`.
//!
//! The tower passes nothing between the two sessions: they talk directly
//! (`tower queue`). It keeps the record, sends the owner one short note per
//! round that found something critical, and puts a review stuck after
//! three rounds to the user, as one decision about both sessions.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::state::Shared;
use crate::prs::{Pr, Prs};
use crate::reviews::{self, Book, Review};

/// A reviewer that hasn't opened this long after its review began never will.
const OPEN_MS: u64 = 5 * 60_000;

/// A space as the reviews need it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpaceNow {
    /// Its PRs get a reviewer (`missions::reviews_in`).
    pub review: bool,
    /// It is closed or archived.
    pub closed: bool,
    pub goal: String,
    pub done_when: String,
}

/// What the reviews reach outside their store: Herdr, `tower start`, the
/// send queue, the decisions, the record (`Real` in the daemon).
pub trait Hands: Send + Sync {
    /// A terminal as `hc get` has it.
    fn terminal(&self, address: &str) -> Option<Value>;
    /// Whether that terminal is live (and, with `created_ms`, still the
    /// same one). None: Herdr couldn't say.
    fn alive(&self, address: &str, created_ms: Option<u64>) -> Option<bool>;
    /// The live terminals' names here (None: Herdr couldn't be asked).
    fn names(&self) -> Option<Vec<(String, String)>>;
    fn space(&self, space: &str) -> SpaceNow;
    /// `tower start` with these arguments: what it printed.
    fn start(&self, args: &[String]) -> Result<Value>;
    fn label(&self, address: &str, labels: &[String]);
    /// Ends a terminal (`hc kill`, then `hc rm`).
    fn end(&self, address: &str);
    /// A quiet note to a session, through the send queue.
    fn note(&self, address: &str, text: &str) -> Result<()>;
    /// The decision for a stuck review, about both sessions: its id.
    fn raise(&self, r: &Review) -> Option<String>;
    fn decision_open(&self, id: &str) -> bool;
    /// The decision isn't needed any more (the review is over).
    fn settle(&self, id: &str);
    /// A line in the record (history kind `review`).
    fn record(&self, r: &Review, text: &str);
}

#[derive(Clone)]
pub struct Reviews {
    book: Arc<Mutex<Book>>,
    path: PathBuf,
    /// Where a reviewer works when its PR's repo can't be checked out here.
    dir: PathBuf,
    prs: Prs,
    shared: Option<Shared>,
    hands: Arc<dyn Hands>,
    /// Reviews whose reviewer is being opened right now.
    opening: Arc<Mutex<HashSet<String>>>,
    /// The heavy parts (opening, ending) run on threads of their own; tests run them in place.
    background: bool,
}

static RUNNING: OnceLock<Reviews> = OnceLock::new();

/// The daemon's, once it runs.
pub fn running() -> Option<&'static Reviews> {
    RUNNING.get()
}

/// `address` is a reviewer's terminal (the daemon's store says so).
pub fn is_reviewer(address: &str) -> bool {
    running().is_some_and(|r| r.reviewer_at(address).is_some())
}

/// What a reviewer is on, for the introductions: "reviewing PR #12 …".
pub fn task_of(address: &str) -> Option<String> {
    let r = running()?.reviewer_at(address)?;
    Some(format!("reviewing {} for critical issues only: {}", r.short(), r.url))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The repo a session works in: the one the tower gave it a worktree of,
/// else the checkout its folder is in (a worktree's own repo).
pub fn repo_of_terminal(t: &Value) -> Option<PathBuf> {
    if let Some(repo) = crate::worktrees::given(t).repo {
        return Some(PathBuf::from(repo));
    }
    let cwd = t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str())?;
    let top = crate::worktrees::toplevel(Path::new(cwd))?;
    Some(crate::worktrees::repo_of(&top).map(|(r, _)| r).unwrap_or(top))
}

/// The Claude account a session runs on: its process's (`accounts`), else
/// the one its command line names (`cca <account> …`, as `tower start` runs it).
pub fn account_of_terminal(t: &Value) -> Option<String> {
    crate::accounts::of_terminal(t).or_else(|| {
        let argv: Vec<&str> = t["argv"].as_array()?.iter().filter_map(Value::as_str).collect();
        let at = argv.iter().position(|a| a.rsplit('/').next() == Some("cca"))?;
        argv.get(at + 1).filter(|a| !a.starts_with('-')).map(|a| a.to_string())
    })
}

/// A reviewer's terminal, by its label.
pub fn labelled_reviewer(t: &Value) -> bool {
    t["labels"][reviews::LABEL_ROLE].as_str() == Some("reviewer")
}

impl Reviews {
    pub fn open(prs: Prs, shared: Shared, hands: Arc<dyn Hands>) -> Reviews {
        let mut r = Reviews::at(reviews::path(), crate::config::state_dir().join("reviews"), prs, Some(shared), hands);
        r.background = true;
        r
    }

    pub fn at(path: PathBuf, dir: PathBuf, prs: Prs, shared: Option<Shared>, hands: Arc<dyn Hands>) -> Reviews {
        Reviews {
            book: Arc::new(Mutex::new(Book::load_from(&path))),
            path,
            dir,
            prs,
            shared,
            hands,
            opening: Arc::default(),
            background: false,
        }
    }

    /// From now on: the state carries the reviews, each PR its review's
    /// word, and every 20 seconds each review not over is looked at.
    pub fn start(&self) {
        let _ = RUNNING.set(self.clone());
        let keys: Vec<String> = lock(&self.book).reviews.iter().map(|r| r.key.clone()).collect();
        for k in &keys {
            self.publish(k);
        }
        let me = self.clone();
        thread::Builder::new()
            .name("reviews".into())
            .spawn(move || {
                // The radar's first look settles first.
                thread::sleep(Duration::from_secs(10));
                loop {
                    let ended = me.sweep();
                    if !ended.is_empty() {
                        eprintln!("[reviews] ended: {}", ended.join(", "));
                    }
                    thread::sleep(Duration::from_secs(20));
                }
            })
            .ok();
    }

    fn save(&self, b: &Book) {
        if let Err(e) = b.save_to(&self.path) {
            eprintln!("[reviews] saving {}: {e:#}", self.path.display());
        }
    }

    /// The PR's chip and the state say where `key`'s review stands.
    fn publish(&self, key: &str) {
        let now = crate::history::now_ms();
        let (brief, views) = {
            let b = lock(&self.book);
            (b.get(key).map(Review::brief), b.views(now))
        };
        if brief.is_some() {
            self.prs.set_review(key, brief);
        }
        if let Some(s) = &self.shared {
            s.update(|st| st.reviews = views);
        }
    }

    /// The review whose reviewer is at `address` now (not over).
    pub fn reviewer_at(&self, address: &str) -> Option<Review> {
        lock(&self.book).active().find(|r| r.reviewer == address).cloned()
    }

    pub fn get(&self, key: &str) -> Option<Review> {
        lock(&self.book).get(key).cloned()
    }

    fn run(&self, f: impl FnOnce() + Send + 'static) {
        if self.background {
            thread::spawn(f);
        } else {
            f();
        }
    }

    /// A PR changed (`prs::OnChange`): one found now may get its reviewer;
    /// one that closed ends its review.
    pub fn pr_changed(&self, op: &str, pr: &Pr) {
        match op {
            "open" => {
                if let Some(r) = self.found(pr) {
                    let me = self.clone();
                    self.run(move || me.open_reviewer(&r.key));
                }
            }
            "closed" => {
                let why = if pr.state == "merged" { "the PR was merged" } else { "the PR was closed" };
                let (me, key) = (self.clone(), pr.key());
                self.run(move || drop(me.end_review(&key, why)));
            }
            _ => {}
        }
    }

    /// A PR found now: its review begins, when it is to have one (kept
    /// before its reviewer opens: never two for one PR).
    pub fn found(&self, pr: &Pr) -> Option<Review> {
        let space = pr.space.clone().unwrap_or_default();
        let on = !space.trim().is_empty() && self.hands.space(&space).review;
        let owner = self.hands.terminal(&pr.session);
        let by_reviewer = owner.as_ref().is_some_and(labelled_reviewer);
        let mut b = lock(&self.book);
        let by_reviewer = by_reviewer || b.is_reviewer(&pr.session, owner.as_ref().and_then(crate::hc::created_ms));
        if let Err(why) = b.wants_reviewer(pr, on, by_reviewer) {
            eprintln!("[reviews] {}: no reviewer ({why})", pr.key());
            return None;
        }
        // Its session's account, unless that one is out of usage (then the default `tower start` picks).
        let account = owner.as_ref().and_then(account_of_terminal).filter(|a| crate::failover::usable(a));
        let r = b.begin(pr, account, owner.as_ref().and_then(crate::hc::created_ms), crate::history::now_ms());
        lock(&self.opening).insert(r.key.clone());
        self.save(&b);
        drop(b);
        eprintln!("[reviews] {}: a reviewer opens for {} ({}) in {space}", r.key, pr.name, pr.session);
        self.publish(&r.key);
        Some(r)
    }

    /// Opens the reviewer of `key`: its checkout, its name, `tower start`.
    pub fn open_reviewer(&self, key: &str) {
        let res = self.try_open(key);
        lock(&self.opening).remove(key);
        if let Err(e) = res {
            let why = format!("its reviewer didn't open: {e:#}");
            eprintln!("[reviews] {key}: {why}");
            self.end_review(key, &why);
        }
    }

    fn try_open(&self, key: &str) -> Result<()> {
        let Some(r) = self.get(key) else { bail!("no review of {key}") };
        let owner = self.hands.terminal(&r.owner);
        let taken: Vec<String> = self.hands.names().unwrap_or_default().into_iter().map(|(_, n)| n).collect();
        let name = reviews::reviewer_name(&r.space, &r.repo, r.number, &taken);
        // Its own checkout of the PR's head, beside the space's worktrees; else a folder of its own.
        let repo = owner.as_ref().and_then(repo_of_terminal);
        let made = repo.as_ref().map(|repo| {
            let place = reviews::checkout_place(repo, &r.space, r.number);
            crate::worktrees::pr_commit(repo, &r.repo, r.number, &r.head, &r.head_sha).and_then(|rev| crate::worktrees::add_detached(repo, &place, &rev)).map(|_| {
                // Its dependencies, to run the tests with: linked by the tower, taken away with the checkout.
                crate::worktrees::link_dependencies(repo, &place);
                (place, repo.clone())
            })
        });
        let (folder, of) = match made {
            Some(Ok((place, repo))) => (place, repo.to_string_lossy().into_owned()),
            other => {
                if let Some(Err(e)) = other {
                    eprintln!("[reviews] {key}: no checkout of the PR ({e:#}); it reads the PR with gh");
                }
                let dir = self.dir.join(&name);
                std::fs::create_dir_all(&dir)?;
                (dir, String::new())
            }
        };
        // What it may do there: read and comment; never push, merge or edit.
        let settings = folder.join(".claude/settings.local.json");
        if !settings.exists() {
            std::fs::create_dir_all(folder.join(".claude"))?;
            std::fs::write(&settings, serde_json::to_vec_pretty(&reviews::reviewer_settings())?)?;
        }
        let folder_s = folder.to_string_lossy().into_owned();
        let r = {
            let mut b = lock(&self.book);
            let r = b.placed(key, &name, &folder_s, &of);
            self.save(&b);
            r
        };
        let Some(r) = r else { bail!("its review is gone") };
        // Over while its checkout was made (its PR closed, its space's review turned off).
        if r.status.over() {
            self.take_checkout(&r);
            return Ok(());
        }
        let sp = self.hands.space(&r.space);
        let task = reviews::instructions(&r, &sp.goal, &sp.done_when, !of.is_empty());
        let mut args: Vec<String> = ["start", "--no-act", "--no-watch", "--no-track", "--no-worktree", "--name", &name, "--cwd", &folder_s, "--space", &r.space].map(String::from).to_vec();
        if let Some(a) = &r.account {
            args.extend(["--account".into(), a.clone()]);
        }
        args.push(task);
        let v = match self.hands.start(&args) {
            Ok(v) => v,
            Err(e) => {
                self.take_checkout(&r);
                return Err(e);
            }
        };
        let Some(address) = v["address"].as_str().filter(|a| !a.is_empty()).map(String::from) else {
            self.take_checkout(&r);
            bail!("tower start printed no address ({v})");
        };
        let created = v["created_ms"].as_u64().or_else(|| self.hands.terminal(&address).as_ref().and_then(crate::hc::created_ms));
        self.hands.label(&address, &[format!("{}=reviewer", reviews::LABEL_ROLE), format!("{}={key}", reviews::LABEL_REVIEW)]);
        let r = {
            let mut b = lock(&self.book);
            let r = b.started(key, &address, created, crate::history::now_ms());
            self.save(&b);
            r
        };
        // It may have ended meanwhile (the PR closed while it opened).
        if let Some(r) = r {
            eprintln!("[reviews] {key}: {name} ({address}) reviews it in {folder_s}");
            self.hands.record(&r, &format!("{name} ({address}) reviews {} for critical issues, for {} ({})", r.short(), r.owner_name, r.owner));
            if r.status.over() {
                self.retire(&r);
            }
        }
        self.publish(key);
        Ok(())
    }

    /// Ends `key`'s review without agreement (`why`), its reviewer and its checkout.
    pub fn end_review(&self, key: &str, why: &str) -> Option<Review> {
        let r = {
            let mut b = lock(&self.book);
            let r = b.end(key, why, crate::history::now_ms())?;
            self.save(&b);
            r
        };
        eprintln!("[reviews] {key}: ended ({why})");
        self.hands.record(&r, &format!("The review of {} ended: {why}", r.short()));
        self.publish(key);
        if !lock(&self.opening).contains(key) {
            self.retire(&r);
        }
        Some(r)
    }

    /// The reviewer session ends, its checkout goes, and a decision about it is settled.
    fn retire(&self, r: &Review) {
        if let Some(id) = r.decision.as_deref().filter(|id| self.hands.decision_open(id)) {
            self.hands.settle(id);
        }
        if !r.reviewer.is_empty() && self.hands.alive(&r.reviewer, r.reviewer_created_ms) == Some(true) {
            self.hands.end(&r.reviewer);
        }
        self.take_checkout(r);
    }

    /// A reviewer's checkout (or its own folder) is taken away.
    fn take_checkout(&self, r: &Review) {
        let folder = Path::new(&r.folder);
        if r.folder.is_empty() || !folder.exists() {
            return;
        }
        if !r.checkout_of.is_empty() {
            if let Err(e) = crate::worktrees::remove_detached(Path::new(&r.checkout_of), folder) {
                eprintln!("[reviews] {}: its checkout stays: {e:#}", r.key);
            }
        } else if folder.starts_with(&self.dir) {
            let _ = std::fs::remove_dir_all(folder);
        }
    }

    /// Every review not over, looked at: its PR closed, its space closed or
    /// its review off, its reviewer gone (or never opened). The keys ended.
    pub fn sweep(&self) -> Vec<String> {
        let now = crate::history::now_ms();
        let active: Vec<Review> = lock(&self.book).active().cloned().collect();
        let opening = lock(&self.opening).clone();
        let mut ended = vec![];
        for r in active {
            let sp = self.hands.space(&r.space);
            let pr = self.prs.get(&r.key);
            let why = match () {
                _ if pr.as_ref().is_some_and(|p| p.state == "merged") => Some("the PR was merged".to_string()),
                _ if pr.as_ref().is_some_and(|p| p.state == "closed") => Some("the PR was closed".to_string()),
                _ if sp.closed => Some(format!("the space {} was closed", r.space)),
                _ if !sp.review => Some(format!("review was turned off for {}", r.space)),
                _ if opening.contains(&r.key) => None,
                // Nobody is left to fix what it finds (the PR stays the user's to merge).
                _ if self.hands.alive(&r.owner, r.owner_created_ms) == Some(false) => Some("the PR's own session ended".to_string()),
                _ if r.reviewer.is_empty() => self.unopened(&r, now),
                _ => match self.hands.alive(&r.reviewer, r.reviewer_created_ms) {
                    Some(false) => Some("its reviewer session ended".to_string()),
                    _ => None,
                },
            };
            if let Some(why) = why
                && self.end_review(&r.key, &why).is_some()
            {
                ended.push(r.key.clone());
            }
        }
        ended
    }

    /// A review with no reviewer and none opening (the tower restarted while
    /// it opened): one that did open under its name is taken; else, a while
    /// on, it never will.
    fn unopened(&self, r: &Review, now: u64) -> Option<String> {
        if !r.reviewer_name.is_empty()
            && let Some((address, _)) = self.hands.names().unwrap_or_default().into_iter().find(|(_, n)| *n == r.reviewer_name)
        {
            let created = self.hands.terminal(&address).as_ref().and_then(crate::hc::created_ms);
            let mut b = lock(&self.book);
            b.started(&r.key, &address, created, now);
            self.save(&b);
            drop(b);
            self.publish(&r.key);
            return None;
        }
        (now.saturating_sub(r.created_ms) >= OPEN_MS).then(|| "its reviewer didn't open".to_string())
    }

    /// `tower review …`: `list`, `show`, `report`, `agree`, `fixed`, `sweep`.
    pub fn request(&self, action: &str, pr: &str, critical: Option<u32>, text: &str, from: Option<&str>) -> Result<Value> {
        let now = crate::history::now_ms();
        let by = from.map(str::trim).filter(|f| !f.is_empty()).unwrap_or("");
        let find = |b: &Book| -> Result<String> { Ok(b.find(pr, Some(by).filter(|b| !b.is_empty()))?.key.clone()) };
        match action {
            "" | "list" => {
                let b = lock(&self.book);
                let mut list: Vec<&Review> = b.reviews.iter().collect();
                list.sort_by_key(|r| (r.status.over(), std::cmp::Reverse(r.updated_ms)));
                Ok(json!({ "reviews": list.iter().map(|r| r.view()).collect::<Vec<_>>() }))
            }
            "show" => {
                let b = lock(&self.book);
                let key = find(&b)?;
                Ok(json!({ "review": b.get(&key).map(Review::view) }))
            }
            "sweep" => Ok(json!({ "ended": self.sweep() })),
            "report" => {
                let Some(n) = critical else { bail!("tower review report <pr> --critical <n> \"<one line>\"") };
                let (rep, key) = {
                    let mut b = lock(&self.book);
                    let key = find(&b)?;
                    let open = b.get(&key).and_then(|r| r.decision.clone()).is_some_and(|id| self.hands.decision_open(&id));
                    let rep = b.report(&key, n, text, by, open, now)?;
                    self.save(&b);
                    (rep, key)
                };
                let r = &rep.review;
                self.hands.record(r, &format!("{} found {n} critical issue{} in {} (round {}): {}", r.reviewer_name, if n == 1 { "" } else { "s" }, r.short(), r.rounds, r.line));
                let mut noted = false;
                if rep.note_owner {
                    match self.hands.note(&r.owner, &reviews::owner_note(r)) {
                        Ok(()) => noted = true,
                        Err(e) => eprintln!("[reviews] {key}: couldn't note its owner {}: {e:#}", r.owner),
                    }
                }
                let mut decision = r.decision.clone();
                if rep.raise
                    && let Some(id) = self.hands.raise(r)
                {
                    let mut b = lock(&self.book);
                    b.decided(&key, &id);
                    self.save(&b);
                    decision = Some(id);
                }
                self.publish(&key);
                let r = self.get(&key).unwrap_or(rep.review.clone());
                Ok(json!({ "review": r.view(), "noted_owner": noted, "decision": decision }))
            }
            "fixed" => {
                let r = {
                    let mut b = lock(&self.book);
                    let key = find(&b)?;
                    let r = b.fixed(&key, text, by, now)?;
                    self.save(&b);
                    r
                };
                self.hands.record(&r, &format!("{} says the critical issues in {} are fixed: {}", r.owner_name, r.short(), r.fixed));
                self.publish(&r.key);
                Ok(json!({ "review": r.view() }))
            }
            "agree" => {
                let r = {
                    let mut b = lock(&self.book);
                    let key = find(&b)?;
                    let r = b.agree(&key, text, by, now)?;
                    self.save(&b);
                    r
                };
                self.hands.record(&r, &format!("{} and {} agree on {}: {}", r.reviewer_name, r.owner_name, r.short(), r.line));
                self.publish(&r.key);
                // Its reviewer's work is done: it ends (after this answer reached it).
                let (me, done) = (self.clone(), r.clone());
                self.run(move || {
                    if me.background {
                        thread::sleep(Duration::from_secs(3));
                    }
                    me.retire(&done);
                });
                Ok(json!({ "review": r.view(), "ends": r.reviewer }))
            }
            other => bail!("tower review {other}: use list, show <pr>, report <pr> --critical <n> \"<line>\", agree <pr> [\"<line>\"] or fixed <pr> \"<line>\""),
        }
    }
}

/// The daemon's hands: Herdr, this program's `tower start`, the send queue,
/// the decisions and the radar's alerts, the history.
pub struct Real {
    pub sendq: super::sendq::SendQueue,
    pub decisions: super::decisions::Decisions,
    pub watches: super::watch::Watches,
}

impl Hands for Real {
    fn terminal(&self, address: &str) -> Option<Value> {
        crate::hc::get(address)
    }

    fn alive(&self, address: &str, created_ms: Option<u64>) -> Option<bool> {
        match crate::hc::lookup(address) {
            crate::hc::Lookup::Found(t) => Some(t["state"] == "live" && created_ms.is_none_or(|c| crate::hc::created_ms(&t).is_none_or(|x| x == c))),
            crate::hc::Lookup::Gone => Some(false),
            crate::hc::Lookup::Unreachable(_) => None,
        }
    }

    fn names(&self) -> Option<Vec<(String, String)>> {
        let list = crate::hc::list_on(None).ok()?;
        Some(list.iter().filter_map(|t| Some((t["address"].as_str()?.to_string(), crate::hc::name(t)?.to_string()))).collect())
    }

    fn space(&self, space: &str) -> SpaceNow {
        let s = crate::spaces::load();
        let m = crate::missions::of(&s, space).map(|(_, m)| m.clone());
        let goal = s.agendas.iter().find(|(k, _)| k.eq_ignore_ascii_case(space.trim())).map(|(_, g)| g.trim().to_string()).unwrap_or_default();
        SpaceNow {
            review: crate::missions::reviews_in(&s, space),
            closed: m.as_ref().is_some_and(|m| m.state == crate::missions::State::Closed || m.archived_ms.is_some()),
            goal,
            done_when: m.map(|m| m.done_when).unwrap_or_default(),
        }
    }

    fn start(&self, args: &[String]) -> Result<Value> {
        let mut cmd = std::process::Command::new(crate::config::tower_bin());
        cmd.args(args);
        let out = crate::hc::output_within(cmd, "tower start", Duration::from_secs(180))?;
        if out.code != 0 {
            bail!("{}{}", out.stderr.trim().trim_start_matches("tower: "), out.stdout.trim());
        }
        serde_json::from_str(out.stdout.trim()).map_err(|_| anyhow::anyhow!("tower start printed {}", out.stdout.trim()))
    }

    fn label(&self, address: &str, labels: &[String]) {
        let mut args = vec!["label", address];
        args.extend(labels.iter().map(String::as_str));
        let _ = crate::hc::run(&args);
    }

    fn end(&self, address: &str) {
        let _ = crate::hc::run(&["kill", address]);
        let _ = crate::hc::run(&["rm", address]);
        eprintln!("[reviews] ended the reviewer {address}");
    }

    fn note(&self, address: &str, text: &str) -> Result<()> {
        self.sendq.add_note(address, text).map(|_| ())
    }

    fn raise(&self, r: &Review) -> Option<String> {
        let (title, detail, options) = reviews::stuck_decision(r);
        let who = |address: &str, name: &str| -> Option<crate::decisions::Who> {
            let t = crate::hc::get(address).filter(|t| t["state"] == "live")?;
            let name = if name.is_empty() { crate::hc::spoken_name(&t) } else { name.to_string() };
            Some(crate::decisions::Who::new(address, &name))
        };
        let base = crate::decisions::Raise { space: Some(r.space.clone()), who: who(&r.owner, &r.owner_name), why: "review".into(), text: title.clone(), detail, options };
        let (raised, _, told) = self.decisions.raise(base.clone());
        // The reviewer is in it too: the answer goes to both.
        if let Some(w) = who(&r.reviewer, &r.reviewer_name) {
            self.decisions.raise(crate::decisions::Raise { who: Some(w), ..base });
        }
        let entry = crate::history::Entry {
            kind: "raised".into(),
            id: Some(raised.id.clone()),
            session: Some(r.owner.clone()),
            name: Some(r.owner_name.clone()).filter(|n| !n.is_empty()),
            space: Some(r.space.clone()),
            why: Some("review".into()),
            text: title.clone(),
            record: !raised.fresh,
            ..Default::default()
        };
        crate::history::later(move || crate::history::append(entry));
        if told && raised.fresh {
            self.watches.raise(&r.owner, &crate::decisions::said(&r.owner_name, &title, None));
        }
        eprintln!("[reviews] {}: stuck after {} rounds: {} for the user", r.key, r.rounds, raised.id);
        Some(raised.id)
    }

    fn decision_open(&self, id: &str) -> bool {
        self.decisions.get(id).is_some_and(|d| d.closed_ms.is_none())
    }

    fn settle(&self, id: &str) {
        let _ = self.decisions.dismiss(id);
    }

    fn record(&self, r: &Review, text: &str) {
        let entry = crate::history::Entry {
            kind: "review".into(),
            id: Some(r.key.clone()),
            session: Some(if r.reviewer.is_empty() { r.owner.clone() } else { r.reviewer.clone() }),
            name: Some(if r.reviewer_name.is_empty() { r.owner_name.clone() } else { r.reviewer_name.clone() }).filter(|n| !n.is_empty()),
            space: Some(r.space.clone()),
            status: Some(r.status.word().into()),
            text: text.to_string(),
            record: true,
            ..Default::default()
        };
        crate::history::later(move || crate::history::append(entry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prs::tests::{fake_gh, serve};

    /// Hands that open nothing: what was asked is written down.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        started: Mutex<Vec<Vec<String>>>,
        notes: Mutex<Vec<(String, String)>>,
        off: Mutex<HashSet<String>>,
        closed: Mutex<HashSet<String>>,
        gone: Mutex<HashSet<String>>,
        owner: Mutex<Value>,
        open_decisions: Mutex<HashSet<String>>,
    }

    impl Hands for Fake {
        fn terminal(&self, address: &str) -> Option<Value> {
            match address {
                "t9" => Some(json!({ "address": "t9", "created_ms": 900, "labels": { "tower.role": "reviewer" } })),
                _ => Some(lock(&self.owner).clone()),
            }
        }
        fn alive(&self, address: &str, _: Option<u64>) -> Option<bool> {
            Some(!lock(&self.gone).contains(address))
        }
        fn names(&self) -> Option<Vec<(String, String)>> {
            Some(vec![("t3".into(), "fix-login-shop".into())])
        }
        fn space(&self, space: &str) -> SpaceNow {
            SpaceNow { review: !lock(&self.off).contains(space), closed: lock(&self.closed).contains(space), goal: "Fix the login redirect".into(), done_when: String::new() }
        }
        fn start(&self, args: &[String]) -> Result<Value> {
            lock(&self.started).push(args.to_vec());
            Ok(json!({ "address": "t9", "created_ms": 900 }))
        }
        fn label(&self, address: &str, labels: &[String]) {
            lock(&self.calls).push(format!("label {address} {}", labels.join(" ")));
        }
        fn end(&self, address: &str) {
            lock(&self.calls).push(format!("end {address}"));
        }
        fn note(&self, address: &str, text: &str) -> Result<()> {
            lock(&self.notes).push((address.into(), text.into()));
            Ok(())
        }
        fn raise(&self, r: &Review) -> Option<String> {
            lock(&self.calls).push(format!("raise {} {} {}", r.key, r.owner, r.reviewer));
            lock(&self.open_decisions).insert("d000000000001".into());
            Some("d000000000001".into())
        }
        fn decision_open(&self, id: &str) -> bool {
            lock(&self.open_decisions).contains(id)
        }
        fn settle(&self, id: &str) {
            lock(&self.calls).push(format!("settle {id}"));
            lock(&self.open_decisions).remove(id);
        }
        fn record(&self, _: &Review, text: &str) {
            lock(&self.calls).push(format!("record {text}"));
        }
    }

    fn setup(name: &str) -> (Reviews, Arc<Fake>, Prs, PathBuf) {
        let base = crate::worktrees::tests::temp(&format!("reviews-{name}"));
        let repo = crate::worktrees::tests::repo(&base, "shop");
        let sha = crate::worktrees::tests::sh(&repo, &["rev-parse", "HEAD"]);
        let gh = fake_gh(&format!("reviews-{name}"));
        serve(&gh, 12, "OPEN", json!([]), &sha);
        let prs = Prs::at(base.join("prs.json"), gh.clone());
        let hands = Arc::new(Fake::default());
        *lock(&hands.owner) = json!({ "address": "t3", "created_ms": 300, "cwd": repo.to_string_lossy(), "argv": ["/bin/zsh", "-lic", "x", "/usr/local/bin/cca", "orca", "-n", "fix-login-shop", "Fix it"] });
        let reviews = Reviews::at(base.join("reviews.json"), base.join("own"), prs.clone(), None, hands.clone());
        (reviews, hands, prs, base)
    }

    /// The PR a session's turn printed: found, and told to the reviews.
    fn found(prs: &Prs, reviews: &Reviews, session: &str, space: &str) {
        let r2 = reviews.clone();
        prs.on_change(Box::new(move |op, pr| r2.pr_changed(op, pr)));
        prs.found(&[("o/app".into(), 12)], session, "shop", Some(space), true);
    }

    #[test]
    fn a_pr_found_opens_one_reviewer_in_a_checkout_of_its_head() {
        let (reviews, hands, prs, base) = setup("open");
        found(&prs, &reviews, "t3", "fix-login");
        let started = lock(&hands.started).clone();
        assert_eq!(started.len(), 1, "one reviewer");
        let args = &started[0];
        let place = base.join(".tower-worktrees/fix-login/review-shop-12");
        let want: Vec<String> = ["start", "--no-act", "--no-watch", "--no-track", "--no-worktree", "--name", "fix-login-review-app-12", "--cwd", &place.to_string_lossy(), "--space", "fix-login"]
            .map(String::from)
            .to_vec();
        assert_eq!(&args[..want.len()], &want[..], "{args:?}");
        let task = args.last().unwrap();
        assert!(task.contains("You are the reviewer of PR #12 in o/app") && task.contains("CRITICAL issues only") && task.contains("tower queue t3"), "{task}");
        assert!(task.contains("The space's goal: \"Fix the login redirect\""));
        // A checkout detached at the PR's head, that may not push or edit.
        let head = crate::worktrees::tests::sh(&place, &["rev-parse", "HEAD"]);
        assert_eq!(head, crate::worktrees::tests::sh(&base.join("shop"), &["rev-parse", "HEAD"]));
        assert_eq!(crate::worktrees::tests::sh(&place, &["rev-parse", "--abbrev-ref", "HEAD"]), "HEAD", "detached: on no branch");
        let s: Value = serde_json::from_str(&std::fs::read_to_string(place.join(".claude/settings.local.json")).unwrap()).unwrap();
        assert!(s["permissions"]["deny"].as_array().unwrap().iter().any(|d| d == "Bash(git push:*)"));
        // Labelled as a reviewer, kept, and on the PR's chip.
        assert!(lock(&hands.calls).iter().any(|c| c == "label t9 tower.role=reviewer tower.review=o/app#12"));
        let r = reviews.get("o/app#12").unwrap();
        assert_eq!((r.reviewer.as_str(), r.reviewer_name.as_str(), r.owner.as_str(), r.checkout_of.is_empty()), ("t9", "fix-login-review-app-12", "t3", false));
        // On the owner's account.
        assert_eq!(r.account.as_deref(), Some("orca"));
        let at = args.iter().position(|a| a == "--account").expect("--account");
        assert_eq!(args[at + 1], "orca");
        assert_eq!(prs.get("o/app#12").unwrap().review.unwrap().line, "Reviewing…");
        assert!(reviews.reviewer_at("t9").is_some() && reviews.reviewer_at("t3").is_none());
        // Found again (its turn ends on it once more): no second reviewer.
        prs.found(&[("o/app".into(), 12)], "t3", "shop", Some("fix-login"), true);
        assert!(reviews.found(&prs.get("o/app#12").unwrap()).is_none());
        assert_eq!(lock(&hands.started).len(), 1);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn no_reviewer_where_review_is_off_nor_for_a_reviewers_own_pr() {
        let (reviews, hands, prs, base) = setup("off");
        lock(&hands.off).insert("fix-login".into());
        found(&prs, &reviews, "t3", "fix-login");
        assert!(lock(&hands.started).is_empty() && reviews.get("o/app#12").is_none());
        assert!(prs.get("o/app#12").unwrap().review.is_none(), "no review on its chip");
        let (reviews, hands, prs, base2) = setup("own");
        // t9 is labelled a reviewer: a PR it opens gets none.
        found(&prs, &reviews, "t9", "fix-login");
        assert!(lock(&hands.started).is_empty() && reviews.get("o/app#12").is_none());
        let _ = std::fs::remove_dir_all(base);
        let _ = std::fs::remove_dir_all(base2);
    }

    #[test]
    fn reports_note_the_owner_raise_once_when_stuck_and_agreement_ends_the_reviewer() {
        let (reviews, hands, prs, base) = setup("rounds");
        found(&prs, &reviews, "t3", "fix-login");
        let v = reviews.request("report", "o/app#12", Some(2), "a crash on empty carts; SQL injection", Some("t9")).unwrap();
        assert_eq!((v["noted_owner"].as_bool(), v["review"]["status"].as_str(), v["review"]["chip"].as_str()), (Some(true), Some("changes"), Some("Review: 2 critical, round 1")));
        let notes = lock(&hands.notes).clone();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].0, "t3");
        assert!(notes[0].1.contains("left 2 critical comments on PR #12 (o/app)") && !notes[0].1.contains("SQL"), "{}", notes[0].1);
        assert_eq!(prs.get("o/app#12").unwrap().review.unwrap().line, "Review: 2 critical, round 1");
        assert!(crate::prs::gate(&prs.get("o/app#12").unwrap()).warnings.iter().any(|w| w.starts_with("The review isn't agreed")), "the merge card says so");
        reviews.request("fixed", "#12", None, "fixed both", Some("t3")).unwrap();
        reviews.request("report", "#12", Some(1), "still one", Some("t9")).unwrap();
        let v = reviews.request("report", "#12", Some(1), "still one", Some("t9")).unwrap();
        assert_eq!((v["review"]["status"].as_str(), v["decision"].as_str()), (Some("stuck"), Some("d000000000001")));
        assert_eq!(lock(&hands.notes).len(), 2, "round 3 isn't noted: the user decides");
        assert_eq!(lock(&hands.calls).iter().filter(|c| c.starts_with("raise ")).count(), 1);
        assert!(lock(&hands.calls).iter().any(|c| c == "raise o/app#12 t3 t9"));
        // Another round while the decision waits: nothing.
        reviews.request("report", "#12", Some(1), "still one", Some("t9")).unwrap();
        assert_eq!((lock(&hands.notes).len(), lock(&hands.calls).iter().filter(|c| c.starts_with("raise ")).count()), (2, 1));
        // Agreed: the reviewer ends, its checkout goes, the decision is settled; the record stays.
        let v = reviews.request("agree", "#12", None, "approved", Some("t9")).unwrap();
        assert_eq!(v["review"]["status"], "agreed");
        let calls = lock(&hands.calls).clone();
        assert!(calls.contains(&"end t9".to_string()) && calls.contains(&"settle d000000000001".to_string()), "{calls:?}");
        assert!(!base.join(".tower-worktrees/fix-login/review-shop-12").exists());
        assert_eq!(prs.get("o/app#12").unwrap().review.unwrap().line, "Review: agreed");
        assert!(crate::prs::gate(&prs.get("o/app#12").unwrap()).warnings.iter().all(|w| !w.contains("review")));
        assert!(reviews.request("report", "#12", Some(1), "x", Some("t9")).is_err());
        let list = reviews.request("list", "", None, "", None).unwrap();
        assert_eq!(list["reviews"][0]["status"], "agreed");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_review_ends_when_its_pr_closes_its_space_closes_or_its_review_is_turned_off() {
        let (reviews, hands, prs, base) = setup("ends");
        found(&prs, &reviews, "t3", "fix-login");
        // Review turned off: the sweep ends it and its reviewer.
        lock(&hands.off).insert("fix-login".into());
        assert_eq!(reviews.sweep(), ["o/app#12"]);
        let r = reviews.get("o/app#12").unwrap();
        assert_eq!((r.status, r.line.as_str()), (reviews::Status::Ended, "review was turned off for fix-login"));
        assert!(lock(&hands.calls).contains(&"end t9".to_string()));
        assert!(reviews.sweep().is_empty(), "once");
        // The PR closes on GitHub: its review ends with it.
        let (reviews, _hands, prs, base2) = setup("closed");
        found(&prs, &reviews, "t3", "fix-login");
        serve(prs.gh(), 12, "MERGED", json!([]), "x");
        prs.poll("o/app#12");
        assert_eq!(reviews.get("o/app#12").unwrap().line, "the PR was merged");
        // The space closes; or the reviewer's session ends by itself.
        let (reviews, hands, prs, base3) = setup("space");
        found(&prs, &reviews, "t3", "fix-login");
        lock(&hands.closed).insert("fix-login".into());
        reviews.sweep();
        assert_eq!(reviews.get("o/app#12").unwrap().line, "the space fix-login was closed");
        let (reviews, hands, prs, base4) = setup("gone");
        found(&prs, &reviews, "t3", "fix-login");
        lock(&hands.gone).insert("t9".into());
        reviews.sweep();
        assert_eq!(reviews.get("o/app#12").unwrap().line, "its reviewer session ended");
        // The PR's own session ends: nobody is left to fix it, and its reviewer ends.
        let (reviews, hands, prs, base5) = setup("owner");
        found(&prs, &reviews, "t3", "fix-login");
        lock(&hands.gone).insert("t3".into());
        assert_eq!(reviews.sweep(), ["o/app#12"]);
        assert_eq!(reviews.get("o/app#12").unwrap().line, "the PR's own session ended");
        assert!(lock(&hands.calls).contains(&"end t9".to_string()));
        for b in [base, base2, base3, base4, base5] {
            let _ = std::fs::remove_dir_all(b);
        }
    }
}
