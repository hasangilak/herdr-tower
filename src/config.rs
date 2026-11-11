//! Where the tower keeps things, and the settings in `~/.config/herder-tower/config.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub fn home() -> PathBuf {
    dirs::home_dir().expect("no home directory")
}

/// `~/.config/herder-tower`: config.toml and the brain's folder.
pub fn config_dir() -> PathBuf {
    std::env::var_os("HERDER_TOWER_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config/herder-tower"))
}

/// The folder the tower's agent (the brain) runs in: its CLAUDE.md, AGENTS.md and hooks.
pub fn brain_dir() -> PathBuf {
    config_dir().join("brain")
}

/// Where each space's own brain has its folder (`brains/<space-slug>/`, see `crate::brains`).
pub fn brains_dir() -> PathBuf {
    config_dir().join("brains")
}

/// `~/.local/state/herder-tower`: the daemon's socket.
pub fn state_dir() -> PathBuf {
    std::env::var_os("HERDER_TOWER_STATE").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state/herder-tower"))
}

/// The folder the Herdr mod watches. Only `state.json` lives here, so the
/// mod wakes for nothing else.
pub fn ui_dir() -> PathBuf {
    state_dir().join("ui")
}

pub fn socket_path() -> PathBuf {
    state_dir().join("tower.sock")
}

/// `~/.local/share/herder-tower`: Whisper models, the history, attached files.
pub fn data_dir() -> PathBuf {
    std::env::var_os("HERDER_TOWER_DATA").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share/herder-tower"))
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// The `hc` binary: on PATH, else where Herdr installs it.
pub fn hc_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDER_TOWER_HC") {
        return PathBuf::from(p);
    }
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        for name in ["hc", "herdr-core"] {
            let p = PathBuf::from(dir).join(name);
            if p.is_file() {
                return p;
            }
        }
    }
    home().join(".local/bin/herdr-core")
}

/// The `herdr-app` program (Herdr's window: its spaces and tabs): on PATH,
/// or `HERDER_TOWER_HERDR_APP` (a stand-in, for trying things out).
pub fn herdr_app_bin() -> PathBuf {
    std::env::var_os("HERDER_TOWER_HERDR_APP").filter(|p| !p.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("herdr-app"))
}

