//! Pull requests the tower's sessions open, as chips in the chat.
//!
//! - **Found** at a session's turn end: a `github.com/<owner>/<repo>/pull/<n>`
//!   link in what it said, or in what its turn printed (`gh pr create`
//!   prints the link). Tied to the session and its space. Only a PR that is
//!   open when first looked at becomes a chip.
//! - **Polled** with `gh pr view --json …` while open (every 30 s while
//!   checks run or while it waits for the user's merge, 90 s otherwise, 15
//!   min once it's days old), again at once when the queue is looked at
//!   (`look_now`), and once more after it closes. Kept in `prs.json` beside `state.json` (the orb watches
//!   that folder), across restarts.
//! - **In the chat**: one history entry (`kind` `pr`, `id` the PR's key) when
//!   it's found; the chip shows the PR as it is now, so it changes in place
//!   (checks finish, merged, closed). "Waiting for you" when the session
//!   that made it finished its turn on it, or asked the user to merge.
//! - **Merged** by the user only: from the Mac panel, or a paired phone with
//!   a confirm token (`/v1/prs/…/confirm`, then `…/merge`). Never by the
//!   brain: no `tower` command and no socket request merges. `merge` uses the
//!   repo's default method, pinned to the head commit that was confirmed,
//!   never `--admin`. Failing checks or conflicts disable it (`gate`);
//!   failing checks the user knows to be unrelated can be overridden by a
//!   second, explicit confirmation.
//! - **Afterwards** the daemon checks with GitHub that it is merged, updates
//!   the chip, queues the session "The user merged PR #N …" and tells the
//!   brain (`Prs::merged`, through the daemon's wiring).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A PR's checks, rolled up: the latest run of each check.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Checks {
    /// `pending`, `passing`, `failing`, or `none` (no checks ran).
    pub state: String,
    /// The checks that failed, by name.
    pub failing: Vec<String>,
    /// The checks still running, by name.
    pub pending: Vec<String>,
    pub passed: usize,
    pub total: usize,
}

/// One pull request, as the chip shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pr {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub title: String,
    /// The branch it merges into (`dev`, `main`) and from.
    pub base: String,
    pub head: String,
    /// The head commit when last looked at: a merge is pinned to it.
    pub head_sha: String,
    /// `open`, `merged` or `closed`.
    pub state: String,
    pub draft: bool,
    /// `mergeable`, `conflicting` or `unknown` (GitHub is still working it out).
    pub mergeable: String,
    /// GitHub's merge state, lowercase (`clean`, `behind`: its branch is
    /// behind the base and the repo wants it up to date, `dirty`:
    /// conflicts, `blocked`, `unstable`, `has_hooks`, `draft`, `unknown`);
    /// empty when it wasn't asked.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub merge_state: String,
    pub checks: Checks,
    /// The session that made it (address), its name and space.
    pub session: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// The session finished its turn on it, or asked the user to merge it.
    pub waiting: bool,
    pub found_ms: u64,
    pub checked_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_ms: Option<u64>,
    /// Who merged it: `mac`, `phone:<device>`, `tower` (low risk, into dev:
    /// `crate::prscore::auto_path`), or `github` (seen merged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_by: Option<String>,
    /// Looked at once more after it closed: no more polling.
    pub settled: bool,
    /// The last time `gh` couldn't say (kept until it can).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Its review (`crate::reviews`), as the chip and the merge card show
    /// it; none when its space has review off, or it got no reviewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<crate::reviews::Brief>,
    /// Whether it needs a human code review, and why (`crate::prscore`):
    /// its plan's prediction, read again on its own files at its head.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<crate::prscore::Score>,
    /// The tower's own merge of it (`[prs] auto_merge_low_risk`): merged, or why it couldn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<crate::prscore::Auto>,
}

impl Pr {
    /// `owner/name#123`: its id everywhere (the chat entry's `id` too).
    pub fn key(&self) -> String {
        key(&self.repo, self.number)
    }

    pub fn open(&self) -> bool {
        self.state == "open"
    }

    /// As the chat, the panel and the phone show it: the PR and whether
    /// (and why not) it can be merged now.
    pub fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["key"] = json!(self.key());
        v["merge"] = gate(self).view();
        v
    }

    /// A short line for speech and the brain: "PR #12 into main (owner/app)".
    pub fn short(&self) -> String {
        format!("PR #{} into {} ({})", self.number, if self.base.is_empty() { "?" } else { &self.base }, self.repo)
    }

    /// Its checks in a few words: "checks passing", "1 check failing: lint".
    pub fn checks_line(&self) -> String {
        let c = &self.checks;
        match c.state.as_str() {
            "failing" => format!("{} check{} failing: {}", c.failing.len(), if c.failing.len() == 1 { "" } else { "s" }, c.failing.join(", ")),
            "pending" => format!("checks running ({} of {} done)", c.total - c.pending.len(), c.total),
            "passing" => "checks passing".into(),
            _ => "no checks".into(),
        }
    }
}

pub fn key(repo: &str, number: u64) -> String {
    format!("{repo}#{number}")
}

// ---- finding them ----

/// Every `github.com/<owner>/<repo>/pull/<n>` in `text`, in order, once each.
pub fn links(text: &str) -> Vec<(String, u64)> {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let mut out: Vec<(String, u64)> = vec![];
    let mut rest = text;
    while let Some(i) = rest.find("github.com/") {
        rest = &rest[i + "github.com/".len()..];
        let parts: Vec<&str> = rest.splitn(4, '/').collect();
        if parts.len() < 4 || parts[2] != "pull" {
            continue;
        }
        let (owner, name) = (parts[0], parts[1]);
        let digits: String = parts[3].chars().take_while(char::is_ascii_digit).collect();
        if owner.is_empty() || name.is_empty() || !owner.chars().all(ok) || !name.chars().all(ok) || digits.is_empty() {
            continue;
        }
        let Ok(n) = digits.parse::<u64>() else { continue };
        let pr = (format!("{owner}/{}", name.trim_end_matches(".git")), n);
        if !out.contains(&pr) {
            out.push(pr);
        }
    }
    out
}

