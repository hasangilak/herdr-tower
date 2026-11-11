//! How much the tower tells the user (`[tell] level` in config.toml, `tower
//! tell`). It speaks in two cases: the user asked, or the decision is the
//! user's. Everything else it writes down.
//!
//! - `decisions` (the default): only what the user must decide, and direct
//!   replies, reach them.
//! - `results`: also one receipt when a request of theirs is finished.
//! - `everything`: the tower as it was before, every note told.
//!
//! What it says out loud is a setting of its own (`[tell] voice`, `tower tell
//! --voice`): `off` (nothing), `decisions` (a decision, once, in a sentence,
//! and nothing else) or `replies` (the default: its answers too).
//!
//! Both are read from config.toml again whenever the file changes, so the
//! daemon, the brain's hooks and the commands always agree. Design:
//! `docs/quiet-tower.md`.

use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Level {
    #[default]
    Decisions,
    Results,
    Everything,
}

pub const LEVELS: &[&str] = &["decisions", "results", "everything"];

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_lowercase().as_str() {
            "decisions" => Some(Level::Decisions),
            "results" => Some(Level::Results),
            "everything" => Some(Level::Everything),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Level::Decisions => "decisions",
            Level::Results => "results",
            Level::Everything => "everything",
        }
    }

    /// News is written down, not told: every level but `everything`.
    pub fn quiet(self) -> bool {
        self != Level::Everything
    }

    /// A finished request's receipt reaches the user.
    pub fn receipts(self) -> bool {
        self >= Level::Results
    }

    /// One line for the user.
    pub fn says(self) -> &'static str {
        match self {
            Level::Decisions => "Telling you decisions only: what you must decide, and answers to what you ask. The rest is in the record.",
            Level::Results => {
                "Telling you decisions and results: what you must decide, answers, and one receipt when a request is finished."
            }
            Level::Everything => "Telling you everything, as before: every delivery, finish and note.",
        }
    }
}

/// A word config.toml doesn't know is the default, never an error: a typo
/// there must not keep the voice from starting.
impl From<String> for Level {
    fn from(s: String) -> Level {
        Level::parse(&s).unwrap_or_default()
    }
}

impl From<Level> for String {
    fn from(l: Level) -> String {
        l.word().to_string()
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.word())
    }
}

/// What the tower says out loud (`[tell] voice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Voice {
    /// Nothing: no words, no bell, no chimes (`tower mute`).
    Off,
    /// A decision, once, in one sentence. Nothing else.
    Decisions,
    /// Its answers to what the user asks, too.
    #[default]
    Replies,
}

pub const VOICES: &[&str] = &["off", "decisions", "replies"];

/// A sound the tower may make, for the voice setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spoken {
    /// Its answer to what the user asked.
    Reply,
    /// "Okay.": a request went in.
    Ack,
    /// What only the user can decide or unblock, in a sentence: a
    /// question, a wait, a merge, a raise, a session stuck on an error.
    Decision,
    /// The one line for a finished request.
    Receipt,
    /// Any other news: a finish, a delivery, what the brain tells unasked.
    News,
    /// The tones of the microphone opening and closing.
    Chime,
}

impl Voice {
    pub fn parse(s: &str) -> Option<Voice> {
        match s.trim().to_lowercase().as_str() {
            "off" | "none" | "mute" | "muted" => Some(Voice::Off),
            "decisions" => Some(Voice::Decisions),
            "replies" | "on" | "all" => Some(Voice::Replies),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Voice::Off => "off",
            Voice::Decisions => "decisions",
            Voice::Replies => "replies",
        }
    }

    /// Whether a sound of this kind is made.
    pub fn speaks(self, what: Spoken) -> bool {
        match self {
            Voice::Off => false,
            // The microphone's own tones answer the talk key, not the tower.
            Voice::Decisions => matches!(what, Spoken::Decision | Spoken::Chime),
            Voice::Replies => true,
        }
    }

    /// One line for the user.
    pub fn says(self) -> &'static str {
        match self {
            Voice::Off => "It never speaks. Cards and replies still appear.",
            Voice::Decisions => "It speaks a decision once, in one sentence, and nothing else.",
            Voice::Replies => "It speaks a decision once, and answers out loud when you ask it something.",
        }
    }
}

/// A word config.toml doesn't know is the default, as for the level.
impl From<String> for Voice {
    fn from(s: String) -> Voice {
        Voice::parse(&s).unwrap_or_default()
    }
}

impl From<Voice> for String {
    fn from(v: Voice) -> String {
        v.word().to_string()
    }
}

