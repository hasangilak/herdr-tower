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
use super::state::lock;
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
    /// How much the tower tells the user now (`tell::level`).
    level: fn() -> crate::tell::Level,
}

struct Inner {
    /// Each brain's transcripts, followed: the main brain's, and every
    /// space's own while it runs.
    trails: Vec<Trail>,
    /// What each side said lately, normalized.
    heard: Vec<Heard>,
}

/// One brain's transcripts.
struct Trail {
    /// Whose they are (`tower`, `tower-<space>`): whose turn a line is.
    brain: String,
    dir: PathBuf,
    /// How far each transcript has been read. Files there when the brain
    /// is first followed are read from their end: their past is not news.
    offsets: HashMap<PathBuf, u64>,
    current: Option<PathBuf>,
    scanned: Option<Instant>,
    /// The mouth's hush count when the turn began: a hush since mutes the
    /// rest of the turn's narration (the final answer still comes).
    hushes: u64,
}

impl Trail {
    fn new(brain: &str, dir: PathBuf, hushes: u64) -> Trail {
        let offsets = list(&dir).into_iter().map(|(p, len, _)| (p, len)).collect();
        Trail { brain: brain.to_string(), dir, offsets, current: None, scanned: None, hushes }
    }
}

struct Heard {
    /// Whose words.
    brain: String,
    text: String,
    by_hook: bool,
    at: Instant,
}

/// Long enough for any turn's final answer to show up both ways.
const KEEP: Duration = Duration::from_secs(90);

