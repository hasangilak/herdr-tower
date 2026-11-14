//! Herdr spaces in the tower, and focus.
//!
//! The user works in several spaces at once ("B2B ticketing", "B2C
//! support"), each with its own sessions. With a space in focus, its news is
//! spoken as always. From every other space, focus quiets what merely
//! happened there, and never what the user is waiting on:
//!
//! - **Asked for**: a report on work the brain handed out (a watch, an open
//!   job) reaches the brain wherever it is, marked as from a space out of
//!   focus, and the brain says it in one short line naming the space.
//! - **Needs you**: someone there is blocked on the user or waits for them
//!   (to merge, approve, test, decide): a bell and the request, in a line
//!   that names the space (`alerts.held_urgent`).
//! - **Ambient**: everything else (a session finishing that nobody watched,
//!   the send queue's notes): held, not spoken, kept as unread for the
//!   panel, the phone and "what did I miss".
//!
//! No focus: everything is spoken, as before. A focus ends by itself
//! `alerts.focus_hours` (8) after it was set, so one left on doesn't keep
//! holding news for days; setting it again starts it over. Talking about
//! another space doesn't move it: work the user starts there comes through
//! anyway (it is asked for), and its held news is on the radar line.
//!
//! `spaces.json` (beside `state.json`, which the orb watches) keeps the
//! focus, each space's agenda (its goal), how far its held news has been
//! read, which spaces the user follows (their finished requests' receipts
//! reach the user whatever the tell level is: see `tell`), which are muted
//! (their decisions stay out of the main queue and the orb's count: see
//! `decisions`), and each space as a mission with an end (`missions`: what
//! counts as done, whether it is open, quiet, ready to close or closed, its
//! closing report, the worktrees the tower made for it: see `crate::missions`).

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config;
use crate::history::{self, Entry};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Spaces {
    /// The space whose news is spoken; None: every space's.
    pub focus: Option<String>,
    /// When the focus was set (Unix ms): it ends `alerts.focus_hours` later.
    /// None for a focus an older tower set: dated when first seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus_ms: Option<u64>,
    /// What each space is for, set by the user or the brain.
    pub agendas: BTreeMap<String, String>,
    /// Held news up to this time (Unix ms) has been delivered.
    pub read_ms: BTreeMap<String, u64>,
    /// Passed over in a catch-up at this time; it comes round again with newer news.
    pub skipped_ms: BTreeMap<String, u64>,
    /// Spaces the user follows (`tower space follow`): their results reach
    /// the user as they land.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub follow: std::collections::BTreeSet<String>,
    /// Spaces the user muted (`tower space mute`): what needs them there
    /// stays in that space's own view, off the orb's count and out of the
    /// main queue, and isn't said.
    #[serde(skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub mute: std::collections::BTreeSet<String>,
    /// Each space as a mission (see `crate::missions`), by its name. Its
    /// goal is its agenda above.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missions: BTreeMap<String, crate::missions::Mission>,
}

/// Whether `space` is muted, by its name (any case).
pub fn muted_in(s: &Spaces, space: &str) -> bool {
    let want = space.trim();
    !want.is_empty() && s.mute.iter().any(|m| m.eq_ignore_ascii_case(want))
}

/// Mutes a space, or lets it be heard again. True when that changed something.
pub fn set_mute(s: &mut Spaces, space: &str, on: bool) -> bool {
    let was = muted_in(s, space);
    if on && !was {
        s.mute.insert(space.trim().to_string());
    } else if !on && was {
        s.mute.retain(|m| !m.eq_ignore_ascii_case(space.trim()));
    }
    was != on
}

/// Whether `space` is followed, by its name (any case).
pub fn followed_in(s: &Spaces, space: &str) -> bool {
    let want = space.trim();
    !want.is_empty() && s.follow.iter().any(|f| f.eq_ignore_ascii_case(want))
}

/// Whether the user follows `space` now.
pub fn followed(space: &str) -> bool {
    followed_in(&load(), space)
}

/// Follows a space, or stops. True when that changed something.
pub fn set_follow(s: &mut Spaces, space: &str, on: bool) -> bool {
    let was = followed_in(s, space);
    if on && !was {
        s.follow.insert(space.trim().to_string());
    } else if !on && was {
        s.follow.retain(|f| !f.eq_ignore_ascii_case(space.trim()));
    }
    was != on
}

pub fn path_in(ui_dir: &Path) -> PathBuf {
    ui_dir.join("spaces.json")
}

