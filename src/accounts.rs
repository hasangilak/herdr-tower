//! Claude Code accounts, as the claude-accounts mod (`cca`, in the Herdr
//! header) defines them. Nothing here knows any account by name: the list
//! and the account of a running `claude` both come from `cca --porcelain`,
//! asked at run time.
//!
//! - `cca --porcelain` prints one `A\t<name>\t<email>` line per account. The
//!   first is the one used when none is asked for (cca's own `default`:
//!   `~/.claude`, `CLAUDE_CONFIG_DIR` unset).
//! - `cca --porcelain <pid>` also prints `C\t<name>`: the account that
//!   process runs under, from its `CLAUDE_CONFIG_DIR` (cca maps the folder
//!   back to a name: its special profiles, `~/.claude-accounts/<name>`, or
//!   unset for the default).
//! - `cca <name> [claude args]` runs claude under an account: `tower start`
//!   launches sessions that way.
//!
//! - `B\t<name>\t<browser>`: the browser the account's links open in (an
//!   app name, or `system` for the Mac's default), set with
//!   `cca --porcelain-browser <name> <browser>`. cca puts its `cca-open` in
//!   each session's `BROWSER`, so the links a session opens (an Artifact, a
//!   sign-in page) open there; the tower opens a link of an account's the
//!   same way (`open_link`).
//!
//! A process keeps its environment for life, so a pid's account is looked up
//! once and kept (in memory and in `accounts.json` in the state folder, for
//! the short-lived `tower agents`). The list itself is read fresh each time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Account {
    pub name: String,
    pub email: String,
    /// The browser its links open in (an app name); None: the Mac's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser: Option<String>,
}

/// One usage window of an account, as the mod last recorded it (its
/// status-line tap writes what Claude Code reports).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Percent used.
    pub used: f64,
    /// When it resets (Unix ms), when known.
    pub resets_ms: Option<u64>,
}

/// An account's usage: its 5-hour and 7-day windows (None: not recorded).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

impl Usage {
    /// Until when the account has no usage left: a window at 100% that
    /// hasn't reset yet (the later of two). None: it has some.
    pub fn spent_until(&self, now_ms: u64) -> Option<u64> {
        [&self.five_hour, &self.seven_day]
            .into_iter()
            .flatten()
            .filter(|w| w.used >= 100.0 && w.resets_ms.is_none_or(|r| r > now_ms))
            .map(|w| w.resets_ms.unwrap_or(now_ms + crate::failover::UNKNOWN_OUT_MS))
            .max()
    }
}

/// What `cca --porcelain [pid]` printed.
#[derive(Debug, Default, PartialEq)]
pub struct Porcelain {
    /// `C`: the process's account (only meaningful when a pid was given).
    pub current: Option<String>,
    /// `D`: the account the header has selected for new shell sessions.
    pub selected: Option<String>,
    /// `A`: every account, in cca's order.
    pub accounts: Vec<Account>,
    /// `U`: each account's usage, by its name (only those the mod recorded).
    pub usage: std::collections::BTreeMap<String, Usage>,
}

/// What the mod writes for "the Mac's default browser".
pub const SYSTEM: &str = "system";

pub fn parse(text: &str) -> Porcelain {
    let mut p = Porcelain::default();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["C", name, ..] if !name.is_empty() => p.current = Some(name.to_string()),
            ["D", name, ..] if !name.is_empty() => p.selected = Some(name.to_string()),
            ["A", name, rest @ ..] if !name.is_empty() => {
                p.accounts.push(Account { name: name.to_string(), email: rest.first().unwrap_or(&"").to_string(), browser: None })
            }
            // (After its `A` line.)
            ["B", name, app, ..] if !name.is_empty() => {
                let app = app.trim();
                if let Some(a) = p.accounts.iter_mut().rev().find(|a| a.name == *name) {
                    a.browser = (!app.is_empty() && app != SYSTEM).then(|| app.to_string());
                }
            }
            // `U <name> <5h used> <5h resets> <7d used> <7d resets>` (resets in Unix seconds, either may be empty).
            ["U", name, rest @ ..] if !name.is_empty() => {
                let window = |used: Option<&&str>, resets: Option<&&str>| {
                    let used = used.and_then(|u| u.trim().parse::<f64>().ok())?;
                    let resets_ms = resets.and_then(|r| r.trim().parse::<f64>().ok()).filter(|r| *r > 0.0).map(|r| (r * 1000.0) as u64);
                    Some(Window { used, resets_ms })
                };
                let u = Usage { five_hour: window(rest.first(), rest.get(1)), seven_day: window(rest.get(2), rest.get(3)) };
                p.usage.insert(name.to_string(), u);
            }
            _ => {}
        }
    }
    p
}

