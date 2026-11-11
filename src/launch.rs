//! `tower launch "<goal>"`: one sentence opens a space.
//!
//! It names the space from the goal (its first two words that carry meaning,
//! made unique among the spaces there are), takes a setup (`setups`: the one named, else the
//! only one, else the first), makes the Herdr space, opens one session per
//! repo of the setup on its account (each through `tower start --space`, so
//! each in the space's worktree of its repo with a dev port of its own: see
//! `worktrees`), sets the space's goal, and hands the goal out as the first
//! request: one agenda, one task per session. The sessions are introduced to
//! each other by the voice as they join (`introduce`).
//!
//! It asks nothing. What it chose is in its one receipt (`assumed`), and
//! `--dry-run` prints the same plan without doing any of it.

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::accounts::Account;
use crate::config::Setup;
use crate::worktrees::{self, Place};

/// A session's name is cut to this (Herdr shows it on a tab).
const NAME_CHARS: usize = 48;
/// How a space got its name, in the receipt.
const NAME_WHY: &str = "the goal's first two words, small ones left out";
/// The same, when the goal's first line only asks for sessions.
const NAME_NEXT_WHY: &str = "the first two words of the line after the one that asks for sessions (the ticket's title)";

/// One session of the plan: a repo of the setup.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// `<space>-<repo>`, in Herdr's name alphabet.
    pub name: String,
    /// The repo as the setup has it, and its folder.
    pub repo: String,
    pub folder: String,
    /// The worktree it will work in (None: the folder itself).
    pub worktree: Option<Place>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub goal: String,
    pub space: String,
    /// A space of that name is there already (`--name`): the sessions join it.
    pub joins: bool,
    pub setup: Setup,
    /// The account as the claude-accounts mod knows it (None: no accounts, plain `claude`).
    pub account: Option<String>,
    pub sessions: Vec<Session>,
    /// What it chose by itself, for the receipt.
    pub assumed: Value,
}

/// What a plan is made from: what there is now.
#[derive(Debug, Clone, Default)]
pub struct World {
    pub setups: Vec<Setup>,
    /// Every space's name: Herdr's, and those the tower remembers (closed ones too).
    pub spaces: Vec<String>,
    /// Herdr's spaces now.
    pub open: Vec<String>,
    /// The live sessions' names.
    pub names: Vec<String>,
    pub accounts: Vec<Account>,
    /// `[spaces] worktrees`.
    pub worktrees: bool,
    /// Spaces the user keeps (`tower space standing`).
    pub standing: Vec<String>,
    /// The project folders (`projects::candidates`): the repos a goal names.
    pub projects: Vec<std::path::PathBuf>,
}

/// Words of a goal's first line that say how, not what: "open a session at …".
const ASKING: &[&str] = &[
    "open", "opens", "start", "starts", "make", "create", "spin", "up", "launch", "session", "sessions", "at", "into", "please", "this", "is",
    "these", "are", "here", "ticket", "task", "issue", "repo", "repos", "repository", "repositories", "project", "projects", "both", "two", "work",
];

/// The goal's first line, up to a colon that brings the material in ("this is the ticket:").
fn asking_line(goal: &str) -> &str {
    let first = goal.trim_start().lines().next().unwrap_or("");
    first.split_once(':').map(|(a, _)| a).unwrap_or(first)
}

/// The repos a goal's first line names ("open a session at eyzflow and
/// mediaroom. this is the ticket: …"), in the order it names them: a word
/// that is a project folder's whole name, or a distinctive part of only one
/// folder's name (`mediaroom` in `eyz-mediaroom-serverless`; at least five
/// letters). The rest of the goal (a pasted ticket) is never read for them.
pub fn repos_named(goal: &str, folders: &[std::path::PathBuf]) -> Vec<(String, std::path::PathBuf)> {
    let parts = |name: &str| -> Vec<String> { name.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect() };
    let name_of = |p: &std::path::PathBuf| p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
    let words: Vec<String> = asking_line(goal).split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_')).filter(|w| w.len() >= 3).map(str::to_lowercase).collect();
    let mut out: Vec<(String, std::path::PathBuf)> = vec![];
    for w in words {
        if FILLER.contains(&w.as_str()) || ASKING.contains(&w.as_str()) {
            continue;
        }
        let exact: Vec<&std::path::PathBuf> = folders.iter().filter(|p| name_of(p) == w).collect();
        let found = match exact.as_slice() {
            [] if w.len() >= 5 => {
                let part: Vec<&std::path::PathBuf> = folders.iter().filter(|p| parts(&name_of(p)).contains(&w)).collect();
                match part.as_slice() {
                    [one] => Some((*one).clone()),
                    _ => None,
                }
            }
            [] => None,
            many => {
                // The same name twice (two roots): the checkout, else the first.
                Some(many.iter().find(|p| p.join(".git").is_dir()).copied().unwrap_or(many[0]).clone())
            }
        };
        if let Some(f) = found
            && !out.iter().any(|(_, o)| *o == f)
        {
            out.push((w, f));
        }
    }
    out
}