pub fn load_from(ui_dir: &Path) -> Spaces {
    std::fs::read_to_string(path_in(ui_dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn load() -> Spaces {
    load_from(&config::ui_dir())
}

pub fn save_to(ui_dir: &Path, s: &Spaces) {
    let _ = std::fs::create_dir_all(ui_dir);
    // Written beside the watched folder, then moved in: the orb wakes once.
    let tmp = ui_dir.parent().unwrap_or(ui_dir).join(format!(".spaces.json.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path_in(ui_dir));
    }
}

/// Reads `spaces.json`, changes it and writes it back when it changed, one
/// writer at a time (the voice's look at the missions and a `tower space`
/// command never undo each other).
pub fn update_in<R>(ui_dir: &Path, f: impl FnOnce(&mut Spaces) -> R) -> R {
    use std::os::fd::AsRawFd;
    let dir = ui_dir.parent().unwrap_or(ui_dir);
    let _ = std::fs::create_dir_all(dir);
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".spaces.lock")).ok();
    if let Some(l) = &lock {
        // SAFETY: flock on a file we hold open; released when it closes.
        unsafe { libc::flock(l.as_raw_fd(), libc::LOCK_EX) };
    }
    // A file that is there and doesn't read (a hand edit, a word a later tower wrote) is about to
    // be written over with what this one knows: its goals, kept spaces and worktrees are kept aside first.
    let text = std::fs::read_to_string(path_in(ui_dir)).ok();
    let unreadable = text.as_deref().filter(|t| !t.trim().is_empty() && serde_json::from_str::<Spaces>(t).is_err());
    let mut s = load_from(ui_dir);
    let before = s.clone();
    let r = f(&mut s);
    if s != before {
        if let Some(text) = unreadable {
            let aside = dir.join(format!("spaces.bad-{}.json", history::now_ms()));
            match std::fs::write(&aside, text) {
                Ok(()) => eprintln!("[spaces] {} doesn't read; kept as {} before it is written again", path_in(ui_dir).display(), aside.display()),
                Err(e) => eprintln!("[spaces] {} doesn't read, and couldn't be kept aside ({e})", path_in(ui_dir).display()),
            }
        }
        save_to(ui_dir, &s);
    }
    r
}

pub fn update<R>(f: impl FnOnce(&mut Spaces) -> R) -> R {
    update_in(&config::ui_dir(), f)
}

/// The focus as it stands at `now`: none once `hours` have passed since it
/// was set (`hours` 0: it never ends).
pub fn active_focus(s: &Spaces, now_ms: u64, hours: u64) -> Option<&str> {
    let f = s.focus.as_deref()?;
    match s.focus_ms {
        Some(at) if hours > 0 && now_ms.saturating_sub(at) >= hours * 3_600_000 => None,
        _ => Some(f),
    }
}

/// The focus now, for the daemon: a focus past its hours is cleared (and
/// said in the log); one an older tower set without a time is dated now.
pub fn focus_now(hours: u64) -> Option<String> {
    let s = load();
    let f = s.focus.clone()?;
    let now = history::now_ms();
    // (Each change under the file's lock: the missions are kept in the same file.)
    if s.focus_ms.is_none() {
        update(|s| {
            if s.focus.is_some() && s.focus_ms.is_none() {
                s.focus_ms = Some(now);
            }
        });
        return Some(f);
    }
    if active_focus(&s, now, hours).is_none() {
        eprintln!("[spaces] the focus on {f} ended after {hours} h: every space's news is spoken again");
        update(|s| {
            if s.focus.as_deref() == Some(f.as_str()) {
                (s.focus, s.focus_ms) = (None, None);
            }
        });
        return None;
    }
    Some(f)
}

/// What a piece of news is to the user, for the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum News {
    /// Something happened there that nobody asked to hear about.
    Ambient,
    /// A report on work the user asked for: the brain watches only what it
    /// handed out (a watch, or an open job).
    Asked,
    /// Someone there is blocked on the user, or waits for them.
    NeedsYou,
}

impl News {
    /// A report's kind (`watch`, `job`, `alert`, `queue`) and status.
    pub fn of(kind: &str, status: &str) -> News {
        match (kind, status) {
            // A watch's or a job's report; a PR the user merged from a session's work; the agenda ledger's notes.
            ("watch" | "job" | "pr" | "agenda", _) => News::Asked,
            (_, "blocked") => News::NeedsYou,
            _ => News::Ambient,
        }
    }
}

/// What to do with news from a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Say it, as always.
    Speak,
    /// Say it, though its space is out of focus: work the user asked for.
    /// The brain is told so (`out_of_focus`) and names the space.
    Through,
    /// Keep it for later: unread, not spoken.
    Hold,
    /// Keep it, but a bell and the request, naming the space, now.
    Urgent,
}

/// News from `space` (None: a session in no space), with `focus` set or not.
/// `urgent_cue`: someone needing the user breaks through the focus (config
/// `alerts.held_urgent`).
pub fn gate(space: Option<&str>, focus: Option<&str>, news: News, urgent_cue: bool) -> Gate {
    match (focus, space) {
        (None, _) | (_, None) => Gate::Speak,
        (Some(f), Some(s)) if f == s => Gate::Speak,
        _ => match news {
            News::Asked => Gate::Through,
            News::NeedsYou if urgent_cue => Gate::Urgent,
            _ => Gate::Hold,
        },
    }
}

/// What the brain is told after a report that comes through the focus.
pub fn out_of_focus(space: &str, focus: &str) -> String {
    format!(
        " This is from the space {space}, not the one in focus ({focus}), but it's work the user asked for: tell them in one short line that starts with \"In {space}\" (and what they need to do, if anything)."
    )
}

/// The short line said when someone in an unfocused space needs the user:
/// the request itself when it's short, else just that they're needed.
pub fn needs_you_line(space: &str, text: &str) -> String {
    let t = text.trim().trim_end_matches(['.', '!']);
    if t.is_empty() || t.chars().count() > 160 { format!("{space} needs you.") } else { format!("In {space}: {t}.") }
}

/// The space a terminal is in: Herdr's `where` label reads "<space> / <tab>".
pub fn space_of(t: &Value) -> Option<String> {
    t["labels"]["where"].as_str().and_then(|w| w.split(" / ").next()).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Keeps news from an unfocused space.
pub fn hold(kind: &str, session: &str, name: &str, space: &str, text: &str, status: Option<&str>) {
    history::append(Entry {
        kind: kind.into(),
        session: Some(session.into()),
        name: Some(name.into()).filter(|n: &String| !n.is_empty()),
        space: Some(space.into()),
        status: status.map(String::from),
        text: text.into(),
        held: true,
        ..Default::default()
    });
}

/// Held news not yet delivered, by space, oldest first.
pub fn unread<'a>(entries: &'a [Entry], s: &Spaces) -> BTreeMap<String, Vec<&'a Entry>> {
    let mut out: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.held) {
        let Some(space) = e.space.as_ref() else { continue };
        if e.at > s.read_ms.get(space).copied().unwrap_or(0) {
            out.entry(space.clone()).or_default().push(e);
        }
    }
    out
}

/// What one space holds for the user, unread: in all, reports on work they
/// asked for (`News::Asked`: a watch's or a job's), and someone needing them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Held {
    pub space: String,
    pub total: usize,
    pub asked: usize,
    pub needs_you: usize,
}

impl Held {
    pub fn of(space: &str, items: &[&Entry]) -> Held {
        let news = |e: &Entry| News::of(&e.kind, e.status.as_deref().unwrap_or(""));
        Held {
            space: space.to_string(),
            total: items.len(),
            asked: items.iter().filter(|e| news(e) == News::Asked).count(),
            needs_you: items.iter().filter(|e| e.status.as_deref() == Some("blocked")).count(),
        }
    }

    /// Worth telling: work the user asked for, or someone needing them.
    pub fn matters(&self) -> bool {
        self.asked > 0 || self.needs_you > 0
    }
}

/// Every space's unread held news, the ones that matter first, then the most.
pub fn held(entries: &[Entry], s: &Spaces) -> Vec<Held> {
    let mut out: Vec<Held> = unread(entries, s).iter().map(|(space, items)| Held::of(space, items)).collect();
    out.sort_by_key(|h| (!h.matters(), std::cmp::Reverse(h.needs_you), std::cmp::Reverse(h.asked), std::cmp::Reverse(h.total)));
    out
}

