//! `tower selftest`: Kokoro says a command, the resampler takes it to
//! 16 kHz, Whisper writes it down. No microphone or speakers involved.

use std::time::Instant;

use anyhow::{Context, Result};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::config::Config;
use crate::daemon::mic::{RATE, resample};

pub fn run() -> Result<()> {
    let cfg = Config::load()?;
    let phrase = "Tower, tell the eyzflow session to run the tests and fix anything that fails.";

    let t = Instant::now();
    let engine = loqui::Engine::builder().preload(true).build().context("loading Kokoro")?;
    println!("Kokoro loaded in {} ms", t.elapsed().as_millis());
    let t = Instant::now();
    let req =
        loqui::SpeakRequest { voice: cfg.voice.clone(), speed: cfg.speed, format: loqui::Format::Pcm, ..loqui::SpeakRequest::new(phrase) };
    let speech = engine.speak(&req)?;
    let samples: Vec<f32> = speech.audio.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
    let synth_ms = t.elapsed().as_millis();
    println!(
        "Kokoro: {:.1} s of speech in {synth_ms} ms ({:.0}x real time)",
        speech.duration_secs,
        speech.duration_secs * 1000.0 / synth_ms.max(1) as f64
    );

    let audio = resample(&samples, 24_000, RATE);

    whisper_rs::install_logging_hooks();
    let t = Instant::now();
    let mut params = WhisperContextParameters::default();
    params.use_gpu(true);
    let ctx = WhisperContext::new_with_params(cfg.whisper_model.to_str().unwrap_or_default(), params)?;
    let mut state = ctx.create_state()?;
    println!("Whisper ({}) loaded in {} ms", cfg.whisper_model.file_name().unwrap_or_default().to_string_lossy(), t.elapsed().as_millis());
    for run in 1..=2 {
        let t = Instant::now();
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_language(Some(&cfg.language));
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        p.set_initial_prompt("Tower, Claude, Codex, Herdr, eyzflow.");
        state.full(p, &audio)?;
        let heard: String = state.as_iter().map(|s| s.to_str_lossy().unwrap_or_default().into_owned()).collect();
        println!("Whisper run {run}: {:?} in {} ms", heard.trim(), t.elapsed().as_millis());
    }
    println!("Said:  {phrase:?}");
    Ok(())
}
