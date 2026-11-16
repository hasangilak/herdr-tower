//! Bugs found off a space's goal, spun out to a space of their own.
//!
//! A session that finds a bug while it works on something else reports it
//! to its tower (`tower queue tower "…"`), and doesn't chase it. The tower
//! judges it against the space's goal and done-when; one that is off them
//! is spun out (`tower finding report`):
//!
//! - **Judged** (`judge`): the bug's words against the goal's. A bug in
//!   what the goal is about is the session's own work; one that shares
//!   nothing with it is off goal. A space with no goal (a standing one) is
//!   the tower's call (`--off-goal`), as is any bug it judges off goal
//!   though some words match.
//! - **Once** (`same_bug`): the same bug found again, by this session or one
//!   of another space, is the finding there is: its reporter is added, and
//!   it is told where the bug went. Nothing new opens.
//! - **Written up** (`writeup`, `findings/<id>.md`): what was found, where
//!   (repo, files and lines), how to reproduce it, the evidence, and which
//!   space and session found it. The tower publishes it as an artifact and
//!   says so (`tower finding published <id> --url <link>`).
//! - **Spun out** (`published`): a space for it opens (`tower launch`, its
//!   goal from the finding, in its own worktree), whose session gets the
//!   write-up and its link; the session that found it is told to note it
//!   and carry on with its own goal; the user is told once, in one line,
//!   with the new space's name and the link.

use std::collections::BTreeSet;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{config, history};

/// What a session found: the tower's words for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// A line: what is wrong.
    pub title: String,
    /// What was found, in a few sentences.
    pub what: String,
    /// The repo's folder (its main checkout).
    pub repo: String,
    /// Files and lines: `src/auth.rs:120-140`.
    pub places: Vec<String>,
    /// How to reproduce it.
    pub repro: String,
    /// What shows it: an error, a log line, a failing test.
    pub evidence: String,
    /// Who found it: the space, the session's address and name.
    pub space: String,
    pub session: String,
    pub session_name: String,
}

/// A finding, as kept (`findings.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Finding {
    pub id: String,
    pub report: Report,
    pub at: u64,
    /// `drafted` (written up, its artifact not yet published), `spun-out`.
    pub state: String,
    /// Its artifact.
    pub url: String,
    /// The space it was spun out to.
    pub spun_to: String,
    /// Whoever found it again: "space › session (address)".
    pub again: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub findings: Vec<Finding>,
    pub next: u64,
}

// ---- the judgment ----

/// Words that say nothing of what a bug or a goal is about.
const PLAIN: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "when", "then", "than", "they", "them", "their", "there", "here", "have", "has", "had", "was",
    "were", "are", "is", "be", "been", "being", "not", "but", "all", "any", "some", "one", "two", "its", "it's", "our", "your", "you", "can", "could", "would",
    "should", "will", "does", "did", "doing", "done", "make", "made", "makes", "get", "gets", "got", "use", "used", "uses", "using", "new", "old", "also", "only",
    "just", "more", "most", "less", "very", "still", "after", "before", "while", "where", "what", "which", "who", "why", "how", "each", "every", "other", "same",
    "bug", "bugs", "issue", "error", "errors", "wrong", "broken", "fails", "fail", "failing", "fix", "fixed", "fixes", "found", "find", "work", "works", "working",
    "code", "file", "files", "line", "lines", "test", "tests", "repo", "project", "session", "space", "goal", "should", "instead", "returns", "return", "value",
    "values", "data", "case", "cases", "when", "time", "times", "way", "thing", "things", "sure", "need", "needs", "want", "wants", "like", "about", "over", "under",
];

/// A text's meaningful words, roughly stemmed (`tokens`, `token`: one);
/// a short one only as an acronym (`KB`, `S3`).
pub fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3 || (w.chars().count() == 2 && w.chars().any(|c| c.is_ascii_uppercase()) && !w.chars().any(|c| c.is_lowercase())))
        .map(|w| w.to_lowercase())
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit()) && !PLAIN.contains(&w.as_str()))
        .map(|w| stem(&w))
        .collect()
}

/// Two words alike: the same, or one the other's start (`translat`,
/// `translation`), or the same first five letters (`rebuild`, `rebuilt`).
fn alike(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    (short.chars().count() >= 4 && long.starts_with(short)) || a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count() >= 5
}

