//! The ears: the microphone (cpal), resampled to 16 kHz mono, cut into
//! utterances by an energy detector that follows the room's noise floor.
//!
//! Press-to-talk takes one utterance per press (it ends on a pause, or on a
//! second press). Hands-free keeps the microphone open and hands every
//! utterance to the transcriber, which listens for the wake word.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::state::Shared;
use super::tts::{Hold, Mouth};
use crate::config::Vad;

pub const RATE: u32 = 16_000;
const FRAME: usize = 480; // 30 ms
const FRAME_MS: u32 = 30;
const PREROLL_FRAMES: usize = 12;
/// Twenty minutes of holding the talk key.
const HELD_MAX_FRAMES: u32 = 20 * 60 * 1000 / FRAME_MS;

pub enum Ctl {
    Mode(String),
    Talk(String),
    Cancel,
}

pub enum Source {
    /// A press-to-talk take: meant for the tower.
    Ptt,
    /// Hands-free: for the tower only after the wake word. `over_speech`:
    /// the tower was talking, so it may be its own echo.
    HandsFree { over_speech: bool },
}

pub struct Utterance {
    pub samples: Vec<f32>,
    pub source: Source,
}

enum Event {
    Audio(Vec<f32>),
    Ctl(Ctl),
    StreamError(String),
}

#[derive(Clone)]
pub struct Ears {
    tx: Sender<Event>,
}

impl Ears {
    pub fn control(&self, c: Ctl) {
        let _ = self.tx.send(Event::Ctl(c));
    }
}

pub fn start(mode: String, device: String, vad: Vad, shared: Shared, mouth: Mouth, out: Sender<Utterance>) -> Result<Ears> {
    let (tx, rx) = channel();
    let ears = Ears { tx: tx.clone() };
    thread::Builder::new().name("mic".into()).spawn(move || {
        let mut mic = Mic {
            mode,
            device,
            vad,
            shared,
            mouth,
            out,
            tx,
            stream: None,
            resampler: None,
            pending: Vec::new(),
            floor: 0.003,
            preroll: VecDeque::new(),
            seg: None,
            take: None,
            speech_ended: None,
        };
        mic.apply_mode();
        mic.run(rx);
    })?;
    Ok(ears)
}

struct Segment {
    samples: Vec<f32>,
    speech_frames: u32,
    silence_run: u32,
    started: Instant,
    over_speech: bool,
}

struct Take {
    samples: Vec<f32>,
    /// 150 ms of speech so far: a key click is not a request.
    heard_speech: bool,
    speech_frames: u32,
    /// Started by holding the talk key: it lasts until the key is let go,
    /// pauses or not.
    held: bool,
    silence_run: u32,
    started: Instant,
}

struct Mic {
    mode: String,
    device: String,
    vad: Vad,
    shared: Shared,
    mouth: Mouth,
    out: Sender<Utterance>,
    tx: Sender<Event>,
    stream: Option<cpal::Stream>,
    resampler: Option<Resampler>,
    pending: Vec<f32>,
    floor: f32,
    preroll: VecDeque<Vec<f32>>,
    seg: Option<Segment>,
    take: Option<Take>,
    /// When the tower last stopped talking: its echo can trail a little.
    speech_ended: Option<Instant>,
}

