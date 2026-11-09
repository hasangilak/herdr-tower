//! The mouth: Kokoro-82M (loqui, ONNX Runtime) speaking through rodio.
//!
//! Everything the tower says goes through one queue (see `queue`), spoken a
//! sentence at a time by this thread: the first sentence plays while the
//! next is made, so speech starts in a fraction of a second, and nothing is
//! said over anything else. Replies to the user go first; alerts wait while
//! the user waits for an answer, and get a soft bell first.
//!
//! The user comes first: pressing the talk key pauses speech at once
//! (`hold`), and starting a take drops what was playing and queued
//! (`hush`). What comes in while they talk is held, and afterwards only
//! what is still news is said. The listening chimes play on their own
//! player, so a pause never swallows them.
//!
//! Muted (`tower mute`), nothing makes a sound: no words, no bell, no
//! chimes. What it would have said still shows (`said`), and alerts are
//! counted, so unmuting can say how many were missed.

use std::collections::VecDeque;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rodio::Player;
use rodio::buffer::SamplesBuffer;

use super::lan::hub::Hub;
use super::queue::{Kind, Queue};
use super::state::{MISSED_MAX, Shared, lock, now_ms};
use super::timing;

pub enum Msg {
    Say {
        text: String,
        kind: Kind,
        interrupt: bool,
    },
    /// An alert about one agent: what to say, its address and status.
    Alert {
        text: String,
        about: Option<(String, String)>,
    },
    /// A short tone: listening started (`true`) or stopped.
    Chime(bool),
    /// A short spoken acknowledgement, after `delay` unless something was
    /// said by then.
    Ack {
        delay: Duration,
    },
    /// Everything queued is dropped (the player was cleared already).
    Hush,
    /// Muted (`true`: what was playing and queued goes, the player was
    /// cleared already) or unmuted. Sent in turn with everything else, so
    /// the speech thread applies it in order.
    Mute(bool),
    /// A take was sent (`true`: its answer is coming), or came to nothing.
    Expect(bool),
    /// A request reached the brain (`true`: the user took something back,
    /// "never mind": a short reply is not said).
    /// `.1`: the phone whose replies go to it alone (its route is `phone`).
    Asked(bool, Option<String>),
    /// The brain's turn is over: its answer is all in.
    Answered,
}

/// Who holds the speech: the talk key is down, or a take is running.
#[derive(Clone, Copy)]
pub enum Hold {
    Key = 1,
    Take = 2,
}

#[derive(Clone)]
pub struct Mouth {
    tx: Sender<Msg>,
    generation: Arc<AtomicU64>,
    hushes: Arc<AtomicU64>,
    held: Arc<AtomicU8>,
    player: Arc<Player>,
    /// Paired phones; while one is the voice, speech goes there instead.
    hub: Arc<Hub>,
    /// Muted (`tower mute`): nothing is said, here or on a phone.
    muted: Arc<AtomicBool>,
    /// Held while the mute changes: the file, the flag and the state
    /// change together.
    mute_lock: Arc<Mutex<()>>,
    /// The file that keeps the mute (see `mute_file`).
    mute_path: PathBuf,
    /// Everything plays here, never on a phone (`voice_on_mac`).
    on_mac: Arc<AtomicBool>,
    shared: Shared,
}

/// Where the mute is kept: the file is there while the tower is muted.
/// Tests keep theirs in a temporary folder.
pub fn mute_file() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir().join(format!("herder-tower-test-{}", std::process::id())).join("muted");
    }
    crate::config::state_dir().join("muted")
}

/// Whether the tower was left muted (the file is there), into the state.
fn muted_at_start(path: &Path, shared: &Shared) -> bool {
    let muted = path.exists();
    shared.update(|s| s.muted = muted);
    if muted {
        eprintln!("[speech] muted (tower mute off to hear it again)");
    }
    muted
}

/// Keeps the mute on disk. A failure is only a warning: the mute holds
/// all the same, until the voice restarts.
fn save_mute(path: &Path, on: bool) -> Result<()> {
    if on {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, "muted\n").with_context(|| format!("writing {}", path.display()))?;
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    Ok(())
}

impl Mouth {
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// Whether everything said plays on this Mac.
    pub fn on_mac(&self) -> bool {
        self.on_mac.load(Ordering::SeqCst)
    }

    /// Plays everything on this Mac (`on`), or lets the phones have it as
    /// they route it. From the next sound on: what a phone is playing ends
    /// there. (Kept in config.toml by the caller.)
    pub fn set_on_mac(&self, on: bool) {
        if self.on_mac.swap(on, Ordering::SeqCst) != on {
            eprintln!("[speech] {}", if on { "voice on this Mac: nothing goes to the phones" } else { "voice goes to the phones as they route it" });
        }
        self.shared.update(|s| s.voice_on_mac = on);
    }

    /// Mutes the tower (`on`) or lets it talk again. Muting stops what is
    /// being said, here and on the phones, and from then on nothing makes
    /// a sound: no replies, alerts, acknowledgements, bells or listening
    /// chimes. Text and chat go on as before (`said` still shows what it
    /// would have said), and alerts missed meanwhile are counted, for one
    /// short line on unmuting. It is kept on disk, so a restarted voice
    /// stays muted; if the file can't be written the mute holds all the
    /// same (a warning in the log), only not past a restart. Never fails:
    /// the `Result` is for callers that print errors.
    pub fn set_muted(&self, on: bool) -> Result<()> {
        let _held = lock(&self.mute_lock);
        self.apply_mute(on);
        Ok(())
    }