/// Every account the mod knows, with what it recorded of their usage.
/// Empty when the mod isn't installed.
pub fn list_with_usage() -> Result<(Vec<Account>, std::collections::BTreeMap<String, Usage>)> {
    let Some(cca) = cca_bin() else { return Ok((vec![], Default::default())) };
    let p = porcelain(&cca, None)?;
    Ok((p.accounts, p.usage))
}

/// The account a new session starts on when none is asked for: the first
/// listed (the default) unless the tower found it out of usage (see
/// `failover`), then the next one listed that has some. All out: the first.
pub fn default_of(list: &[Account]) -> Option<String> {
    crate::failover::first_available(list, crate::history::now_ms()).or_else(|| list.first().map(|a| a.name.clone()))
}

/// The `cca` script: on PATH, else where the mod installs it.
pub fn cca_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TOWER_CCA")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let home = dirs::home_dir().unwrap_or_default();
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).map(|d| d.join("cca"));
    on_path.chain([home.join(".local/bin/cca"), home.join(".config/herdr-app/mods/claude-accounts/cca")]).find(|p| p.is_file())
}

fn porcelain(cca: &Path, pid: Option<u32>) -> Result<Porcelain> {
    let mut cmd = Command::new(cca);
    cmd.arg("--porcelain");
    if let Some(pid) = pid {
        cmd.arg(pid.to_string());
    }
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::piped()).output().with_context(|| format!("running {}", cca.display()))?;
    if !out.status.success() {
        bail!("cca --porcelain: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(parse(&String::from_utf8_lossy(&out.stdout)))
}

/// Every account the mod knows, the default first, read now (accounts can
/// be added at any time). Empty when the mod isn't installed.
pub fn list() -> Result<Vec<Account>> {
    let Some(cca) = cca_bin() else { return Ok(vec![]) };
    Ok(porcelain(&cca, None)?.accounts)
}

// ---- the browser an account's links open in ----

/// The browsers the tower offers to pick from, by app name, with the word
/// people say for each.
pub const BROWSERS: [(&str, &str); 6] =
    [("Google Chrome", "Chrome"), ("Brave Browser", "Brave"), ("Safari", "Safari"), ("Firefox", "Firefox"), ("Arc", "Arc"), ("Microsoft Edge", "Edge")];

/// The app a word names: "brave" -> "Brave Browser", "Chrome" -> "Google
/// Chrome", "system" or "default" -> `SYSTEM`; anything else as written
/// (an app's own name).
pub fn browser_app(word: &str) -> String {
    let w = word.trim();
    let lw = w.to_lowercase();
    if matches!(lw.as_str(), "" | "system" | "default" | "system default" | "the mac's default") {
        return SYSTEM.into();
    }
    BROWSERS
        .iter()
        .find(|(app, short)| lw == app.to_lowercase() || lw == short.to_lowercase() || lw == app.to_lowercase().replace(' ', "-"))
        .map(|(app, _)| app.to_string())
        .unwrap_or_else(|| w.to_string())
}

/// How a browser is named to people: "Brave", "Chrome"; None: "the Mac's
/// default browser".
pub fn browser_word(app: Option<&str>) -> String {
    match app {
        None => "the Mac's default browser".into(),
        Some(a) => BROWSERS.iter().find(|(x, _)| *x == a).map(|(_, w)| w.to_string()).unwrap_or_else(|| a.to_string()),
    }
}

/// The browsers of `BROWSERS` this Mac has (in /Applications or
/// ~/Applications).
pub fn installed_browsers() -> Vec<&'static str> {
    let home = dirs::home_dir().unwrap_or_default();
    BROWSERS
        .iter()
        .map(|(app, _)| *app)
        .filter(|app| [PathBuf::from("/Applications"), home.join("Applications")].iter().any(|d| d.join(format!("{app}.app")).exists()))
        .collect()
}

/// Sets the browser `account`'s links open in (`browser`: an app, a word
/// for one, or "system"), through the mod: what it set.
pub fn set_browser(account: &str, browser: &str) -> Result<String> {
    let Some(cca) = cca_bin() else { bail!("the claude-accounts mod (cca) isn't installed: no accounts to set a browser for") };
    set_browser_with(&cca, account, browser)
}