/// What a Claude Code turn said, and what its `gh pr create` printed, from
/// its transcript: after the last prompt, every message's text and the
/// output of each command that creates a PR (where its link is). Other
/// commands' output isn't: `gh pr list` names PRs it didn't make.
pub fn turn_text(jsonl: &str) -> String {
    let mut out = String::new();
    let mut creates: Vec<String> = vec![];
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let c = &v["message"]["content"];
        match v["type"].as_str() {
            Some("user") => {
                // A prompt starts a turn; a tool's result is part of it.
                let prompt = c.is_string() || c.as_array().is_some_and(|a| a.iter().any(|b| b["type"] == "text"));
                if prompt {
                    out.clear();
                    creates.clear();
                }
                for b in c.as_array().into_iter().flatten().filter(|b| b["type"] == "tool_result") {
                    if !creates.iter().any(|id| b["tool_use_id"].as_str() == Some(id)) {
                        continue;
                    }
                    match &b["content"] {
                        Value::String(s) => out.push_str(s),
                        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).for_each(|t| out.push_str(t)),
                        _ => {}
                    }
                    out.push('\n');
                }
            }
            Some("assistant") => {
                for b in c.as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("text") => {
                            out.push_str(b["text"].as_str().unwrap_or(""));
                            out.push('\n');
                        }
                        Some("tool_use") if creates_pr(&b["input"]) => creates.push(b["id"].as_str().unwrap_or("").to_string()),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// A tool call that opens a PR: `gh pr create`, or GitHub's own tool for it.
fn creates_pr(input: &Value) -> bool {
    let cmd = input["command"].as_str().unwrap_or("");
    cmd.contains("gh pr create") || cmd.contains("hub pull-request") || input.get("head").is_some() && input.get("base").is_some() && input.get("title").is_some()
}

// ---- what GitHub says ----

/// The fields asked of `gh pr view`.
pub const FIELDS: &str = "number,title,url,state,isDraft,mergeable,mergeStateStatus,statusCheckRollup,baseRefName,headRefName,headRefOid,mergedAt,closedAt";

/// The PR as `gh pr view --json FIELDS` gives it, onto `pr` (keeping who
/// made it and what the tower knows).
pub fn apply_gh(pr: &mut Pr, v: &Value) {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    if let Some(n) = v["number"].as_u64() {
        pr.number = n;
    }
    for (field, k) in [(&mut pr.title, "title"), (&mut pr.url, "url"), (&mut pr.base, "baseRefName"), (&mut pr.head, "headRefName"), (&mut pr.head_sha, "headRefOid")] {
        let x = s(k);
        if !x.is_empty() {
            *field = x;
        }
    }
    if pr.repo.is_empty()
        && let Some((repo, _)) = links(&pr.url).into_iter().next()
    {
        pr.repo = repo;
    }
    pr.state = s("state").to_lowercase();
    pr.draft = v["isDraft"].as_bool().unwrap_or(false);
    pr.mergeable = match s("mergeable").as_str() {
        "MERGEABLE" => "mergeable",
        "CONFLICTING" => "conflicting",
        _ => "unknown",
    }
    .into();
    pr.merge_state = s("mergeStateStatus").to_lowercase();
    pr.checks = rollup(&v["statusCheckRollup"]);
}

/// The checks, the latest run of each (a check re-run after failing counts as its last run).
pub fn rollup(v: &Value) -> Checks {
    // name → (when, outcome): outcome 0 passed, 1 pending, 2 failed.
    let mut latest: BTreeMap<String, (String, u8)> = BTreeMap::new();
    for c in v.as_array().into_iter().flatten() {
        let (name, when, outcome) = if c["__typename"] == "StatusContext" || c.get("context").is_some() {
            let outcome = match c["state"].as_str().unwrap_or("") {
                "SUCCESS" => 0,
                "PENDING" | "EXPECTED" => 1,
                _ => 2,
            };
            (c["context"].as_str().unwrap_or("").to_string(), c["startedAt"].as_str().or(c["createdAt"].as_str()).unwrap_or("").to_string(), outcome)
        } else {
            let outcome = if c["status"].as_str().unwrap_or("") != "COMPLETED" {
                1
            } else {
                match c["conclusion"].as_str().unwrap_or("") {
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" => 0,
                    _ => 2,
                }
            };
            let name = match (c["workflowName"].as_str().filter(|w| !w.is_empty()), c["name"].as_str().unwrap_or("")) {
                (Some(w), n) if w != n => format!("{w} / {n}"),
                (_, n) => n.to_string(),
            };
            (name, c["startedAt"].as_str().unwrap_or("").to_string(), outcome)
        };
        if name.is_empty() {
            continue;
        }
        match latest.get(&name) {
            Some((w, _)) if *w > when => {}
            _ => {
                latest.insert(name, (when, outcome));
            }
        }
    }
    let mut c = Checks { total: latest.len(), ..Default::default() };
    for (name, (_, o)) in latest {
        match o {
            0 => c.passed += 1,
            1 => c.pending.push(name),
            _ => c.failing.push(name),
        }
    }
    c.state = if !c.failing.is_empty() {
        "failing"
    } else if !c.pending.is_empty() {
        "pending"
    } else if c.total > 0 {
        "passing"
    } else {
        "none"
    }
    .into();
    c
}

// ---- merging ----

/// Whether the user can merge it now, and what to tell them first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gate {
    /// Why it can't be merged at all (merged, closed, a draft, conflicts).
    pub blocked: Option<String>,
    /// It can, but only with a second, explicit confirmation: checks fail
    /// (the user says they are unrelated).
    pub needs_override: Option<String>,
    /// Said in the confirmation: into main or prod, checks still running, …
    pub warnings: Vec<String>,
}

impl Gate {
    pub fn view(&self) -> Value {
        json!({
            "can": self.blocked.is_none(),
            "blocked": self.blocked,
            "needs_override": self.needs_override,
            "warnings": self.warnings,
        })
    }
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Branches whose merge ships to production.
pub fn is_prod(base: &str) -> bool {
    matches!(base.to_lowercase().as_str(), "main" | "master" | "prod" | "production" | "release" | "live")
}

pub fn gate(pr: &Pr) -> Gate {
    let mut g = Gate {
        blocked: match () {
        _ if pr.state == "merged" => Some("It's merged already.".into()),
        _ if pr.state == "closed" => Some("It's closed.".into()),
        _ if pr.state != "open" => Some("GitHub hasn't said where it stands yet.".into()),
        _ if pr.draft => Some("It's a draft: mark it ready for review first.".into()),
        _ if pr.mergeable == "conflicting" => Some(format!("It has conflicts with {}: they need resolving first.", pr.base)),
        _ => None,
        },
        ..Default::default()
    };
    if pr.checks.state == "failing" {
        g.needs_override = Some(format!("{}. Merge anyway only if you know the failure is unrelated.", capital(&pr.checks_line())));
    }
    if is_prod(&pr.base) {
        g.warnings.push(format!("It merges into {}: that ships to production.", pr.base));
    }
    match pr.checks.state.as_str() {
        "pending" => g.warnings.push(format!("{} check{} still running.", pr.checks.pending.len(), if pr.checks.pending.len() == 1 { " is" } else { "s are" })),
        "none" => g.warnings.push("No checks ran on it.".into()),
        _ => {}
    }
    if pr.mergeable == "unknown" && pr.open() {
        g.warnings.push("GitHub hasn't worked out yet whether it merges cleanly.".into());
    }
    // A review that isn't agreed: said plainly; the merge stays the user's.
    if let Some(w) = pr.review.as_ref().and_then(crate::reviews::merge_warning) {
        g.warnings.push(w);
    }
    // Its score says a person must read it: said before the merge too.
    if let Some(s) = pr.score.as_ref().filter(|s| s.needs_review) {
        g.warnings.push(format!("{}.", s.line.trim_end_matches('.')));
    }
    g
}

/// Where `gh` is: `HERDER_TOWER_GH` (tests use a fake), else Homebrew's or
/// the system's (an app started from the Dock has a short PATH), else `gh`.
pub fn gh_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDER_TOWER_GH") {
        return PathBuf::from(p);
    }
    for p in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"] {
        if Path::new(p).exists() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from("gh")
}

