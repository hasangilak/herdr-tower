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

use std::collections::VecDeque;
use std::num::NonZero;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rodio::Player;
use rodio::buffer::SamplesBuffer;

use super::lan::hub::Hub;
use super::queue::{Kind, Queue};
use super::state::{Shared, now_ms};
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
}

impl Mouth {
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
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.player.clear();
        if self.held.load(Ordering::SeqCst) == 0 {
            self.player.play();
        }
        self.hub.broadcast("hush", serde_json::json!({}));
        let _ = self.tx.send(Msg::Hush);
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
        let (tx, rx) = channel();
        // Messages are dropped unread.
        std::mem::forget(rx);
        let (player, _out) = Player::new();
        Mouth {
            tx,
            generation: Arc::new(AtomicU64::new(0)),
            hushes: Arc::new(AtomicU64::new(0)),
            held: Arc::new(AtomicU8::new(0)),
            player: Arc::new(player),
            hub,
        }
    }
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
}

/// Starts the speech thread. Kokoro loads (and on first use downloads,
/// about 330 MB) before this returns, so the first sentence is quick.
pub fn start(settings: Settings, shared: Shared, hub: Arc<Hub>) -> Result<Mouth> {
    let sink = rodio::DeviceSinkBuilder::open_default_sink().context("opening the speakers")?;
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
    let mouth = Mouth {
        tx,
        generation: Arc::new(AtomicU64::new(0)),
        hushes: Arc::new(AtomicU64::new(0)),
        held: Arc::new(AtomicU8::new(0)),
        player: player.clone(),
        hub: hub.clone(),
    };
    let speech = Speech {
        engine,
        q: Queue::new(settings.alert_max_age),
        settings,
        acks,
        player,
        bell,
        generation: mouth.generation.clone(),
        held: mouth.held.clone(),
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
    engine: loqui::Engine,
    q: Queue,
    settings: Settings,
    acks: Vec<(String, Vec<f32>)>,
    player: Arc<Player>,
    bell: Player,
    generation: Arc<AtomicU64>,
    held: Arc<AtomicU8>,
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
                let brain = self.shared.get().brain;
                // The answer is overdue, or isn't coming.
                if since.elapsed() > self.settings.alert_wait || brain == "blocked" || brain == "absent" {
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
            Msg::Chime(on) => self.bell.append(chime(on)),
            Msg::Ack { delay } => {
                self.acked = false;
                if !self.acks.is_empty() {
                    self.ack = Some((Instant::now() + delay, self.replies_started));
                }
            }
            Msg::Say { text, kind, interrupt } => {
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
                self.shared.update(|s| {
                    s.said = text.clone();
                    s.said_ms = now_ms();
                });
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
                self.shared.update(|s| {
                    s.said = text.clone();
                    s.said_ms = now_ms();
                });
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

    /// The acknowledgement, if nothing was said since the request.
    fn maybe_ack(&mut self) {
        let Some((at, before)) = self.ack else { return };
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
        self.acked = true;
        timing::mark("acknowledged");
    }

    fn speak_next(&mut self) {
        let alerts_may = self.awaiting.is_none();
        let agents = if self.q.has_alerts() && alerts_may { self.shared.get().agents } else { vec![] };
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
        match self.engine.speak(&req) {
            Ok(speech) => {
                eprintln!(
                    "[tts] {}{:.1}s of speech in {} ms",
                    if next.kind == Kind::Alert { "alert: " } else { "" },
                    speech.duration_secs,
                    t.elapsed().as_millis()
                );
                // Hushed while it was being made.
                if self.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                let label = if next.kind == Kind::Alert { "alert" } else { "reply" };
                self.append(next.kind, pcm(&speech.audio), &next.text, label);
                if next.kind == Kind::Reply {
                    timing::mark("first words playing");
                }
            }
            Err(e) => eprintln!("[tts] {:?}: {e}", next.text),
        }
    }

    /// Plays a sound: here, or on the phone that is the voice (the Mac
    /// stays silent). `text` is what it says; `label` what it is (`reply`,
    /// `alert`, `ack`, `cue`), for the phone.
    fn append(&mut self, kind: Kind, samples: Vec<f32>, text: &str, label: &str) {
        // A reply to what a phone sent: that phone only. Else a phone that is the voice, or here.
        let to_phone = if kind == Kind::Reply { self.turn_phone.clone() } else { None };
        if to_phone.is_some() || self.hub.voice() {
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
    use super::{cue, quick_start};

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