impl Narrator {
    pub fn start(enabled: bool, mouth: Mouth, max_chars: usize) -> Self {
        let main = Trail::new(&crate::brain::name(), transcripts_dir(&config::brain_dir()), mouth.hushes());
        let inner = Inner { trails: vec![main], heard: vec![] };
        let n = Self { inner: Arc::new(Mutex::new(inner)), mouth, max_chars, enabled, level: crate::tell::level };
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

    /// Follows another brain's transcripts too: a space's own, run in
    /// `folder`. What it wrote before now is not news.
    pub fn follow(&self, brain: &str, folder: &Path) {
        let mut inner = lock(&self.inner);
        if inner.trails.iter().any(|t| t.brain == brain) {
            return;
        }
        let trail = Trail::new(brain, transcripts_dir(folder), self.mouth.hushes());
        inner.trails.push(trail);
    }

    /// A brain is gone: its transcripts are followed no more.
    pub fn unfollow(&self, brain: &str) {
        let mut inner = lock(&self.inner);
        inner.trails.retain(|t| t.brain != brain || t.brain == crate::brain::name());
    }

    /// The brains whose transcripts are followed.
    #[cfg(test)]
    pub fn following(&self) -> Vec<String> {
        lock(&self.inner).trails.iter().map(|t| t.brain.clone()).collect()
    }

    /// Whether this (the Stop hook's final answer, of the brain called
    /// `brain`) was heard already, as the brain wrote it. If not, it is
    /// about to be, and the transcript's copy will be let pass.
    pub fn already_said(&self, brain: &str, text: &str) -> bool {
        if !self.enabled {
            return false;
        }
        self.poll(true);
        let n = text::normalize(&text::speakable(text, self.max_chars));
        if n.is_empty() {
            return false;
        }
        let mut inner = lock(&self.inner);
        inner.heard.retain(|h| h.at.elapsed() < KEEP);
        if let Some(i) = inner.heard.iter().position(|h| !h.by_hook && h.brain == brain && h.text == n) {
            inner.heard.remove(i);
            return true;
        }
        // The hook's text can be several blocks in one.
        let narrated: Vec<&str> = inner.heard.iter().filter(|h| !h.by_hook && h.brain == brain).map(|h| h.text.as_str()).collect();
        if narrated.join(" ").contains(&n) {
            return true;
        }
        inner.heard.push(Heard { brain: brain.to_string(), text: n, by_hook: true, at: Instant::now() });
        false
    }

    fn poll(&self, force: bool) {
        let mut inner = lock(&self.inner);
        for i in 0..inner.trails.len() {
            let rescan = force || inner.trails[i].scanned.is_none_or(|t| t.elapsed() > Duration::from_millis(700));
            if rescan {
                inner.trails[i].scanned = Some(Instant::now());
                // The newest transcript is the brain's current session.
                let newest = list(&inner.trails[i].dir).into_iter().max_by_key(|(_, _, m)| *m).map(|(p, _, _)| p);
                if newest.is_some() && newest != inner.trails[i].current {
                    if let Some(old) = inner.trails[i].current.clone() {
                        self.read(&mut inner, i, &old);
                    }
                    inner.trails[i].current = newest;
                }
            }
            if let Some(path) = inner.trails[i].current.clone() {
                self.read(&mut inner, i, &path);
            }
        }
    }

    /// Handles the complete lines added to a transcript (of trail `i`) since last time.
    fn read(&self, inner: &mut Inner, i: usize, path: &Path) {
        let Ok(mut f) = std::fs::File::open(path) else { return };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let offset = *inner.trails[i].offsets.entry(path.to_path_buf()).or_insert(0);
        if len <= offset {
            if len < offset {
                inner.trails[i].offsets.insert(path.to_path_buf(), len);
            }
            return;
        }
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(offset)).is_err() || f.read_to_end(&mut buf).is_err() {
            return;
        }
        // A line still being written waits for its newline.
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return };
        inner.trails[i].offsets.insert(path.to_path_buf(), offset + end as u64 + 1);
        for line in buf[..end].split(|&b| b == b'\n') {
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                self.line(inner, i, &v);
            }
        }
    }

    /// One line of the transcript of trail `i`: a prompt begins that brain's
    /// turn, and what it writes is spoken as that turn allows.
    fn line(&self, inner: &mut Inner, i: usize, v: &Value) {
        if v["isSidechain"] == true || v["isMeta"] == true {
            return;
        }
        let who = inner.trails[i].brain.clone();
        match v["type"].as_str() {
            Some("user") => {
                let c = &v["message"]["content"];
                let prompt = c.as_str().map(String::from).or_else(|| {
                    c.as_array().and_then(|a| a.iter().find(|b| b["type"] == "text")).and_then(|b| b["text"].as_str().map(String::from))
                });
                // Tool results are not a new turn.
                let Some(prompt) = prompt else { return };
                inner.trails[i].hushes = self.mouth.hushes();
                // `hc send` from inside Herdr wraps it: `<hc from="...">[voice] ...`.
                if prompt.contains("[voice]") {
                    timing::mark("the brain has it");
                }
                // News for the user and not an answer; a note from the
                // tower (at a quiet level, for the record); a question note.
                crate::brain::set_turn(&who, crate::brain::Turn::of_prompt(&prompt));
            }
            Some("assistant") => {
                let Some(blocks) = v["message"]["content"].as_array() else { return };
                for b in blocks {
                    let Some(t) = b["text"].as_str().filter(|_| b["type"] == "text") else { continue };
                    let spoken = text::speakable(t, self.max_chars);
                    let turn = crate::brain::turn_of(&who);
                    // Recommending answers: quiet.
                    if spoken.is_empty() || turn.question {
                        continue;
                    }
                    // A note turn: its words are for the record unless the tower tells everything.
                    if turn.note && (self.level)().quiet() {
                        continue;
                    }
                    timing::mark("the brain's first words");
                    let n = text::normalize(&spoken);
                    inner.heard.retain(|h| h.at.elapsed() < KEEP);
                    // The Stop hook brought it first.
                    if let Some(i) = inner.heard.iter().position(|h| h.by_hook && h.brain == who && h.text == n) {
                        inner.heard.remove(i);
                        continue;
                    }
                    if inner.heard.iter().any(|h| h.by_hook && h.brain == who && h.text.contains(&n)) {
                        continue;
                    }
                    // Hushed: say nothing more this turn but the answer.
                    if self.mouth.hushes() != inner.trails[i].hushes {
                        continue;
                    }
                    inner.heard.push(Heard { brain: who.clone(), text: n, by_hook: false, at: Instant::now() });
                    self.mouth.say_as(spoken, turn_kind_of(&who), false);
                }
            }
            _ => {}
        }
    }
}

/// What the main brain is saying now: an answer, or a watch's news.
#[cfg(test)]
pub fn turn_kind() -> Kind {
    turn_kind_of(&crate::brain::name())
}