/// The words of `of` that are alike one of `in_`.
fn common<'a>(of: &'a BTreeSet<String>, in_: &BTreeSet<String>) -> Vec<&'a String> {
    of.iter().filter(|w| in_.iter().any(|v| alike(w, v))).collect()
}

fn stem(w: &str) -> String {
    for end in ["ings", "ing", "ies", "es", "ed", "s"] {
        if let Some(s) = w.strip_suffix(end)
            && s.chars().count() >= 4
        {
            return if end == "ies" { format!("{s}y") } else { s.to_string() };
        }
    }
    w.to_string()
}

/// The names a bug's places give (a file's name without its folder or
/// extension, the folders it is in): `src/kb/index.rs:12` is `kb`, `index`.
fn place_words(places: &[String]) -> BTreeSet<String> {
    places
        .iter()
        .flat_map(|p| {
            let path = p.split(':').next().unwrap_or(p);
            path.split('/').map(|part| part.rsplit_once('.').map(|(a, _)| a).unwrap_or(part).to_string()).collect::<Vec<_>>()
        })
        .flat_map(|part| words(&part).into_iter().chain((part.chars().count() == 2 && part.chars().all(|c| c.is_ascii_alphanumeric())).then(|| part.to_lowercase())))
        .collect()
}

/// The tower's judgment of a bug against its space's goal.
#[derive(Debug, Clone, PartialEq)]
pub struct Judged {
    /// Off the goal: spin it out.
    pub off: bool,
    /// None: the space has no goal to judge it by (the tower's call).
    pub known: bool,
    pub why: String,
}

/// A bug against its space's goal and done-when: on goal when it is about
/// what they are about (two of their words in its title and account, or
/// one in its title or places with a third of its title's words in them);
/// off goal when it shares nothing with them.
pub fn judge(goal: &str, done_when: &str, r: &Report) -> Judged {
    let aim = words(&format!("{goal} {done_when}"));
    if aim.is_empty() {
        return Judged { off: false, known: false, why: "the space has no goal to judge it by".into() };
    }
    let title = words(&r.title);
    let told: BTreeSet<String> = title.iter().cloned().chain(words(&r.what)).collect();
    let places = place_words(&r.places);
    let shared: Vec<&String> = common(&told, &aim);
    let in_title = common(&title, &aim).len();
    let in_places: Vec<&String> = common(&places, &aim);
    let on = shared.len() >= 2 && in_title >= 1 || (in_title + in_places.len() >= 1 && (in_title * 3 >= title.len().max(1) || !in_places.is_empty()) && shared.len() + in_places.len() >= 2);
    let named: Vec<String> = shared.iter().map(|w| w.to_string()).chain(in_places.iter().map(|w| w.to_string())).collect::<BTreeSet<_>>().into_iter().collect();
    if on {
        Judged { off: false, known: true, why: format!("on its goal: it is about {}", named.join(", ")) }
    } else if named.is_empty() {
        Judged { off: true, known: true, why: "off its goal: it shares nothing with the goal or what counts as done".into() }
    } else {
        Judged { off: true, known: true, why: format!("off its goal: only {} in common", named.join(", ")) }
    }
}

// ---- once per bug ----

fn repo_key(repo: &str) -> String {
    Path::new(repo.trim_end_matches('/')).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn files_of(places: &[String]) -> BTreeSet<String> {
    places.iter().map(|p| p.split(':').next().unwrap_or(p).trim().trim_start_matches("./").to_lowercase()).filter(|p| !p.is_empty()).collect()
}

fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    let (i, u) = (a.intersection(b).count(), a.union(b).count());
    if u == 0 { 0.0 } else { i as f32 / u as f32 }
}

/// The same bug: in the same repo (or one says none), and either in the
/// same files with a title much alike, or a title and account alike enough
/// on their own. Found by any session, in any space.
pub fn same_bug(a: &Report, b: &Report) -> bool {
    let (ra, rb) = (repo_key(&a.repo), repo_key(&b.repo));
    if !ra.is_empty() && !rb.is_empty() && ra != rb {
        return false;
    }
    let (ta, tb) = (words(&a.title), words(&b.title));
    let (fa, fb) = (files_of(&a.places), files_of(&b.places));
    let all_a: BTreeSet<String> = ta.iter().cloned().chain(words(&a.what)).collect();
    let all_b: BTreeSet<String> = tb.iter().cloned().chain(words(&b.what)).collect();
    let same_files = !fa.is_empty() && !fb.is_empty() && fa.intersection(&fb).next().is_some();
    (same_files && (overlap(&ta, &tb) >= 0.25 || overlap(&all_a, &all_b) >= 0.25)) || overlap(&ta, &tb) >= 0.6 || overlap(&all_a, &all_b) >= 0.5
}