/// This program, as installed (a stable path for hooks and the mod).
pub fn tower_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDER_TOWER_BIN") {
        return PathBuf::from(p);
    }
    let installed = home().join(".local/bin/tower");
    if installed.exists() {
        return installed;
    }
    std::env::current_exe().unwrap_or(installed)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Kokoro voice, or a blend such as `af_bella(2)+af_sky(1)`.
    pub voice: String,
    /// Kokoro speed, 0.5 to 2.0.
    pub speed: f32,
    /// A whisper.cpp GGML model file.
    pub whisper_model: PathBuf,
    /// Language spoken to the tower (`en`, `de`, ... or `auto`).
    pub language: String,
    /// How the microphone starts: `ptt` (press to talk), `handsfree` (always
    /// listening for a wake word) or `off`.
    pub mode: String,
    /// In hands-free mode, an utterance is for the tower only when it starts
    /// with one of these.
    pub wake_words: Vec<String>,
    /// Words that stop the tower talking.
    pub stop_words: Vec<String>,
    /// The key that talks to the tower, anywhere on the Mac: `ctrl` (either
    /// Control), `left_ctrl`, `right_ctrl` or `off`. Tap it to start a take
    /// that ends on a pause; hold it to talk until you let go.
    pub talk_key: String,
    /// The microphone, by name; empty for the system default.
    pub input_device: String,
    /// Which agent is the brain: `claude` or `codex`.
    pub brain: String,
    /// The brain's model before `[brains] model` (`tower model`) chose it:
    /// still read from a file that has it and no `[brains] model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_model: Option<String>,
    /// Its effort, likewise (`[brains] effort` now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_effort: Option<String>,
    /// Extra arguments for `claude` when it is the brain.
    pub claude_args: Vec<String>,
    /// The brain's model when it is Codex (`gpt-6.1-terra`, ...; empty:
    /// Codex's own default from `~/.codex/config.toml`).
    pub codex_model: String,
    /// Its reasoning effort: `low`, `medium`, `high`, `xhigh`, ... (empty:
    /// Codex's own default).
    pub codex_effort: String,
    /// Its service tier: `priority` is Codex's fast mode, `default` the
    /// standard speed (empty: Codex's own default).
    pub codex_service_tier: String,
    /// Extra arguments for `codex` when it is the brain.
    pub codex_args: Vec<String>,
    pub vad: Vad,
    pub alerts: Alerts,
    /// The phone API (`tower phone`): off unless enabled.
    pub lan: Lan,
    /// The agent sessions themselves (`[sessions]`).
    pub sessions: Sessions,
    /// How session questions are handled (`tower policy`): `normal` (the
    /// tower answers routine ones itself, the user decides the rest),
    /// `keep-moving` (it answers more), `ask-everything`.
    pub question_policy: String,
    /// How many characters of a reply are read aloud at most.
    pub max_spoken_chars: usize,
    /// Said the moment a request reaches the brain, in turn ("Got it.").
    /// Empty: say nothing.
    pub ack: Vec<String>,
    /// Wait this long before the acknowledgement, and skip it if the brain
    /// has spoken by then (0: at once).
    pub ack_delay_ms: u64,
    /// Speak what the brain writes as it writes it (Claude Code), not only
    /// its final message when its turn is over.
    pub narrate: bool,
    /// Speech volume, 0 to 1.
    pub volume: f32,
    /// Play everything the tower says on this Mac, even while a phone is
    /// the voice or a phone asked (`tower mac-voice`; off: the phones
    /// play it, as they route it).
    pub voice_on_mac: bool,
    /// A soft bell before an alert (an agent needs you, a watch reports
    /// back), so you know it isn't the answer to what you asked.
    pub alert_cue: bool,
    /// Alerts wait this long for the answer to your request to be said.
    pub alert_wait_ms: u64,
    /// An alert that waited longer than this (seconds) is dropped unsaid.
    pub alert_max_age_secs: u64,
    /// How much the tower tells the user (`[tell]`, `tower tell`).
    pub tell: Tell,
    /// Spaces as missions (`[spaces]`): worktrees, dev ports, closing by itself.
    pub spaces: Spaces,
    /// The agenda ledger (`[agenda]`).
    pub agenda: Agenda,
    /// The send queue (`[queue]`).
    pub queue: Queue,
    /// The tower's brains (`[brains]`): one per open space, and the model they run on.
    pub brains: Brains,
    /// What `tower launch` opens (`[[setups]]`, `tower setups`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub setups: Vec<Setup>,
}

/// `[brains]` in config.toml: one setting for every tower brain, the main
/// one and each space's own (see `crate::brains`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Brains {
    /// Each open space with a live session has a tower of its own
    /// (`tower-<space>`). Off: one brain for everything.
    pub per_space: bool,
    /// The model a brain is started on when it is Claude Code (`tower
    /// model`): a full name or one of the program's own aliases.
    pub model: String,
    /// Its effort: `low`, `medium`, `high`, `xhigh` or `max`.
    pub effort: String,
    /// The model comes from the older `claude_model` key (the file has no `[brains] model`).
    #[serde(skip)]
    pub model_from_old_key: bool,
    /// The effort comes from the older `claude_effort` key.
    #[serde(skip)]
    pub effort_from_old_key: bool,
}

impl Default for Brains {
    fn default() -> Self {
        Self {
            per_space: true,
            model: crate::brains::DEFAULT_MODEL.into(),
            effort: crate::brains::DEFAULT_EFFORT.into(),
            model_from_old_key: false,
            effort_from_old_key: false,
        }
    }
}

/// `[spaces]` in config.toml.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spaces {
    /// A session the tower opens for a space that isn't standing works in a
    /// git worktree of its repo (see `crate::worktrees`).
    pub worktrees: bool,
    /// The dev ports handed out to those sessions, one each: `"3000-3050"`.
    pub port_range: String,
    /// A space that is ready to close, its sessions all idle this many
    /// hours, is closed by the tower itself. 0 (the default): never; a ready
    /// space waits for the user to close it.
    pub auto_close_hours: u64,
}

