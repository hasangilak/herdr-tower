//! Understanding: whisper.cpp (whisper-rs, Metal) turns utterances into
//! text, then decides what they are for: the brain, a hush, or nothing.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::mic::{RATE, Source, Utterance};
use super::state::{Shared, now_ms};
use super::timing;
use super::tts::Mouth;
use crate::brain;
use crate::text;

pub struct Settings {
    pub model: std::path::PathBuf,
    pub language: String,
    pub wake_words: Vec<String>,
    pub stop_words: Vec<String>,
    /// Acknowledge a request after this long (None: never).
    pub ack: Option<Duration>,
}

pub fn start(settings: Settings, rx: Receiver<Utterance>, shared: Shared, mouth: Mouth) -> Result<()> {
    let ctx = load(&settings.model)?;
    thread::Builder::new().name("stt".into()).spawn(move || {
        let mut state = match ctx.create_state() {
            Ok(s) => s,
            Err(e) => {
                // Not silently: every take is turned down, out loud.
                let msg = format!("speech recognition didn't start: {e}");
                eprintln!("[stt] {msg}");
                shared.update(|s| s.error = msg.clone());
                refuse_all(&rx, &shared, &mouth, &msg);
                return;
            }
        };
        let mut armed_until: Option<Instant> = None;
        let mut queue: VecDeque<Utterance> = VecDeque::new();
        loop {
            if queue.is_empty() {
                match rx.recv() {
                    Ok(u) => queue.push_back(u),
                    Err(_) => return,
                }
            }
            queue.extend(rx.try_iter());
            let Some(utt) = next(&mut queue) else { continue };
            if matches!(utt.source, Source::Partial) {
                partial(&mut state, &utt.samples, &settings.language, &shared);
                continue;
            }
            // A press-to-talk take that asks nothing: alerts needn't wait.
            let ptt = matches!(utt.source, Source::Ptt);
            let secs = utt.samples.len() as f32 / RATE as f32;
            shared.update(|s| s.transcribing = true);
            let started = Instant::now();
            let heard = transcribe(&mut state, &utt.samples, &settings.language, &vocabulary(&shared));
            // The take's final text replaces its last partial (sending it clears that).
            shared.update(|s| {
                s.transcribing = false;
                s.hearing = if ptt { heard.as_deref().map(|h| h.trim().to_string()).unwrap_or_default() } else { String::new() };
            });
            let heard = match heard {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[stt] {e:#}");
                    if ptt {
                        shared.update(|s| s.hearing.clear());
                        mouth.nothing_asked();
                    }
                    continue;
                }
            };
            eprintln!("[stt] {secs:.1}s in {:.0} ms: {heard:?}", started.elapsed().as_millis());
            timing::mark("transcribed");
            if text::is_hallucination(&heard) {
                if ptt {
                    shared.update(|s| s.hearing.clear());
                    mouth.nothing_asked();
                }
                continue;
            }
            let norm = text::normalize(&heard);
            let is_stop = is_only_stop(&norm, &settings.stop_words);

            match utt.source {
                Source::Partial => {}
                Source::Ptt => {
                    if is_stop {
                        shared.update(|s| s.hearing.clear());
                        mouth.nothing_asked();
                        mouth.hush();
                        continue;
                    }
                    // The panel holds a draft (an image, typed text): this
                    // goes into it, and Enter sends them together.
                    if crate::draft::held() {
                        eprintln!("[stt] into the panel's draft");
                        shared.update(|s| {
                            s.hearing.clear();
                            s.dictated = heard.trim().to_string();
                            s.dictated_ms = now_ms();
                        });
                        mouth.nothing_asked();
                        continue;
                    }
                    let _ = deliver(&shared, &mouth, heard.trim(), settings.ack, "voice");
                }
                Source::Phone { reply, attachments, markers, send, phone, device } => {
                    if !send {
                        // Dictation: the text goes back, nothing to the tower.
                        let _ = reply.send(heard.trim().to_string());
                        continue;
                    }
                    if is_stop && attachments.is_empty() {
                        mouth.hush();
                        continue;
                    }
                    let message = crate::history::compose(&super::lan::with_markers(heard.trim(), &markers), &attachments);
                    if deliver_from(&shared, &mouth, &message, settings.ack, phone, &format!("phone:{device}")).is_ok() {
                        let _ = reply.send(heard.trim().to_string());
                    }
                }
                Source::HandsFree { over_speech } => {
                    if over_speech || shared.speaking() {
                        // Probably the tower hearing itself; only a stop word
                        // it isn't saying itself counts.
                        let said = shared.with(|s| text::normalize(&s.said));
                        if is_stop || heard_stop(&norm, &said, &settings.stop_words) {
                            mouth.hush();
                        }
                        continue;
                    }
                    if is_stop {
                        mouth.hush();
                        continue;
                    }
                    if armed_until.is_some_and(|t| Instant::now() < t) {
                        armed_until = None;
                        shared.update(|s| s.armed = false);
                        let _ = deliver(&shared, &mouth, heard.trim(), settings.ack, "voice");
                        continue;
                    }
                    if let Some(rest) = after_wake_word(&heard, &settings.wake_words) {
                        if rest.split_whitespace().count() == 0 {
                            // "Tower." alone: listen for the request.
                            armed_until = Some(Instant::now() + Duration::from_secs(8));
                            shared.update(|s| s.armed = true);
                            mouth.chime(true);
                        } else {
                            let _ = deliver(&shared, &mouth, &rest, settings.ack, "voice");
                        }
                    }
                }
            }
            if armed_until.is_some_and(|t| Instant::now() >= t) {
                armed_until = None;
                shared.update(|s| s.armed = false);
            }
        }
    })?;
    Ok(())
}

