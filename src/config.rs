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
    /// The brain's model when it is Claude Code (`sonnet`, `opus`, `haiku`
    /// or a full model name; empty: Claude Code's own default). A voice
    /// waits for every step it takes, so a fast one matters most.
    pub claude_model: String,
    /// Its effort: `low`, `medium`, `high`, `xhigh`, `max` (empty: Claude
    /// Code's own default, which may be your `effortLevel`).
    pub claude_effort: String,
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
    /// A soft bell before an alert (an agent needs you, a watch reports
    /// back), so you know it isn't the answer to what you asked.
    pub alert_cue: bool,
    /// Alerts wait this long for the answer to your request to be said.
    pub alert_wait_ms: u64,
    /// An alert that waited longer than this (seconds) is dropped unsaid.
    pub alert_max_age_secs: u64,
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
            claude_model: "sonnet".into(),
            claude_effort: "medium".into(),
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
            max_spoken_chars: 900,
            ack: ["Got it.", "Okay.", "Sure."].map(String::from).to_vec(),
            ack_delay_ms: 0,
            narrate: true,
            volume: 1.0,
            alert_cue: true,
            alert_wait_ms: 20_000,
            alert_max_age_secs: 90,
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
}

impl Default for Lan {
    fn default() -> Self {
        Self { enabled: false, port: 7843, bind: "auto".into(), allow_public: false }
    }
}

impl Default for Alerts {
    fn default() -> Self {
        Self { blocked: true, done: false, remote_poll_secs: 15, show_sessions: true, held_urgent: true }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
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