fn gh(bin: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(bin).args(args).stdin(Stdio::null()).output().with_context(|| format!("running {}", bin.display()))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("{}", if e.is_empty() { format!("gh {} failed", args.first().unwrap_or(&"")) } else { e });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `gh pr view` of one PR.
pub fn fetch(bin: &Path, repo: &str, number: u64) -> Result<Value> {
    let out = gh(bin, &["pr", "view", &number.to_string(), "--repo", repo, "--json", FIELDS])?;
    serde_json::from_str(&out).context("reading gh's answer")
}

/// Brings a PR's branch up to date with its base on GitHub (a merge of the
/// base into it, `gh pr update-branch`): for one that fell behind.
pub fn update_branch(bin: &Path, repo: &str, number: u64) -> Result<()> {
    gh(bin, &["pr", "update-branch", &number.to_string(), "--repo", repo]).map(|_| ())
}

/// The files a PR changes and its head commit, as GitHub has them now
/// (`gh pr view --json files,headRefOid`): what its score reads.
pub fn fetch_files(bin: &Path, repo: &str, number: u64) -> Result<(Vec<String>, String)> {
    let out = gh(bin, &["pr", "view", &number.to_string(), "--repo", repo, "--json", "files,headRefOid"])?;
    let v: Value = serde_json::from_str(&out).context("reading gh's answer")?;
    Ok((crate::prscore::files_of(&v), v["headRefOid"].as_str().unwrap_or("").to_string()))
}

/// The repo's default merge method for this user: `--merge`, `--squash` or `--rebase`.
pub fn merge_method(bin: &Path, repo: &str) -> String {
    let v: Value = gh(bin, &["repo", "view", repo, "--json", "viewerDefaultMergeMethod"]).ok().and_then(|o| serde_json::from_str(&o).ok()).unwrap_or_default();
    match v["viewerDefaultMergeMethod"].as_str().unwrap_or("") {
        "SQUASH" => "--squash",
        "REBASE" => "--rebase",
        _ => "--merge",
    }
    .into()
}

/// What merging changes on the way: why it didn't go ahead.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// It can't be merged (`Gate::blocked`).
    Blocked(String),
    /// Checks fail and the user didn't say to merge anyway.
    NeedsOverride(String),
    /// New commits since the user confirmed: look again.
    Changed(String),
    /// gh couldn't merge it (branch protection, a required review, …).
    Failed(String),
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Blocked(_) => "blocked",
            Refusal::NeedsOverride(_) => "needs_override",
            Refusal::Changed(_) => "changed",
            Refusal::Failed(_) => "merge_failed",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Refusal::Blocked(m) | Refusal::NeedsOverride(m) | Refusal::Changed(m) | Refusal::Failed(m) => m,
        }
    }
}

/// Merges `key` as the user confirmed it (`sha`: the head commit they saw;
/// `override_failing`: they said the failing checks are unrelated). Looks
/// at the PR again first; the gate holds then as it did. The repo's own
/// default method, never `--admin`, pinned to `sha`. Returns the PR as
/// GitHub has it afterwards.
pub fn merge(bin: &Path, repo: &str, number: u64, sha: &str, override_failing: bool) -> Result<Pr, Refusal> {
    let mut pr = Pr { repo: repo.into(), number, ..Default::default() };
    let v = fetch(bin, repo, number).map_err(|e| Refusal::Failed(format!("Couldn't look at it: {e:#}")))?;
    apply_gh(&mut pr, &v);
    let g = gate(&pr);
    if let Some(b) = g.blocked {
        return Err(Refusal::Blocked(b));
    }
    if !sha.is_empty() && pr.head_sha != sha {
        return Err(Refusal::Changed("It has new commits since you confirmed: look at it again.".into()));
    }
    if let Some(o) = g.needs_override
        && !override_failing
    {
        return Err(Refusal::NeedsOverride(o));
    }
    let method = merge_method(bin, repo);
    let n = number.to_string();
    gh(bin, &["pr", "merge", &n, "--repo", repo, &method, "--match-head-commit", &pr.head_sha]).map_err(|e| Refusal::Failed(format!("{e:#}")))?;
    if let Ok(v) = fetch(bin, repo, number) {
        apply_gh(&mut pr, &v);
    }
    Ok(pr)
}

// ---- the store ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    prs: Vec<Pr>,
}