/// The transcriber couldn't start: each take gets the error, shown and (for
/// one asked of the tower) said, instead of vanishing.
fn refuse_all(rx: &Receiver<Utterance>, shared: &Shared, mouth: &Mouth, msg: &str) {
    for utt in rx.iter() {
        refuse(utt, shared, mouth, msg);
    }
}

fn refuse(utt: Utterance, shared: &Shared, mouth: &Mouth, msg: &str) {
    match utt.source {
        Source::Partial => super::mic::partial_done(),
        Source::Ptt => {
            shared.update(|s| {
                s.hearing.clear();
                s.error = msg.to_string();
            });
            mouth.nothing_asked();
            mouth.say(REFUSED, false);
        }
        // Nothing heard: the phone says so.
        Source::Phone { reply, .. } => {
            shared.update(|s| s.error = msg.to_string());
            let _ = reply.send(String::new());
        }
        // Not asked of the tower (no wake word heard): the error shows.
        Source::HandsFree { .. } => shared.update(|s| s.error = msg.to_string()),
    }
}

/// Said for a take while speech recognition is down.
const REFUSED: &str = "I can't understand speech right now: speech recognition didn't start. Type to me instead.";

/// The next utterance to transcribe. A partial is only worth it while
/// nothing newer waits behind it (a newer partial, or the take's end).
fn next(queue: &mut VecDeque<Utterance>) -> Option<Utterance> {
    while queue.len() > 1 && matches!(queue.front().map(|u| &u.source), Some(Source::Partial)) {
        queue.pop_front();
        super::mic::partial_done();
    }
    queue.pop_front()
}

/// A press-to-talk take so far, for the panel to show while you talk.
fn partial(state: &mut whisper_rs::WhisperState, samples: &[f32], language: &str, shared: &Shared) {
    let started = Instant::now();
    let heard = transcribe(state, samples, language, &vocabulary(shared));
    super::mic::partial_done();
    let Ok(heard) = heard else { return };
    if heard.is_empty() || text::is_hallucination(&heard) {
        return;
    }
    eprintln!("[stt] partial {:.1}s in {:.0} ms: {heard:?}", samples.len() as f32 / RATE as f32, started.elapsed().as_millis());
    // Only while the take still runs: a late partial mustn't undo the final text.
    shared.update(|s| {
        if s.mic == "recording" && !s.transcribing {
            s.hearing = heard;
        }
    });
}

fn load(model: &Path) -> Result<WhisperContext> {
    whisper_rs::install_logging_hooks();
    let mut params = WhisperContextParameters::default();
    params.use_gpu(true);
    let path = model.to_str().context("model path is not UTF-8")?;
    let t = Instant::now();
    let ctx = WhisperContext::new_with_params(path, params).with_context(|| format!("loading Whisper from {}", model.display()))?;
    eprintln!("[stt] {} loaded in {} ms", model.display(), t.elapsed().as_millis());
    Ok(ctx)
}

fn transcribe(state: &mut whisper_rs::WhisperState, samples: &[f32], language: &str, prompt: &str) -> Result<String> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(if language == "auto" { None } else { Some(language) });
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_no_context(true);
    params.set_single_segment(false);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_no_speech_thold(0.6);
    params.set_n_threads(4);
    if !prompt.is_empty() {
        params.set_initial_prompt(prompt);
    }
    // Whisper wants at least a second; pad short takes with silence.
    let mut padded;
    let audio = if samples.len() < RATE as usize + RATE as usize / 4 {
        padded = samples.to_vec();
        padded.resize(RATE as usize + RATE as usize / 4, 0.0);
        &padded[..]
    } else {
        samples
    };
    state.full(params, audio).context("transcribing")?;
    let mut out = String::new();
    for seg in state.as_iter() {
        if seg.no_speech_probability() > 0.8 {
            continue;
        }
        out.push_str(&seg.to_str_lossy().unwrap_or_default());
    }
    Ok(out.trim().to_string())
}

