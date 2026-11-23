//! Which MCP servers each Claude account has, and which account a task
//! that needs one goes to.
//!
//! The accounts are the claude-accounts mod's (`cca`, see `accounts`). Each
//! has a profile folder of its own (cca's `CLAUDE_CONFIG_DIR` for it; unset
//! for the default: `~/.claude`, with `~/.claude.json`), and with it its own
//! MCP servers:
//!
//! - **claude.ai connectors** (`claude.ai Claude Docs`, `claude.ai Figma`…):
//!   the account's own on claude.ai, only seen by asking Claude Code.
//! - **Plugins' servers** (`plugin:sentry:sentry`): the plugins the profile
//!   has on (`enabledPlugins` in its `settings.json`), each signed in (or
//!   not) per account.
//! - **Local servers**: its `.claude.json`'s `mcpServers` (every folder), and
//!   those of one project only (`projects.<folder>.mcpServers`).
//!
//! **Reading them** (`refresh`): the profile folder is asked of cca itself
//! (`profile_of`: cca runs a stand-in `claude` that prints it), so nothing
//! here knows an account by name. Its config is read (`local`), then `cca
//! <account> mcp list` is run in the person's login shell, as a session
//! runs, from their home folder: Claude Code's own word on each server,
//! connected, needing sign-in, or failing (`parse_list`). The two are put
//! together (`merge`): a live check that fails keeps the last one it had,
//! and says how old. Kept in `mcps.json` in the state folder; the daemon
//! reads them again every `EVERY` (`keep_fresh`), and a command that finds
//! them older than `STALE` reads them itself.
//!
//! **Has.** Only a connected server is one a session can use. One that needs
//! sign-in, or fails, is there but not usable; one only read from config
//! (no live check answered) is taken as usable, and said to be unchecked.
//!
//! **Where a task goes** (`choose`): its needs are what `--needs-mcp` names
//! and the services its words name (`named_in`); an account asked for is
//! kept (and told what it lacks); else the first account with usage left
//! that has them all, else the one with the most, else the one it would
//! have had. What it has, lacks, or nobody has is said plainly.
//!
//! **A failover** (`lost`) keeps a session's servers where it can: those it
//! used (its transcript's `mcp__<server>__…` calls) that its account has
//! connected and another hasn't count against that one, and a move that
//! loses some says so.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::accounts::Account;

/// How often the daemon reads them again.
pub const EVERY: Duration = Duration::from_secs(10 * 60);
/// Older than this, a command reads them again itself.
pub const STALE_MS: u64 = 25 * 60_000;
/// How long `claude mcp list` may take under one account (it starts every stdio server).
const LIST_LIMIT: Duration = Duration::from_secs(90);

// ---- what is kept ----

/// Where a server comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The account's own on claude.ai.
    Connector,
    /// A plugin's (`plugin:<plugin>:<server>`).
    Plugin,
    /// The profile's own, in every folder.
    Local,
    /// The profile's own, in one project folder only.
    Project,
}

impl Source {
    pub fn word(self) -> &'static str {
        match self {
            Source::Connector => "claude.ai",
            Source::Plugin => "plugin",
            Source::Local => "local",
            Source::Project => "project",
        }
    }
}

/// What Claude Code says of a server under an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Connected,
    NeedsAuth,
    Failed,
    /// Only read from config: no live check said.
    Unchecked,
}

impl Status {
    /// A session on the account can use it (or, unchecked, should).
    pub fn usable(self) -> bool {
        matches!(self, Status::Connected | Status::Unchecked)
    }

    pub fn word(self) -> &'static str {
        match self {
            Status::Connected => "connected",
            Status::NeedsAuth => "needs sign-in",
            Status::Failed => "fails to connect",
            Status::Unchecked => "configured (not checked)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Server {
    /// As Claude Code names it: `claude.ai Claude Docs`, `plugin:sentry:sentry`, `blender`.
    pub name: String,
    /// As people call it: `Claude Docs`, `Sentry`, `blender`.
    pub label: String,
    pub source: Source,
    pub status: Status,
    /// Its URL, or the command that runs it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// A `Project` server's folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// One account's servers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Of {
    pub account: String,
    /// Its profile folder (`CLAUDE_CONFIG_DIR`, or `~/.claude` for the default).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub dir: String,
    pub servers: Vec<Server>,
    /// When its config was read.
    pub read_ms: u64,
    /// When `claude mcp list` last answered under it (None: never).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_ms: Option<u64>,
    /// Why the last live check (or the read) didn't answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `mcps.json`: every account's, in the mod's order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Map {
    pub at_ms: u64,
    pub accounts: Vec<Of>,
}

impl Map {
    pub fn of(&self, account: &str) -> Option<&Of> {
        self.accounts.iter().find(|o| o.account == account)
    }
}

pub fn path() -> PathBuf {
    // Tests keep theirs in a temporary folder, never in the user's state folder.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("herder-tower-test-{}", std::process::id())).join("mcps.json");
    }
    crate::config::state_dir().join("mcps.json")
}

/// What was read last (empty: never).
pub fn load() -> Map {
    load_from(&path())
}

