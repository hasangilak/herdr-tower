//! What was open, kept across a restart of the Mac (or of Herdr's core),
//! and putting it back.
//!
//! - **Kept** (`open.json` in the state folder): every live agent session
//!   in a space, as the missions' look sees it every 20 seconds: its space
//!   and tab, its folder, its account, its program, its conversation (the
//!   session id its hooks gave, kept on its terminal as `tower.session`)
//!   and what the tower gave it (its `tower.*` labels: a worktree, a port).
//! - **Lost, or ended.** A session missing from a look was lost or ended.
//!   Herdr's core started again since it was last seen (the Mac
//!   restarted, the core was stopped): it was lost, and stays to be
//!   offered back (for `KEEP_LOST_MS`). The same core still runs: it ended
//!   while everything stayed up (the user quit it, its space was closed),
//!   and is forgotten once it has been gone `GRACE_MS`. A shutdown takes
//!   the daemon down well before that.
//! - **The first look** of a tower that keeps nothing yet (`seed`): the
//!   sessions the hooks told of before this core started, in a terminal
//!   Herdr kept as a shell, are taken as lost.
//! - **The offer** is one card (see `daemon::reopen`), raised once Herdr's
//!   window answers again: reopen them all, only one space's, or forget
//!   them. Dismissing it is "not now": `tower reopen` still has them.
//! - **Putting it back** (`plan`, `run`): each session's own conversation,
//!   resumed (`cca <account> -n <name> --resume <id>`, `codex resume <id>`)
//!   where it was: typed into its own terminal when Herdr kept that one as
//!   a shell at its prompt, else in a new tab of its space, else in a new
//!   space of that name. Its `tower.*` labels go with it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config;
use crate::daemon::sessions::AgentSession;
use crate::missions::Seen;
use crate::spaces::Window;

/// A session that ended while everything stayed up is forgotten this long after.
pub const GRACE_MS: u64 = 3 * 60_000;
/// A lost session is offered back for this long.
pub const KEEP_LOST_MS: u64 = 7 * 24 * 3600 * 1000;
/// Herdr's core start, as read twice, may differ by this much.
const SLACK_MS: u64 = 30_000;
/// A live session's `seen_ms` is written down at most this often.
const SEEN_EVERY_MS: u64 = 5 * 60_000;
/// A session the tower put back has this long to be seen again.
pub const REOPEN_WAIT_MS: u64 = 2 * 60_000;
/// The label that ties a terminal to its agent's conversation.
pub const LABEL_SESSION: &str = "tower.session";
/// The shells a kept terminal comes back as.
const SHELLS: [&str; 5] = ["zsh", "bash", "sh", "fish", "dash"];

/// A session as it was last seen.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Kept {
    /// Its conversation: what is resumed.
    pub session_id: String,
    /// `claude-code` or `codex`.
    pub app: String,
    pub space: String,
    /// Its tab's name in the space.
    pub tab: String,
    /// Its terminal then.
    pub address: String,
    pub name: String,
    /// The folder it worked in.
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// What its program was started with that a resume keeps (`--model …`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    /// The tower's labels on its terminal (`tower.name`, `tower.worktree`, …), but its session.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Its conversation's file, when its hooks said.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub transcript: String,
    /// Its last words, short.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub said: String,
    pub seen_ms: u64,
    /// When Herdr's core it was seen under started (0: one before any kept).
    pub core_ms: u64,
    /// First missed while the same core ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gone_ms: Option<u64>,
    /// When the tower put it back, until it is seen again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reopened_ms: Option<u64>,
}

impl Kept {
    /// How it is called: its name, else its tab's, else its terminal.
    pub fn called(&self) -> &str {
        [self.name.as_str(), self.tab.as_str(), self.address.as_str()].into_iter().find(|s| !s.trim().is_empty()).unwrap_or("a session")
    }

    /// It was seen under an older core than `core_ms`: a restart came between.
    pub fn before(&self, core_ms: u64) -> bool {
        self.core_ms.saturating_add(SLACK_MS) < core_ms
    }
}

/// `open.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Book {
    pub sessions: Vec<Kept>,
    /// Each space's folder in Herdr, to make it again.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub folders: BTreeMap<String, String>,
    /// The core (its start) whose losses were offered: once per restart.
    pub offered_core_ms: u64,
    /// The card that offers them, while it is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
}

pub fn path() -> PathBuf {
    config::state_dir().join("open.json")
}

/// The book (None: there is none yet, so the first look seeds it).
pub fn load_from(path: &Path) -> Option<Book> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(serde_json::from_str(&text).unwrap_or_default())
}

pub fn save_to(path: &Path, b: &Book) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(b)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---- what is seen ----

fn label(t: &Value, key: &str) -> Option<String> {
    t["labels"][key].as_str().map(str::trim).filter(|v| !v.is_empty()).map(String::from)
}

/// "space / tab" → the tab.
fn tab_of(t: &Value) -> String {
    let place = t["labels"]["where"].as_str().or_else(|| t["where"].as_str()).unwrap_or("");
    place.split_once(" / ").map(|(_, tab)| tab.trim().to_string()).unwrap_or_default()
}