/// Names Whisper should expect: the tower's words and the agents' names.
fn vocabulary(shared: &Shared) -> String {
    let mut names: Vec<String> = shared.with(|s| s.agents.iter().map(|a| a.name.clone()).collect());
    names.sort();
    names.dedup();
    names.truncate(30);
    format!("Tower, Claude, Codex, Herdr, terminal, agent. {}.", names.join(", "))
}

/// Hands what was spoken to the brain and acknowledges it out loud.
/// `source`: `voice` (this Mac's microphone), for the chat.
pub fn deliver(shared: &Shared, mouth: &Mouth, text: &str, ack: Option<Duration>, source: &str) -> anyhow::Result<()> {
    deliver_from(shared, mouth, text, ack, None, source)
}

/// `deliver` for what was spoken into a paired phone whose replies are
/// heard on the phone only (`phone`: its device id; None: heard as usual).
pub fn deliver_from(shared: &Shared, mouth: &Mouth, text: &str, ack: Option<Duration>, phone: Option<String>, source: &str) -> anyhow::Result<()> {
    deliver_to(shared, mouth, text, ack, phone, source, None, true)
}

/// `deliver_from`, for a request made in a space's view (`space`): it goes
/// to that space's own brain when it has one that can take it, else to the
/// main brain with the space named (see `super::brains::ask`). `spoken`:
/// it was said, not typed: only then is its answer said out loud.
#[allow(clippy::too_many_arguments)]
pub fn deliver_to(shared: &Shared, mouth: &Mouth, text: &str, ack: Option<Duration>, phone: Option<String>, source: &str, space: Option<&str>, spoken: bool) -> anyhow::Result<()> {
    shared.update(|s| {
        s.heard = text.to_string();
        s.heard_ms = now_ms();
        s.hearing.clear();
        s.error.clear();
    });
    match super::brains::ask(space, text, shared, source) {
        Ok(()) => {
            timing::mark("sent to the brain");
            mouth.asked(text, phone, spoken);
            if let Some(delay) = ack {
                mouth.ack(delay);
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("[brain] {e:#}");
            let msg = format!("{e:#}");
            shared.update(|s| s.error = msg.clone());
            mouth.nothing_asked();
            mouth.say(brain::spoken_error(&msg), true);
            Err(e)
        }
    }
}

/// One line: every agent the radar sees and its status; then what spaces
/// out of focus hold that the user would want and hasn't been told.
pub(super) fn radar(shared: &Shared) -> String {
    let line = radar_line(&shared.with(|s| s.agents.clone()));
    let hours = crate::config::Config::load().map(|c| c.alerts.focus_hours).unwrap_or(8);
    let focus = crate::spaces::focus_now(hours);
    let spaces = crate::spaces::load();
    let line = match crate::spaces::radar_note(&crate::spaces::held(&crate::history::load(), &spaces), focus.as_deref()) {
        Some(note) => format!("{line} {note}"),
        None => line,
    };
    // What the user asked for earlier and isn't finished yet.
    match super::agendas::radar_note() {
        Some(note) => format!("{line} {note}"),
        None => line,
    }
}

/// `[radar] t5 fix login: working; t6 api: idle` for these agents.
pub(super) fn radar_line(agents: &[super::state::AgentLine]) -> String {
    if agents.is_empty() {
        return "[radar] no agents running".to_string();
    }
    let list: Vec<String> = agents.iter().map(radar_entry).collect();
    format!("[radar] {}", list.join("; "))
}

/// `t5 fix login [eyzflow]: working, 2 queued`: who, in which project, doing what.
fn radar_entry(a: &super::state::AgentLine) -> String {
    let folder = a.folder.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let folder = if folder.is_empty() || crate::split::normalize(folder) == crate::split::normalize(&a.name) {
        String::new()
    } else {
        format!(" [{folder}]")
    };
    let status = if a.status.is_empty() { "starting" } else { &a.status };
    let queued = if a.queued > 0 { format!(", {} queued", a.queued) } else { String::new() };
    format!("{} {}{folder}: {status}{queued}", a.address, a.name)
}

fn is_only_stop(norm: &str, stop_words: &[String]) -> bool {
    let n = norm.trim_start_matches("tower ").trim_start_matches("hey tower ").trim();
    stop_words.iter().any(|w| n == w || n == format!("{w} {w}") || n == format!("{w} please") || n == format!("please {w}"))
}

/// A stop word in what was heard that isn't in what the tower is saying.
fn heard_stop(norm: &str, said: &str, stop_words: &[String]) -> bool {
    let has = |text: &str, w: &str| {
        if w.contains(' ') { format!(" {text} ").contains(&format!(" {w} ")) } else { text.split(' ').any(|word| word == w) }
    };
    stop_words.iter().any(|w| has(norm, w) && !has(said, w))
}

/// The request after the wake word, keeping the original wording. None when
/// the utterance doesn't start with a wake word.
fn after_wake_word(heard: &str, wake_words: &[String]) -> Option<String> {
    let words: Vec<&str> = heard.split_whitespace().collect();
    let norm_words: Vec<String> = words.iter().map(|w| text::normalize(w)).collect();
    let mut wakes: Vec<Vec<String>> = wake_words.iter().map(|w| text::normalize(w).split(' ').map(String::from).collect()).collect();
    // Longest first: "hey tower" before "tower".
    wakes.sort_by_key(|w| std::cmp::Reverse(w.len()));
    for wake in wakes {
        // Allow a filler word before it ("so tower ...", "uh, tower").
        for skip in 0..=1usize {
            if norm_words.len() >= skip + wake.len() && norm_words[skip..skip + wake.len()] == wake[..] {
                let rest = words[skip + wake.len()..].join(" ");
                let rest = rest.trim_start_matches([',', '.', '!', '?', ':', ' ']).to_string();
                return Some(rest);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utt(source: Source) -> Utterance {
        Utterance { samples: vec![], source }
    }

    #[test]
    fn a_stale_partial_gives_way() {
        let kinds = |q: &VecDeque<Utterance>| q.iter().map(|u| matches!(u.source, Source::Partial)).collect::<Vec<_>>();
        // The take ended while its partial waited: only the final is transcribed.
        let mut q: VecDeque<Utterance> = [utt(Source::Partial), utt(Source::Ptt)].into();
        assert!(matches!(next(&mut q).unwrap().source, Source::Ptt));
        assert!(q.is_empty());
        // Of several partials, the newest.
        let mut q: VecDeque<Utterance> = [utt(Source::Partial), utt(Source::Partial)].into();
        assert!(matches!(next(&mut q).unwrap().source, Source::Partial));
        assert!(q.is_empty());
        // A partial alone is transcribed; a final is never skipped.
        let mut q: VecDeque<Utterance> = [utt(Source::Partial)].into();
        assert!(next(&mut q).is_some());
        let mut q: VecDeque<Utterance> = [utt(Source::Ptt), utt(Source::Partial)].into();
        assert!(matches!(next(&mut q).unwrap().source, Source::Ptt));
        assert_eq!(kinds(&q), vec![true]);
    }

    fn wakes() -> Vec<String> {
        ["hey tower", "okay tower", "tower"].map(String::from).to_vec()
    }

    #[test]
    fn takes_are_turned_down_out_loud_when_whisper_is_down() {
        let shared = Shared::new(super::super::state::State::new("ptt", "test", "test"));
        let mouth = Mouth::silent(std::sync::Arc::new(super::super::lan::hub::Hub::default()));
        let (tx, rx) = std::sync::mpsc::channel();
        let (reply, heard) = std::sync::mpsc::channel();
        tx.send(utt(Source::Ptt)).unwrap();
        let phone = Source::Phone { reply, attachments: vec![], markers: vec![], send: true, phone: None, device: "d".into() };
        tx.send(utt(phone)).unwrap();
        drop(tx);
        refuse_all(&rx, &shared, &mouth, "speech recognition didn't start: no GPU");
        assert_eq!(shared.get().error, "speech recognition didn't start: no GPU");
        // The phone hears back at once: nothing was heard.
        assert_eq!(heard.try_recv().unwrap(), "");
    }

    #[test]
    fn wake_word() {
        assert_eq!(after_wake_word("Tower, what's eyzflow doing?", &wakes()).as_deref(), Some("what's eyzflow doing?"));
        assert_eq!(after_wake_word("Hey Tower. Status.", &wakes()).as_deref(), Some("Status."));
        assert_eq!(after_wake_word("So tower run the tests", &wakes()).as_deref(), Some("run the tests"));
        assert_eq!(after_wake_word("Tower.", &wakes()).as_deref(), Some(""));
        assert_eq!(after_wake_word("I think the tower is tall", &wakes()), None);
    }

    #[test]
    fn stops() {
        let stops: Vec<String> = ["stop", "hush", "be quiet"].map(String::from).to_vec();
        assert!(is_only_stop("stop", &stops));
        assert!(is_only_stop("tower stop", &stops));
        assert!(is_only_stop("be quiet", &stops));
        assert!(!is_only_stop("stop the dev server", &stops));
        assert!(heard_stop("light over the city stop", "the long afternoon light over the city", &stops));
        assert!(!heard_stop("tests stop failing", "the tests stop failing now", &stops));
        assert!(!heard_stop("stopped the server", "", &stops));
    }
}