/// What changed, for whoever listens (the phones' `pr` event, the chat).
pub type OnChange = Box<dyn Fn(&str, &Pr) + Send + Sync>;

/// A PR merged (by mac, phone:<device>, or github for one its session
/// waited on): the daemon tells the session and the brain.
pub type OnMerged = Box<dyn Fn(&Pr, &str) + Send + Sync>;

/// A PR found just now, before it is shown: its review score (the daemon's:
/// its plan's prediction, and its own files).
pub type OnScore = Box<dyn Fn(&Pr) -> Option<crate::prscore::Score> + Send + Sync>;

/// A phone's confirmation: the PR's key, the device, the head commit, until when.
type Token = (String, String, String, u64);

/// The PRs the tower knows, kept in `prs.json`; the daemon's is the only one writing.
#[derive(Clone)]
pub struct Prs {
    path: PathBuf,
    gh: PathBuf,
    inner: Arc<Mutex<BTreeMap<String, Pr>>>,
    version: Arc<AtomicU64>,
    on_change: Arc<OnceLock<OnChange>>,
    on_merged: Arc<OnceLock<OnMerged>>,
    on_score: Arc<OnceLock<OnScore>>,
    /// Phone confirmations: token → (key, device, head sha, until).
    tokens: Arc<Mutex<BTreeMap<String, Token>>>,
    /// Chat entries are written to the history (the daemon's; tests keep none).
    chat: bool,
}

pub fn path_in(ui_dir: &Path) -> PathBuf {
    ui_dir.join("prs.json")
}