fn set_browser_with(cca: &Path, account: &str, browser: &str) -> Result<String> {
    let app = browser_app(browser);
    let out = Command::new(cca).args(["--porcelain-browser", account, &app]).stdin(Stdio::null()).output().with_context(|| format!("running {}", cca.display()))?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("cca: ").to_string();
        if out.status.code() == Some(127) || e.contains("unknown option") || e.contains("No account --porcelain-browser") {
            bail!("this cca has no browser setting: update the claude-accounts mod");
        }
        bail!("{e}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The mod's `cca-open`: the `BROWSER` a session gets, which opens a link
/// in the browser of the account it runs on. None: a mod without one.
pub fn cca_open_bin() -> Option<PathBuf> {
    let cca = cca_bin()?;
    let home = dirs::home_dir().unwrap_or_default();
    [cca.with_file_name("cca-open"), home.join(".local/bin/cca-open"), home.join(".config/herdr-app/mods/claude-accounts/cca-open")].into_iter().find(|p| p.is_file())
}

/// The browser a link of `account` opens in: that account's; one not known
/// (None, or one the mod doesn't list) is the default account's, the one a
/// session runs on when none is asked for. None: the Mac's default.
pub fn browser_for(account: Option<&str>, list: &[Account]) -> Option<String> {
    let a = account.and_then(|a| list.iter().find(|x| x.name == a)).or(list.first())?;
    a.browser.clone()
}

/// The command that opens `url` in `browser` (None: the Mac's default).
pub fn open_argv(url: &str, browser: Option<&str>) -> Vec<String> {
    match browser {
        Some(b) => vec!["open".into(), "-a".into(), b.into(), url.into()],
        None => vec!["open".into(), url.into()],
    }
}

/// Opens a web link the way a session of `account` would (`browser_for`),
/// read from the mod now: the browser it went to, in words. Only http(s)
/// links. Tests never open one.
pub fn open_link(url: &str, account: Option<&str>) -> Result<String> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.chars().any(char::is_whitespace) {
        bail!("not a web link: {url:?}");
    }
    let list = list().unwrap_or_default();
    let browser = browser_for(account, &list);
    let argv = open_argv(url, browser.as_deref());
    if !cfg!(test) {
        let ok = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        // A browser that isn't there any more: the Mac's default, as cca-open does.
        if !ok && browser.is_some() {
            let plain = open_argv(url, None);
            let st = Command::new(&plain[0]).args(&plain[1..]).stdin(Stdio::null()).status().context("running open")?;
            if !st.success() {
                bail!("open {url} failed");
            }
            return Ok(browser_word(None));
        }
        if !ok {
            bail!("open {url} failed");
        }
    }
    Ok(browser_word(browser.as_deref()))
}

/// The account to use: `want` when the mod knows it, the default when
/// nothing is asked for (the first listed, or the next one with usage left
/// while that one is out: `default_of`). `None`: no accounts (no mod),
/// nothing asked.
pub fn resolve(want: Option<&str>, list: &[Account]) -> Result<Option<String>> {
    let want = want.map(str::trim).filter(|w| !w.is_empty());
    match want {
        None => Ok(default_of(list)),
        Some(w) => {
            if let Some(a) = list.iter().find(|a| a.name == w).or_else(|| list.iter().find(|a| a.name.eq_ignore_ascii_case(w))) {
                return Ok(Some(a.name.clone()));
            }
            if list.is_empty() {
                bail!("no Claude account {w:?}: the claude-accounts mod (cca) isn't installed, so there are no accounts to pick from");
            }
            // "second", "my 2nd account": by its place in the list.
            if let Some(n) = ordinal(w)
                && let Some(a) = list.get(n)
            {
                return Ok(Some(a.name.clone()));
            }
            // Part of a name, when only one has it.
            let lw = w.to_lowercase();
            let part: Vec<&Account> = list.iter().filter(|a| a.name.to_lowercase().contains(&lw) || lw.contains(&a.name.to_lowercase())).collect();
            if let [one] = part.as_slice() {
                return Ok(Some(one.name.clone()));
            }
            let names: Vec<&str> = list.iter().map(|a| a.name.as_str()).collect();
            bail!("no Claude account {w:?} (the claude-accounts mod has: {})", names.join(", "))
        }
    }
}

/// "second" -> 1, "my 3rd account" -> 2, "1" -> 0.
fn ordinal(w: &str) -> Option<usize> {
    const WORDS: &[&str] = &["first", "second", "third", "fourth", "fifth"];
    const SHORT: &[&str] = &["1st", "2nd", "3rd", "4th", "5th"];
    w.to_lowercase().split(|c: char| !c.is_alphanumeric()).find_map(|t| {
        WORDS.iter().position(|x| *x == t).or_else(|| SHORT.iter().position(|x| *x == t)).or_else(|| t.parse::<usize>().ok().filter(|n| (1..=9).contains(n)).map(|n| n - 1))
    })
}

// ---- which account a running session uses ----

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    /// "<pid>" -> account, for live processes.
    pids: HashMap<String, String>,
}