/// For the brain's radar line: spaces out of focus holding news the user
/// would want (work they asked for, someone needing them), not told yet.
pub fn radar_note(held: &[Held], focus: Option<&str>) -> Option<String> {
    let parts: Vec<String> = held
        .iter()
        .filter(|h| h.matters() && Some(h.space.as_str()) != focus)
        .take(4)
        .map(|h| {
            let mut what = vec![];
            if h.asked > 0 {
                what.push(format!("{} on work the user asked for", h.asked));
            }
            if h.needs_you > 0 {
                what.push(format!("{} needing them", h.needs_you));
            }
            format!("{} {} held ({})", h.space, h.total, what.join(", "))
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    let focus = focus.map(|f| format!(" (focus: {f})")).unwrap_or_default();
    Some(format!(
        "[held]{focus} not told yet: {}. If the request is about one of these spaces, or the user asks what's new, `tower digest --space <space>` reads it.",
        parts.join("; ")
    ))
}

/// How pressing a space's news is: someone needs the user (0), something
/// went wrong (1), work finished (2), anything else (3).
fn urgency(items: &[&Entry]) -> u8 {
    items
        .iter()
        .map(|e| match e.status.as_deref() {
            Some("blocked") => 0,
            Some("error" | "failed" | "exited") => 1,
            Some("done" | "idle") => 2,
            _ => 3,
        })
        .min()
        .unwrap_or(3)
}

/// The order to catch up in: whoever needs the user first, then trouble,
/// then finished work, the most recent first within each. A space passed
/// over waits until it has newer news.
pub fn digest_order(unread: &BTreeMap<String, Vec<&Entry>>, s: &Spaces) -> Vec<String> {
    let mut spaces: Vec<(u8, std::cmp::Reverse<u64>, String)> = unread
        .iter()
        .filter_map(|(space, items)| {
            let latest = items.iter().map(|e| e.at).max()?;
            if s.skipped_ms.get(space).is_some_and(|&k| k >= latest) {
                return None;
            }
            Some((urgency(items), std::cmp::Reverse(latest), space.clone()))
        })
        .collect();
    spaces.sort();
    spaces.into_iter().map(|(_, _, s)| s).collect()
}

/// A space by what someone called it: exactly, by its start, or by a part
/// ("b2c" is "B2C support"), ignoring case.
pub fn resolve(want: &str, known: &[String]) -> Option<String> {
    let w = want.trim().to_lowercase();
    if w.is_empty() {
        return None;
    }
    let by = |f: &dyn Fn(&str) -> bool| known.iter().find(|k| f(&k.to_lowercase())).cloned();
    by(&|k| k == w).or_else(|| by(&|k| k.starts_with(&w))).or_else(|| by(&|k| k.contains(&w)))
}

/// The space of exactly that name (any case). Of two that differ only in
/// case it is the one spelled just so, else neither.
pub fn exactly(want: &str, known: &[String]) -> Option<String> {
    let w = want.trim();
    if w.is_empty() {
        return None;
    }
    if let Some(k) = known.iter().find(|k| k.trim() == w) {
        return Some(k.clone());
    }
    let mut same = known.iter().filter(|k| k.trim().eq_ignore_ascii_case(w));
    let first = same.next()?;
    same.all(|k| k == first).then(|| first.clone())
}

/// A space by what someone called it, for what can't be taken back
/// (closing it): its whole name, else the only one the words are a part
/// of. Several that fit are named (`Err`), never chosen between; none is an
/// empty `Err`.
pub fn resolve_only(want: &str, known: &[String]) -> std::result::Result<String, Vec<String>> {
    if let Some(k) = exactly(want, known) {
        return Ok(k);
    }
    let w = want.trim().to_lowercase();
    if w.is_empty() {
        return Err(vec![]);
    }
    let mut hits: Vec<String> = vec![];
    for k in known.iter().filter(|k| k.to_lowercase().contains(&w)) {
        if !hits.contains(k) {
            hits.push(k.clone());
        }
    }
    match hits.len() {
        1 => Ok(hits.remove(0)),
        _ => Err(hits),
    }
}

/// A space in Herdr's window, for placing a new session in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub id: String,
    pub name: String,
    pub folder: String,
    /// The machine its terminals run on (None: this Mac).
    pub machine: Option<String>,
    /// The one the window shows now.
    pub active: bool,
}

/// Herdr's spaces with their ids, from `herdr-app ls` (empty when Herdr isn't open).
pub fn windows() -> Vec<Window> {
    windows_open().unwrap_or_default()
}

/// `windows`, or None when Herdr's window doesn't answer (not open yet).
pub fn windows_open() -> Option<Vec<Window>> {
    let out = std::process::Command::new(config::herdr_app_bin()).arg("ls").stdin(std::process::Stdio::null()).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v["spaces"].is_array().then(|| windows_from(&v))
}

pub fn windows_from(v: &Value) -> Vec<Window> {
    v["spaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(Window {
                id: s["id"].as_str()?.to_string(),
                name: s["name"].as_str()?.to_string(),
                folder: s["folder"].as_str().unwrap_or("").to_string(),
                machine: s["machine"].as_str().filter(|m| !m.is_empty()).map(String::from),
                active: s["active"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

/// The space `want` names: its whole name, or else a part of it (any case).
/// No match, or several, is an error that lists them.
pub fn pick_window<'a>(want: &str, list: &'a [Window]) -> Result<&'a Window> {
    let w = want.trim().to_lowercase();
    if w.is_empty() {
        bail!("give a space's name (or part of it)");
    }
    let names = |l: &[&Window]| l.iter().map(|s| format!("\"{}\"", s.name)).collect::<Vec<_>>().join(", ");
    let exact: Vec<&Window> = list.iter().filter(|s| s.name.to_lowercase() == w).collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }
    let part: Vec<&Window> = list.iter().filter(|s| s.name.to_lowercase().contains(&w)).collect();
    match part.as_slice() {
        [one] => Ok(one),
        [] if list.is_empty() => bail!("no space matches {want:?}: Herdr's window isn't open, so there are no spaces"),
        [] => bail!("no space matches {want:?} (spaces: {})", names(&list.iter().collect::<Vec<_>>())),
        many => bail!("{want:?} matches several spaces: {}; say which", names(many)),
    }
}

/// The space `tower start` was asked for: its name, and whether the user
/// asked for a new one.
#[derive(Debug, Clone)]
pub struct Asked {
    pub name: String,
    pub new: bool,
}

impl std::ops::Deref for Asked {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}

/// Where `tower start --space` puts a session.
#[derive(Debug, PartialEq)]
pub enum Placing<'a> {
    /// The space that name means.
    One(&'a Window),
    /// Several fit: the closest (the name starts with it, else the shortest
    /// name), and the names of all that fit.
    Closest(&'a Window, Vec<String>),
    /// None fits: a new space, called that.
    New(String),
}

/// Like `pick_window`, but never an error to ask about: several matches
/// take the closest, none makes a new space.
pub fn best_window<'a>(want: &str, list: &'a [Window]) -> Placing<'a> {
    let w = want.trim().to_lowercase();
    if let Ok(one) = pick_window(want, list) {
        return Placing::One(one);
    }
    let part: Vec<&Window> = list.iter().filter(|s| !w.is_empty() && s.name.to_lowercase().contains(&w)).collect();
    if part.is_empty() {
        return Placing::New(want.trim().to_string());
    }
    let best = part.iter().min_by_key(|s| (!s.name.to_lowercase().starts_with(&w), s.name.len())).copied().unwrap_or(part[0]);
    Placing::Closest(best, part.iter().map(|s| format!("\"{}\"", s.name)).collect())
}