/// The tower's own labels on a terminal, to carry over (not its session:
/// a resumed one labels itself; not the test mark, which `test` says).
fn tower_labels(t: &Value) -> BTreeMap<String, String> {
    t["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.starts_with("tower.") && *k != LABEL_SESSION)
        .filter_map(|(k, v)| v.as_str().filter(|v| !v.is_empty()).map(|v| (k.clone(), v.to_string())))
        .collect()
}

/// What a Claude Code command line was started with that its resume keeps.
pub fn flags_of(argv: &[String]) -> Vec<String> {
    let program = argv.first().map(|p| p.rsplit('/').next().unwrap_or(p)).unwrap_or("");
    if program != "claude" {
        return vec![];
    }
    const WITH_VALUE: [&str; 5] = ["--model", "--effort", "--permission-mode", "--add-dir", "--settings"];
    const ALONE: [&str; 1] = ["--dangerously-skip-permissions"];
    let mut out = vec![];
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        let key = a.split_once('=').map_or(a, |(k, _)| k);
        if WITH_VALUE.contains(&key) {
            if a.contains('=') {
                out.push(a.to_string());
            } else if let Some(v) = argv.get(i + 1) {
                out.extend([a.to_string(), v.clone()]);
                i += 1;
            }
        } else if ALONE.contains(&a) {
            out.push(a.to_string());
        }
        i += 1;
    }
    out
}

/// A session of a space, live now (`x`, with its terminal `t` as `hc get`
/// has it, and what its hooks said), as it is kept. None for what isn't
/// put back: the tower's planner and reviewers (the tower starts those
/// itself), sessions on another machine, and those whose conversation
/// isn't known.
pub fn kept_of(x: &Seen, t: &Value, hooks: Option<&AgentSession>, core_ms: u64, now: u64) -> Option<Kept> {
    if x.address.contains('/') || t["machine"].is_string() || label(t, "tower.role").is_some() {
        return None;
    }
    let hooks = hooks.filter(|h| h.terminal == x.address && !h.session_id.is_empty());
    let app = crate::hc::agent_app(t).or_else(|| hooks.map(|h| h.app.clone()))?;
    if app != "claude-code" && app != "codex" {
        return None;
    }
    let session_id = label(t, LABEL_SESSION).or_else(|| hooks.map(|h| h.session_id.clone()))?;
    let cwd = t["foreground"]["cwd"].as_str().or(t["cwd"].as_str()).map(String::from).or_else(|| hooks.map(|h| h.cwd.clone())).unwrap_or_default();
    let argv: Vec<String> = t["foreground"]["argv"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    Some(Kept {
        transcript: hooks.filter(|h| h.session_id == session_id).map(|h| h.transcript.clone()).unwrap_or_default(),
        session_id,
        app,
        space: x.space.clone(),
        tab: tab_of(t),
        address: x.address.clone(),
        // Its own name (`-n`), not the one the tower says.
        name: crate::hc::name(t).unwrap_or("").to_string(),
        cwd,
        account: x.account.clone(),
        flags: flags_of(&argv),
        labels: tower_labels(t),
        said: x.said.clone().unwrap_or_default(),
        seen_ms: now,
        core_ms,
        gone_ms: None,
        reopened_ms: None,
    })
}

/// A terminal Herdr kept as a shell at its prompt (its program was lost):
/// live, and its foreground is its own shell.
pub fn kept_shell(t: &Value) -> bool {
    let fg = &t["foreground"];
    let program = fg["argv"][0].as_str().unwrap_or("").rsplit('/').next().unwrap_or("").trim_start_matches('-');
    t["state"] == "live" && SHELLS.contains(&program) && (fg["pid"].is_null() || fg["pid"] == t["pid"])
}

/// The first look of a tower that keeps nothing yet: the sessions its hooks
/// told of before this core started (`core_ms`), whose terminal Herdr kept
/// as a shell, still tied to that conversation, in a space that isn't over
/// (`over`). They are taken as lost. Not for a test daemon's sessions
/// (`left_alone`: the other tower's).
pub fn seed(hooks: &[AgentSession], get: &dyn Fn(&str) -> Option<Value>, core_ms: u64, now: u64, over: &dyn Fn(&str) -> bool, left_alone: &dyn Fn(&Value) -> bool) -> Vec<Kept> {
    let mut out: Vec<Kept> = vec![];
    for h in hooks {
        let recent = h.event_ms < core_ms && now.saturating_sub(h.event_ms) < KEEP_LOST_MS;
        if h.session_id.is_empty() || h.terminal.contains('/') || !recent || !matches!(h.app.as_str(), "claude-code" | "codex") {
            continue;
        }
        let Some(t) = get(&h.terminal) else { continue };
        if !kept_shell(&t) || label(&t, LABEL_SESSION).as_deref() != Some(h.session_id.as_str()) || label(&t, "tower.role").is_some() || left_alone(&t) {
            continue;
        }
        let place = t["labels"]["where"].as_str().unwrap_or("");
        let space = place.split(" / ").next().map(str::trim).unwrap_or("").to_string();
        if space.is_empty() || over(&space) || out.iter().any(|k| k.session_id == h.session_id) {
            continue;
        }
        out.push(Kept {
            session_id: h.session_id.clone(),
            app: h.app.clone(),
            space,
            tab: tab_of(&t),
            address: h.terminal.clone(),
            name: crate::hc::name(&t).unwrap_or("").to_string(),
            cwd: if h.cwd.is_empty() { t["cwd"].as_str().unwrap_or("").to_string() } else { h.cwd.clone() },
            labels: tower_labels(&t),
            transcript: h.transcript.clone(),
            said: crate::missions::clip_said(&h.said),
            seen_ms: h.event_ms,
            core_ms: 0,
            ..Default::default()
        });
    }
    out
}

/// One look: `live` is every session kept now (under the core that started
/// at `core_ms`). The rest are lost, or ended and forgotten after
/// `GRACE_MS`. Whether anything worth writing down changed.
pub fn follow(b: &mut Book, live: Vec<Kept>, core_ms: u64, now: u64) -> bool {
    let mut changed = false;
    let ids: Vec<String> = live.iter().map(|k| k.session_id.clone()).collect();
    for k in live {
        match b.sessions.iter_mut().find(|s| s.session_id == k.session_id) {
            Some(s) => {
                let seen_due = now.saturating_sub(s.seen_ms) >= SEEN_EVERY_MS;
                let moved = Kept { seen_ms: 0, said: String::new(), ..s.clone() } != Kept { seen_ms: 0, said: String::new(), ..k.clone() };
                if moved || seen_due {
                    *s = k;
                    changed = true;
                } else if s.said != k.said {
                    s.said = k.said;
                }
            }
            None => {
                b.sessions.push(k);
                changed = true;
            }
        }
    }
    let before = b.sessions.len();
    b.sessions.retain_mut(|s| {
        if ids.contains(&s.session_id) {
            return true;
        }
        if s.before(core_ms) {
            // Lost to a restart: offered back for a while.
            return now.saturating_sub(s.seen_ms) < KEEP_LOST_MS;
        }
        // Ended while everything stayed up.
        if s.gone_ms.is_none() {
            s.gone_ms = Some(now);
            changed = true;
        }
        now.saturating_sub(s.gone_ms.unwrap_or(now)) < GRACE_MS
    });
    changed |= b.sessions.len() != before;
    changed
}

/// What was lost to a restart and not put back (in the order kept). One
/// the tower put back that isn't seen again within `REOPEN_WAIT_MS` (its
/// resume failed) is lost again.
pub fn lost(b: &Book, core_ms: u64, now: u64) -> Vec<&Kept> {
    b.sessions.iter().filter(|s| s.before(core_ms) && s.reopened_ms.is_none_or(|t| now.saturating_sub(t) >= REOPEN_WAIT_MS)).collect()
}

/// The spaces of `lost`, in order.
pub fn spaces_of(lost: &[&Kept]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for k in lost {
        if !out.iter().any(|s| s.eq_ignore_ascii_case(&k.space)) {
            out.push(k.space.clone());
        }
    }
    out
}

// ---- the offer ----

/// What a reopen card's answer asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pick {
    /// Every lost session.
    All,
    /// Those of these spaces.
    Spaces(Vec<String>),
    /// None of them: let them go.
    Forget,
    /// The user's own words: the spaces they name.
    Words(String),
}