static PIDS: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);

fn cache_path() -> PathBuf {
    // Tests keep theirs in a temporary folder, never in the user's state folder.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("herder-tower-test-{}", std::process::id())).join("accounts.json");
    }
    config::state_dir().join("accounts.json")
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
}

/// The account process `pid` (a running `claude`) uses, or None when it
/// can't be told (no mod, process gone).
pub fn of_pid(pid: u32) -> Option<String> {
    if !alive(pid) {
        return None;
    }
    let mut mem = PIDS.lock().unwrap_or_else(|e| e.into_inner());
    let map = mem.get_or_insert_with(|| {
        let c: Cache = std::fs::read_to_string(cache_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        c.pids.into_iter().filter_map(|(k, v)| k.parse::<u32>().ok().map(|p| (p, v))).collect()
    });
    if let Some(a) = map.get(&pid) {
        return Some(a.clone());
    }
    let cca = cca_bin()?;
    let account = porcelain(&cca, Some(pid)).ok()?.current?;
    // ps had nothing to say if the process ended meanwhile: don't keep a guess.
    if !alive(pid) {
        return None;
    }
    map.insert(pid, account.clone());
    map.retain(|p, _| alive(*p));
    let c = Cache { pids: map.iter().map(|(p, a)| (p.to_string(), a.clone())).collect() };
    let path = cache_path();
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::create_dir_all(config::state_dir());
    if std::fs::write(&tmp, serde_json::to_string(&c).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    Some(account)
}

/// Whether a terminal (as `hc get` or the `hc events` snapshot has it) is a
/// Claude Code session on this machine, and its program's pid.
pub fn claude_pid(t: &Value) -> Option<u32> {
    if t["machine"].is_string() || t["address"].as_str().is_some_and(|a| a.contains('/')) {
        return None;
    }
    let fg = &t["foreground"];
    let program = fg["argv"][0].as_str().unwrap_or("").rsplit('/').next().unwrap_or("");
    if program != "claude" && crate::hc::agent_app(t).as_deref() != Some("claude-code") {
        return None;
    }
    // Until claude itself runs (`cca` or a shell still starting it), the
    // environment isn't the session's yet.
    if matches!(program, "sh" | "bash" | "zsh" | "dash" | "env" | "cca") {
        return None;
    }
    fg["pid"].as_u64().or_else(|| t["pid"].as_u64()).map(|p| p as u32)
}

/// The account of a terminal's Claude Code session: None for Codex, plain
/// programs, other machines, or when it can't be told.
pub fn of_terminal(t: &Value) -> Option<String> {
    of_pid(claude_pid(t)?)
}

/// `of_terminal` for a row of `hc ls` or `herdr-app ls`, which have no
/// pids: asks `hc get`.
pub fn of_address(address: &str) -> Option<String> {
    if address.is_empty() || address.contains('/') {
        return None;
    }
    of_terminal(&crate::hc::get(address)?)
}

// ---- starting a session on an account ----

/// What `tower start` asks for.
pub struct Start<'a> {
    pub name: &'a str,
    pub cwd: Option<&'a str>,
    pub on: Option<&'a str>,
    pub task: &'a str,
    /// None: no accounts known, run plain `claude`.
    pub account: Option<&'a str>,
    /// The `cca` to run it with (a path here, `cca` on another machine).
    pub cca: &'a str,
    /// With its task list on (`[sessions] todo_tools`): `env
    /// CLAUDE_CODE_ENABLE_TODO_TOOLS=1` before it.
    pub todo_tools: bool,
    /// The model it asks for advice (`[sessions] advisor`): `--advisor <model>`.
    pub advisor: Option<&'a str>,
}

/// `flags` with `--advisor <model>` added when they name no advisor of
/// their own (a session the tower resumes or moves gets `[sessions] advisor`).
pub fn with_advisor(mut flags: Vec<String>, advisor: Option<&str>) -> Vec<String> {
    let named = flags.iter().any(|f| f == "--advisor" || f.starts_with("--advisor="));
    if let Some(m) = advisor.filter(|_| !named) {
        flags.extend(["--advisor".into(), m.to_string()]);
    }
    flags
}

/// What turns a session's task list on (`Start::todo_tools`).
pub const TODO_TOOLS: &str = "CLAUDE_CODE_ENABLE_TODO_TOOLS=1";

/// The program a command line runs, past an `env VAR=value …` before it:
/// `env CLAUDE_CODE_ENABLE_TODO_TOOLS=1 claude …` runs `claude`. Past the
/// login shell it is started in, too (`in_login_shell`, or Herdr's own for
/// a tab: `/bin/zsh -lic '"$@"; exec "$0" -li' /bin/zsh claude …`).
pub fn program_of<'a>(argv: &[&'a str]) -> &'a str {
    let base = |a: &'a str| a.rsplit('/').next().unwrap_or(a);
    if let [sh, flags, _script, rest @ ..] = argv
        && SHELLS.contains(&base(sh))
        && flags.starts_with('-')
        && flags.contains('c')
    {
        // (zsh, bash, sh: the word after the script is its `$0`; fish has none.)
        let rest = if base(sh) == "fish" { rest } else { rest.get(1..).unwrap_or(&[]) };
        return program_of(rest);
    }
    let mut it = argv.iter().copied();
    let first = it.next().map(base).unwrap_or("");
    if first != "env" {
        return first;
    }
    it.find(|a| !a.contains('=') && !a.starts_with('-')).map(base).unwrap_or("")
}

