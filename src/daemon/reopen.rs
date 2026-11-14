//! What a restart closed, in the daemon (see `crate::reopen`): kept with
//! every look of the missions, offered back on one card once Herdr's
//! window answers, and put back when the user says so (the card's answer,
//! `tower reopen go`).
//!
//! - The missions' look hands over every live session of a space (`look`);
//!   the first look of a tower that kept nothing yet seeds the book from
//!   the hooks' sessions.
//! - Lost sessions are offered once per start of Herdr's core: one card
//!   (`Decisions::offer_reopen`), changed in place as sessions come back by
//!   hand, stopped when none is left. Dismissed, it isn't raised again
//!   until the next restart; `tower reopen` lists them all the same.
//! - Putting them back (`put_back`) runs Herdr's steps (`Real`), marks each
//!   one reopened, writes a line per space into the record, and starts the
//!   tower's own brain again when it is down (never in a test daemon).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::state::lock;
use crate::history::{self, Entry};
use crate::reopen::{self, Book, Kept, Pick};

#[derive(Clone)]
pub struct Reopen {
    /// None until the first look (which seeds it when there was no file).
    book: Arc<Mutex<Option<Book>>>,
    path: PathBuf,
    decisions: super::decisions::Decisions,
    /// One putting back at a time.
    busy: Arc<Mutex<()>>,
}

static RUNNING: OnceLock<Reopen> = OnceLock::new();

/// The daemon's, once it runs (None in a command, and in tests).
pub fn get() -> Option<&'static Reopen> {
    RUNNING.get()
}

/// When Herdr's core started (see `hc::core_since_ms`). A test daemon may
/// be told another start (`HERDER_TOWER_CORE_SINCE`: a file holding Unix
/// ms), to try a restart without making one.
fn core_ms() -> Option<u64> {
    if super::watch::test_daemon()
        && let Some(file) = std::env::var_os("HERDER_TOWER_CORE_SINCE").filter(|f| !f.is_empty())
        && let Some(ms) = std::fs::read_to_string(file).ok().and_then(|t| t.trim().parse::<u64>().ok())
    {
        return Some(ms);
    }
    crate::hc::core_since_ms()
}

/// Whether a space is over for the tower (closed or archived): what was in it isn't offered back.
fn over(space: &str) -> bool {
    let s = crate::spaces::load_from(&crate::config::ui_dir());
    s.missions.iter().any(|(n, m)| n.eq_ignore_ascii_case(space) && (m.state == crate::missions::State::Closed || m.archived_ms.is_some()))
}

/// The tower's own brain isn't running (a test daemon never starts the real one).
fn brain_down() -> bool {
    !super::watch::test_daemon() && !crate::hc::is_live(crate::brain::NAME)
}

impl Reopen {
    pub fn open(decisions: super::decisions::Decisions) -> Reopen {
        Reopen::at(reopen::path(), decisions)
    }

    pub fn at(path: PathBuf, decisions: super::decisions::Decisions) -> Reopen {
        let book = reopen::load_from(&path);
        Reopen { book: Arc::new(Mutex::new(book)), path, decisions, busy: Arc::default() }
    }

    pub fn start(&self) {
        let _ = RUNNING.set(self.clone());
    }

    fn save(&self, b: &Book) {
        if let Err(e) = reopen::save_to(&self.path, b) {
            eprintln!("[reopen] {}: {e:#}", self.path.display());
        }
    }

