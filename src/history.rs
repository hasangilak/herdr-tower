//! The tower's history: everything said to and by it, every alert, and the
//! jobs it hands out, kept in `~/.local/share/herder-tower/history.jsonl`
//! (one JSON object a line, only ever appended to). The daemon, the brain's
//! hooks and `tower job` / `tower summary` / `tower log` write it; the orb's
//! panel shows it.
//!
//! A job is a piece of work the brain handed to a session: `tower watch`
//! starts one, the watch firing (or `tower job done`) ends it. Its events
//! share a `job` id; the latest one says where it stands.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::hc;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Entry {
    /// Unix milliseconds.
    pub at: u64,
    /// `request` (the user, spoken or typed), `answer` (the brain),
    /// `watch` (a watched session reporting back), `alert`, `job`,
    /// `summary` (of a session) or `note`.
    pub kind: String,
    /// The session's address (`t3`, `vps/t2`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Its name, as Herdr shows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The Herdr space it is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// The Claude account the session runs under (a name from the
    /// claude-accounts mod, `cca`). Absent for Codex, other machines, or
    /// when it can't be told.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// For jobs: `working`, `done`, `blocked`, `error`, `exited`, `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub text: String,
    /// Files the user attached.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// News from a space that wasn't in focus, kept instead of spoken (see `spaces`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
    /// `screenshot`: its id (the stored file's name), for `GET /v1/screenshots/<id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `screenshot`: what it shows, in the session's words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// `screenshot`: the tower's own copy (absolute, under attachments/screenshots).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `answer`: the screenshots the brain attached to it (ids), from its
    /// `[shot:<id>]` / `[shots:<session>]` markers, which `text` no longer has.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shots: Vec<String>,
    /// `request`: where it came from: `mac` (typed in the panel or `tower
    /// ask`), `voice` (the Mac's microphone) or `phone:<device id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `behavior`: what changed, line by line (`+ `, `- `, `  `).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

pub fn path() -> PathBuf {
    config::data_dir().join("history.jsonl")
}