/// The space's name for `goal`: from its first line, without the words that
/// only ask for sessions and the repos it names; a first line that is only
/// that ("open a session at eyzflow and mediaroom. this is the ticket:")
/// names it from the next line (the ticket's title).
pub fn space_name_of(goal: &str, repos: &[String]) -> String {
    space_name_why(goal, repos).0
}

/// `space_name_of`, and whether the name came from the line after the first
/// (the first only asked for sessions).
fn space_name_why(goal: &str, repos: &[String]) -> (String, bool) {
    let telling = |line: &str, drop_repos: bool| -> Vec<String> {
        let plain: String = line.chars().filter(|c| !matches!(c, '\'' | '’' | '`')).collect();
        plain
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .filter(|w| !FILLER.contains(&w.as_str()) && !ASKING.contains(&w.as_str()) && !(drop_repos && repos.iter().any(|r| r.eq_ignore_ascii_case(w))))
            .take(2)
            .collect()
    };
    let first = telling(asking_line(goal), true);
    if !first.is_empty() || repos.is_empty() {
        return (if first.is_empty() { space_name(goal) } else { first.join("-") }, false);
    }
    let next = goal.trim_start().lines().skip(1).find(|l| !l.trim().is_empty()).unwrap_or("");
    let rest = match goal.trim_start().lines().next().unwrap_or("").split_once(':') {
        Some((_, after)) if !after.trim().is_empty() => after,
        _ => next,
    };
    match telling(rest, false) {
        t if t.is_empty() => (space_name(goal), false),
        t => (t.join("-"), true),
    }
}

/// Small words that say nothing about what a space is for.
const FILLER: &[&str] = &["the", "a", "an", "to", "for", "of", "and", "in", "on", "with", "my", "our", "new", "some"];

/// The goal's first two words that carry meaning, lower-cased, joined with a
/// dash: `Fix the login redirect` is `fix-login`, not `fix-the`. A goal of
/// small words only keeps its first two as they are.
pub fn space_name(goal: &str) -> String {
    let plain: String = goal.chars().filter(|c| !matches!(c, '\'' | '’' | '`')).collect();
    let words: Vec<String> = plain.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let telling: Vec<&str> = words.iter().map(String::as_str).filter(|w| !FILLER.contains(w)).take(2).collect();
    if telling.is_empty() { words.iter().take(2).map(String::as_str).collect::<Vec<_>>().join("-") } else { telling.join("-") }
}

/// `base`, or `base-2`, `base-3`, …: the first no space has (any case).
pub fn unique(base: &str, taken: &[String]) -> String {
    let has = |n: &str| taken.iter().any(|t| t.trim().eq_ignore_ascii_case(n));
    if !has(base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base}-{n}")).find(|n| !has(n)).unwrap_or_else(|| base.to_string())
}

fn repo_name(folder: &str) -> String {
    Path::new(folder.trim_end_matches('/')).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into())
}

/// `<space>-<repo>` in Herdr's name alphabet (a-z 0-9 - _), unique among `taken`.
fn session_name(space: &str, folder: &str, taken: &mut BTreeSet<String>) -> String {
    let (space, repo) = (worktrees::slug(space), worktrees::slug(&repo_name(folder)));
    // Never a name the towers' own terminals have (`tower`, `tower-…`): in a space called
    // "tower-pr" a session is `<repo>-tower-pr`, not `tower-pr-<repo>`.
    let mut base = [format!("{space}-{repo}"), format!("{repo}-{space}")]
        .into_iter()
        .find(|n| !crate::brain::is_brain_name(n))
        .unwrap_or_else(|| format!("s-{space}-{repo}"));
    base.truncate(NAME_CHARS);
    let base = base.trim_end_matches('-').to_string();
    let name = unique(&base, &taken.iter().cloned().collect::<Vec<_>>());
    taken.insert(name.clone());
    name
}

