//! The new spaces in draft (the full window's New space): several at once.
//! Each is what was said to make it (the ticket, the conversation), the
//! brain's draft of it, the project picked and the review switch. One is
//! open in the launcher; the others wait, listed above it, until each is
//! launched or deleted.
//!
//! - They are kept in `<data>/new-spaces.json` (`path`), written whenever
//!   one changes, so a draft outlasts the window, the orb and the tower.
//! - A draft with nothing said in it yet (`SpaceDraft::is_empty`) is not
//!   kept: starting another and leaving it blank leaves no row behind.
//! - The book remembers which one was open, and opens on it again.
//! - A file that is there and doesn't read is kept aside
//!   (`new-spaces.bad-<ms>.json`) before it is written over.
//!
//! Nothing here draws.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;
use crate::launch::Told;

/// One new space in draft.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpaceDraft {
    pub id: String,
    /// The ticket, as the conversation has said it.
    pub ticket: String,
    /// The conversation that makes it, oldest first.
    pub story: Vec<Told>,
    /// The brain's draft of it, if there is one.
    pub drafted: Option<Drafting>,
    /// The project picked (none: the brain picks).
    pub picked: Option<String>,
    /// "Review its PRs".
    pub review: bool,
    /// The space the tower said it makes (a first line that names its
    /// repos is launched as it says, with no draft of the brain's).
    pub named: Option<String>,
    pub created_ms: u64,
    /// When something in it last changed.
    pub updated_ms: u64,
}

/// The brain's draft, as `tower launch "<ticket>" --draft --json` printed
/// it, and the ticket it is a draft of (the ticket may have been added to since).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Drafting {
    pub of: String,
    pub raw: Value,
}

impl Default for SpaceDraft {
    fn default() -> Self {
        Self {
            id: String::new(),
            ticket: String::new(),
            story: vec![],
            drafted: None,
            picked: None,
            review: true,
            named: None,
            created_ms: 0,
            updated_ms: 0,
        }
    }
}

/// An id no other draft of this run has (`d<ms>-<n>`).
pub fn new_id(now: u64) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!("d{now}-{}", N.fetch_add(1, Ordering::Relaxed))
}

impl SpaceDraft {
    /// A new draft with nothing in it, started at `now`.
    pub fn fresh(now: u64) -> Self {
        Self { id: new_id(now), created_ms: now, updated_ms: now, ..Default::default() }
    }

    /// Nothing has been said in it yet.
    pub fn is_empty(&self) -> bool {
        self.ticket.trim().is_empty() && self.story.is_empty() && self.drafted.is_none()
    }

    /// The same draft as `other`, whenever each was last changed.
    pub fn same(&self, other: &SpaceDraft) -> bool {
        SpaceDraft { updated_ms: 0, ..self.clone() } == SpaceDraft { updated_ms: 0, ..other.clone() }
    }

    /// What it is called in the list: the space its draft (or the tower)
    /// names, else a name from its ticket by the tower's rule, else "New draft".
    pub fn title(&self) -> String {
        let drafted = self.drafted.as_ref().and_then(|d| d.raw["space"].as_str());
        if let Some(space) = drafted.or(self.named.as_deref()).map(str::trim).filter(|s| !s.is_empty()) {
            return space.to_string();
        }
        match crate::launch::space_name_of(&self.ticket, &[]) {
            n if n.is_empty() => "New draft".to_string(),
            n => n,
        }
    }
}

/// Every new space in draft, oldest first, and the one open last.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub drafts: Vec<SpaceDraft>,
    pub open: Option<String>,
}

