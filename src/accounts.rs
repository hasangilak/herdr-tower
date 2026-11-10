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
}

pub fn parse(text: &str) -> Porcelain {
    let mut p = Porcelain::default();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["C", name, ..] if !name.is_empty() => p.current = Some(name.to_string()),
            ["D", name, ..] if !name.is_empty() => p.selected = Some(name.to_string()),
            ["A", name, rest @ ..] if !name.is_empty() => {
                p.accounts.push(Account { name: name.to_string(), email: rest.first().unwrap_or(&"").to_string() })
            }
            _ => {}
        }
    }
    p
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

/// The account to use: `want` when the mod knows it, the first one listed
/// when nothing is asked for. `None`: no accounts (no mod), nothing asked.
pub fn resolve(want: Option<&str>, list: &[Account]) -> Result<Option<String>> {
    let want = want.map(str::trim).filter(|w| !w.is_empty());
    match want {
        None => Ok(list.first().map(|a| a.name.clone())),
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
}

/// The `hc run` arguments that start the session.
pub fn hc_run_args(s: &Start) -> Vec<String> {
    let mut a: Vec<String> = ["run", "--bg", "--json", "--outlive-parent", "--name", s.name].map(String::from).to_vec();
    if let Some(cwd) = s.cwd {
        a.extend(["--cwd".into(), cwd.into()]);
    }
    if let Some(m) = s.on {
        a.extend(["--on".into(), m.into()]);
    }
    a.push("--".into());
    a.extend(command(s));
    a
}

/// The program that runs the session: `cca <account> -n <name> "<task>"`
/// (cca sets or unsets CLAUDE_CONFIG_DIR and CCA_ACCOUNT, then execs claude),
/// or plain `claude` when there are no accounts.
pub fn command(s: &Start) -> Vec<String> {
    let mut a: Vec<String> = match s.account {
        Some(acc) => vec![s.cca.into(), acc.into()],
        None => vec!["claude".into()],
    };
    a.extend(["-n".into(), s.name.into()]);
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

    const OUT: &str = "C\twork\nD\tdefault\nA\tdefault\tme@example.com\nL\tdefault\tMine\nU\tdefault\t12\t\t40\t\nA\twork\tai@example.com\nA\tside\t\nS\t9bea6336-0eba-435c-8cd9-bec697e79160\t/tmp\n";

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
    fn builds_the_start_command() {
        let s = Start {
            name: "fix-login",
            cwd: Some("/w/app"),
            on: None,
            task: "look at the login bug",
            account: Some("work"),
            cca: "/bin/cca",
        };
        assert_eq!(
            hc_run_args(&s),
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
        let s = Start { name: "x", cwd: None, on: Some("vps"), task: "", account: None, cca: "cca" };
        assert_eq!(hc_run_args(&s), ["run", "--bg", "--json", "--outlive-parent", "--name", "x", "--on", "vps", "--", "claude", "-n", "x"]);
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