/// A goal as typed or pasted: each line's runs of spaces one space, the
/// lines kept, at most one empty line between paragraphs, none at the ends.
pub fn tidy(goal: &str) -> String {
    let mut out: Vec<String> = vec![];
    for line in goal.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")) {
        if line.is_empty() && out.last().is_none_or(|l: &String| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// What a launch would do.
pub fn plan(goal: &str, setup: Option<&str>, name: Option<&str>, w: &World) -> Result<Plan> {
    // Each line's spaces made one, the lines kept (a pasted ticket keeps its paragraphs and lists).
    let goal = tidy(goal);
    if goal.is_empty() {
        bail!("tower launch \"<the goal, in a sentence>\" [--setup <name>] [--name <space>] [--dry-run]");
    }
    // The repos: a setup that was named; else those the goal's first line
    // names, on the default account; else a saved setup.
    let named = repos_named(&goal, &w.projects);
    let (setup, setup_why): (Setup, String) = match setup.map(str::trim).filter(|s| !s.is_empty()) {
        Some(asked) => {
            let (s, why) = crate::setups::pick(&w.setups, Some(asked))?;
            (s.clone(), why)
        }
        None if !named.is_empty() => {
            let same = |a: &str, b: &Path| Path::new(crate::dispatch::expand(a.trim()).trim_end_matches('/')) == b;
            // A saved setup that has every repo the goal names keeps its account.
            match w.setups.iter().find(|s| named.iter().all(|(_, f)| s.repos.iter().any(|r| same(r, f)))) {
                Some(s) => (s.clone(), "the one that has the repos the goal names".to_string()),
                None => {
                    let words: Vec<&str> = named.iter().map(|(w, _)| w.as_str()).collect();
                    (
                        Setup { name: format!("the goal's {}", words.join(" and ")), account: String::new(), repos: named.iter().map(|(_, f)| f.to_string_lossy().into_owned()).collect() },
                        "the repos its first line names".to_string(),
                    )
                }
            }
        }
        None if w.setups.is_empty() => bail!(
            "name the repos in the goal's first line (\"open sessions in eyzflow and mediaroom\"): no project folder fits the words of \"{}\". Or save a setup: tower setup add \"<name>\" --account <account> --repo <folder>",
            asking_line(&goal).trim()
        ),
        None => {
            let (s, why) = crate::setups::pick(&w.setups, None)?;
            (s.clone(), why)
        }
    };
    let from_goal = setup_why == "the repos its first line names";
    let setup = &setup;
    if setup.repos.is_empty() {
        bail!("the setup \"{}\" has no repos: tower setup add \"{}\" --account <account> --repo <folder>", setup.name, setup.name);
    }
    let (space, name_why, joins) = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => {
            let there = w.open.iter().find(|s| s.eq_ignore_ascii_case(n));
            (there.cloned().unwrap_or_else(|| n.to_string()), "as given".to_string(), there.is_some())
        }
        None => {
            let repo_words: Vec<String> = named.iter().map(|(w, _)| w.clone()).collect();
            let (base, from_next) = space_name_why(&goal, &repo_words);
            let base = if base.is_empty() { "space".to_string() } else { base };
            let name = unique(&base, &w.spaces);
            let rule = if from_next { NAME_NEXT_WHY } else { NAME_WHY };
            let why = if name == base { rule.to_string() } else { format!("{rule}; \"{base}\" is taken") };
            (name, why, false)
        }
    };
    // The account as the mod knows it; none known: plain `claude`, and it says so.
    let (account, account_why) = match crate::accounts::resolve(Some(&setup.account).filter(|a| !a.trim().is_empty()).map(String::as_str), &w.accounts) {
        Ok(Some(a)) if setup.account.trim().is_empty() && from_goal => (Some(a), "the default one: the goal names none".to_string()),
        Ok(Some(a)) if setup.account.trim().is_empty() => (Some(a), "the default one: the setup names none".to_string()),
        Ok(a) => (a, "the setup's".to_string()),
        Err(_) if w.accounts.is_empty() => (None, format!("the setup's is \"{}\", but no accounts are known (cca isn't installed): plain claude", setup.account)),
        Err(e) => return Err(e),
    };
    let standing = w.standing.iter().any(|s| s.eq_ignore_ascii_case(&space));
    let mut taken: BTreeSet<String> = w.names.iter().cloned().collect();
    let mut sessions = vec![];
    for repo in &setup.repos {
        let folder = crate::dispatch::expand(repo.trim());
        if !Path::new(&folder).is_dir() {
            bail!("the setup \"{}\" names {repo}, which isn't a folder here", setup.name);
        }
        let worktree = (w.worktrees && !standing).then(|| worktrees::toplevel(Path::new(&folder))).flatten().map(|top| worktrees::place(&top, &space));
        sessions.push(Session { name: session_name(&space, &folder, &mut taken), repo: repo.clone(), folder, worktree });
    }
    let worktrees_why = if !w.worktrees {
        "off ([spaces] worktrees = false): each session works in its repo itself"
    } else if standing {
        "none: it is a standing space, so each session works in its repo itself"
    } else if sessions.iter().all(|s| s.worktree.is_some()) {
        "one per repo, on a branch of its own"
    } else if sessions.iter().any(|s| s.worktree.is_some()) {
        "one per git repo; a folder that isn't one is used as it is"
    } else {
        "none: no repo of the setup is a git checkout"
    };
    let assumed = json!({
        "setup": setup.name, "setup_why": setup_why,
        "account": account, "account_why": account_why,
        "name": space, "name_why": name_why,
        "worktrees": sessions.iter().any(|s| s.worktree.is_some()), "worktrees_why": worktrees_why,
    });
    Ok(Plan { goal, space, joins, setup: setup.clone(), account, sessions, assumed })
}

/// What a session of the plan is asked first: the goal, and its part in it.
pub fn instruction(p: &Plan, me: &Session) -> String {
    let others: Vec<String> = p.sessions.iter().filter(|s| s.name != me.name).map(|s| format!("{} ({})", s.name, repo_name(&s.folder))).collect();
    let with = match others.as_slice() {
        [] => String::new(),
        list => format!(" The space's other sessions each have their own repository: {}. The tower introduces you to each other; work the seams out with them.", list.join(", ")),
    };
    format!(
        "{}\n\nThat is the goal of the space \"{}\". You are its {} session: do the part of it that belongs in this repository.{with} When your part is done, say what you did and what is left.",
        p.goal,
        p.space,
        repo_name(&me.folder)
    )
}

/// The plan (and, after a launch, what came of each session) as one receipt.
pub fn receipt(p: &Plan, started: &[Value], agenda: Option<&str>, dry_run: bool) -> Value {
    let sessions: Vec<Value> = p
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let got = started.get(i).cloned().unwrap_or(Value::Null);
            json!({
                "name": s.name,
                "repo": s.repo,
                "address": got["address"],
                "account": if got["account"].is_null() { json!(p.account) } else { got["account"].clone() },
                "worktree": if got["worktree"].is_null() { json!(s.worktree.as_ref().map(|w| w.path.clone())) } else { got["worktree"].clone() },
                "branch": if got["branch"].is_null() { json!(s.worktree.as_ref().map(|w| w.branch.clone())) } else { got["branch"].clone() },
                "port": got["port"],
                "task": got["task"],
                "error": got["error"],
            })
        })
        .collect();
    json!({
        "space": p.space,
        "goal": p.goal,
        "made": !p.joins,
        "dry_run": dry_run,
        "agenda": agenda,
        "sessions": sessions,
        "assumed": p.assumed,
    })
}

