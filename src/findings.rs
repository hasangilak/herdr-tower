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
//!   it is told where the bug went. Nothing new opens. Unless that finding's
//!   space has closed (or was resolved) since: nobody works on it there any
//!   more, so it is filed new, following the old one (`Finding::follows`),
//!   and opens a space of its own.
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
    /// `drafted` (written up, its artifact not yet published), `publishing`
    /// (its space opening), `spun-out`.
    pub state: String,
    /// When its space began to open (`publishing`): another `published` waits for it.
    pub publishing_at: u64,
    /// Its artifact.
    pub url: String,
    /// The space it was spun out to.
    pub spun_to: String,
    /// Whoever found it again: "space › session (address)".
    pub again: Vec<String>,
    /// The finding it follows (`f2`): the same bug found before, whose
    /// space has closed since (fixed for the cause found then, resolved, or
    /// left). Nobody works on that one any more: this one opens a space of
    /// its own, and says where the first went.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub follows: String,
    /// That finding's space (closed) and link.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub follows_space: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub follows_url: String,
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

/// A place's file, as written: `lib/user.js:17-21`, `lib/user.js (pickProfileFields)`
/// and `lib/user.js#L17` are `lib/user.js`; a bare name (`RegisterForm`) stays a name.
fn file_of(place: &str) -> String {
    let p = place.trim().split(|c: char| c.is_whitespace() || c == '(' || c == '#').next().unwrap_or("");
    let p = p.split(':').next().unwrap_or(p);
    p.trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == ',').trim_start_matches("./").to_lowercase()
}

/// A file's name without its folders and extension: `components/auth/RegisterForm.js` is `registerform`.
fn file_stem(f: &str) -> &str {
    let name = f.rsplit('/').next().unwrap_or(f);
    name.split_once('.').map(|(a, _)| a).unwrap_or(name)
}

/// Two places the same file: the same path, one the other's end
/// (`user.js`, `lib/functions/user.js`), or a bare name of four letters or
/// more (`RegisterForm`: no folder, no extension) the other's file name.
fn same_file(a: &str, b: &str) -> bool {
    let ends = |long: &str, short: &str| long.strip_suffix(short).is_some_and(|rest| rest.ends_with('/'));
    let bare = |f: &str| !f.contains('/') && !f.contains('.') && f.chars().count() >= 4;
    a == b || ends(a, b) || ends(b, a) || (bare(a) && file_stem(b) == a) || (bare(b) && file_stem(a) == b)
}

/// The names in code a text gives: `pickProfileFields`, `marketing_email_opt_in`,
/// `RegisterForm`, `lib/functions/user.js`. Plain words aren't.
fn code_names(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '/'))
        .map(|w| w.trim_matches(|c: char| c == '.' || c == '/'))
        .filter(|w| w.chars().count() >= 4 && w.chars().any(|c| c.is_alphabetic()))
        .filter(|w| {
            let camel = w.chars().zip(w.chars().skip(1)).any(|(x, y)| x.is_lowercase() && y.is_uppercase());
            let snake = w.contains('_') && !w.starts_with('_');
            let path = w.contains('/') || w.rsplit_once('.').is_some_and(|(a, ext)| !a.is_empty() && (1..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphabetic()));
            camel || snake || path
        })
        .map(|w| w.to_lowercase())
        .collect()
}

/// A place's lines: `a.js:12` is 12–12, `a.js:12-20` and `a.js#L12-L20`
/// 12–20; none when it gives none (`a.js`, `a.js (pickProfileFields)`).
fn lines_of(place: &str) -> Option<(u32, u32)> {
    let p = place.trim().trim_matches(|c: char| c == '`' || c == '"' || c == '\'');
    let rest = p.split_once(':').map(|(_, r)| r).or_else(|| p.split_once("#L").map(|(_, r)| r))?;
    let num = |s: &str| -> Option<u32> {
        let d: String = s.trim_start_matches('L').chars().take_while(|c| c.is_ascii_digit()).collect();
        d.parse().ok()
    };
    let start = num(rest)?;
    let end = rest.split_once('-').and_then(|(_, e)| num(e)).filter(|e| *e >= start).unwrap_or(start);
    Some((start, end))
}

/// The `file:line`s a text gives: `(components/client/Navbar.js:151)`,
/// and in words, `Navbar.js line 151`, `layout.js lines 30-38`: a path or a
/// file name with an extension, then a line.
fn text_places(text: &str) -> Vec<String> {
    let named = |f: &str| f.contains('/') || f.rsplit_once('.').is_some_and(|(a, ext)| !a.is_empty() && (1..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphabetic()));
    let digit = |w: &str| w.starts_with(|c: char| c.is_ascii_digit());
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '`' | '"' | '\''))
        .map(|w| w.trim_matches(['[', ']']).trim_end_matches(['.', ':']))
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = vec![];
    for (i, w) in tokens.iter().enumerate() {
        if let Some((f, line)) = w.split_once(':') {
            if named(f) && digit(line) {
                out.push(w.to_string());
            }
        } else if named(w)
            && tokens.get(i + 1).is_some_and(|l| l.eq_ignore_ascii_case("line") || l.eq_ignore_ascii_case("lines"))
            && let Some(n) = tokens.get(i + 2).filter(|n| digit(n))
        {
            out.push(format!("{w}:{n}"));
        }
    }
    out
}

/// The places a report names, with their lines when given: its places,
/// and any `file:line` its title or account gives.
fn spots(r: &Report) -> Vec<(String, Option<(u32, u32)>)> {
    r.places.iter().cloned().chain(text_places(&format!("{} {}", r.title, r.what))).map(|p| (file_of(&p), lines_of(&p))).filter(|(f, _)| !f.is_empty()).collect()
}

/// How alike two reports at other lines of the same files must be to be one
/// bug: their titles' words, or all their words (title and account).
pub const APART_TITLE: f32 = 0.8;
pub const APART_ALL: f32 = 0.7;

/// Lines this close are one place: the line above or below what the other gave.
pub const NEAR_LINES: u32 = 2;

/// Two reports' places, file by file.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Spots {
    /// No file in common.
    Elsewhere,
    /// A file in common, at the same lines (or one doesn't say which).
    Same,
    /// Files in common, but every one at other lines: other code in them.
    Apart,
}

