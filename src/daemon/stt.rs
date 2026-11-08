//! Understanding: whisper.cpp (whisper-rs, Metal) turns utterances into
//! text, then decides what they are for: the brain, a hush, or nothing.

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
                eprintln!("[stt] {e}");
                return;
            }
        };
        let mut armed_until: Option<Instant> = None;
        for utt in rx {
            // A press-to-talk take that asks nothing: alerts needn't wait.
            let ptt = matches!(utt.source, Source::Ptt);
            let secs = utt.samples.len() as f32 / RATE as f32;
            shared.update(|s| s.transcribing = true);
            let started = Instant::now();
            let heard = transcribe(&mut state, &utt.samples, &settings.language, &vocabulary(&shared));
            shared.update(|s| s.transcribing = false);
            let heard = match heard {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[stt] {e:#}");
                    if ptt {
                        mouth.nothing_asked();
                    }
                    continue;
                }
            };
            eprintln!("[stt] {secs:.1}s in {:.0} ms: {heard:?}", started.elapsed().as_millis());
            timing::mark("transcribed");
            if text::is_hallucination(&heard) {
                if ptt {
                    mouth.nothing_asked();
                }
                continue;
            }
            let norm = text::normalize(&heard);
            let is_stop = is_only_stop(&norm, &settings.stop_words);

            match utt.source {
                Source::Ptt => {
                    if is_stop {
                        mouth.nothing_asked();
                        mouth.hush();
                        continue;
                    }
                    let _ = deliver(&shared, &mouth, heard.trim(), settings.ack);
                }
                Source::HandsFree { over_speech } => {
                    if over_speech || shared.get().speaking {
                        // Probably the tower hearing itself; only a stop word
                        // it isn't saying itself counts.
                        let said = text::normalize(&shared.get().said);
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
                        let _ = deliver(&shared, &mouth, heard.trim(), settings.ack);
                        continue;
                    }
                    if let Some(rest) = after_wake_word(&heard, &settings.wake_words) {
                        if rest.split_whitespace().count() == 0 {
                            // "Tower." alone: listen for the request.
                            armed_until = Some(Instant::now() + Duration::from_secs(8));
                            shared.update(|s| s.armed = true);
                            mouth.chime(true);
                        } else {
                            let _ = deliver(&shared, &mouth, &rest, settings.ack);
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
    let s = shared.get();
    let mut names: Vec<String> = s.agents.iter().map(|a| a.name.clone()).collect();
    names.sort();
    names.dedup();
    names.truncate(30);
    format!("Tower, Claude, Codex, Herdr, terminal, agent. {}.", names.join(", "))
}

/// Hands a request to the brain and acknowledges it out loud.
pub fn deliver(shared: &Shared, mouth: &Mouth, text: &str, ack: Option<Duration>) -> anyhow::Result<()> {
    shared.update(|s| {
        s.heard = text.to_string();
        s.heard_ms = now_ms();
        s.error.clear();
    });
    match brain::ask(text, &radar(shared)) {
        Ok(()) => {
            timing::mark("sent to the brain");
            mouth.asked(text);
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

/// One line: every agent the radar sees and its status.
fn radar(shared: &Shared) -> String {
    let agents = shared.get().agents;
    if agents.is_empty() {
        return "[radar] no agents running".into();
    }
    let list: Vec<String> =
        agents.iter().map(|a| format!("{} {}: {}", a.address, a.name, if a.status.is_empty() { "starting" } else { &a.status })).collect();
    format!("[radar] {}", list.join("; "))
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

    fn wakes() -> Vec<String> {
        ["hey tower", "okay tower", "tower"].map(String::from).to_vec()
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