/// The receipt in words.
pub fn said(r: &Value) -> String {
    let dry = r["dry_run"] == true;
    let space = r["space"].as_str().unwrap_or("");
    let mut out = format!(
        "{} \"{space}\"{}: {}\n",
        if dry { "Would launch" } else { "Launched" },
        if r["made"] == true { "" } else { " (a space that is there already)" },
        r["goal"].as_str().unwrap_or("")
    );
    for s in r["sessions"].as_array().into_iter().flatten() {
        let place = match (s["worktree"].as_str(), s["branch"].as_str()) {
            (Some(w), Some(b)) => format!("in the worktree {w} (branch {b})"),
            _ => format!("in {}", s["repo"].as_str().unwrap_or("")),
        };
        let port = s["port"].as_u64().map(|p| format!(", port {p}")).unwrap_or_default();
        match (s["error"].as_str(), s["address"].as_str()) {
            (Some(e), _) => out += &format!("  {} didn't open: {e}\n", s["name"].as_str().unwrap_or("")),
            (None, Some(a)) => out += &format!("  {} ({a}) {place}{port}\n", s["name"].as_str().unwrap_or("")),
            (None, None) => out += &format!("  {} {place}{}\n", s["name"].as_str().unwrap_or(""), if dry { ", with a dev port of its own" } else { "" }),
        }
    }
    match r["agenda"].as_str() {
        Some(a) => out += &format!("The goal is its first request ({a}): one task per session. They are introduced to each other as they join.\n"),
        None if dry => out += "The goal would be its first request: one agenda, one task per session.\n",
        None => out += "The goal went to each session as its first task (the voice isn't running, so no agenda follows it).\n",
    }
    let a = &r["assumed"];
    out += &format!(
        "Assumed: the setup \"{}\" ({}); the account {} ({}); the name \"{}\" ({}); worktrees: {}.{}",
        a["setup"].as_str().unwrap_or(""),
        a["setup_why"].as_str().unwrap_or(""),
        a["account"].as_str().unwrap_or("none"),
        a["account_why"].as_str().unwrap_or(""),
        a["name"].as_str().unwrap_or(""),
        a["name_why"].as_str().unwrap_or(""),
        a["worktrees_why"].as_str().unwrap_or(""),
        if dry { "\n(dry run: nothing was opened)" } else { "" }
    );
    if let Some(id) = r["act"].as_str() {
        out += &format!("\nTo take it back: tower undo {id}.");
    }
    out
}

/// What there is now, for a plan.
fn world() -> Result<World> {
    let cfg = crate::config::Config::load()?;
    let kept = crate::spaces::load();
    let open: Vec<String> = crate::spaces::windows().into_iter().map(|w| w.name).collect();
    let mut spaces = open.clone();
    spaces.extend(kept.missions.keys().cloned());
    spaces.extend(kept.agendas.keys().cloned());
    Ok(World {
        setups: cfg.setups,
        spaces,
        open,
        names: crate::hc::list_on(None).unwrap_or_default().iter().filter_map(|t| crate::hc::name(t).map(String::from)).collect(),
        accounts: crate::accounts::list().unwrap_or_default(),
        worktrees: cfg.spaces.worktrees,
        standing: kept.missions.iter().filter(|(_, m)| m.standing).map(|(n, _)| n.clone()).collect(),
        projects: crate::projects::candidates(&crate::projects::roots()),
    })
}