fn compare_spots(a: &Report, b: &Report) -> Spots {
    let (sa, sb) = (spots(a), spots(b));
    let mut shared = false;
    for (fa, la) in &sa {
        for (fb, lb) in &sb {
            if !same_file(fa, fb) {
                continue;
            }
            shared = true;
            match (la, lb) {
                (Some((a0, a1)), Some((b0, b1))) if *a0 > b1 + NEAR_LINES || *b0 > a1 + NEAR_LINES => {}
                _ => return Spots::Same,
            }
        }
    }
    if shared { Spots::Apart } else { Spots::Elsewhere }
}

fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    let (i, u) = (a.intersection(b).count(), a.union(b).count());
    if u == 0 { 0.0 } else { i as f32 / u as f32 }
}

/// The same bug: in the same repo (or one says none), and either in the
/// same files (however its places are written: `same_file`) or naming two
/// of the same names in code, with a title or account somewhat alike, or a
/// title and account alike enough on their own. Found by any session, in
/// any space, reported by any tower.
///
/// Files in common at other lines (`Navbar.js:148` and `Navbar.js:151`,
/// `Footer.js:95` and `Footer.js:110`) are other code in those files: a
/// title and account somewhat alike (both about German labels) aren't
/// enough then, only a title or account all but the same.
pub fn same_bug(a: &Report, b: &Report) -> bool {
    let (ra, rb) = (repo_key(&a.repo), repo_key(&b.repo));
    if !ra.is_empty() && !rb.is_empty() && ra != rb {
        return false;
    }
    let (ta, tb) = (words(&a.title), words(&b.title));
    let all_a: BTreeSet<String> = ta.iter().cloned().chain(words(&a.what)).collect();
    let all_b: BTreeSet<String> = tb.iter().cloned().chain(words(&b.what)).collect();
    let spots = compare_spots(a, b);
    if spots == Spots::Apart {
        return overlap(&ta, &tb) >= APART_TITLE || overlap(&all_a, &all_b) >= APART_ALL;
    }
    let same_files = spots == Spots::Same;
    // Two names in code in common (what it said, its places): as the same files.
    let names = |r: &Report| -> BTreeSet<String> { code_names(&format!("{} {} {}", r.title, r.what, r.places.join(" "))) };
    let same_names = names(a).intersection(&names(b)).count() >= 2;
    ((same_files || same_names) && (overlap(&ta, &tb) >= 0.25 || overlap(&all_a, &all_b) >= 0.25)) || overlap(&ta, &tb) >= 0.6 || overlap(&all_a, &all_b) >= 0.5
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
    if !f.follows.is_empty() {
        s += &format!(
            "## Found before\n\nThe same bug, or the same symptom, was found before as {} and spun out to the space \"{}\"{}, which has closed since: fixed for the cause found then, resolved, or left. This report is what was found now: check it against that one's fix first.\n\n",
            f.follows,
            f.follows_space,
            if f.follows_url.is_empty() { String::new() } else { format!(" ({})", f.follows_url) }
        );
    }
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

/// The new space's name: the space it was found in, then `bug-` and two
/// words of its title (`support › bug-stats-mean`), never one there is.
/// Herdr has no fork of a space: the name says where it comes from, and
/// it is placed right after that space (`fork_position`).
pub fn space_for(f: &Finding, taken: &[String]) -> String {
    let base = crate::launch::space_name(f.report.title.trim());
    let base = if base.is_empty() { "bug".to_string() } else { format!("bug-{base}") };
    let from = f.report.space.trim();
    let base = if from.is_empty() { base } else { format!("{from} {FORK_MARK} {base}") };
    crate::launch::unique(&base, taken)
}

/// Between a spun-out space's source and its own name.
pub const FORK_MARK: &str = "\u{203a}";

/// Where `new` goes in Herdr's list of spaces (`order`, as `herdr-app ls`
/// has them) to sit right after `source`: its position (1 = first) for
/// `space.move`. None: either isn't there, or it is there already.
pub fn fork_position(order: &[String], source: &str, new: &str) -> Option<usize> {
    let at = |n: &str| order.iter().position(|s| s.eq_ignore_ascii_case(n.trim()));
    let (src, me) = (at(source)?, at(new)?);
    if me == src + 1 {
        return None;
    }
    // (Taken out first: a source after it moves up by one.)
    Some(if me < src { src + 1 } else { src + 2 })
}

/// The spun-out space `new` right after `source` in Herdr's window, and the
/// link kept on its mission (`Mission::spun_from`). Says what it did.
fn place_after(source: &str, new: &str) -> String {
    let source = source.trim();
    if source.is_empty() {
        return String::new();
    }
    crate::spaces::update(|s| {
        if let Some(m) = s.missions.get_mut(new) {
            m.spun_from = Some(source.to_string());
        }
    });
    let Some(list) = crate::spaces::windows_open() else { return "Herdr's window didn't answer: not placed".into() };
    let order: Vec<String> = list.iter().map(|w| w.name.clone()).collect();
    let (Some(to), Some(me)) = (fork_position(&order, source, new), list.iter().find(|w| w.name.eq_ignore_ascii_case(new))) else {
        return format!("right after {source}");
    };
    let index = to.to_string();
    let ok = std::process::Command::new(crate::config::herdr_app_bin())
        .args(["do", "space.move", "--id", &me.id, "--index", &index])
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success());
    if ok { format!("right after {source}") } else { format!("not moved after {source}: herdr-app space.move failed") }
}

/// What the new space's session is asked first (`tower launch`'s goal): the finding, its link.
pub fn ticket(f: &Finding) -> String {
    let (goal, _) = goal_of(f);
    format!(
        "{goal}\n\nThis bug was found off its space's goal and spun out here by the tower. Its write-up: {}\n\n{}\n\nReproduce it first, fix it in this worktree with a test that shows it, and commit on this space's branch. When it is fixed, push this space's branch and open its PR into dev yourself, without asking (only merging it is the user's), then report to the tower.",
        f.url,
        writeup(f)
    )
}