    /// After a look of the missions that reached Herdr: `seen` is every
    /// live session of a space, `get` a terminal's `hc get`.
    pub fn look(&self, seen: &[crate::missions::Seen], get: &dyn Fn(&str) -> Option<Value>) {
        let Some(core) = core_ms() else { return };
        let now = history::now_ms();
        let registry = super::sessions::registry();
        let live: Vec<Kept> = seen
            .iter()
            .filter_map(|x| {
                let t = get(&x.address)?;
                reopen::kept_of(x, &t, registry.get(&x.address).as_ref(), core, now)
            })
            .collect();
        let mut guard = lock(&self.book);
        let mut changed = guard.is_none();
        let book = guard.get_or_insert_with(|| {
            let seeded = reopen::seed(&registry.all(), &|a| crate::hc::get(a), core, now, &over, &|t| super::watch::left_alone(t));
            if !seeded.is_empty() {
                eprintln!(
                    "[reopen] first look: {} session(s) from before Herdr's core started are taken as lost: {}",
                    seeded.len(),
                    seeded.iter().map(|k| format!("{} in {}", k.called(), k.space)).collect::<Vec<_>>().join(", ")
                );
            }
            Book { sessions: seeded, ..Default::default() }
        });
        changed |= reopen::follow(book, live, core, now);
        // A space's folder, to make it again: once, from Herdr's window.
        let unknown = book.sessions.iter().any(|k| !k.before(core) && !book.folders.contains_key(&k.space));
        let mut windows: Option<Vec<crate::spaces::Window>> = None;
        if unknown {
            windows = crate::spaces::windows_open();
            for w in windows.iter().flatten().filter(|w| w.machine.is_none() && !w.folder.is_empty()) {
                if book.sessions.iter().any(|k| k.space == w.name) && book.folders.get(&w.name) != Some(&w.folder) {
                    book.folders.insert(w.name.clone(), w.folder.clone());
                    changed = true;
                }
            }
        }
        changed |= self.offer(book, core, now, windows);
        if changed {
            self.save(book);
        }
    }

    /// The card, as what is lost stands now. Whether the book changed.
    fn offer(&self, book: &mut Book, core: u64, now: u64, windows: Option<Vec<crate::spaces::Window>>) -> bool {
        let open = book.decision.as_deref().and_then(|id| self.decisions.get(id)).is_some_and(|d| d.closed_ms.is_none());
        let mut changed = false;
        if !open && book.decision.take().is_some() {
            changed = true;
        }
        let lost = reopen::lost(book, core, now);
        if lost.is_empty() {
            if open {
                self.decisions.offer_reopen(None);
                eprintln!("[reopen] nothing a restart closed is left: the card stops");
            }
            return changed;
        }
        // Offered once per start of the core (dismissed: not again until the next one).
        let due = open || book.offered_core_ms != core;
        if !due {
            return changed;
        }
        // Not before Herdr's window answers: what comes back is put there.
        if windows.is_none() && crate::spaces::windows_open().is_none() {
            return changed;
        }
        let offer = reopen::offer(&lost, brain_down());
        if let Some((id, _)) = self.decisions.offer_reopen(offer)
            && (book.decision.as_deref() != Some(id.as_str()) || book.offered_core_ms != core)
        {
            eprintln!("[reopen] {id}: {} session(s) lost to a restart offered back", lost.len());
            (book.decision, book.offered_core_ms) = (Some(id), core);
            changed = true;
        }
        changed
    }

    /// `tower reopen`: `list` (what is lost), `go` (put back all, or the
    /// `spaces` named; `dry_run`: only how), `forget` (let them go).
    pub fn request(&self, action: &str, spaces: &[String], dry_run: bool) -> Result<Value> {
        let pick = if spaces.is_empty() { Pick::All } else { Pick::Spaces(spaces.to_vec()) };
        match action {
            "" | "list" | "ls" => {
                let core = core_ms().context("hc doesn't say when its core started")?;
                let guard = lock(&self.book);
                let empty = Book::default();
                Ok(reopen::listing(guard.as_ref().unwrap_or(&empty), core, history::now_ms()))
            }
            "go" | "reopen" | "resume" => self.put_back(&pick, dry_run),
            "forget" if spaces.is_empty() => self.put_back(&Pick::Forget, dry_run),
            "forget" => self.forget_spaces(spaces),
            other => bail!("tower reopen {other}: use list, go [<space>…] or forget [<space>…]"),
        }
    }

    fn forget_spaces(&self, spaces: &[String]) -> Result<Value> {
        let core = core_ms().context("hc doesn't say when its core started")?;
        let now = history::now_ms();
        let mut guard = lock(&self.book);
        let book = guard.get_or_insert_with(Book::default);
        let lost: Vec<String> = reopen::chosen(&reopen::lost(book, core, now), &Pick::Spaces(spaces.to_vec()))?.iter().map(|k| k.session_id.clone()).collect();
        book.sessions.retain(|k| !lost.contains(&k.session_id));
        self.save(book);
        Ok(json!({ "forgot": lost.len() }))
    }