impl Default for Spaces {
    fn default() -> Self {
        Self { worktrees: true, port_range: "3000-3050".into(), auto_close_hours: 0 }
    }
}

/// `[agenda]` in config.toml.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Agenda {
    /// A task that failed is sent to its session once more by the tower
    /// itself (at `tower tell decisions|results`). Off: nothing is sent
    /// again unasked; the brain hears of the failure and puts it to the user.
    pub retry_failed: bool,
}

impl Default for Agenda {
    fn default() -> Self {
        Self { retry_failed: true }
    }
}

/// `[queue]` in config.toml: the send queue (`tower queue`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Queue {
    /// A message one session queues for another waits while its receiver
    /// works; when the receiver is a Claude Code session that has been
    /// working longer than this (minutes), it is typed in anyway: Claude
    /// Code keeps what is typed while it works and sends it as its next
    /// message when its turn ends. 0: never (it waits for the turn to end).
    pub busy_deliver_minutes: u64,
}

impl Default for Queue {
    fn default() -> Self {
        Self { busy_deliver_minutes: 10 }
    }
}

/// One `[[setups]]` entry: a project. Its name, the Claude account its
/// sessions run on, what it is (`about`), its repos, and what each repo is
/// in it (`roles`, keyed by the repo as `repos` writes it). `tower launch`
/// opens a space with it; a ticket's draft reads `about` and the roles to
/// pick the repos the ticket needs (`crate::ticket`).
///
/// ```toml
/// [[setups]]
/// name = "EYZFlow"
/// account = "default"
/// about = "the video platform agencies run their media on"
/// repos = ["~/Work/eyz/eyzflow", "~/Work/eyz/eyz-mediaroom-serverless"]
/// roles = { "~/Work/eyz/eyzflow" = "the web app and its API", "~/Work/eyz/eyz-mediaroom-serverless" = "the media backend" }
/// ```
///
/// A table from before (no `about`, no `roles`) reads as one without them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Setup {
    pub name: String,
    pub account: String,
    /// What the project is, in a line.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub about: String,
    /// Folders (`~` allowed), one session each.
    pub repos: Vec<String>,
    /// What each repo is in the project, in a line, by the repo as `repos` has it.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub roles: std::collections::BTreeMap<String, String>,
}

impl Setup {
    /// What `repo` is in this project (its line in `roles`), matched as
    /// written or as the same folder (`~` made whole, a trailing `/` aside).
    pub fn role_of(&self, repo: &str) -> &str {
        let norm = |r: &str| crate::dispatch::expand(r.trim()).trim_end_matches('/').to_string();
        let want = norm(repo);
        self.roles.get(repo).or_else(|| self.roles.iter().find(|(k, _)| norm(k) == want).map(|(_, v)| v)).map(|v| v.trim()).unwrap_or("")
    }
}