/// The line the finding session gets: note it, don't chase it.
pub fn told_finder(f: &Finding, again: bool) -> String {
    let title = f.report.title.trim().trim_end_matches('.');
    let first = if again {
        format!("The bug you reported (\"{title}\") was found before")
    } else if !f.follows.is_empty() {
        format!("Thanks: the bug you reported (\"{title}\") is off this space's goal, and was found before ({}), but its space \"{}\" has closed since", f.follows, f.follows_space)
    } else {
        format!("Thanks: the bug you reported (\"{title}\") is off this space's goal")
    };
    format!(
        "{first}: the tower spun it out to the space \"{}\", where a session of its own works on it (write-up: {}). Note it in your report if it matters there, don't fix it here, and carry on with your own goal.",
        f.spun_to, f.url
    )
}

/// The user's one line.
pub fn told_user(f: &Finding) -> String {
    let title = f.report.title.trim().trim_end_matches('.');
    let before = if f.follows.is_empty() { String::new() } else { format!(" It was found before as {}, whose space \"{}\" has closed since.", f.follows, f.follows_space) };
    format!("A bug {} found off its goal in {} is spun out to the new space \"{}\": {title} ({}).{before}", or_name(&f.report), f.report.space, f.spun_to, f.url)
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

/// Files a report in `book` (the judgment made), every space open.
#[cfg(test)]
pub fn file(book: &mut Book, r: Report, now: u64) -> Filed {
    file_with(book, r, now, |_| true)
}

/// Files a report in `book` (the judgment made): new, or the finding there is.
///
/// A finding's own space (the one it was spun out to) works on that bug, so
/// whatever its sessions report off that space's goal is another bug.
///
/// `open`: whether a space is open (`space_open`). A finding spun out to a
/// space that has closed since is nobody's work any more: the same bug
/// found again is filed new, following it (the latest such one), and
/// opens a space of its own.
pub fn file_with(book: &mut Book, r: Report, now: u64, open: impl Fn(&str) -> bool) -> Filed {
    let own_space = |f: &Finding| !f.spun_to.is_empty() && f.spun_to == r.space;
    let closed = |f: &Finding| f.state == "spun-out" && !open(&f.spun_to);
    let follows = book.findings.iter().rev().find(|f| !own_space(f) && closed(f) && same_bug(&f.report, &r)).map(|f| (f.id.clone(), f.spun_to.clone(), f.url.clone()));
    if let Some(f) = book.findings.iter_mut().find(|f| !own_space(f) && !closed(f) && same_bug(&f.report, &r)) {
        let who = format!("{} › {} ({})", r.space, or_name(&r), r.session);
        let first = format!("{} › {} ({})", f.report.space, or_name(&f.report), f.report.session);
        if who != first && !f.again.contains(&who) {
            f.again.push(who);
        }
        return Filed::Again(f.clone());
    }
    book.next += 1;
    let (follows, follows_space, follows_url) = follows.unwrap_or_default();
    let f = Finding { id: format!("f{}", book.next), report: r, at: now, state: "drafted".into(), follows, follows_space, follows_url, ..Default::default() };
    book.findings.push(f.clone());
    Filed::New(f)
}

/// Whether `space` is open, by the tower's missions: a spun-out finding's
/// space is one `tower launch` made, so one the tower doesn't know (any
/// more) is gone, as one closed (or resolved, which closes it) is.
pub fn space_open(s: &crate::spaces::Spaces, space: &str) -> bool {
    !space.trim().is_empty() && crate::missions::of(s, space).is_some_and(|(_, m)| m.state != crate::missions::State::Closed && m.closed_ms.is_none())
}

/// A draft older than this, or a space that began to open longer ago, is
/// let go: whoever filed it didn't publish it, or its launch died.
pub const CLAIM_MS: u64 = 10 * 60 * 1000;

/// What `published` may do with a finding.
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// Its space is to open now: this call's (its link kept).
    Go(Finding),
    /// Spun out already: its link and space as they are.
    Done(Finding),
    /// Its space is opening, by another call: nothing to do.
    Opening(Finding),
}

/// Claims finding `id` for its space to open, once: two towers that
/// reported one bug, or one that publishes twice, open one space.
pub fn claim(book: &mut Book, id: &str, url: &str, now: u64) -> Option<Claim> {
    let f = book.findings.iter_mut().find(|f| f.id == id)?;
    Some(match f.state.as_str() {
        "spun-out" => Claim::Done(f.clone()),
        "publishing" if now.saturating_sub(f.publishing_at) < CLAIM_MS => Claim::Opening(f.clone()),
        _ => {
            (f.state, f.url, f.publishing_at) = ("publishing".into(), url.into(), now);
            Claim::Go(f.clone())
        }
    })
}

/// Its addresses of whoever found it again ("space › name (t1pd)"), but its own finder's.
fn again_addresses(f: &Finding) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for a in &f.again {
        if let Some(addr) = a.rsplit_once('(').and_then(|(_, r)| r.strip_suffix(')')).map(str::trim)
            && !addr.is_empty()
            && addr != f.report.session
            && !out.iter().any(|o| o == addr)
        {
            out.push(addr.to_string());
        }
    }
    out
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