    /// Flips the mute; returns whether it is muted now.
    pub fn toggle_muted(&self) -> Result<bool> {
        let _held = lock(&self.mute_lock);
        let on = !self.muted();
        self.apply_mute(on);
        Ok(on)
    }

    /// `set_muted`, with `mute_lock` held.
    fn apply_mute(&self, on: bool) {
        if let Err(e) = save_mute(&self.mute_path, on) {
            eprintln!("[speech] warning: {e:#}: the {} holds until the voice restarts", if on { "mute" } else { "unmute" });
        }
        let was = self.muted.swap(on, Ordering::SeqCst);
        self.shared.update(|s| s.muted = on);
        if was == on {
            return;
        }
        eprintln!("[speech] {}", if on { "muted: nothing is said until unmuted" } else { "unmuted" });
        if on {
            // Quiet now, but not a hush: the narrator carries on after an
            // unmute in the middle of a turn.
            self.stop_audio();
            // And no phone plays a clip made before the mute, later.
            self.hub.drop_clips();
        }
        let _ = self.tx.send(Msg::Mute(on));
    }

    /// The brain's words to the user.
    pub fn say(&self, text: impl Into<String>, interrupt: bool) {
        self.say_as(text, Kind::Reply, interrupt);
    }

    pub fn say_as(&self, text: impl Into<String>, kind: Kind, interrupt: bool) {
        let _ = self.tx.send(Msg::Say { text: text.into(), kind, interrupt });
    }

    /// News nobody asked for. `about`: the agent's address and status, so
    /// it can be dropped if that changes before it is said.
    pub fn alert(&self, text: impl Into<String>, about: Option<(&str, &str)>) {
        let about = about.map(|(a, s)| (a.to_string(), s.to_string()));
        let _ = self.tx.send(Msg::Alert { text: text.into(), about });
    }

    pub fn chime(&self, on: bool) {
        let _ = self.tx.send(Msg::Chime(on));
    }

    /// "Got it.": a request went in.
    pub fn ack(&self, delay: Duration) {
        let _ = self.tx.send(Msg::Ack { delay });
    }

    /// A request reached the brain: its answer is next. `request`: what was asked.
    /// `phone`: it came from that paired phone, whose replies are heard there only.
    pub fn asked(&self, request: &str, phone: Option<String>) {
        let _ = self.tx.send(Msg::Asked(crate::text::is_dismissal(request), phone));
    }

    /// The brain's answer is all in.
    pub fn answered(&self) {
        let _ = self.tx.send(Msg::Answered);
    }

    /// How many hushes so far: a change means someone asked for quiet.
    pub fn hushes(&self) -> u64 {
        self.hushes.load(Ordering::SeqCst)
    }

    /// Stops speaking now and drops everything queued.
    pub fn hush(&self) {
        self.hushes.fetch_add(1, Ordering::SeqCst);
        self.stop_audio();
        let _ = self.tx.send(Msg::Hush);
    }

    /// Stops what is playing, here and on the phones. A sentence being
    /// made is let go (its generation is over).
    fn stop_audio(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.player.clear();
        if self.held.load(Ordering::SeqCst) == 0 {
            self.player.play();
        }
        self.hub.broadcast("hush", serde_json::json!({}));
    }

    /// The user is about to talk (`on`), or isn't any more: speech pauses at
    /// once, and nothing new starts until every hold is let go.
    pub fn hold(&self, who: Hold, on: bool) {
        let bit = who as u8;
        if on {
            if self.held.fetch_or(bit, Ordering::SeqCst) & bit == 0 {
                self.player.pause();
                self.hub.broadcast("pause", serde_json::json!({}));
            }
        } else if self.held.fetch_and(!bit, Ordering::SeqCst) == bit {
            self.player.play();
            self.hub.broadcast("resume", serde_json::json!({}));
        }
    }

    /// A take ended: `sent` if it goes to the transcriber (its answer will
    /// come first), and the hold it had is let go.
    pub fn take_ended(&self, sent: bool) {
        // Sent before the hold is let go: the speech thread reads the hold
        // before its messages, so it never starts an alert in between.
        if sent {
            let _ = self.tx.send(Msg::Expect(true));
        }
        self.hold(Hold::Take, false);
    }

    /// The take that was sent asked nothing after all.
    pub fn nothing_asked(&self) {
        let _ = self.tx.send(Msg::Expect(false));
    }
}

#[cfg(test)]
impl Mouth {
    /// A mouth that says nothing (no speakers, no Kokoro), for tests.
    pub fn silent(hub: Arc<Hub>) -> Mouth {
        let (mouth, rx) = Mouth::for_test(hub, test_mute_path());
        // Messages are dropped unread.
        std::mem::forget(rx);
        mouth
    }

    /// A mouth with its messages, and its own mute file at `mute_path`
    /// (read as at start-up).
    fn for_test(hub: Arc<Hub>, mute_path: PathBuf) -> (Mouth, Receiver<Msg>) {
        let (tx, rx) = channel();
        let (player, _out) = Player::new();
        let shared = Shared::new(super::state::State::new("ptt", "test", "test"));
        let muted = muted_at_start(&mute_path, &shared);
        let mouth = Mouth {
            tx,
            generation: Arc::new(AtomicU64::new(0)),
            hushes: Arc::new(AtomicU64::new(0)),
            held: Arc::new(AtomicU8::new(0)),
            player: Arc::new(player),
            hub,
            muted: Arc::new(AtomicBool::new(muted)),
            mute_lock: Arc::new(Mutex::new(())),
            mute_path,
            on_mac: Arc::new(AtomicBool::new(false)),
            shared,
        };
        (mouth, rx)
    }
}