// ---- the write-up ----

fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

/// The finding written up (markdown): what the artifact and the new space's session get.
pub fn writeup(f: &Finding) -> String {
    let r = &f.report;
    let or = |t: &str, none: &str| if t.trim().is_empty() { none.to_string() } else { t.trim().to_string() };
    let places = if r.places.is_empty() { "- (not given)".to_string() } else { r.places.iter().map(|p| format!("- `{p}`")).collect::<Vec<_>>().join("\n") };
    let mut s = format!("# {}\n\n", r.title.trim());
    s += &format!("Found {} by {} ({}) in the space \"{}\", while it worked on that space's own goal. Spun out by the tower ({}).\n\n", when(f.at), or(&r.session_name, &r.session), r.session, r.space, f.id);
    s += &format!("## What was found\n\n{}\n\n", or(&r.what, "(not given)"));
    s += &format!("## Where\n\n- Repo: `{}`\n{places}\n\n", tilde(&or(&r.repo, "(not given)")));
    s += &format!("## How to reproduce\n\n{}\n\n", or(&r.repro, "(not given: find it from the evidence)"));
    s += &format!("## Evidence\n\n{}\n", or(&r.evidence, "(not given)"));
    if !f.again.is_empty() {
        s += &format!("\n## Found again by\n\n{}\n", f.again.iter().map(|a| format!("- {a}")).collect::<Vec<_>>().join("\n"));
    }
    s
}

fn when(at: u64) -> String {
    let secs = (at / 1000) as i64;
    chrono_like(secs)
}

/// "2026-11-16 19:40" (local time, by `date`): no date crate here.
fn chrono_like(secs: i64) -> String {
    std::process::Command::new("date")
        .args(["-r", &secs.to_string(), "+%Y-%m-%d %H:%M"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("at {secs}"))
}

/// The new space's goal (a line) and done-when.
pub fn goal_of(f: &Finding) -> (String, String) {
    let t = f.report.title.trim().trim_end_matches('.');
    let title: String = if t.chars().count() > 90 { format!("{}…", t.chars().take(89).collect::<String>().trim_end()) } else { t.to_string() };
    (format!("Fix: {title}"), "The bug is fixed with a test that reproduces it, and the fix is committed on this space's branch".to_string())
}

/// The new space's name: `bug-` and two words of its title (`bug-stats-mean`),
/// never one there is.
pub fn space_for(f: &Finding, taken: &[String]) -> String {
    let base = crate::launch::space_name(f.report.title.trim());
    let base = if base.is_empty() { "bug".to_string() } else { format!("bug-{base}") };
    crate::launch::unique(&base, taken)
}

/// What the new space's session is asked first (`tower launch`'s goal): the finding, its link.
pub fn ticket(f: &Finding) -> String {
    let (goal, _) = goal_of(f);
    format!(
        "{goal}\n\nThis bug was found off its space's goal and spun out here by the tower. Its write-up: {}\n\n{}\n\nReproduce it first, fix it in this worktree with a test that shows it, and commit on this space's branch. Report to the tower when it is fixed. Don't push or open a PR unless the user says so.",
        f.url,
        writeup(f)
    )
}

/// The line the finding session gets: note it, don't chase it.
pub fn told_finder(f: &Finding, again: bool) -> String {
    let title = f.report.title.trim().trim_end_matches('.');
    let first = if again { format!("The bug you reported (\"{title}\") was found before") } else { format!("Thanks: the bug you reported (\"{title}\") is off this space's goal") };
    format!(
        "{first}: the tower spun it out to the space \"{}\", where a session of its own works on it (write-up: {}). Note it in your report if it matters there, don't fix it here, and carry on with your own goal.",
        f.spun_to, f.url
    )
}

/// The user's one line.
pub fn told_user(f: &Finding) -> String {
    let title = f.report.title.trim().trim_end_matches('.');
    format!("A bug {} found off its goal in {} is spun out to the new space \"{}\": {title} ({}).", or_name(&f.report), f.report.space, f.spun_to, f.url)
}

fn or_name(r: &Report) -> String {
    if r.session_name.trim().is_empty() { r.session.clone() } else { r.session_name.clone() }
}

// ---- the store ----

pub fn path() -> PathBuf {
    config::state_dir().join("findings.json")
}

pub fn dir() -> PathBuf {
    config::state_dir().join("findings")
}

pub fn load_from(p: &Path) -> Book {
    std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Reads the book, changes it, writes it back: one writer at a time.
pub fn update_at<R>(p: &Path, f: impl FnOnce(&mut Book) -> R) -> R {
    use std::os::fd::AsRawFd;
    let dir = p.parent().unwrap_or(Path::new("."));
    let _ = std::fs::create_dir_all(dir);
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".findings.lock")).ok();
    if let Some(l) = &lock {
        // SAFETY: flock on a file we hold open; released when it closes.
        unsafe { libc::flock(l.as_raw_fd(), libc::LOCK_EX) };
    }
    let mut b = load_from(p);
    let before = b.clone();
    let r = f(&mut b);
    if b != before {
        let tmp = dir.join(format!(".findings.json.{}", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&b).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, p);
        }
    }
    r
}