    /// The card's answer (see `decisions::Act::Reopen`).
    pub fn answer(&self, pick: &Pick) -> Result<Value> {
        self.put_back(pick, false)
    }

    /// Puts back what `pick` takes of what a restart closed (or, for
    /// `Forget`, lets it go). What was done, session by session.
    fn put_back(&self, pick: &Pick, dry_run: bool) -> Result<Value> {
        let _one = lock(&self.busy);
        let core = core_ms().context("hc doesn't say when its core started")?;
        let now = history::now_ms();
        let (chosen, folders) = {
            let mut guard = lock(&self.book);
            let book = guard.get_or_insert_with(Book::default);
            let lost = reopen::lost(book, core, now);
            if lost.is_empty() {
                bail!("nothing a restart closed is waiting to be reopened");
            }
            let chosen: Vec<Kept> = reopen::chosen(&lost, pick)?.into_iter().cloned().collect();
            if *pick == Pick::Forget {
                if !dry_run {
                    book.sessions.retain(|k| !chosen.iter().any(|c| c.session_id == k.session_id));
                    self.save(book);
                    eprintln!("[reopen] the user let go of {} session(s) a restart closed", chosen.len());
                }
                return Ok(json!({ "forgot": chosen.len(), "dry_run": dry_run }));
            }
            (chosen, book.folders.clone())
        };
        let windows = crate::spaces::windows_open().context("Herdr's window doesn't answer: open Herdr, then reopen them")?;
        let cca = crate::accounts::cca_bin().map(|p| p.to_string_lossy().into_owned());
        let default_account = crate::accounts::list().ok().and_then(|l| crate::accounts::default_of(&l));
        let refs: Vec<&Kept> = chosen.iter().collect();
        let steps = reopen::plan(&refs, &windows, &folders, &|a| crate::hc::get(a), &|p| Path::new(p).exists(), cca.as_deref(), default_account.as_deref());
        if dry_run {
            let rows: Vec<Value> = steps
                .iter()
                .map(|(k, s)| {
                    let how = match s {
                        Ok(reopen::Step::Type { address, line }) => json!({ "how": "typed", "terminal": address, "line": line }),
                        Ok(reopen::Step::Tab { tab, folder, argv, .. }) => json!({ "how": "tab", "tab": tab, "folder": folder, "argv": argv }),
                        Ok(reopen::Step::Space { name, folder, tab, argv, home }) => json!({ "how": "space", "space": name, "folder": folder, "tab": tab, "argv": argv, "home": home }),
                        Err(why) => json!({ "how": "failed", "why": why }),
                    };
                    json!({ "space": k.space, "name": k.called(), "step": how })
                })
                .collect();
            return Ok(json!({ "dry_run": true, "steps": rows, "brain": brain_down().then_some("would start") }));
        }
        let was_shown = windows.iter().find(|w| w.active).map(|w| w.id.clone());
        let done = reopen::run(steps, &Real);
        // The window shows what it showed.
        if let Some(id) = was_shown
            && done.iter().any(|d| d.how == "tab" || d.how == "space")
        {
            let _ = std::process::Command::new(crate::config::herdr_app_bin()).args(["do", "space.select", "--id", &id]).stdin(std::process::Stdio::null()).output();
        }
        {
            let mut guard = lock(&self.book);
            if let Some(book) = guard.as_mut() {
                for d in done.iter().filter(|d| d.how != "failed") {
                    if let Some(k) = book.sessions.iter_mut().find(|k| k.session_id == d.session_id) {
                        k.reopened_ms = Some(now);
                        k.address = d.address.clone();
                    }
                }
                self.save(book);
            }
        }
        for sp in reopen::spaces_of(&chosen.iter().collect::<Vec<_>>()) {
            let back: Vec<&str> = done.iter().filter(|d| d.space == sp && d.how != "failed").map(|d| d.name.as_str()).collect();
            let failed: Vec<String> = done.iter().filter(|d| d.space == sp && d.how == "failed").map(|d| format!("{} ({})", d.name, d.why)).collect();
            let mut text = String::new();
            if !back.is_empty() {
                text = format!("Reopened after the restart: {} (each resumed its own conversation).", back.join(", "));
            }
            if !failed.is_empty() {
                text = format!("{text}{}Not reopened: {}.", if text.is_empty() { "" } else { " " }, failed.join("; "));
            }
            eprintln!("[reopen] {sp}: {text}");
            if !cfg!(test) {
                let entry = Entry { kind: "space".into(), space: Some(sp.clone()), status: Some("reopened".into()), text, record: true, ..Default::default() };
                history::later(move || history::append(entry));
            }
        }
        let back = done.iter().filter(|d| d.how != "failed").count();
        // The tower's brain, which the spaces' own wait for.
        let brain = if back > 0 && brain_down() {
            std::thread::spawn(|| {
                let cfg = crate::config::Config::live();
                match crate::brain::launch(&cfg, &cfg.brain) {
                    Ok(address) => eprintln!("[reopen] the tower's brain runs again as {address}"),
                    Err(e) => eprintln!("[reopen] the tower's brain didn't start: {e:#}"),
                }
                super::missions::kick();
            });
            Some("starting")
        } else {
            None
        };
        super::missions::kick();
        if back == 0 {
            bail!("none came back: {}", done.iter().map(|d| format!("{}: {}", d.name, d.why)).collect::<Vec<_>>().join("; "));
        }
        Ok(json!({ "reopened": done, "brain": brain }))
    }
}