/// How a report filed new was judged: its words' judgment, or, when the
/// tower filed it with `--off-goal` though its words say on goal, both.
fn judged_line(j: &Judged) -> String {
    if j.off {
        j.why.clone()
    } else if !j.known {
        format!("off its goal by the tower's call (--off-goal): {}", j.why)
    } else {
        format!("off its goal by the tower's call (--off-goal); by its words alone {}", j.why)
    }
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
    let filed = update_at(&path(), |b| file_with(b, r, now, |space| space_open(&s, space)));
    match filed {
        Filed::Again(f) => {
            // Found before: told where it went, once more for this one.
            if f.state == "spun-out" {
                queue_to(from, &told_finder(&f, true))?;
            }
            Ok(json!({ "filed": "again", "id": f.id, "state": f.state, "space": f.spun_to, "url": f.url,
                "next": if f.state == "spun-out" { format!("Found before ({}): {from} is told it is in \"{}\". Nothing new opens; nothing to tell the user.", f.id, f.spun_to) }
                        else if f.state == "publishing" || now.saturating_sub(f.at) < CLAIM_MS {
                            format!("Found before ({}, filed {} s ago): its space is opening or about to, by whoever filed it first, and {from} is told where it went when it opens. Nothing new opens: don't publish it again. Only if you filed {} yourself and haven't published it: publish {} and run `tower finding published {} --url <link>`.", f.id, now.saturating_sub(f.at) / 1000, f.id, dir().join(format!("{}.md", f.id)).display(), f.id)
                        } else { format!("Found before ({}), not yet published: publish {} and run `tower finding published {} --url <link>`.", f.id, dir().join(format!("{}.md", f.id)).display(), f.id) } }))
        }
        Filed::New(f) => {
            let md = dir().join(format!("{}.md", f.id));
            std::fs::create_dir_all(dir())?;
            std::fs::write(&md, writeup(&f))?;
            let before = if f.follows.is_empty() {
                String::new()
            } else {
                format!("Found before as {}, but its space \"{}\" has closed since, so nobody works on it: filed as a new finding, {}, that follows {} (its write-up says so). ", f.follows, f.follows_space, f.id, f.follows)
            };
            Ok(json!({ "filed": "new", "id": f.id, "judged": judged_line(&j), "writeup": md, "follows": f.follows, "follows_space": f.follows_space,
                "next": format!("{before}Publish {} as an HTML page Artifact with your Artifact tool (its words as they are: what was found, where, how to reproduce, evidence, who found it), then run `tower finding published {} --url <its link>`: that opens its space and tells the session and the user.", md.display(), f.id) }))
        }
    }
}

/// `tower finding published <id> --url <link>`: its space opens, the finder and the user are told.
pub fn published(id: &str, url: &str) -> Result<Value> {
    if !url.starts_with("https://") {
        bail!("--url is the artifact's link (https://claude.ai/…), not {url:?}");
    }
    let f = match update_at(&path(), |b| claim(b, id, url, history::now_ms())).with_context(|| format!("no finding {id} (tower finding list)"))? {
        Claim::Go(f) => f,
        Claim::Done(f) => return Ok(json!({ "id": f.id, "space": f.spun_to, "url": f.url, "next": format!("Already spun out to \"{}\" ({}): nothing more to do.", f.spun_to, f.url) })),
        Claim::Opening(f) => return Ok(json!({ "id": f.id, "url": f.url, "next": format!("Its space is opening already ({}): nothing more to do.", f.url) })),
    };
    let r = open_space(&f);
    if r.is_err() {
        // Let go, for a publish once more.
        update_at(&path(), |b| {
            if let Some(f) = b.findings.iter_mut().find(|f| f.id == id && f.state == "publishing") {
                (f.state, f.publishing_at) = ("drafted".into(), 0);
            }
        });
    }
    r
}

