//! What was typed into one of the tower's inputs and not sent yet, kept per
//! input in `<data>/drafts.json` (`~/.local/share/herder-tower/drafts.json`):
//! a restart of the tower or the orb, a window closed and opened again, or
//! another view shown and back gives each input its own text again. It goes
//! once it is sent (or emptied); one left alone for 30 days is let go.
//!
//! An input's key says which it is (`Key`): the main tower's chat (the
//! floating panel and All spaces), a space's chat, a session's view, the
//! behaviors' input, each new space in draft (New space), and the "Other"
//! field of a card's question. The files dropped on an input (its tokens,
//! `[Image #1]`) are kept with their places in its text.
//!
//! One process writes it (the orb, every window of it); its windows read
//! and write through the same lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// A draft left alone this long is let go.
const KEEP_MS: u64 = 30 * 24 * 3_600_000;

/// What an input held, unsent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub text: String,
    /// The files on it, where their tokens sit in `text` (byte offsets).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tokens: Vec<Token>,
    /// When it was last typed in.
    pub at_ms: u64,
}

/// A file's token in a draft: `text[start..end]` is its label (`[Image #1]`), `id` its path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    pub id: String,
    pub text: String,
}

impl Draft {
    /// Nothing worth keeping: no words and no files.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.tokens.is_empty()
    }
}

/// Which input a draft is of.
pub struct Key;

impl Key {
    /// The main tower's chat: the floating panel's input, and All spaces'.
    pub const MAIN: &'static str = "main";
    /// The input of the behaviors' view.
    pub const BEHAVIORS: &'static str = "behaviors";

    /// A space's chat.
    pub fn space(name: &str) -> String {
        format!("space:{name}")
    }

    /// A session's view (its input goes to its space's tower).
    pub fn session(addr: &str) -> String {
        format!("session:{addr}")
    }

    /// A new space in draft, in New space.
    pub fn new_space(draft: &str) -> String {
        format!("new:{draft}")
    }

    /// The "Other" field of a card's question (`card`: a decision's or a question set's id).
    pub fn other(card: &str, tab: usize) -> String {
        format!("other:{card}:{tab}")
    }

    /// Every "Other" field of a card.
    pub fn others_of(card: &str) -> String {
        format!("other:{card}:")
    }
}

pub fn path() -> PathBuf {
    crate::config::data_dir().join("drafts.json")
}

fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The draft of the input `key`, if one is kept.
pub fn get(key: &str) -> Option<Draft> {
    let _l = lock();
    get_in(&path(), key)
}

/// Keeps what the input `key` holds now (`None`, or nothing in it: its draft goes).
pub fn put(key: &str, draft: Option<Draft>) {
    let _l = lock();
    put_in(&path(), key, draft, crate::history::now_ms());
}

/// Lets every draft whose key starts with `prefix` go (a card's "Other"
/// fields, once its answer went).
pub fn clear(prefix: &str) {
    let _l = lock();
    clear_in(&path(), prefix);
}

/// Every draft whose key starts with `prefix` (a card's "Other" fields).
pub fn with_prefix(prefix: &str) -> Vec<(String, Draft)> {
    let _l = lock();
    load(&path()).into_iter().filter(|(k, d)| k.starts_with(prefix) && !d.is_empty()).collect()
}

fn load(file: &Path) -> BTreeMap<String, Draft> {
    std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Written whole: a temporary file, then a rename.
fn save(file: &Path, all: &BTreeMap<String, Draft>) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    if serde_json::to_vec_pretty(all).ok().is_some_and(|bytes| std::fs::write(&tmp, bytes).is_ok()) {
        let _ = std::fs::rename(&tmp, file);
    }
}

fn get_in(file: &Path, key: &str) -> Option<Draft> {
    load(file).remove(key).filter(|d| !d.is_empty())
}

fn put_in(file: &Path, key: &str, draft: Option<Draft>, now: u64) {
    let mut all = load(file);
    let before = all.clone();
    match draft.filter(|d| !d.is_empty()) {
        Some(mut d) => {
            d.at_ms = now;
            // The same words again: kept as they were (no write for a caret moved).
            if all.get(key).is_some_and(|old| old.text == d.text && old.tokens == d.tokens) {
                return;
            }
            all.insert(key.to_string(), d);
        }
        None => {
            all.remove(key);
        }
    }
    all.retain(|_, d| now.saturating_sub(d.at_ms) < KEEP_MS);
    if all != before {
        save(file, &all);
    }
}