/// What a report came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Filed {
    /// New: written up, to publish (`published`).
    New(Finding),
    /// Found before: this one.
    Again(Finding),
}

/// Files a report in `book` (the judgment made): new, or the finding there is.
pub fn file(book: &mut Book, r: Report, now: u64) -> Filed {
    if let Some(f) = book.findings.iter_mut().find(|f| same_bug(&f.report, &r)) {
        let who = format!("{} › {} ({})", r.space, or_name(&r), r.session);
        let first = format!("{} › {} ({})", f.report.space, or_name(&f.report), f.report.session);
        if who != first && !f.again.contains(&who) {
            f.again.push(who);
        }
        return Filed::Again(f.clone());
    }
    book.next += 1;
    let f = Finding { id: format!("f{}", book.next), report: r, at: now, state: "drafted".into(), ..Default::default() };
    book.findings.push(f.clone());
    Filed::New(f)
}

// ---- the command ----

#[derive(Debug, Default)]
pub struct Args {
    pub action: String,
    pub id: Option<String>,
    pub from: Option<String>,
    pub title: Option<String>,
    pub what: Option<String>,
    pub repo: Option<String>,
    pub places: Vec<String>,
    pub repro: Option<String>,
    pub evidence: Option<String>,
    pub url: Option<String>,
    pub off_goal: bool,
    pub json: bool,
}

/// A folder's main checkout (a worktree's repo), so the new space gets a worktree of its own from it.
fn main_checkout(folder: &str) -> String {
    let out = std::process::Command::new("git").args(["-C", folder, "rev-parse", "--path-format=absolute", "--git-common-dir"]).output();
    match out {
        Ok(o) if o.status.success() => {
            let common = String::from_utf8_lossy(&o.stdout).trim().to_string();
            Path::new(&common).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| folder.to_string())
        }
        _ => folder.to_string(),
    }
}

fn session_info(address: &str) -> (String, String, String) {
    let t = crate::hc::get(address);
    let name = t.as_ref().and_then(|t| crate::hc::name(t).map(String::from)).unwrap_or_default();
    let space = t.as_ref().and_then(crate::spaces::space_of).unwrap_or_default();
    let folder = t.as_ref().and_then(|t| t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str()).map(String::from)).unwrap_or_default();
    (name, space, folder)
}

/// A queued message to a session, from the tower.
fn queue_to(session: &str, text: &str) -> Result<()> {
    let r = crate::protocol::send(&crate::protocol::Request::Queue {
        action: "add".into(),
        session: session.into(),
        text: text.into(),
        id: None,
        after: vec![],
        start: vec![],
        tag: None,
        from: None,
    })?;
    if !r.ok {
        bail!("{}", r.error.unwrap_or_default());
    }
    Ok(())
}

fn say(text: &str) {
    let _ = crate::protocol::send(&crate::protocol::Request::Say { text: text.into(), interrupt: false, kind: String::new(), from: None, brain: None });
}