/// A mute file of its own for each test mouth, under `mute_file`'s folder.
#[cfg(test)]
fn test_mute_path() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = mute_file().with_file_name(format!("mouth-{}", N.fetch_add(1, Ordering::SeqCst)));
    dir.join("muted")
}

pub struct Settings {
    pub voice: String,
    pub speed: f32,
    /// 0 (silent) to 1.
    pub volume: f32,
    /// Acknowledgements, taken in turn.
    pub acks: Vec<String>,
    /// A soft bell before an alert.
    pub cue: bool,
    /// How long alerts wait for the answer to the user's request.
    pub alert_wait: Duration,
    /// An alert older than this is dropped unsaid.
    pub alert_max_age: Duration,
    /// Everything plays on this Mac, never on a phone (at start).
    pub on_mac: bool,
}

/// Opens the default speakers. rodio asks for about 50 ms of buffer
/// (2048 frames at 44.1 kHz); some devices, such as Bluetooth headphones,
/// take at most 1024, so smaller buffers are tried before giving up.
fn open_speakers() -> Result<rodio::MixerDeviceSink> {
    let first = match rodio::DeviceSinkBuilder::open_default_sink() {
        Ok(sink) => return Ok(sink),
        Err(e) => e,
    };
    for frames in [1024, 512, 256] {
        let sink = rodio::DeviceSinkBuilder::from_default_device()
            .and_then(|b| b.with_buffer_size(cpal::BufferSize::Fixed(frames)).open_stream());
        if let Ok(sink) = sink {
            eprintln!("[speech] speakers open with a {frames}-frame buffer ({first})");
            return Ok(sink);
        }
    }
    Err(first).context("opening the speakers")
}

/// Starts the speech thread. Kokoro loads (and on first use downloads,
/// about 330 MB) before this returns, so the first sentence is quick.
pub fn start(settings: Settings, shared: Shared, hub: Arc<Hub>) -> Result<Mouth> {
    let sink = open_speakers()?;
    let player = Arc::new(Player::connect_new(sink.mixer()));
    let bell = Player::connect_new(sink.mixer());
    let volume = settings.volume.clamp(0.0, 1.0);
    player.set_volume(volume);
    bell.set_volume(volume);
    let engine = loqui::Engine::builder().preload(true).build().context("loading Kokoro")?;
    // Make sure the voice exists and is downloaded before anyone waits on it.
    engine
        .speak(&loqui::SpeakRequest { voice: settings.voice.clone(), speed: settings.speed, ..loqui::SpeakRequest::new("Ready.") })
        .with_context(|| format!("trying voice {:?}", settings.voice))?;
    let acks: Vec<(String, Vec<f32>)> = settings
        .acks
        .iter()
        .filter_map(|a| {
            let req = loqui::SpeakRequest {
                voice: settings.voice.clone(),
                speed: settings.speed,
                format: loqui::Format::Pcm,
                ..loqui::SpeakRequest::new(a.clone())
            };
            engine.speak(&req).map_err(|e| eprintln!("[tts] ack {a:?}: {e}")).ok().map(|s| (a.clone(), pcm(&s.audio)))
        })
        .collect();

    let (tx, rx) = channel();
    let mute_path = mute_file();
    let muted = muted_at_start(&mute_path, &shared);
    let mouth = Mouth {
        tx,
        generation: Arc::new(AtomicU64::new(0)),
        hushes: Arc::new(AtomicU64::new(0)),
        held: Arc::new(AtomicU8::new(0)),
        player: player.clone(),
        hub: hub.clone(),
        muted: Arc::new(AtomicBool::new(muted)),
        mute_lock: Arc::new(Mutex::new(())),
        mute_path,
        on_mac: Arc::new(AtomicBool::new(settings.on_mac)),
        shared: shared.clone(),
    };
    shared.update(|s| s.voice_on_mac = settings.on_mac);
    if settings.on_mac {
        eprintln!("[speech] voice on this Mac: nothing goes to the phones");
    }
    let speech = Speech {
        engine: Some(engine),
        q: Queue::new(settings.alert_max_age),
        settings,
        acks,
        player,
        bell,
        generation: mouth.generation.clone(),
        held: mouth.held.clone(),
        muted: mouth.muted.clone(),
        on_mac: mouth.on_mac.clone(),
        shared,
        playing: VecDeque::new(),
        hub,
        routed: VecDeque::new(),
        awaiting: None,
        ack: None,
        acked: false,
        dismissed: false,
        turn_phone: None,
        replies_started: 0,
        next_ack: 0,
        last_alert: None,
        published: None,
    };
    thread::Builder::new().name("tts".into()).spawn(move || {
        // The output device lives as long as this thread.
        let mut sink = sink;
        sink.log_on_drop(false);
        speech.run(rx);
    })?;
    Ok(mouth)
}

struct Speech {
    /// None in tests: nothing is made.
    engine: Option<loqui::Engine>,
    q: Queue,
    settings: Settings,
    acks: Vec<(String, Vec<f32>)>,
    player: Arc<Player>,
    bell: Player,
    generation: Arc<AtomicU64>,
    held: Arc<AtomicU8>,
    /// Muted (`tower mute`): what comes in is dropped unsaid.
    muted: Arc<AtomicBool>,
    /// Everything plays here, never on a phone.
    on_mac: Arc<AtomicBool>,
    shared: Shared,
    /// The kind of each sound in the player, oldest (playing) first.
    playing: VecDeque<Kind>,
    hub: Arc<Hub>,
    /// Sent to the phone instead: when each will have played, and its kind.
    routed: VecDeque<(Instant, Kind)>,
    /// The user waits for an answer since then: alerts wait too.
    awaiting: Option<Instant>,
    ack: Option<(Instant, u64)>,
    /// The canned acknowledgement was played for the current request: the
    /// brain's own "Okay, sure." is not said again (see `text::without_ack`).
    acked: bool,
    /// The current request takes something back ("never mind").
    dismissed: bool,
    /// The current request came from this phone (route `phone`): its
    /// replies and acknowledgement are heard there only.
    turn_phone: Option<String>,
    replies_started: u64,
    next_ack: usize,
    /// When the last alert sentence was queued: alerts in a row share a bell.
    last_alert: Option<Instant>,
    published: Option<(bool, bool)>,
}