/// `[tell]` in config.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tell {
    /// `decisions` (only what the user must decide, and direct replies),
    /// `results` (also one receipt when a request is finished) or
    /// `everything` (every note told, as before). See `crate::tell`.
    pub level: crate::tell::Level,
    /// What it says out loud: `off`, `decisions` (a decision, once, and
    /// nothing else) or `replies` (the default: its answers too).
    pub voice: crate::tell::Voice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Vad {
    /// Speech is louder than the room's noise floor by this factor.
    pub ratio: f32,
    /// ... and at least this loud (RMS, 0..1).
    pub min_rms: f32,
    /// Silence that ends an utterance in hands-free mode.
    pub handsfree_silence_ms: u32,
    /// Silence after speech that ends a press-to-talk take.
    pub ptt_silence_ms: u32,
    /// A press-to-talk take with no speech in this long is dropped.
    pub ptt_no_speech_ms: u32,
    /// The longest take.
    pub max_utterance_ms: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Alerts {
    /// Say so when any agent, on any machine, needs a person.
    pub blocked: bool,
    /// Say so when any agent finishes a turn.
    pub done: bool,
    /// Check other machines this often (seconds; 0: never).
    pub remote_poll_secs: u64,
    /// Put sessions the tower starts in a tab of the Herdr window (they run
    /// hidden otherwise).
    pub show_sessions: bool,
    /// With a space in focus, news from other spaces is held; but an agent
    /// there needing the user still rings the bell and says "<space> needs you".
    pub held_urgent: bool,
    /// A focus ends this many hours after it was set (0: never), so a focus
    /// left on doesn't keep holding other spaces' news for days.
    pub focus_hours: u64,
}

impl Default for Config {
    fn default() -> Self {
        let turbo = data_dir().join("models/ggml-large-v3-turbo-q5_0.bin");
        let base = home().join(".config/sasha/voice/ggml-base.en.bin");
        let whisper_model = if !turbo.exists() && base.exists() { base } else { turbo };
        Self {
            voice: "af_bella".into(),
            speed: 1.05,
            whisper_model,
            language: "en".into(),
            mode: "ptt".into(),
            wake_words: ["hey tower", "okay tower", "ok tower", "tower"].map(String::from).to_vec(),
            stop_words: ["stop", "hush", "quiet", "shut up", "be quiet", "enough"].map(String::from).to_vec(),
            talk_key: "ctrl".into(),
            input_device: String::new(),
            brain: "claude".into(),
            claude_model: None,
            claude_effort: None,
            claude_args: vec![],
            codex_model: String::new(),
            // Measured with the tower's prompt: Terra at its usual xhigh with
            // fast mode beat low and medium effort.
            codex_effort: String::new(),
            codex_service_tier: "priority".into(),
            codex_args: vec![],
            vad: Vad::default(),
            alerts: Alerts::default(),
            lan: Lan::default(),
            sessions: Sessions::default(),
            question_policy: "normal".into(),
            max_spoken_chars: 900,
            ack: ["Got it.", "Okay.", "Sure."].map(String::from).to_vec(),
            ack_delay_ms: 0,
            narrate: true,
            volume: 1.0,
            voice_on_mac: false,
            alert_cue: true,
            alert_wait_ms: 20_000,
            alert_max_age_secs: 90,
            tell: Tell::default(),
            spaces: Spaces::default(),
            agenda: Agenda::default(),
            queue: Queue::default(),
            brains: Brains::default(),
            setups: vec![],
        }
    }
}

impl Default for Vad {
    fn default() -> Self {
        Self {
            ratio: 3.0,
            min_rms: 0.006,
            handsfree_silence_ms: 800,
            ptt_silence_ms: 1400,
            ptt_no_speech_ms: 8000,
            max_utterance_ms: 60_000,
        }
    }
}

/// `[sessions]` in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Sessions {
    /// When a Claude session joins a Herdr space that has others, tell every
    /// Claude session there who the others are (`tower introduce`).
    pub introduce: bool,
}

impl Default for Sessions {
    fn default() -> Self {
        Self { introduce: true }
    }
}

/// The phone API, for the Android app (docs/mobile-api.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Lan {
    /// Off unless turned on (`tower phone enable`).
    pub enabled: bool,
    pub port: u16,
    /// The address to listen on: `auto` (this Mac's address on the local
    /// network, the one its default route uses) or an IP of this Mac.
    pub bind: String,
    /// Listen on an address that isn't private (10/8, 172.16/12, 192.168/16,
    /// 100.64/10, link-local). Leave off.
    pub allow_public: bool,
    /// Also listen on this Mac's Tailscale address (100.64/10), while
    /// Tailscale runs: `auto` (the default) or `off`.
    pub tailscale: String,
    /// `[lan.networks]`: the names the user gave the networks this Mac is
    /// on, by range (the /24 of its address there): `"10.10.30" = "Office"`
    /// (`tower phone network name "Office"`, or Settings > Phone).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub networks: std::collections::BTreeMap<String, String>,
    /// Be found by name on the local network (Bonjour: `_herdr-tower._tcp`
    /// on the LAN listener's interface), so a paired phone finds this Mac on
    /// whatever Wi-Fi both are on. On by default.
    pub bonjour: bool,
    /// The name it is found by; empty: this Mac's name (Sharing's Computer Name).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub bonjour_name: String,
}