/// What a launch opened, for the daemon to keep as one act (`crate::acts`):
/// its space, its sessions that opened, and what the plan assumed.
pub fn act_of(p: &Plan, started: &[Value], by: Option<&str>) -> Value {
    let sessions: Vec<Value> = p
        .sessions
        .iter()
        .zip(started)
        .filter_map(|(s, got)| {
            let address = got["address"].as_str()?;
            let worktree = match (got["worktree"].as_str(), got["repo"].as_str()) {
                (Some(path), Some(repo)) => json!({ "path": path, "repo": repo, "branch": got["branch"].as_str().unwrap_or("") }),
                _ => Value::Null,
            };
            Some(json!({
                "address": address, "name": s.name, "worktree": worktree, "fresh": got["worktree_fresh"] == true,
                "first": instruction(p, s), "task": got["task"], "created_ms": got["created_ms"],
            }))
        })
        .collect();
    json!({ "kind": "launch", "space": p.space, "space_made": !p.joins, "goal": p.goal, "sessions": sessions, "assumed": p.assumed, "by": by })
}

/// Opens the plan: the space with its goal, the request, a session per repo.
/// `each` is given the receipt so far after every session (those that have
/// opened carry their address; the others are as planned). `by`: who asked
/// for it, when it isn't the terminal this runs in (`phone:<device>`). The
/// receipt names the act the daemon keeps for it (`act`: `tower undo`).
pub fn open(p: &Plan, by: Option<&str>, each: &mut dyn FnMut(&Value)) -> Result<Value> {
    let now = crate::history::now_ms();
    // The space is a mission from now on, with its goal: its sessions get worktrees.
    crate::spaces::update(|s| {
        s.agendas.insert(p.space.clone(), p.goal.clone());
        let m = s.missions.entry(p.space.clone()).or_insert_with(|| crate::missions::Mission::born(now));
        if m.state == crate::missions::State::Closed || m.archived_ms.is_some() {
            m.reopen(now);
        }
        (m.born_ms, m.goal_ms) = (now, now);
        m.setup = p.setup.name.clone();
    });
    // One request for the goal; each session's first task joins it.
    let agenda = match crate::protocol::is_running() {
        true => {
            let cmd = crate::agenda::Cmd { action: "open".into(), text: p.goal.clone(), title: crate::agenda::clip(&p.goal, 80), spaces: vec![p.space.clone()], ..Default::default() };
            match crate::agenda_do(&cmd) {
                Ok(v) => v["id"].as_str().map(String::from),
                Err(e) => {
                    eprintln!("tower: no agenda for the goal: {e:#}");
                    None
                }
            }
        }
        false => None,
    };
    let me = std::env::current_exe()?;
    let mut started = vec![];
    for s in &p.sessions {
        // One act for the whole space, kept below: not one per session.
        let mut args: Vec<String> = ["start", "--no-act", "--name", &s.name, "--cwd", &s.folder, "--space", &p.space, "--new-space"].map(String::from).to_vec();
        if let Some(a) = &p.account {
            args.extend(["--account".into(), a.clone()]);
        }
        if let Some(a) = &agenda {
            args.extend(["--agenda".into(), a.clone(), "--title".into(), repo_name(&s.folder)]);
        }
        args.push(instruction(p, s));
        let mut cmd = Command::new(&me);
        cmd.args(&args);
        started.push(match crate::hc::output_within(cmd, "tower start", Duration::from_secs(180)) {
            Ok(out) if out.code == 0 => serde_json::from_str::<Value>(out.stdout.trim()).unwrap_or_else(|_| json!({ "error": format!("tower start printed {}", out.stdout.trim()) })),
            Ok(out) => json!({ "error": format!("{}{}", out.stderr.trim().trim_start_matches("tower: "), out.stdout.trim()) }),
            Err(e) => json!({ "error": format!("{e:#}") }),
        });
        each(&receipt(p, &started, agenda.as_deref(), false));
    }
    let mut r = receipt(p, &started, agenda.as_deref(), false);
    // What it opened is one act: shown with the reply, and undone in one go.
    let what = act_of(p, &started, by);
    if what["sessions"].as_array().is_some_and(|s| !s.is_empty()) && crate::protocol::is_running() {
        let from = if by.is_some() { None } else { crate::peers::caller() };
        let kept = crate::protocol::send(&crate::protocol::Request::Act { action: "record".into(), id: None, force: false, act: Some(what), from });
        r["act"] = kept.ok().filter(|k| k.ok).and_then(|k| k.state).map(|v| v["id"].clone()).unwrap_or(Value::Null);
    }
    Ok(r)
}