/// What `prs.json` holds, for readers (the orb).
pub fn load_from(ui_dir: &Path) -> BTreeMap<String, Pr> {
    let f: File = std::fs::read_to_string(path_in(ui_dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    f.prs.into_iter().map(|p| (p.key(), p)).collect()
}

/// The daemon's store, once it runs (the asks' turn ends report to it).
static DAEMON: OnceLock<Prs> = OnceLock::new();

pub fn daemon() -> Option<&'static Prs> {
    DAEMON.get()
}

const MIN: u64 = 60_000;

/// When a PR is due to be looked at again (None: never).
pub fn next_check(pr: &Pr) -> Option<u64> {
    if pr.settled {
        return None;
    }
    if !pr.open() {
        // Closed: once more, a minute on (GitHub's final word), then never.
        return Some(pr.closed_ms.unwrap_or(pr.checked_ms) + MIN);
    }
    let age = pr.checked_ms.saturating_sub(pr.found_ms);
    // One that waits for the user's merge is often merged on GitHub: looked at every 30 s, however old.
    let every = if pr.waiting {
        MIN / 2
    } else if age > 3 * 24 * 60 * MIN {
        15 * MIN
    } else if pr.checks.state == "pending" || pr.mergeable == "unknown" {
        MIN / 2
    } else {
        3 * MIN / 2
    };
    Some(pr.checked_ms + every)
}

impl Prs {
    pub fn at(path: PathBuf, gh: PathBuf) -> Prs {
        let f: File = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Prs {
            path,
            gh,
            inner: Arc::new(Mutex::new(f.prs.into_iter().map(|p| (p.key(), p)).collect())),
            version: Arc::default(),
            on_change: Arc::default(),
            on_merged: Arc::default(),
            on_score: Arc::default(),
            tokens: Arc::default(),
            chat: false,
        }
    }

    /// The daemon's: `prs.json` in the UI folder, the real `gh`, chat entries on.
    pub fn open_daemon() -> Prs {
        let mut p = Prs::at(path_in(&crate::config::ui_dir()), gh_bin());
        p.chat = true;
        let _ = DAEMON.set(p.clone());
        p
    }

    pub fn on_change(&self, f: OnChange) {
        let _ = self.on_change.set(f);
    }

    pub fn on_merged(&self, f: OnMerged) {
        let _ = self.on_merged.set(f);
    }

    /// How a PR found from now on is scored, before it is shown.
    pub fn on_score(&self, f: OnScore) {
        let _ = self.on_score.set(f);
    }

    /// The user merged `key` (from the Mac panel or a phone): GitHub is
    /// asked, and when it says merged, the chip moves and the session and the
    /// brain are told (`on_merged`), once.
    pub fn report_merged(&self, k: &str, by: &str) -> Option<Pr> {
        let pr = self.merged(k, by)?;
        if let Some(f) = self.on_merged.get() {
            f(&pr, by);
        }
        Some(pr)
    }

    pub fn gh(&self) -> &Path {
        &self.gh
    }

    /// Its review as it stands now (`crate::reviews`): the chip, the merge
    /// card and the phones follow. Nothing when it is the same.
    pub fn set_review(&self, k: &str, review: Option<crate::reviews::Brief>) {
        let Some(mut pr) = self.get(k) else { return };
        if pr.review == review {
            return;
        }
        pr.review = review;
        self.store(pr, "update", true);
    }

    /// Its review score as it stands now (read again at a new head commit).
    pub fn set_score(&self, k: &str, score: Option<crate::prscore::Score>) {
        let Some(mut pr) = self.get(k) else { return };
        if pr.score == score {
            return;
        }
        pr.score = score;
        self.store(pr, "update", true);
    }

    /// The tower's own merge of it, as it went.
    pub fn set_auto(&self, k: &str, auto: Option<crate::prscore::Auto>) {
        let Some(mut pr) = self.get(k) else { return };
        if pr.auto == auto {
            return;
        }
        pr.auto = auto;
        self.store(pr, "update", true);
    }

    /// Bumped at every change (the event stream's chat diff looks).
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    pub fn list(&self) -> Vec<Pr> {
        self.lock().values().cloned().collect()
    }

    pub fn map(&self) -> BTreeMap<String, Pr> {
        self.lock().clone()
    }

    pub fn get(&self, key: &str) -> Option<Pr> {
        self.lock().get(key).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Pr>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, all: &BTreeMap<String, Pr>) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let f = File { prs: all.values().cloned().collect() };
        // Written beside the watched folder, then moved in: the orb wakes once.
        let tmp = self.path.parent().and_then(Path::parent).unwrap_or(Path::new(".")).join(format!(".prs.json.{}", std::process::id()));
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&f).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// Stores `pr` as it is now, tells listeners (`op`: open, update, closed).
    /// Its review, score and the tower's merge stay as `set_review`,
    /// `set_score` and `set_auto` last put them: a look at GitHub that began
    /// before they moved doesn't take them back.
    fn put(&self, pr: Pr, op: &str) {
        self.store(pr, op, false);
    }

    /// `own`: the tower's own fields (review, score, auto) are `pr`'s.
    fn store(&self, mut pr: Pr, op: &str, own: bool) {
        {
            let mut all = self.lock();
            if !own && let Some(old) = all.get(&pr.key()) {
                keep_own(&mut pr, old);
            }
            all.insert(pr.key(), pr.clone());
            self.save(&all);
        }
        self.version.fetch_add(1, Ordering::SeqCst);
        if let Some(f) = self.on_change.get() {
            f(op, &pr);
        }
    }

    /// A session's turn ended mentioning these PRs: each one not known and
    /// open (GitHub asked now) becomes a chip, its chat entry written; each
    /// known and open one is marked waiting. Returns the open ones (for the
    /// line said to the user). `waiting`: the session finished its turn on
    /// them (or asked to merge).
    pub fn found(&self, found: &[(String, u64)], session: &str, name: &str, space: Option<&str>, waiting: bool) -> Vec<Pr> {
        let now = crate::history::now_ms();
        let mut open = vec![];
        for (repo, n) in found {
            let k = key(repo, *n);
            if let Some(mut pr) = self.get(&k) {
                if pr.open() {
                    if waiting && !pr.waiting {
                        pr.waiting = true;
                        self.put(pr.clone(), "update");
                    }
                    open.push(pr);
                }
                continue;
            }
            let mut pr = Pr {
                repo: repo.clone(),
                number: *n,
                session: session.into(),
                name: name.into(),
                space: space.map(String::from),
                waiting,
                found_ms: now,
                checked_ms: now,
                ..Default::default()
            };
            match fetch(&self.gh, repo, *n) {
                Ok(v) => apply_gh(&mut pr, &v),
                Err(e) => {
                    eprintln!("[prs] {k}: gh can't say ({e:#}); not shown");
                    continue;
                }
            }
            // A merged or closed PR it only mentions: no chip.
            if !pr.open() {
                continue;
            }
            // Its review score, from the start (its plan's prediction, its own files).
            if let Some(f) = self.on_score.get() {
                pr.score = f(&pr);
            }
            eprintln!(
                "[prs] {k} by {session} ({name}): {} into {}, {}{}",
                pr.title,
                pr.base,
                pr.checks_line(),
                pr.score.as_ref().map(|s| format!("; {}", s.line)).unwrap_or_default()
            );
            if self.chat {
                crate::history::append(crate::history::Entry {
                    kind: "pr".into(),
                    id: Some(k.clone()),
                    session: Some(session.into()),
                    name: Some(name.into()).filter(|n: &String| !n.is_empty()),
                    space: space.map(String::from),
                    text: pr.title.clone(),
                    ..Default::default()
                });
            }
            self.put(pr.clone(), "open");
            open.push(pr);
        }
        open
    }

    /// The queue is being looked at (a window opened, a phone asked): the
    /// PRs that wait for a merge are looked at again now, in the background,
    /// so one merged on GitHub leaves the queue at once. At most every 15 s.
    pub fn look_now(&self) {
        static LAST: AtomicU64 = AtomicU64::new(0);
        let now = crate::history::now_ms();
        if now.saturating_sub(LAST.load(Ordering::Relaxed)) < 15_000 {
            return;
        }
        LAST.store(now, Ordering::Relaxed);
        let keys: Vec<String> = self.lock().values().filter(|p| p.open() && p.waiting && !p.settled).map(Pr::key).collect();
        if keys.is_empty() {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            for k in keys {
                me.poll(&k);
            }
        });
    }

    /// The PRs due a look now.
    pub fn due(&self, now: u64) -> Vec<String> {
        self.lock().values().filter(|p| next_check(p).is_some_and(|t| t <= now)).map(Pr::key).collect()
    }

    /// Looks at one PR again. Returns it when it closed just now (merged
    /// elsewhere, or closed), for the daemon to tell the session (`waiting`
    /// as it was before it closed).
    pub fn poll(&self, k: &str) -> Option<Pr> {
        let mut pr = self.get(k)?;
        let before = pr.clone();
        let now = crate::history::now_ms();
        pr.checked_ms = now;
        match fetch(&self.gh, &pr.repo, pr.number) {
            Ok(v) => {
                apply_gh(&mut pr, &v);
                pr.error = None;
            }
            Err(e) => pr.error = Some(format!("{e:#}")),
        }
        let closed_now = before.open() && !pr.open() && pr.error.is_none();
        if closed_now {
            pr.closed_ms = Some(now);
            pr.waiting = false;
            if pr.state == "merged" && pr.merged_by.is_none() {
                pr.merged_by = Some("github".into());
            }
        } else if !before.open() {
            // The look once more after it closed.
            pr.settled = true;
        }
        let changed = strip(&pr) != strip(&before);
        let op = if closed_now { "closed" } else { "update" };
        if changed || closed_now {
            self.put(pr.clone(), op);
        } else {
            // Only when it was looked at changed: saved, nobody told.
            let mut all = self.lock();
            if let Some(old) = all.get(&pr.key()) {
                keep_own(&mut pr, old);
            }
            all.insert(pr.key(), pr.clone());
            self.save(&all);
        }
        // Merged on GitHub while its session waited on it: told as a merge.
        if closed_now
            && pr.state == "merged"
            && before.waiting
            && let Some(f) = self.on_merged.get()
        {
            f(&Pr { waiting: true, ..pr.clone() }, "github");
        }
        // As it was just now: whether its session was waiting on it.
        closed_now.then_some(Pr { waiting: before.waiting, ..pr })
    }

    /// Merged by the user (`by`: mac, phone:<device>): GitHub is asked; when
    /// it says merged, the chip moves and the PR is returned (once: a
    /// second report of the same merge returns None).
    pub fn merged(&self, k: &str, by: &str) -> Option<Pr> {
        let mut pr = self.get(k)?;
        if !pr.open() {
            return None;
        }
        let v = fetch(&self.gh, &pr.repo, pr.number).ok()?;
        apply_gh(&mut pr, &v);
        if pr.state != "merged" {
            return None;
        }
        let now = crate::history::now_ms();
        pr.checked_ms = now;
        pr.closed_ms = Some(now);
        pr.waiting = false;
        pr.merged_by = Some(by.into());
        self.put(pr.clone(), "closed");
        Some(pr)
    }

    /// A phone's confirmation: the PR as GitHub has it now, the gate, and a
    /// token for `device` to merge it with, good for two minutes and for
    /// this head commit only.
    pub fn confirm(&self, k: &str, device: &str) -> Result<(String, Pr, Gate, String)> {
        let mut pr = self.get(k).context("no such PR")?;
        let v = fetch(&self.gh, &pr.repo, pr.number)?;
        apply_gh(&mut pr, &v);
        pr.checked_ms = crate::history::now_ms();
        self.put(pr.clone(), "update");
        let g = gate(&pr);
        let token = token();
        let until = crate::history::now_ms() + 2 * MIN;
        let mut t = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        let now = crate::history::now_ms();
        t.retain(|_, (_, _, _, u)| *u > now);
        t.insert(token.clone(), (k.into(), device.into(), pr.head_sha.clone(), until));
        let method = merge_method(&self.gh, &pr.repo).trim_start_matches("--").to_string();
        Ok((token, pr, g, method))
    }

    /// Takes a phone's token (once): the head commit it was given for.
    pub fn take_token(&self, token: &str, k: &str, device: &str) -> Option<String> {
        let mut t = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        let (key, dev, sha, until) = t.remove(token)?;
        (key == k && dev == device && until > crate::history::now_ms()).then_some(sha)
    }
}