impl Speech {
    fn run(mut self, rx: Receiver<Msg>) {
        let mut was_held = false;
        let mut key_alone: Option<Instant> = None;
        loop {
            // Read before the messages (see `Mouth::take_ended`).
            let bits = self.held.load(Ordering::SeqCst);
            let held = bits != 0;
            // Control held alone starts a take within half a second; alone
            // for long, its release was missed. Don't stay quiet for ever.
            if bits == Hold::Key as u8 {
                if key_alone.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(5) {
                    eprintln!("[speech] the talk key seems stuck: carrying on");
                    self.held.fetch_and(!(Hold::Key as u8), Ordering::SeqCst);
                    self.player.play();
                }
            } else {
                key_alone = None;
            }
            match rx.recv_timeout(Duration::from_millis(30)) {
                Ok(m) => {
                    self.message(m);
                    while let Ok(m) = rx.try_recv() {
                        self.message(m);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.publish();
            if held {
                if !was_held && self.q.len() > 0 {
                    eprintln!("[speech] holding {} while you talk", self.q.len());
                }
                was_held = true;
                continue;
            }
            if was_held {
                was_held = false;
                if self.q.len() > 0 {
                    eprintln!("[speech] you're done talking: {} waiting", self.q.len());
                }
            }
            if let Some(since) = self.awaiting {
                let gone = self.shared.with(|s| s.brain == "blocked" || s.brain == "absent");
                // The answer is overdue, or isn't coming.
                if since.elapsed() > self.settings.alert_wait || gone {
                    self.awaiting = None;
                }
            }
            self.maybe_ack();
            // One sentence ahead of the one playing is enough: what comes in
            // meanwhile can still go first.
            if self.out_len() > 1 {
                continue;
            }
            self.speak_next();
        }
    }

    fn message(&mut self, m: Msg) {
        match m {
            Msg::Chime(on) => {
                if !self.is_muted() {
                    self.bell.append(chime(on));
                }
            }
            Msg::Ack { delay } => {
                self.acked = false;
                if !self.acks.is_empty() && !self.is_muted() {
                    self.ack = Some((Instant::now() + delay, self.replies_started));
                }
            }
            Msg::Say { text, kind, interrupt } => {
                if self.is_muted() {
                    // Not heard, but it shows; news (a watch's report the brain tells) is kept as missed.
                    if !text.trim().is_empty() {
                        eprintln!("[speech] muted: not saying {text:?}");
                        self.show_said(&text);
                        if kind == Kind::Alert {
                            self.missed(&text);
                        }
                    }
                    return;
                }
                if kind == Kind::Reply && self.dismissed && !text.trim().is_empty() && crate::text::is_short_reply(&text) {
                    eprintln!("[speech] you said never mind: not saying {text:?}");
                    return;
                }
                let text = match (kind == Kind::Reply && self.acked && !text.trim().is_empty()).then(|| crate::text::without_ack(&text)) {
                    Some(None) => {
                        eprintln!("[speech] already acknowledged: not saying {text:?}");
                        return;
                    }
                    Some(Some(t)) => t,
                    None => text,
                };
                self.show_said(&text);
                let item = self.q.item(kind, &text, None);
                if interrupt {
                    self.generation.fetch_add(1, Ordering::SeqCst);
                    self.player.clear();
                    self.routed.clear();
                    self.hub.broadcast("hush", serde_json::json!({}));
                    if self.held.load(Ordering::SeqCst) == 0 {
                        self.player.play();
                    }
                    self.q.clear();
                    self.q.push_front(item);
                } else if !self.q.push(item) {
                    eprintln!("[speech] already waiting to be said: {text:?}");
                }
            }
            Msg::Alert { text, about } => {
                self.show_said(&text);
                if self.is_muted() {
                    // Kept, so unmuting can say what was missed.
                    eprintln!("[speech] muted: not saying the alert {text:?}");
                    self.missed(&text);
                    return;
                }
                let item = self.q.item(Kind::Alert, &text, about);
                if self.q.push(item) {
                    eprintln!("[speech] alert queued: {text:?} ({} waiting)", self.q.len());
                }
            }
            Msg::Hush => {
                self.routed.clear();
                let n = self.q.clear();
                if n > 0 {
                    eprintln!("[speech] hushed: dropped {n} waiting");
                }
                self.ack = None;
            }
            Msg::Mute(true) => {
                // Anything made or queued before the mute goes.
                self.player.clear();
                if self.held.load(Ordering::SeqCst) == 0 {
                    self.player.play();
                }
                self.routed.clear();
                let n = self.q.clear();
                if n > 0 {
                    eprintln!("[speech] muted: dropped {n} waiting");
                }
                self.ack = None;
            }
            Msg::Mute(false) => {
                // Muted again since: the missed stay for the next unmute.
                if self.is_muted() {
                    return;
                }
                let mut missed = vec![];
                self.shared.update(|s| missed = std::mem::take(&mut s.missed_while_muted));
                if let Some(line) = missed_line(missed.len()) {
                    eprintln!("[speech] missed while muted: {missed:?}");
                    self.show_said(&line);
                    let item = self.q.item(Kind::Alert, &line, None);
                    self.q.push(item);
                }
            }
            Msg::Expect(true) => {
                // A new question is on its way: what came in for the old
                // one during the take is no longer wanted.
                let n = self.q.asked();
                if n > 0 {
                    eprintln!("[speech] you asked something new: dropped {n} old replies");
                }
                self.awaiting = Some(Instant::now());
                self.acked = false;
                self.dismissed = false;
                // A take from the Mac's microphone.
                self.turn_phone = None;
            }
            Msg::Expect(false) => self.awaiting = None,
            Msg::Asked(dismissed, phone) => {
                self.acked = false;
                self.dismissed = dismissed;
                self.turn_phone = phone;
                let n = self.q.asked();
                if n > 0 {
                    eprintln!("[speech] new request: dropped {n} old replies");
                }
                self.awaiting = Some(Instant::now());
            }
            Msg::Answered => self.awaiting = None,
        }
    }

    /// What it is saying (or would, unmuted), for the orb.
    fn show_said(&self, text: &str) {
        self.shared.update(|s| {
            s.said = text.to_string();
            s.said_ms = now_ms();
        });
    }

    /// The acknowledgement, if nothing was said since the request.
    fn maybe_ack(&mut self) {
        let Some((at, before)) = self.ack else { return };
        let generation = self.generation.load(Ordering::SeqCst);
        if self.is_muted() {
            self.ack = None;
            return;
        }
        if Instant::now() < at {
            return;
        }
        self.ack = None;
        if self.replies_started != before {
            return;
        }
        let (text, clip) = self.acks[self.next_ack % self.acks.len()].clone();
        self.next_ack += 1;
        self.append(Kind::Reply, clip, &text, "ack");
        self.unless_stopped(generation);
        self.acked = true;
        timing::mark("acknowledged");
    }

    fn speak_next(&mut self) {
        if self.engine.is_none() {
            return;
        }
        let alerts_may = self.awaiting.is_none();
        let agents = if self.q.has_alerts() && alerts_may { self.shared.with(|s| s.agents.clone()) } else { vec![] };
        let status = |address: &str| agents.iter().find(|a| a.address == address).map(|a| a.status.clone());
        let Some(next) = self.q.next(alerts_may, status) else { return };
        let generation = self.generation.load(Ordering::SeqCst);
        if next.kind == Kind::Alert {
            let in_a_row = self.last_alert.is_some_and(|t| t.elapsed() < Duration::from_secs(4)) && self.out_len() > 0;
            if next.first && self.settings.cue && !in_a_row {
                self.append(Kind::Alert, cue(), "", "cue");
            }
            self.last_alert = Some(Instant::now());
        } else if next.first {
            self.replies_started += 1;
        }
        let req = loqui::SpeakRequest {
            voice: self.settings.voice.clone(),
            speed: self.settings.speed,
            format: loqui::Format::Pcm,
            ..loqui::SpeakRequest::new(next.text.clone())
        };
        let t = Instant::now();
        let Some(made) = self.engine.as_ref().map(|e| e.speak(&req)) else { return };
        match made {
            Ok(speech) => {
                eprintln!(
                    "[tts] {}{:.1}s of speech in {} ms",
                    if next.kind == Kind::Alert { "alert: " } else { "" },
                    speech.duration_secs,
                    t.elapsed().as_millis()
                );
                // Hushed while it was being made (its bell goes too).
                if self.generation.load(Ordering::SeqCst) != generation {
                    self.unless_stopped(generation);
                    return;
                }
                let label = if next.kind == Kind::Alert { "alert" } else { "reply" };
                self.append(next.kind, pcm(&speech.audio), &next.text, label);
                self.unless_stopped(generation);
                if next.kind == Kind::Reply {
                    timing::mark("first words playing");
                }
            }
            Err(e) => eprintln!("[tts] {:?}: {e}", next.text),
        }
    }

    /// News not heard while muted, kept (the newest few) for the unmute.
    fn missed(&self, text: &str) {
        let text = text.to_string();
        self.shared.update(|s| {
            s.missed_while_muted.push(text);
            let over = s.missed_while_muted.len().saturating_sub(MISSED_MAX);
            s.missed_while_muted.drain(..over);
        });
    }

    fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// A hush or a mute came from another thread between the last check and
    /// the append: what was just added goes too, so nothing plays after it.
    /// (They bump the generation before clearing the player, so one of the
    /// two always sees the other.)
    fn unless_stopped(&mut self, generation: u64) {
        if self.generation.load(Ordering::SeqCst) == generation && !self.is_muted() {
            return;
        }
        self.player.clear();
        if self.held.load(Ordering::SeqCst) == 0 {
            self.player.play();
        }
        if !self.routed.is_empty() {
            self.routed.clear();
            self.hub.broadcast("hush", serde_json::json!({}));
        }
        self.publish();
    }

    /// Plays a sound: here, or on the phone that is the voice (the Mac
    /// stays silent); always here with the voice on this Mac. `text` is what it says; `label` what it is (`reply`,
    /// `alert`, `ack`, `cue`), for the phone. Nothing, while muted (a
    /// sentence made just before the mute).
    fn append(&mut self, kind: Kind, samples: Vec<f32>, text: &str, label: &str) {
        if self.is_muted() {
            self.q.clear();
            return;
        }
        // The voice on this Mac: here, whatever the phones want. Else a reply
        // to what a phone sent: that phone only. Else a phone that is the voice, or here.
        let on_mac = self.on_mac.load(Ordering::SeqCst);
        let to_phone = if kind == Kind::Reply && !on_mac { self.turn_phone.clone() } else { None };
        if !on_mac && (to_phone.is_some() || self.hub.voice()) {
            let dur = Duration::from_secs_f32(samples.len() as f32 / RATE as f32);
            let start = self.routed.back().map(|(end, _)| *end).filter(|end| *end > Instant::now()).unwrap_or_else(Instant::now);
            self.hub.push_audio(label, text, &samples, RATE, to_phone.as_deref());
            self.routed.push_back((start + dur, kind));
        } else {
            self.player.append(SamplesBuffer::new(NonZero::new(1).unwrap(), NonZero::new(RATE).unwrap(), samples));
            self.playing.push_back(kind);
        }
        self.publish();
    }

    /// Sounds still to play, here and (by the clock) on the phone.
    fn out_len(&mut self) -> usize {
        let now = Instant::now();
        while self.routed.front().is_some_and(|(end, _)| *end <= now) {
            self.routed.pop_front();
        }
        self.player.len() + self.routed.len()
    }

    /// Speaking or not, and whether it's an alert (the orb shows which).
    fn publish(&mut self) {
        let left = self.player.len();
        while self.playing.len() > left {
            self.playing.pop_front();
        }
        let speaking = self.out_len() > 0;
        let front = if left > 0 { self.playing.front() } else { self.routed.front().map(|(_, k)| k) };
        let announcing = speaking && front == Some(&Kind::Alert);
        if self.published == Some((speaking, announcing)) {
            return;
        }
        self.published = Some((speaking, announcing));
        self.shared.update(|s| {
            s.speaking = speaking;
            s.announcing = announcing;
        });
    }
}

/// What unmuting says about the alerts missed meanwhile (none: nothing).
fn missed_line(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("While you were muted: 1 update. Ask me for it.".into()),
        n => Some(format!("While you were muted: {n} updates. Ask me for them.")),
    }
}

/// Kokoro's 16-bit PCM as samples.
fn pcm(audio: &[u8]) -> Vec<f32> {
    audio.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect()
}

/// A long first piece is cut at its first sentence end, or else its first
/// comma (semicolon, colon, dash) past a few words: Kokoro makes a short
/// piece in a fraction of the time, and the rest is made while it plays.
pub(super) fn quick_start(mut parts: Vec<String>) -> Vec<String> {
    const LONG: usize = 60;
    const FAR: usize = 120;
    let Some(first) = parts.first() else { return parts };
    let chars: Vec<char> = first.chars().collect();
    if chars.len() <= LONG {
        return parts;
    }
    let cut = (8..FAR.min(chars.len() - 1))
        .find(|&i| chars[i + 1] == ' ' && (matches!(chars[i], '.' | '!' | '?') || i >= 20 && matches!(chars[i], ',' | ';' | ':' | '—')));
    if let Some(i) = cut {
        let head: String = chars[..=i].iter().collect();
        let tail: String = chars[i + 1..].iter().collect();
        parts[0] = tail.trim().to_string();
        parts.insert(0, head.trim().to_string());
    }
    parts
}

/// Kokoro's sample rate.
const RATE: u32 = 24_000;

/// Two soft tones, rising when listening starts and falling when it stops.
fn chime(on: bool) -> SamplesBuffer {
    let (a, b) = if on { (660.0, 880.0) } else { (880.0, 587.0) };
    let mut samples = Vec::new();
    for (freq, ms) in [(a, 70u32), (b, 90u32)] {
        let n = RATE * ms / 1000;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let env = (std::f32::consts::PI * i as f32 / n as f32).sin();
            samples.push(0.18 * env * (2.0 * std::f32::consts::PI * freq * t).sin());
        }
    }
    SamplesBuffer::new(NonZero::new(1).unwrap(), NonZero::new(RATE).unwrap(), samples)
}

/// The bell before an alert: two struck notes, a fourth apart, ringing out
/// (unlike the chimes' short even tones), then a breath before the words.
fn cue() -> Vec<f32> {
    let mut samples = vec![0.0f32; (RATE * 420 / 1000) as usize];
    for (freq, start_ms) in [(1046.5f32, 0u32), (1396.9, 110)] {
        let start = (RATE * start_ms / 1000) as usize;
        for (i, s) in samples[start..].iter_mut().enumerate() {
            let t = i as f32 / RATE as f32;
            let attack = (t / 0.004).min(1.0);
            let env = attack * (-t / 0.09).exp();
            let tone = (2.0 * std::f32::consts::PI * freq * t).sin() + 0.25 * (2.0 * std::f32::consts::PI * 2.0 * freq * t).sin();
            *s += 0.11 * env * tone;
        }
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mouth and its speech thread's state, without speakers or Kokoro:
    /// messages are handed over by `deliver`.
    fn pair() -> (Mouth, Speech, Receiver<Msg>) {
        pair_at(test_mute_path())
    }

    fn pair_at(mute_path: PathBuf) -> (Mouth, Speech, Receiver<Msg>) {
        let hub = Arc::new(Hub::default());
        let (mouth, rx) = Mouth::for_test(hub.clone(), mute_path);
        let (bell, _out) = Player::new();
        let speech = Speech {
            engine: None,
            q: Queue::new(Duration::from_secs(60)),
            settings: Settings {
                voice: "test".into(),
                speed: 1.0,
                volume: 1.0,
                acks: vec!["Got it.".into()],
                cue: true,
                alert_wait: Duration::from_secs(10),
                alert_max_age: Duration::from_secs(60),
                on_mac: false,
            },
            acks: vec![("Got it.".into(), vec![0.0; 10])],
            player: mouth.player.clone(),
            bell,
            generation: mouth.generation.clone(),
            held: mouth.held.clone(),
            muted: mouth.muted.clone(),
            on_mac: mouth.on_mac.clone(),
            shared: mouth.shared.clone(),
            playing: VecDeque::new(),
            hub,
            routed: VecDeque::new(),
            awaiting: None,
            ack: None,
            acked: false,
            dismissed: false,
            turn_phone: None,
            replies_started: 0,
            next_ack: 0,
            last_alert: None,
            published: None,
        };
        (mouth, speech, rx)
    }

    /// The speech thread takes what was sent.
    fn deliver(speech: &mut Speech, rx: &Receiver<Msg>) {
        while let Ok(m) = rx.try_recv() {
            speech.message(m);
        }
    }

    #[test]
    fn nothing_makes_a_sound_while_muted() {
        let (mouth, mut speech, rx) = pair();
        mouth.set_muted(true).unwrap();
        mouth.say("The tests pass.", false);
        mouth.alert("Eyzflow finished.", Some(("t22", "done")));
        mouth.ack(Duration::ZERO);
        mouth.chime(true);
        deliver(&mut speech, &rx);
        assert_eq!(speech.q.len(), 0);
        assert!(speech.ack.is_none());
        assert_eq!(speech.bell.len(), 0);
        speech.maybe_ack();
        assert_eq!(speech.player.len(), 0);
        // Unmuted, the same are heard.
        mouth.set_muted(false).unwrap();
        mouth.say("The tests pass.", false);
        mouth.chime(true);
        mouth.ack(Duration::from_secs(60));
        deliver(&mut speech, &rx);
        assert_eq!(speech.q.len(), 2, "the missed line and the reply");
        assert_eq!(speech.bell.len(), 1);
        assert!(speech.ack.is_some());
    }

    #[test]
    fn what_it_would_say_still_shows_while_muted() {
        let (mouth, mut speech, rx) = pair();
        mouth.set_muted(true).unwrap();
        mouth.say("Two sessions are working.", false);
        deliver(&mut speech, &rx);
        let s = mouth.shared.get();
        assert_eq!(s.said, "Two sessions are working.");
        assert!(s.said_ms > 0);
        mouth.alert("The orb session needs you.", None);
        deliver(&mut speech, &rx);
        assert_eq!(mouth.shared.get().said, "The orb session needs you.");
    }

    #[test]
    fn news_the_brain_tells_counts_as_missed_too() {
        let (mouth, mut speech, rx) = pair();
        mouth.set_muted(true).unwrap();
        // `tower say --alert`, and a [watch] report the brain tells.
        mouth.say_as("The api session finished.", Kind::Alert, false);
        // A reply to the user is no missed update.
        mouth.say_as("Two sessions are working.", Kind::Reply, false);
        deliver(&mut speech, &rx);
        assert_eq!(mouth.shared.get().missed_while_muted, vec!["The api session finished.".to_string()]);
    }

    #[test]
    fn unmuting_says_how_many_updates_were_missed() {
        let (mouth, mut speech, rx) = pair();
        mouth.set_muted(true).unwrap();
        for i in 0..25 {
            mouth.alert(format!("Session {i} finished."), None);
        }
        deliver(&mut speech, &rx);
        let missed = mouth.shared.get().missed_while_muted;
        assert_eq!(missed.len(), MISSED_MAX);
        assert_eq!(missed.last().unwrap(), "Session 24 finished.");
        mouth.set_muted(false).unwrap();
        deliver(&mut speech, &rx);
        assert!(mouth.shared.get().missed_while_muted.is_empty());
        let said: Vec<String> = std::iter::from_fn(|| speech.q.next(true, |_| None)).map(|n| n.text).collect();
        assert_eq!(said.join(" "), "While you were muted: 20 updates. Ask me for them.");
        // Nothing missed: nothing said.
        mouth.set_muted(true).unwrap();
        mouth.set_muted(false).unwrap();
        deliver(&mut speech, &rx);
        assert_eq!(speech.q.len(), 0);
        assert_eq!(missed_line(1).as_deref(), Some("While you were muted: 1 update. Ask me for it."));
    }

    #[test]
    fn muting_drops_what_was_queued_and_is_not_a_hush() {
        let (mouth, mut speech, rx) = pair();
        mouth.say("One. Two. Three.", false);
        deliver(&mut speech, &rx);
        assert_eq!(speech.q.len(), 1);
        let generation = mouth.generation.load(Ordering::SeqCst);
        mouth.set_muted(true).unwrap();
        deliver(&mut speech, &rx);
        assert_eq!(speech.q.len(), 0);
        assert_eq!(mouth.generation.load(Ordering::SeqCst), generation + 1);
        // The narrator's count of hushes is left alone.
        assert_eq!(mouth.hushes(), 0);
        // Muting again changes nothing: no second stop.
        mouth.set_muted(true).unwrap();
        assert_eq!(mouth.generation.load(Ordering::SeqCst), generation + 1);
        assert!(rx.try_recv().is_err());
        // A sentence made before the mute, appended after it, doesn't stay.
        speech.unless_stopped(generation);
        assert_eq!(speech.player.len(), 0);
    }

    #[test]
    fn the_voice_on_this_mac_keeps_everything_off_the_phones() {
        let (mouth, mut speech, _rx) = pair();
        // A phone is the voice, and the user just asked from another phone.
        let (_, phone) = speech.hub.subscribe("d_voice", true, true);
        let (_, asker) = speech.hub.subscribe("d_asker", false, true);
        speech.turn_phone = Some("d_asker".into());
        let samples = || vec![0.0; 240];

        // Off (the default): as before, the phones have it.
        assert!(!mouth.on_mac() && !mouth.shared.get().voice_on_mac);
        speech.append(Kind::Alert, samples(), "News.", "alert");
        speech.append(Kind::Reply, samples(), "Done.", "reply");
        assert_eq!((speech.player.len(), speech.routed.len()), (0, 2));
        assert_eq!(phone.try_recv().unwrap().data["text"], "News.");
        assert_eq!(asker.try_recv().unwrap().data["text"], "Done.", "a phone hears its own reply");

        // On: everything plays here, the phones get nothing.
        mouth.set_on_mac(true);
        assert!(mouth.on_mac() && mouth.shared.get().voice_on_mac);
        speech.append(Kind::Alert, samples(), "More news.", "alert");
        speech.append(Kind::Reply, samples(), "Done again.", "reply");
        assert_eq!((speech.player.len(), speech.routed.len()), (2, 2));
        assert!(phone.try_recv().is_err() && asker.try_recv().is_err());

        // Off again: back to the phones.
        mouth.set_on_mac(false);
        speech.append(Kind::Alert, samples(), "Later news.", "alert");
        assert_eq!(phone.try_recv().unwrap().data["text"], "Later news.");
    }

    #[test]
    fn the_voice_on_this_mac_is_kept_in_config() {
        let text = "voice = \"af_bella\"\n\n[lan]\nenabled = true\n";
        let on = crate::phone::with_top_level(text, "voice_on_mac", "true");
        let cfg: crate::config::Config = toml::from_str(&on).unwrap();
        assert!(cfg.voice_on_mac && cfg.lan.enabled, "{on}");
        let off = crate::phone::with_top_level(&on, "voice_on_mac", "false");
        assert_eq!(off.matches("voice_on_mac").count(), 1, "{off}");
        assert!(!toml::from_str::<crate::config::Config>(&off).unwrap().voice_on_mac);
        assert!(!toml::from_str::<crate::config::Config>("").unwrap().voice_on_mac, "off unless set");
    }

    #[test]
    fn toggle_flips_and_is_kept_on_disk() {
        let (mouth, _speech, _rx) = pair();
        let file = mouth.mute_path.clone();
        assert!(!mouth.muted());
        assert!(mouth.toggle_muted().unwrap());
        assert!(mouth.muted() && mouth.shared.get().muted && file.exists());
        assert!(!mouth.toggle_muted().unwrap());
        assert!(!mouth.muted() && !mouth.shared.get().muted && !file.exists());
        // Unmuting with no file is fine.
        mouth.set_muted(false).unwrap();
    }

    #[test]
    fn a_mute_that_cant_be_saved_still_holds() {
        // Its folder is a file: the mute file can't be written.
        let dir = test_mute_path().with_file_name("not-a-folder");
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(&dir, "").unwrap();
        let (mouth, _speech, _rx) = pair_at(dir.join("muted"));
        mouth.set_muted(true).unwrap();
        assert!(mouth.muted() && mouth.shared.get().muted);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn a_restarted_voice_stays_muted() {
        let path = test_mute_path();
        let (mouth, _speech, _rx) = pair_at(path.clone());
        assert!(!mouth.muted());
        mouth.set_muted(true).unwrap();
        // Started again with the file there.
        let (again, _speech, _rx) = pair_at(path.clone());
        assert!(again.muted() && again.shared.get().muted);
        again.set_muted(false).unwrap();
        let (third, _speech, _rx) = pair_at(path);
        assert!(!third.muted() && !third.shared.get().muted);
    }

    #[test]
    fn toggles_at_once_agree_in_the_end() {
        let (mouth, _speech, _rx) = pair();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let m = mouth.clone();
                std::thread::spawn(move || {
                    for j in 0..25 {
                        if (i + j) % 3 == 0 {
                            m.set_muted(j % 2 == 0).unwrap();
                        } else {
                            m.toggle_muted().unwrap();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let on = mouth.muted();
        assert_eq!(mouth.shared.get().muted, on);
        assert_eq!(mouth.mute_path.exists(), on);
        // 200 plain toggles from an even start end where they began.
        let before = mouth.muted();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let m = mouth.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        m.toggle_muted().unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(mouth.muted(), before);
        assert_eq!(mouth.shared.get().muted, before);
        assert_eq!(mouth.mute_path.exists(), before);
    }

    #[test]
    fn tests_never_touch_the_real_mute_file() {
        assert!(mute_file().starts_with(std::env::temp_dir()));
    }

    #[test]
    fn cuts_a_long_first_sentence() {
        let long = "The orb rust session is matching the orb to Herdr's look, reading the drawing code and applying the dot colours.";
        let parts = quick_start(vec![long.into(), "Next.".into()]);
        assert_eq!(parts[0], "The orb rust session is matching the orb to Herdr's look,");
        assert_eq!(parts[1], "reading the drawing code and applying the dot colours.");
        assert_eq!(parts[2], "Next.");
        let merged = "Nothing is blocked. The orb rust session and this voice speed session are both still working, and the herder tower session has finished.";
        assert_eq!(quick_start(vec![merged.into()])[0], "Nothing is blocked.");
        let short = vec!["Nothing's waiting on you.".to_string()];
        assert_eq!(quick_start(short.clone()), short);
    }

    #[test]
    fn the_cue_is_short_and_soft() {
        let c = cue();
        assert!(c.len() < 24_000 / 2);
        let peak = c.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.05 && peak < 0.3, "{peak}");
        // It has rung out before the words.
        assert!(c[c.len() - 200..].iter().all(|s| s.abs() < 0.01));
    }
}