impl Default for Lan {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7843,
            bind: "auto".into(),
            allow_public: false,
            tailscale: "auto".into(),
            networks: Default::default(),
            bonjour: true,
            bonjour_name: String::new(),
        }
    }
}

impl Default for Alerts {
    fn default() -> Self {
        Self { blocked: true, done: false, remote_poll_secs: 15, show_sessions: true, held_urgent: true, focus_hours: 8 }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_text(&text).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The settings as config.toml has them now, for what reads them while
    /// it runs (the daemon's looks, every few seconds). A file that doesn't
    /// read just then (a hand edit saved with a typo, a write caught half
    /// way) keeps the last settings that did read, never the defaults: the
    /// defaults close spaces by themselves, send failed tasks again and
    /// start a brain per space, and the user may have turned those off.
    pub fn live() -> Self {
        static LAST: std::sync::Mutex<Option<Config>> = std::sync::Mutex::new(None);
        Self::kept(Self::load(), &mut LAST.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// `live`, of a read and the last one that worked (before any did: the defaults).
    fn kept(read: Result<Self>, last: &mut Option<Config>) -> Self {
        match read {
            Ok(cfg) => {
                *last = Some(cfg.clone());
                cfg
            }
            Err(e) => {
                eprintln!("[config] {e:#}; keeping the settings as they last read");
                last.clone().unwrap_or_default()
            }
        }
    }

    /// The settings a config.toml's text holds. The brains' model and
    /// effort are `[brains] model` and `effort`; a file that says neither
    /// but still has the older `claude_model` / `claude_effort` keeps what
    /// those say; with none of them, the defaults.
    pub fn from_text(text: &str) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text)?;
        let raw: toml::Value = toml::from_str(text)?;
        let said = |key: &str| raw.get("brains").and_then(|b| b.get(key)).and_then(|v| v.as_str()).is_some_and(|s| !s.trim().is_empty());
        let old = |v: &Option<String>| v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let defaults = Brains::default();
        match (said("model"), old(&cfg.claude_model)) {
            (true, _) => cfg.brains.model = cfg.brains.model.trim().to_string(),
            (false, Some(m)) => (cfg.brains.model, cfg.brains.model_from_old_key) = (m, true),
            (false, None) => cfg.brains.model = defaults.model,
        }
        match (said("effort"), old(&cfg.claude_effort)) {
            (true, _) => cfg.brains.effort = cfg.brains.effort.trim().to_string(),
            (false, Some(e)) => (cfg.brains.effort, cfg.brains.effort_from_old_key) = (e, true),
            (false, None) => cfg.brains.effort = defaults.effort,
        }
        Ok(cfg)
    }

    /// Writes the defaults, commented, unless a config exists.
    pub fn write_default_if_missing() -> Result<bool> {
        let path = config_file();
        if path.exists() {
            return Ok(false);
        }
        std::fs::create_dir_all(config_dir())?;
        let text = toml::to_string_pretty(&Self::default())?;
        std::fs::write(&path, format!("# herder-tower settings. Restart with `tower restart` after a change.\n\n{text}"))?;
        Ok(true)
    }
}

/// Sets `key = <value>` under `[table]` in a config.toml's text, keeping
/// the rest as it is (comments, order, the other tables). `value` is
/// written as given: a TOML value (`true`, `"high"`). The table is added at
/// the end when the file has none.
pub fn with_table_key(text: &str, table: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = {value}");
    let header = format!("[{table}]");
    let is_header = |l: &str| l.split('#').next().unwrap_or("").trim() == header;
    // Where a table's last key ends: before the blank lines and the comments that lead into the next one.
    let end = |out: &[String]| out.iter().rposition(|l| !l.trim().is_empty() && !l.trim().starts_with('#')).map(|i| i + 1).unwrap_or(out.len());
    let mut out: Vec<String> = vec![];
    let (mut inside, mut done, mut saw) = (false, false, false);
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            if inside && !done {
                out.insert(end(&out), line.clone());
                done = true;
            }
            inside = !done && is_header(t);
            saw |= inside;
        } else if inside && !done && !t.starts_with('#') && t.split('=').next().map(str::trim) == Some(key) {
            out.push(line.clone());
            done = true;
            continue;
        }
        out.push(l.to_string());
    }
    if inside && !done {
        out.insert(end(&out), line.clone());
        done = true;
    }
    if !saw && !done {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(header);
        out.push(line);
    }
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_stops_reading_keeps_what_the_user_turned_off() {
        // The user turned off the three things the tower does by itself.
        let off = Config::from_text("[spaces]\nauto_close_hours = 0\n\n[agenda]\nretry_failed = false\n\n[brains]\nper_space = false\n").unwrap();
        let mut last = None;
        let now = Config::kept(Ok(off), &mut last);
        assert_eq!((now.spaces.auto_close_hours, now.agenda.retry_failed, now.brains.per_space), (0, false, false));
        // A hand edit saved with a typo: the next look still has them off (the defaults are 24, true, true).
        let broken = Config::from_text("[spaces]\nauto_close_hours = 0\nvoice = \n");
        assert!(broken.is_err());
        let now = Config::kept(broken, &mut last);
        assert_eq!((now.spaces.auto_close_hours, now.agenda.retry_failed, now.brains.per_space), (0, false, false));
        let d = Config::default();
        assert_eq!((d.spaces.auto_close_hours, d.agenda.retry_failed, d.brains.per_space), (0, true, true), "a ready space waits for the user");
        // Fixed, with other words: those hold from then on.
        let now = Config::kept(Config::from_text("[spaces]\nauto_close_hours = 48\n"), &mut last);
        assert_eq!((now.spaces.auto_close_hours, last.as_ref().map(|c| c.spaces.auto_close_hours)), (48, Some(48)));
    }

    #[test]
    fn a_message_goes_into_a_busy_session_after_ten_minutes_unless_set_otherwise() {
        assert_eq!(Config::default().queue.busy_deliver_minutes, 10);
        assert_eq!(Config::from_text("[queue]\nbusy_deliver_minutes = 0\n").unwrap().queue.busy_deliver_minutes, 0, "0: never");
        assert_eq!(Config::from_text("[queue]\nbusy_deliver_minutes = 30\n").unwrap().queue.busy_deliver_minutes, 30);
        assert_eq!(Config::from_text("voice = \"af_bella\"\n").unwrap().queue.busy_deliver_minutes, 10, "a file from before has the default");
    }

    /// A config.toml as an install from before the brains' own table has it.
    const OLD: &str = "# herder-tower settings.\n\nvoice = \"af_bella\"\nbrain = \"claude\"\nclaude_args = []\ncodex_args = []\n\n[vad]\nratio = 3.0\n\n[alerts]\nblocked = true # say so\n\n[[setups]]\nname = \"web\"\naccount = \"orca\"\nrepos = [\"~/w/web\"]\n";

    #[test]
    fn a_file_that_says_nothing_gets_the_defaults() {
        let cfg = Config::from_text(OLD).unwrap();
        assert_eq!((cfg.brains.model.as_str(), cfg.brains.effort.as_str(), cfg.brains.per_space), ("claude-opus-5-5", "high", true));
        assert!(!cfg.brains.model_from_old_key && !cfg.brains.effort_from_old_key);
        assert_eq!(cfg.brain, "claude", "which program the brain is stays its own key");
        // No file at all: the same.
        let none = Config::default();
        assert_eq!((none.brains.model.as_str(), none.brains.effort.as_str(), none.brains.per_space), ("claude-opus-5-5", "high", true));
        // The defaults as a new install writes them say so in the file, and never the older keys.
        let written = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(written.contains("[brains]\nper_space = true\nmodel = \"claude-opus-5-5\"\neffort = \"high\""), "{written}");
        assert!(!written.contains("claude_model") && !written.contains("claude_effort") && !written.contains("old_key"), "{written}");
        assert_eq!(Config::from_text(&written).unwrap().brains, Brains::default());
    }

    #[test]
    fn the_older_keys_are_kept_until_the_brains_table_says_otherwise() {
        let old = OLD.replace("claude_args = []", "claude_model = \"sonnet\"\nclaude_effort = \"medium\"\nclaude_args = []");
        let cfg = Config::from_text(&old).unwrap();
        assert_eq!((cfg.brains.model.as_str(), cfg.brains.effort.as_str()), ("sonnet", "medium"), "what the file says is kept");
        assert!(cfg.brains.model_from_old_key && cfg.brains.effort_from_old_key);
        // The picker's choice wins, key by key.
        let picked = with_table_key(&old, "brains", "model", "\"claude-fable-5-2\"");
        let cfg = Config::from_text(&picked).unwrap();
        assert_eq!((cfg.brains.model.as_str(), cfg.brains.effort.as_str()), ("claude-fable-5-2", "medium"));
        assert!(!cfg.brains.model_from_old_key && cfg.brains.effort_from_old_key);
        // An empty value says nothing.
        let empty = OLD.replace("claude_args = []", "claude_model = \"\"\nclaude_args = []") + "\n[brains]\nmodel = \" \"\n";
        let cfg = Config::from_text(&empty).unwrap();
        assert_eq!((cfg.brains.model.as_str(), cfg.brains.model_from_old_key), ("claude-opus-5-5", false));
    }

    #[test]
    fn a_key_is_set_in_its_table_and_the_rest_of_the_file_stays() {
        // No table yet: added at the end, after everything else.
        let a = with_table_key(OLD, "brains", "model", "\"claude-sonnet-5-5\"");
        assert!(a.starts_with(OLD.trim_end()), "{a}");
        assert!(a.ends_with("\n[brains]\nmodel = \"claude-sonnet-5-5\"\n"), "{a}");
        // A second key joins the table; the first is changed in its place.
        let b = with_table_key(&a, "brains", "effort", "\"xhigh\"");
        assert!(b.ends_with("\n[brains]\nmodel = \"claude-sonnet-5-5\"\neffort = \"xhigh\"\n"), "{b}");
        let c = with_table_key(&b, "brains", "model", "\"claude-haiku-5-5\"");
        assert!(c.ends_with("\n[brains]\nmodel = \"claude-haiku-5-5\"\neffort = \"xhigh\"\n"), "{c}");
        let d = with_table_key(&c, "brains", "per_space", "false");
        let cfg = Config::from_text(&d).unwrap();
        assert_eq!((cfg.brains.model.as_str(), cfg.brains.effort.as_str(), cfg.brains.per_space), ("claude-haiku-5-5", "xhigh", false));
        // Nothing else was lost: the comment, the other tables, the setups.
        assert!(d.contains("# herder-tower settings.") && d.contains("blocked = true # say so"));
        assert_eq!((cfg.voice.as_str(), cfg.brain.as_str(), cfg.vad.ratio, cfg.setups.len(), cfg.setups[0].name.as_str()), ("af_bella", "claude", 3.0, 1, "web"));
        // A table in the middle of the file: the key lands in it, not in the next one.
        let mid = "[brains]\nper_space = false\n\n# the phone\n[lan]\nenabled = true\n";
        let e = with_table_key(mid, "brains", "model", "\"x\"");
        assert_eq!(e, "[brains]\nper_space = false\nmodel = \"x\"\n\n# the phone\n[lan]\nenabled = true\n");
        // A key of the same name in another table, or commented out, is left alone.
        let other = "[tell]\nlevel = \"results\"\n\n[brains] # the towers\n# model = \"old\"\neffort = \"low\"\n";
        let f = with_table_key(other, "brains", "model", "\"y\"");
        assert_eq!(f, "[tell]\nlevel = \"results\"\n\n[brains] # the towers\n# model = \"old\"\neffort = \"low\"\nmodel = \"y\"\n");
        assert_eq!(with_table_key("", "brains", "per_space", "true"), "[brains]\nper_space = true\n");
    }
}