/// `tower finding report --from <session> --title "…" --what "…" [--repo <folder>] [--place <file:lines>…] [--repro "…"] [--evidence "…"] [--off-goal]`.
pub fn report(a: &Args, now: u64) -> Result<Value> {
    let from = a.from.as_deref().map(str::trim).filter(|s| !s.is_empty()).context("--from <the session that found it>")?;
    let title = a.title.as_deref().map(str::trim).filter(|s| !s.is_empty()).context("--title \"<what is wrong, in a line>\"")?;
    let (name, space, folder) = session_info(from);
    let repo = a.repo.as_deref().map(str::trim).filter(|r| !r.is_empty()).map(String::from).unwrap_or(folder);
    let r = Report {
        title: title.into(),
        what: a.what.clone().unwrap_or_default(),
        repo: if repo.is_empty() { repo } else { main_checkout(&repo) },
        places: a.places.clone(),
        repro: a.repro.clone().unwrap_or_default(),
        evidence: a.evidence.clone().unwrap_or_default(),
        space: space.clone(),
        session: from.into(),
        session_name: name,
    };
    // Against its space's goal and done-when.
    let s = crate::spaces::load();
    let (goal, done_when) = crate::missions::of(&s, &space).map(|(k, m)| (crate::missions::goal_of(&s, k).to_string(), m.done_when.clone())).unwrap_or_default();
    let j = judge(&goal, &done_when, &r);
    if !a.off_goal && !(j.off && j.known) {
        let why = if j.known { j.why.clone() } else { format!("{}: say --off-goal if it is off it", j.why) };
        return Ok(json!({ "filed": "on-goal", "space": space, "goal": goal, "done_when": done_when, "why": why,
            "next": format!("Not spun out ({why}). If it is the session's own work, let it fix it; if you judge it off goal, run this again with --off-goal.") }));
    }
    let filed = update_at(&path(), |b| file(b, r, now));
    match filed {
        Filed::Again(f) => {
            // Found before: told where it went, once more for this one.
            if f.state == "spun-out" {
                queue_to(from, &told_finder(&f, true))?;
            }
            Ok(json!({ "filed": "again", "id": f.id, "state": f.state, "space": f.spun_to, "url": f.url,
                "next": if f.state == "spun-out" { format!("Found before ({}): {from} is told it is in \"{}\". Nothing new opens; nothing to tell the user.", f.id, f.spun_to) }
                        else { format!("Found before ({}), not yet published: publish {} and run `tower finding published {} --url <link>`.", f.id, dir().join(format!("{}.md", f.id)).display(), f.id) } }))
        }
        Filed::New(f) => {
            let md = dir().join(format!("{}.md", f.id));
            std::fs::create_dir_all(dir())?;
            std::fs::write(&md, writeup(&f))?;
            Ok(json!({ "filed": "new", "id": f.id, "judged": j.why, "writeup": md,
                "next": format!("Publish {} as an HTML page Artifact with your Artifact tool (its words as they are: what was found, where, how to reproduce, evidence, who found it), then run `tower finding published {} --url <its link>`: that opens its space and tells the session and the user.", md.display(), f.id) }))
        }
    }
}