impl Pick {
    /// The code an option of the card carries (`decisions::Opt::text`).
    pub fn code(&self) -> String {
        match self {
            Pick::All => "reopen:*".into(),
            Pick::Spaces(s) => format!("reopen:{}", s.join("\u{1f}")),
            Pick::Forget => "reopen:forget".into(),
            Pick::Words(w) => format!("reopen:words:{w}"),
        }
    }

    pub fn of_code(code: &str) -> Option<Pick> {
        let rest = code.strip_prefix("reopen:")?;
        Some(match rest {
            "*" => Pick::All,
            "forget" => Pick::Forget,
            r => match r.strip_prefix("words:") {
                Some(w) => Pick::Words(w.to_string()),
                None => Pick::Spaces(r.split('\u{1f}').map(String::from).collect()),
            },
        })
    }
}

/// The card: its line, what it lists, its options.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Offer {
    pub title: String,
    pub detail: String,
    /// (label, why, pick, the tower's pick).
    pub options: Vec<(String, String, Pick, bool)>,
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// The card for what was lost (none for nothing). `brain_down`: the
/// tower's own brain isn't running either (it comes back with them).
pub fn offer(lost: &[&Kept], brain_down: bool) -> Option<Offer> {
    if lost.is_empty() {
        return None;
    }
    let spaces = spaces_of(lost);
    let title = format!(
        "{} {} closed when the Mac or Herdr restarted. Reopen {} where {}, with {} conversation{}?",
        count(lost.len(), "session", "sessions"),
        if spaces.len() == 1 { format!("in {}", spaces[0]) } else { format!("in {} spaces", spaces.len()) },
        if lost.len() == 1 { "it" } else { "them" },
        if lost.len() == 1 { "it was" } else { "they were" },
        if lost.len() == 1 { "its" } else { "their" },
        if lost.len() == 1 { "" } else { "s" },
    );
    let mut detail: Vec<String> = spaces
        .iter()
        .map(|sp| {
            let names: Vec<&str> = lost.iter().filter(|k| k.space.eq_ignore_ascii_case(sp)).map(|k| k.called()).collect();
            format!("{sp}: {}", names.join(", "))
        })
        .collect();
    if brain_down {
        detail.push("The tower's own brain isn't running either: it starts again with them.".into());
    }
    let mut options = vec![(
        if lost.len() == 1 { "Reopen it".to_string() } else { "Reopen all".to_string() },
        "each one resumes its own conversation in its tab".to_string(),
        Pick::All,
        true,
    )];
    if spaces.len() > 1 {
        for sp in spaces.iter().take(3) {
            options.push((format!("Only {sp}"), String::new(), Pick::Spaces(vec![sp.clone()]), false));
        }
    }
    options.push(("Forget them".into(), "the tower stops offering them".into(), Pick::Forget, false));
    Some(Offer { title, detail: detail.join("\n"), options })
}