pub fn load_from(path: &Path) -> Map {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_to(path: &Path, m: &Map) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(m)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// What was read, read again first when it is older than `STALE_MS` (or
/// was never read), as a command needs it. Never fails: what there is.
pub fn current() -> Map {
    let m = load();
    let fresh = m.at_ms > 0 && crate::history::now_ms().saturating_sub(m.at_ms) < STALE_MS;
    if fresh || cfg!(test) {
        return m;
    }
    refresh().unwrap_or(m)
}

// ---- names ----

/// How people write the services an MCP server is for, by their lowercase
/// name. The words a task names a service by (`named_in`) are these, and
/// the connectors' and plugins' own; a word that is also plain English
/// (Linear, Monday, Square…) only counts through `--needs-mcp`.
pub const SERVICES: &[&str] = &[
    "YouTrack",
    "Figma",
    "Sentry",
    "Stripe",
    "Vercel",
    "Notion",
    "Jira",
    "Confluence",
    "Atlassian",
    "Slack",
    "Gmail",
    "Google Drive",
    "Google Calendar",
    "Asana",
    "Supabase",
    "Cloudflare",
    "Netlify",
    "Canva",
    "HubSpot",
    "Intercom",
    "PayPal",
    "Zapier",
    "Datadog",
    "PostHog",
    "Blender",
    "Claude Docs",
    "Airtable",
    "ClickUp",
    "Miro",
    "Webflow",
    "Shopify",
    "Twilio",
    "Context7",
];

/// A server name's words, lowercase: `plugin:sentry:sentry` → plugin, sentry, sentry.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Lowercase letters and digits only: `Claude Docs` → `claudedocs`.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// A service's name as people write it: `figma` → `Figma`; one not known, as it is.
fn pretty(word: &str) -> String {
    SERVICES.iter().find(|s| squash(s) == squash(word)).map(|s| s.to_string()).unwrap_or_else(|| word.to_string())
}

/// What people call a server: a connector's name past `claude.ai `, a
/// plugin's server by its service, a local one as it is named.
pub fn label_of(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("claude.ai ") {
        return rest.trim().to_string();
    }
    if let Some(rest) = name.strip_prefix("plugin:") {
        let server = rest.rsplit(':').next().unwrap_or(rest);
        return pretty(server);
    }
    name.to_string()
}

fn source_of(name: &str) -> Source {
    if name.starts_with("claude.ai ") {
        Source::Connector
    } else if name.starts_with("plugin:") {
        Source::Plugin
    } else {
        Source::Local
    }
}

/// The name a server's tools carry in a session (`mcp__<key>__<tool>`):
/// what isn't a letter, digit, `_` or `-` becomes `_`, as Claude Code does
/// (`claude.ai Claude Docs` → `claude_ai_Claude_Docs`).
pub fn tool_key(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// Whether `need` (a word, as a task or `--needs-mcp` names it) is this
/// server: its label, its name, or a word of its name that says what it is
/// (never `plugin`, `claude`, `ai`, `mcp`).
pub fn is(need: &str, s: &Server) -> bool {
    let n = squash(need);
    if n.is_empty() {
        return false;
    }
    if n == squash(&s.label) || n == squash(&s.name) || n == squash(&tool_key(&s.name)) {
        return true;
    }
    words(&s.name).iter().any(|w| !matches!(w.as_str(), "plugin" | "claude" | "ai" | "mcp" | "server") && *w == n)
}

// ---- reading an account's ----

/// An account's profile folder and its `.claude.json`, as cca runs claude for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub dir: PathBuf,
    pub config: PathBuf,
}

/// The profile of a `CLAUDE_CONFIG_DIR` (empty: the default one).
pub fn profile_at(config_dir: &str, home: &Path) -> Profile {
    let d = config_dir.trim();
    if d.is_empty() {
        Profile { dir: home.join(".claude"), config: home.join(".claude.json") }
    } else {
        let dir = PathBuf::from(d.trim_end_matches('/'));
        Profile { config: dir.join(".claude.json"), dir }
    }
}

/// The stand-in `claude` cca runs when asked for an account's profile: it
/// only prints the folder cca gave it.
const PROBE: &str = "#!/bin/sh\nprintf 'CLAUDE_CONFIG_DIR\\t%s\\n' \"${CLAUDE_CONFIG_DIR-}\"\n";

/// The profile cca gives `account`: cca runs a stand-in `claude` (first on
/// PATH) that prints its `CLAUDE_CONFIG_DIR`. Nothing else runs.
pub fn profile_of(cca: &Path, account: &str) -> Result<Profile> {
    if cfg!(test) {
        bail!("tests never run cca");
    }
    let dir = crate::config::state_dir().join("mcp-probe");
    std::fs::create_dir_all(&dir)?;
    let probe = dir.join("claude");
    if std::fs::read_to_string(&probe).ok().as_deref() != Some(PROBE) {
        std::fs::write(&probe, PROBE)?;
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755))?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(std::iter::once(dir.clone()).chain(std::env::split_paths(&path)))?;
    let mut cmd = Command::new(cca);
    cmd.arg(account).env("PATH", path).env_remove("CLAUDE_CONFIG_DIR").env_remove("CCA_ACCOUNT");
    let out = crate::hc::output_within(cmd, "cca", Duration::from_secs(15))?;
    let line = out.stdout.lines().find_map(|l| l.strip_prefix("CLAUDE_CONFIG_DIR\t"));
    match line {
        Some(d) if out.code == 0 => Ok(profile_at(d, &dirs::home_dir().unwrap_or_default())),
        _ => bail!("cca {account} didn't say its profile: {}", out.stderr.trim().trim_start_matches("cca: ")),
    }
}

/// The servers a `.claude.json` names: its own (`Local`) and each
/// project's (`Project`), all unchecked.
pub fn of_config(v: &Value) -> Vec<Server> {
    let mut out = vec![];
    let mut add = |name: &str, def: &Value, project: Option<&str>| {
        out.push(Server {
            name: name.to_string(),
            label: label_of(name),
            source: if project.is_some() { Source::Project } else { Source::Local },
            status: Status::Unchecked,
            target: target_of(def),
            project: project.map(String::from),
        });
    };
    for (name, def) in v["mcpServers"].as_object().into_iter().flatten() {
        add(name, def, None);
    }
    for (folder, p) in v["projects"].as_object().into_iter().flatten() {
        for (name, def) in p["mcpServers"].as_object().into_iter().flatten() {
            add(name, def, Some(folder));
        }
    }
    out
}

/// A server definition's URL, or its command line.
fn target_of(def: &Value) -> String {
    if let Some(u) = def["url"].as_str() {
        return u.to_string();
    }
    let mut parts: Vec<String> = def["command"].as_str().map(String::from).into_iter().collect();
    parts.extend(def["args"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)));
    parts.join(" ")
}

/// The servers of the plugins a profile has on (`settings.json`'s
/// `enabledPlugins`), from `installed_plugins.json` and each plugin's own
/// `.mcp.json` or `.claude-plugin/plugin.json` (`read`: a JSON file, by its
/// path). A plugin installed for one project only is that project's.
pub fn of_plugins(settings: &Value, installed: &Value, read: &dyn Fn(&Path) -> Option<Value>) -> Vec<Server> {
    let mut out: Vec<Server> = vec![];
    for (key, on) in settings["enabledPlugins"].as_object().into_iter().flatten() {
        if on.as_bool() != Some(true) {
            continue;
        }
        let plugin = key.split('@').next().unwrap_or(key);
        let mut seen = BTreeSet::new();
        for inst in installed["plugins"][key.as_str()].as_array().into_iter().flatten() {
            let Some(at) = inst["installPath"].as_str() else { continue };
            let project = inst["projectPath"].as_str().filter(|_| inst["scope"].as_str() != Some("user")).map(String::from);
            if !seen.insert((at.to_string(), project.clone())) {
                continue;
            }
            let at = Path::new(at);
            let servers = read(&at.join(".mcp.json"))
                .map(|v| v["mcpServers"].clone())
                .filter(Value::is_object)
                .or_else(|| read(&at.join(".claude-plugin/plugin.json")).map(|v| v["mcpServers"].clone()).filter(Value::is_object));
            for (server, def) in servers.as_ref().and_then(Value::as_object).into_iter().flatten() {
                let name = format!("plugin:{plugin}:{server}");
                if out.iter().any(|s| s.name == name && s.project == project) {
                    continue;
                }
                out.push(Server {
                    label: label_of(&name),
                    source: Source::Plugin,
                    status: Status::Unchecked,
                    target: target_of(def),
                    project: project.clone(),
                    name,
                });
            }
        }
    }
    out
}

