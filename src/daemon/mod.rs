//! `tower daemon`: the ears, the mouth and the radar, behind a Unix socket.

pub mod acts;
pub mod agendas;
pub mod automerge;
pub mod asks;
pub mod brains;
pub mod card;
mod book;
pub mod decisions;
mod desk;
pub mod goals;
pub mod hotkey;
pub mod introduce;
pub mod lan;
pub mod merges;
pub mod mic;
pub mod missions;
pub mod narrate;
pub mod plans;
pub mod queue;
pub mod releases;
pub mod reopen;
pub mod reviews;
pub mod sendq;
pub mod sessions;
pub mod state;
pub mod stt;
pub mod timing;
pub mod tts;
pub mod typed;
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
    prs: crate::prs::Prs,
    agendas: agendas::Agendas,
    introducer: introduce::Introducer,
    decisions: decisions::Decisions,
    missions: missions::Missions,
    brains: brains::Brains,
    acts: acts::Acts,
    plans: plans::Plans,
    reviews: reviews::Reviews,
}

pub fn run() -> Result<()> {
    let mut cfg = Config::load()?;
    // What the looks fall back on, should the file stop reading while this runs.
    let _ = Config::live();
    // Without its voice (`tts::voiceless`, a daemon in a sandbox): nothing
    // listens either, whatever its config says.
    let voiceless = tts::voiceless();
    if voiceless {
        (cfg.mode, cfg.talk_key) = ("off".into(), "off".into());
    }
    std::fs::create_dir_all(config::state_dir())?;
    let sock = config::socket_path();
    if crate::protocol::is_running() {
        anyhow::bail!("the tower's voice is already running ({})", sock.display());
    }
    let _ = std::fs::remove_file(&sock);

    let model_name = cfg.whisper_model.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let shared = Shared::new(State::new(&cfg.mode, &cfg.voice, &model_name));
    let hub = std::sync::Arc::new(lan::hub::Hub::default());
    // What only the user can decide, one decision each, as the last run left them.
    let decisions = decisions::Decisions::open(shared.clone());
    let asks = asks::Asks::new(shared.clone(), hub.clone(), &cfg.question_policy).stored_at(asks::Asks::path()).with_decisions(decisions.clone());
    if !voiceless {
        eprintln!("[tower] loading Kokoro ({}) ...", cfg.voice);
    }
    let mouth = tts::start(
        tts::Settings {
            voice: cfg.voice.clone(),
            speed: cfg.speed,
            volume: cfg.volume,
            acks: cfg.ack.clone(),
            cue: cfg.alert_cue,
            alert_wait: Duration::from_millis(cfg.alert_wait_ms),
            alert_max_age: Duration::from_secs(cfg.alert_max_age_secs),
            on_mac: cfg.voice_on_mac,
            says: cfg.tell.voice,
        },
        shared.clone(),
        hub.clone(),
    )?;
    let narrator = narrate::Narrator::start(cfg.narrate, mouth.clone(), cfg.max_spoken_chars);
    // The brains: the main one and each space's own. The radar tells of them
    // as it sees them, so this comes before its first look.
    let brains = brains::Brains::start(shared.clone(), narrator.clone());
    let (utt_tx, utt_rx) = channel();
    if voiceless {
        // No Whisper: a take (there is none without a microphone) would go nowhere.
        drop(utt_rx);
    } else {
        eprintln!("[tower] loading Whisper ...");
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
    }
    // The PRs sessions open: every change goes to the phones (`pr` event),
    // and those that wait for a merge are one decision per repo and space.
    let prs = crate::prs::Prs::open_daemon();
    {
        let (hub, decisions, all) = (hub.clone(), decisions.clone(), prs.clone());
        prs.on_change(Box::new(move |op, pr| {
            hub.broadcast("pr", serde_json::json!({ "op": op, "pr": pr.view() }));
            // A PR found in a space with review on gets its reviewer (first: a PR the
            // tower will merge itself waits on it, and is no decision meanwhile); one
            // that closed ends its review.
            if let Some(r) = reviews::running() {
                r.pr_changed(op, pr);
            }
            sync_merges(&decisions, &all);
        }));
    }
    let lan = lan::Lan::new(lan::Ctx {
        shared: shared.clone(),
        mouth: mouth.clone(),
        hub: hub.clone(),
        stt: utt_tx.clone(),
        asks: asks.clone(),
        prs: prs.clone(),
    });
    let ears = mic::start(cfg.mode.clone(), cfg.input_device.clone(), cfg.vad.clone(), shared.clone(), mouth.clone(), utt_tx)?;
    hotkey::start(&cfg.talk_key, ears.clone(), shared.clone(), mouth.clone());
    let watches = watch::Watches::default();
    watch::start(shared.clone(), mouth.clone(), cfg.alerts.clone(), watches.clone(), asks.clone(), decisions.clone());
    // The cards the last run left are decisions again by now, and so are the
    // PRs that wait: what is open at the start was told before, and isn't again.
    sync_merges(&decisions, &prs);
    decisions.told_all();
    decisions.start();

    let listener = UnixListener::bind(&sock).with_context(|| format!("listening on {}", sock.display()))?;
    eprintln!("[tower] ready on {} (mode {})", sock.display(), cfg.mode);
    load_screenshots(&shared);
    shared.update(|s| s.behaviors = crate::behaviors::Store::open().summary());
    // How much the user is told, and what the sessions of each space wrote to each other.
    {
        let (level, threads) = (crate::tell::level(), crate::peers::threads(&crate::history::load()));
        eprintln!("[tower] telling the user: {level}");
        shared.update(|s| {
            s.tell_level = level.word().into();
            s.threads = threads;
        });
    }
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
    // A hand edit of `[tell]` in config.toml reaches the state and the mouth without a command.
    {
        let (shared, mouth) = (shared.clone(), mouth.clone());
        thread::Builder::new().name("tell".into()).spawn(move || {
            let mut was = (crate::tell::level(), crate::tell::voice());
            let mut round: u64 = 0;
            loop {
                thread::sleep(Duration::from_secs(2));
                round += 1;
                // The behaviors as their files have them now: seeded after this started (an
                // install, `tower up`), or changed by hand. Every ten seconds.
                if round.is_multiple_of(5) {
                    state::follow_behaviors(&shared, crate::behaviors::Store::open().summary());
                }
                let now = (crate::tell::level(), crate::tell::voice());
                if now.0 != was.0 {
                    shared.update(|s| s.tell_level = now.0.word().into());
                }
                if now.1 != was.1 && now.1 != mouth.voice() {
                    eprintln!("[tell] voice: {} (config.toml changed)", now.1);
                    let _ = mouth.set_voice(now.1);
                }
                was = now;
            }
        })?;
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
                if (test_name && !watch::test_daemon()) || (!note.session.is_empty() && crate::hc::get(&note.session).is_some_and(|t| watch::left_alone(&t))) {
                    eprintln!("[queue] (test session) {}", note.msg);
                    return;
                }
                tell.tell(note)
            }),
            watch: Box::new(move |session, text| {
                // Test sessions are never watched (nothing would ever report on them), and neither is a brain.
                if crate::hc::get(session).is_some_and(|t| watch::left_alone(&t) || brain::brain_of(&t).is_some()) {
                    return;
                }
                if let Err(e) = watch.add(session, text, None) {
                    eprintln!("[queue] couldn't watch {session}: {e:#}");
                }
            }),
            // A message between sessions: into its space's thread.
            peer: Box::new({
                let shared = shared.clone();
                move |e| shared.update(|s| crate::peers::push(&mut s.threads, e))
            }),
        });
    }
    sendq.start();
    {
        // A merge (the user's, or on GitHub one its session waited on): the session and the brain are told.
        let (sendq, watches, asks) = (sendq.clone(), watches.clone(), asks.clone());
        prs.on_merged(Box::new(move |pr, by| {
            // Its questions ("merge it, or leave it open?") are settled by the merge.
            asks.settled_by_merge(pr, by);
            after_merge(&sendq, &watches, pr, by)
        }));
        // Open PRs looked at gently; those the tower may merge itself (`[prs]
        // auto_merge_low_risk`, off unless the user turns it on) merged.
        let (prs, decisions, shared) = (prs.clone(), decisions.clone(), shared.clone());
        let merger = automerge::AutoMerger::new(prs.clone(), auto_merge_on).with_roots(crate::impact::roots);
        thread::Builder::new().name("prs".into()).spawn(move || {
            let mut was: Option<bool> = None;
            loop {
                for k in prs.due(crate::history::now_ms()) {
                    prs.poll(&k);
                }
                // The setting turned: in the state for the windows, and the queue as it is now.
                let on = merger.on();
                if was != Some(on) {
                    shared.update(|s| s.auto_merge = on);
                    if was.is_some() {
                        eprintln!("[automerge] {}", if on { "on: low-risk PRs into dev are merged by the tower" } else { "off" });
                        sync_merges(&decisions, &prs);
                    }
                    was = Some(on);
                }
                merger.tick();
                thread::sleep(Duration::from_secs(10));
            }
        })?;
    }
    // A reviewer for each PR a space's sessions open (where review is on), until both agree.
    let reviews = reviews::Reviews::open(
        prs.clone(),
        shared.clone(),
        std::sync::Arc::new(reviews::Real { sendq: sendq.clone(), decisions: decisions.clone(), watches: watches.clone() }),
    );
    reviews.start();
    // The agenda ledger: what the user asked for, followed until it's done.
    let agendas = agendas::Agendas::open();
    // A space's goal follows the user's requests there.
    goals::start();
    agendas.start(agendas::Ctx { sendq: sendq.clone(), watches: watches.clone(), asks: asks.clone(), shared: shared.clone(), decisions: decisions.clone() });
    // Claude sessions in one space are told who the others are, as they join.
    let introducer = introduce::Introducer::new(sendq.clone());
    introducer.start();
    // Each space as a mission: quiet when its sessions are gone, ready when its work is done.
    let missions = {
        let watches = watches.clone();
        missions::Missions::new(shared.clone(), decisions.clone(), agendas.clone(), move |note| watches.tell_agenda(note))
    };
    missions.start();
    // What a restart of the Mac or Herdr closes, kept with the missions' looks and offered back.
    reopen::Reopen::open(decisions.clone()).start();
    // "Merge all N": a merge card's PRs, merged in order by the tower.
    merges::Merges::new(shared.clone(), prs.clone(), sendq.clone()).start();
    // What the tower opened for the user lately, to show with the reply and to undo.
    let acts = acts::Acts::open(shared.clone(), hub.clone());
    // Where each space's changes stand: open, on dev, on prod (from GitHub, never from what sessions say).
    releases::Releases::new(prs.clone(), shared.clone(), prs.gh().to_path_buf(), Some(releases::cache_path())).start();
    // A space's plan before it is launched, and its planner (`tower plan`).
    let plans = plans::Plans::open(shared.clone(), sendq.clone());
    plans.start();
    // A PR found from now on carries its review score: its plan's prediction, and its own files.
    {
        let (plans, gh) = (plans.clone(), prs.gh().to_path_buf());
        prs.on_score(Box::new(move |pr| score_found(&plans, &gh, pr, &crate::impact::roots())));
        // A session that finished its turn on a PR without reporting its planned checks: told once, quietly.
        let sendq = sendq.clone();
        prs.on_unreported(Box::new(move |pr, missing| {
            if pr.session.is_empty() {
                return;
            }
            let note = crate::prscore::unreported_note(pr, missing, &crate::config::tower_bin().display().to_string());
            if let Err(e) = sendq.add_note(&pr.session, &note) {
                eprintln!("[prs] {}: couldn't tell {} about its unreported checks: {e:#}", pr.key(), pr.session);
            }
        }));
    }
    let daemon = Daemon { cfg, shared, mouth, narrator, ears, watches, lan, asks, sendq, prs, agendas, introducer, decisions, missions, brains, acts, plans, reviews };
    // The phone API reaches the rest of the tower through the daemon itself
    // (`desk`), so it starts only now that the daemon is whole.
    daemon.lan.attach(std::sync::Arc::new(daemon.clone()));
    if daemon.cfg.lan.enabled
        && let Err(e) = daemon.lan.start(&daemon.cfg.lan)
    {
        eprintln!("[phone] the phone API didn't start: {e:#}");
    }
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
    /// A session took a prompt (its hook said so). Whose it is: the tower's
    /// own (it typed it: the queue, a reply, the task the session was started
    /// with), another terminal's (`hc send` from a brain or a session), or the
    /// user's, typed in the session's own terminal (`typed`). The user's
    /// answers the question that session asked at the end of its turn, if
    /// one is open (and a reply the tower held for it is not sent, when the
    /// words answer the same): written down as a `request` with status
    /// `answer`, no agenda; else it is new work: their request in that session, in the record (a
    /// `request` from `terminal:<address>`, in its space, never a chat
    /// message) and an agenda of its own (origin `terminal`). Nothing of it
    /// is said, and no brain is told.
    fn prompted(&self, ev: &sessions::AgentEvent, t: Option<&serde_json::Value>) {
        let Some(t) = t else { return };
        // A test session (the tower's own fixtures), or a brain: not the user's work.
        if watch::left_alone(t) || brain::brain_of(t).is_some() {
            return;
        }
        let given = typed::given_task(t);
        let whose = typed::whose(&ev.terminal, &ev.text, given.as_deref(), ev.at_ms);
        if whose != typed::Whose::User {
            eprintln!("[terminal] {}: a prompt from {whose:?}, not the user's", ev.terminal);
            self.asks.typed_by_tower(&ev.terminal, crate::hc::turn(t));
            return;
        }
        let (d, ev, t) = (self.clone(), ev.clone(), t.clone());
        thread::spawn(move || d.typed_there(&ev, &t));
    }

    /// The brain `brain` handed `session` an instruction: when the user spoke
    /// to that brain since the session asked them something at the end of
    /// its turn, the instruction may be their answer, relayed (`Asks::relayed`,
    /// in a thread: it asks the model). Its card closes then.
    fn relayed(&self, brain: &str, session: &str, text: &str) {
        let Some((at_ms, words)) = brain::last_asked(brain) else { return };
        if !self.asks.list().iter().any(|s| s.session == session && s.kind == "text" && s.at_ms <= at_ms) {
            return;
        }
        let (asks, session, text) = (self.asks.clone(), session.to_string(), text.to_string());
        thread::spawn(move || {
            for id in asks.relayed(&session, &words, at_ms, &text) {
                eprintln!("[questions] {id} ({session}): the user's answer, passed on by the tower: settled");
            }
        });
    }

    /// What the user typed in a session's own terminal (`prompted`).
    fn typed_there(&self, ev: &sessions::AgentEvent, t: &serde_json::Value) {
        let address = ev.terminal.as_str();
        let words = crate::introduce::task_from_prompt(&ev.text).unwrap_or_else(|| ev.text.trim().to_string());
        let (name, space) = (crate::hc::spoken_name(t), crate::spaces::space_of(t));
        eprintln!("[terminal] {address} ({name}): the user typed in its terminal: {}", crate::agenda::clip(&words, 120));
        // What the tower has waiting for it may say what the user just did: checked before it goes.
        self.sendq.user_said(address, &words, ev.at_ms);
        if self.asks.typed_there(address, &words, ev.at_ms) {
            // An answer to its question, not new work: no agenda, but what the
            // user said there is written down (their conversation with it shows it).
            let entry = crate::history::Entry {
                kind: "request".into(),
                status: Some("answer".into()),
                text: words,
                source: Some(format!("terminal:{address}")),
                session: Some(address.to_string()),
                name: Some(name).filter(|n| !n.is_empty()),
                space,
                record: true,
                ..Default::default()
            };
            crate::history::later(move || crate::history::append(entry));
            return;
        }
        let r = self.agendas.typed(crate::agenda::Typed {
            session: address.to_string(),
            name: name.clone(),
            created_ms: crate::hc::created_ms(t),
            space: space.clone(),
            words: words.clone(),
            at_ms: ev.at_ms,
        });
        let entry = crate::history::Entry {
            kind: "request".into(),
            text: words,
            source: Some(format!("terminal:{address}")),
            session: Some(address.to_string()),
            name: Some(name).filter(|n| !n.is_empty()),
            space,
            agenda: Some(r.agenda),
            record: true,
            ..Default::default()
        };
        crate::history::later(move || crate::history::append(entry));
    }
}