/// The lost sessions a pick takes (`Words`: the spaces they name, or all
/// for a plain yes). Err: words that name none of them.
pub fn chosen<'a>(lost: &[&'a Kept], pick: &Pick) -> Result<Vec<&'a Kept>> {
    let spaces = spaces_of(lost);
    let want: Vec<String> = match pick {
        Pick::All | Pick::Forget => return Ok(lost.to_vec()),
        Pick::Spaces(s) => s.clone(),
        Pick::Words(words) => {
            let lower = words.to_lowercase();
            let named: Vec<String> = spaces.iter().filter(|s| lower.contains(&s.to_lowercase())).cloned().collect();
            if named.is_empty() {
                let yes = lower.split(|c: char| !c.is_alphanumeric()).any(|w| matches!(w, "all" | "yes" | "everything" | "reopen" | "ok"));
                if !yes {
                    bail!("name the spaces to reopen ({}), or choose an option", spaces.join(", "));
                }
                return Ok(lost.to_vec());
            }
            named
        }
    };
    let out: Vec<&Kept> = lost.iter().copied().filter(|k| want.iter().any(|w| w.eq_ignore_ascii_case(&k.space))).collect();
    if out.is_empty() {
        bail!("nothing lost in {}: {}", want.join(", "), if spaces.is_empty() { "nothing is".to_string() } else { format!("only in {}", spaces.join(", ")) });
    }
    Ok(out)
}

// ---- putting it back ----

/// The command line that resumes a session's conversation: on its account
/// through `cca` (the default one when it isn't known), else plain.
pub fn resume_argv(k: &Kept, cca: Option<&str>, default_account: Option<&str>) -> Option<Vec<String>> {
    match k.app.as_str() {
        "claude-code" => {
            let account = k.account.as_deref().or(default_account);
            let mut a: Vec<String> = match (cca, account) {
                (Some(cca), Some(acc)) => vec![cca.into(), acc.into()],
                _ => vec!["claude".into()],
            };
            if !k.name.trim().is_empty() {
                a.extend(["-n".into(), k.name.clone()]);
            }
            a.extend(k.flags.iter().cloned());
            a.extend(["--resume".into(), k.session_id.clone()]);
            Some(a)
        }
        "codex" => Some(vec!["codex".into(), "resume".into(), k.session_id.clone()]),
        _ => None,
    }
}

/// A word as a shell reads it back.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What is typed into a kept shell: into its folder, then the resume.
pub fn shell_line(cwd: &str, argv: &[String]) -> String {
    let cmd = argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    if cwd.is_empty() { cmd } else { format!("cd {} && {cmd}", quote(cwd)) }
}

/// How one session goes back.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Typed into its own terminal, which Herdr kept as a shell.
    Type { address: String, line: String },
    /// A new tab in a space that is there (by its id).
    Tab { space_id: String, tab: String, folder: String, argv: Vec<String> },
    /// A new space (for the first of its sessions; the next ones are its
    /// tabs). `home`: for a session in one of the tower's worktrees, the
    /// space opens on a home tab there first (`worktrees::home_of`).
    Space { name: String, folder: String, tab: String, argv: Vec<String>, home: Option<String> },
}