/// The servers Claude Code noted as needing sign-in in this profile
/// (`mcp-needs-auth-cache.json`).
pub fn needs_sign_in(v: &Value) -> BTreeSet<String> {
    v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default()
}

/// The connectors a profile has used (`claudeAiMcpEverConnected`): there
/// when no live check answered.
pub fn connectors_seen(v: &Value) -> Vec<Server> {
    v["claudeAiMcpEverConnected"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| Server { name: name.to_string(), label: label_of(name), source: Source::Connector, status: Status::Unchecked, target: String::new(), project: None })
        .collect()
}

fn read_json(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str(&t).ok())
}

/// A profile's servers, from its files only (every one unchecked, or
/// needing sign-in where Claude Code noted it).
pub fn local(p: &Profile) -> Result<Vec<Server>> {
    let config = read_json(&p.config).with_context(|| format!("no readable {}", p.config.display()))?;
    let settings = read_json(&p.dir.join("settings.json")).unwrap_or_default();
    let installed = read_json(&p.dir.join("plugins/installed_plugins.json")).unwrap_or_default();
    let auth = needs_sign_in(&read_json(&p.dir.join("mcp-needs-auth-cache.json")).unwrap_or_default());
    let mut out = connectors_seen(&config);
    out.extend(of_plugins(&settings, &installed, &read_json));
    out.extend(of_config(&config));
    for s in out.iter_mut() {
        if auth.contains(&s.name) {
            s.status = Status::NeedsAuth;
        }
    }
    Ok(out)
}

/// What `claude mcp list` printed: one server a line, `<name>: <url or
/// command> - <status>` (the name up to the first `: `, the status past the
/// last ` - `). A status it doesn't know is no connection.
pub fn parse_list(text: &str) -> Vec<Server> {
    let mut out = vec![];
    for line in text.lines().map(str::trim) {
        let Some((name, rest)) = line.split_once(": ") else { continue };
        let Some((target, status)) = rest.rsplit_once(" - ") else { continue };
        let name = name.trim();
        if name.is_empty() || name.contains('…') {
            continue;
        }
        let st = status.to_lowercase();
        let status = if st.contains("connected") && !st.contains("not connected") && !st.contains("disconnected") {
            Status::Connected
        } else if st.contains("auth") {
            Status::NeedsAuth
        } else if st.contains("fail") || st.contains("error") || st.contains("✗") {
            Status::Failed
        } else {
            continue;
        };
        let target = target.trim().trim_end_matches(" (HTTP)").trim_end_matches(" (SSE)").to_string();
        out.push(Server { name: name.to_string(), label: label_of(name), source: source_of(name), status, target, project: None });
    }
    out
}

/// `claude mcp list` under `account` (`cca <account> mcp list`), in the
/// person's login shell, from their home folder (no project's own servers).
fn list_live(cca: &Path, account: &str) -> Result<Vec<Server>> {
    if cfg!(test) {
        bail!("tests never run claude");
    }
    let argv = crate::accounts::in_login_shell(&crate::accounts::login_shell(), vec![cca.to_string_lossy().into_owned(), account.into(), "mcp".into(), "list".into()]);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(dirs::home_dir().unwrap_or_else(|| "/".into())).env_remove("CLAUDE_CONFIG_DIR").env_remove("CCA_ACCOUNT");
    let out = crate::hc::output_within(cmd, "claude mcp list", LIST_LIMIT)?;
    let servers = parse_list(&out.stdout);
    if out.code != 0 && servers.is_empty() {
        let why = out.stderr.lines().chain(out.stdout.lines()).map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("it failed").to_string();
        bail!("claude mcp list under {account}: {why}");
    }
    Ok(servers)
}

/// An account's servers: what the live check said (when it answered: it
/// is Claude Code's word), else the last one that answered (`prev`), then
/// what only its files have (one project's servers, a plugin the check
/// didn't list), as they have it.
pub fn merge(account: &str, dir: &str, local: Vec<Server>, live: Result<Vec<Server>, String>, prev: Option<&Of>, now_ms: u64) -> Of {
    let (mut servers, checked_ms, error) = match live {
        Ok(l) => (l, Some(now_ms), None),
        Err(e) => match prev.filter(|p| p.checked_ms.is_some()) {
            Some(p) => (p.servers.iter().filter(|s| s.source != Source::Project).cloned().collect(), p.checked_ms, Some(e)),
            None => (vec![], None, Some(e)),
        },
    };
    for s in local {
        match servers.iter_mut().find(|x| x.name == s.name && x.project == s.project) {
            // The live word stands; the files know where it comes from and what it runs.
            Some(x) => {
                if s.source == Source::Project || x.source == Source::Local {
                    x.source = s.source;
                }
                if x.target.is_empty() {
                    x.target = s.target;
                }
            }
            None => servers.push(s),
        }
    }
    servers.sort_by_key(|s| (s.project.is_some(), s.source, s.label.to_lowercase()));
    Of { account: account.into(), dir: dir.into(), servers, read_ms: now_ms, checked_ms, error }
}

/// Every account's servers, read now (each account's files, and `claude mcp
/// list` under it), kept in `mcps.json`. Empty when the mod isn't installed.
pub fn refresh() -> Result<Map> {
    if cfg!(test) {
        bail!("tests never read the real accounts");
    }
    let list = crate::accounts::list()?;
    let Some(cca) = crate::accounts::cca_bin() else { return Ok(Map::default()) };
    let prev = load();
    let now = crate::history::now_ms();
    let home = dirs::home_dir().unwrap_or_default();
    // Each account's check runs beside the others': they take seconds each.
    let handles: Vec<_> = list
        .iter()
        .map(|a| {
            let (cca, name, home) = (cca.clone(), a.name.clone(), home.clone());
            std::thread::spawn(move || {
                let profile = profile_of(&cca, &name).map_err(|e| format!("{e:#}"));
                let local = match &profile {
                    Ok(p) => local(p).map_err(|e| format!("{e:#}")),
                    Err(e) => Err(e.clone()),
                };
                let live = list_live(&cca, &name).map_err(|e| format!("{e:#}"));
                let dir = profile.as_ref().map(|p| p.dir.to_string_lossy().into_owned()).unwrap_or_else(|_| {
                    // (Only to show: the default's is known without asking.)
                    if name == "default" { home.join(".claude").to_string_lossy().into_owned() } else { String::new() }
                });
                (name, dir, local, live)
            })
        })
        .collect();
    let mut m = Map { at_ms: now, accounts: vec![] };
    for h in handles {
        let Ok((name, dir, local, live)) = h.join() else { continue };
        let mut of = merge(&name, &dir, local.as_ref().cloned().unwrap_or_default(), live, prev.of(&name), now);
        if let (Err(e), None) = (&local, &of.error) {
            of.error = Some(e.clone());
        }
        m.accounts.push(of);
    }
    save_to(&path(), &m)?;
    Ok(m)
}