/// A space as Herdr's window has it now.
#[derive(Debug, Clone, Default)]
pub struct Live {
    pub name: String,
    /// (address, name, status) for each terminal in it.
    pub sessions: Vec<(String, String, String)>,
}

/// Herdr's spaces, from `herdr-app ls` (empty when Herdr isn't open).
pub fn live() -> Vec<Live> {
    let Ok(out) = std::process::Command::new(config::herdr_app_bin()).arg("ls").stdin(std::process::Stdio::null()).output() else { return vec![] };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v["spaces"]
        .as_array()
        .map(|spaces| {
            spaces
                .iter()
                .map(|s| Live {
                    name: s["name"].as_str().unwrap_or("").to_string(),
                    sessions: s["tabs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|tab| tab["terminals"].as_array().cloned().unwrap_or_default())
                        .map(|t| {
                            (
                                t["terminal"].as_str().unwrap_or("").to_string(),
                                t["name"].as_str().unwrap_or("").to_string(),
                                t["status"].as_str().unwrap_or("").to_string(),
                            )
                        })
                        .collect(),
                })
                .filter(|l| !l.name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// A terminal in a space, as Herdr's window has it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Term {
    pub address: String,
    pub name: String,
    /// `idle`, `working`, ... ("" for a plain program).
    pub status: String,
    /// `claude-code`, `codex`, or "" for a plain program.
    pub app: String,
}

/// A space of Herdr's window with every terminal in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tree {
    pub window: Window,
    pub terminals: Vec<Term>,
}

/// Herdr's spaces with their terminals, from `herdr-app ls` (empty when Herdr isn't open).
pub fn tree() -> Vec<Tree> {
    let Ok(out) = std::process::Command::new(config::herdr_app_bin()).arg("ls").stdin(std::process::Stdio::null()).output() else { return vec![] };
    tree_from(&serde_json::from_slice(&out.stdout).unwrap_or_default())
}

pub fn tree_from(v: &Value) -> Vec<Tree> {
    let windows = windows_from(v);
    v["spaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let window = windows.iter().find(|w| Some(w.id.as_str()) == s["id"].as_str())?.clone();
            let terminals = s["tabs"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|tab| tab["terminals"].as_array().cloned().unwrap_or_default())
                .filter_map(|t| {
                    let raw = t["terminal"].as_str().filter(|a| !a.is_empty())?;
                    // A space on another machine names its terminals as that machine does.
                    let address = match &window.machine {
                        Some(m) if !raw.contains('/') => format!("{m}/{raw}"),
                        _ => raw.to_string(),
                    };
                    let word = |k: &str| t[k].as_str().unwrap_or("").to_string();
                    Some(Term { address, name: word("name"), status: word("status"), app: word("app") })
                })
                .collect();
            Some(Tree { window, terminals })
        })
        .collect()
}

/// Every space the tower knows: Herdr's, and those in the history or with an agenda.
pub fn known(live: &[Live], entries: &[Entry], s: &Spaces) -> Vec<String> {
    let mut names: Vec<String> = live.iter().map(|l| l.name.clone()).collect();
    for n in entries.iter().filter_map(|e| e.space.clone()).chain(s.agendas.keys().cloned()).chain(s.missions.keys().cloned()) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

/// The space `want` means, among every space the tower knows.
pub fn pick(want: &str) -> Result<String> {
    let names = known(&live(), &history::load(), &load());
    match resolve(want, &names) {
        Some(n) => Ok(n),
        None => bail!("no space called {want:?} (spaces: {})", names.join(", ")),
    }
}

// ---- `tower focus`, `tower space agenda`, `tower spaces`, `tower digest` ----

pub fn cmd_focus(space: Option<String>, clear: bool) -> Result<()> {
    let s = load();
    if clear {
        update(|s| (s.focus, s.focus_ms) = (None, None));
        println!("No focus: news from every space is spoken.");
        return Ok(());
    }
    let Some(want) = space else {
        match &s.focus {
            Some(f) => println!("Focused on {f}."),
            None => println!("No focus: news from every space is spoken."),
        }
        return Ok(());
    };
    let name = pick(&want)?;
    let s = update(|s| {
        (s.focus, s.focus_ms) = (Some(name.clone()), Some(history::now_ms()));
        s.clone()
    });
    let held: usize = unread(&history::load(), &s).iter().filter(|(k, _)| **k != name).map(|(_, v)| v.len()).sum();
    println!(
        "Focused on {name}: its news is spoken, other spaces' is held{}.",
        if held > 0 { format!(" ({held} held so far; `tower digest` to catch up)") } else { String::new() }
    );
    Ok(())
}

/// `tower space follow|unfollow <space>`.
pub fn cmd_follow(space: String, on: bool) -> Result<()> {
    let name = pick(&space)?;
    // One writer at a time: the daemon keeps the missions in the same file.
    update(|s| set_follow(s, &name, on));
    // The missions in the state say so at once (the daemon looks again by itself otherwise).
    crate::missions::poke();
    if on {
        println!("Following {name}: when a request there is finished, its receipt is shown and said.");
    } else {
        println!("Not following {name}: its results are kept in the record (unless the tower tells results: tower tell).");
    }
    Ok(())
}

/// `tower space mute|unmute <space>`.
pub fn cmd_mute(space: String, on: bool) -> Result<()> {
    let name = pick(&space)?;
    // One writer at a time: the daemon keeps the missions in the same file.
    update(|s| set_mute(s, &name, on));
    // The daemon's queue and count follow at once (it looks again by itself
    // otherwise), and so do the missions in the state.
    if crate::protocol::is_running() {
        let _ = crate::protocol::send(&crate::protocol::Request::Decisions { space: None, all: false });
    }
    crate::missions::poke();
    if on {
        println!("Muted {name}: what needs you there stays in its own view (`tower decisions --space \"{name}\"`), off the orb's count, and isn't said.");
    } else {
        println!("Unmuted {name}: what needs you there is in the queue and on the orb's count again.");
    }
    Ok(())
}

/// Every space, as `tower spaces` prints it when piped (and the phone API serves it).
pub fn spaces_value() -> Value {
    let (focus, rows) = rows(false);
    json!({ "focus": focus, "spaces": rows })
}

/// Focuses a space (None: no focus). Returns the focused space's full name.
pub fn set_focus(space: Option<&str>) -> Result<Option<String>> {
    let focus = match space.map(str::trim).filter(|w| !w.is_empty()) {
        Some(want) => Some(pick(want)?),
        None => None,
    };
    update(|s| {
        s.focus = focus.clone();
        s.focus_ms = s.focus.is_some().then(history::now_ms);
    });
    Ok(focus)
}

fn rows(all: bool) -> (Option<String>, Vec<Value>) {
    focus_now(config::Config::load().map(|c| c.alerts.focus_hours).unwrap_or(8));
    let s = load();
    let entries = history::load();
    let live = live();
    let unread = unread(&entries, &s);
    let jobs = history::jobs(&entries);
    let names = known(&live, &entries, &s);
    let rows: Vec<Value> = names
        .iter()
        // A space quiet for a day, or closed, is out of the listing unless asked for.
        .filter(|n| all || !s.missions.get(*n).is_some_and(|m| m.archived_ms.is_some()))
        .map(|n| {
            let sessions = live.iter().find(|l| &l.name == n).map(|l| l.sessions.clone()).unwrap_or_default();
            let running = jobs.iter().filter(|j| &j.space == n && j.open()).count();
            let last = entries.iter().rev().find(|e| e.space.as_ref() == Some(n)).map(|e| e.at).unwrap_or(0);
            let m = s.missions.get(n).cloned().unwrap_or_default();
            json!({
                "space": n,
                "focus": s.focus.as_ref() == Some(n),
                "follow": followed_in(&s, n),
                "mute": muted_in(&s, n),
                // The space's goal, under both names.
                "agenda": s.agendas.get(n),
                "goal": s.agendas.get(n),
                "done_when": Some(&m.done_when).filter(|d| !d.is_empty()),
                "standing": m.standing,
                "state": m.state.word(),
                "report": Some(&m.report).filter(|r| !r.is_empty()),
                "quiet_since_ms": m.quiet_since_ms,
                "ready_since_ms": m.ready_since_ms,
                "closed_ms": m.closed_ms,
                "archived": m.archived_ms.is_some(),
                "unread": unread.get(n).map(|v| v.len()).unwrap_or(0),
                // Of those: reports on work the user asked for, and someone needing them.
                "unread_asked": unread.get(n).map(|v| Held::of(n, v).asked).unwrap_or(0),
                "unread_needs_you": unread.get(n).map(|v| Held::of(n, v).needs_you).unwrap_or(0),
                "running_jobs": running,
                "sessions": sessions
                    .iter()
                    .map(|(a, name, st)| json!({ "address": a, "name": name, "status": st, "account": crate::accounts::of_address(a) }))
                    .collect::<Vec<_>>(),
                "last_activity_ms": last,
            })
        })
        .collect();
    (s.focus, rows)
}

/// `tower spaces [--all]`.
pub fn cmd_spaces(all: bool) -> Result<()> {
    let (focus, rows) = rows(all);
    if std::io::stdout().is_terminal() {
        for r in &rows {
            let busy = r["sessions"].as_array().map(|a| a.iter().filter(|t| t["status"] == "working").count()).unwrap_or(0);
            println!(
                "{} {:<20} {:<8} {:>2} unread  {} working  {}{}",
                if r["focus"] == true { "◉" } else { " " },
                r["space"].as_str().unwrap_or(""),
                if r["archived"] == true && r["state"] != "closed" { "archived" } else { r["state"].as_str().unwrap_or("") },
                r["unread"],
                busy,
                r["goal"].as_str().unwrap_or(""),
                if r["standing"] == true { "  (standing)" } else { "" }
            );
        }
        if focus.is_none() {
            println!("(no focus: every space's news is spoken)");
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&json!({ "focus": focus, "spaces": rows }))?);
    }
    Ok(())
}

/// What one space's held news says, for the brain to read out.
fn digest_text(space: &str, items: &[&Entry], s: &Spaces) -> String {
    let mut out = format!("{space}: {} update{}", items.len(), if items.len() == 1 { "" } else { "s" });
    if let Some(a) = s.agendas.get(space) {
        out += &format!(" (agenda: {a})");
    }
    out.push('\n');
    for e in items {
        let who = e.name.clone().or_else(|| e.session.clone()).unwrap_or_default();
        let t = chrono_hm(e.at);
        out += &format!("- {t} {who} ({}): {}\n", e.session.as_deref().unwrap_or(""), e.text);
    }
    out
}

fn chrono_hm(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r fills the struct we own.
    let t = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format!("{:02}:{:02}", t.tm_hour, t.tm_min)
}

/// What the user asked for and isn't finished (the agenda ledger), at the end of a
/// digest: those touching `space`, or all of them.
fn agenda_tail(space: Option<&str>) {
    let ledger = crate::agenda::Ledger::load_from(&crate::agenda::path());
    let lines = crate::agenda::digest_lines(&ledger, space);
    if lines.is_empty() {
        return;
    }
    match space {
        Some(sp) => println!("Unfinished from the user's requests, touching {sp} (`tower agenda show <id>`; tell the user these too):"),
        None => println!("Unfinished from the user's requests (`tower agenda show <id>`; tell the user these too):"),
    }
    for l in lines {
        println!("{l}");
    }
}

/// `tower digest`: one space's held news at a time, the most pressing first;
/// what is delivered is marked read. `--list` only says which spaces have
/// news; `--skip` passes over the next one (or `--space`).
/// A space's brief as the daemon last published it (`crate::briefs`).
fn brief_of(space: &str) -> Option<String> {
    let state: serde_json::Value = std::fs::read_to_string(crate::config::ui_dir().join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok())?;
    state["missions"]["spaces"].as_array()?.iter().find(|m| m["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(space)))?["brief"]["text"].as_str().map(String::from)
}

pub fn cmd_digest(list: bool, skip: bool, space: Option<String>, peek: bool) -> Result<()> {
    let s = load();
    let entries = history::load();
    let unread = unread(&entries, &s);
    let order = digest_order(&unread, &s);
    if list {
        if order.is_empty() {
            println!("Nothing held: no news waiting from any space.");
        }
        for sp in &order {
            println!("{sp}: {} update{}", unread[sp].len(), if unread[sp].len() == 1 { "" } else { "s" });
        }
        agenda_tail(None);
        return Ok(());
    }
    let target = match space {
        Some(w) => {
            let names: Vec<String> = unread.keys().cloned().collect();
            match resolve(&w, &names) {
                Some(n) => n,
                None => {
                    println!("No held news from {w}.");
                    agenda_tail(Some(&w));
                    return Ok(());
                }
            }
        }
        None => match order.first() {
            Some(n) => n.clone(),
            None => {
                println!("Nothing held: no news waiting from any space.");
                agenda_tail(None);
                return Ok(());
            }
        },
    };
    let items = unread.get(&target).cloned().unwrap_or_default();
    if skip {
        let latest = items.iter().map(|e| e.at).max().unwrap_or_else(history::now_ms);
        update(|s| s.skipped_ms.insert(target.clone(), latest));
        println!("Skipped {target} for now (still unread).");
    } else {
        // Where it stands, before its news (`tower brief`).
        if let Some(b) = brief_of(&target) {
            println!("Where {target} stands: {b}");
        }
        print!("{}", digest_text(&target, &items, &s));
        agenda_tail(Some(&target));
        if !peek {
            let latest = items.iter().map(|e| e.at).max().unwrap_or_else(history::now_ms);
            update(|s| s.read_ms.insert(target.clone(), latest));
        }
    }
    let after = digest_order(&unread, &s);
    match after.iter().find(|n| **n != target) {
        Some(next) => println!(
            "Next: {next} ({} update{}). Ask the user before going on.",
            unread[next].len(),
            if unread[next].len() == 1 { "" } else { "s" }
        ),
        None => println!("That was the last space with news."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(at: u64, space: &str, status: Option<&str>, held: bool) -> Entry {
        Entry {
            at,
            kind: "watch".into(),
            space: Some(space.into()),
            status: status.map(String::from),
            text: "x".into(),
            held,
            ..Default::default()
        }
    }

    #[test]
fn picks_a_space_by_part_of_its_name() {
    let v = json!({ "spaces": [
        { "id": "sp1", "name": "M5 Pro", "folder": "/u", "machine": null, "active": false },
        { "id": "sp2", "name": "B2B ticketing", "folder": "/w/a", "machine": null, "active": false },
        { "id": "sp3", "name": "B2C support", "folder": "/w/b", "machine": null, "active": true },
        { "id": "sp4", "name": "Support", "folder": "/w/c", "machine": "vps", "active": false },
    ]});
    let l = windows_from(&v);
    assert_eq!(l.len(), 4);
    assert_eq!(l[3].machine.as_deref(), Some("vps"));
    assert_eq!(pick_window("ticket", &l).unwrap().id, "sp2");
    assert_eq!(pick_window("b2c", &l).unwrap().id, "sp3");
    assert_eq!(pick_window("m5 PRO", &l).unwrap().id, "sp1");
    // A whole name wins over the spaces it is part of.
    assert_eq!(pick_window("support", &l).unwrap().id, "sp4");
    let e = pick_window("b2", &l).unwrap_err().to_string();
    assert!(e.contains("several") && e.contains("\"B2B ticketing\"") && e.contains("\"B2C support\""), "{e}");
    let e = pick_window("nope", &l).unwrap_err().to_string();
    assert!(e.contains("no space matches") && e.contains("\"M5 Pro\""), "{e}");
    assert!(pick_window("x", &[]).unwrap_err().to_string().contains("isn't open"));
    assert!(pick_window(" ", &l).is_err());
}

#[test]
fn a_spaces_terminals_come_with_it() {
    let v = json!({ "spaces": [
        { "id": "sp1", "name": "Shop", "folder": "/w/shop", "machine": null, "active": true, "tabs": [
            { "id": "tab1", "name": "web", "terminals": [{ "terminal": "t3", "pane": "p3", "status": "working", "app": "claude-code", "name": "shop-web", "sub_agents": [] }] },
            { "id": "tab2", "name": "sh", "terminals": [{ "terminal": "t4", "pane": "p4", "status": null, "app": null, "name": null }, { "terminal": "", "pane": "p5" }] },
        ] },
        { "id": "sp2", "name": "Far", "folder": "/srv", "machine": "vps", "active": false, "tabs": [
            { "id": "tab3", "name": "api", "terminals": [{ "terminal": "t2", "status": "idle", "app": "codex", "name": "api" }, { "terminal": "vps/t9", "status": "idle", "app": "claude-code", "name": "x" }] },
        ] },
        { "id": "sp3", "name": "Empty", "tabs": [] },
    ]});
    let t = tree_from(&v);
    assert_eq!(t.iter().map(|x| (x.window.name.as_str(), x.terminals.len())).collect::<Vec<_>>(), [("Shop", 2), ("Far", 2), ("Empty", 0)]);
    assert_eq!(t[0].terminals[0], Term { address: "t3".into(), name: "shop-web".into(), status: "working".into(), app: "claude-code".into() });
    assert_eq!(t[0].terminals[1], Term { address: "t4".into(), ..Default::default() }, "a plain program: no status, no app");
    // A space on another machine names its terminals as that machine does.
    assert_eq!(t[1].terminals.iter().map(|x| x.address.as_str()).collect::<Vec<_>>(), ["vps/t2", "vps/t9"]);
    assert!(tree_from(&json!({})).is_empty());
}

/// "Open … in a new space called sandbox": `tower start --space` makes it,
/// and several matches take the closest, so opening never stops to ask.
#[test]
fn starting_in_a_space_never_stops_to_ask() {
    let v = json!({ "spaces": [
        { "id": "sp1", "name": "M5 Pro", "folder": "/u", "active": false },
        { "id": "sp2", "name": "B2B ticketing", "folder": "/w/a", "active": false },
        { "id": "sp3", "name": "B2C support", "folder": "/w/b", "active": true },
        { "id": "sp5", "name": "b2c-tickets", "folder": "/w/d", "active": false },
    ]});
    let l = windows_from(&v);
    assert!(matches!(best_window("ticket", &l), Placing::Closest(w, _) if w.id == "sp5"));
    assert!(matches!(best_window("B2B ticketing", &l), Placing::One(w) if w.id == "sp2"));
    match best_window("b2", &l) {
        Placing::Closest(w, all) => {
            assert_eq!(w.id, "sp3", "the shortest that starts with it, the first of equals");
            assert_eq!(all.len(), 3);
        }
        p => panic!("{p:?}"),
    }
    assert_eq!(best_window(" sandbox ", &l), Placing::New("sandbox".into()));
    assert_eq!(best_window("sandbox", &[]), Placing::New("sandbox".into()));
}

#[test]
    fn focus_holds_other_spaces() {
        use News::*;
        // No focus: everything is spoken.
        assert_eq!(gate(Some("B2C support"), None, Ambient, true), Gate::Speak);
        assert_eq!(gate(Some("B2C support"), None, NeedsYou, true), Gate::Speak);
        // The focused space is spoken; a session in no space too.
        assert_eq!(gate(Some("B2B ticketing"), Some("B2B ticketing"), Ambient, true), Gate::Speak);
        assert_eq!(gate(None, Some("B2B ticketing"), Ambient, true), Gate::Speak);
        // Another space: held, unless someone there needs the user.
        assert_eq!(gate(Some("B2C support"), Some("B2B ticketing"), Ambient, true), Gate::Hold);
        assert_eq!(gate(Some("B2C support"), Some("B2B ticketing"), NeedsYou, true), Gate::Urgent);
        assert_eq!(gate(Some("B2C support"), Some("B2B ticketing"), NeedsYou, false), Gate::Hold);
    }

    /// The missed release (t58 in b2c-tickets, focus on B2C support): a
    /// watch's report on work the user asked for came in done and was held,
    /// so neither the brain nor the user heard the PR waited for them.
    #[test]
    fn work_the_user_asked_for_comes_through_the_focus() {
        // A watch's or an open job's report, done or blocked: through.
        assert_eq!(News::of("watch", "done"), News::Asked);
        assert_eq!(News::of("job", "idle"), News::Asked);
        assert_eq!(News::of("watch", "blocked"), News::Asked);
        assert_eq!(gate(Some("b2c-tickets"), Some("B2C support"), News::of("watch", "done"), true), Gate::Through);
        // ... even with the urgent cue off: it's an answer to the user, not an alert.
        assert_eq!(gate(Some("b2c-tickets"), Some("B2C support"), News::of("watch", "done"), false), Gate::Through);
        // A finish nobody watched, the queue's notes: still held.
        assert_eq!(gate(Some("b2c-tickets"), Some("B2C support"), News::of("alert", "done"), true), Gate::Hold);
        assert_eq!(gate(Some("b2c-tickets"), Some("B2C support"), News::of("queue", "news"), true), Gate::Hold);
        // The agenda ledger's notes are work the user asked for: through the focus, marked.
        assert_eq!(gate(Some("b2c-tickets"), Some("B2C support"), News::of("agenda", "news"), true), Gate::Through);
        // Blocked there: the urgent cue.
        assert_eq!(gate(Some("b2c-tickets"), Some("B2C support"), News::of("alert", "blocked"), true), Gate::Urgent);
        // The brain is told where it's from and to name it.
        let note = out_of_focus("b2c-tickets", "B2C support");
        assert!(note.contains("b2c-tickets") && note.contains("B2C support") && note.contains("\"In b2c-tickets\""), "{note}");
    }

    #[test]
    fn held_news_is_counted_by_what_it_is_and_named_to_the_brain() {
        let mut held_watch = e(10, "b2c-tickets", Some("done"), true);
        held_watch.kind = "watch".into();
        let mut queue = e(11, "b2c-tickets", Some("news"), true);
        queue.kind = "queue".into();
        let mut blocked = e(12, "eyz-mediaroom-serverless", Some("blocked"), true);
        blocked.kind = "alert".into();
        let mut chatter = e(13, "M5 Pro", Some("done"), true);
        chatter.kind = "alert".into();
        let entries = vec![held_watch, queue, blocked, chatter];
        let s = Spaces { focus: Some("B2C support".into()), ..Default::default() };
        let h = held(&entries, &s);
        // What matters first: someone needing the user, then asked-for work.
        assert_eq!(h[0], Held { space: "eyz-mediaroom-serverless".into(), total: 1, asked: 0, needs_you: 1 });
        assert_eq!(h[1], Held { space: "b2c-tickets".into(), total: 2, asked: 1, needs_you: 0 });
        assert_eq!(h[2], Held { space: "M5 Pro".into(), total: 1, asked: 0, needs_you: 0 });
        let note = radar_note(&h, s.focus.as_deref()).unwrap();
        assert!(note.starts_with("[held] (focus: B2C support) not told yet: "), "{note}");
        assert!(note.contains("eyz-mediaroom-serverless 1 held (1 needing them)"), "{note}");
        assert!(note.contains("b2c-tickets 2 held (1 on work the user asked for)"), "{note}");
        assert!(!note.contains("M5 Pro"), "chatter alone isn't named: {note}");
        // Only chatter held: nothing to say.
        assert_eq!(radar_note(&h[2..], s.focus.as_deref()), None);
    }

    #[test]
    fn a_focus_ends_after_its_hours() {
        let h = 3_600_000;
        let mut s = Spaces { focus: Some("B2C support".into()), focus_ms: Some(10 * h), ..Default::default() };
        assert_eq!(active_focus(&s, 10 * h, 8), Some("B2C support"));
        assert_eq!(active_focus(&s, 17 * h + 59 * 60_000, 8), Some("B2C support"));
        assert_eq!(active_focus(&s, 18 * h, 8), None, "8 hours on: over");
        assert_eq!(active_focus(&s, 1000 * h, 0), Some("B2C support"), "0: never ends");
        // Set by an older tower, without a time: it stands (dated when the daemon sees it).
        s.focus_ms = None;
        assert_eq!(active_focus(&s, 1000 * h, 8), Some("B2C support"));
        // An older spaces.json loads, and a focus without a time isn't written with one.
        let old: Spaces = serde_json::from_str(r#"{"focus":"B2C support","agendas":{},"read_ms":{},"skipped_ms":{}}"#).unwrap();
        assert_eq!((old.focus.as_deref(), old.focus_ms), (Some("B2C support"), None));
        assert!(!serde_json::to_string(&Spaces::default()).unwrap().contains("focus_ms"));
    }

    #[test]
    fn needing_you_elsewhere_says_what_is_needed() {
        assert_eq!(needs_you_line("b2c-tickets", "mediaroom waits for you to merge PR #1250."), "In b2c-tickets: mediaroom waits for you to merge PR #1250.");
        assert_eq!(needs_you_line("b2c-tickets", ""), "b2c-tickets needs you.");
        assert_eq!(needs_you_line("b2c-tickets", &"x".repeat(200)), "b2c-tickets needs you.");
    }

    #[test]
    fn unread_is_held_news_after_the_read_mark() {
        let entries = vec![e(1, "A", None, true), e(2, "A", None, false), e(3, "A", None, true), e(4, "B", None, true)];
        let mut s = Spaces::default();
        s.read_ms.insert("A".into(), 1);
        let u = unread(&entries, &s);
        assert_eq!(u["A"].iter().map(|e| e.at).collect::<Vec<_>>(), vec![3]);
        assert_eq!(u["B"].len(), 1);
    }

    #[test]
    fn a_followed_space_is_kept_with_the_spaces() {
        let dir = std::env::temp_dir().join(format!("tower-spaces-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ui = dir.join("ui");
        let mut s = load_from(&ui);
        assert!(!followed_in(&s, "B2C support"), "nothing is followed until the user says so");
        assert!(set_follow(&mut s, "B2C support", true));
        assert!(!set_follow(&mut s, "b2c SUPPORT", true), "already");
        save_to(&ui, &s);
        let back = load_from(&ui);
        assert!(followed_in(&back, "b2c support") && !followed_in(&back, "B2B ticketing") && !followed_in(&back, ""));
        // A receipt from there reaches the user at `decisions`; one from elsewhere is kept.
        use crate::tell::{Level, receipt_told};
        assert!(receipt_told(Level::Decisions, true, followed_in(&back, "B2C support")));
        assert!(!receipt_told(Level::Decisions, true, followed_in(&back, "B2B ticketing")));
        assert!(receipt_told(Level::Results, true, followed_in(&back, "B2B ticketing")), "every space's, at results");
        let mut s = back;
        assert!(set_follow(&mut s, "b2c support", false));
        assert!(!followed_in(&s, "B2C support"));
        // A spaces file from before this has none, and one without any doesn't mention it.
        let old: Spaces = serde_json::from_str(r#"{"focus":"A","agendas":{"A":"x"}}"#).unwrap();
        assert!(old.follow.is_empty());
        assert!(!serde_json::to_string(&s).unwrap().contains("follow"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_muted_space_is_kept_with_the_spaces() {
        let dir = std::env::temp_dir().join(format!("tower-spaces-mute-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ui = dir.join("ui");
        let mut s = load_from(&ui);
        assert!(!muted_in(&s, "Docs"), "nothing is muted until the user says so");
        assert!(set_mute(&mut s, "Docs", true) && !set_mute(&mut s, "docs ", true), "once");
        // Muting and following are each their own.
        assert!(set_follow(&mut s, "Docs", true));
        save_to(&ui, &s);
        let back = load_from(&ui);
        assert!(muted_in(&back, "DOCS") && followed_in(&back, "Docs") && !muted_in(&back, "Shop") && !muted_in(&back, ""));
        let mut s = back;
        assert!(set_mute(&mut s, "docs", false) && !muted_in(&s, "Docs") && followed_in(&s, "Docs"));
        // A spaces file from before this has none, and one without any doesn't mention it.
        let old: Spaces = serde_json::from_str(r#"{"focus":"A","follow":["A"]}"#).unwrap();
        assert!(old.mute.is_empty() && !serde_json::to_string(&s).unwrap().contains("mute"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_goes_by_urgency_then_recency_and_skips_wait() {
        let entries = vec![
            e(10, "Done space", Some("done"), true),
            e(20, "Newer done", Some("done"), true),
            e(5, "Needs you", Some("blocked"), true),
            e(30, "Chatter", None, true),
            e(15, "Broken", Some("error"), true),
        ];
        let mut s = Spaces::default();
        let order = digest_order(&unread(&entries, &s), &s);
        assert_eq!(order, vec!["Needs you", "Broken", "Newer done", "Done space", "Chatter"]);
        // Skipped: waits until it has news newer than the skip.
        s.skipped_ms.insert("Needs you".into(), 5);
        assert_eq!(digest_order(&unread(&entries, &s), &s)[0], "Broken");
        let mut more = entries.clone();
        more.push(e(40, "Needs you", None, true));
        assert_eq!(digest_order(&unread(&more, &s), &s)[0], "Needs you");
    }

    #[test]
    fn names_resolve_loosely() {
        let known = vec!["B2B ticketing".to_string(), "B2C support".to_string(), "M5 Pro".to_string()];
        assert_eq!(resolve("b2c", &known).as_deref(), Some("B2C support"));
        assert_eq!(resolve("B2B Ticketing", &known).as_deref(), Some("B2B ticketing"));
        assert_eq!(resolve("support", &known).as_deref(), Some("B2C support"));
        assert_eq!(resolve("nothing", &known), None);
    }

    #[test]
    fn a_file_that_doesnt_read_is_kept_aside_before_it_is_written_over() {
        let dir = std::env::temp_dir().join(format!("tower-spaces-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ui = dir.join("ui");
        std::fs::create_dir_all(&ui).unwrap();
        // A state word this tower doesn't know (a hand edit, a later tower): the whole file doesn't read.
        let old = r#"{"agendas": {"Shop": "Ship the checkout"}, "missions": {"Shop": {"standing": true, "state": "paused", "born_ms": 1}}}"#;
        std::fs::write(path_in(&ui), old).unwrap();
        assert!(load_from(&ui).missions.is_empty());
        update_in(&ui, |s| s.focus = Some("Docs".into()));
        assert_eq!(load_from(&ui).focus.as_deref(), Some("Docs"));
        let aside: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("spaces.bad-")).collect();
        assert_eq!(aside.len(), 1, "what it held isn't lost");
        assert_eq!(std::fs::read_to_string(aside[0].path()).unwrap(), old);
        // A file that reads is written as ever, with nothing put aside.
        update_in(&ui, |s| s.focus = None);
        assert_eq!(std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("spaces.bad-")).count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_cant_be_taken_back_never_guesses_between_spaces() {
        let known: Vec<String> = ["Work A", "Work B", "shop-2", "Workshop", "M5 Pro", "M5 Pro"].map(String::from).to_vec();
        // A whole name, in any case; a part that fits one space only.
        assert_eq!(resolve_only("work a", &known), Ok("Work A".to_string()));
        assert_eq!(resolve_only("m5", &known), Ok("M5 Pro".to_string()), "the same name twice is one space");
        assert_eq!(resolve_only("shop-", &known), Ok("shop-2".to_string()));
        // A part that fits several: they are named, and none is taken (`resolve` takes the first).
        assert_eq!(resolve("work", &known).as_deref(), Some("Work A"));
        assert_eq!(resolve_only("work", &known), Err(vec!["Work A".to_string(), "Work B".to_string(), "Workshop".to_string()]));
        assert_eq!(resolve_only("shop", &known), Err(vec!["shop-2".to_string(), "Workshop".to_string()]));
        assert_eq!(resolve_only("nothing", &known), Err(vec![]));
        assert_eq!(resolve_only(" ", &known), Err(vec![]));
        // Two that differ only in case: the one spelled just so, else neither.
        let twins: Vec<String> = ["Shop", "shop"].map(String::from).to_vec();
        assert_eq!((exactly("shop", &twins), exactly("Shop", &twins), exactly("SHOP", &twins)), (Some("shop".to_string()), Some("Shop".to_string()), None));
        assert_eq!(resolve_only("SHOP", &twins), Err(twins.clone()));
        assert_eq!(exactly("sho", &twins), None);
    }

    #[test]
    fn space_comes_from_herdrs_where_label() {
        assert_eq!(space_of(&json!({ "labels": { "where": "B2C support / bot" } })).as_deref(), Some("B2C support"));
        assert_eq!(space_of(&json!({ "labels": {} })), None);
    }
}