/// A PR its session made was merged (`by`: mac, phone:<device>, github):
/// the session is queued that it was, so it can carry on, and the brain is
/// told (as a report on work it handed out: through the focus).
/// `tower validate`: a planned check of a PR, reported by its session (or
/// the PR's checks, with no check named). The PR is its key, its link, or
/// `#12` (the caller's own); one the tower doesn't know yet is looked up on
/// GitHub now as the caller's, as its turn's end would.
fn validate(prs: &crate::prs::Prs, pr: &str, check: &str, status: &str, evidence: &str, from: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let key = pr_key(prs, pr, from)?;
    let pr = if check.trim().is_empty() {
        prs.get(&key).ok_or_else(|| anyhow::anyhow!("no PR {key}"))?
    } else {
        let p = prs.report_check(&key, check, status, evidence, from.unwrap_or(""))?;
        eprintln!("[prs] {key}: check {check} {status} (by {})", from.unwrap_or("?"));
        p
    };
    Ok(serde_json::json!({ "pr": pr.key(), "validation": pr.validation, "line": pr.validation.as_ref().map(|v| v.line()) }))
}

/// `tower impact`: who depends on a PR's changes (with no verdict), or a
/// break found, or none, by compiling, type-checking or running the dependents' tests.
fn impact_report(prs: &crate::prs::Prs, pr: &str, verdict: &str, text: &str, from: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let key = pr_key(prs, pr, from)?;
    let pr = if verdict.trim().is_empty() {
        prs.get(&key).ok_or_else(|| anyhow::anyhow!("no PR {key}"))?
    } else {
        let p = prs.report_impact(&key, verdict, text, from.unwrap_or(""))?;
        eprintln!("[prs] {key}: impact {verdict} (by {}): {text}", from.unwrap_or("?"));
        p
    };
    Ok(serde_json::json!({ "pr": pr.key(), "impact": pr.impact, "line": pr.impact.as_ref().map(|i| i.line()), "breaks": pr.impact.as_ref().is_some_and(|i| i.breaks()) }))
}