/// Where attached files are kept, so they outlive the place they came from.
pub fn attachments_dir() -> PathBuf {
    config::data_dir().join("attachments")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Adds an entry. Never fails the caller: history is a record, not a duty.
pub fn append(mut e: Entry) {
    if e.at == 0 {
        e.at = now_ms();
    }
    let Ok(mut line) = serde_json::to_string(&e) else { return };
    line.push('\n');
    let _ = std::fs::create_dir_all(config::data_dir());
    // One write per line, in append mode: writers in several processes don't interleave.
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// An entry about a session, with its name and space filled in from Herdr.
pub fn about(kind: &str, session: &str, text: &str) -> Entry {
    let mut e = Entry { kind: kind.into(), session: Some(session.into()), text: text.into(), ..Default::default() };
    if let Some(t) = hc::get(session) {
        e.name = t["name"].as_str().filter(|n| !n.is_empty()).map(String::from).or_else(|| Some(hc::spoken_name(&t)));
        // `where` reads "<space> / <tab>".
        e.space = t["labels"]["where"].as_str().and_then(|w| w.split(" / ").next()).map(String::from);
        if let Some(a) = t["address"].as_str() {
            e.session = Some(a.into());
        }
        e.account = crate::accounts::of_terminal(&t);
    }
    e
}

/// Runs `f` on its own thread: the daemon's threads never wait for history
/// (filling in a session's name asks herdr-core).
pub fn later(f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name("history".into()).spawn(f);
}

pub fn record(kind: &str, text: &str) {
    append(Entry { kind: kind.into(), text: text.into(), ..Default::default() });
}

/// What the brain said (its Stop hook): the text without its screenshot
/// markers, and the screenshots they name.
pub fn record_said(kind: &str, text: &str) {
    let (clean, refs) = crate::text::shot_refs(text);
    if refs.is_empty() {
        return record(kind, text);
    }
    let shots = resolve_shots(&refs, &load());
    if clean.trim().is_empty() && shots.is_empty() {
        return;
    }
    append(Entry { kind: kind.into(), text: clean, shots, ..Default::default() });
}

/// How many screenshots one message carries.
pub const SHOTS_PER_MESSAGE: usize = 4;
/// Screenshots a session sent this close together came in one go.
const ONE_GO_MS: u64 = 2 * 60 * 1000;

/// The screenshot ids the markers name, in order, each once, at most
/// `SHOTS_PER_MESSAGE`: `[shot:<id>]` if there is one; `[shots:<session>]`
/// the latest ones the session (address or name) sent in one go.
pub fn resolve_shots(refs: &[crate::text::ShotRef], entries: &[Entry]) -> Vec<String> {
    use crate::text::ShotRef;
    let shots: Vec<&Entry> = entries.iter().filter(|e| e.kind == "screenshot" && e.id.is_some()).collect();
    let mut out: Vec<String> = vec![];
    for r in refs {
        let ids: Vec<String> = match r {
            ShotRef::Id(id) => shots.iter().filter(|e| e.id.as_deref() == Some(id)).map(|_| id.clone()).take(1).collect(),
            ShotRef::Session(who) => {
                let of = |e: &&&Entry| {
                    e.session.as_deref() == Some(who.as_str()) || e.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(who))
                };
                let latest = shots.iter().filter(of).map(|e| e.at).max().unwrap_or(0);
                shots.iter().filter(of).filter(|e| e.at + ONE_GO_MS >= latest).filter_map(|e| e.id.clone()).collect()
            }
        };
        for id in ids {
            if !out.contains(&id) && out.len() < SHOTS_PER_MESSAGE {
                out.push(id);
            }
        }
    }
    out
}

pub fn record_session(kind: &str, session: &str, text: &str, status: Option<&str>) {
    let mut e = about(kind, session, text);
    e.status = status.map(String::from);
    append(e);
}

/// What the user asked. A message typed in the orb's panel may end with the
/// files attached to it (see `compose`).
/// An entry about a space (and, when given, one of its sessions).
pub fn record_space(kind: &str, space: &str, session: Option<&str>, text: &str) {
    let mut e = match session {
        Some(t) => about(kind, t, text),
        None => Entry { kind: kind.into(), text: text.into(), ..Default::default() },
    };
    e.space = Some(space.into());
    append(e);
}

pub fn record_request(message: &str, source: &str) {
    let (text, attachments) = split(message);
    let source = (!source.is_empty()).then(|| source.to_string());
    append(Entry { kind: "request".into(), text, attachments, source, ..Default::default() });
}

/// A job starts: the brain handed `session` this work. Returns its id.
pub fn job_start(session: &str, title: &str) -> String {
    job_start_on(session, title, None)
}

/// `job_start` for a session started on a known Claude account (`tower start`):
/// recorded as given, since claude may not be running yet to ask.
pub fn job_start_on(session: &str, title: &str, account: Option<String>) -> String {
    let mut e = about("job", session, title);
    if account.is_some() {
        e.account = account;
    }
    let id = format!("{}-{}", e.session.as_deref().unwrap_or(session).replace('/', "_"), now_ms());
    e.job = Some(id.clone());
    e.status = Some("working".into());
    append(e);
    id
}

/// Where a session's latest job stands now (`done`, `blocked`, ...), with
/// what happened. Starts a job if the session has none.
pub fn job_update(session: &str, status: &str, text: &str) {
    let jobs = jobs(&load());
    let mut e = about("job", session, text);
    let addr = e.session.clone().unwrap_or_else(|| session.into());
    let job = jobs.iter().filter(|j| j.session == addr).max_by_key(|j| (j.open(), j.started));
    e.job = Some(match job {
        Some(j) => j.id.clone(),
        None => format!("{}-{}", addr.replace('/', "_"), now_ms()),
    });
    e.status = Some(status.into());
    append(e);
}

/// A watched session stopped (`status`): ends its open job, or, when the
/// watch began before any job was recorded, records the job (titled with
/// what was asked) and its end.
pub fn job_watched(session: &str, status: &str, asked: &str) {
    let addr = hc::get(session).and_then(|t| t["address"].as_str().map(String::from)).unwrap_or_else(|| session.into());
    if !jobs(&load()).iter().any(|j| j.session == addr && j.open()) {
        job_start(session, asked);
    }
    job_update(session, status, "");
}

/// Every entry, oldest first. Lines that don't parse are skipped.
pub fn load() -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path()) else { return vec![] };
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub session: String,
    pub name: String,
    pub space: String,
    /// The Claude account it ran on, when known.
    pub account: Option<String>,
    /// What was asked.
    pub title: String,
    pub status: String,
    /// The latest word on it (a watch report or `tower job done`).
    pub summary: String,
    pub started: u64,
    pub updated: u64,
}