fn clear_in(file: &Path, prefix: &str) {
    let mut all = load(file);
    let n = all.len();
    all.retain(|k, _| !k.starts_with(prefix));
    if all.len() != n {
        save(file, &all);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3_600_000;

    fn file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tower-unsent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("drafts.json")
    }

    fn words(text: &str) -> Draft {
        Draft { text: text.into(), ..Default::default() }
    }

    /// Each input gets its own text back; sent (emptied), it is gone; one
    /// left a month goes; a card's fields go together once it is answered.
    #[test]
    fn each_input_keeps_its_own_unsent_text() {
        let f = file("each");
        put_in(&f, Key::MAIN, Some(words("ask the tower about the release")), 10 * DAY);
        put_in(&f, &Key::space("isa"), Some(words("wait for the ingest filters")), 10 * DAY);
        put_in(&f, &Key::new_space("n1"), Some(words("a space for the ingest page")), 10 * DAY);
        put_in(&f, &Key::other("d1", 0), Some(words("merge it after lunch")), 10 * DAY);
        put_in(&f, &Key::other("d1", 1), Some(words("and tell eyzflow")), 10 * DAY);
        assert_eq!(get_in(&f, Key::MAIN).map(|d| d.text), Some("ask the tower about the release".into()));
        assert_eq!(get_in(&f, &Key::space("isa")).map(|d| (d.text, d.at_ms)), Some(("wait for the ingest filters".into(), 10 * DAY)));
        assert_eq!(get_in(&f, &Key::space("other")), None, "another space's input has nothing");
        // Typed on: the newest words.
        put_in(&f, Key::MAIN, Some(words("ask the tower about the release, and the docs")), 11 * DAY);
        assert_eq!(get_in(&f, Key::MAIN).map(|d| (d.text, d.at_ms)), Some(("ask the tower about the release, and the docs".into(), 11 * DAY)));
        // The same words again (a caret moved): not written again.
        put_in(&f, Key::MAIN, Some(words("ask the tower about the release, and the docs")), 12 * DAY);
        assert_eq!(get_in(&f, Key::MAIN).map(|d| d.at_ms), Some(11 * DAY));
        // Sent: the input is empty, and so is its draft; whitespace is nothing too.
        put_in(&f, Key::MAIN, Some(words("  \n")), 12 * DAY);
        assert_eq!(get_in(&f, Key::MAIN), None);
        put_in(&f, &Key::new_space("n1"), None, 12 * DAY);
        assert_eq!(get_in(&f, &Key::new_space("n1")), None);
        // A card answered: its fields go, another card's stay.
        put_in(&f, &Key::other("d12", 0), Some(words("not this one")), 12 * DAY);
        clear_in(&f, &Key::others_of("d1"));
        assert_eq!((get_in(&f, &Key::other("d1", 0)), get_in(&f, &Key::other("d1", 1))), (None, None));
        assert!(get_in(&f, &Key::other("d12", 0)).is_some(), "d1's prefix doesn't take d12's");
        // A month on, what was left alone goes with the next write.
        put_in(&f, &Key::session("t5"), Some(words("fresh")), 41 * DAY);
        assert_eq!(get_in(&f, &Key::space("isa")), None);
        assert!(get_in(&f, &Key::session("t5")).is_some());
        let _ = std::fs::remove_dir_all(f.parent().unwrap());
    }

    /// A file dropped on an input is kept with its place, so it comes back as a token.
    #[test]
    fn a_drafts_files_are_kept_with_their_places() {
        let f = file("tokens");
        let d = Draft { text: "see [Image #1] please".into(), tokens: vec![Token { start: 4, end: 14, id: "/tmp/a.png".into(), text: "[Image #1]".into() }], at_ms: 0 };
        put_in(&f, Key::MAIN, Some(d.clone()), DAY);
        let back = get_in(&f, Key::MAIN).unwrap();
        assert_eq!((back.text.as_str(), back.tokens.as_slice()), (d.text.as_str(), d.tokens.as_slice()));
        // Only a file, no words: still a draft.
        let only = Draft { text: "[File #1: a.pdf]".into(), tokens: vec![Token { start: 0, end: 16, id: "/tmp/a.pdf".into(), text: "[File #1: a.pdf]".into() }], at_ms: 0 };
        assert!(!only.is_empty());
        let _ = std::fs::remove_dir_all(f.parent().unwrap());
    }
}