/// Where each chosen session goes, or why it can't (`(session, why)`).
/// `windows`: Herdr's spaces now; `get`: a terminal as `hc get` has it;
/// `exists`: whether a path is there.
pub fn plan(
    chosen: &[&Kept],
    windows: &[Window],
    folders: &BTreeMap<String, String>,
    get: &dyn Fn(&str) -> Option<Value>,
    exists: &dyn Fn(&str) -> bool,
    cca: Option<&str>,
    default_account: Option<&str>,
) -> Vec<(Kept, Result<Step, String>)> {
    let mut made: Vec<String> = vec![];
    let mut out = vec![];
    for k in chosen {
        let step = (|| {
            if !k.cwd.is_empty() && !exists(&k.cwd) {
                return Err(format!("its folder {} is gone", k.cwd));
            }
            if k.app == "claude-code" && !k.transcript.is_empty() && !exists(&k.transcript) {
                return Err("its conversation's file is gone".to_string());
            }
            let Some(argv) = resume_argv(k, cca, default_account) else { return Err(format!("the tower can't resume a {} session", k.app)) };
            // Its own terminal, kept by Herdr as a shell in the same space, still its conversation's.
            if let Some(t) = get(&k.address)
                && kept_shell(&t)
                && t["labels"]["where"].as_str().and_then(|w| w.split(" / ").next()).is_some_and(|s| s.trim().eq_ignore_ascii_case(&k.space))
                && label(&t, LABEL_SESSION).is_none_or(|s| s == k.session_id)
            {
                return Ok(Step::Type { address: k.address.clone(), line: shell_line(&k.cwd, &argv) });
            }
            let tab = if k.tab.trim().is_empty() || k.tab.chars().all(|c| c.is_ascii_digit()) { k.called().to_string() } else { k.tab.clone() };
            if let Some(w) = windows.iter().find(|w| w.name.eq_ignore_ascii_case(&k.space) && w.machine.is_none()) {
                return Ok(Step::Tab { space_id: w.id.clone(), tab, folder: k.cwd.clone(), argv });
            }
            if made.iter().any(|m| m.eq_ignore_ascii_case(&k.space)) {
                // Made a step ago: its id is known once it is there.
                return Ok(Step::Tab { space_id: String::new(), tab, folder: k.cwd.clone(), argv });
            }
            made.push(k.space.clone());
            let folder = folders.get(&k.space).filter(|f| !f.is_empty() && exists(f)).cloned().unwrap_or_else(|| k.cwd.clone());
            // In a worktree: the space opens on its home tab, so Herdr doesn't nest it under the repo's.
            let home = k
                .labels
                .get("tower.worktree")
                .or(Some(&k.cwd))
                .and_then(|w| crate::worktrees::repo_of(Path::new(w)))
                .and_then(|(_, top)| top.parent().map(|h| h.to_string_lossy().into_owned()))
                .filter(|h| exists(h));
            Ok(Step::Space { name: k.space.clone(), folder, tab, argv, home })
        })();
        out.push(((*k).clone(), step));
    }
    out
}

/// What reopening needs of Herdr.
pub trait Hands {
    fn type_line(&self, address: &str, line: &str) -> Result<()>;
    /// A new tab: its terminal's address.
    fn tab(&self, space_id: &str, tab: &str, folder: &str, argv: &[String]) -> Result<String>;
    /// A new space with its first tab (after a home tab in `home`, when
    /// given): its terminal's address.
    fn space(&self, name: &str, folder: &str, tab: &str, argv: &[String], home: Option<&str>) -> Result<String>;
    /// A space's id by its name, as Herdr has it now.
    fn space_id(&self, name: &str) -> Option<String>;
    fn label(&self, address: &str, labels: &[String]);
}

/// One session put back, or not.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Done {
    pub space: String,
    pub name: String,
    pub session_id: String,
    /// Its terminal now (empty when it failed).
    pub address: String,
    /// `typed` (in its own terminal), `tab`, `space`, or `failed`.
    pub how: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// Carries out `plan`'s steps.
pub fn run(steps: Vec<(Kept, Result<Step, String>)>, hands: &dyn Hands) -> Vec<Done> {
    let mut out = vec![];
    for (k, step) in steps {
        let done = |address: String, how: &str, why: String| Done { space: k.space.clone(), name: k.called().to_string(), session_id: k.session_id.clone(), address, how: how.into(), why };
        let result = match step {
            Err(why) => Err(anyhow::anyhow!(why)),
            Ok(Step::Type { address, line }) => hands.type_line(&address, &line).map(|_| (address, "typed")),
            Ok(Step::Tab { space_id, tab, folder, argv }) => {
                let id = if space_id.is_empty() { hands.space_id(&k.space) } else { Some(space_id) };
                match id {
                    Some(id) => hands.tab(&id, &tab, &folder, &argv).map(|a| (a, "tab")),
                    None => Err(anyhow::anyhow!("its space {} isn't in Herdr's window", k.space)),
                }
            }
            Ok(Step::Space { name, folder, tab, argv, home }) => hands.space(&name, &folder, &tab, &argv, home.as_deref()).map(|a| (a, "space")),
        };
        match result {
            Ok((address, how)) => {
                // What the tower gave it, on its terminal again (a tab's name is a label).
                if how != "typed" {
                    let mut labels: Vec<String> = k.labels.iter().map(|(key, v)| format!("{key}={v}")).collect();
                    if !k.labels.contains_key("tower.name") && !k.name.is_empty() {
                        labels.push(format!("tower.name={}", k.name));
                    }
                    if !labels.is_empty() {
                        hands.label(&address, &labels);
                    }
                }
                out.push(done(address, how, String::new()));
            }
            Err(e) => out.push(done(String::new(), "failed", format!("{e:#}"))),
        }
    }
    out
}