/// `argv` past the shell it was started in (`in_login_shell`, or Herdr's
/// own for a tab), when it was: what that shell runs.
pub fn past_shell(argv: &[String]) -> &[String] {
    let base = |a: &str| a.rsplit('/').next().unwrap_or(a).to_string();
    match argv {
        [sh, flags, _script, rest @ ..] if SHELLS.contains(&base(sh).as_str()) && flags.starts_with('-') && flags.contains('c') => {
            let rest = if base(sh) == "fish" { rest } else { rest.get(1..).unwrap_or(&[]) };
            past_shell(rest)
        }
        _ => argv,
    }
}

/// The shells a command line may be started in (`program_of`).
const SHELLS: [&str; 4] = ["zsh", "bash", "sh", "fish"];

/// The shell a person's own terminals start in: `$SHELL`, else zsh.
pub fn login_shell() -> String {
    std::env::var("SHELL").ok().filter(|s| s.starts_with('/')).unwrap_or_else(|| "/bin/zsh".into())
}

/// `argv`, run as a terminal the person opens runs it: in their login,
/// interactive `shell` (`-lic`), so it has the PATH their shell's profile
/// and rc files set (nvm's node, Homebrew, bun…), as Herdr runs a tab's
/// agent. Not run so, it has only what the terminal server started with,
/// and a session's hooks that need `node` fail ("Executable not found in
/// $PATH"). The shell execs it: none is left behind once it ends.
pub fn in_login_shell(shell: &str, argv: Vec<String>) -> Vec<String> {
    let fish = shell.rsplit('/').next() == Some("fish");
    let mut a: Vec<String> = vec![shell.into(), "-lic".into()];
    if fish {
        a.push("exec $argv".into());
    } else {
        a.extend(["exec \"$@\"".into(), shell.into()]);
    }
    a.extend(argv);
    a
}

/// The `hc run` arguments that start the session, in the person's login
/// shell (`login_shell`) here; on another machine, as that machine runs it.
pub fn hc_run_args(s: &Start) -> Vec<String> {
    hc_run_args_in(s, s.on.is_none().then(login_shell).as_deref())
}

/// `hc_run_args`, with the session started in `shell` (None: as it is).
pub fn hc_run_args_in(s: &Start, shell: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["run", "--bg", "--json", "--outlive-parent", "--name", s.name].map(String::from).to_vec();
    if let Some(cwd) = s.cwd {
        a.extend(["--cwd".into(), cwd.into()]);
    }
    if let Some(m) = s.on {
        a.extend(["--on".into(), m.into()]);
    }
    a.push("--".into());
    match shell {
        Some(sh) => a.extend(in_login_shell(sh, command(s))),
        None => a.extend(command(s)),
    }
    a
}

/// The program that runs the session: `cca <account> -n <name> "<task>"`
/// (cca sets or unsets CLAUDE_CONFIG_DIR and CCA_ACCOUNT, then execs claude),
/// or plain `claude` when there are no accounts.
pub fn command(s: &Start) -> Vec<String> {
    let mut a: Vec<String> = if s.todo_tools { vec!["env".into(), TODO_TOOLS.into()] } else { vec![] };
    match s.account {
        Some(acc) => a.extend([s.cca.into(), acc.into()]),
        None => a.push("claude".into()),
    }
    a.extend(["-n".into(), s.name.into()]);
    if let Some(m) = s.advisor {
        a.extend(["--advisor".into(), m.into()]);
    }
    if !s.task.is_empty() {
        a.push(s.task.into());
    }
    a
}