/// The daemon's: read them now and every `EVERY`; `changed` gets the map
/// each time what the brains are told of it (`prompt_section`) changes.
pub fn keep_fresh(changed: impl Fn(&Map) + Send + 'static) {
    let _ = std::thread::Builder::new().name("mcps".into()).spawn(move || {
        // Past the daemon's own start first.
        std::thread::sleep(Duration::from_secs(20));
        let mut told = prompt_section(&load());
        loop {
            match refresh() {
                Ok(m) => {
                    let now = prompt_section(&m);
                    if now != told {
                        eprintln!("[mcps] the accounts' MCP servers changed: the brains are told");
                        changed(&m);
                        told = now;
                    }
                    for o in m.accounts.iter().filter(|o| o.error.is_some()) {
                        eprintln!("[mcps] {}: {}", o.account, o.error.as_deref().unwrap_or(""));
                    }
                }
                Err(e) => eprintln!("[mcps] couldn't read the accounts' MCP servers: {e:#}"),
            }
            std::thread::sleep(EVERY);
        }
    });
}

// ---- what an account has ----

/// What an account has of a need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Have {
    Connected,
    Unchecked,
    NeedsAuth,
    Failed,
    No,
}

impl Have {
    pub fn usable(self) -> bool {
        matches!(self, Have::Connected | Have::Unchecked)
    }
}

/// Whether a project server counts in `folder`: it is that project, or in it.
fn in_project(project: &str, folder: Option<&str>) -> bool {
    let Some(f) = folder else { return false };
    let (p, f) = (project.trim_end_matches('/'), f.trim_end_matches('/'));
    f == p || f.starts_with(&format!("{p}/"))
}

/// What `of` has of `need` (in `folder`: a project's own servers count only there). The best it has.
pub fn has(of: &Of, need: &str, folder: Option<&str>) -> Have {
    of.servers
        .iter()
        .filter(|s| s.project.as_deref().is_none_or(|p| in_project(p, folder)))
        .filter(|s| is(need, s))
        .map(|s| match s.status {
            Status::Connected => Have::Connected,
            Status::Unchecked => Have::Unchecked,
            Status::NeedsAuth => Have::NeedsAuth,
            Status::Failed => Have::Failed,
        })
        .min()
        .unwrap_or(Have::No)
}

/// A need as it is said: a service's own spelling, else a server's label, else as written.
pub fn need_word(need: &str, map: &Map) -> String {
    if let Some(s) = SERVICES.iter().find(|s| squash(s) == squash(need)) {
        return s.to_string();
    }
    map.accounts.iter().flat_map(|o| o.servers.iter()).find(|s| is(need, s)).map(|s| s.label.clone()).unwrap_or_else(|| need.trim().to_string())
}

/// "a", "a and b", "a, b and c".
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What every account has of a need, in a line: `Sentry: connected on
/// orca; needs sign-in on default`, `no account has YouTrack`.
pub fn where_words(need: &str, map: &Map, list: &[Account], folder: Option<&str>) -> String {
    let word = need_word(need, map);
    let names: Vec<&str> = if list.is_empty() { map.accounts.iter().map(|o| o.account.as_str()).collect() } else { list.iter().map(|a| a.name.as_str()).collect() };
    let mut by: BTreeMap<Have, Vec<String>> = BTreeMap::new();
    for n in &names {
        let h = map.of(n).map(|o| has(o, need, folder)).unwrap_or(Have::No);
        by.entry(h).or_default().push(n.to_string());
    }
    let read: Vec<&str> = names.iter().copied().filter(|n| map.of(n).is_some()).collect();
    if read.is_empty() {
        return format!("{word}: the accounts' MCP servers aren't known yet");
    }
    if by.keys().all(|h| *h == Have::No) {
        return match names.len() {
            1 => format!("no account has {word} ({} hasn't)", names[0]),
            2 => format!("no account has {word} (neither {} nor {})", names[0], names[1]),
            _ => format!("no account has {word}"),
        };
    }
    let mut parts = vec![];
    for (h, accts) in &by {
        let what = match h {
            Have::Connected => "connected on",
            Have::Unchecked => "configured (not checked) on",
            Have::NeedsAuth => "needs sign-in on",
            Have::Failed => "fails to connect on",
            Have::No => "not on",
        };
        parts.push(format!("{what} {}", and_list(accts)));
    }
    if !by.keys().any(|h| h.usable()) {
        return format!("no account has {word} connected ({})", parts.join("; "));
    }
    format!("{word}: {}", parts.join("; "))
}

// ---- what a task needs ----

/// The services `text` names, as whole words (`SERVICES`, and every
/// connector's and plugin's label in `map`), in the order first named.
pub fn named_in(text: &str, map: &Map) -> Vec<String> {
    let toks: Vec<String> = words(text);
    let mut vocab: Vec<String> = SERVICES.iter().map(|s| s.to_string()).collect();
    for s in map.accounts.iter().flat_map(|o| o.servers.iter()).filter(|s| matches!(s.source, Source::Connector | Source::Plugin)) {
        if !vocab.iter().any(|v| squash(v) == squash(&s.label)) {
            vocab.push(s.label.clone());
        }
    }
    let mut found: Vec<(usize, String)> = vec![];
    for v in vocab {
        let vw = words(&v);
        if vw.is_empty() {
            continue;
        }
        if let Some(at) = toks.windows(vw.len()).position(|w| w == vw.as_slice()) {
            found.push((at, v));
        }
    }
    found.sort_by_key(|(at, _)| *at);
    found.into_iter().map(|(_, v)| v).collect()
}

/// A task's needs: what `--needs-mcp` named (each value may list several,
/// comma-separated) and what its words name, once each.
pub fn needs_of(asked: &[String], text: &str, map: &Map) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let named = named_in(text, map);
    for n in asked.iter().flat_map(|a| a.split(',')).map(str::trim).filter(|n| !n.is_empty()).map(String::from).chain(named) {
        if !out.iter().any(|o| squash(o) == squash(&n)) {
            out.push(n);
        }
    }
    out
}

/// Where a task that needs MCP servers goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub account: String,
    /// It isn't the account it would have had (`fallback`).
    pub moved: bool,
    /// Why, as the receipt says it (`account_why`): the whole reason when
    /// `moved`, else what it adds to the usual one.
    pub why: String,
    /// What to say besides: what it lacks, what nobody has.
    pub notes: Vec<String>,
}

/// What a task asks of the accounts.
pub struct Want<'a> {
    pub needs: &'a [String],
    /// The account asked for (resolved): kept, whatever it lacks.
    pub asked: Option<&'a str>,
    /// The one it would have had: a setup's, else the default.
    pub fallback: Option<&'a str>,
    /// The folder it works in (a project's own servers count there only).
    pub folder: Option<&'a str>,
}