/// A PR as a session names it (its key, its link, or `#12`: the caller's
/// own), as its key; one the tower doesn't know yet is looked up on GitHub
/// now as the caller's, as its turn's end would.
fn pr_key(prs: &crate::prs::Prs, pr: &str, from: Option<&str>) -> anyhow::Result<String> {
    let Some((repo, n)) = crate::reviews::pr_of(pr) else { anyhow::bail!("give the PR: its key (owner/repo#12), its link, or #12") };
    let key = match repo {
        Some(r) => crate::prs::key(&r, n),
        None => prs
            .list()
            .into_iter()
            .filter(|p| p.number == n && p.open())
            .min_by_key(|p| from.is_none_or(|f| p.session != f))
            .map(|p| p.key())
            .ok_or_else(|| anyhow::anyhow!("no open PR #{n} the tower knows: give its key (owner/repo#{n}) or its link"))?,
    };
    if prs.get(&key).is_none() {
        let caller = from.ok_or_else(|| anyhow::anyhow!("the tower doesn't know {key} yet: run this from the session that opened it"))?;
        let t = crate::hc::get(caller).unwrap_or_default();
        let (name, space) = (crate::hc::spoken_name(&t), crate::spaces::space_of(&t));
        let repo = key.rsplit_once('#').map(|(r, _)| r.to_string()).unwrap_or_default();
        if prs.found(&[(repo, n)], caller, &name, space.as_deref(), false).is_empty() {
            anyhow::bail!("{key} isn't an open PR GitHub knows");
        }
    }
    Ok(key)
}

