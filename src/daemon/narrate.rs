//! Speaks the brain's words as it writes them. Claude Code appends each
//! finished text block to its transcript (JSONL) at once, so following the
//! file hears a sentence written before a tool call seconds before the turn
//! ends. The Stop hook still sends the final answer, and it often comes
//! first (Claude Code writes the last block a moment after its hook runs):
//! whichever brings a text first speaks it, and the other lets it pass. The
//! hook alone speaks for a Codex brain.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::queue::Kind;
use super::timing;
use super::tts::Mouth;
use crate::config;
use crate::text;

#[derive(Clone)]
pub struct Narrator {
    inner: Arc<Mutex<Inner>>,
    mouth: Mouth,
    max_chars: usize,
    enabled: bool,
}

struct Inner {
    dir: PathBuf,
    /// How far each transcript has been read. Files there at start-up are
    /// read from their end: their past is not news.
    offsets: HashMap<PathBuf, u64>,
    current: Option<PathBuf>,
    scanned: Option<Instant>,
    /// What each side said lately, normalized.
    heard: Vec<Heard>,
    /// The mouth's hush count when the turn began: a hush since mutes the
    /// rest of the turn's narration (the final answer still comes).
    hushes: u64,
}

struct Heard {
    text: String,
    by_hook: bool,
    at: Instant,
}

/// Long enough for any turn's final answer to show up both ways.
const KEEP: Duration = Duration::from_secs(90);

impl Narrator {
    pub fn start(enabled: bool, mouth: Mouth, max_chars: usize) -> Self {
        let dir = transcripts_dir(&config::brain_dir());
        let offsets = list(&dir).into_iter().map(|(p, len, _)| (p, len)).collect();
        let inner = Inner { dir, offsets, current: None, scanned: None, heard: vec![], hushes: mouth.hushes() };
        let n = Self { inner: Arc::new(Mutex::new(inner)), mouth, max_chars, enabled };
        if enabled {
            let me = n.clone();
            thread::Builder::new()
                .name("narrate".into())
                .spawn(move || {
                    loop {
                        me.poll(false);
                        thread::sleep(Duration::from_millis(80));
                    }
                })
                .ok();
        }
        n
    }

    /// Whether this (the Stop hook's final answer) was heard already, as the
    /// brain wrote it. If not, it is about to be, and the transcript's copy
    /// will be let pass.
    pub fn already_said(&self, text: &str) -> bool {
        if !self.enabled {
            return false;
        }
        self.poll(true);
        let n = text::normalize(&text::speakable(text, self.max_chars));
        if n.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.heard.retain(|h| h.at.elapsed() < KEEP);
        if let Some(i) = inner.heard.iter().position(|h| !h.by_hook && h.text == n) {
            inner.heard.remove(i);
            return true;
        }
        // The hook's text can be several blocks in one.
        let narrated: Vec<&str> = inner.heard.iter().filter(|h| !h.by_hook).map(|h| h.text.as_str()).collect();
        if narrated.join(" ").contains(&n) {
            return true;
        }
        inner.heard.push(Heard { text: n, by_hook: true, at: Instant::now() });
        false
    }

    fn poll(&self, force: bool) {
        let mut inner = self.inner.lock().unwrap();
        let rescan = force || inner.scanned.is_none_or(|t| t.elapsed() > Duration::from_millis(700));
        if rescan {
            inner.scanned = Some(Instant::now());
            // The newest transcript is the brain's current session.
            let newest = list(&inner.dir).into_iter().max_by_key(|(_, _, m)| *m).map(|(p, _, _)| p);
            if newest.is_some() && newest != inner.current {
                if let Some(old) = inner.current.clone() {
                    self.read(&mut inner, &old);
                }
                inner.current = newest;
            }
        }
        if let Some(path) = inner.current.clone() {
            self.read(&mut inner, &path);
        }
    }

    /// Handles the complete lines added to a transcript since last time.
    fn read(&self, inner: &mut Inner, path: &Path) {
        let Ok(mut f) = std::fs::File::open(path) else { return };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let offset = *inner.offsets.entry(path.to_path_buf()).or_insert(0);
        if len <= offset {
            if len < offset {
                inner.offsets.insert(path.to_path_buf(), len);
            }
            return;
        }
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(offset)).is_err() || f.read_to_end(&mut buf).is_err() {
            return;
        }
        // A line still being written waits for its newline.
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return };
        inner.offsets.insert(path.to_path_buf(), offset + end as u64 + 1);
        for line in buf[..end].split(|&b| b == b'\n') {
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                self.line(inner, &v);
            }
        }
    }

    fn line(&self, inner: &mut Inner, v: &Value) {
        if v["isSidechain"] == true || v["isMeta"] == true {
            return;
        }
        match v["type"].as_str() {
            Some("user") => {
                let c = &v["message"]["content"];
                let prompt = c.as_str().map(String::from).or_else(|| {
                    c.as_array().and_then(|a| a.iter().find(|b| b["type"] == "text")).and_then(|b| b["text"].as_str().map(String::from))
                });
                // Tool results are not a new turn.
                let Some(prompt) = prompt else { return };
                inner.hushes = self.mouth.hushes();
                // `hc send` from inside Herdr wraps it: `<hc from="...">[voice] ...`.
                if prompt.contains("[voice]") {
                    timing::mark("the brain has it");
                }
                // What it says this turn is news for the user, not an answer.
                crate::brain::set_watch_turn(prompt.contains("[watch]"));
            }
            Some("assistant") => {
                let Some(blocks) = v["message"]["content"].as_array() else { return };
                for b in blocks {
                    let Some(t) = b["text"].as_str().filter(|_| b["type"] == "text") else { continue };
                    let spoken = text::speakable(t, self.max_chars);
                    if spoken.is_empty() {
                        continue;
                    }
                    timing::mark("the brain's first words");
                    let n = text::normalize(&spoken);
                    inner.heard.retain(|h| h.at.elapsed() < KEEP);
                    // The Stop hook brought it first.
                    if let Some(i) = inner.heard.iter().position(|h| h.by_hook && h.text == n) {
                        inner.heard.remove(i);
                        continue;
                    }
                    if inner.heard.iter().any(|h| h.by_hook && h.text.contains(&n)) {
                        continue;
                    }
                    // Hushed: say nothing more this turn but the answer.
                    if self.mouth.hushes() != inner.hushes {
                        continue;
                    }
                    inner.heard.push(Heard { text: n, by_hook: false, at: Instant::now() });
                    self.mouth.say_as(spoken, turn_kind(), false);
                }
            }
            _ => {}
        }
    }
}

/// What the brain is saying now: an answer, or a watch's news.
pub fn turn_kind() -> Kind {
    if crate::brain::watch_turn() { Kind::Alert } else { Kind::Reply }
}

/// Where Claude Code keeps the transcripts of sessions run in `dir`: every
/// character but letters and digits becomes a dash.
fn transcripts_dir(dir: &Path) -> PathBuf {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| config::home().join(".claude"));
    let name: String = dir.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    root.join("projects").join(name)
}

/// The transcripts in a folder: path, length, last change.
fn list(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((e.path(), m.len(), m.modified().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_folder() {
        let d = transcripts_dir(Path::new("/Users/me/.config/herder-tower/brain"));
        assert!(d.ends_with("projects/-Users-me--config-herder-tower-brain"), "{}", d.display());
    }
}