/// The account for a task that needs `want.needs` (None: no needs, or no
/// accounts). `out`: whether an account is out of usage now.
pub fn choose(want: &Want, list: &[Account], map: &Map, out: &dyn Fn(&str) -> bool) -> Option<Choice> {
    if want.needs.is_empty() || list.is_empty() {
        return None;
    }
    let words = |n: &str| need_word(n, map);
    let has_of = |acct: &str, n: &str| map.of(acct).map(|o| has(o, n, want.folder)).unwrap_or(Have::No);
    let lacks = |acct: &str| -> Vec<String> { want.needs.iter().filter(|n| !has_of(acct, n).usable()).cloned().collect() };
    let fallback = want.fallback.filter(|f| list.iter().any(|a| a.name == *f)).map(String::from).unwrap_or_else(|| list[0].name.clone());
    let mut notes: Vec<String> = vec![];
    // What is said of a need `acct` lacks: that nobody has it, or who has it (and is out of usage now).
    let lack_note = |acct: &str, n: &str| -> String {
        let w = where_words(n, map, list, want.folder);
        if !list.iter().any(|a| has_of(&a.name, n).usable()) {
            return format!("{w}, so it runs on {acct} without it");
        }
        let out_with: Vec<String> = list.iter().filter(|a| out(&a.name) && has_of(&a.name, n).usable()).map(|a| a.name.clone()).collect();
        let extra = if out_with.is_empty() { String::new() } else { format!("; {} has it but is out of usage now", and_list(&out_with)) };
        format!("{acct} has no {} connected ({w}{extra})", words(n))
    };
    // Asked for: it stays, and is told what it lacks.
    if let Some(asked) = want.asked {
        let missing = lacks(asked);
        for n in &missing {
            notes.push(lack_note(asked, n));
        }
        let why = if missing.is_empty() { format!("it has {} connected", and_list(&want.needs.iter().map(|n| words(n)).collect::<Vec<_>>())) } else { String::new() };
        return Some(Choice { account: asked.to_string(), moved: false, why, notes });
    }
    let signed_in = |a: &Account| a.email.trim() != "not signed in";
    let open: Vec<&Account> = list.iter().filter(|a| signed_in(a) && !out(&a.name)).collect();
    let score = |a: &str| want.needs.len() - lacks(a).len();
    let best = open.iter().map(|a| score(&a.name)).max().unwrap_or(0);
    // Nobody has any, or the one it would have had is as good as any: that one.
    let chosen = if best == 0 || open.iter().any(|a| a.name == fallback && score(&a.name) == best) {
        fallback.clone()
    } else {
        open.iter().find(|a| score(&a.name) == best).map(|a| a.name.clone()).unwrap_or(fallback.clone())
    };
    let missing = lacks(&chosen);
    let had: Vec<String> = want.needs.iter().filter(|n| !missing.contains(n)).map(|n| words(n)).collect();
    let moved = chosen != fallback;
    let why = if moved {
        let only = list.iter().filter(|a| a.name != chosen).all(|a| score(&a.name) < best);
        let others: Vec<String> = want.needs.iter().map(|n| where_words(n, map, list, want.folder)).collect();
        format!(
            "the {} account with {} connected ({})",
            if only { "only" } else { "first" },
            and_list(&had),
            others.join("; ")
        )
    } else if had.is_empty() {
        String::new()
    } else {
        format!("it has {} connected", and_list(&had))
    };
    for n in &missing {
        notes.push(lack_note(&chosen, n));
    }
    Some(Choice { account: chosen, moved, why, notes })
}

// ---- a session's own, for a failover ----

/// The servers a session used, by their tool key (`mcp__<key>__<tool>`
/// in its transcript's tool calls; signing in to one is no use of it).
pub fn used_in(transcript: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for line in transcript.lines().filter(|l| l.contains("\"tool_use\"") && l.contains("mcp__")) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        for c in v["message"]["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "tool_use") {
            let Some(rest) = c["name"].as_str().and_then(|n| n.strip_prefix("mcp__")) else { continue };
            let Some((key, tool)) = rest.split_once("__") else { continue };
            if matches!(tool, "authenticate" | "complete_authentication") || key.is_empty() {
                continue;
            }
            if !out.iter().any(|k| k == key) {
                out.push(key.to_string());
            }
        }
    }
    out
}

/// `used_in`, of a transcript file (none: nothing used).
pub fn used_in_file(path: &str) -> Vec<String> {
    if path.trim().is_empty() {
        return vec![];
    }
    std::fs::read_to_string(path).map(|t| used_in(&t)).unwrap_or_default()
}

/// What a session that used `used` (tool keys) loses going from `from` to
/// `to`: the labels of the servers `from` has usable that it used and `to`
/// hasn't. Servers neither account lists (a session's own) aren't counted.
pub fn lost(map: &Map, from: &str, to: &str, used: &[String]) -> Vec<String> {
    let (Some(f), Some(t)) = (map.of(from), map.of(to)) else { return vec![] };
    let mut out: Vec<String> = vec![];
    for s in f.servers.iter().filter(|s| s.status.usable() && used.contains(&tool_key(&s.name))) {
        let kept = t.servers.iter().any(|x| x.name == s.name && x.project == s.project && x.status.usable());
        if !kept && !out.contains(&s.label) {
            out.push(s.label.clone());
        }
    }
    out
}

// ---- what is said ----

/// An account's servers by what they are to it (labels).
#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    /// Connected (or only configured: not checked).
    pub usable: Vec<String>,
    pub needs_sign_in: Vec<String>,
    pub failing: Vec<String>,
    /// One project's: (label, its folder's name).
    pub projects: Vec<(String, String)>,
}

/// An account's servers, sorted for a line.
pub fn summary(of: &Of) -> Summary {
    let mut usable = vec![];
    let mut auth = vec![];
    let mut failed = vec![];
    let mut projects = vec![];
    for s in &of.servers {
        if let Some(p) = &s.project {
            let folder = p.trim_end_matches('/').rsplit('/').next().unwrap_or(p).to_string();
            projects.push((s.label.clone(), folder));
            continue;
        }
        let list = match s.status {
            Status::Connected | Status::Unchecked => &mut usable,
            Status::NeedsAuth => &mut auth,
            Status::Failed => &mut failed,
        };
        if !list.contains(&s.label) {
            list.push(s.label.clone());
        }
    }
    Summary { usable, needs_sign_in: auth, failing: failed, projects }
}

/// An account's servers in a line: `Claude Docs, Sentry, blender · needs
/// sign-in: Figma, Stripe · fails: x · one project only: notion (app)`.
pub fn line(of: &Of) -> String {
    let Summary { usable, needs_sign_in: auth, failing: failed, projects } = summary(of);
    let mut parts = vec![if usable.is_empty() { "none connected".to_string() } else { usable.join(", ") }];
    if !auth.is_empty() {
        parts.push(format!("needs sign-in: {}", auth.join(", ")));
    }
    if !failed.is_empty() {
        parts.push(format!("fails to connect: {}", failed.join(", ")));
    }
    if !projects.is_empty() {
        parts.push(format!("one project only: {}", projects.iter().map(|(l, f)| format!("{l} ({f})")).collect::<Vec<_>>().join(", ")));
    }
    parts.join(" · ")
}

