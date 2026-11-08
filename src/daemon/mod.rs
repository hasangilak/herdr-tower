//! `tower daemon`: the ears, the mouth and the radar, behind a Unix socket.

mod book;
pub mod hotkey;
pub mod lan;
pub mod mic;
pub mod narrate;
pub mod queue;
pub mod state;
pub mod stt;
pub mod timing;
pub mod tts;
pub mod watch;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::brain;
use crate::config::{self, Config};
use crate::protocol::{Request, Response};
use crate::text;
use state::{Shared, State};

#[derive(Clone)]
struct Daemon {
    cfg: Config,
    shared: Shared,
    mouth: tts::Mouth,
    narrator: narrate::Narrator,
    ears: mic::Ears,
    watches: watch::Watches,
    lan: lan::Lan,
}

pub fn run() -> Result<()> {
    let cfg = Config::load()?;
    std::fs::create_dir_all(config::state_dir())?;
    let sock = config::socket_path();
    if crate::protocol::is_running() {
        anyhow::bail!("the tower's voice is already running ({})", sock.display());
    }
    let _ = std::fs::remove_file(&sock);

    let model_name = cfg.whisper_model.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let shared = Shared::new(State::new(&cfg.mode, &cfg.voice, &model_name));
    let hub = std::sync::Arc::new(lan::hub::Hub::default());
    eprintln!("[tower] loading Kokoro ({}) ...", cfg.voice);
    let mouth = tts::start(
        tts::Settings {
            voice: cfg.voice.clone(),
            speed: cfg.speed,
            volume: cfg.volume,
            acks: cfg.ack.clone(),
            cue: cfg.alert_cue,
            alert_wait: Duration::from_millis(cfg.alert_wait_ms),
            alert_max_age: Duration::from_secs(cfg.alert_max_age_secs),
        },
        shared.clone(),
        hub.clone(),
    )?;
    let narrator = narrate::Narrator::start(cfg.narrate, mouth.clone(), cfg.max_spoken_chars);
    eprintln!("[tower] loading Whisper ...");
    let (utt_tx, utt_rx) = channel();
    stt::start(
        stt::Settings {
            model: cfg.whisper_model.clone(),
            language: cfg.language.clone(),
            wake_words: cfg.wake_words.clone(),
            stop_words: cfg.stop_words.clone(),
            ack: ack_delay(&cfg),
        },
        utt_rx,
        shared.clone(),
        mouth.clone(),
    )?;
    let lan = lan::Lan::new(lan::Ctx { shared: shared.clone(), mouth: mouth.clone(), hub, ack: ack_delay(&cfg), stt: utt_tx.clone() });
    let ears = mic::start(cfg.mode.clone(), cfg.input_device.clone(), cfg.vad.clone(), shared.clone(), mouth.clone(), utt_tx)?;
    hotkey::start(&cfg.talk_key, ears.clone(), shared.clone(), mouth.clone());
    let watches = watch::Watches::default();
    watch::start(shared.clone(), mouth.clone(), cfg.alerts.clone(), watches.clone());

    let listener = UnixListener::bind(&sock).with_context(|| format!("listening on {}", sock.display()))?;
    eprintln!("[tower] ready on {} (mode {})", sock.display(), cfg.mode);
    if cfg.lan.enabled
        && let Err(e) = lan.start(&cfg.lan)
    {
        eprintln!("[phone] the phone API didn't start: {e:#}");
    }
    load_screenshots(&shared);
    {
        let shared = shared.clone();
        thread::Builder::new().name("screenshots".into()).spawn(move || {
            loop {
                let n = crate::screenshots::prune(&crate::screenshots::dir(), crate::screenshots::KEEP);
                if n > 0 {
                    eprintln!("[tower] removed {n} screenshot(s) older than 30 days");
                    load_screenshots(&shared);
                }
                thread::sleep(Duration::from_secs(6 * 3600));
            }
        })?;
    }
    let daemon = Daemon { cfg, shared, mouth, narrator, ears, watches, lan };
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let d = daemon.clone();
        thread::spawn(move || {
            if let Err(e) = d.serve(conn) {
                eprintln!("[socket] {e:#}");
            }
        });
    }
    Ok(())
}