/// Opens a claimed finding's space and tells its finders and the user.
fn open_space(f: &Finding) -> Result<Value> {
    let id = f.id.as_str();
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
    // As a fork of the space it was found in would be: right after it, linked to it.
    let placed = place_after(&f.report.space, &space);
    let f = update_at(&path(), |b| {
        let f = b.findings.iter_mut().find(|f| f.id == id)?;
        (f.state, f.spun_to) = ("spun-out".into(), space.clone());
        Some(f.clone())
    })
    .context("the finding went while its space opened")?;
    // The write-up kept with the link, for whoever reads it from the folder.
    let _ = std::fs::write(dir().join(format!("{}.md", f.id)), format!("{}\nArtifact: {}\n", writeup(&f), f.url));
    let told = queue_to(&f.report.session, &told_finder(&f, false));
    // Whoever found it again before it opened: where it went.
    for addr in again_addresses(&f) {
        let _ = queue_to(&addr, &told_finder(&f, true));
    }
    // Said, unless where it was found is muted (what happens there isn't said).
    if !crate::spaces::muted_in(&crate::spaces::load(), &f.report.space) {
        say(&told_user(&f));
    }
    history::append(history::Entry { kind: "finding".into(), space: Some(f.report.space.clone()), session: Some(f.report.session.clone()), text: told_user(&f), record: true, ..Default::default() });
    Ok(json!({ "id": f.id, "space": space, "spun_from": f.report.space, "placed": placed, "url": f.url, "finder_told": told.is_ok(), "launch": r }))
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

    /// 2026-11-28: a nightly smoke check was fixed for its first cause and
    /// its finding's space closed; a session then found a new cause of the
    /// same symptom, and it was filed "again" into the closed space, where
    /// nobody would work on it.
    #[test]
    fn a_bug_found_again_after_its_space_closed_opens_a_new_finding_that_follows_it() {
        use crate::missions::{Mission, State};
        let places = [".github/workflows/nightly.yml:40-60"];
        let mut b = Book { next: 1, ..Default::default() };
        let first = rep("The nightly's Dev smoke fails every night: its public upload link expired", "The nightly's Dev smoke step uploads a fixture and reads it back through a public link, which has expired.", "/w/media-api", &places);
        let Filed::New(f2) = file(&mut b, first, 1) else { panic!("new") };
        let Some(Claim::Go(_)) = claim(&mut b, &f2.id, "https://claude.ai/artifact/f2", 2) else { panic!("go") };
        (b.findings[0].state, b.findings[0].spun_to) = ("spun-out".into(), "bug-nightly-smoke".into());
        let mut again = rep("The nightly's Dev smoke fails every night: its default workflow times out", "The nightly's Dev smoke step fails again: the default workflow it starts times out.", "/w/media-api", &places);
        (again.space, again.session, again.session_name) = ("inbox".into(), "t41".into(), "mail".into());
        assert!(same_bug(&f2.report, &again), "the same symptom: a match");
        // The spaces as the tower's missions have them.
        let mut s = crate::spaces::Spaces::default();
        s.missions.insert("bug-nightly-smoke".into(), Mission::born(1));
        s.missions.insert("inbox".into(), Mission::born(1));
        // While its space is open, the same bug is the finding there is: nothing new opens.
        let mut open_book = b.clone();
        assert!(matches!(file_with(&mut open_book, again.clone(), 3, |sp| space_open(&s, sp)), Filed::Again(ref f) if f.id == "f2"));
        // Its space closes (fixed for its first cause, or resolved).
        let m = s.missions.get_mut("bug-nightly-smoke").unwrap();
        (m.state, m.closed_ms) = (State::Closed, Some(4));
        assert!(!space_open(&s, "bug-nightly-smoke") && space_open(&s, "inbox"));
        assert!(!space_open(&s, "never-made"), "a space the tower doesn't know is gone");
        // Reported again: a new finding, following the old one.
        let Filed::New(f) = file_with(&mut b, again.clone(), 5, |sp| space_open(&s, sp)) else { panic!("its space closed: a new finding") };
        assert_eq!((f.id.as_str(), f.follows.as_str(), f.follows_space.as_str(), f.follows_url.as_str()), ("f3", "f2", "bug-nightly-smoke", "https://claude.ai/artifact/f2"));
        assert!(b.findings[0].again.is_empty(), "not filed into the closed one");
        let w = writeup(&f);
        assert!(w.contains("## Found before") && w.contains("found before as f2 and spun out to the space \"bug-nightly-smoke\" (https://claude.ai/artifact/f2), which has closed since"), "{w}");
        // Spun out: the finder and the user are told it follows f2.
        b.findings[1].state = "publishing".into();
        let mut out = f.clone();
        (out.state, out.spun_to, out.url) = ("spun-out".into(), "inbox › bug-nightly-smoke".into(), "https://claude.ai/artifact/f3".into());
        assert!(told_user(&out).ends_with("It was found before as f2, whose space \"bug-nightly-smoke\" has closed since."), "{}", told_user(&out));
        assert!(told_finder(&out, false).contains("was found before (f2), but its space \"bug-nightly-smoke\" has closed since: the tower spun it out to the space \"inbox › bug-nightly-smoke\""), "{}", told_finder(&out, false));
        // Its new space open, the next report of it is that one, not a third.
        b.findings[1] = out.clone();
        s.missions.insert(out.spun_to.clone(), Mission::born(6));
        let mut third = again;
        (third.space, third.session, third.session_name) = ("sandbox".into(), "t40".into(), "api".into());
        assert!(matches!(file_with(&mut b, third, 7, |sp| space_open(&s, sp)), Filed::Again(ref f) if f.id == "f3"));
        assert_eq!(b.findings.len(), 2);
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

    /// f31 and f32 (2026-11-19): one sign-up bug, reported by the main tower
    /// and then by the space's own tower, from one session's account, ten
    /// seconds apart, in other words and with its places written otherwise.
    fn sign_up_reports() -> (Report, Report) {
        let mut main_tower = rep(
            "Sign-up drops the newsletter opt-in on sooner-app-web",
            "RegisterForm sends the newsletter opt-in as 'mailing', but pickProfileFields in lib/functions/user.js only forwards marketing_email_opt_in to PATCH /auth/me, so a viewer who opts in at registration is stored as opted out.",
            "/w/eyz/sooner-app-web",
            &["lib/functions/user.js (pickProfileFields)", "RegisterForm"],
        );
        let mut space_tower = rep(
            "Sign-up drops the newsletter opt-in, so new viewers are saved with marketing_email_opt_in false",
            "A viewer who ticks the newsletter box at registration is saved with marketing_email_opt_in false. RegisterForm sends the opt-in as mailing; createUserData passes the input through pickProfileFields, whose allow-list has marketing_email_opt_in but not mailing, so PATCH /auth/me never gets the opt-in. Only the legacy AppSync mirror in lib/legacy/registerUser.js receives mailing.",
            "/w/eyz/sooner-app-web",
            &["components/auth/RegisterForm.js:109", "lib/functions/user.js:17-21"],
        );
        for r in [&mut main_tower, &mut space_tower] {
            (r.space, r.session, r.session_name) = ("sooner-help-faqs \u{203a} bug-account-settings".into(), "t1pd".into(), "sooner-app".into());
        }
        (main_tower, space_tower)
    }

    #[test]
    fn one_bug_reported_by_two_towers_is_one_finding() {
        let (first, second) = sign_up_reports();
        let mut b = Book { next: 30, ..Default::default() };
        let Filed::New(f31) = file(&mut b, first, 1) else { panic!("new") };
        assert_eq!(f31.id, "f31");
        // Not yet published when the second report comes: still the same one.
        assert_eq!(f31.state, "drafted");
        match file(&mut b, second, 2) {
            Filed::Again(f) => assert_eq!(f.id, "f31", "matched to the first"),
            Filed::New(f) => panic!("filed again as {}", f.id),
        }
        assert_eq!(b.findings.len(), 1);
        assert!(f31.again.is_empty() && b.findings[0].again.is_empty(), "one finder (t1pd), not listed twice");

        // Either of its signals alone: its places, however written ...
        let (first, mut second) = sign_up_reports();
        second.what = "A viewer who ticks the newsletter box at registration is saved as opted out.".into();
        assert!(same_bug(&first, &second), "the same files: user.js (pickProfileFields) and user.js:17-21, RegisterForm and RegisterForm.js:109");
        // ... or the names in code its accounts give, with no places at all.
        let (mut first, mut second) = sign_up_reports();
        (first.places, second.places) = (vec![], vec![]);
        assert!(same_bug(&first, &second), "pickProfileFields, RegisterForm, marketing_email_opt_in in both");

        // Another bug of the same repo and field (f30, the settings toggle): its own finding.
        let mut settings = rep(
            "Account settings shows the newsletter toggle off for every viewer",
            "Account Settings shows the newsletter toggle off for every viewer, because a mount effect resets it from user.mailing, a legacy field the eyzflow user object doesn't carry, while the form starts from and saves marketing_email_opt_in.",
            "/w/eyz/sooner-app-web",
            &["components/account/SettingsSection.js:22-25", "components/account/SettingsSection.js:16"],
        );
        settings.space = "sooner-help-faqs".into();
        let (first, second) = sign_up_reports();
        assert!(!same_bug(&settings, &first) && !same_bug(&settings, &second));
        // A place's note and a bare name, as places.
        assert_eq!(file_of("lib/functions/user.js (pickProfileFields)"), "lib/functions/user.js");
        assert_eq!(file_of("`src/a.rs:12-14`"), "src/a.rs");
        assert!(same_file("registerform", "components/auth/registerform.js") && same_file("user.js", "lib/functions/user.js"));
        assert!(!same_file("user.js", "lib/functions/superuser.js") && !same_file("lib", "lib/functions/user.js") && !same_file("index.js", "src/index.ts"));
    }

    /// f56 (2026-11-25): the logo link's English screen-reader label, spun
    /// out to "sooner-support-deploy › bug-logo-links".
    fn logo_finding() -> Finding {
        let mut r = rep(
            "Logo link's screen-reader label stays English on German pages",
            "The logo link's aria-label stays English (\"Sooner, home\") on German pages. No German translation exists: t() looks it up in the remote eyzflow translations and gets nothing back, and SupportTicketPage and Footer hardcode it.",
            "/w/eyz/sooner-app-web",
            &["components/client/HelpCenterBar.js:70", "components/client/Navbar.js:148", "components/client/SupportTicketPage.js:16", "components/server/Footer.js:95"],
        );
        (r.space, r.session, r.session_name) = ("sooner-support-deploy".into(), "t221".into(), "sooner-support-deploy-sooner-app-web".into());
        Finding { id: "f56".into(), report: r, at: 1, state: "spun-out".into(), url: "https://claude.ai/artifact/f56".into(), spun_to: "sooner-support-deploy \u{203a} bug-logo-links".into(), ..Default::default() }
    }

    /// What t224, a session of f56's own space, reported: another bug in
    /// two of f56's files, as its tower filed it four times (the first three
    /// came back as f56).
    fn nav_and_footer_reports() -> Vec<Report> {
        let navbar_footer = ["components/client/Navbar.js:151", "components/server/Footer.js:110"];
        let reports = [
            rep(
                "Navbar and Footer screen-reader labels stay English on German pages",
                "Two more screen-reader labels stay English on German pages: Navbar's nav aria-label t('Primary') has no entry in translations/sooner/de.json, and Footer hardcodes aria-label='Footer links N'. Not fixed in PR #156 (out of its scope, which was the logo link's label).",
                "/w/eyz/sooner-app-web",
                &navbar_footer,
            ),
            rep(
                "Main nav and footer link groups have English screen-reader names on German pages",
                "Not the logo link (f56): the Navbar's <nav aria-label={t('Primary')}> has no German entry in translations/sooner/de.json, so it renders 'Primary'; the Footer hardcodes aria-label='Footer links N' on its link groups, untranslated.",
                "/w/eyz/sooner-app-web",
                &navbar_footer,
            ),
            rep(
                "Main nav and footer link groups have English screen-reader names on German pages",
                "Not the logo link (f56): the Navbar's nav aria-label t('Primary') (components/client/Navbar.js:151) has no German entry in translations/sooner/de.json, so it renders 'Primary'; the Footer hardcodes aria-label='Footer links N' on its link groups (components/server/Footer.js:110), untranslated.",
                "/w/eyz/sooner-app-web",
                &[],
            ),
            rep(
                "Main nav and footer link groups have English screen-reader names on German pages",
                "Not the logo link (f56): the Navbar's nav aria-label t('Primary') (Navbar.js line 151) has no German entry in translations/sooner/de.json, so it renders 'Primary'; the Footer hardcodes aria-label='Footer links N' on its link groups (Footer.js line 110), untranslated.",
                "/w/eyz/sooner-app-web",
                &[],
            ),
        ];
        reports
            .into_iter()
            .map(|mut r| {
                (r.space, r.session, r.session_name) = ("sooner-support-deploy \u{203a} bug-logo-links".into(), "t224".into(), "sooner-support-deploy-bug-logo-links-sooner-app".into());
                r
            })
            .collect()
    }

    #[test]
    fn another_bug_in_a_findings_files_from_its_own_space_is_filed_new() {
        let f56 = logo_finding();
        for (i, r) in nav_and_footer_reports().into_iter().enumerate() {
            // From f56's own space: a new finding, f57, and f56 doesn't list t224 as finding it again.
            let mut b = Book { findings: vec![f56.clone()], next: 56 };
            match file(&mut b, r.clone(), 2) {
                Filed::New(f) => assert_eq!(f.id, "f57", "report {i}"),
                Filed::Again(f) => panic!("report {i} filed again as {}", f.id),
            }
            assert!(b.findings[0].again.is_empty(), "report {i}");
            // The same report from any other space: new on what it says alone
            // (other lines of Navbar.js and Footer.js, another title and account).
            let mut elsewhere = r;
            (elsewhere.space, elsewhere.session) = ("sooner-help-faqs".into(), "t300".into());
            assert!(!same_bug(&f56.report, &elsewhere), "report {i}");
        }
        // The first one's title alone is as alike as before (7 of 11 words), but its lines are elsewhere.
        let first = &nav_and_footer_reports()[0];
        assert!(overlap(&words(&f56.report.title), &words(&first.title)) >= 0.6);

        // Its space alone can say so: the main nav's label, in Navbar.js
        // with no line, reads as f56 to a tower anywhere else (the same file,
        // titles somewhat alike), but from f56's own space it is another bug.
        let mut nav = nav_and_footer_reports().remove(1);
        nav.places = vec!["components/client/Navbar.js".into()];
        assert!(same_bug(&f56.report, &nav));
        let mut b = Book { findings: vec![f56.clone()], next: 56 };
        assert!(matches!(file(&mut b, nav, 2), Filed::New(ref f) if f.id == "f57"));
        assert_eq!(compare_spots(&f56.report, first), Spots::Apart);
    }

    #[test]
    fn the_same_bug_in_the_same_files_is_still_found_again() {
        let f56 = logo_finding();
        let elsewhere = |title: &str, what: &str, places: &[&str]| {
            let mut r = rep(title, what, "/w/eyz/sooner-app-web", places);
            (r.space, r.session) = ("sooner-help-faqs".into(), "t300".into());
            r
        };
        // At its line, or the line next to it, in other words.
        for line in ["148", "150"] {
            let r = elsewhere("The logo's aria-label 'Sooner, home' is English on German pages", "On /de pages the logo link reads 'Sooner, home'.", &[&format!("components/client/Navbar.js:{line}")]);
            assert!(same_bug(&f56.report, &r), "line {line}");
        }
        // No line given on one side: the same file is enough, as before.
        let r = elsewhere("The logo's aria-label 'Sooner, home' is English on German pages", "", &["Navbar.js"]);
        assert!(same_bug(&f56.report, &r));
        // Its lines moved on another branch, its words all but the same.
        let r = elsewhere("Logo link's screen-reader label stays English on German pages", "", &["components/client/Navbar.js:162"]);
        assert!(same_bug(&f56.report, &r));
        let mut b = Book { findings: vec![f56.clone()], next: 56 };
        assert!(matches!(file(&mut b, r, 2), Filed::Again(ref f) if f.id == "f56"));

        // A session of f56's own space that finds another finding's bug: that one, found again.
        let (sign_up, _) = sign_up_reports();
        let mut b = Book { findings: vec![f56.clone(), Finding { id: "f31".into(), report: sign_up.clone(), state: "spun-out".into(), ..Default::default() }], next: 56 };
        let mut again = sign_up;
        (again.space, again.session) = (f56.spun_to.clone(), "t224".into());
        assert!(matches!(file(&mut b, again, 2), Filed::Again(ref f) if f.id == "f31"));
    }

    #[test]
    fn a_places_lines_are_read_from_places_and_accounts() {
        assert_eq!(lines_of("components/client/Navbar.js:151"), Some((151, 151)));
        assert_eq!(lines_of("lib/functions/user.js:17-21"), Some((17, 21)));
        assert_eq!(lines_of("src/a.rs#L12-L20"), Some((12, 20)));
        assert_eq!(lines_of("lib/functions/user.js (pickProfileFields)"), None);
        assert_eq!(lines_of("RegisterForm"), None);
        assert_eq!(text_places("aria-label t('Primary') (components/client/Navbar.js:151) has no entry; see Footer.js:110."), ["components/client/Navbar.js:151", "Footer.js:110"]);
        assert!(text_places("at 10:30, PATCH /auth/me, translations/sooner/de.json").is_empty());
        // In words, as f57 and t229 wrote them.
        assert_eq!(
            text_places("t('Primary') (Navbar.js line 151) has no entry; app/[locale]/layout.js line 38 hardcodes it; see Footer.js lines 108-112."),
            ["Navbar.js:151", "app/[locale]/layout.js:38", "Footer.js:108-112"]
        );
        assert!(text_places("the line 151 of it, a file line").is_empty());
        assert_eq!(lines_of("Footer.js:108-112"), Some((108, 112)));
    }

    /// f57 (2026-11-25): the main nav and footer link groups, spun out to
    /// "… › bug-logo-links › bug-main-nav"; its lines only in its account.
    fn nav_finding() -> Finding {
        let r = nav_and_footer_reports().remove(3);
        Finding { id: "f57".into(), report: r, at: 2, state: "spun-out".into(), url: "https://claude.ai/artifact/f57".into(), spun_to: "sooner-support-deploy \u{203a} bug-logo-links \u{203a} bug-main-nav".into(), ..Default::default() }
    }

    /// What t229, a session of f57's own space, reported: a third bug (the
    /// mobile-menu toggle at Navbar.js:287, the skip link at layout.js:38),
    /// as its towers filed it. The first two came back as f56, the third as
    /// f57; only the fourth, reworded, was filed new (f58).
    fn menu_and_skip_link_reports() -> Vec<Report> {
        let places = ["components/client/Navbar.js:287", "app/[locale]/layout.js:38"];
        let repo = "/w/eyz/sooner-app-web";
        [
            rep(
                "Mobile-menu button and skip link are announced in English on German pages",
                "On German pages the Navbar's mobile-menu toggle uses t('Open menu') / t('Close menu'), which have no entries in translations/sooner/de.json, so its aria-label stays English; the layout hardcodes the skip link's text 'Skip to main content'. Found while fixing the nav landmarks' names (PR #157 covers only the nav landmarks).",
                repo,
                &places,
            ),
            rep(
                "Mobile-menu toggle 'Open menu'/'Close menu' and the 'Skip to main content' skip link stay English on German pages (not the logo link, f56)",
                "Distinct from f56 (logo link label) and f57 (nav landmark names); PR #156 and #157 change neither. The Navbar's mobile-menu button uses t('Open menu') / t('Close menu') with no entries in translations/sooner/de.json, and app/[locale]/layout.js hardcodes the skip link text 'Skip to main content'.",
                repo,
                &places,
            ),
            rep(
                "Mobile menu button and skip link stay English on German pages",
                "Not the logo link (f56) nor the nav/footer labels (f57): the mobile-menu button's t('Open menu') / t('Close menu') (Navbar.js line 287) has no entry in translations/sooner/de.json, and app/[locale]/layout.js line 38 hardcodes 'Skip to main content'. Both stay English on German pages; neither PR #156 nor #157 touches them.",
                repo,
                &[],
            ),
            rep(
                "Hamburger toggle and skip-to-content link untranslated in the de locale",
                "The hamburger toggle's t('Open menu') / t('Close menu') (Navbar.js line 287) has no key in translations/sooner/de.json, and app/[locale]/layout.js line 38 hardcodes 'Skip to main content'. Separate from f56 and f57; PRs #156 and #157 don't touch either.",
                repo,
                &[],
            ),
        ]
        .into_iter()
        .map(|mut r| {
            (r.space, r.session, r.session_name) = ("sooner-support-deploy \u{203a} bug-logo-links \u{203a} bug-main-nav".into(), "t229".into(), "sooner-support-deploy-bug-logo-links-bug-main-na".into());
            r
        })
        .collect()
    }

    #[test]
    fn a_third_bug_from_a_spun_out_findings_space_is_filed_new() {
        let (f56, f57) = (logo_finding(), nav_finding());
        for (i, r) in menu_and_skip_link_reports().into_iter().enumerate() {
            // From f57's own space, with f56 (its parent's finding) in the book too: f58.
            let mut b = Book { findings: vec![f56.clone(), f57.clone()], next: 57 };
            match file(&mut b, r.clone(), 3) {
                Filed::New(f) => assert_eq!(f.id, "f58", "report {i}"),
                Filed::Again(f) => panic!("report {i} filed again as {}", f.id),
            }
            assert!(b.findings.iter().all(|f| f.again.is_empty()), "report {i}");
            // From any other space too: other lines of Navbar.js (287, not
            // 148 or 151), and words in common (English, German pages,
            // label, translations/sooner/de.json) aren't enough.
            let mut elsewhere = r;
            (elsewhere.space, elsewhere.session) = ("sooner-help-faqs".into(), "t300".into());
            assert!(!same_bug(&f56.report, &elsewhere) && !same_bug(&f57.report, &elsewhere), "report {i}");
        }
        // f57's lines, though written in words, are read: Navbar.js 151 against 287.
        assert_eq!(compare_spots(&f57.report, &menu_and_skip_link_reports()[2]), Spots::Apart);
        // And f57's own reports from f56's space are still new beside f56.
        for r in nav_and_footer_reports() {
            let mut b = Book { findings: vec![f56.clone()], next: 56 };
            assert!(matches!(file(&mut b, r, 2), Filed::New(_)));
        }
    }

    #[test]
    fn a_report_filed_off_goal_by_the_tower_says_so() {
        let on = Judged { off: false, known: true, why: "on its goal: it is about link, main".into() };
        assert_eq!(judged_line(&on), "off its goal by the tower's call (--off-goal); by its words alone on its goal: it is about link, main");
        let off = Judged { off: true, known: true, why: "off its goal: only label in common".into() };
        assert_eq!(judged_line(&off), "off its goal: only label in common");
        let none = Judged { off: false, known: false, why: "the space has no goal to judge it by".into() };
        assert!(judged_line(&none).starts_with("off its goal by the tower's call (--off-goal)"));
    }

    #[test]
    fn two_towers_publishing_one_finding_open_one_space() {
        let (first, second) = sign_up_reports();
        let mut b = Book { next: 30, ..Default::default() };
        let Filed::New(f) = file(&mut b, first, 1_000) else { panic!("new") };
        // The space tower's report of it comes while the main tower publishes it.
        let mut other = second;
        (other.session, other.session_name) = ("t1pm".into(), "other".into());
        assert!(matches!(file(&mut b, other, 2_000), Filed::Again(_)));
        let Some(Claim::Go(g)) = claim(&mut b, &f.id, "https://claude.ai/artifact/first", 3_000) else { panic!("the main tower's") };
        assert_eq!(g.url, "https://claude.ai/artifact/first");
        // The space tower publishes it too: its space is opening, its link isn't kept.
        let Some(Claim::Opening(o)) = claim(&mut b, &f.id, "https://claude.ai/artifact/second", 4_000) else { panic!("opening") };
        assert_eq!(o.url, "https://claude.ai/artifact/first");
        // Spun out: done, as it is.
        b.findings[0].state = "spun-out".into();
        assert!(matches!(claim(&mut b, &f.id, "https://claude.ai/artifact/third", 5_000), Some(Claim::Done(ref d)) if d.url == "https://claude.ai/artifact/first"));
        // A launch that died long ago is let go.
        b.findings[0].state = "publishing".into();
        assert!(matches!(claim(&mut b, &f.id, "https://claude.ai/artifact/again", 3_000 + CLAIM_MS), Some(Claim::Go(_))));
        assert!(claim(&mut b, "f99", "https://x", 0).is_none());
        // Whoever found it again is told where it went, its first finder once.
        let mut f = b.findings[0].clone();
        f.again.push("s › first (t1pd)".into());
        assert_eq!(again_addresses(&f), ["t1pm"]);
    }

    #[test]
    fn a_spun_out_space_goes_right_after_the_one_it_came_from() {
        let order = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        // Made last, as a new space is: moved to just after its source.
        let now = order(&["sandbox", "search-llm", "support", "M5 Pro", "support \u{203a} bug-x"]);
        assert_eq!(fork_position(&now, "support", "support \u{203a} bug-x"), Some(4));
        // Its source is the last one, or it is right after it already: nothing to move.
        assert_eq!(fork_position(&order(&["a", "support", "support \u{203a} bug-x"]), "support", "support \u{203a} bug-x"), None);
        // Before its source (moved by hand): its source moves up once it is taken out.
        assert_eq!(fork_position(&order(&["support \u{203a} bug-x", "a", "support", "b"]), "Support", "support \u{203a} bug-x"), Some(3));
        // Either not in the window: nothing.
        assert_eq!(fork_position(&now, "gone", "support \u{203a} bug-x"), None);
        assert_eq!(fork_position(&now, "support", "bug-y"), None);
        // Its tower is told where it was spun out of; its mission keeps the link through a reopen.
        let section = crate::brains::space_section("support \u{203a} bug-x", "tower-support-bug-x", std::path::Path::new("/b"), &serde_json::json!({ "goal": "Fix: x", "spun_from": "support" }));
        assert!(section.contains("spun out of the space \"support\""), "{section}");
        let mut m = crate::missions::Mission { spun_from: Some("support".into()), ..crate::missions::Mission::born(1) };
        m.reopen(2);
        assert_eq!(m.spun_from.as_deref(), Some("support"));
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
        // Named after the space it was found in (Herdr has no fork of a space), never one there is.
        assert_eq!(space_for(&f, &[]), "kb-refresh \u{203a} bug-video-upload");
        assert_eq!(space_for(&f, &["kb-refresh \u{203a} bug-video-upload".to_string()]), "kb-refresh \u{203a} bug-video-upload-2", "never one there is");
        let mut odd = f.clone();
        odd.report.title = "stats.mean([]) raises ZeroDivisionError on an empty list".into();
        assert_eq!(space_for(&odd, &[]), "kb-refresh \u{203a} bug-stats-mean");
        // Found where no space is known: as before.
        let mut nowhere = odd.clone();
        nowhere.report.space = String::new();
        assert_eq!(space_for(&nowhere, &[]), "bug-stats-mean");
        // Its slug (worktrees, branches, its tower's name) stays plain.
        assert_eq!(crate::worktrees::slug(&space_for(&f, &[])), "kb-refresh-bug-video-upload");
        assert!(ticket(&f).contains("https://claude.ai/artifact/x") && ticket(&f).contains("## Evidence"));
        assert!(told_finder(&f, false).contains("carry on with your own goal") && told_finder(&f, false).contains("\"fix-video\""));
        let line = told_user(&f);
        assert!(line.contains("\"fix-video\"") && line.contains("https://claude.ai/artifact/x") && !line.contains('\n'), "{line}");
    }
}