/// Herdr, for real: `hc send` into a kept shell, `herdr-app do tab.new` / `space.new`.
struct Real;

/// A terminal's address from `herdr-app do …`'s answer.
fn made(out: std::io::Result<std::process::Output>, what: &str) -> Result<String> {
    let out = out.with_context(|| format!("herdr-app {what}"))?;
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    match v["terminal"].as_str() {
        Some(t) => Ok(t.to_string()),
        None => bail!("herdr-app {what}: {}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim()),
    }
}

impl reopen::Hands for Real {
    fn type_line(&self, address: &str, line: &str) -> Result<()> {
        let out = crate::hc::run(&["send", address, line])?;
        if out.code != 0 {
            bail!("hc send: {}{}", out.stderr.trim(), out.stdout.trim());
        }
        Ok(())
    }

    fn tab(&self, space_id: &str, tab: &str, folder: &str, argv: &[String]) -> Result<String> {
        let argv = serde_json::to_string(argv)?;
        let mut args = vec!["do", "tab.new", "--space", space_id, "--name", tab, "--argv", &argv];
        if !folder.is_empty() {
            args.extend(["--folder", folder]);
        }
        made(std::process::Command::new(crate::config::herdr_app_bin()).args(&args).stdin(std::process::Stdio::null()).output(), "tab.new")
    }

    fn space(&self, name: &str, folder: &str, tab: &str, argv: &[String], home: Option<&str>) -> Result<String> {
        // A home tab first (a shell beside its worktrees), then the session as a tab.
        if let Some(home) = home {
            let args = ["do", "space.new", "--name", name, "--tab", name, "--folder", home];
            let t = made(std::process::Command::new(crate::config::herdr_app_bin()).args(args).stdin(std::process::Stdio::null()).output(), "space.new")?;
            let _ = crate::hc::run(&["label", &t, crate::worktrees::LABEL_HOME, &format!("tower.name={name}")]);
            let Some(id) = self.space_id(name) else { bail!("herdr-app made the space {name}, but its window doesn't list it") };
            return self.tab(&id, tab, folder, argv);
        }
        let argv = serde_json::to_string(argv)?;
        let mut args = vec!["do", "space.new", "--name", name, "--tab", tab, "--argv", &argv];
        if !folder.is_empty() {
            args.extend(["--folder", folder]);
        }
        made(std::process::Command::new(crate::config::herdr_app_bin()).args(&args).stdin(std::process::Stdio::null()).output(), "space.new")
    }

    fn space_id(&self, name: &str) -> Option<String> {
        crate::spaces::windows_open()?.into_iter().find(|w| w.name.eq_ignore_ascii_case(name)).map(|w| w.id)
    }

    fn label(&self, address: &str, labels: &[String]) {
        let mut args = vec!["label", address];
        args.extend(labels.iter().map(String::as_str));
        let _ = crate::hc::run(&args);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::{Shared, State};

    fn kept(space: &str, name: &str, core_ms: u64) -> Kept {
        Kept { space: space.into(), name: name.into(), tab: name.into(), session_id: format!("s-{name}"), app: "claude-code".into(), core_ms, seen_ms: 1, ..Default::default() }
    }

    #[test]
    fn what_a_restart_closed_is_one_card_that_follows_it_and_goes_when_it_is_back() {
        let dir = std::env::temp_dir().join(format!("tower-reopen-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let shared = Shared::new(State::new("ptt", "test", "test"));
        let decisions = super::super::decisions::Decisions::at(shared.clone(), dir.join("decisions.json"));
        let r = Reopen::at(dir.join("open.json"), decisions.clone());
        let core = 10_000_000;
        let mut book = Book { sessions: vec![kept("sandbox", "a", 0), kept("sandbox", "b", 0), kept("zendesk", "c", 0)], ..Default::default() };
        let windows = Some(vec![]);
        assert!(r.offer(&mut book, core, core + 1, windows.clone()));
        let id = book.decision.clone().unwrap();
        let d = decisions.get(&id).unwrap();
        assert_eq!((d.kind, d.why.as_str(), d.space.as_deref()), (crate::decisions::Kind::Choice, "reopen", None));
        assert_eq!(d.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Reopen all", "Only sandbox", "Only zendesk", "Forget them"]);
        assert!(d.sessions.is_empty(), "it names no session: one that's gone doesn't close it");
        // c comes back by hand: the same card lists the rest.
        book.sessions[2].core_ms = core;
        r.offer(&mut book, core, core + 2, windows.clone());
        let d = decisions.get(&id).unwrap();
        assert_eq!(book.decision.as_deref(), Some(id.as_str()));
        assert!(d.title.starts_with("2 sessions in sandbox ") && d.options.len() == 2, "{}", d.title);
        // Answering it: the tower does it, nothing is sent; what was chosen is its answer.
        let picked = Mutex::new(None);
        let reopen = |p: &Pick| -> Result<Value> {
            *picked.lock().unwrap() = Some(p.clone());
            Ok(json!({ "reopened": [] }))
        };
        let asks = super::super::asks::Asks::new(shared, Arc::new(crate::daemon::lan::hub::Hub::default()), "normal").stored_at(dir.join("questions.json"));
        let send = |_: &str, _: &str| -> Result<Value> { panic!("nothing is sent to a session") };
        let agenda = |_: &crate::agenda::Cmd| crate::protocol::Response { ok: true, error: None, state: None };
        let accounts = |_: bool| -> Result<Value> { panic!("no account card here") };
        let plan = |_: &str, _: &str, _: &str| -> Result<Value> { panic!("no plan card here") };
        let carry = super::super::decisions::Carry { asks: &asks, send: &send, agenda: &agenda, reopen: &reopen, accounts: &accounts, plan: &plan };
        let v = decisions.answer_with(&id, &json!({ "choice": 0 }), true, &carry).unwrap();
        assert_eq!((v["state"].as_str(), v["answer"].as_str()), (Some("answered"), Some("Reopen all")));
        assert_eq!(picked.lock().unwrap().clone(), Some(Pick::All));
        assert_eq!(crate::decisions::rule_for(crate::decisions::Kind::Choice, "reopen", &d.title, None, "Reopen all"), None, "never made a rule");
        // Answered (or dismissed): not offered again under this core.
        r.offer(&mut book, core, core + 3, windows.clone());
        assert_eq!(book.decision, None);
        assert_eq!(decisions.list(None, false)["count"], 0);
        // The next restart offers what is still lost.
        r.offer(&mut book, core + 5_000_000, core + 5_000_001, windows.clone());
        assert!(book.decision.is_some());
        // Nothing left: the card stops.
        for k in book.sessions.iter_mut() {
            k.core_ms = core + 5_000_000;
        }
        r.offer(&mut book, core + 5_000_000, core + 5_000_002, windows);
        assert_eq!(decisions.list(None, false)["count"], 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