/// The accounts on another machine (its own `cca`), or empty when it has none.
pub fn list_on(machine: &str) -> Vec<Account> {
    let Ok(out) = crate::hc::run(&["exec", "--on", machine, "--", "cca", "--porcelain"]) else { return vec![] };
    if out.code != 0 { vec![] } else { parse(&out.stdout).accounts }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OUT: &str = "C\twork\nD\tdefault\nA\tdefault\tme@example.com\nL\tdefault\tMine\nB\tdefault\tGoogle Chrome\nU\tdefault\t12\t\t40\t\nA\twork\tai@example.com\nB\twork\tBrave Browser\nA\tside\t\nB\tside\tsystem\nS\t9bea6336-0eba-435c-8cd9-bec697e79160\t/tmp\n";

    fn accounts() -> Vec<Account> {
        parse(OUT).accounts
    }

    #[test]
    fn parses_porcelain() {
        let p = parse(OUT);
        assert_eq!(p.current.as_deref(), Some("work"));
        assert_eq!(p.selected.as_deref(), Some("default"));
        let names: Vec<_> = p.accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["default", "work", "side"]);
        assert_eq!(p.accounts[1].email, "ai@example.com");
        assert_eq!(p.accounts[2].email, "");
        assert_eq!(parse(""), Porcelain::default());
        // `U`: what the mod recorded of each account's usage (resets in seconds).
        assert_eq!(p.usage["default"].five_hour, Some(Window { used: 12.0, resets_ms: None }));
        assert_eq!(p.usage["default"].seven_day, Some(Window { used: 40.0, resets_ms: None }));
        let spent = parse("A\twork\tw@x\nU\twork\t100\t1794600000\t55.5\t\n");
        let u = &spent.usage["work"];
        assert_eq!(u.five_hour, Some(Window { used: 100.0, resets_ms: Some(1_794_600_000_000) }));
        assert_eq!(u.spent_until(1_794_599_000_000), Some(1_794_600_000_000), "at 100% until its reset");
        assert_eq!(u.spent_until(1_794_600_000_001), None, "reset already");
        assert_eq!(Usage::default().spent_until(1), None);
    }

    #[test]
    fn each_account_has_the_browser_its_links_open_in() {
        let l = accounts();
        let browsers: Vec<_> = l.iter().map(|a| a.browser.as_deref()).collect();
        assert_eq!(browsers, [Some("Google Chrome"), Some("Brave Browser"), None], "system: the Mac's default");
        // A `B` line before its account's `A` line, or for one not listed: nothing.
        assert_eq!(parse("B\tx\tSafari\nA\tx\t\n").accounts[0].browser, None);
        // A mod without the setting: every account on the Mac's default.
        assert!(parse("A\tdefault\tme@x\nA\torca\tai@x\n").accounts.iter().all(|a| a.browser.is_none()));
        // A link opens in its account's browser; one of an account not known, in the default one's.
        assert_eq!(browser_for(Some("work"), &l).as_deref(), Some("Brave Browser"));
        assert_eq!(browser_for(Some("side"), &l), None);
        assert_eq!(browser_for(None, &l).as_deref(), Some("Google Chrome"));
        assert_eq!(browser_for(Some("gone"), &l).as_deref(), Some("Google Chrome"));
        assert_eq!(browser_for(Some("work"), &[]), None, "no mod: the Mac's default");
        assert_eq!(open_argv("https://claude.ai/artifact/x", Some("Brave Browser")), ["open", "-a", "Brave Browser", "https://claude.ai/artifact/x"]);
        assert_eq!(open_argv("https://claude.ai/artifact/x", None), ["open", "https://claude.ai/artifact/x"]);
        // The words people say for a browser.
        for (w, app) in [("brave", "Brave Browser"), ("Chrome", "Google Chrome"), ("google-chrome", "Google Chrome"), (" safari ", "Safari"), ("EDGE", "Microsoft Edge"), ("default", "system"), ("System", "system"), ("Vivaldi", "Vivaldi")] {
            assert_eq!(browser_app(w), app, "{w}");
        }
        assert_eq!((browser_word(Some("Brave Browser")), browser_word(Some("Vivaldi")), browser_word(None)), ("Brave".into(), "Vivaldi".into(), "the Mac's default browser".into()));
        // Only a web link is opened.
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "https://x y", ""] {
            assert!(open_link(bad, None).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_browser_is_set_through_the_mod() {
        let dir = std::env::temp_dir().join(format!("herder-tower-test-browser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("args");
        let cca = dir.join("cca");
        // As the mod answers: the app it set, or exit 2/3 with why.
        let script = format!(
            "#!/bin/sh\nprintf '%s|' \"$@\" > '{}'\ncase \"$2\" in nobody) echo 'cca: No account nobody.' >&2; exit 3;; esac\ncase \"$3\" in Nope) echo 'cca: No browser \"Nope\" on this Mac.' >&2; exit 2;; esac\necho \"$3\"\n",
            log.display()
        );
        std::fs::write(&cca, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cca, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(set_browser_with(&cca, "orca", "brave").unwrap(), "Brave Browser");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "--porcelain-browser|orca|Brave Browser|", "the app's name goes to the mod");
        assert_eq!(set_browser_with(&cca, "orca", "default").unwrap(), "system");
        assert_eq!(set_browser_with(&cca, "nobody", "chrome").unwrap_err().to_string(), "No account nobody.");
        assert_eq!(set_browser_with(&cca, "orca", "Nope").unwrap_err().to_string(), "No browser \"Nope\" on this Mac.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_accounts() {
        let l = accounts();
        assert_eq!(resolve(None, &l).unwrap().as_deref(), Some("default"), "nothing asked: the first listed");
        assert_eq!(resolve(Some(" "), &l).unwrap().as_deref(), Some("default"));
        assert_eq!(resolve(Some("work"), &l).unwrap().as_deref(), Some("work"));
        assert_eq!(resolve(Some("Side"), &l).unwrap().as_deref(), Some("side"));
        // "my second claude account": by place; a part of one name.
        assert_eq!(resolve(Some("second"), &l).unwrap().as_deref(), Some("work"));
        assert_eq!(resolve(Some("my second claude account"), &l).unwrap().as_deref(), Some("work"));
        assert_eq!(resolve(Some("3rd"), &l).unwrap().as_deref(), Some("side"));
        assert_eq!(resolve(Some("wor"), &l).unwrap().as_deref(), Some("work"));
        assert!(resolve(Some("ninth"), &l).is_err());
        let e = resolve(Some("nope"), &l).unwrap_err().to_string();
        assert!(e.contains("\"nope\"") && e.contains("default, work, side"), "{e}");
        assert_eq!(resolve(None, &[]).unwrap(), None);
        assert!(resolve(Some("work"), &[]).unwrap_err().to_string().contains("isn't installed"));
    }

    #[test]
    fn the_sessions_the_tower_opens_get_its_advisor() {
        // `tower start`: as `/advisor fable` typed in it, before its task.
        let s = Start { name: "x", cwd: None, on: None, task: "fix it", account: Some("orca"), cca: "/bin/cca", todo_tools: false, advisor: Some("fable") };
        assert_eq!(command(&s), ["/bin/cca", "orca", "-n", "x", "--advisor", "fable", "fix it"]);
        assert_eq!(command(&Start { advisor: None, ..s }), ["/bin/cca", "orca", "-n", "x", "fix it"], "advisor off: none");
        // Its resume keeps it, as the started command had it…
        let argv: Vec<String> = ["claude", "-n", "x", "--advisor", "fable", "fix it"].map(String::from).to_vec();
        assert_eq!(crate::reopen::flags_of(&argv), ["--advisor", "fable"]);
        // …and one started without gets the tower's; one with its own keeps that.
        let f = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(with_advisor(f(&["--model", "opus"]), Some("fable")), ["--model", "opus", "--advisor", "fable"]);
        assert_eq!(with_advisor(f(&["--advisor", "opus"]), Some("fable")), ["--advisor", "opus"]);
        assert_eq!(with_advisor(f(&["--advisor=opus"]), Some("fable")), ["--advisor=opus"]);
        assert_eq!(with_advisor(f(&[]), None), Vec::<String>::new());
        // `[sessions] advisor`: fable unless set; off, none or "": no advisor.
        assert_eq!(crate::config::Config::from_text("").unwrap().sessions.advisor, "fable");
        assert_eq!(crate::config::Config::from_text("[sessions]\nadvisor = \"opus\"\n").unwrap().sessions.advisor, "opus");
        for off in ["off", "OFF", "none", "", "  "] {
            assert_eq!(crate::config::advisor_of(off), None, "{off:?}");
        }
        assert_eq!(crate::config::advisor_of(" fable "), Some("fable".into()));
        assert_eq!(crate::config::advisor(), Some("fable".into()), "tests: the default");
    }

    #[test]
    fn builds_the_start_command() {
        let s = Start {
            name: "fix-login",
            cwd: Some("/w/app"),
            on: None,
            task: "look at the login bug",
            account: Some("work"),
            cca: "/bin/cca",
            todo_tools: false,
            advisor: None,
        };
        assert_eq!(
            hc_run_args_in(&s, None),
            [
                "run",
                "--bg",
                "--json",
                "--outlive-parent",
                "--name",
                "fix-login",
                "--cwd",
                "/w/app",
                "--",
                "/bin/cca",
                "work",
                "-n",
                "fix-login",
                "look at the login bug"
            ]
        );
        assert_eq!(command(&s), ["/bin/cca", "work", "-n", "fix-login", "look at the login bug"], "what a tab in a space runs");
        // Here, in the person's login shell: the PATH their own terminals have (node for its hooks).
        let here = hc_run_args_in(&s, Some("/bin/zsh"));
        assert_eq!(
            here[8..],
            ["--", "/bin/zsh", "-lic", "exec \"$@\"", "/bin/zsh", "/bin/cca", "work", "-n", "fix-login", "look at the login bug"],
            "{here:?}"
        );
        let here: Vec<&str> = here.iter().map(String::as_str).collect();
        assert_eq!(program_of(&here[9..]), "cca", "the program behind the shell");
        assert_eq!(hc_run_args(&s)[9..11], [login_shell(), "-lic".to_string()], "hc_run_args starts it so");
        let fish = in_login_shell("/opt/homebrew/bin/fish", vec!["claude".into(), "-n".into(), "x".into()]);
        assert_eq!(fish, ["/opt/homebrew/bin/fish", "-lic", "exec $argv", "claude", "-n", "x"]);
        // On another machine: as that machine runs it, no shell of this one's.
        let s = Start { name: "x", cwd: None, on: Some("vps"), task: "", account: None, cca: "cca", todo_tools: false, advisor: None };
        assert_eq!(hc_run_args(&s), ["run", "--bg", "--json", "--outlive-parent", "--name", "x", "--on", "vps", "--", "claude", "-n", "x"]);
        // With its task list on (the default): env sets it, here and on another machine alike.
        let s = Start { todo_tools: true, ..s };
        assert_eq!(command(&s), ["env", "CLAUDE_CODE_ENABLE_TODO_TOOLS=1", "claude", "-n", "x"]);
        let s = Start { account: Some("work"), cca: "/bin/cca", ..s };
        assert_eq!(command(&s), ["env", "CLAUDE_CODE_ENABLE_TODO_TOOLS=1", "/bin/cca", "work", "-n", "x"]);
        assert_eq!(program_of(&["env", "CLAUDE_CODE_ENABLE_TODO_TOOLS=1", "/bin/cca", "work"]), "cca");
        assert_eq!(program_of(&["/usr/bin/env", "A=1", "-i", "claude"]), "claude");
        assert_eq!(program_of(&["/opt/bin/claude", "-n", "x"]), "claude");
        assert_eq!(program_of(&[]), "");
        // Past a shell it was started in: Herdr's for a tab, or `in_login_shell`.
        assert_eq!(program_of(&["/bin/zsh", "-lic", "\"$@\"; exec \"$0\" -li", "/bin/zsh", "env", "A=1", "/bin/claude", "-n", "x"]), "claude");
        assert_eq!(program_of(&["/bin/zsh", "-l"]), "zsh", "a shell itself");
        assert_eq!(program_of(&["fish", "-lic", "exec $argv", "codex"]), "codex");
    }

    #[test]
    fn finds_claude_pids() {
        let t = json!({ "address": "t3", "pid": 10, "foreground": { "argv": ["claude", "do it"], "pid": 11 }, "status": { "app": "claude-code" } });
        assert_eq!(claude_pid(&t), Some(11));
        // Still in cca (a shell): its environment isn't claude's yet.
        let starting = json!({ "address": "t3", "foreground": { "argv": ["/bin/sh", "/u/cca", "work"], "pid": 12 }, "status": { "app": "claude-code" } });
        assert_eq!(claude_pid(&starting), None);
        let codex = json!({ "address": "t4", "foreground": { "argv": ["codex"], "pid": 13 }, "status": { "app": "codex" } });
        assert_eq!(claude_pid(&codex), None);
        let remote = json!({ "address": "vps/t2", "foreground": { "argv": ["claude"], "pid": 14 } });
        assert_eq!(claude_pid(&remote), None);
        let shell = json!({ "address": "t5", "foreground": { "argv": ["zsh"], "pid": 15 } });
        assert_eq!(claude_pid(&shell), None);
    }

    #[test]
    fn own_process_is_the_default_account() {
        // This test process runs under whatever the shell gave it; with the mod
        // installed, cca must name one of its listed accounts for it.
        let Some(_) = cca_bin() else { return };
        let l = list().unwrap();
        if let Some(a) = of_pid(std::process::id()) {
            assert!(l.iter().any(|x| x.name == a), "{a} not in {l:?}");
        }
    }
}