impl Daemon {
    fn serve(&self, conn: UnixStream) -> Result<()> {
        let mut reader = BufReader::new(conn.try_clone()?);
        let mut line = String::new();
        // A probe (is it running?) connects and leaves without a word.
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            return Ok(());
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => self.handle(req),
            Err(e) => Response::err(format!("bad request: {e}")),
        };
        let mut conn = conn;
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        conn.write_all(out.as_bytes())?;
        Ok(())
    }

    fn handle(&self, req: Request) -> Response {
        match req {
            Request::Say { text, interrupt, kind } => {
                let answer = kind == "answer";
                if !text.trim().is_empty() {
                    timing::mark("text to say came in (tower say or the Stop hook)");
                }
                // The brain's final answer (its Stop hook) is usually heard
                // already, as it wrote it. (This reads its transcript, so the
                // turn's kind below is current.)
                let heard = !interrupt && self.narrator.already_said(&text);
                // A watch's news, or the brain telling one, waits its turn.
                let kind = if kind == "alert" { queue::Kind::Alert } else { narrate::turn_kind() };
                let spoken = text::speakable(&text, self.cfg.max_spoken_chars);
                if !heard && !spoken.is_empty() {
                    self.mouth.say_as(spoken, kind, interrupt);
                }
                // The answer is all in: alerts may follow it.
                if answer && kind == queue::Kind::Reply {
                    self.mouth.answered();
                }
            }
            Request::Hush => self.mouth.hush(),
            Request::Talk { action } => self.ears.control(mic::Ctl::Talk(action)),
            Request::Cancel => self.ears.control(mic::Ctl::Cancel),
            Request::Mode { mode } => {
                if !["ptt", "handsfree", "off"].contains(&mode.as_str()) {
                    return Response::err("mode is ptt, handsfree or off");
                }
                self.ears.control(mic::Ctl::Mode(mode));
            }
            Request::Ask { text } => {
                timing::start("asked");
                if let Err(e) = stt::deliver(&self.shared, &self.mouth, &text, ack_delay(&self.cfg)) {
                    return Response::err(brain::spoken_error(&format!("{e:#}")));
                }
            }
            Request::Watch { terminal, note, account } => self.watches.add(&terminal, &note, account),
            Request::Status => {}
            Request::Screenshots => load_screenshots(&self.shared),
            Request::Phone { action, arg } => {
                let info = match action.as_str() {
                    "on" => {
                        // The config as it is now: `tower phone enable` just wrote it.
                        let cfg = Config::load().map(|c| c.lan).unwrap_or_else(|_| self.cfg.lan.clone());
                        match self.lan.start(&cfg) {
                            Ok(_) => Ok(self.lan.info()),
                            Err(e) => Err(e),
                        }
                    }
                    "off" => {
                        self.lan.stop();
                        Ok(self.lan.info())
                    }
                    "pair" => self.lan.pair(),
                    "revoked" => {
                        self.lan.revoked(&arg);
                        Ok(self.lan.info())
                    }
                    _ => Ok(self.lan.info()),
                };
                return match info {
                    Ok(v) => Response { ok: true, error: None, state: Some(v) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::Quit => {
                let _ = std::fs::remove_file(config::socket_path());
                self.shared.update(|s| {
                    s.mic = "closed".into();
                    s.running = false;
                });
                std::process::exit(0);
            }
        }
        Response { ok: true, error: None, state: Some(self.shared.json()) }
    }
}

/// The state's list of recent screenshots, from the history.
fn load_screenshots(shared: &Shared) {
    let entries = crate::history::load();
    let list: Vec<serde_json::Value> =
        crate::screenshots::recent(&entries, crate::screenshots::RECENT).into_iter().map(crate::screenshots::row).collect();
    shared.update(|s| s.screenshots = list);
}

fn ack_delay(cfg: &Config) -> Option<Duration> {
    (!cfg.ack.is_empty()).then(|| Duration::from_millis(cfg.ack_delay_ms))
}