/// What a launch would do, as things stand now (`plan`, of the world as it is).
pub fn planned(goal: &str, setup: Option<&str>, name: Option<&str>) -> Result<Plan> {
    plan(goal, setup, name, &world()?)
}

/// What a launch did (or, with `dry_run`, would do): its receipt.
pub fn launch(goal: &str, setup: Option<&str>, name: Option<&str>, dry_run: bool) -> Result<Value> {
    let p = planned(goal, setup, name)?;
    if dry_run {
        return Ok(receipt(&p, &[], None, true));
    }
    open(&p, None, &mut |_| ())
}

/// `tower launch "<goal>" [--setup <name>] [--name <space>] [--dry-run] [--json]`.
pub fn cmd(goal: &str, setup: Option<&str>, name: Option<&str>, dry_run: bool, as_json: bool) -> Result<()> {
    let r = launch(goal, setup, name, dry_run)?;
    if as_json || !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        println!("{}", said(&r));
    }
    let failed = r["sessions"].as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|s| !s["error"].is_null()));
    if failed {
        bail!("no session of \"{}\" opened", r["space"].as_str().unwrap_or(""));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktrees::tests::{repo, temp};

    fn setup(name: &str, account: &str, repos: &[&str]) -> Setup {
        Setup { name: name.into(), account: account.into(), repos: repos.iter().map(|r| r.to_string()).collect() }
    }

    fn accounts(names: &[&str]) -> Vec<Account> {
        names.iter().map(|n| Account { name: n.to_string(), email: format!("{n}@example.invalid") }).collect()
    }

    #[test]
    fn a_space_is_named_from_its_goal_and_never_twice() {
        assert_eq!(space_name("Fix the login redirect on eyzflow"), "fix-login", "not fix-the");
        assert_eq!(space_name("  Ship: checkout v2, today!"), "ship-checkout");
        assert_eq!(space_name("Don't break prod"), "dont-break");
        assert_eq!(space_name("Refactor"), "refactor");
        assert_eq!(space_name("?!"), "");
        // Small words are passed over wherever they stand, in any case.
        assert_eq!(space_name("A plan for the storefront live editor"), "plan-storefront");
        assert_eq!(space_name("Add some tests to my new parser"), "add-tests");
        assert_eq!(space_name("The Onboarding of our users"), "onboarding-users");
        assert_eq!(space_name("Move to Postgres"), "move-postgres");
        assert_eq!(space_name("Login with Google and Apple"), "login-google");
        // Only one word carries meaning: that one. None does: the words as they are.
        assert_eq!(space_name("Fix the"), "fix");
        assert_eq!(space_name("For the new"), "for-the");
        assert_eq!(space_name("To a"), "to-a");
        // A word that only begins like a small one is kept.
        assert_eq!(space_name("Android onboarding"), "android-onboarding");
        let taken = ["Fix-Login".to_string(), "fix-login-2".to_string(), "M5 Pro".to_string()];
        assert_eq!(unique("fix-login", &taken), "fix-login-3", "any case, and the next number free");
        assert_eq!(unique("ship-checkout", &taken), "ship-checkout");
        // A session is `<space>-<repo>`, but never called what a tower's own terminal is called.
        let mut names = BTreeSet::new();
        assert_eq!(session_name("fix-login", "/w/eyz/eyzflow", &mut names), "fix-login-eyzflow");
        assert_eq!(space_name("Tower PR chips"), "tower-pr");
        let in_tower_pr = session_name("tower-pr", "/w/herder-tower", &mut names);
        assert_eq!(in_tower_pr, "herder-tower-tower-pr");
        assert!(!crate::brain::is_brain_name(&in_tower_pr) && !crate::brain::is_brain_name(&session_name("Tower", "/w/tower-ui", &mut names)));
    }

    #[test]
    fn with_no_setup_the_goals_first_line_names_the_repos_and_its_ticket_names_the_space() {
        let base = temp("launch-goal-repos");
        let (flow, room) = (repo(&base, "eyzflow"), repo(&base, "eyz-mediaroom-serverless"));
        let others: Vec<std::path::PathBuf> = ["eyzflow-admin-dashboard", "eyz-customer-support", "eyz-openpencil", "landing-design"].iter().map(|n| repo(&base, n)).collect();
        let projects: Vec<std::path::PathBuf> = [vec![flow.clone(), room.clone()], others].concat();
        let w = World { accounts: accounts(&["default", "orca"]), worktrees: true, projects, ..Default::default() };
        let ticket = "open a session at eyzflow and mediaroom. this is the ticket:\nEYZFlow Support \u{2013} Enable Automatic Message and Reply Translation\n\nCustomers write in many languages.   Support answers in German.\n\n- Detect the language\n- Translate both ways, landing in the admin dashboard";
        let p = plan(ticket, None, None, &w).unwrap();
        assert_eq!(p.sessions.iter().map(|x| x.folder.clone()).collect::<Vec<_>>(), [flow.to_string_lossy().into_owned(), room.to_string_lossy().into_owned()]);
        assert_eq!(p.space, "eyzflow-support", "named from the ticket's title, not from \"open a session\"");
        assert_eq!(p.account.as_deref(), Some("default"));
        assert_eq!(p.assumed["setup_why"], "the repos its first line names");
        assert_eq!(p.assumed["account_why"], "the default one: the goal names none");
        assert!(p.assumed["name_why"].as_str().unwrap().contains("the ticket's title"));
        // The ticket keeps its paragraphs and list for the sessions; spaces inside a line are one.
        assert!(p.goal.contains("languages. Support answers in German.\n\n- Detect the language\n- Translate"), "{}", p.goal);
        assert!(instruction(&p, &p.sessions[0]).starts_with("open a session at eyzflow and mediaroom. this is the ticket:\nEYZFlow Support"));
        // Words of the ticket ("support", "admin dashboard", "landing") name no repo: only the first line counts.
        assert_eq!(repos_named(ticket, &w.projects).iter().map(|(w, _)| w.as_str()).collect::<Vec<_>>(), ["eyzflow", "mediaroom"]);
        // "open" is no repo, though eyz-openpencil has it inside a word: a part must be a whole part of one folder.
        assert!(repos_named("open the pencil thing", &w.projects).is_empty());
        // Nothing named, no setup: it says how to name them, instead of sending you to a terminal.
        let err = plan("Fix the flaky tests", None, None, &w).unwrap_err().to_string();
        assert!(err.contains("name the repos in the goal's first line"), "{err}");
        // A saved setup that has the named repos keeps its account.
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let with = World { setups: vec![setup("flow on orca", "orca", &[&s(&flow), &s(&room)])], ..w.clone() };
        let q = plan(ticket, None, None, &with).unwrap();
        assert_eq!((q.account.as_deref(), q.assumed["setup_why"].as_str()), (Some("orca"), Some("the one that has the repos the goal names")));
        // A goal of one line names its space from its own words, the repo left out.
        assert_eq!(space_name_of("Fix the login redirect in eyzflow", &["eyzflow".into()]), "fix-login");
        assert_eq!(tidy("  a  b \n\n\n\n c \n\n"), "a b\n\nc");
    }

    #[test]
    fn a_launch_is_planned_without_asking_and_says_what_it_assumed() {
        let base = temp("launch-plan");
        let (flow, room) = (repo(&base, "eyzflow"), repo(&base, "eyz-mediaroom-serverless"));
        let notes = base.join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let w = World {
            setups: vec![setup("eyzflow + mediaroom on orca", "orca", &[&s(&flow), &s(&room)]), setup("notes", "", &[&s(&notes)])],
            spaces: vec!["M5 Pro".into(), "fix-login".into(), "Old one".into()],
            open: vec!["M5 Pro".into(), "fix-login".into()],
            names: vec!["fix-login-2-eyzflow".into()],
            accounts: accounts(&["default", "orca"]),
            worktrees: true,
            standing: vec!["M5 Pro".into()],
            projects: vec![],
        };
        let p = plan("  Fix the   login redirect on eyzflow ", None, None, &w).unwrap();
        assert_eq!((p.goal.as_str(), p.space.as_str(), p.joins), ("Fix the login redirect on eyzflow", "fix-login-2", false));
        assert_eq!(p.account.as_deref(), Some("orca"));
        // One session per repo, named for the space and the repo, each in the space's worktree of it.
        assert_eq!(p.sessions.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["fix-login-2-eyzflow-2", "fix-login-2-eyz-mediaroom-serverless"]);
        let wt = p.sessions[0].worktree.clone().unwrap();
        assert_eq!((wt.path, wt.branch, wt.repo), (s(&base.join(".tower-worktrees/fix-login-2/eyzflow")), "fix-login-2/eyzflow".to_string(), s(&flow)));
        assert!(!base.join(".tower-worktrees").exists(), "a plan makes nothing");
        assert_eq!(
            p.assumed,
            json!({
                "setup": "eyzflow + mediaroom on orca", "setup_why": "the first of 2",
                "account": "orca", "account_why": "the setup's",
                "name": "fix-login-2", "name_why": "the goal's first two words, small ones left out; \"fix-login\" is taken",
                "worktrees": true, "worktrees_why": "one per repo, on a branch of its own",
            })
        );
        // Each session gets the goal, its part in it, and who the others are.
        let first = instruction(&p, &p.sessions[0]);
        assert!(first.starts_with("Fix the login redirect on eyzflow\n\nThat is the goal of the space \"fix-login-2\". You are its eyzflow session"), "{first}");
        assert!(first.contains("fix-login-2-eyz-mediaroom-serverless (eyz-mediaroom-serverless)") && !first.contains("fix-login-2-eyzflow-2 ("), "{first}");
        // The receipt of a dry run: the same plan, nothing opened.
        let r = receipt(&p, &[], None, true);
        assert_eq!((r["space"].as_str(), r["made"].as_bool(), r["dry_run"].as_bool(), r["agenda"].is_null()), (Some("fix-login-2"), Some(true), Some(true), true));
        assert_eq!(
            r["sessions"][0],
            json!({
                "name": "fix-login-2-eyzflow-2", "repo": s(&flow), "address": null, "account": "orca",
                "worktree": s(&base.join(".tower-worktrees/fix-login-2/eyzflow")), "branch": "fix-login-2/eyzflow", "port": null, "task": null, "error": null,
            })
        );
        assert_eq!(r["assumed"], p.assumed);
        let text = said(&r);
        assert!(text.starts_with("Would launch \"fix-login-2\": Fix the login redirect on eyzflow\n"), "{text}");
        assert!(text.contains("Assumed: the setup \"eyzflow + mediaroom on orca\" (the first of 2); the account orca (the setup's); the name \"fix-login-2\"") && text.ends_with("(dry run: nothing was opened)"), "{text}");
        // After a launch: what each `tower start` answered, and the request.
        let started = [json!({ "address": "t9", "account": "orca", "worktree": "/x/wt", "branch": "fix-login-2/eyzflow", "port": 3004, "task": "a7.1" }), json!({ "error": "hc run: no" })];
        let r = receipt(&p, &started, Some("a7"), false);
        assert_eq!((r["sessions"][0]["address"].as_str(), r["sessions"][0]["port"].as_u64(), r["sessions"][0]["worktree"].as_str(), r["sessions"][0]["task"].as_str()), (Some("t9"), Some(3004), Some("/x/wt"), Some("a7.1")));
        assert_eq!(r["sessions"][1]["error"], "hc run: no");
        let text = said(&r);
        assert!(text.contains("  fix-login-2-eyzflow-2 (t9) in the worktree /x/wt (branch fix-login-2/eyzflow), port 3004\n") && text.contains("didn't open: hc run: no") && text.contains("first request (a7)"), "{text}");

        // The setup that is named (a part is enough); a folder that is no git checkout is used as it is.
        let p = plan("Tidy the notes", Some("NOTES"), None, &w).unwrap();
        assert_eq!((p.space.as_str(), p.setup.name.as_str(), p.sessions[0].worktree.clone(), p.account.as_deref()), ("tidy-notes", "notes", None, Some("default")));
        assert_eq!((p.assumed["setup_why"].as_str(), p.assumed["account_why"].as_str()), (Some("the one named"), Some("the default one: the setup names none")));
        assert_eq!(p.assumed["worktrees_why"], "none: no repo of the setup is a git checkout");
        // A name of the user's own; one that is a space already is joined, and a standing one gets no worktrees.
        let p = plan("Fix the login", None, Some("  Login fix "), &w).unwrap();
        assert_eq!((p.space.as_str(), p.joins, p.assumed["name_why"].as_str()), ("Login fix", false, Some("as given")));
        assert_eq!(p.sessions[0].name, "login-fix-eyzflow");
        let p = plan("Fix the login", None, Some("m5 pro"), &w).unwrap();
        assert_eq!((p.space.as_str(), p.joins, p.sessions[0].worktree.clone()), ("M5 Pro", true, None));
        assert!(p.assumed["worktrees_why"].as_str().unwrap().contains("standing space"));
        // Worktrees off: the repos themselves.
        let off = World { worktrees: false, ..w.clone() };
        assert!(plan("Fix the login", None, None, &off).unwrap().sessions.iter().all(|x| x.worktree.is_none()));
        // No accounts known: plain claude, and it says so; an account the mod doesn't know is an error, not a question.
        let none = World { accounts: vec![], ..w.clone() };
        let p = plan("Fix the login", None, None, &none).unwrap();
        assert!(p.account.is_none() && p.assumed["account_why"].as_str().unwrap().contains("plain claude"));
        let other = World { accounts: accounts(&["default"]), ..w.clone() };
        assert!(plan("Fix the login", None, None, &other).unwrap_err().to_string().contains("no Claude account"));
        // Nothing to open with: it says how to add a setup.
        assert!(plan("Fix the login", None, None, &World::default()).unwrap_err().to_string().contains("tower setup add"));
        assert!(plan("Fix the login", Some("nope"), None, &w).unwrap_err().to_string().contains("no setup"));
        assert!(plan("  ", None, None, &w).is_err());
        let gone = World { setups: vec![setup("x", "orca", &["/no/such/folder"])], ..w.clone() };
        assert!(plan("Fix it", None, None, &gone).unwrap_err().to_string().contains("isn't a folder"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