/// How fresh an account's reading is, in words: `checked 3 min ago`, or why not.
pub fn checked_words(of: &Of, now_ms: u64) -> String {
    let ago = |ms: u64| {
        let m = now_ms.saturating_sub(ms) / 60_000;
        match m {
            0 => "just now".to_string(),
            1..=59 => format!("{m} min ago"),
            _ => format!("{} h ago", m / 60),
        }
    };
    match (&of.checked_ms, &of.error) {
        (Some(c), None) => format!("checked {}", ago(*c)),
        (Some(c), Some(e)) => format!("last checked {} (now: {e})", ago(*c)),
        (None, Some(e)) => format!("from its config only ({e})"),
        (None, None) => "from its config only".to_string(),
    }
}

/// What the brains' instructions say of the accounts' MCP servers. Only
/// what stays (no times), so it changes only when a server does. Empty
/// when nothing was read.
pub fn prompt_section(map: &Map) -> String {
    if map.accounts.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "# MCP servers by Claude account\n\nWhat each Claude account reaches through MCP (its claude.ai connectors, its plugins' servers, its local servers), as the tower last read it; `tower accounts` shows it now. Only a connected server is usable: one that needs sign-in isn't, until the user signs in to it under that account. A task that needs one goes to an account that has it connected: `tower start --needs-mcp <name>` (also `tower launch --needs-mcp`, and `\"needs_mcp\": [\"<name>\"]` on a `tower dispatch` task); the tower also reads the services a task names (Figma, Sentry, YouTrack…), says which account it chose and why, and says when no account has it. Don't pick the account for an MCP yourself unless the user names one.\n\n",
    );
    for o in &map.accounts {
        s += &format!("- {}: {}\n", o.account, line(o));
    }
    s
}

/// The radar's word on the services the user's words name: `[mcp]
/// Sentry: connected on orca; needs sign-in on default`. None when they
/// name none (or nothing was read).
pub fn radar_note(text: &str, map: &Map, list: &[Account]) -> Option<String> {
    if map.accounts.is_empty() {
        return None;
    }
    let named = named_in(text, map);
    if named.is_empty() {
        return None;
    }
    let parts: Vec<String> = named.iter().map(|n| where_words(n, map, list, None)).collect();
    Some(format!("[mcp] {}", parts.join("; ")))
}