impl Mic {
    fn run(&mut self, rx: Receiver<Event>) {
        let mut was_speaking = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Audio(chunk)) => self.audio(&chunk),
                Ok(Event::Ctl(Ctl::Mode(m))) => {
                    self.mode = m;
                    self.shared.update(|s| s.mode = self.mode.clone());
                    self.apply_mode();
                }
                Ok(Event::Ctl(Ctl::Talk(action))) => self.talk(&action),
                Ok(Event::Ctl(Ctl::Cancel)) => {
                    if let Some(t) = self.take.take() {
                        log_take("cancelled", &t.samples, self.floor);
                        self.mouth.chime(false);
                        self.mouth.take_ended(false);
                    }
                    self.shared.update(|s| s.armed = false);
                    self.after_take();
                }
                Ok(Event::StreamError(e)) => {
                    eprintln!("[mic] stream error: {e}");
                    self.shared.update(|s| s.error = format!("microphone: {e}"));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let speaking = self.shared.get().speaking;
            if was_speaking && !speaking {
                self.speech_ended = Some(Instant::now());
            }
            was_speaking = speaking;
            // A take that never heard anything gives up.
            if let Some(t) = &self.take
                && !t.heard_speech
                && !t.held
                && t.started.elapsed() > Duration::from_millis(self.vad.ptt_no_speech_ms as u64)
            {
                self.take = None;
                self.mouth.chime(false);
                self.mouth.take_ended(false);
                self.shared.update(|s| s.error = "I didn't hear anything.".into());
                self.after_take();
            }
        }
    }

    fn talk(&mut self, action: &str) {
        let recording = self.take.is_some();
        let held = action == "hold";
        let start = match action {
            "start" | "hold" => true,
            "stop" => false,
            _ => !recording,
        };
        if start && !recording {
            // Talking over the tower stops it, and what was waiting to be
            // said goes too. What comes in during the take waits for its end.
            self.mouth.hold(Hold::Take, true);
            self.mouth.hush();
            if let Err(e) = self.open() {
                self.mouth.hold(Hold::Take, false);
                self.shared.update(|s| s.error = format!("microphone: {e:#}"));
                return;
            }
            self.seg = None;
            self.take = Some(Take {
                samples: self.preroll.iter().flatten().copied().collect(),
                heard_speech: false,
                speech_frames: 0,
                held,
                silence_run: 0,
                started: Instant::now(),
            });
            self.mouth.chime(true);
            self.shared.update(|s| {
                s.mic = "recording".into();
                s.error.clear();
            });
        } else if held && recording {
            if let Some(t) = self.take.as_mut() {
                t.held = true;
            }
        } else if !start && recording {
            self.finish_take();
        }
    }

    fn finish_take(&mut self) {
        let Some(take) = self.take.take() else { return };
        log_take(if take.heard_speech { "sent" } else { "silent" }, &take.samples, self.floor);
        self.mouth.chime(false);
        // Its answer comes before anything held meanwhile.
        self.mouth.take_ended(take.heard_speech);
        if take.heard_speech {
            super::timing::start("take ended");
            let _ = self.out.send(Utterance { samples: take.samples, source: Source::Ptt });
        } else {
            self.shared.update(|s| s.error = "I didn't hear anything.".into());
        }
        self.after_take();
    }

    fn after_take(&mut self) {
        if self.mode == "handsfree" {
            self.shared.update(|s| s.mic = "waiting".into());
        } else {
            self.close();
        }
    }

    fn apply_mode(&mut self) {
        match self.mode.as_str() {
            "handsfree" => match self.open() {
                Ok(()) => self.shared.update(|s| {
                    if s.mic == "closed" {
                        s.mic = "waiting".into();
                    }
                }),
                Err(e) => self.shared.update(|s| s.error = format!("microphone: {e:#}")),
            },
            _ => {
                if self.take.is_none() {
                    self.close();
                }
            }
        }
    }

    fn open(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let host = cpal::default_host();
        let device = if self.device.is_empty() {
            host.default_input_device().ok_or_else(|| anyhow!("no microphone"))?
        } else {
            host.input_devices()?
                .find(|d| d.description().is_ok_and(|n| n.name() == self.device))
                .ok_or_else(|| anyhow!("no microphone called {:?}", self.device))?
        };
        let config = device.default_input_config().context("asking the microphone for its format")?;
        let channels = config.channels() as usize;
        let rate = config.sample_rate();
        self.resampler = Some(Resampler::new(rate, RATE));
        let tx = self.tx.clone();
        let err_tx = self.tx.clone();
        let err = move |e: cpal::Error| {
            let _ = err_tx.send(Event::StreamError(e.to_string()));
        };
        macro_rules! build {
            ($t:ty, $conv:expr) => {
                device.build_input_stream(
                    config.clone().into(),
                    move |data: &[$t], _: &cpal::InputCallbackInfo| {
                        let mono: Vec<f32> =
                            data.chunks(channels).map(|f| f.iter().map(|&s| $conv(s)).sum::<f32>() / channels as f32).collect();
                        let _ = tx.send(Event::Audio(mono));
                    },
                    err,
                    None,
                )
            };
        }
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => build!(f32, |s: f32| s),
            cpal::SampleFormat::I16 => build!(i16, |s: i16| s as f32 / 32768.0),
            cpal::SampleFormat::I32 => build!(i32, |s: i32| s as f32 / 2147483648.0),
            other => return Err(anyhow!("unsupported microphone format {other}")),
        }
        .context("opening the microphone")?;
        stream.play().context("starting the microphone")?;
        let name = device.description().map(|d| d.name().to_string()).unwrap_or_default();
        eprintln!("[mic] open: {name} at {rate} Hz, {channels} channel(s)");
        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) {
        if self.stream.take().is_some() {
            eprintln!("[mic] closed");
        }
        self.seg = None;
        self.pending.clear();
        self.preroll.clear();
        self.shared.update(|s| s.mic = "closed".into());
    }

    fn audio(&mut self, chunk: &[f32]) {
        let Some(r) = self.resampler.as_mut() else { return };
        r.process(chunk, &mut self.pending);
        while self.pending.len() >= FRAME {
            let frame: Vec<f32> = self.pending.drain(..FRAME).collect();
            self.frame(frame);
        }
    }

    fn frame(&mut self, frame: Vec<f32>) {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32).sqrt();
        let threshold = (self.floor * self.vad.ratio).max(self.vad.min_rms);
        let speech = rms > threshold;
        let max_frames = self.vad.max_utterance_ms / FRAME_MS;

        if let Some(take) = self.take.as_mut() {
            take.samples.extend_from_slice(&frame);
            if speech {
                take.speech_frames += 1;
                take.heard_speech |= take.speech_frames >= 5;
                take.silence_run = 0;
            } else {
                take.silence_run += 1;
                self.floor = 0.97 * self.floor + 0.03 * rms.max(1e-4);
            }
            // A held take ends when the key is let go; the cap is a safety net.
            let cap = if take.held { HELD_MAX_FRAMES } else { max_frames };
            let long = take.samples.len() as u32 / FRAME as u32 >= cap;
            let paused = !take.held && take.heard_speech && take.silence_run * FRAME_MS >= self.vad.ptt_silence_ms;
            if long || paused {
                self.finish_take();
            }
            return;
        }

        if self.mode != "handsfree" {
            return;
        }

        match self.seg.as_mut() {
            None => {
                if speech {
                    let over_speech =
                        self.shared.get().speaking || self.speech_ended.is_some_and(|t| t.elapsed() < Duration::from_millis(700));
                    let mut samples: Vec<f32> = self.preroll.iter().flatten().copied().collect();
                    samples.extend_from_slice(&frame);
                    self.seg = Some(Segment { samples, speech_frames: 1, silence_run: 0, started: Instant::now(), over_speech });
                    self.shared.update(|s| s.mic = "hearing".into());
                } else {
                    self.floor = 0.97 * self.floor + 0.03 * rms.max(1e-4);
                }
            }
            Some(seg) => {
                seg.samples.extend_from_slice(&frame);
                if speech {
                    seg.speech_frames += 1;
                    seg.silence_run = 0;
                } else {
                    seg.silence_run += 1;
                }
                let ended = seg.silence_run * FRAME_MS >= self.vad.handsfree_silence_ms;
                let long = seg.started.elapsed() > Duration::from_millis(self.vad.max_utterance_ms as u64);
                // Over the tower's own voice the segment never pauses: listen
                // in short slices, so a "stop" is heard while it talks.
                let slice = seg.over_speech && seg.samples.len() >= 2 * RATE as usize;
                if ended || long || slice {
                    let seg = self.seg.take().unwrap();
                    // Clicks and coughs are shorter than a word.
                    if seg.speech_frames >= 8 {
                        let _ =
                            self.out.send(Utterance { samples: seg.samples, source: Source::HandsFree { over_speech: seg.over_speech } });
                    }
                    self.shared.update(|s| s.mic = "waiting".into());
                }
            }
        }
        self.preroll.push_back(frame);
        while self.preroll.len() > PREROLL_FRAMES {
            self.preroll.pop_front();
        }
    }
}