/// The tower's own fields of `old` onto `pr` (a look at GitHub knows none of them).
fn keep_own(pr: &mut Pr, old: &Pr) {
    pr.review = old.review.clone();
    pr.score = old.score.clone();
    pr.auto = old.auto.clone();
}

/// A PR without when and whether it is looked at: what the chip shows.
fn strip(p: &Pr) -> Pr {
    Pr { checked_ms: 0, settled: false, ..p.clone() }
}

fn token() -> String {
    let mut b = [0u8; 16];
    if getrandom(&mut b).is_err() {
        let n = crate::history::now_ms() ^ (std::process::id() as u64) << 32;
        b[..8].copy_from_slice(&n.to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn getrandom(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn finds_pr_links_in_what_a_session_says_and_prints() {
        let said = "Opened [PR #1250](https://github.com/eyzhub/eyz-mediaroom-serverless/pull/1250) into main. \
                    Also see https://github.com/eyzhub/eyzflow-chat-sdk/pull/88/files and again \
                    https://github.com/eyzhub/eyz-mediaroom-serverless/pull/1250. Not a PR: https://github.com/eyzhub/x/issues/3, \
                    github.com/a/b/pull/ and https://github.com/eyzhub/eyz-mediaroom-serverless/actions/runs/42645505320.";
        assert_eq!(links(said), vec![("eyzhub/eyz-mediaroom-serverless".to_string(), 1250), ("eyzhub/eyzflow-chat-sdk".to_string(), 88)]);
    }

    #[test]
    fn a_turns_text_is_what_it_said_and_printed_since_its_prompt() {
        let lines = [
            json!({"type":"user","message":{"content":"earlier prompt"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"old https://github.com/o/r/pull/1"}]}}),
            json!({"type":"user","message":{"content":[{"type":"text","text":"open the PR"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"u1","name":"Bash","input":{"command":"gh pr list --state open"}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"u1","content":"https://github.com/o/r/pull/3\nhttps://github.com/o/r/pull/4"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"u2","name":"Bash","input":{"command":"git push && gh pr create --base dev --fill"}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"u2","content":[{"type":"text","text":"https://github.com/o/r/pull/7\n"}]}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"PR is open."}]}}),
        ];
        let jsonl: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let t = turn_text(&jsonl);
        // The PR it created, not the ones `gh pr list` showed, nor the earlier turn's.
        assert_eq!(links(&t), vec![("o/r".to_string(), 7)], "{t}");
        assert!(t.contains("PR is open."));
    }

    fn gh_json(state: &str, mergeable: &str, checks: Value) -> Value {
        json!({
            "number": 12, "title": "release: tickets", "url": "https://github.com/o/app/pull/12", "state": state, "isDraft": false,
            "mergeable": mergeable, "baseRefName": "main", "headRefName": "dev", "headRefOid": "abc123", "statusCheckRollup": checks,
        })
    }

    #[test]
    fn checks_roll_up_to_each_checks_latest_run() {
        let run = |name: &str, status: &str, conclusion: &str, at: &str| {
            json!({"__typename":"CheckRun","name":name,"workflowName":"CI","status":status,"conclusion":conclusion,"startedAt":at})
        };
        // A failed run, then the same check re-run and passing: passing.
        let c = rollup(&json!([run("test", "COMPLETED", "FAILURE", "2026-11-09T10:00:00Z"), run("test", "COMPLETED", "SUCCESS", "2026-11-09T11:00:00Z")]));
        assert_eq!((c.state.as_str(), c.passed, c.total), ("passing", 1, 1));
        let c = rollup(&json!([
            run("test", "COMPLETED", "SUCCESS", "a"),
            run("lint", "COMPLETED", "FAILURE", "a"),
            run("deploy", "IN_PROGRESS", "", "a"),
            run("docs", "COMPLETED", "SKIPPED", "a"),
            {"__typename":"StatusContext","context":"vercel","state":"PENDING"},
        ]));
        assert_eq!(c.state, "failing");
        assert_eq!(c.failing, vec!["CI / lint"]);
        assert_eq!(c.pending, vec!["CI / deploy", "vercel"]);
        assert_eq!((c.passed, c.total), (2, 5));
        assert_eq!(rollup(&json!([])).state, "none");
        let c = rollup(&json!([run("deploy", "QUEUED", "", "a"), run("test", "COMPLETED", "SUCCESS", "a")]));
        assert_eq!(c.state, "pending");
    }

    #[test]
    fn the_merge_button_says_why_not_and_what_to_mind() {
        let mut pr = Pr::default();
        apply_gh(&mut pr, &gh_json("OPEN", "MERGEABLE", json!([{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS"}])));
        let g = gate(&pr);
        assert_eq!((g.blocked.as_deref(), g.needs_override.as_deref()), (None, None));
        assert_eq!(g.warnings, vec!["It merges into main: that ships to production."]);
        // Failing checks: only with a second, explicit confirmation.
        apply_gh(&mut pr, &gh_json("OPEN", "MERGEABLE", json!([{"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"FAILURE"}])));
        let g = gate(&pr);
        assert!(g.blocked.is_none());
        assert_eq!(g.needs_override.as_deref(), Some("1 check failing: lint. Merge anyway only if you know the failure is unrelated."));
        // Conflicts, a draft, merged: not at all.
        apply_gh(&mut pr, &gh_json("OPEN", "CONFLICTING", json!([])));
        assert_eq!(gate(&pr).blocked.as_deref(), Some("It has conflicts with main: they need resolving first."));
        let mut draft = pr.clone();
        draft.mergeable = "mergeable".into();
        draft.draft = true;
        assert!(gate(&draft).blocked.unwrap().contains("draft"));
        apply_gh(&mut pr, &gh_json("MERGED", "UNKNOWN", json!([])));
        assert_eq!(gate(&pr).blocked.as_deref(), Some("It's merged already."));
        // Into dev, checks running: allowed, said.
        apply_gh(&mut pr, &gh_json("OPEN", "UNKNOWN", json!([{"__typename":"CheckRun","name":"test","status":"IN_PROGRESS"}])));
        pr.base = "dev".into();
        let g = gate(&pr);
        assert!(g.blocked.is_none() && g.needs_override.is_none());
        assert_eq!(g.warnings, vec!["1 check is still running.", "GitHub hasn't worked out yet whether it merges cleanly."]);
        assert_eq!(pr.view()["merge"]["can"], true);
        assert_eq!(pr.view()["key"], "o/app#12");
    }

    #[test]
    fn polled_gently_while_open_and_once_after_it_closes() {
        let mut pr = Pr { state: "open".into(), found_ms: 0, checked_ms: 10 * MIN, ..Default::default() };
        pr.checks.state = "pending".into();
        assert_eq!(next_check(&pr), Some(10 * MIN + MIN / 2), "checks running: every 30 s");
        pr.checks.state = "passing".into();
        pr.mergeable = "mergeable".into();
        assert_eq!(next_check(&pr), Some(10 * MIN + 3 * MIN / 2), "settled: every 90 s");
        pr.checked_ms = 5 * 24 * 60 * MIN;
        assert_eq!(next_check(&pr), Some(5 * 24 * 60 * MIN + 15 * MIN), "days old: every 15 min");
        // Waiting for the user's merge: every 30 s however old (it is often merged on GitHub).
        pr.waiting = true;
        assert_eq!(next_check(&pr), Some(5 * 24 * 60 * MIN + MIN / 2), "waiting: every 30 s");
        pr.waiting = false;
        pr.state = "merged".into();
        pr.closed_ms = Some(100 * MIN);
        assert_eq!(next_check(&pr), Some(101 * MIN), "once more after it closed");
        pr.settled = true;
        assert_eq!(next_check(&pr), None);
    }

    /// A fake `gh` in a folder of its own: `pr view N` answers `pr-N.json`,
    /// `repo view` says SQUASH, `pr merge N` marks `pr-N.json` merged; every
    /// call is logged to `calls.log`. Never the real gh: nothing is merged.
    pub(crate) fn fake_gh(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tower-prs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("gh");
        std::fs::write(
            &script,
            r#"#!/bin/bash
D="$(cd "$(dirname "$0")" && pwd)"
echo "$*" >> "$D/calls.log"
case "$1 $2" in
  "pr view") f="$D/pr-$3.json"; [ -f "$f" ] && cat "$f" || { echo "no pull requests found" >&2; exit 1; } ;;
  "repo view") echo '{"viewerDefaultMergeMethod":"SQUASH"}' ;;
  "pr merge") sed -i '' 's/"state":"OPEN"/"state":"MERGED"/' "$D/pr-$3.json" ;;
  *) echo "fake gh: $*" >&2; exit 1 ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    pub(crate) fn serve(gh: &Path, n: u64, state: &str, checks: Value, sha: &str) {
        let mut v = gh_json(state, "MERGEABLE", checks);
        v["number"] = json!(n);
        v["url"] = json!(format!("https://github.com/o/app/pull/{n}"));
        v["headRefOid"] = json!(sha);
        std::fs::write(gh.with_file_name(format!("pr-{n}.json")), serde_json::to_string(&v).unwrap()).unwrap();
    }

    pub(crate) fn calls(gh: &Path) -> String {
        std::fs::read_to_string(gh.with_file_name("calls.log")).unwrap_or_default()
    }

    fn passing() -> Value {
        json!([{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS"}])
    }

    #[test]
    fn a_pr_a_session_opened_is_a_chip_kept_across_restarts() {
        let gh = fake_gh("found");
        serve(&gh, 12, "OPEN", json!([{"__typename":"CheckRun","name":"test","status":"IN_PROGRESS"}]), "abc");
        serve(&gh, 9, "MERGED", passing(), "old");
        let path = gh.with_file_name("prs.json");
        let prs = Prs::at(path.clone(), gh.clone());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        prs.on_change(Box::new(move |op, pr| s2.lock().unwrap().push(format!("{op} {} {}", pr.key(), pr.checks.state))));
        let found = vec![("o/app".to_string(), 12), ("o/app".to_string(), 9), ("o/app".to_string(), 404)];
        let open = prs.found(&found, "t58", "mediaroom", Some("b2c-tickets"), false);
        // Only the open one: a merged PR it mentions, or one gh can't find, is no chip.
        assert_eq!(open.iter().map(Pr::key).collect::<Vec<_>>(), ["o/app#12"]);
        let pr = prs.get("o/app#12").unwrap();
        assert_eq!((pr.session.as_str(), pr.space.as_deref(), pr.base.as_str(), pr.waiting), ("t58", Some("b2c-tickets"), "main", false));
        assert_eq!(*seen.lock().unwrap(), ["open o/app#12 pending"]);
        // Its turn ended on it: waiting for the user.
        prs.found(&found[..1], "t58", "mediaroom", Some("b2c-tickets"), true);
        assert!(prs.get("o/app#12").unwrap().waiting);
        // Another start: the same.
        let again = Prs::at(path, gh.clone());
        assert_eq!(again.get("o/app#12").unwrap(), prs.get("o/app#12").unwrap());
        assert!(!calls(&gh).contains("pr merge"), "nothing merged");
    }

    #[test]
    fn polling_follows_it_in_place_and_tells_once_when_it_closes() {
        let gh = fake_gh("poll");
        serve(&gh, 12, "OPEN", json!([{"__typename":"CheckRun","name":"test","status":"IN_PROGRESS"}]), "abc");
        let prs = Prs::at(gh.with_file_name("prs.json"), gh.clone());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        prs.on_change(Box::new(move |op, pr| s2.lock().unwrap().push(format!("{op} {} {}", pr.state, pr.checks.state))));
        prs.found(&[("o/app".into(), 12)], "t58", "mediaroom", None, true);
        // Nothing changed: nobody told.
        assert_eq!(prs.poll("o/app#12"), None);
        // The checks pass; then it's merged on GitHub.
        serve(&gh, 12, "OPEN", passing(), "abc");
        assert_eq!(prs.poll("o/app#12"), None);
        serve(&gh, 12, "MERGED", passing(), "abc");
        let closed = prs.poll("o/app#12").expect("closed just now");
        assert_eq!((closed.merged_by.as_deref(), closed.waiting), (Some("github"), true), "its session was waiting");
        assert!(!prs.get("o/app#12").unwrap().waiting, "not any more");
        // Once more after it closed, then never.
        assert_eq!(prs.poll("o/app#12"), None);
        let pr = prs.get("o/app#12").unwrap();
        assert!(pr.settled && next_check(&pr).is_none());
        assert_eq!(*seen.lock().unwrap(), ["open open pending", "update open passing", "closed merged passing"]);
    }

    #[test]
    fn a_prs_review_is_kept_through_its_polls_and_said_before_a_merge() {
        let gh = fake_gh("review");
        serve(&gh, 12, "OPEN", passing(), "abc");
        let prs = Prs::at(gh.with_file_name("prs.json"), gh.clone());
        prs.found(&[("o/app".into(), 12)], "t58", "mediaroom", Some("shop"), true);
        // A look at GitHub that began before the review moved doesn't take it back.
        let stale = prs.get("o/app#12").unwrap();
        let brief = crate::reviews::Brief { status: "changes".into(), rounds: 2, critical: 2, line: "Review: 2 critical, round 2".into(), ..Default::default() };
        prs.set_review("o/app#12", Some(brief.clone()));
        prs.put(Pr { title: "renamed".into(), ..stale }, "update");
        let pr = prs.get("o/app#12").unwrap();
        assert_eq!((pr.title.as_str(), pr.review.as_ref()), ("renamed", Some(&brief)));
        serve(&gh, 12, "OPEN", json!([]), "abc");
        prs.poll("o/app#12");
        assert_eq!(prs.get("o/app#12").unwrap().review, Some(brief.clone()));
        // The merge stays the user's: said plainly, not blocked.
        let g = gate(&prs.get("o/app#12").unwrap());
        assert!(g.blocked.is_none() && g.warnings.contains(&"The review isn't agreed: 2 critical issues open, round 2.".to_string()), "{g:?}");
        assert_eq!(prs.get("o/app#12").unwrap().view()["review"]["line"], "Review: 2 critical, round 2");
    }

    #[test]
    fn merging_asks_github_again_and_never_forces() {
        let gh = fake_gh("merge");
        serve(&gh, 12, "OPEN", json!([{"__typename":"CheckRun","name":"lint","status":"COMPLETED","conclusion":"FAILURE"}]), "abc");
        // Failing checks: not without the second confirmation.
        assert!(matches!(merge(&gh, "o/app", 12, "abc", false), Err(Refusal::NeedsOverride(_))));
        // New commits since it was confirmed: look again.
        assert!(matches!(merge(&gh, "o/app", 12, "old", true), Err(Refusal::Changed(_))));
        assert!(!calls(&gh).contains("pr merge"));
        // Confirmed twice (the failure is unrelated): the repo's method, pinned to the commit.
        let pr = merge(&gh, "o/app", 12, "abc", true).unwrap();
        assert_eq!(pr.state, "merged");
        let log = calls(&gh);
        assert!(log.contains("pr merge 12 --repo o/app --squash --match-head-commit abc"), "{log}");
        assert!(!log.contains("--admin") && !log.contains("--auto"), "{log}");
        // Merged already: blocked.
        assert!(matches!(merge(&gh, "o/app", 12, "abc", true), Err(Refusal::Blocked(_))));
    }

    #[test]
    fn a_merge_is_reported_once_and_a_phones_token_works_once() {
        let gh = fake_gh("merged");
        serve(&gh, 12, "OPEN", passing(), "abc");
        let prs = Prs::at(gh.with_file_name("prs.json"), gh.clone());
        prs.found(&[("o/app".into(), 12)], "t58", "mediaroom", None, true);
        let (token, pr, g, method) = prs.confirm("o/app#12", "d_phone").unwrap();
        assert_eq!((pr.head_sha.as_str(), g.blocked.is_none(), method.as_str()), ("abc", true, "squash"));
        assert_eq!(prs.take_token(&token, "o/app#12", "d_other"), None, "another device can't use it");
        let (token, ..) = prs.confirm("o/app#12", "d_phone").unwrap();
        assert_eq!(prs.take_token(&token, "o/app#12", "d_phone").as_deref(), Some("abc"));
        assert_eq!(prs.take_token(&token, "o/app#12", "d_phone"), None, "once");
        // Not merged yet: nothing to report.
        assert_eq!(prs.merged("o/app#12", "mac"), None);
        merge(&gh, "o/app", 12, "abc", false).unwrap();
        let m = prs.merged("o/app#12", "mac").unwrap();
        assert_eq!((m.state.as_str(), m.merged_by.as_deref()), ("merged", Some("mac")));
        assert_eq!(prs.merged("o/app#12", "phone:d_phone"), None, "told once");
    }
}