/// The two accounts as they were here when this was written (Sentry
/// signed in on orca only), for tests.
#[cfg(test)]
pub fn sample() -> Map {
    let srv = |name: &str, status: Status| Server { name: name.into(), label: label_of(name), source: source_of(name), status, target: String::new(), project: None };
    let common = |sentry: Status| {
        vec![
            srv("claude.ai Claude Docs", Status::Connected),
            srv("plugin:figma:figma", Status::NeedsAuth),
            srv("plugin:sentry:sentry", sentry),
            srv("plugin:vercel:vercel", Status::NeedsAuth),
            srv("plugin:stripe:stripe", Status::NeedsAuth),
            srv("blender", Status::Connected),
            Server { project: Some("/w/app".into()), source: Source::Project, ..srv("notion", Status::Unchecked) },
        ]
    };
    Map {
        at_ms: 1,
        accounts: vec![
            Of { account: "default".into(), servers: common(Status::NeedsAuth), checked_ms: Some(1), ..Default::default() },
            Of { account: "orca".into(), servers: common(Status::Connected), checked_ms: Some(1), ..Default::default() },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn acc(name: &str) -> Account {
        Account { name: name.into(), email: format!("{name}@example.invalid"), browser: None }
    }

    fn srv(name: &str, status: Status) -> Server {
        Server { name: name.into(), label: label_of(name), source: source_of(name), status, target: String::new(), project: None }
    }

    /// The two accounts as they are here: Sentry signed in on orca only.
    fn map() -> Map {
        sample()
    }

    const LIST: &str = "Checking MCP server health…\n\nclaude.ai Claude Docs: https://api.anthropic.com/v1/pages/mcp - ✔ Connected\nplugin:figma:figma: https://mcp.figma.com/mcp (HTTP) - ! Needs authentication\nplugin:sentry:sentry: https://mcp.sentry.dev/mcp?utm_source=plugin (HTTP) - ✔ Connected\nblender: /u/bin/uv run --directory /a b/blender-mcp blender-mcp - ✔ Connected\nbroken: npx -y some-server - ✗ Failed to connect\nodd: x - ? Something new\n";

    #[test]
    fn reads_what_claude_mcp_list_says() {
        let l = parse_list(LIST);
        let got: Vec<(&str, &str, Source, Status)> = l.iter().map(|s| (s.name.as_str(), s.label.as_str(), s.source, s.status)).collect();
        assert_eq!(
            got,
            [
                ("claude.ai Claude Docs", "Claude Docs", Source::Connector, Status::Connected),
                ("plugin:figma:figma", "Figma", Source::Plugin, Status::NeedsAuth),
                ("plugin:sentry:sentry", "Sentry", Source::Plugin, Status::Connected),
                ("blender", "blender", Source::Local, Status::Connected),
                ("broken", "broken", Source::Local, Status::Failed),
            ],
            "the header and a status it doesn't know are left out"
        );
        assert_eq!(l[1].target, "https://mcp.figma.com/mcp", "(HTTP) off the URL");
        assert_eq!(l[3].target, "/u/bin/uv run --directory /a b/blender-mcp blender-mcp", "a command with spaces, up to the last \" - \"");
        assert!(parse_list("No MCP servers configured. Use `claude mcp add` to add a server.").is_empty());
    }

    #[test]
    fn reads_an_accounts_config() {
        let config = json!({
            "mcpServers": { "blender": { "type": "stdio", "command": "uv", "args": ["run", "blender-mcp"] } },
            "projects": {
                "/w/app": { "mcpServers": { "notion": { "type": "http", "url": "https://mcp.notion.com/mcp" } } },
                "/w/other": { "allowedTools": [] }
            },
            "claudeAiMcpEverConnected": ["claude.ai Claude Docs"]
        });
        let s = of_config(&config);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].name.as_str(), s[0].source, s[0].status, s[0].target.as_str()), ("blender", Source::Local, Status::Unchecked, "uv run blender-mcp"));
        assert_eq!((s[1].name.as_str(), s[1].source, s[1].project.as_deref()), ("notion", Source::Project, Some("/w/app")));
        let c = connectors_seen(&config);
        assert_eq!((c[0].label.as_str(), c[0].source), ("Claude Docs", Source::Connector));
        // Plugins: only those on, their servers from .mcp.json or plugin.json; one project's plugin is that project's.
        let settings = json!({ "enabledPlugins": { "figma@official": true, "sentry@official": true, "caveman@caveman": false, "genui@genui": true } });
        let installed = json!({ "plugins": {
            "figma@official": [{ "scope": "user", "installPath": "/p/figma" }],
            "sentry@official": [{ "scope": "user", "installPath": "/p/sentry" }],
            "caveman@caveman": [{ "scope": "user", "installPath": "/p/caveman" }],
            "genui@genui": [{ "scope": "local", "projectPath": "/w/genui", "installPath": "/p/genui" }, { "scope": "local", "projectPath": "/w/genui", "installPath": "/p/genui" }]
        }});
        let read = |p: &Path| -> Option<Value> {
            match p.to_str()? {
                "/p/figma/.mcp.json" => Some(json!({ "mcpServers": { "figma": { "type": "http", "url": "https://mcp.figma.com/mcp" } } })),
                "/p/sentry/.claude-plugin/plugin.json" => Some(json!({ "name": "sentry", "mcpServers": { "sentry": { "type": "http", "url": "https://mcp.sentry.dev/mcp" } } })),
                "/p/caveman/.mcp.json" => Some(json!({ "mcpServers": { "cave": { "command": "x" } } })),
                "/p/genui/.mcp.json" => Some(json!({ "mcpServers": { "open-pencil": { "command": "op" } } })),
                _ => None,
            }
        };
        let p = of_plugins(&settings, &installed, &read);
        let got: Vec<(&str, &str, Option<&str>)> = p.iter().map(|s| (s.name.as_str(), s.label.as_str(), s.project.as_deref())).collect();
        assert_eq!(got, [("plugin:figma:figma", "Figma", None), ("plugin:sentry:sentry", "Sentry", None), ("plugin:genui:open-pencil", "open-pencil", Some("/w/genui"))]);
        assert_eq!(needs_sign_in(&json!({ "plugin:figma:figma": { "timestamp": 1 } })), BTreeSet::from(["plugin:figma:figma".to_string()]));
    }

    #[test]
    fn reads_the_profiles_files_together() {
        let dir = std::env::temp_dir().join(format!("tower-mcps-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("plugins/p/sentry")).unwrap();
        let p = profile_at(&dir.to_string_lossy(), Path::new("/nowhere"));
        assert_eq!(p.config, dir.join(".claude.json"), "a profile's .claude.json is in it");
        assert_eq!(profile_at("", Path::new("/h")), Profile { dir: "/h/.claude".into(), config: "/h/.claude.json".into() }, "the default's is beside it");
        std::fs::write(&p.config, json!({ "mcpServers": { "blender": { "command": "uv" } } }).to_string()).unwrap();
        std::fs::write(dir.join("settings.json"), json!({ "enabledPlugins": { "sentry@o": true } }).to_string()).unwrap();
        let at = dir.join("plugins/p/sentry");
        std::fs::write(dir.join("plugins/installed_plugins.json"), json!({ "plugins": { "sentry@o": [{ "scope": "user", "installPath": at }] } }).to_string()).unwrap();
        std::fs::write(at.join(".mcp.json"), json!({ "mcpServers": { "sentry": { "url": "https://s" } } }).to_string()).unwrap();
        std::fs::write(dir.join("mcp-needs-auth-cache.json"), json!({ "plugin:sentry:sentry": { "timestamp": 1 } }).to_string()).unwrap();
        let s = local(&p).unwrap();
        let got: Vec<(&str, Status)> = s.iter().map(|s| (s.label.as_str(), s.status)).collect();
        assert_eq!(got, [("Sentry", Status::NeedsAuth), ("blender", Status::Unchecked)]);
        assert!(local(&profile_at("/nowhere/at/all", Path::new("/h"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_live_word_stands_and_a_failed_check_keeps_the_last() {
        let local = vec![
            Server { target: "uv run blender".into(), ..srv("blender", Status::Unchecked) },
            Server { project: Some("/w/app".into()), source: Source::Project, ..srv("notion", Status::Unchecked) },
            srv("plugin:figma:figma", Status::NeedsAuth),
        ];
        let live = parse_list(LIST);
        let now = merge("orca", "/p", local.clone(), Ok(live), None, 100);
        assert_eq!(now.checked_ms, Some(100));
        assert_eq!(now.servers.iter().find(|s| s.name == "plugin:sentry:sentry").unwrap().status, Status::Connected);
        assert_eq!(now.servers.iter().filter(|s| s.name == "blender").count(), 1, "one blender: the live one");
        assert_eq!(now.servers.last().unwrap().name, "notion", "one project's servers last");
        // The next check fails: the last answer stands, with its time, and why.
        let later = merge("orca", "/p", local.clone(), Err("timed out".into()), Some(&now), 200);
        assert_eq!((later.checked_ms, later.error.as_deref()), (Some(100), Some("timed out")));
        assert_eq!(later.servers.iter().find(|s| s.name == "plugin:sentry:sentry").unwrap().status, Status::Connected);
        assert!(checked_words(&later, 100 + 5 * 60_000).starts_with("last checked 5 min ago"));
        // Never answered: the files only, said so.
        let files = merge("orca", "/p", local, Err("no claude".into()), None, 300);
        assert_eq!((files.checked_ms, files.servers.len()), (None, 3));
        assert_eq!(checked_words(&files, 300), "from its config only (no claude)");
    }

    #[test]
    fn needs_are_matched_by_what_people_call_them() {
        let m = map();
        let orca = m.of("orca").unwrap();
        assert_eq!(has(orca, "sentry", None), Have::Connected);
        assert_eq!(has(orca, "Sentry", None), Have::Connected);
        assert_eq!(has(orca, "plugin:sentry:sentry", None), Have::Connected);
        assert_eq!(has(m.of("default").unwrap(), "sentry", None), Have::NeedsAuth);
        assert_eq!(has(orca, "claude docs", None), Have::Connected);
        assert_eq!(has(orca, "docs", None), Have::Connected, "a word of its name");
        assert_eq!(has(orca, "claude", None), Have::No, "never by `claude` alone");
        assert_eq!(has(orca, "plugin", None), Have::No);
        assert_eq!(has(orca, "youtrack", None), Have::No);
        // One project's server counts only in it.
        assert_eq!(has(orca, "notion", None), Have::No);
        assert_eq!(has(orca, "notion", Some("/w/app/src")), Have::Unchecked);
        assert_eq!(has(orca, "notion", Some("/w/application")), Have::No, "not a folder that only starts the same");
        assert_eq!(tool_key("claude.ai Claude Docs"), "claude_ai_Claude_Docs");
        assert_eq!(tool_key("plugin:sentry:sentry"), "plugin_sentry_sentry");
    }

    #[test]
    fn a_task_names_services_by_whole_words() {
        let m = map();
        assert_eq!(named_in("Look at the Sentry issue and the Figma frame", &m), ["Sentry", "Figma"]);
        assert_eq!(named_in("read https://x.myjetbrains.com/youtrack/issue/EYF-12", &m), ["YouTrack"]);
        assert_eq!(named_in("update the claude docs page", &m), ["Claude Docs"]);
        assert!(named_in("sentryless refactor of the figmatic parser", &m).is_empty(), "whole words only");
        assert!(named_in("a linear search over the sandbox", &m).is_empty(), "plain words and local servers aren't services");
        assert_eq!(needs_of(&["youtrack, Sentry".into()], "fix the sentry alert", &m), ["youtrack", "Sentry"], "once each");
        assert_eq!(need_word("youtrack", &m), "YouTrack");
    }

    #[test]
    fn a_task_goes_to_an_account_that_has_its_mcp() {
        let m = map();
        let list = [acc("default"), acc("orca")];
        let none_out = |_: &str| false;
        let choose_for = |needs: &[&str], asked: Option<&str>, fallback: Option<&str>, out: &dyn Fn(&str) -> bool| {
            let needs: Vec<String> = needs.iter().map(|n| n.to_string()).collect();
            choose(&Want { needs: &needs, asked, fallback, folder: None }, &list, &m, out)
        };
        // No needs: nothing to choose.
        assert_eq!(choose_for(&[], None, None, &none_out), None);
        // Sentry: only orca has it connected.
        let c = choose_for(&["Sentry"], None, None, &none_out).unwrap();
        assert_eq!((c.account.as_str(), c.moved), ("orca", true));
        assert_eq!(c.why, "the only account with Sentry connected (Sentry: connected on orca; needs sign-in on default)");
        assert!(c.notes.is_empty());
        // Claude Docs: both have it: the one it would have had.
        let c = choose_for(&["claude docs"], None, Some("default"), &none_out).unwrap();
        assert_eq!((c.account.as_str(), c.moved, c.why.as_str()), ("default", false, "it has Claude Docs connected"));
        // YouTrack: nobody has it, said plainly; it starts where it would have.
        let c = choose_for(&["youtrack"], None, None, &none_out).unwrap();
        assert_eq!((c.account.as_str(), c.moved, c.why.as_str()), ("default", false, ""));
        assert_eq!(c.notes, ["no account has YouTrack (neither default nor orca), so it runs on default without it"]);
        // Figma: both need sign-in: that is said, not "nobody has it".
        let c = choose_for(&["figma"], None, None, &none_out).unwrap();
        assert_eq!(c.account, "default");
        assert_eq!(c.notes, ["no account has Figma connected (needs sign-in on default and orca), so it runs on default without it"]);
        // Asked for: kept, and told what it lacks.
        let c = choose_for(&["sentry"], Some("default"), None, &none_out).unwrap();
        assert_eq!((c.account.as_str(), c.moved), ("default", false));
        assert_eq!(c.notes, ["default has no Sentry connected (Sentry: connected on orca; needs sign-in on default)"]);
        // The one that has it is out of usage: not taken, and that is said.
        let orca_out = |a: &str| a == "orca";
        let c = choose_for(&["sentry"], None, None, &orca_out).unwrap();
        assert_eq!(c.account, "default");
        assert_eq!(c.notes, ["default has no Sentry connected (Sentry: connected on orca; needs sign-in on default; orca has it but is out of usage now)"]);
        // Two needs, one had by orca: the most wins; what it still lacks is said.
        let c = choose_for(&["sentry", "youtrack"], None, None, &none_out).unwrap();
        assert_eq!(c.account, "orca");
        assert_eq!(c.notes, ["no account has YouTrack (neither default nor orca), so it runs on orca without it"]);
        // A setup's own account that has it stays.
        let c = choose_for(&["claude docs"], None, Some("orca"), &none_out).unwrap();
        assert_eq!((c.account.as_str(), c.moved), ("orca", false));
        // Nothing read yet: said so, it starts as it would have.
        let empty = Map::default();
        let c = choose(&Want { needs: &["sentry".into()], asked: None, fallback: None, folder: None }, &list, &empty, &none_out).unwrap();
        assert_eq!((c.account.as_str(), c.notes.len()), ("default", 1));
        assert!(c.notes[0].contains("aren't known yet"), "{:?}", c.notes);
    }

    #[test]
    fn a_session_uses_the_servers_it_called() {
        let t = [
            json!({ "type": "assistant", "message": { "content": [{ "type": "tool_use", "name": "mcp__plugin_sentry_sentry__search_issues", "input": {} }] } }),
            json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "mcp__plugin_figma_figma__x" }, { "type": "tool_use", "name": "Bash" }] } }),
            json!({ "type": "assistant", "message": { "content": [{ "type": "tool_use", "name": "mcp__plugin_figma_figma__authenticate" }] } }),
            json!({ "type": "assistant", "message": { "content": [{ "type": "tool_use", "name": "mcp__claude_ai_Claude_Docs__batch" }, { "type": "tool_use", "name": "mcp__claude-in-chrome__navigate" }] } }),
            json!({ "type": "user", "message": { "content": [{ "type": "tool_result", "content": "\"tool_use\" mcp__blender__x" }] } }),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        assert_eq!(used_in(&t), ["plugin_sentry_sentry", "claude_ai_Claude_Docs", "claude-in-chrome"], "tool calls only; signing in is no use");
        let m = map();
        // From orca (Sentry connected) to default (needs sign-in): Sentry is lost; Claude Docs isn't; Chrome is neither's.
        assert_eq!(lost(&m, "orca", "default", &used_in(&t)), ["Sentry"]);
        assert!(lost(&m, "default", "orca", &used_in(&t)).is_empty(), "default never had Sentry connected: nothing to lose");
        assert!(lost(&m, "orca", "nobody", &used_in(&t)).is_empty(), "an account not read: not said");
    }

    #[test]
    fn the_brains_and_the_radar_are_told() {
        let m = map();
        let s = prompt_section(&m);
        assert!(s.contains("- default: Claude Docs, blender · needs sign-in: Figma, Sentry, Vercel, Stripe · one project only: notion (app)"), "{s}");
        assert!(s.contains("- orca: Claude Docs, Sentry, blender · needs sign-in: Figma, Vercel, Stripe"), "{s}");
        assert!(s.contains("--needs-mcp"));
        assert_eq!(prompt_section(&Map::default()), "", "nothing read: nothing said");
        let list = [acc("default"), acc("orca")];
        assert_eq!(radar_note("check sentry for the crash", &m, &list).as_deref(), Some("[mcp] Sentry: connected on orca; needs sign-in on default"));
        assert_eq!(radar_note("any youtrack tickets?", &m, &list).as_deref(), Some("[mcp] no account has YouTrack (neither default nor orca)"));
        assert_eq!(radar_note("what's left?", &m, &list), None);
    }
}
