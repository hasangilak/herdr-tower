//! `tower daemon`: the ears, the mouth and the radar, behind a Unix socket.

pub mod asks;
mod book;
pub mod hotkey;
pub mod lan;
pub mod mic;
pub mod narrate;
pub mod queue;
pub mod sendq;
pub mod sessions;
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
    asks: asks::Asks,
    sendq: sendq::SendQueue,
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
    let asks = asks::Asks::new(shared.clone(), hub.clone(), &cfg.question_policy);
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
    let lan = lan::Lan::new(lan::Ctx { shared: shared.clone(), mouth: mouth.clone(), hub: hub.clone(), ack: ack_delay(&cfg), stt: utt_tx.clone(), asks: asks.clone() });
    let ears = mic::start(cfg.mode.clone(), cfg.input_device.clone(), cfg.vad.clone(), shared.clone(), mouth.clone(), utt_tx)?;
    hotkey::start(&cfg.talk_key, ears.clone(), shared.clone(), mouth.clone());
    let watches = watch::Watches::default();
    watch::start(shared.clone(), mouth.clone(), cfg.alerts.clone(), watches.clone(), asks.clone());

    let listener = UnixListener::bind(&sock).with_context(|| format!("listening on {}", sock.display()))?;
    eprintln!("[tower] ready on {} (mode {})", sock.display(), cfg.mode);
    if cfg.lan.enabled
        && let Err(e) = lan.start(&cfg.lan)
    {
        eprintln!("[phone] the phone API didn't start: {e:#}");
    }
    load_screenshots(&shared);
    shared.update(|s| s.behaviors = crate::behaviors::Store::open().summary());
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
    // A history grown large is compacted (the whole file kept aside).
    match crate::history::compact(&crate::history::path(), 16 << 20, 20_000) {
        Ok(0) => {}
        Ok(n) => eprintln!("[tower] history: {n} old entries moved aside"),
        Err(e) => eprintln!("[tower] history: couldn't compact it: {e}"),
    }
    // What agent sessions' hooks left while no daemon ran.
    let spooled = sessions::registry().take_spool(&sessions::spool_path());
    if spooled > 0 {
        eprintln!("[sessions] {spooled} hook event(s) from while the voice was down");
    }
    let sendq = sendq::SendQueue::open();
    {
        // What the queue delivers is watched; what it has to say goes through the radar's outbox.
        let (tell, watch) = (watches.clone(), watches.clone());
        sendq::wire(sendq::Wiring {
            tell: Box::new(move |note| {
                // A test session's queue news never reaches the brain (as with its alerts).
                let test_name = note.name.starts_with("qfix ") || note.name.starts_with("qfix-");
                if test_name || (!note.session.is_empty() && crate::hc::get(&note.session).is_some_and(|t| watch::is_test(&t))) {
                    eprintln!("[queue] (test session) {}", note.msg);
                    return;
                }
                tell.tell(note)
            }),
            watch: Box::new(move |session, text| {
                // Test sessions are never watched (nothing would ever report on them).
                if crate::hc::get(session).is_some_and(|t| watch::is_test(&t)) {
                    return;
                }
                if let Err(e) = watch.add(session, text, None) {
                    eprintln!("[queue] couldn't watch {session}: {e:#}");
                }
            }),
        });
    }
    sendq.start();
    let daemon = Daemon { cfg, shared, mouth, narrator, ears, watches, lan, asks, sendq };
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
                let kind_word = kind.clone();
                let kind = if kind == "alert" { queue::Kind::Alert } else { narrate::turn_kind() };
                let spoken = text::speakable(&text, self.cfg.max_spoken_chars);
                // The brain recommending answers to a session's questions: it stays quiet.
                let quiet = brain::question_turn() && kind_word != "alert";
                if quiet && !spoken.is_empty() {
                    eprintln!("[questions] the brain's [question] turn isn't spoken: {spoken:?}");
                }
                if !heard && !spoken.is_empty() && !quiet {
                    self.mouth.say_as(spoken, kind, interrupt);
                }
                // The answer is all in: alerts may follow it.
                if answer && kind == queue::Kind::Reply {
                    self.mouth.answered();
                }
            }
            Request::Hush => self.mouth.hush(),
            Request::Mute { action } => {
                // The toggle reads and flips under the mouth's lock: two at once can't both read "unmuted".
                let done = match action.as_str() {
                    "on" => self.mouth.set_muted(true),
                    "off" => self.mouth.set_muted(false),
                    "toggle" => self.mouth.toggle_muted().map(|_| ()),
                    "status" | "" => Ok(()),
                    _ => return Response::err("mute is on, off, toggle or status"),
                };
                if let Err(e) = done {
                    return Response::err(format!("{e:#}"));
                }
            }
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
                if let Err(e) = stt::deliver(&self.shared, &self.mouth, &text, ack_delay(&self.cfg), "mac") {
                    return Response::err(brain::spoken_error(&format!("{e:#}")));
                }
            }
            Request::Watch { terminal, note, account } => {
                return match self.watches.add(&terminal, &note, account) {
                    Ok(address) => Response { ok: true, error: None, state: Some(serde_json::json!({ "address": address })) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::Queue { action, session, text, id, after, start } => {
                if action == "start" {
                    return match self.sendq.add_start(&session, &text, &start, &after) {
                        Ok(id) => Response { ok: true, error: None, state: Some(serde_json::json!({ "id": id, "name": session })) },
                        Err(e) => Response::err(format!("{e:#}")),
                    };
                }
                return self.queue(&action, &session, &text, id, &after);
            }
            Request::Agent { event } => {
                let change = sessions::registry().apply(&event);
                if change != sessions::Change::Nothing {
                    self.watches.agent(&event, &change);
                    self.sendq.kick();
                }
                return Response { ok: true, error: None, state: None };
            }
            Request::Sessions => {
                let all = sessions::registry().all();
                return Response { ok: true, error: None, state: Some(serde_json::json!({ "sessions": all })) };
            }
            Request::Status => {}
            Request::Questions => {
                let v: Vec<serde_json::Value> = self.asks.list().iter().map(|s| serde_json::to_value(s).unwrap_or_default()).collect();
                return Response { ok: true, error: None, state: Some(serde_json::json!({ "questions": v })) };
            }
            Request::Answer { target, answers, submit } => {
                let answers: Vec<crate::questions::Answer> = match serde_json::from_value(answers) {
                    Ok(a) => a,
                    Err(e) => return Response::err(format!("answers: {e}")),
                };
                return match self.asks.answer(&target, &answers, submit) {
                    Ok(set) => Response { ok: true, error: None, state: Some(serde_json::to_value(set).unwrap_or_default()) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::BehaviorsChanged { by, change } => self.behaviors_changed(&by, &change),
            Request::Dismiss { id } => {
                return match self.asks.dismiss(&id) {
                    Ok(()) => Response { ok: true, error: None, state: None },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::Policy { policy } => {
                if !["normal", "keep-moving", "ask-everything"].contains(&policy.as_str()) {
                    return Response::err("policy is normal, keep-moving or ask-everything");
                }
                self.asks.set_policy(&policy);
            }
            Request::Recommend { id, recs, decision, reason, none } => {
                return match self.asks.recommend(&id, &recs, decision.as_deref(), &reason, none) {
                    Ok(set) => Response { ok: true, error: None, state: Some(serde_json::to_value(set).unwrap_or_default()) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
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
                    // `<device id>`: its route changed on file.
                    "route" => {
                        self.lan.route_changed(&arg);
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

impl Daemon {
    /// The behaviors changed: the state, the phones, a card in the chat, and
    /// the brain (told to read its instructions again, when it wasn't the one).
    fn behaviors_changed(&self, by: &str, change: &serde_json::Value) {
        let summary = crate::behaviors::Store::open().summary();
        self.shared.update(|s| s.behaviors = summary);
        self.lan.hub().broadcast("behaviors", change.clone());
        let title = change["title"].as_str().unwrap_or("").to_string();
        let verb = match change["op"].as_str().unwrap_or("") {
            "add" => "Added",
            "edit" => "Changed",
            "enable" => "Turned on",
            "disable" => "Turned off",
            "remove" => "Removed",
            "undo" => "Undid the last change to",
            other => other,
        }
        .to_string();
        let diff = change["diff"].as_str().map(String::from);
        crate::history::later(move || {
            crate::history::append(crate::history::Entry {
                kind: "behavior".into(),
                text: format!("{verb} a behavior: {title}"),
                diff,
                ..Default::default()
            })
        });
        if by != "brain" {
            let note = format!(
                "[behaviors] {} \"{}\": read ~/.config/herder-tower/brain/CLAUDE.md again, and write nothing.",
                change["op"].as_str().unwrap_or("changed"),
                change["title"].as_str().unwrap_or("")
            );
            std::thread::spawn(move || {
                let _ = crate::brain::note(&note);
            });
        }
    }
}

impl Daemon {
    /// `tower queue`: add (sent now when the session is free), list, cancel.
    fn queue(&self, action: &str, session: &str, text: &str, id: Option<u64>, after: &[u64]) -> Response {
        let ok = |v: serde_json::Value| Response { ok: true, error: None, state: Some(v) };
        match action {
            "retarget" => match id.map(|id| self.sendq.retarget(id, session)) {
                Some(Ok(item)) => ok(serde_json::json!({ "retargeted": item })),
                Some(Err(e)) => Response::err(format!("{e:#}")),
                None => Response::err("retarget needs an id and a session"),
            },
            "track" => match self.sendq.track(session, text) {
                Ok(id) => ok(serde_json::json!({ "id": id, "session": session })),
                Err(e) => Response::err(format!("{e:#}")),
            },
            "add" => match self.sendq.add(session, text, after) {
                Ok((sendq::Added::Sent { id }, address, name)) => ok(serde_json::json!({ "sent": true, "id": id, "session": address, "name": name })),
                Ok((sendq::Added::Queued { id, position, status }, address, name)) => {
                    ok(serde_json::json!({ "sent": false, "id": id, "position": position, "status": status, "session": address, "name": name }))
                }
                Err(e) => Response::err(format!("{e:#}")),
            },
            "list" => ok(serde_json::json!({ "items": self.sendq.items() })),
            "cancel" => match id.map(|id| self.sendq.cancel(id)) {
                Some(Ok(item)) => ok(serde_json::json!({ "cancelled": item })),
                Some(Err(e)) => Response::err(format!("{e:#}")),
                None => Response::err("cancel needs an id"),
            },
            _ => Response::err("queue is add, list, cancel or retarget"),
        }
    }
}

fn ack_delay(cfg: &Config) -> Option<Duration> {
    (!cfg.ack.is_empty()).then(|| Duration::from_millis(cfg.ack_delay_ms))
}