impl std::fmt::Display for Voice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.word())
    }
}

/// A word of `[tell]` in a config.toml's text.
fn word_in(text: &str, key: &str) -> Option<String> {
    toml::from_str::<toml::Value>(text).ok().and_then(|v| v.get("tell").and_then(|t| t.get(key)).and_then(|l| l.as_str().map(String::from)))
}

/// The level a config.toml's text sets (the default when it sets none, or doesn't parse).
pub fn level_in(text: &str) -> Level {
    word_in(text, "level").and_then(|l| Level::parse(&l)).unwrap_or_default()
}

/// What a config.toml's text lets the tower say (the default when it says nothing, or doesn't parse).
pub fn voice_in(text: &str) -> Voice {
    word_in(text, "voice").and_then(|v| Voice::parse(&v)).unwrap_or_default()
}

/// The level in the config file at `path`.
#[cfg(test)]
pub fn level_at(path: &Path) -> Level {
    std::fs::read_to_string(path).map(|t| level_in(&t)).unwrap_or_default()
}

/// The voice setting in the config file at `path`.
#[cfg(test)]
pub fn voice_at(path: &Path) -> Voice {
    std::fs::read_to_string(path).map(|t| voice_in(&t)).unwrap_or_default()
}

/// `[tell]` now. The file is read again only when it changed.
fn now() -> (Level, Voice) {
    // Tests never read the user's own config.
    if cfg!(test) {
        return Default::default();
    }
    static CACHE: Mutex<Option<(Option<SystemTime>, u64, Level, Voice)>> = Mutex::new(None);
    let path = config::config_file();
    let key = std::fs::metadata(&path).ok().map(|m| (m.modified().ok(), m.len()));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    match (key, *cache) {
        (None, _) => Default::default(),
        (Some((at, len)), Some((was_at, was_len, level, voice))) if at == was_at && len == was_len => (level, voice),
        (Some((at, len)), _) => {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let (level, voice) = (level_in(&text), voice_in(&text));
            *cache = Some((at, len, level, voice));
            (level, voice)
        }
    }
}

/// The level now.
pub fn level() -> Level {
    now().0
}

/// What the tower says out loud now.
pub fn voice() -> Voice {
    now().1
}

/// Sets `key = "<word>"` under `[tell]` in a config.toml's text, keeping the rest as it is.
fn with_word(text: &str, key: &str, word: &str) -> String {
    let line = format!("{key} = \"{word}\"");
    let mut out: Vec<String> = vec![];
    let (mut in_tell, mut done, mut saw) = (false, false, false);
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            if in_tell && !done {
                // After the table's last line, not after the blank lines that end it.
                let at = out.iter().rposition(|l| !l.trim().is_empty()).map(|i| i + 1).unwrap_or(out.len());
                out.insert(at, line.clone());
                done = true;
            }
            in_tell = t == "[tell]";
            saw |= in_tell;
        } else if in_tell && !done && t.split('=').next().map(str::trim) == Some(key) {
            out.push(line.clone());
            done = true;
            continue;
        }
        out.push(l.to_string());
    }
    if in_tell && !done {
        let at = out.iter().rposition(|l| !l.trim().is_empty()).map(|i| i + 1).unwrap_or(out.len());
        out.insert(at, line.clone());
        done = true;
    }
    if !saw && !done {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push("[tell]".into());
        out.push(line);
    }
    out.join("\n") + "\n"
}

/// Sets `[tell] level` in a config.toml's text, keeping the rest as it is.
pub fn with_level(text: &str, level: Level) -> String {
    with_word(text, "level", level.word())
}

/// Sets `[tell] voice` in a config.toml's text, keeping the rest as it is.
pub fn with_voice(text: &str, voice: Voice) -> String {
    with_word(text, "voice", voice.word())
}

/// Keeps the level in the config file at `path`.
pub fn set_at(path: &Path, level: Level) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, with_level(&text, level))?;
    Ok(())
}

/// Keeps the voice setting in the config file at `path`.
pub fn set_voice_at(path: &Path, voice: Voice) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, with_voice(&text, voice))?;
    Ok(())
}

/// Keeps what the tower says out loud in config.toml.
pub fn set_voice(voice: Voice) -> Result<()> {
    config::Config::write_default_if_missing()?;
    set_voice_at(&config::config_file(), voice)
}

/// A voice setting as the user gave it.
pub fn parse_voice(word: &str) -> Result<Voice> {
    match Voice::parse(word) {
        Some(v) => Ok(v),
        None => bail!("tower tell --voice {}", VOICES.join("|")),
    }
}