/// `[prs] auto_merge_low_risk` as config.toml has it now.
fn auto_merge_on() -> bool {
    crate::config::Config::live().prs.auto_merge_low_risk
}

/// The PRs that wait for the user's merge, as decisions: those the tower
/// is on its way to merging itself (auto-merge on) wait for nobody.
fn sync_merges(decisions: &decisions::Decisions, prs: &crate::prs::Prs) {
    decisions.sync_prs(&crate::prscore::for_the_user(&prs.list(), auto_merge_on()));
}

/// A PR's review score when it is found: the prediction it is in the plan
/// its space was launched with (none: no plan predicted it), and its own
/// title and files at its head (GitHub asked). When GitHub can't say, the
/// prediction alone, to be read again before any merge. With it, the
/// validation the plan gives it (its checks, none reported yet, and its edge cases).
/// And who outside depends on what it changes (`crate::impact`: its diff,
/// and every checkout under `roots`), which makes it a breaking change
/// when anything uses what it removes or changes.
fn score_found(plans: &plans::Plans, gh: &std::path::Path, pr: &crate::prs::Pr, roots: &[std::path::PathBuf]) -> crate::prscore::Scored {
    let predicted = plans.match_pr(pr);
    let (files, sha) = match crate::prs::fetch_files(gh, &pr.repo, pr.number) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[prs] {}: couldn't read its files: {e:#}", pr.key());
            (vec![], String::new())
        }
    };
    let impact = match crate::impact::of_pr(gh, &pr.repo, pr.number, &sha, roots) {
        Ok(i) => {
            eprintln!("[prs] {}: {}", pr.key(), i.line());
            Some(i)
        }
        Err(e) => {
            eprintln!("[prs] {}: couldn't read its diff: {e:#}", pr.key());
            None
        }
    };
    let mut real = crate::prscore::real_score(&pr.title, &files);
    if let Some(i) = &impact {
        real = crate::prscore::union(&real, &i.score());
    }
    let mut s = crate::prscore::combine(predicted.as_ref().map(|(_, p)| &p.score), &real, &sha);
    let validation = predicted.as_ref().map(|(_, p)| p.validation());
    s.planned = predicted.map(|(id, p)| format!("{id}#{}", p.n));
    crate::prscore::Scored { score: Some(s), validation, impact }
}