/// What `tower reopen` shows: the lost ones by space, and those put back lately.
pub fn listing(b: &Book, core_ms: u64, now: u64) -> Value {
    let lost = lost(b, core_ms, now);
    let row = |k: &Kept| {
        json!({
            "space": k.space, "name": k.called(), "tab": k.tab, "address": k.address, "app": k.app, "folder": k.cwd,
            "account": k.account, "session_id": k.session_id, "seen_ms": k.seen_ms, "said": k.said,
        })
    };
    let back: Vec<Value> = b
        .sessions
        .iter()
        .filter(|k| k.reopened_ms.is_some_and(|t| now.saturating_sub(t) < REOPEN_WAIT_MS))
        .map(|k| json!({ "space": k.space, "name": k.called(), "address": k.address, "reopened_ms": k.reopened_ms }))
        .collect();
    json!({
        "lost": lost.iter().map(|k| row(k)).collect::<Vec<_>>(),
        "spaces": spaces_of(&lost),
        "open": b.sessions.iter().filter(|k| !k.before(core_ms) && k.gone_ms.is_none() && k.reopened_ms.is_none()).count(),
        "reopening": back,
        "decision": b.decision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(space: &str, address: &str, name: &str) -> Seen {
        Seen { space: space.into(), address: address.into(), name: name.into(), status: "idle".into(), account: Some("default".into()), ..Default::default() }
    }

    fn terminal(address: &str, place: &str, session: &str) -> Value {
        json!({
            "address": address, "state": "live", "pid": 10,
            "foreground": { "argv": ["claude", "-n", "x", "--model", "claude-opus-5-5", "fix the bug"], "cwd": "/w/app", "pid": 11 },
            "labels": { "where": place, "tower.session": session, "tower.name": "x", "tower.worktree": "/w/.tw/app", "herdr.seen": "3" },
            "status": { "app": "claude-code", "state": "idle" },
        })
    }

    fn shell(address: &str, place: &str, session: &str) -> Value {
        json!({
            "address": address, "state": "live", "pid": 20,
            "foreground": { "argv": ["/bin/zsh", "-l"], "cwd": "/w/app", "pid": 20 },
            "labels": { "where": place, "tower.session": session },
        })
    }

    #[test]
    fn a_live_session_is_kept_with_its_conversation_place_and_what_the_tower_gave_it() {
        let k = kept_of(&seen("Shop", "t7", "x"), &terminal("t7", "Shop / shop-app", "s-1"), None, 1000, 5000).unwrap();
        assert_eq!((k.session_id.as_str(), k.app.as_str(), k.space.as_str(), k.tab.as_str(), k.cwd.as_str()), ("s-1", "claude-code", "Shop", "shop-app", "/w/app"));
        assert_eq!(k.flags, ["--model", "claude-opus-5-5"], "its task is not typed again; its model is kept");
        assert_eq!(k.labels.keys().collect::<Vec<_>>(), ["tower.name", "tower.worktree"], "not its session, not Herdr's");
        assert_eq!((k.core_ms, k.seen_ms, k.account.as_deref()), (1000, 5000, Some("default")));
        // The tower's own reviewers and planner, another machine's sessions, plain programs: not kept.
        let mut reviewer = terminal("t8", "Shop / review", "s-2");
        reviewer["labels"]["tower.role"] = json!("reviewer");
        assert!(kept_of(&seen("Shop", "t8", "r"), &reviewer, None, 1000, 5000).is_none());
        assert!(kept_of(&seen("Shop", "mini/t3", "x"), &terminal("mini/t3", "Shop / x", "s-3"), None, 1000, 5000).is_none());
        let mut plain = terminal("t9", "Shop / x", "s-4");
        plain["status"]["app"] = json!("vim");
        assert!(kept_of(&seen("Shop", "t9", "x"), &plain, None, 1000, 5000).is_none());
    }

    #[test]
    fn a_session_lost_to_a_restart_stays_and_one_that_ended_is_forgotten() {
        let (core, later_core) = (1_000_000, 9_000_000);
        let mut b = Book::default();
        let a = kept_of(&seen("Shop", "t7", "x"), &terminal("t7", "Shop / app", "s-1"), None, core, 2_000_000).unwrap();
        let c = kept_of(&seen("Shop", "t9", "y"), &terminal("t9", "Shop / api", "s-2"), None, core, 2_000_000).unwrap();
        assert!(follow(&mut b, vec![a.clone(), c.clone()], core, 2_000_000));
        assert!(!follow(&mut b, vec![a.clone(), c.clone()], core, 2_020_000), "nothing new: nothing to write");
        // y ends while everything stays up: missed, then forgotten after the grace.
        follow(&mut b, vec![a.clone()], core, 2_040_000);
        assert_eq!(b.sessions.len(), 2);
        assert!(b.sessions[1].gone_ms.is_some());
        follow(&mut b, vec![a.clone()], core, 2_040_000 + GRACE_MS);
        assert_eq!(b.sessions.iter().map(|k| k.called()).collect::<Vec<_>>(), ["x"]);
        // The Mac shuts down: the daemon sees x gone once, then dies before the grace is over.
        follow(&mut b, vec![], core, 3_000_000);
        // After the restart: a new core; x is lost, never forgotten for being gone.
        let at = 9_100_000 + GRACE_MS * 10;
        follow(&mut b, vec![], later_core, at);
        let lost = lost(&b, later_core, at);
        assert_eq!(lost.iter().map(|k| k.session_id.as_str()).collect::<Vec<_>>(), ["s-1"]);
        // Put back by the tower: not offered while it starts; not back in time, lost again.
        b.sessions[0].reopened_ms = Some(at);
        assert!(super::lost(&b, later_core, at + 1000).is_empty());
        assert_eq!(super::lost(&b, later_core, at + REOPEN_WAIT_MS).len(), 1);
        // Brought back by hand: seen under the new core, no longer lost.
        let back = kept_of(&seen("Shop", "t7", "x"), &terminal("t7", "Shop / app", "s-1"), None, later_core, 9_200_000).unwrap();
        follow(&mut b, vec![back], later_core, 9_200_000);
        assert!(super::lost(&b, later_core, 9_200_000).is_empty());
        assert_eq!(b.sessions[0].reopened_ms, None);
        // A lost one is let go after a week.
        let mut old = Book { sessions: vec![Kept { session_id: "s-9".into(), core_ms: 0, seen_ms: 1, ..Default::default() }], ..Default::default() };
        follow(&mut old, vec![], later_core, KEEP_LOST_MS + 2);
        assert!(old.sessions.is_empty());
    }

    #[test]
    fn the_first_look_takes_what_herdr_kept_as_a_shell_from_before_the_restart() {
        let core = 5_000_000;
        let hook = |t: &str, id: &str, at: u64| AgentSession { terminal: t.into(), app: "claude-code".into(), session_id: id.into(), cwd: "/w/app".into(), event_ms: at, ..Default::default() };
        let hooks = vec![
            hook("t7h", "s-1", 4_000_000),
            hook("t7j", "s-2", 4_000_000),
            hook("t7p", "s-3", 4_000_000),
            hook("t10b", "s-4", 4_000_000),
            hook("t2", "s-5", 6_000_000),
        ];
        let get = |a: &str| -> Option<Value> {
            match a {
                "t7h" => Some(shell("t7h", "sandbox / sandbox-eyzflow", "s-1")),
                // Its shell now ties to another conversation: not this one's terminal.
                "t7j" => Some(shell("t7j", "sandbox / sandbox-mediaroom", "s-other")),
                // Resumed by hand already: live, not a shell.
                "t7p" => Some(terminal("t7p", "M5 Pro / tower", "s-3")),
                "t10b" => Some(shell("t10b", "ns-check / x", "s-4")),
                "t2" => Some(shell("t2", "sandbox / later", "s-5")),
                _ => None,
            }
        };
        let got = seed(&hooks, &get, core, 6_500_000, &|s| s == "ns-check", &|_| false);
        assert_eq!(got.iter().map(|k| (k.session_id.as_str(), k.space.as_str(), k.tab.as_str())).collect::<Vec<_>>(), [("s-1", "sandbox", "sandbox-eyzflow")]);
        assert_eq!(got[0].core_ms, 0, "from before any core: lost");
        assert_eq!(lost(&Book { sessions: got, ..Default::default() }, core, 6_500_000).len(), 1);
    }

    #[test]
    fn the_card_offers_all_each_space_and_forgetting() {
        let k = |space: &str, name: &str| Kept { space: space.into(), name: name.into(), session_id: name.into(), ..Default::default() };
        let (a, b, c) = (k("sandbox", "sandbox-eyzflow"), k("sandbox", "sandbox-mediaroom"), k("zendesk", "zendesk-eyzflow"));
        let o = offer(&[&a, &b, &c], true).unwrap();
        assert_eq!(o.title, "3 sessions in 2 spaces closed when the Mac or Herdr restarted. Reopen them where they were, with their conversations?");
        assert_eq!(o.detail, "sandbox: sandbox-eyzflow, sandbox-mediaroom\nzendesk: zendesk-eyzflow\nThe tower's own brain isn't running either: it starts again with them.");
        assert_eq!(o.options.iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["Reopen all", "Only sandbox", "Only zendesk", "Forget them"]);
        assert!(o.options[0].3 && !o.options[1].3);
        let one = offer(&[&a], false).unwrap();
        assert_eq!(one.title, "1 session in sandbox closed when the Mac or Herdr restarted. Reopen it where it was, with its conversation?");
        assert_eq!(one.options.iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["Reopen it", "Forget them"]);
        assert!(offer(&[], false).is_none());
        // Each option's answer, and the user's own words.
        for p in [Pick::All, Pick::Forget, Pick::Spaces(vec!["sandbox".into()]), Pick::Words("only zendesk please".into())] {
            assert_eq!(Pick::of_code(&p.code()), Some(p));
        }
        let lost = [&a, &b, &c];
        let names = |p: &Pick| chosen(&lost, p).map(|v| v.iter().map(|k| k.called().to_string()).collect::<Vec<_>>());
        assert_eq!(names(&Pick::Spaces(vec!["Sandbox".into()])).unwrap(), ["sandbox-eyzflow", "sandbox-mediaroom"]);
        assert_eq!(names(&Pick::Words("just zendesk".into())).unwrap(), ["zendesk-eyzflow"]);
        assert_eq!(names(&Pick::Words("yes, all of them".into())).unwrap().len(), 3);
        assert!(names(&Pick::Words("later".into())).is_err());
    }

    struct Fake {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl Hands for Fake {
        fn type_line(&self, address: &str, line: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("type {address} {line}"));
            Ok(())
        }
        fn tab(&self, space_id: &str, tab: &str, folder: &str, argv: &[String]) -> Result<String> {
            self.calls.lock().unwrap().push(format!("tab {space_id} {tab} {folder} {}", argv.join(" ")));
            Ok("t50".into())
        }
        fn space(&self, name: &str, folder: &str, tab: &str, argv: &[String], home: Option<&str>) -> Result<String> {
            self.calls.lock().unwrap().push(format!("space {name} {folder} {tab} {}{}", argv.join(" "), home.map(|h| format!(" home {h}")).unwrap_or_default()));
            Ok("t60".into())
        }
        fn space_id(&self, name: &str) -> Option<String> {
            (name == "zendesk").then(|| "sp-new".to_string())
        }
        fn label(&self, address: &str, labels: &[String]) {
            self.calls.lock().unwrap().push(format!("label {address} {}", labels.join(" ")));
        }
    }

    #[test]
    fn each_goes_back_where_it_was_resuming_its_own_conversation() {
        let k = |space: &str, address: &str, tab: &str, id: &str, cwd: &str| Kept {
            space: space.into(),
            address: address.into(),
            tab: tab.into(),
            name: tab.into(),
            session_id: id.into(),
            app: "claude-code".into(),
            cwd: cwd.into(),
            account: Some("orca".into()),
            ..Default::default()
        };
        let kept_shell_one = k("sandbox", "t7h", "sandbox-eyzflow", "s-1", "/w/eyzflow");
        let mut in_a_tab = k("sandbox", "t7j", "sandbox-mediaroom", "s-2", "/w/media room");
        in_a_tab.account = None;
        in_a_tab.labels.insert("tower.port".into(), "3002".into());
        let mut first = k("zendesk", "t10h", "zendesk-eyzflow", "s-3", "/w/.tower-worktrees/zendesk/eyzflow");
        first.labels.insert("tower.worktree".into(), "/w/.tower-worktrees/zendesk/eyzflow".into());
        let second = Kept { app: "codex".into(), ..k("zendesk", "t10j", "zendesk-api", "s-4", "/w/tw/api") };
        let gone = k("sandbox", "t7k", "sandbox-project", "s-5", "/w/removed");
        let windows = vec![Window { id: "sp-sandbox".into(), name: "sandbox".into(), folder: "/w/eyzflow".into(), machine: None, active: false }];
        let get = |a: &str| (a == "t7h").then(|| shell("t7h", "sandbox / sandbox-eyzflow", "s-1"));
        let exists = |p: &str| p != "/w/removed";
        let folders = BTreeMap::from([("zendesk".to_string(), "/w/tw".to_string())]);
        let chosen = [&kept_shell_one, &in_a_tab, &first, &second, &gone];
        let steps = plan(&chosen, &windows, &folders, &get, &exists, Some("/bin/cca"), Some("default"));
        assert_eq!(steps[0].1, Ok(Step::Type { address: "t7h".into(), line: "cd /w/eyzflow && /bin/cca orca -n sandbox-eyzflow --resume s-1".into() }));
        assert!(matches!(&steps[1].1, Ok(Step::Tab { space_id, argv, .. }) if space_id == "sp-sandbox" && argv[..2] == ["/bin/cca".to_string(), "default".to_string()]), "its account not known: the default one");
        assert!(matches!(&steps[2].1, Ok(Step::Space { name, folder, home, .. }) if name == "zendesk" && folder == "/w/tw" && home.as_deref() == Some("/w/.tower-worktrees/zendesk")), "{:?}", steps[2].1);
        assert!(matches!(&steps[3].1, Ok(Step::Tab { space_id, argv, .. }) if space_id.is_empty() && argv == &["codex", "resume", "s-4"]));
        assert_eq!(steps[4].1, Err("its folder /w/removed is gone".into()));
        let hands = Fake { calls: Default::default() };
        let done = run(steps, &hands);
        assert_eq!(done.iter().map(|d| d.how.as_str()).collect::<Vec<_>>(), ["typed", "tab", "space", "tab", "failed"]);
        let calls = hands.calls.lock().unwrap().clone();
        assert_eq!(calls[0], "type t7h cd /w/eyzflow && /bin/cca orca -n sandbox-eyzflow --resume s-1");
        assert_eq!(calls[1], "tab sp-sandbox sandbox-mediaroom /w/media room /bin/cca default -n sandbox-mediaroom --resume s-2");
        assert_eq!(calls[2], "label t50 tower.port=3002 tower.name=sandbox-mediaroom", "what the tower gave it goes with it");
        assert!(calls[3].starts_with("space zendesk /w/tw zendesk-eyzflow ") && calls[3].ends_with(" home /w/.tower-worktrees/zendesk"), "{}", calls[3]);
        assert_eq!(calls[5], "tab sp-new zendesk-api /w/tw/api codex resume s-4", "into the space made a step ago");
        assert_eq!(shell_line("/w/media room", &["claude".into(), "it's".into()]), r"cd '/w/media room' && claude 'it'\''s'");
    }
}