/// What the brain called `brain` is saying now: an answer, or a watch's news.
pub fn turn_kind_of(brain: &str) -> Kind {
    if crate::brain::turn_of(brain).watch { Kind::Alert } else { Kind::Reply }
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

    /// The turn's kind is one flag for the whole process: one test at a time reads it.
    static TURN: Mutex<()> = Mutex::new(());

    /// A narrator at `level`, and what it gives the mouth to say.
    fn narrator(level: fn() -> crate::tell::Level) -> (Narrator, std::sync::mpsc::Receiver<super::super::tts::Msg>) {
        let hub = Arc::new(super::super::lan::hub::Hub::default());
        let (mouth, said) = Mouth::for_test(hub, super::super::tts::test_mute_path());
        (Narrator { inner: Arc::new(Mutex::new(inner_of(&["tower"]))), mouth, max_chars: 300, enabled: false, level }, said)
    }

    /// Trails for these brains, over folders that hold nothing.
    fn inner_of(brains: &[&str]) -> Inner {
        let dir = std::env::temp_dir().join(format!("tower-narrate-{}", std::process::id()));
        Inner { trails: brains.iter().map(|b| Trail { brain: b.to_string(), dir: dir.join(b), offsets: HashMap::new(), current: None, scanned: None, hushes: 0 }).collect(), heard: vec![] }
    }

    fn spoken(said: &std::sync::mpsc::Receiver<super::super::tts::Msg>) -> Vec<String> {
        said.try_iter()
            .filter_map(|m| match m {
                super::super::tts::Msg::Say { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_note_turn_is_silent_unless_the_tower_tells_everything() {
        let _one = lock(&TURN);
        let user = |text: &str| serde_json::json!({ "type": "user", "message": { "content": text } });
        let says = |text: &str| serde_json::json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": text }] } });
        for level in [|| crate::tell::Level::Decisions, || crate::tell::Level::Results] as [fn() -> crate::tell::Level; 2] {
            let (n, said) = narrator(level);
            let mut inner = lock(&n.inner);
            for note in ["[watch] t3 (api) is now done. You had asked it: fix the login.", "[queue] t5 (web) is gone, with 1 instruction(s) queued for it", "[agenda] a3 is done: every task is settled"] {
                n.line(&mut inner, 0, &user(note));
                n.line(&mut inner, 0, &says("The api session finished the login fix."));
            }
            assert!(spoken(&said).is_empty(), "a note turn's words are for the record");
            assert!(inner.heard.is_empty());
            // The user asks: the answer is spoken, as always.
            n.line(&mut inner, 0, &user("[voice] is the login fixed?"));
            n.line(&mut inner, 0, &says("Yes, the api session fixed it."));
            assert_eq!(spoken(&said), vec!["Yes, the api session fixed it.".to_string()]);
        }
        // Telling everything: the note's news is told, as before.
        let (n, said) = narrator(|| crate::tell::Level::Everything);
        let mut inner = lock(&n.inner);
        n.line(&mut inner, 0, &user("[watch] t3 (api) is now done. You had asked it: fix the login."));
        n.line(&mut inner, 0, &says("The api session finished the login fix."));
        assert_eq!(spoken(&said), vec!["The api session finished the login fix.".to_string()]);
        assert_eq!(turn_kind(), Kind::Alert, "as news, after the bell");
        n.line(&mut inner, 0, &user("[voice] thanks"));
    }

    #[test]
    fn a_queue_note_is_news_too() {
        let _one = lock(&TURN);
        let hub = Arc::new(super::super::lan::hub::Hub::default());
        let n = Narrator { inner: Arc::new(Mutex::new(inner_of(&["tower"]))), mouth: Mouth::silent(hub), max_chars: 300, enabled: false, level: || crate::tell::Level::Everything };
        let user = |text: &str| serde_json::json!({ "type": "user", "message": { "content": text } });
        let mut inner = lock(&n.inner);
        n.line(&mut inner, 0, &user("<hc from=\"tower\">[queue] delivered \"run the tests\" to eyzflow"));
        assert!(crate::brain::turn_of("tower").watch);
        assert_eq!(turn_kind(), Kind::Alert);
        n.line(&mut inner, 0, &user("[watch] eyzflow finished"));
        assert!(crate::brain::turn_of("tower").watch);
        n.line(&mut inner, 0, &user("[voice] what's running?"));
        assert!(!crate::brain::turn_of("tower").watch);
    }

    #[test]
    fn each_brain_has_its_own_turn() {
        let _one = lock(&TURN);
        let hub = Arc::new(super::super::lan::hub::Hub::default());
        let (mouth, said) = Mouth::for_test(hub, super::super::tts::test_mute_path());
        let n = Narrator { inner: Arc::new(Mutex::new(inner_of(&["tower", "tower-sandbox"]))), mouth, max_chars: 300, enabled: false, level: || crate::tell::Level::Decisions };
        let user = |text: &str| serde_json::json!({ "type": "user", "message": { "content": text } });
        let says = |text: &str| serde_json::json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": text }] } });
        let mut inner = lock(&n.inner);
        // The space's brain is on a note turn while the main one answers the user.
        n.line(&mut inner, 1, &user("[watch] t5 (sandbox app) is now done. You had asked it: fix the login."));
        n.line(&mut inner, 0, &user("[voice] what's left?"));
        assert!(crate::brain::turn_of("tower-sandbox").note && !crate::brain::turn_of("tower").note);
        n.line(&mut inner, 1, &says("Recorded: the login is fixed."));
        n.line(&mut inner, 0, &says("Two requests are open."));
        assert_eq!(spoken(&said), vec!["Two requests are open.".to_string()], "the note turn's words are for the record; the answer is said");
        // Asked in its space's view: the space's brain answers, and is heard like the main one.
        n.line(&mut inner, 1, &user("[voice] is the login fixed? [radar] t5 sandbox app: done"));
        n.line(&mut inner, 0, &user("[agenda] a3 is done: every task is settled"));
        assert!(!crate::brain::turn_of("tower-sandbox").note && crate::brain::turn_of("tower").note);
        n.line(&mut inner, 1, &says("Yes, the sandbox app session fixed it."));
        n.line(&mut inner, 0, &says("Nothing to add."));
        assert_eq!(spoken(&said), vec!["Yes, the sandbox app session fixed it.".to_string()]);
        assert_eq!((turn_kind_of("tower-sandbox"), turn_kind_of("tower")), (Kind::Reply, Kind::Reply));
        // The same words from two brains are each their own: one's hook doesn't swallow the other's.
        inner.heard.clear();
        drop(inner);
        let n = Narrator { enabled: true, ..n };
        assert!(!n.already_said("tower", "Done."), "the main brain's hook brings it first");
        assert!(!n.already_said("tower-sandbox", "Done."), "the space's own says the same: still to be said");
        let mut inner = lock(&n.inner);
        n.line(&mut inner, 0, &user("[voice] and now?"));
        n.line(&mut inner, 0, &says("Done."));
        assert!(spoken(&said).is_empty(), "the transcript's copy of what its hook brought is let pass");
        assert_eq!(inner.heard.iter().filter(|h| h.by_hook).map(|h| h.brain.as_str()).collect::<Vec<_>>(), ["tower-sandbox"]);
        crate::brain::forget_turn("tower-sandbox");
        n.line(&mut inner, 0, &user("[voice] thanks"));
    }

    #[test]
    fn a_space_brains_transcripts_are_followed_while_it_runs() {
        let hub = Arc::new(super::super::lan::hub::Hub::default());
        let n = Narrator { inner: Arc::new(Mutex::new(inner_of(&["tower"]))), mouth: Mouth::silent(hub), max_chars: 300, enabled: false, level: || crate::tell::Level::Decisions };
        let folder = std::env::temp_dir().join(format!("tower-narrate-follow-{}", std::process::id())).join("brains/sandbox");
        n.follow("tower-sandbox", &folder);
        n.follow("tower-sandbox", &folder);
        assert_eq!(n.following(), ["tower", "tower-sandbox"]);
        assert_eq!(lock(&n.inner).trails[1].dir, transcripts_dir(&folder));
        n.unfollow("tower-sandbox");
        n.unfollow("tower");
        assert_eq!(n.following(), ["tower"], "the main brain's are always followed");
    }

    #[test]
    fn transcript_folder() {
        let d = transcripts_dir(Path::new("/Users/me/.config/herder-tower/brain"));
        assert!(d.ends_with("projects/-Users-me--config-herder-tower-brain"), "{}", d.display());
    }
}