/// `tower finding published <id> --url <link>`: its space opens, the finder and the user are told.
pub fn published(id: &str, url: &str) -> Result<Value> {
    if !url.starts_with("https://") {
        bail!("--url is the artifact's link (https://claude.ai/…), not {url:?}");
    }
    let f = update_at(&path(), |b| {
        let f = b.findings.iter_mut().find(|f| f.id == id)?;
        f.url = url.into();
        Some(f.clone())
    })
    .with_context(|| format!("no finding {id} (tower finding list)"))?;
    if f.state == "spun-out" {
        return Ok(json!({ "id": f.id, "space": f.spun_to, "url": f.url, "next": "Already spun out: nothing more to do." }));
    }
    let (goal, done_when) = goal_of(&f);
    let repos: Vec<String> = if f.report.repo.is_empty() { vec![] } else { vec![f.report.repo.clone()] };
    let taken = crate::launch::world_now().map(|w| w.spaces.into_iter().chain(w.open).collect::<Vec<_>>()).unwrap_or_default();
    let name = space_for(&f, &taken);
    let ask = crate::launch::Ask { name: Some(&name), title: Some(&goal), done_when: Some(&done_when), repos: &repos, ..Default::default() };
    let r = crate::launch::launch(&ticket(&f), &ask, false)?;
    let space = r["space"].as_str().unwrap_or("").to_string();
    let opened = r["sessions"].as_array().is_some_and(|a| a.iter().any(|s| s["error"].is_null()));
    if space.is_empty() || !opened {
        bail!("its space didn't open: {}", r);
    }
    let f = update_at(&path(), |b| {
        let f = b.findings.iter_mut().find(|f| f.id == id)?;
        (f.state, f.spun_to) = ("spun-out".into(), space.clone());
        Some(f.clone())
    })
    .context("the finding went while its space opened")?;
    // The write-up kept with the link, for whoever reads it from the folder.
    let _ = std::fs::write(dir().join(format!("{}.md", f.id)), format!("{}\nArtifact: {}\n", writeup(&f), f.url));
    let told = queue_to(&f.report.session, &told_finder(&f, false));
    say(&told_user(&f));
    history::append(history::Entry { kind: "finding".into(), space: Some(f.report.space.clone()), session: Some(f.report.session.clone()), text: told_user(&f), record: true, ..Default::default() });
    Ok(json!({ "id": f.id, "space": space, "url": f.url, "finder_told": told.is_ok(), "launch": r }))
}