fn log_take(what: &str, samples: &[f32], floor: f32) {
    let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
    eprintln!("[mic] take {what}: {:.1}s, peak {peak:.4}, rms {rms:.4}, noise floor {floor:.4}", samples.len() as f32 / RATE as f32);
}

/// Resamples a whole recording at once.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let mut r = Resampler::new(from, to);
    let mut out = Vec::with_capacity(input.len() * to as usize / from as usize + 1);
    r.process(input, &mut out);
    out
}

/// Converts any rate to 16 kHz: a box filter against aliasing, then linear
/// interpolation. Plenty for speech recognition.
struct Resampler {
    step: f64,
    pos: f64,
    taps: usize,
    hist: VecDeque<f32>,
    filtered: Vec<f32>,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        let step = from as f64 / to as f64;
        let taps = step.floor().max(1.0) as usize;
        Self { step, pos: 0.0, taps, hist: VecDeque::new(), filtered: Vec::new() }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for &s in input {
            self.hist.push_back(s);
            if self.hist.len() > self.taps {
                self.hist.pop_front();
            }
            self.filtered.push(self.hist.iter().sum::<f32>() / self.hist.len() as f32);
        }
        while self.pos + 1.0 < self.filtered.len() as f64 {
            let i = self.pos.floor() as usize;
            let frac = (self.pos - i as f64) as f32;
            out.push(self.filtered[i] * (1.0 - frac) + self.filtered[i + 1] * frac);
            self.pos += self.step;
        }
        let used = (self.pos.floor() as usize).min(self.filtered.len());
        self.filtered.drain(..used);
        self.pos -= used as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_48k_to_16k() {
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        for _ in 0..10 {
            r.process(&vec![0.5; 4800], &mut out);
        }
        assert!((out.len() as i64 - 16_000).abs() < 4, "{}", out.len());
        assert!((out[100] - 0.5).abs() < 1e-5);
    }
}