impl Job {
    pub fn open(&self) -> bool {
        matches!(self.status.as_str(), "working" | "blocked")
    }
}

/// The jobs in `entries`, newest first.
pub fn jobs(entries: &[Entry]) -> Vec<Job> {
    let mut out: Vec<Job> = vec![];
    for e in entries.iter().filter(|e| e.kind == "job") {
        let Some(id) = &e.job else { continue };
        match out.iter_mut().find(|j| &j.id == id) {
            Some(j) => {
                if let Some(s) = &e.status {
                    j.status = s.clone();
                }
                if !e.text.is_empty() {
                    j.summary = e.text.clone();
                }
                if j.account.is_none() {
                    j.account = e.account.clone();
                }
                j.updated = e.at;
            }
            None => out.push(Job {
                id: id.clone(),
                session: e.session.clone().unwrap_or_default(),
                name: e.name.clone().unwrap_or_default(),
                space: e.space.clone().unwrap_or_default(),
                account: e.account.clone(),
                title: e.text.clone(),
                status: e.status.clone().unwrap_or_else(|| "working".into()),
                summary: String::new(),
                started: e.at,
                updated: e.at,
            }),
        }
    }
    out.sort_by_key(|j| std::cmp::Reverse(j.updated));
    out
}

impl Job {
    /// As `tower history --jobs` prints it when piped (and the phone API serves it).
    pub fn row(&self) -> serde_json::Value {
        serde_json::json!({
            "job": self.id, "session": self.session, "name": self.name, "space": self.space, "account": self.account,
            "status": self.status, "asked": self.title, "summary": self.summary, "started_ms": self.started, "updated_ms": self.updated,
        })
    }
}

const ATTACHED: &str = " [attached: ";

/// One line for the brain: the message, then the files attached to it.
/// (One line, because `hc send` types it into the brain's prompt.)
pub fn compose(text: &str, attachments: &[String]) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if attachments.is_empty() { text } else { format!("{text}{ATTACHED}{}]", attachments.join(" | ")) }
}

/// `[Image #1] … [Image #n]` for the images the text doesn't already mark,
/// in front, as the panel puts a token where you attach (the Nth token is the Nth file).
pub fn with_image_markers(text: &str, n: usize) -> String {
    let missing: Vec<String> = (1..=n).map(|k| format!("[Image #{k}]")).filter(|m| !text.contains(m.as_str())).collect();
    let text = text.trim();
    match (missing.is_empty(), text.is_empty()) {
        (true, _) => text.to_string(),
        (false, true) => missing.join(" "),
        (false, false) => format!("{} {text}", missing.join(" ")),
    }
}

/// `compose` undone.
pub fn split(message: &str) -> (String, Vec<String>) {
    match message.rfind(ATTACHED) {
        Some(i) if message.ends_with(']') => {
            let files = &message[i + ATTACHED.len()..message.len() - 1];
            (message[..i].to_string(), files.split(" | ").map(String::from).filter(|f| !f.is_empty()).collect())
        }
        _ => (message.to_string(), vec![]),
    }
}