pub fn cmd(a: Args) -> Result<()> {
    let v = match a.action.as_str() {
        "report" => report(&a, history::now_ms())?,
        "published" => published(a.id.as_deref().context("tower finding published <id> --url <link>")?, a.url.as_deref().unwrap_or(""))?,
        "list" => json!(load_from(&path()).findings),
        "show" => {
            let id = a.id.as_deref().context("tower finding show <id>")?;
            let b = load_from(&path());
            let f = b.findings.iter().find(|f| f.id == id).with_context(|| format!("no finding {id}"))?;
            json!({ "finding": f, "writeup": writeup(f) })
        }
        other => bail!("tower finding report|published|list|show, not {other:?}"),
    };
    if a.json || !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if let Some(next) = v["next"].as_str() {
        println!("{next}");
    } else if a.action == "published" {
        println!("Spun out to \"{}\": the session that found it and the user are told.", v["space"].as_str().unwrap_or(""));
    } else {
        println!("{}", serde_json::to_string_pretty(&v)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(title: &str, what: &str, repo: &str, places: &[&str]) -> Report {
        Report { title: title.into(), what: what.into(), repo: repo.into(), places: places.iter().map(|p| p.to_string()).collect(), space: "kb-refresh".into(), session: "t9".into(), ..Default::default() }
    }

    const KB_GOAL: &str = "Update KB for EYZFlow and EYZ MediaRoom; prepare io; herdr sandbox sessions; KB English, model translates";
    const KB_DONE: &str = "The knowledge base answers in English for both products, and its index is rebuilt";

    #[test]
    fn a_bug_off_the_goal_is_off_and_one_in_it_is_on() {
        // kb-refresh's session finds the mediaroom upload timing out: nothing to do with the KB.
        let upload = rep("Video upload times out after 30 s on large files", "The S3 multipart upload in the uploader retries forever when a part fails.", "/w/eyz-mediaroom-serverless", &["src/upload/multipart.ts:88-120"]);
        let j = judge(KB_GOAL, KB_DONE, &upload);
        assert!(j.off && j.known, "{j:?}");
        // A stale entry in the KB index it is rebuilding: its own work.
        let index = rep("KB index keeps stale entries after a rebuild", "Old knowledge base documents stay in the index.", "/w/eyz-kb", &["src/kb/index.ts:40"]);
        let j = judge(KB_GOAL, KB_DONE, &index);
        assert!(!j.off && j.known, "{j:?}");
        // Its places alone can say so: a file in the kb folder, its title about translation.
        let tr = rep("Translations come back empty for German questions", "", "/w/eyz-kb", &["src/kb/translate.ts:12"]);
        assert!(!judge(KB_GOAL, KB_DONE, &tr).off, "{:?}", judge(KB_GOAL, KB_DONE, &tr));
        // One plain word in common isn't enough ("update").
        let dep = rep("Dependabot update breaks the Android build", "Gradle plugin bump fails the CI build of the app.", "/w/eyz-android", &["app/build.gradle:3"]);
        let j = judge(KB_GOAL, KB_DONE, &dep);
        assert!(j.off, "{j:?}");
        // No goal (a standing space): the tower's call.
        let j = judge("", "", &upload);
        assert!(!j.known && !j.off);
    }

    #[test]
    fn the_same_bug_is_one_finding_whoever_finds_it() {
        let mut b = Book::default();
        let first = rep("Video upload times out after 30 s on large files", "The multipart upload retries forever when a part fails.", "/w/eyz-mediaroom-serverless", &["src/upload/multipart.ts:88-120"]);
        let Filed::New(f1) = file(&mut b, first, 1) else { panic!("new") };
        // The same session reports it again, in other words: the same one.
        let again = rep("Large file upload hangs (timeout)", "multipart upload never gives up when a part fails", "/w/eyz-mediaroom-serverless", &["src/upload/multipart.ts:95"]);
        assert!(matches!(file(&mut b, again, 2), Filed::Again(ref f) if f.id == f1.id));
        // Another space's session, from its own worktree of the repo.
        let mut other = rep("Upload times out on large video files", "Large uploads time out after 30 s.", "/w/worktrees/eyz-mediaroom-serverless", &[]);
        (other.space, other.session, other.session_name) = ("sandbox".into(), "t40".into(), "api".into());
        let Filed::Again(f) = file(&mut b, other, 3) else { panic!("again") };
        assert_eq!(f.again, ["sandbox › api (t40)"], "the first finder isn't listed again; the other one is");
        // Another bug in the same repo and file: a finding of its own.
        let other_bug = rep("Thumbnail of a portrait video is cropped", "The thumbnail step crops portrait frames to landscape.", "/w/eyz-mediaroom-serverless", &["src/upload/thumbs.ts:10"]);
        assert!(matches!(file(&mut b, other_bug, 4), Filed::New(ref f) if f.id == "f2"));
        // The same words in another repo: not the same bug.
        let elsewhere = rep("Video upload times out after 30 s on large files", "", "/w/eyzflow", &["src/upload/multipart.ts:88"]);
        assert!(matches!(file(&mut b, elsewhere, 5), Filed::New(_)));
        assert_eq!(b.findings.len(), 3);
    }

    #[test]
    fn its_writeup_says_what_where_how_and_who() {
        let mut r = rep("Video upload times out", "Retries forever.", "/w/eyz-mediaroom-serverless", &["src/upload/multipart.ts:88-120"]);
        (r.repro, r.evidence, r.session_name) = ("Upload a 2 GB file on dev.".into(), "Lambda log: Task timed out after 30.00 seconds".into(), "kb".into());
        let f = Finding { id: "f1".into(), report: r, at: 0, url: "https://claude.ai/artifact/x".into(), spun_to: "fix-video".into(), ..Default::default() };
        let w = writeup(&f);
        for part in ["# Video upload times out", "## What was found\n\nRetries forever.", "`src/upload/multipart.ts:88-120`", "Repo: `/w/eyz-mediaroom-serverless`", "## How to reproduce\n\nUpload a 2 GB file", "Task timed out", "by kb (t9) in the space \"kb-refresh\""] {
            assert!(w.contains(part), "{part:?} in {w}");
        }
        assert_eq!(goal_of(&f).0, "Fix: Video upload times out");
        assert_eq!(space_for(&f, &[]), "bug-video-upload");
        assert_eq!(space_for(&f, &["bug-video-upload".to_string()]), "bug-video-upload-2", "never one there is");
        let mut odd = f.clone();
        odd.report.title = "stats.mean([]) raises ZeroDivisionError on an empty list".into();
        assert_eq!(space_for(&odd, &[]), "bug-stats-mean");
        assert!(ticket(&f).contains("https://claude.ai/artifact/x") && ticket(&f).contains("## Evidence"));
        assert!(told_finder(&f, false).contains("carry on with your own goal") && told_finder(&f, false).contains("\"fix-video\""));
        let line = told_user(&f);
        assert!(line.contains("\"fix-video\"") && line.contains("https://claude.ai/artifact/x") && !line.contains('\n'), "{line}");
    }
}