impl Book {
    pub fn get(&self, id: &str) -> Option<&SpaceDraft> {
        self.drafts.iter().find(|d| d.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut SpaceDraft> {
        self.drafts.iter_mut().find(|d| d.id == id)
    }

    /// `d` as it is now: kept in its place (or last, a new one; `updated_ms`
    /// moves to `now` when something in it changed), or let go while it is
    /// empty. Whether the book changed.
    pub fn put(&mut self, d: &SpaceDraft, now: u64) -> bool {
        if d.is_empty() {
            return self.remove(&d.id).is_some();
        }
        match self.drafts.iter_mut().find(|k| k.id == d.id) {
            Some(k) if k.same(d) => false,
            Some(k) => {
                *k = SpaceDraft { updated_ms: now, ..d.clone() };
                true
            }
            None => {
                self.drafts.push(SpaceDraft { updated_ms: now, ..d.clone() });
                true
            }
        }
    }

    /// Takes the draft `id` out (launched, or deleted).
    pub fn remove(&mut self, id: &str) -> Option<SpaceDraft> {
        let i = self.drafts.iter().position(|d| d.id == id)?;
        if self.open.as_deref() == Some(id) {
            self.open = None;
        }
        Some(self.drafts.remove(i))
    }

    /// The draft to open with: the one open last, if it is still kept.
    pub fn opened(&self) -> Option<&SpaceDraft> {
        self.open.as_deref().and_then(|id| self.get(id))
    }

    /// The draft to go on with once `id` is gone: the one changed last of the others.
    pub fn next_after(&self, id: &str) -> Option<&SpaceDraft> {
        self.drafts.iter().filter(|d| d.id != id).max_by_key(|d| d.updated_ms)
    }
}

/// `<data>/new-spaces.json`.
pub fn path() -> PathBuf {
    config::data_dir().join("new-spaces.json")
}

/// The book in `file`; none when there is no file. One that doesn't read
/// is kept aside first (it is about to be written over), and said so.
pub fn load_from(file: &Path) -> Book {
    let Ok(text) = std::fs::read_to_string(file) else { return Book::default() };
    if text.trim().is_empty() {
        return Book::default();
    }
    match serde_json::from_str::<Book>(&text) {
        Ok(b) => b,
        Err(e) => {
            let aside = file.with_file_name(format!("new-spaces.bad-{}.json", crate::history::now_ms()));
            match std::fs::rename(file, &aside) {
                Ok(()) => eprintln!("[drafts] {} doesn't read ({e}); kept as {}", file.display(), aside.display()),
                Err(err) => eprintln!("[drafts] {} doesn't read ({e}), and couldn't be kept aside ({err})", file.display()),
            }
            Book::default()
        }
    }
}

/// Writes `text` (the book, as `to_text` makes it) to `file`, whole: beside it first, then moved in.
pub fn write_to(file: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_file_name(format!(".new-spaces.json.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file)
}

/// The book as it is written.
pub fn to_text(book: &Book) -> String {
    serde_json::to_string_pretty(book).unwrap_or_default()
}

/// The book in `file`, changed by `f` and written back when it changed:
/// for an answer that comes after the window that asked had closed.
pub fn update_at(file: &Path, f: impl FnOnce(&mut Book)) {
    let mut book = load_from(file);
    let before = book.clone();
    f(&mut book);
    if book != before
        && let Err(e) = write_to(file, &to_text(&book))
    {
        eprintln!("[drafts] couldn't write {}: {e}", file.display());
    }
}

/// When `file` was last written, if it is there.
pub fn written_at(file: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(file).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(who: &str, text: &str, at: u64) -> Told {
        Told { who: who.into(), text: text.into(), at }
    }

    #[test]
    fn several_drafts_are_kept_in_their_order_until_each_goes() {
        let mut b = Book::default();
        let mut one = SpaceDraft::fresh(10);
        let mut two = SpaceDraft::fresh(10);
        assert_ne!(one.id, two.id, "two started in the same moment are two");
        // Nothing said: not kept.
        assert!(!b.put(&one, 11));
        assert!(b.drafts.is_empty());
        one.ticket = "Fix the login redirect".into();
        one.story.push(said("you", "Fix the login redirect", 12));
        assert!(b.put(&one, 12));
        two.ticket = "Case exports as PDF".into();
        assert!(b.put(&two, 13));
        // The same again changes nothing, not even its time.
        assert!(!b.put(&one, 20));
        assert_eq!(b.get(&one.id).unwrap().updated_ms, 12);
        // A change keeps its place, with the time it changed.
        one.story.push(said("tower", "Drafted fix-login", 21));
        assert!(b.put(&one, 21));
        assert_eq!(b.drafts.iter().map(|d| d.title()).collect::<Vec<_>>(), ["fix-login", "case-exports"]);
        assert_eq!(b.get(&one.id).unwrap().updated_ms, 21);
        // Emptied (Start over): it goes.
        b.open = Some(two.id.clone());
        two.ticket.clear();
        assert!(b.put(&two, 22));
        assert_eq!(b.drafts.len(), 1);
        assert_eq!(b.open, None, "the open one went with it");
        assert!(b.remove(&one.id).is_some());
        assert!(b.remove(&one.id).is_none());
    }

    #[test]
    fn a_draft_is_called_by_its_space_else_by_its_ticket() {
        let mut d = SpaceDraft::fresh(1);
        assert_eq!(d.title(), "New draft");
        d.ticket = "open a session in herder-tower: try the drafts".into();
        assert_eq!(d.title(), crate::launch::space_name_of(&d.ticket, &[]));
        // The name the tower said for a first line that names the repos.
        d.named = Some("try-drafts".into());
        assert_eq!(d.title(), "try-drafts");
        d.drafted = Some(Drafting { of: d.ticket.clone(), raw: serde_json::json!({ "space": "drafts-try" }) });
        assert_eq!(d.title(), "drafts-try");
    }

    #[test]
    fn the_book_is_read_back_as_it_was_written_and_a_bad_file_is_kept_aside() {
        let dir = std::env::temp_dir().join(format!("tower-drafts-test-{}-{}", std::process::id(), crate::history::now_ms()));
        let file = dir.join("new-spaces.json");
        assert_eq!(load_from(&file), Book::default(), "no file: none");
        let mut d = SpaceDraft::fresh(5);
        d.ticket = "Voice clips for the Ingest tour".into();
        d.story = vec![said("you", "Voice clips for the Ingest tour", 6)];
        d.drafted = Some(Drafting {
            of: d.ticket.clone(),
            raw: serde_json::json!({ "space": "voice-clips", "sessions": [{ "repo": "/w/ingest", "why": "the tour" }] }),
        });
        d.picked = Some("ingest".into());
        d.review = false;
        let mut b = Book { open: Some(d.id.clone()), ..Default::default() };
        b.put(&d, 7);
        write_to(&file, &to_text(&b)).unwrap();
        let back = load_from(&file);
        assert_eq!(back, b);
        assert_eq!(back.opened().unwrap().title(), "voice-clips");
        // A file from before the review switch, or with words this tower doesn't know, still reads.
        std::fs::write(&file, r#"{"drafts":[{"id":"x","ticket":"Rework","later":1}]}"#).unwrap();
        let old = load_from(&file);
        assert!(old.drafts[0].review, "on, as a new draft has it");
        assert_eq!(old.opened(), None);
        // One that doesn't read is moved aside, not lost.
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(load_from(&file), Book::default());
        assert!(!file.exists());
        let aside: Vec<_> =
            std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("new-spaces.bad-")).collect();
        assert_eq!(aside.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_next_draft_is_the_one_changed_last() {
        let mut b = Book::default();
        let mk = |t: &str| SpaceDraft { ticket: t.into(), ..SpaceDraft::fresh(1) };
        let (a, c, e) = (mk("alpha one"), mk("charlie two"), mk("echo three"));
        b.put(&a, 30);
        b.put(&c, 10);
        b.put(&e, 20);
        assert_eq!(b.next_after(&a.id).unwrap().id, e.id);
        assert_eq!(b.next_after(&e.id).unwrap().id, a.id);
        b.remove(&a.id);
        b.remove(&e.id);
        assert!(b.next_after(&c.id).is_none());
    }
}