/// Copies a file the user attached into the tower's own folder (an APFS
/// clone, so even a big video costs nothing) and returns the copy's path;
/// the original's when copying fails.
pub fn keep_attachment(src: &std::path::Path) -> PathBuf {
    if src.starts_with(attachments_dir()) {
        return src.to_path_buf();
    }
    let dir = attachments_dir().join(now_ms().to_string());
    let name = src.file_name().map(|n| n.to_owned()).unwrap_or_else(|| "file".into());
    let dst = dir.join(name);
    if std::fs::create_dir_all(&dir).is_ok() && std::fs::copy(src, &dst).is_ok() { dst } else { src.to_path_buf() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_markers_like_the_panel() {
        assert_eq!(with_image_markers("what is wrong here?", 2), "[Image #1] [Image #2] what is wrong here?");
        assert_eq!(with_image_markers("compare [Image #1] with [Image #2]", 2), "compare [Image #1] with [Image #2]", "marked already");
        assert_eq!(with_image_markers("see [Image #2]", 2), "[Image #1] see [Image #2]");
        assert_eq!(with_image_markers("  ", 1), "[Image #1]");
        assert_eq!(with_image_markers("no images", 0), "no images");
        let m = compose(&with_image_markers("fix this", 1), &["/a/x.png".into()]);
        assert_eq!(m, "[Image #1] fix this [attached: /a/x.png]");
    }

    #[test]
    fn markers_become_the_answers_screenshots() {
        use crate::text::ShotRef;
        let shot = |at, session: &str, name: &str, id: &str| Entry {
            at,
            kind: "screenshot".into(),
            session: Some(session.into()),
            name: Some(name.into()),
            id: Some(id.into()),
            ..Default::default()
        };
        let entries = vec![
            shot(1_000, "t26", "orb-rust", "old.png"),
            shot(900_000, "t26", "orb-rust", "a.png"),
            shot(901_000, "t26", "orb-rust", "b.png"),
            shot(902_000, "t3", "eyzflow", "c.png"),
        ];
        // A session's latest go (by address or name), not the old one.
        assert_eq!(resolve_shots(&[ShotRef::Session("t26".into())], &entries), vec!["a.png", "b.png"]);
        assert_eq!(resolve_shots(&[ShotRef::Session("Orb-Rust".into())], &entries), vec!["a.png", "b.png"]);
        // By id; unknown ids and repeats drop; never more than four.
        let refs = [ShotRef::Id("c.png".into()), ShotRef::Id("nope.png".into()), ShotRef::Session("t26".into()), ShotRef::Id("c.png".into()), ShotRef::Id("old.png".into())];
        assert_eq!(resolve_shots(&refs, &entries), vec!["c.png", "a.png", "b.png", "old.png"]);
        let many: Vec<Entry> = (0..6).map(|i| shot(1_000 + i, "t9", "x", &format!("{i}.png"))).collect();
        assert_eq!(resolve_shots(&[ShotRef::Session("t9".into())], &many).len(), SHOTS_PER_MESSAGE);
    }

    #[test]
    fn compose_and_split() {
        let files = vec!["/tmp/a b.png".to_string(), "/tmp/c.mov".to_string()];
        let m = compose("look at\nthis https://x.dev/a.", &files);
        assert_eq!(m, "look at this https://x.dev/a. [attached: /tmp/a b.png | /tmp/c.mov]");
        assert_eq!(split(&m), ("look at this https://x.dev/a.".to_string(), files));
        assert_eq!(split("no files"), ("no files".to_string(), vec![]));
    }

    #[test]
    fn account_is_kept_only_when_known() {
        let e = Entry { at: 1, kind: "job".into(), account: Some("work".into()), ..Default::default() };
        let line = serde_json::to_string(&e).unwrap();
        assert!(line.contains(r#""account":"work""#), "{line}");
        assert_eq!(serde_json::from_str::<Entry>(&line).unwrap(), e);
        let none = serde_json::to_string(&Entry { kind: "note".into(), ..Default::default() }).unwrap();
        assert!(!none.contains("account"), "{none}");
        // Entries written before accounts existed still load.
        let old: Entry = serde_json::from_str(r#"{"at":1,"kind":"job","session":"t3","text":"x"}"#).unwrap();
        assert_eq!(old.account, None);
    }

    #[test]
    fn jobs_fold() {
        let e = |at, status: &str, text: &str| Entry {
            at,
            kind: "job".into(),
            session: Some("t3".into()),
            job: Some("t3-1".into()),
            status: Some(status.into()),
            text: text.into(),
            ..Default::default()
        };
        let mut first = e(1, "working", "run the tests");
        first.account = Some("work".into());
        let j = jobs(&[first, e(5, "done", "all 40 pass")]);
        assert_eq!(j.len(), 1);
        assert_eq!(j[0].account.as_deref(), Some("work"), "the job keeps the account it started on");
        assert_eq!((j[0].title.as_str(), j[0].status.as_str(), j[0].summary.as_str()), ("run the tests", "done", "all 40 pass"));
        assert!(!j[0].open());
    }
}