/// Keeps the level in config.toml and writes the brain's instructions for
/// it (they say what a note is at this level).
pub fn set(level: Level) -> Result<()> {
    config::Config::write_default_if_missing()?;
    set_at(&config::config_file(), level)?;
    crate::brain::write_prompt()?;
    Ok(())
}

/// A level as the user (or the brain) gave it.
pub fn parse(word: &str) -> Result<Level> {
    match Level::parse(word) {
        Some(l) => Ok(l),
        None => bail!("tower tell {}", LEVELS.join("|")),
    }
}

/// A status that puts something in front of the user at every level: a
/// session waits for them, or is stuck on an error.
pub fn needs_you(status: &str) -> bool {
    matches!(status, "blocked" | "error" | "failed")
}

/// News with this status is only written down at `level`: a finish, a
/// delivery, a start. What needs the user never is.
pub fn for_record(level: Level, status: &str) -> bool {
    level.quiet() && !needs_you(status)
}

/// A receipt reaches the user (shown and spoken) at `results` and up when
/// the request was theirs (`asked`: not work the tower started on its own),
/// or when its space is followed; otherwise it is kept in the record.
pub fn receipt_told(level: Level, asked: bool, followed: bool) -> bool {
    (level.receipts() && asked) || followed
}

/// Ends every note the brain gets at a quiet level: what the note asks it to
/// tell is to be acted on instead.
pub const NOTE_TAIL: &str = " (For the record: nothing you write on this turn is shown or spoken. Act on it; `tower raise` what the user must decide or do; `tower receipt` when their request is finished.)";