fn after_merge(sendq: &sendq::SendQueue, watches: &watch::Watches, pr: &crate::prs::Pr, by: &str) {
    let (who, whom) = match by {
        "mac" => ("the user", " from the Mac"),
        b if b.starts_with("phone:") => ("the user", " from their phone"),
        "tower" => ("the tower", " itself (auto-merge is on: low risk, into dev)"),
        _ => ("someone", " on GitHub"),
    };
    let note = match who {
        "the user" => format!("The user merged PR #{} ({}) into {}.", pr.number, pr.repo, pr.base),
        "the tower" => format!("The tower merged PR #{} ({}) into {} itself: {}.", pr.number, pr.repo, pr.base, automerge::MERGED_WHY),
        _ => format!("PR #{} ({}) into {} was merged on GitHub.", pr.number, pr.repo, pr.base),
    };
    let queued = !pr.session.is_empty()
        && match sendq.add(&pr.session, &note, &[]) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[prs] couldn't tell {}: {e:#}", pr.session);
                false
            }
        };
    eprintln!("[prs] {} merged by {by}; {} {}", pr.key(), pr.session, if queued { "queued" } else { "not told" });
    let then = if queued {
        format!("I queued it \"{note}\", so it carries on from there.")
    } else {
        "Its session couldn't be told (it has ended or can't be reached).".to_string()
    };
    let msg = format!(
        "[watch] {} ({}): {who} merged {}{whom}. {then} Tell the user in one short line only if there's something more for them to do.",
        pr.session,
        pr.name,
        pr.short()
    );
    watches.news(&pr.session, &pr.name, pr.space.clone(), msg, format!("{} is merged.", pr.short()));
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
            Request::Say { text, interrupt, kind, from, brain: named } => {
                let answer = kind == "answer";
                if !text.trim().is_empty() {
                    timing::mark("text to say came in (tower say or the Stop hook)");
                }
                // Whose words they are: a space's own brain's hook names it,
                // `tower say` from a brain's terminal is that brain's, and
                // everything else goes by the main brain's turn.
                let from_brain = from.as_deref().and_then(brains::name_at);
                let who = named.filter(|b| crate::brains::is_space_brain(b)).or_else(|| from_brain.clone()).unwrap_or_else(brain::name);
                // The brain's final answer (its Stop hook) is usually heard
                // already, as it wrote it. (This reads its transcript, so the
                // turn's kind below is current.)
                let heard = !interrupt && self.narrator.already_said(&who, &text);
                let turn = brain::turn_of(&who);
                // A watch's news, or the brain telling one, waits its turn.
                let kind_word = kind.clone();
                let kind = if kind == "alert" { queue::Kind::Alert } else { narrate::turn_kind_of(&who) };
                let spoken = text::speakable(&text, self.cfg.max_spoken_chars);
                // The brain recommending answers to a session's questions: it stays quiet.
                let quiet = turn.question && kind_word != "alert";
                if quiet && !spoken.is_empty() {
                    eprintln!("[questions] the brain's [question] turn isn't spoken: {spoken:?}");
                }
                // A note turn at a quiet level: what the brain says by the way
                // (`tower say` from its own terminal) is not for the user
                // either. Its final words come from its Stop hook, which
                // knows the turn by its prompt.
                let for_record = kind_word.is_empty()
                    && turn.note
                    && crate::tell::level().quiet()
                    && (from_brain.is_some() || from.as_deref().and_then(crate::hc::get).is_some_and(|t| brain::brain_of(&t).is_some()));
                if for_record && !spoken.is_empty() {
                    eprintln!("[tell] a note turn's words aren't spoken: {spoken:?}");
                }
                let quiet = quiet || for_record;
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
                // The mute is the voice setting `off`; unmuting goes back to what it was.
                let to = match action.as_str() {
                    "on" => Some(crate::tell::Voice::Off),
                    "off" => Some(self.mouth.mode()),
                    "toggle" => Some(if self.mouth.muted() { self.mouth.mode() } else { crate::tell::Voice::Off }),
                    "status" | "" => None,
                    _ => return Response::err("mute is on, off, toggle or status"),
                };
                if let Some(voice) = to
                    && let Err(e) = self.set_voice(voice)
                {
                    return Response::err(format!("{e:#}"));
                }
            }
            Request::MacVoice { action } => {
                let on = match action.as_str() {
                    "on" => true,
                    "off" => false,
                    "toggle" => !self.mouth.on_mac(),
                    "status" | "" => self.mouth.on_mac(),
                    _ => return Response::err("mac-voice is on, off, toggle or status"),
                };
                if on != self.mouth.on_mac() {
                    // Kept first: what the tower says and what it keeps agree.
                    if let Err(e) = crate::phone::set_voice_on_mac(on) {
                        return Response::err(format!("keeping it in config.toml: {e:#}"));
                    }
                    self.mouth.set_on_mac(on);
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
            Request::Ask { text, space } => {
                timing::start("asked");
                // Asked in a space's view: its own brain's, when it has one that can take it.
                if let Err(e) = stt::deliver_to(&self.shared, &self.mouth, &text, ack_delay(&self.cfg), None, "mac", space.as_deref(), false) {
                    return Response::err(brain::spoken_error(&format!("{e:#}")));
                }
            }
            Request::Watch { terminal, note, account } => {
                return match self.watches.add(&terminal, &note, account) {
                    Ok(address) => Response { ok: true, error: None, state: Some(serde_json::json!({ "address": address })) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::Tell { level, voice } => return self.tell(level.as_deref(), voice.as_deref()),
            Request::Act { action, id, force, act, from } => {
                let ok = |v: serde_json::Value| Response { ok: true, error: None, state: Some(v) };
                return match action.as_str() {
                    "record" => match self.acts.record(&act.unwrap_or_default(), from.as_deref()) {
                        Ok(v) => ok(v),
                        Err(e) => Response::err(format!("{e:#}")),
                    },
                    "undo" => match self.undo(id.as_deref(), force, from.as_deref()) {
                        Ok(v) => ok(v),
                        Err(e) => Response::err(e),
                    },
                    "" | "list" => ok(self.acts.list()),
                    _ => Response::err("act is record, undo or list"),
                };
            }
            Request::Missions => return Response { ok: true, error: None, state: Some(self.missions.now()) },
            Request::Review { action, pr, critical, text, from } => {
                return match self.reviews.request(&action, &pr, critical, &text, from.as_deref()) {
                    Ok(v) => Response { ok: true, error: None, state: Some(v) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::Impact { pr, verdict, text, from } => {
                return match impact_report(&self.prs, &pr, &verdict, &text, from.as_deref()) {
                    Ok(v) => Response { ok: true, error: None, state: Some(v) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::Validate { pr, check, status, evidence, from } => {
                return match validate(&self.prs, &pr, &check, &status, &evidence, from.as_deref()) {
                    Ok(v) => Response { ok: true, error: None, state: Some(v) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::Raise { session, why, text, options, detail, from } => return self.raise(&session, &why, &text, &detail, options, from.as_deref()),
            Request::Receipt { text, agenda, from } => return self.receipt(&text, agenda.as_deref(), from.as_deref()),
            Request::Model { model, effort, restart } => return self.model(model.as_deref(), effort.as_deref(), restart),
            Request::Brains { action, space } => {
                let ok = |v: serde_json::Value| Response { ok: true, error: None, state: Some(v) };
                return match action.as_str() {
                    "close" if !space.trim().is_empty() => ok(serde_json::json!({ "ended": self.brains.close(space.trim()) })),
                    "restart" => ok(serde_json::json!({ "restarted": self.brains.restart() })),
                    "" | "list" => ok(serde_json::json!({ "brains": self.shared.with(|s| s.brains.clone()) })),
                    _ => Response::err("brains is list, close <space> or restart"),
                };
            }
            Request::Queue { action, session, text, id, after, start, tag, from } => {
                if action == "start" {
                    return match self.sendq.add_start(&session, &text, &start, &after) {
                        Ok(id) => {
                            let r = self.ledger(id, "", &session, &text, tag);
                            Response { ok: true, error: None, state: Some(with_agenda(serde_json::json!({ "id": id, "name": session }), r)) }
                        }
                        Err(e) => Response::err(format!("{e:#}")),
                    };
                }
                return self.queue(&action, &session, &text, id, &after, tag, from.as_deref());
            }
            Request::Agenda { ledger } => return self.agendas.request(&ledger),
            Request::MergeChain { action, id, card, prs, by } => {
                return match merges::get() {
                    Some(m) => match m.request(&action, &id, &card, &prs, if by.is_empty() { "mac" } else { &by }) {
                        Ok(v) => Response { ok: true, error: None, state: Some(v) },
                        Err(e) => Response::err(format!("{e:#}")),
                    },
                    None => Response::err("the tower isn't ready to merge yet"),
                };
            }
            Request::Reopen { action, spaces, dry_run } => {
                return match reopen::get() {
                    Some(r) => match r.request(&action, &spaces, dry_run) {
                        Ok(v) => Response { ok: true, error: None, state: Some(v) },
                        Err(e) => Response::err(format!("{e:#}")),
                    },
                    None => Response::err("the tower keeps nothing to reopen yet"),
                };
            }
            Request::Introduce { action, space, dry_run } => {
                if action == "kick" {
                    self.introducer.kick();
                    return Response { ok: true, error: None, state: None };
                }
                let space = if action == "status" { String::new() } else { space };
                return match self.introducer.request(&space, dry_run) {
                    Ok(v) => Response { ok: true, error: None, state: Some(v) },
                    Err(e) => Response::err(format!("{e:#}")),
                };
            }
            Request::Agent { event } => {
                // Only the terminal's own agent speaks for it (not a `claude -p` it runs).
                let terminal = crate::hc::get(&event.terminal);
                let foreground = terminal.as_ref().and_then(|t| t["foreground"]["pid"].as_u64());
                if !sessions::from_terminals_agent(&event, foreground) {
                    eprintln!("[sessions] {}: {} from another agent inside it (pid {}): not the terminal's", event.terminal, event.event, event.pid);
                    return Response { ok: true, error: None, state: None };
                }
                let change = sessions::registry().apply(&event);
                // A prompt beyond its first instruction: the act that opened it is no longer undone lightly.
                if change == sessions::Change::Prompted {
                    acts::given(&event.terminal, &event.text);
                    self.prompted(&event, terminal.as_ref());
                }
                if change != sessions::Change::Nothing {
                    self.watches.agent(&event, &change);
                    self.sendq.kick();
                    // A turn that ends is judged at once, before the next instruction follows it in.
                    self.agendas.kick();
                    // A new session: maybe someone joined a space.
                    if change == sessions::Change::Started {
                        self.introducer.kick();
                    }
                    // A space's sessions came or went, or all stopped: its mission may move.
                    if matches!(change, sessions::Change::Started | sessions::Change::Ended | sessions::Change::Stopped(_)) {
                        missions::kick();
                    }
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
            Request::Decisions { space, all } => {
                // As it stands now: a space may have been muted a moment ago, a PR merged on GitHub.
                self.prs.look_now();
                self.decisions.refresh();
                return Response { ok: true, error: None, state: Some(self.decisions.list(space.as_deref(), all)) };
            }
            // A decision's id: its question sets, its sessions or its task get the answer.
            Request::Answer { target, answers, submit } if crate::decisions::is_id(&target) => return self.answer_decision(&target, &answers, submit),
            Request::Dismiss { id } if crate::decisions::is_id(&id) => return self.dismiss_decision(&id),
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
            Request::BehaviorsChanged { by, change, from } => self.behaviors_changed(&by, &change, from.as_deref()),
            Request::PrMerged { pr, by } => {
                // GitHub says whether it is merged; only then is anyone told.
                return match self.prs.report_merged(&pr, &by) {
                    Some(_) => Response { ok: true, error: None, state: None },
                    None => Response::err(format!("{pr} isn't an open PR the tower knows, or GitHub doesn't say it's merged")),
                };
            }
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
            plan @ Request::Plan { .. } => return self.plans.request(&plan),
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
                    // A network was named in config.toml (`tower phone network name`, Settings), or a device renamed.
                    "renamed" => {
                        self.lan.renamed();
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
    /// the brains (each told to read its own instructions again, but the
    /// one that made the change: `from`, the terminal the command ran in).
    fn behaviors_changed(&self, by: &str, change: &serde_json::Value, from: Option<&str>) {
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
        // The main brain changing them from its own terminal says `by: brain` (an older caller names no terminal).
        let except = from.map(String::from).or_else(|| (by == "brain").then(brain::name));
        let _ = brains::reread(&format!("{} \"{}\"", change["op"].as_str().unwrap_or("changed"), change["title"].as_str().unwrap_or("")), except.as_deref());
    }
}

impl Daemon {
    /// `tower queue`: add (sent now when the session is free), list, cancel.
    /// The agenda ledger takes on what the queue just did.
    fn ledger(&self, queue_id: u64, session: &str, name: &str, text: &str, tag: Option<crate::agenda::Tag>) -> Option<crate::agenda::Registered> {
        self.agendas.register(crate::agenda::Reg { queue_id, session: session.into(), name: name.into(), text: text.into(), tag: tag.unwrap_or_default() })
    }

    #[allow(clippy::too_many_arguments)]
    fn queue(&self, action: &str, session: &str, text: &str, id: Option<u64>, after: &[u64], tag: Option<crate::agenda::Tag>, from: Option<&str>) -> Response {
        let ok = |v: serde_json::Value| Response { ok: true, error: None, state: Some(v) };
        match action {
            "retarget" => match id.map(|id| self.sendq.retarget(id, session)) {
                Some(Ok(item)) => ok(serde_json::json!({ "retargeted": item })),
                Some(Err(e)) => Response::err(format!("{e:#}")),
                None => Response::err("retarget needs an id and a session"),
            },
            "track" => match self.sendq.track(session, text) {
                Ok(id) => {
                    let name = crate::hc::get(session).map(|t| crate::hc::spoken_name(&t)).unwrap_or_default();
                    let r = self.ledger(id, session, &name, text, tag);
                    ok(with_agenda(serde_json::json!({ "id": id, "session": session }), r))
                }
                Err(e) => Response::err(format!("{e:#}")),
            },
            "add" => {
                // Who queued it: another agent session (not a brain, not the
                // user) writes a message, it doesn't hand out work; and so
                // does one brain writing to another (a space's own handing
                // the main one what crosses spaces).
                let caller = from.map(str::trim).filter(|f| !f.is_empty()).and_then(crate::hc::get);
                let receiver = crate::hc::get(session);
                let route = crate::peers::route_between(caller.as_ref(), receiver.as_ref(), crate::tell::level(), text);
                // A session writing to the tower reaches its own space's tower, when its space has one.
                let own = match &route {
                    crate::peers::Route::Message { from } => {
                        let is_main = |t: &serde_json::Value| crate::hc::name(t).is_some_and(|n| n == brain::NAME || n == brain::name());
                        let from_brain = caller.as_ref().is_some_and(|t| brain::brain_of(t).is_some());
                        brains::own_tower_of(from.space.as_deref(), from_brain, receiver.as_ref().is_some_and(is_main))
                    }
                    _ => None,
                };
                let (session, receiver) = match own {
                    Some(address) => {
                        eprintln!("[queue] a message for the tower from {}: to its space's own ({address})", caller.as_ref().and_then(|t| t["address"].as_str()).unwrap_or(""));
                        let t = crate::hc::get(&address);
                        (address, t)
                    }
                    None => (session.to_string(), receiver),
                };
                let session = session.as_str();
                let added = match &route {
                    crate::peers::Route::Message { from } => {
                        // To a brain it is a note from that session, not something the user said.
                        let to_brain = receiver.as_ref().is_some_and(|t| brain::brain_of(t).is_some());
                        let text = if to_brain { crate::peers::for_brain(from, text) } else { text.to_string() };
                        self.sendq.add_from(session, &text, after, from)
                    }
                    crate::peers::Route::Instruction { text } => {
                        let added = self.sendq.add(session, text, after);
                        // A brain passing something on after the user spoke to it: maybe their answer to the session's question.
                        if let (Ok((_, address, _)), Some(brain)) = (&added, caller.as_ref().and_then(brain::brain_of)) {
                            self.relayed(brain, address, text);
                        }
                        added
                    }
                };
                let (id, address, name, mut v) = match added {
                    Ok((sendq::Added::Sent { id }, address, name)) => {
                        let mut v = serde_json::json!({ "sent": true, "id": id, "session": address, "name": name });
                        // Typed in while it works (a message between sessions): it reads it when that turn ends.
                        if self.sendq.typed_while_working(id) {
                            v["while_working"] = true.into();
                        }
                        (id, address, name, v)
                    }
                    Ok((sendq::Added::Queued { id, position, status }, address, name)) => {
                        let v = serde_json::json!({ "sent": false, "id": id, "position": position, "status": status, "session": address, "name": name });
                        (id, address, name, v)
                    }
                    Err(e) => return Response::err(format!("{e:#}")),
                };
                match route {
                    // No task, no watch, no word to the brain: only the message.
                    crate::peers::Route::Message { from } => {
                        v["peer"] = true.into();
                        v["from"] = from.address.clone().into();
                        eprintln!("[queue] #{id} is a message from {} to {address}", from.address);
                        ok(v)
                    }
                    crate::peers::Route::Instruction { text } => {
                        // More work for a session the tower just opened: its act is no longer undone lightly.
                        acts::given(&address, &text);
                        // A session's own (while the tower tells everything, a task): who sent it, so it is no request of the user's.
                        let mut tag = tag.unwrap_or_default();
                        tag.from = caller.as_ref().and_then(crate::peers::sender_of).map(|s| s.address);
                        let r = self.ledger(id, &address, &name, &text, Some(tag));
                        ok(with_agenda(v, r))
                    }
                }
            }
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

impl Daemon {
    /// `tower tell`: how much the user is told. Kept in config.toml, shown
    /// in the state, and the brain reads its instructions again (they say
    /// what a note is at this level).
    fn tell(&self, level: Option<&str>, voice: Option<&str>) -> Response {
        let (was, voice_was) = (crate::tell::level(), self.mouth.voice());
        // Both words are read before either is kept: a bad one changes nothing.
        let parsed = (|| -> Result<_> { Ok((level.map(crate::tell::parse).transpose()?, voice.map(crate::tell::parse_voice).transpose()?)) })();
        let (level, voice) = match parsed {
            Ok(p) => p,
            Err(e) => return Response::err(format!("{e:#}")),
        };
        let mut v = serde_json::json!({ "level": was.word(), "voice": voice_was.word() });
        if let Some(voice) = voice {
            if let Err(e) = self.set_voice(voice) {
                return Response::err(format!("{e:#}"));
            }
            v["voice"] = voice.word().into();
            v["voice_was"] = voice_was.word().into();
        }
        if let Some(level) = level {
            if let Err(e) = crate::tell::set(level) {
                return Response::err(format!("keeping it in config.toml: {e:#}"));
            }
            self.shared.update(|s| s.tell_level = level.word().into());
            eprintln!("[tell] {was} -> {level}");
            // Told even when the level is the same: every brain's instructions were just written again.
            let _ = brains::reread(&format!("how much the tower tells the user is \"{level}\" now (it was \"{was}\")"), None);
            v["level"] = level.word().into();
            v["was"] = was.word().into();
        }
        Response { ok: true, error: None, state: Some(v) }
    }

    /// What the tower says out loud (`tower tell --voice`, `tower mute`):
    /// kept in config.toml first, so what it does and what it keeps agree.
    fn set_voice(&self, voice: crate::tell::Voice) -> Result<()> {
        let was = self.mouth.voice();
        crate::tell::set_voice(voice).context("keeping it in config.toml")?;
        self.mouth.set_voice(voice)?;
        if was != voice {
            eprintln!("[tell] voice: {was} -> {voice}");
        }
        Ok(())
    }

    /// `tower model`: the model and effort every brain is started on, kept
    /// in config.toml (`[brains]`). Nothing is typed into a running brain: a
    /// change applies when it starts, and `restart` restarts those that run.
    fn model(&self, model: Option<&str>, effort: Option<&str>, restart: bool) -> Response {
        let parsed = (|| -> Result<(Option<String>, Option<String>)> { Ok((model.map(crate::brains::parse_model).transpose()?, effort.map(crate::brains::parse_effort).transpose()?)) })();
        let (model, effort) = match parsed {
            Ok(p) => p,
            Err(e) => return Response::err(format!("{e:#}")),
        };
        if model.is_some() || effort.is_some() {
            if Config::load().is_ok_and(|c| c.brain == "codex") {
                return Response::err("the tower's brain is Codex here (brain = \"codex\"): its model comes from codex_model, codex_effort and codex_args in config.toml");
            }
            if let Err(e) = crate::brains::set(model.as_deref(), effort.as_deref()) {
                return Response::err(format!("keeping it in config.toml: {e:#}"));
            }
            eprintln!("[brains] the model is set: {} {}", model.as_deref().unwrap_or("(as it was)"), effort.as_deref().unwrap_or("(effort as it was)"));
        }
        let restarted = if restart { self.brains.restart() } else { vec![] };
        self.brains.publish();
        let mut v = self.brains.model();
        v["restarted"] = serde_json::json!(restarted);
        Response { ok: true, error: None, state: Some(v) }
    }

    /// `tower undo [<id>]`: ends what an act opened (see `crate::acts`),
    /// with this daemon's queue, ledger, watches and missions.
    /// `from`: the terminal the command ran in (none: a window, a phone).
    fn undo(&self, id: Option<&str>, force: bool, from: Option<&str>) -> Result<serde_json::Value, crate::acts::Refused> {
        let hands = acts::Real { sendq: &self.sendq, agendas: &self.agendas, watches: &self.watches, decisions: &self.decisions, missions: &self.missions };
        let v = self.acts.undo(id, force, &hands)?;
        // The brain that opened it hears what was undone, unless it ran the undo itself.
        let by = from.map(str::trim).filter(|f| !f.is_empty()).and_then(|f| brains::name_at(f).or_else(|| crate::hc::get(f).and_then(|t| brain::brain_of(&t).map(String::from))));
        // (A space's own brain that went with its space has nothing left to hear.)
        let went = v["space_closed"] == true && v["act"]["by"].as_str().is_some_and(crate::brains::is_space_brain);
        if let Some(note) = crate::acts::undone_note(&v["act"], by.as_deref()).filter(|_| !went) {
            eprintln!("[acts] telling the brain that opened it: {}", crate::agenda::clip(&note.msg, 160));
            self.watches.tell_agenda(note);
        }
        // The spaces and their sessions as they stand now.
        missions::kick();
        Ok(v)
    }

    /// `tower raise`: a decision for the user (`decisions`), kept in the
    /// history (kind `raised`). One decision per space and line: the first
    /// raise is shown and said once, as a session that needs the user is;
    /// the same thing raised again (or one the tower already shows: a merge,
    /// a session's own wait) joins that decision and says nothing, unless
    /// the tower tells everything.
    fn raise(&self, session: &str, why: &str, text: &str, detail: &str, options: Option<serde_json::Value>, from: Option<&str>) -> Response {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return Response::err("tower raise <session> --why \"<category>\" \"<the decision, one line>\"");
        }
        let options = options.filter(|o| !o.is_null());
        let choices = match options.as_ref().map(crate::decisions::options_from) {
            Some(Ok(o)) => o,
            Some(Err(e)) => return Response::err(format!("{e:#}")),
            None => vec![],
        };
        // A session that has ended can still be what a decision is about; nobody waits there.
        let t = crate::hc::get(session);
        let live = t.as_ref().is_some_and(|t| t["state"] == "live");
        let address = t.as_ref().and_then(|t| t["address"].as_str().map(String::from)).unwrap_or_else(|| session.to_string());
        let name = t.as_ref().map(crate::hc::spoken_name).unwrap_or_else(|| session.to_string());
        // What a space's own brain raises is that space's, whatever session it names.
        let space = from.and_then(brains::name_at).and_then(|b| brains::space_of(&b)).or_else(|| t.as_ref().and_then(crate::spaces::space_of));
        // It waits on this unless it works on something else meanwhile.
        let waiting = t.as_ref().is_none_or(|t| crate::hc::status(t) != crate::hc::SessionStatus::Working);
        let who = live.then(|| crate::decisions::Who { waiting, ..crate::decisions::Who::new(&address, &name) });
        let (raised, d, told) =
            self.decisions.raise(crate::decisions::Raise { space: space.clone(), who, why: why.trim().to_string(), text: text.clone(), detail: crate::hc::clean(detail), options: choices });
        let entry = crate::history::Entry {
            kind: "raised".into(),
            id: Some(raised.id.clone()),
            session: Some(address.clone()),
            name: Some(name.clone()),
            space: space.clone(),
            account: t.as_ref().and_then(crate::accounts::of_terminal),
            why: Some(why.trim().to_string()).filter(|w| !w.is_empty()),
            options,
            text: text.clone(),
            // Raised again: for the record only.
            record: !raised.fresh,
            ..Default::default()
        };
        crate::history::later(move || crate::history::append(entry));
        // What is asked, and what the tower would pick.
        let pick = d.pick().filter(|_| raised.fresh).map(|o| o.label.clone());
        let told = told && self.watches.raise(&address, &crate::decisions::said(&name, &text, pick.as_deref()));
        eprintln!(
            "[tell] raised for the user ({why}) about {address}: {text} ({}, {})",
            raised.id,
            if raised.fresh { "new" } else if raised.folded { "already in front of them" } else { "again" }
        );
        Response {
            ok: true,
            error: None,
            state: Some(serde_json::json!({
                "raised": true, "id": raised.id, "new": raised.fresh, "folded": raised.folded, "kind": d.kind.word(), "title": d.title, "detail": d.detail,
                "count": raised.count, "waiting": raised.waiting, "session": address, "name": name, "space": space, "why": why, "told": told,
            })),
        }
    }

    /// `tower answer <decision>` (see `decisions::Decisions::answer_with`),
    /// carried out with this daemon's question sets, queue and ledger. The
    /// Mac's cards and the phone come through here alike.
    fn decide(&self, id: &str, body: &serde_json::Value, submit: bool) -> Result<serde_json::Value, decisions::Refused> {
        let send = |session: &str, text: &str| -> Result<serde_json::Value> {
            let (added, address, name) = self.sendq.add(session, text, &[])?;
            let (queue_id, now) = match added {
                sendq::Added::Sent { id } => (id, true),
                sendq::Added::Queued { id, .. } => (id, false),
            };
            self.ledger(queue_id, &address, &name, text, None);
            Ok(serde_json::json!({ "session": address, "name": name, "queue_id": queue_id, "sent": now }))
        };
        let agenda = |c: &crate::agenda::Cmd| self.agendas.request(c);
        let reopen = |p: &crate::reopen::Pick| -> anyhow::Result<serde_json::Value> {
            match reopen::get() {
                Some(r) => r.answer(p),
                None => anyhow::bail!("the tower keeps nothing to reopen yet"),
            }
        };
        self.decisions.answer_with(id, body, submit, &decisions::Carry { asks: &self.asks, send: &send, agenda: &agenda, reopen: &reopen })
    }

    fn answer_decision(&self, id: &str, body: &serde_json::Value, submit: bool) -> Response {
        match self.decide(id, body, submit) {
            Ok(v) => Response { ok: true, error: None, state: Some(v) },
            Err(e) => Response::err(e),
        }
    }

    /// `tower dismiss <decision>`: out of the queue, with nothing sent. A
    /// question's sets are dismissed with it.
    fn dismiss_decision(&self, id: &str) -> Response {
        match self.decisions.dismiss_with(id, &self.asks) {
            Ok(v) => Response { ok: true, error: None, state: Some(v) },
            Err(e) => Response::err(e),
        }
    }

    /// `tower receipt`: the one line for a finished request. Shown and said
    /// when the user wants results (or follows its space); otherwise kept.
    fn receipt(&self, text: &str, agenda: Option<&str>, from: Option<&str>) -> Response {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return Response::err("tower receipt [--agenda <id>] \"<one line: what was done>\"");
        }
        // Its request, when named: finished, and not receipted before.
        // (Without one, the brain says it is for a request of the user's.)
        let (id, spaces, asked) = match agenda.map(str::trim).filter(|a| !a.is_empty()) {
            Some(a) => {
                let r = self.agendas.request(&crate::agenda::Cmd { action: "receipt".into(), target: a.into(), ..Default::default() });
                if !r.ok {
                    return r;
                }
                let v = r.state.unwrap_or_default();
                let spaces: Vec<String> = v["spaces"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default();
                (v["agenda"].as_str().map(String::from), spaces, v["asked"].as_bool().unwrap_or(true))
            }
            None => (None, vec![], true),
        };
        // A space's own brain's receipt is that space's, when its request names none.
        let mut spaces = spaces;
        if spaces.is_empty()
            && let Some(own) = from.and_then(brains::name_at).and_then(|b| brains::space_of(&b))
        {
            spaces.push(own);
        }
        let level = crate::tell::level();
        let followed = spaces.iter().any(|s| crate::spaces::followed(s));
        let told = crate::tell::receipt_told(level, asked, followed);
        let entry = crate::history::Entry {
            kind: "receipt".into(),
            text: text.clone(),
            agenda: id.clone(),
            space: spaces.first().cloned(),
            record: !told,
            ..Default::default()
        };
        crate::history::later(move || crate::history::append(entry));
        if told {
            let spoken = text::speakable(&text, self.cfg.max_spoken_chars);
            if !spoken.is_empty() {
                // News the user asked to hear: after the bell, in its turn.
                self.mouth.receipt(spoken);
            }
        }
        eprintln!("[tell] receipt{} ({}): {text}", id.as_deref().map(|i| format!(" for {i}")).unwrap_or_default(), if told { "told" } else { "kept" });
        Response { ok: true, error: None, state: Some(serde_json::json!({ "told": told, "agenda": id, "spaces": spaces, "level": level.word() })) }
    }
}

/// An answer with where its task went in the agenda ledger.
fn with_agenda(mut v: serde_json::Value, r: Option<crate::agenda::Registered>) -> serde_json::Value {
    if let Some(r) = r {
        v["agenda"] = r.agenda.into();
        v["task"] = r.task.into();
        v["agenda_how"] = r.how.into();
    }
    v
}

fn ack_delay(cfg: &Config) -> Option<Duration> {
    (!cfg.ack.is_empty()).then(|| Duration::from_millis(cfg.ack_delay_ms))
}