/// A note for the brain as it goes in at `level`.
pub fn note_at(level: Level, msg: &str) -> String {
    if level.quiet() { format!("{msg}{NOTE_TAIL}") } else { msg.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_decisions_and_a_typo_is_the_default() {
        assert_eq!(Level::default(), Level::Decisions);
        assert_eq!(level_in(""), Level::Decisions);
        assert_eq!(level_in("voice = \"x\"\n\n[tell]\nlevel = \"results\"\n"), Level::Results);
        assert_eq!(level_in("[tell]\nlevel = \"EVERYTHING\"\n"), Level::Everything);
        assert_eq!(level_in("[tell]\nlevel = \"loud\"\n"), Level::Decisions, "a word it doesn't know");
        assert_eq!(level_in("not = toml = at all"), Level::Decisions);
        // The whole config still loads with a word it doesn't know.
        let cfg: crate::config::Config = toml::from_str("[tell]\nlevel = \"loud\"\n").unwrap();
        assert_eq!(cfg.tell.level, Level::Decisions);
        let cfg: crate::config::Config = toml::from_str("[tell]\nlevel = \"everything\"\n").unwrap();
        assert_eq!(cfg.tell.level, Level::Everything);
        assert_eq!(crate::config::Config::default().tell.level, Level::Decisions);
    }

    #[test]
    fn tower_tell_reads_and_writes_the_level() {
        let dir = std::env::temp_dir().join(format!("tower-tell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        assert_eq!(level_at(&path), Level::Decisions, "no file yet");
        set_at(&path, Level::Results).unwrap();
        assert_eq!(level_at(&path), Level::Results);
        // The rest of the file is kept, and the level changes in place.
        std::fs::write(&path, "voice = \"af_bella\"\n\n[tell]\nlevel = \"results\"\n\n[lan]\nenabled = true\n").unwrap();
        set_at(&path, Level::Everything).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "voice = \"af_bella\"\n\n[tell]\nlevel = \"everything\"\n\n[lan]\nenabled = true\n"
        );
        assert_eq!(level_at(&path), Level::Everything);
        set_at(&path, Level::Decisions).unwrap();
        assert_eq!(level_at(&path), Level::Decisions);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(parse("results").is_ok() && parse("loud").is_err());
    }

    #[test]
    fn the_voice_setting_says_which_sounds_are_made() {
        use Spoken::*;
        // Replies (the default): everything, as before the setting.
        assert_eq!(Voice::default(), Voice::Replies);
        for what in [Reply, Ack, Decision, Receipt, News, Chime] {
            assert!(Voice::Replies.speaks(what), "{what:?}");
            assert!(!Voice::Off.speaks(what), "off says nothing: {what:?}");
        }
        // Decisions: a decision's sentence, and the microphone's own tones. Nothing else.
        assert!(Voice::Decisions.speaks(Decision) && Voice::Decisions.speaks(Chime));
        for what in [Reply, Ack, Receipt, News] {
            assert!(!Voice::Decisions.speaks(what), "{what:?}");
        }
        // As config.toml has it; a word it doesn't know is the default, and the file still loads.
        assert_eq!(voice_in(""), Voice::Replies);
        assert_eq!(voice_in("[tell]\nlevel = \"results\"\nvoice = \"decisions\"\n"), Voice::Decisions);
        assert_eq!((voice_in("[tell]\nvoice = \"OFF\"\n"), voice_in("[tell]\nvoice = \"loud\"\n")), (Voice::Off, Voice::Replies));
        let cfg: crate::config::Config = toml::from_str("[tell]\nvoice = \"decisions\"\n").unwrap();
        assert_eq!((cfg.tell.voice, cfg.tell.level), (Voice::Decisions, Level::Decisions));
        assert_eq!(toml::from_str::<crate::config::Config>("[tell]\nvoice = \"shout\"\n").unwrap().tell.voice, Voice::Replies);
        // Kept beside the level, each in its place, the rest of the file as it was.
        assert_eq!(with_voice("mode = \"ptt\"\n", Voice::Decisions), "mode = \"ptt\"\n\n[tell]\nvoice = \"decisions\"\n");
        assert_eq!(
            with_voice("[tell]\nlevel = \"results\"\n\n[lan]\nenabled = true\n", Voice::Off),
            "[tell]\nlevel = \"results\"\nvoice = \"off\"\n\n[lan]\nenabled = true\n"
        );
        assert_eq!(with_level(&with_voice("[tell]\nvoice = \"off\"\n", Voice::Replies), Level::Results), "[tell]\nvoice = \"replies\"\nlevel = \"results\"\n");
        let dir = std::env::temp_dir().join(format!("tower-tell-voice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        assert_eq!(voice_at(&path), Voice::Replies, "no file yet");
        set_voice_at(&path, Voice::Decisions).unwrap();
        set_at(&path, Level::Results).unwrap();
        assert_eq!((voice_at(&path), level_at(&path)), (Voice::Decisions, Level::Results));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(parse_voice("decisions").is_ok() && parse_voice("loud").is_err());
        assert_eq!((Voice::parse("mute"), Voice::parse("on")), (Some(Voice::Off), Some(Voice::Replies)));
    }

    #[test]
    fn the_level_goes_into_its_own_table() {
        assert_eq!(with_level("voice = \"x\"\n", Level::Results), "voice = \"x\"\n\n[tell]\nlevel = \"results\"\n");
        assert_eq!(with_level("[tell]\n[vad]\nratio = 3.0\n", Level::Everything), "[tell]\nlevel = \"everything\"\n[vad]\nratio = 3.0\n");
        assert_eq!(
            with_level("[lan]\nlevel = 1\n", Level::Decisions),
            "[lan]\nlevel = 1\n\n[tell]\nlevel = \"decisions\"\n",
            "another table's `level` is left alone"
        );
    }

    #[test]
    fn what_each_level_tells() {
        use Level::*;
        assert!(Decisions.quiet() && Results.quiet() && !Everything.quiet());
        assert!(!Decisions.receipts() && Results.receipts() && Everything.receipts());
        // A finish is for the record at a quiet level; someone needing the user never is.
        for status in ["done", "idle", "exited", "news", ""] {
            assert!(for_record(Decisions, status) && for_record(Results, status), "{status}");
            assert!(!for_record(Everything, status), "{status}");
        }
        for status in ["blocked", "error", "failed"] {
            assert!(!for_record(Decisions, status) && !for_record(Everything, status), "{status}");
        }
        // Receipts: at results (and everything) for what the user asked, or for a followed space.
        assert!(!receipt_told(Decisions, true, false));
        assert!(receipt_told(Decisions, true, true), "its space is followed");
        assert!(receipt_told(Results, true, false) && receipt_told(Everything, true, false));
        assert!(!receipt_told(Results, false, false), "work the tower started on its own: for the record");
        assert!(receipt_told(Results, false, true) && receipt_told(Decisions, false, true), "a followed space's results all land");
    }

    #[test]
    fn a_note_says_it_is_for_the_record_at_a_quiet_level() {
        let msg = "[watch] t3 (api) is now done.";
        assert_eq!(note_at(Level::Everything, msg), msg);
        let quiet = note_at(Level::Decisions, msg);
        assert!(quiet.starts_with(msg) && quiet.ends_with(NOTE_TAIL));
        assert!(quiet.contains("tower raise") && quiet.contains("tower receipt"));
    }
}
