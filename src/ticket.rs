//! `tower launch "<ticket>" --draft`: the tower's brain reads a ticket and
//! drafts the space for it, for the user to confirm or adjust in words.
//! Nothing is opened: the draft is launched as it was confirmed, with
//! `tower launch "<ticket>" --repo … --name … --title … --done-when …`.
//!
//! - **One headless call**: `cca <the default account> -p --output-format
//!   json --model <[brains] model> --effort low` with no tools, no MCP
//!   servers, no hooks and no session kept (plain `claude -p` without the
//!   accounts mod), the prompt on its stdin, in a clean environment (no
//!   `CLAUDE_CONFIG_DIR`, `ANTHROPIC_*`, `HC_*`, `CLAUDE_CODE_*`, so the
//!   account is the one asked for), cut off after 90 seconds.
//! - **What it reads** (`prompt`): the projects (name, account, about, each
//!   repo with its role), every project folder under the roots with one
//!   cheap line on it (`describe`: `package.json`'s description, else the
//!   first heading or sentence of `README.md` or `CLAUDE.md`, a few KB of
//!   each at most), the ticket, and for a revision the draft before and the
//!   user's words.
//! - **What it answers**: JSON only, `{"project", "account", "repos":
//!   [{"folder", "why"}], "space", "goal", "done_when", "notes"}`
//!   (`parse_reply`), checked before anything is made of it (`check`): a
//!   folder that is no project folder here is left out and said so, at
//!   least one must be left, the space's name is made unique, the goal is a
//!   line of at most 120 characters and the done-when at most 400.
//! - **The draft** is the receipt a dry run prints (`launch::receipt`), with
//!   `draft: true`, `project`, `notes`, `dropped` and each session's `why`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::Setup;
use crate::launch::{self, Ask, World};

/// How long the brain has to draft it.
const LIMIT: Duration = Duration::from_secs(90);
/// The effort the draft is asked at: a pick among a few dozen lines.
const EFFORT: &str = "low";
/// How much of a folder's file is read for its line.
const READ_BYTES: u64 = 6 * 1024;
/// A folder's line, a session's why, a note: at most this long.
const LINE_CHARS: usize = 160;
/// What a failed draft says to do instead.
const INSTEAD: &str = "or name the repos in the first line instead (\"open sessions in eyzflow and mediaroom\")";

// ---------------------------------------------------------------------------
// What a folder is, in a line.

/// The start of `path`: `READ_BYTES` at most, as text.
fn head(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(READ_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `"description": "…"` in a package.json, whole or cut off.
fn package_description(text: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return v["description"].as_str().map(str::trim).filter(|d| !d.is_empty()).map(String::from);
    }
    // Cut off after READ_BYTES: the string after the key, as JSON reads it.
    let at = text.find("\"description\"")? + "\"description\"".len();
    let rest = text[at..].trim_start().strip_prefix(':')?.trim_start();
    let body = rest.strip_prefix('"')?;
    let mut end = None;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => {
                end = Some(i);
                break;
            }
            _ => escaped = false,
        }
    }
    let s: String = serde_json::from_str(&format!("\"{}\"", &body[..end?])).ok()?;
    Some(s.trim().to_string()).filter(|d| !d.is_empty())
}

/// The first heading of a README (with the sentence after it when the
/// heading is only a name), else its first sentence. Badges, images and
/// HTML are passed over.
fn readme_line(text: &str) -> Option<String> {
    let mut heading: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("[![") || line.starts_with("![") || line.starts_with('<') || line.starts_with("```") || line.starts_with("---") || line.starts_with('|') {
            continue;
        }
        if let Some(h) = line.strip_prefix('#') {
            let h = h.trim_start_matches('#').trim().trim_matches(|c: char| c == '*' || c == '`').trim();
            if h.is_empty() {
                continue;
            }
            match &heading {
                None if h.split_whitespace().count() > 2 => return Some(h.to_string()),
                None => heading = Some(h.to_string()),
                // A second heading before any sentence: the first says what it is.
                Some(first) => return Some(first.clone()),
            }
            continue;
        }
        // The first sentence (up to its full stop), its markup taken off.
        let plain: String = line.trim_start_matches(['>', '-', '*', ' ']).replace("**", "").replace('`', "");
        let sentence = match plain.find(". ") {
            Some(i) => plain[..=i].to_string(),
            None => plain.clone(),
        };
        let sentence = sentence.trim();
        if sentence.is_empty() {
            continue;
        }
        return Some(match heading {
            Some(h) => format!("{h}: {sentence}"),
            None => sentence.to_string(),
        });
    }
    heading
}

/// A folder's line from what its files say: package.json's description,
/// else the README's first heading or sentence, else CLAUDE.md's. "" when
/// none says anything.
pub fn line_of(package_json: Option<&str>, readme: Option<&str>, claude_md: Option<&str>) -> String {
    let line = package_json.and_then(package_description).or_else(|| readme.and_then(readme_line)).or_else(|| claude_md.and_then(readme_line)).unwrap_or_default();
    launch::clip_words(&line.split_whitespace().collect::<Vec<_>>().join(" "), LINE_CHARS)
}

/// What the folder is, in a line, from a few KB of its own files.
pub fn describe(folder: &Path) -> String {
    let first = |names: &[&str]| names.iter().find_map(|n| head(&folder.join(n)));
    line_of(first(&["package.json"]).as_deref(), first(&["README.md", "readme.md", "Readme.md", "README"]).as_deref(), first(&["CLAUDE.md"]).as_deref())
}

// ---------------------------------------------------------------------------
// The prompt.

/// A folder as the prompt writes it: whole, from `/`.
fn whole(repo: &str) -> String {
    crate::dispatch::expand(repo.trim()).trim_end_matches('/').to_string()
}

/// The draft before, as the prompt shows it: what a draft receipt says, in
/// the shape the answer has.
pub fn prior_brief(prior: &Value) -> Value {
    let repos: Vec<Value> = match prior["sessions"].as_array() {
        Some(sessions) => sessions.iter().map(|s| json!({ "folder": s["repo"], "why": s["why"] })).collect(),
        None => prior["repos"].as_array().cloned().unwrap_or_default(),
    };
    let account = if prior["assumed"]["account"].is_string() { prior["assumed"]["account"].clone() } else { prior["account"].clone() };
    json!({
        "project": prior["project"], "account": account, "repos": repos, "space": prior["space"],
        "goal": prior["goal"], "done_when": prior["done_when"], "notes": prior["notes"],
    })
}

/// What the brain is asked: the projects, the folders with their lines, the
/// ticket, and for a revision the draft before and what the user wants
/// changed. `pick`: the project the user chose (its repos only).
pub fn prompt(ticket: &str, projects: &[Setup], folders: &[(String, String)], pick: Option<&Setup>, prior: Option<&Value>, adjust: Option<&str>) -> String {
    let mut p = String::new();
    p += "You draft a workspace (a \"space\") for a ticket. A developer's tower opens one Claude Code session per repository the space needs, each with the whole ticket as its brief.\n\n";
    p += "Read the ticket, the projects (each repository's role in it) and the folders, and pick the repositories the ticket needs: only those it really touches (usually one or two). Prefer a project whose repositories fit; a folder outside every project is fine when the ticket needs it. Write each folder exactly as it is listed.\n\n";
    if let Some(s) = pick {
        p += &format!("The user chose the project \"{}\": take your repositories from it, and its account.\n\n", s.name);
    }
    p += "Answer with one JSON object and nothing else (no prose, no code fence):\n";
    p += "{\"project\": <the project's name as listed, or null>, \"account\": <its account, or null>, \"repos\": [{\"folder\": \"<a folder as listed>\", \"why\": \"<one line: what this repository does for the ticket>\"}], \"space\": \"<a short kebab-case name, two or three words>\", \"goal\": \"<the ticket's title, one line>\", \"done_when\": \"<its acceptance criteria, every one, joined with \\\"; \\\" on one line; \\\"\\\" when it has none>\", \"notes\": \"<one line the user should know, or \\\"\\\">\"}\n\n";
    p += "## Projects\n\n";
    if projects.is_empty() {
        p += "(none saved yet)\n";
    }
    for s in projects {
        let account = if s.account.trim().is_empty() { "the default account".to_string() } else { format!("account {}", s.account.trim()) };
        let about = if s.about.trim().is_empty() { String::new() } else { format!(": {}", s.about.trim()) };
        p += &format!("- {} ({account}){about}\n", s.name);
        for r in &s.repos {
            match s.role_of(r) {
                "" => p += &format!("  - {}\n", whole(r)),
                role => p += &format!("  - {}: {role}\n", whole(r)),
            }
        }
    }
    p += "\n## Folders\n\n";
    for (folder, line) in folders {
        if line.is_empty() {
            p += &format!("- {folder}\n");
        } else {
            p += &format!("- {folder}: {line}\n");
        }
    }
    p += "\n## The ticket\n\n<<<\n";
    p += ticket.trim();
    p += "\n>>>\n";
    if let Some(prior) = prior {
        p += "\n## Your draft before\n\n";
        p += &serde_json::to_string_pretty(&prior_brief(prior)).unwrap_or_default();
        p += "\n";
    }
    if let Some(words) = adjust.map(str::trim).filter(|w| !w.is_empty()) {
        p += "\n## What the user wants changed\n\n";
        p += words;
        p += "\n\nChange the draft as the user asks, and keep the rest of it as it was.\n";
    }
    p
}

// ---------------------------------------------------------------------------
// Its answer.

/// The JSON object the brain answered, from what `claude -p --output-format
/// json` printed (its `result`), or from the bare text. Errors say why.
pub fn parse_reply(stdout: &str) -> Result<Value> {
    let text = match serde_json::from_str::<Value>(stdout.trim()) {
        Ok(v) if v["type"] == "result" => {
            if v["is_error"] == true || v["subtype"].as_str().is_some_and(|s| s != "success") {
                let why = v["result"].as_str().or(v["subtype"].as_str()).unwrap_or("it failed");
                bail!("the brain didn't answer: {}", why.trim());
            }
            v["result"].as_str().unwrap_or("").to_string()
        }
        // The object itself (a program that prints it bare).
        Ok(v) if v.is_object() => return Ok(v),
        _ => stdout.to_string(),
    };
    let (Some(from), Some(to)) = (text.find('{'), text.rfind('}')) else { bail!("the brain answered no JSON: {}", launch::clip_words(text.trim(), 120)) };
    if to < from {
        bail!("the brain answered no JSON: {}", launch::clip_words(text.trim(), 120));
    }
    match serde_json::from_str::<Value>(&text[from..=to]) {
        Ok(v) if v.is_object() => Ok(v),
        _ => bail!("the brain's answer isn't the JSON it was asked for: {}", launch::clip_words(text.trim(), 120)),
    }
}

/// A draft, checked: only what the tower knows, as a launch takes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Draft {
    pub project: Option<String>,
    pub account: Option<String>,
    /// Each repo's folder (whole) and why it is in it.
    pub repos: Vec<(String, String)>,
    pub space: String,
    /// The space's name was the brain's, but taken: this is the one it asked for.
    pub space_taken: Option<String>,
    pub goal: String,
    pub done_when: String,
    pub notes: String,
    /// What it named that the tower left out, and why.
    pub dropped: Vec<String>,
}

/// A line: one space between words, at most `max` characters.
fn one_line(v: &Value, max: usize) -> String {
    launch::clip_words(&v.as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// A space's name as Herdr takes one: lower case, a dash between words,
/// at most 32 characters (cut at a dash).
fn kebab(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let mut out = out.trim_matches('-').to_string();
    if out.len() > 32 {
        let cut = &out[..32];
        out = cut.rfind('-').filter(|&i| i >= 8).map(|i| cut[..i].to_string()).unwrap_or_else(|| cut.to_string());
    }
    out.trim_matches('-').to_string()
}

/// The folders a draft may name: every project folder under the roots and
/// every repo of a saved project that is a folder here, each whole and as
/// the file system has it (`~` made whole, links followed).
fn known_folders(w: &World) -> Vec<(PathBuf, PathBuf)> {
    let mut out: Vec<(PathBuf, PathBuf)> = vec![];
    let saved = w.setups.iter().flat_map(|s| s.repos.iter().map(|r| PathBuf::from(whole(r))));
    for p in w.projects.iter().cloned().chain(saved) {
        if !p.is_dir() {
            continue;
        }
        let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if !out.iter().any(|(_, r)| *r == real) {
            out.push((p, real));
        }
    }
    out
}

/// The folder a draft named, if the tower knows it: by its path (`~` or
/// whole, compared as the file system has it), or by a name only one known
/// folder has.
fn known(named: &str, folders: &[(PathBuf, PathBuf)]) -> Option<PathBuf> {
    let named = named.trim().trim_end_matches('/');
    if named.is_empty() {
        return None;
    }
    if named.starts_with('/') || named.starts_with('~') {
        let p = PathBuf::from(whole(named));
        let real = std::fs::canonicalize(&p).unwrap_or(p);
        return folders.iter().find(|(_, r)| *r == real).map(|(p, _)| p.clone());
    }
    let by_name: Vec<&PathBuf> = folders.iter().filter(|(p, _)| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(named))).map(|(p, _)| p).collect();
    match by_name.as_slice() {
        [one] => Some((*one).clone()),
        _ => None,
    }
}

/// The brain's answer checked against what there is: unknown folders,
/// projects and accounts are left out (and said so in `dropped`), the name
/// made unique, the goal and done-when cut to a line each. `pick`: the
/// project the user chose, which wins. An error when no repo is left.
pub fn check(reply: &Value, ticket: &str, w: &World, pick: Option<&str>) -> Result<Draft> {
    let mut d = Draft::default();
    let folders = known_folders(w);
    let entries: Vec<(String, String)> = reply["repos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| match r {
            Value::String(f) => Some((f.clone(), String::new())),
            Value::Object(_) => Some((r["folder"].as_str().or(r["path"].as_str()).unwrap_or("").to_string(), one_line(&r["why"], LINE_CHARS))),
            _ => None,
        })
        .collect();
    for (named, why) in entries {
        match known(&named, &folders) {
            Some(f) => {
                let f = f.to_string_lossy().into_owned();
                if !d.repos.iter().any(|(x, _)| *x == f) {
                    d.repos.push((f, why));
                }
            }
            None if named.trim().is_empty() => {}
            None => d.dropped.push(format!("{}: not a project folder here", named.trim())),
        }
    }
    if d.repos.is_empty() {
        let said = if d.dropped.is_empty() { "it named none".to_string() } else { format!("it left out {}", d.dropped.join("; ")) };
        bail!("the draft has no repo the tower knows ({said}): draft it again, {INSTEAD}");
    }
    // The project: the one the user chose, else the brain's if there is one by that name.
    let project = pick.map(String::from).or_else(|| reply["project"].as_str().map(str::trim).filter(|p| !p.is_empty()).map(String::from));
    d.project = match project {
        Some(p) => match w.setups.iter().find(|s| s.name.trim().eq_ignore_ascii_case(&p)) {
            Some(s) => Some(s.name.clone()),
            None if pick.is_some() => Some(crate::setups::pick(&w.setups, Some(&p))?.0.name.clone()),
            None => {
                d.dropped.push(format!("the project \"{p}\": there is none by that name"));
                None
            }
        },
        None => None,
    };
    // The account: the brain's when the accounts mod knows it; else (None) the project's, else the default.
    if let Some(a) = reply["account"].as_str().map(str::trim).filter(|a| !a.is_empty()) {
        match crate::accounts::resolve(Some(a), &w.accounts) {
            Ok(Some(known)) => d.account = Some(known),
            _ if w.accounts.is_empty() => {}
            _ => d.dropped.push(format!("the account \"{a}\": no Claude account by that name")),
        }
    }
    if pick.is_some() {
        // The user's project brings its own account.
        d.account = None;
    }
    // The goal is a line: the brain's title, else the ticket's own.
    d.goal = match launch::title_text(reply["goal"].as_str().unwrap_or("").lines().next().unwrap_or("")) {
        g if g.is_empty() => launch::title_of(ticket, &[]),
        g => g,
    };
    d.done_when = match one_line(&reply["done_when"], launch::DONE_WHEN_CHARS) {
        x if x.is_empty() => launch::done_when_of(ticket),
        x => x,
    };
    d.notes = one_line(&reply["notes"], LINE_CHARS);
    let base = match kebab(reply["space"].as_str().unwrap_or("")) {
        b if b.is_empty() => kebab(&launch::space_name(&d.goal)),
        b => b,
    };
    let base = if base.is_empty() { "space".to_string() } else { base };
    d.space = launch::unique(&base, &w.spaces);
    if d.space != base {
        d.space_taken = Some(base);
    }
    Ok(d)
}

/// The draft as a dry run's receipt (`launch::receipt`) with what the brain
/// said: `draft`, `project`, `notes`, `dropped`, and each session's `why`.
pub fn receipt(d: &Draft, ticket: &str, w: &World) -> Result<Value> {
    let repos: Vec<String> = d.repos.iter().map(|(f, _)| f.clone()).collect();
    let ask = Ask { setup: d.project.as_deref(), name: Some(&d.space), repos: &repos, title: Some(&d.goal), done_when: Some(&d.done_when), account: d.account.as_deref(), plan: None, review: None, story: None };
    let mut plan = launch::plan_with(ticket, &ask, w)?;
    for s in plan.sessions.iter_mut() {
        if let Some((_, why)) = d.repos.iter().find(|(f, _)| *f == s.folder || whole(f) == s.folder).filter(|(_, why)| !why.is_empty()) {
            s.why = why.clone();
        }
    }
    let mut r = launch::receipt(&plan, &[], None, true);
    r["draft"] = json!(true);
    r["project"] = json!(d.project);
    r["notes"] = json!(d.notes);
    r["dropped"] = json!(d.dropped);
    r["repos_from"] = json!("draft");
    let a = &mut r["assumed"];
    a["setup"] = json!(d.project.clone().unwrap_or_else(|| "the ticket's repos".into()));
    a["setup_why"] = json!(if d.project.is_some() { "the brain took it for the ticket" } else { "the brain picked the repos the ticket needs; no project fits" });
    a["name_why"] = json!(match &d.space_taken {
        Some(base) => format!("the brain's name for it; \"{base}\" is taken"),
        None => "the brain's name for it".to_string(),
    });
    if d.account.is_some() {
        a["account_why"] = json!("the draft's");
    }
    Ok(r)
}

// ---------------------------------------------------------------------------
// Running it.

/// An environment variable the drafting `claude` must not inherit: the
/// account it runs on is the one asked for, never this terminal's.
pub fn left_out(key: &str) -> bool {
    matches!(key, "CLAUDE_CONFIG_DIR" | "CLAUDECODE" | "CCA_ACCOUNT" | "CLAUDE_PID" | "CLAUDE_EFFORT")
        || key.starts_with("ANTHROPIC_")
        || key.starts_with("HC_")
        || key.starts_with("CLAUDE_CODE_")
}

/// The headless call's program and arguments: `cca <account> -p …` (or
/// plain `claude -p …` without the accounts mod), no tools, no MCP servers,
/// no hooks, nothing kept.
pub fn argv(cca: Option<&Path>, account: Option<&str>, model: &str) -> Vec<String> {
    let mut a: Vec<String> = match (cca, account) {
        (Some(cca), Some(acc)) => vec![cca.to_string_lossy().into_owned(), acc.to_string()],
        _ => vec!["claude".into()],
    };
    a.extend(
        ["-p", "--output-format", "json", "--model", model, "--effort", EFFORT, "--no-session-persistence", "--strict-mcp-config", "--settings", r#"{"disableAllHooks":true}"#, "--tools", ""]
            .map(String::from),
    );
    a
}

/// Runs `cmd` with `input` on its stdin, to its end or until `limit`.
pub(crate) fn run_with_input(mut cmd: Command, input: &str, limit: Duration) -> Result<(i32, String, String)> {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let input = input.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let drain = |p: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = p {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("it took longer than {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = writer.join();
    let text = |h: std::thread::JoinHandle<Vec<u8>>| String::from_utf8_lossy(&h.join().unwrap_or_default()).into_owned();
    Ok((status.code().unwrap_or(-1), text(out), text(err)))
}

/// Asks the brain once: what it printed.
fn ask_brain(prompt: &str, w: &World) -> Result<String> {
    let cfg = crate::config::Config::load()?;
    let cca = crate::accounts::cca_bin();
    let account = crate::accounts::default_of(&w.accounts);
    let argv = argv(cca.as_deref().filter(|_| account.is_some()), account.as_deref(), &cfg.brains.model);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(std::env::temp_dir());
    for (key, _) in std::env::vars_os() {
        if left_out(&key.to_string_lossy()) {
            cmd.env_remove(&key);
        }
    }
    let (code, out, err) = run_with_input(cmd, prompt, LIMIT).map_err(|e| anyhow::anyhow!("the tower's brain couldn't read the ticket ({e:#}): try again, {INSTEAD}"))?;
    if code != 0 && !out.trim_start().starts_with('{') {
        let why = err.lines().chain(out.lines()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("it failed");
        bail!("the tower's brain couldn't read the ticket ({}: {why}): try again, {INSTEAD}", argv[0].rsplit('/').next().unwrap_or("claude"));
    }
    Ok(out)
}

/// The draft for `ticket` (with `adjust` and `prior`: the draft before,
/// changed as the user said): its receipt.
pub fn draft(ticket: &str, pick: Option<&str>, adjust: Option<&str>, prior: Option<&str>) -> Result<Value> {
    let ticket = launch::tidy(ticket);
    if ticket.is_empty() {
        bail!("tower launch \"<the ticket>\" --draft: paste the ticket");
    }
    let prior: Option<Value> = match prior.map(str::trim).filter(|p| !p.is_empty()) {
        Some(text) => Some(serde_json::from_str(text).map_err(|e| anyhow::anyhow!("--prior: not a draft's JSON ({e})"))?),
        None => None,
    };
    if adjust.is_some_and(|a| !a.trim().is_empty()) && prior.is_none() {
        bail!("--adjust changes a draft: give the draft before with --prior '<its JSON>'");
    }
    let w = launch::world_now()?;
    let picked = match pick.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Some(crate::setups::pick(&w.setups, Some(p))?.0.clone()),
        None => None,
    };
    // The folders, each with its line: those of the project the user chose, else all of them.
    let folders: Vec<(String, String)> = match &picked {
        Some(s) => s.repos.iter().map(|r| (whole(r), describe(Path::new(&whole(r))))).collect(),
        None => w.projects.iter().map(|p| (p.to_string_lossy().into_owned(), describe(p))).collect(),
    };
    let projects: Vec<Setup> = match &picked {
        Some(s) => vec![s.clone()],
        None => w.setups.clone(),
    };
    let text = prompt(&ticket, &projects, &folders, picked.as_ref(), prior.as_ref(), adjust);
    let started = Instant::now();
    let out = ask_brain(&text, &w)?;
    let reply = parse_reply(&out).map_err(|e| anyhow::anyhow!("{e:#}: draft it again, {INSTEAD}"))?;
    let d = check(&reply, &ticket, &w, picked.as_ref().map(|s| s.name.as_str()))?;
    let mut r = receipt(&d, &ticket, &w)?;
    r["took_ms"] = json!(started.elapsed().as_millis() as u64);
    Ok(r)
}

/// `'…'` for a shell.
fn quoted(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./~=:@".contains(c)) { s.to_string() } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

/// The flags that launch exactly this draft (after `tower launch "<the
/// ticket>"`): `--repo` each, `--name`, `--title`, `--done-when`, and the
/// account and project it settled on.
pub fn confirm_flags(r: &Value) -> Vec<String> {
    let mut a: Vec<String> = vec![];
    for s in r["sessions"].as_array().into_iter().flatten() {
        if let Some(f) = s["repo"].as_str().filter(|f| !f.is_empty()) {
            a.extend(["--repo".into(), f.to_string()]);
        }
    }
    a.extend(["--name".into(), r["space"].as_str().unwrap_or("").to_string(), "--title".into(), r["goal"].as_str().unwrap_or("").to_string()]);
    a.extend(["--done-when".into(), r["done_when"].as_str().unwrap_or("").to_string()]);
    if let Some(acc) = r["assumed"]["account"].as_str().filter(|x| !x.is_empty()) {
        a.extend(["--account".into(), acc.to_string()]);
    }
    if let Some(p) = r["project"].as_str().filter(|x| !x.is_empty()) {
        a.extend(["--setup".into(), p.to_string()]);
    }
    a
}

/// The draft in words.
pub fn said(r: &Value) -> String {
    let mut out = format!("Draft of \"{}\": {}\n", r["space"].as_str().unwrap_or(""), r["goal"].as_str().unwrap_or(""));
    if let Some(d) = r["done_when"].as_str().filter(|d| !d.is_empty()) {
        out += &format!("Done when: {d}\n");
    }
    for s in r["sessions"].as_array().into_iter().flatten() {
        let repo = s["repo"].as_str().unwrap_or("");
        let name = repo.rsplit('/').next().unwrap_or(repo);
        match s["why"].as_str().filter(|w| !w.is_empty()) {
            Some(why) => out += &format!("  {name}: {why}\n"),
            None => out += &format!("  {name}\n"),
        }
    }
    let account = r["assumed"]["account"].as_str().map(|a| format!(", on the account {a}")).unwrap_or_default();
    match r["project"].as_str() {
        Some(p) => out += &format!("Project {p}{account}.\n"),
        None => out += &format!("No project{account}.\n"),
    }
    if let Some(n) = r["notes"].as_str().filter(|n| !n.is_empty()) {
        out += &format!("Note: {n}\n");
    }
    for d in r["dropped"].as_array().into_iter().flatten().filter_map(|d| d.as_str()) {
        out += &format!("Left out {d}.\n");
    }
    let flags: Vec<String> = confirm_flags(r).iter().map(|f| quoted(f)).collect();
    out += &format!("Nothing was opened. To launch it: tower launch \"<the same ticket>\" {}\n", flags.join(" "));
    out += "To change it: tower launch \"<the same ticket>\" --draft --adjust \"<what to change>\" --prior '<this draft's JSON>'";
    out
}

/// `tower launch "<ticket>" --draft [--setup <project>] [--adjust "<words>" --prior '<draft JSON>'] [--json]`.
pub fn cmd(ticket: &str, pick: Option<&str>, adjust: Option<&str>, prior: Option<&str>, as_json: bool) -> Result<()> {
    let r = draft(ticket, pick, adjust, prior)?;
    if as_json || !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        println!("{}", said(&r));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktrees::tests::{repo, temp};

    const TICKET: &str = "EYZFlow Support – Enable Automatic Message and Reply Translation\n\nCustomers write to support in many languages; agents answer in German.\n\nAcceptance criteria:\n- An incoming message shows translated to German beside the original\n- A reply is sent in the customer's language\n\nNotes: use the translation lambda.";

    fn world(base: &Path) -> (World, PathBuf, PathBuf) {
        let (flow, room) = (repo(base, "eyzflow"), repo(base, "eyz-mediaroom-serverless"));
        let other = repo(base, "landing-design");
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut project = Setup { name: "EYZFlow".into(), account: "orca".into(), about: "the video platform agencies run on".into(), repos: vec![s(&flow), s(&room)], ..Default::default() };
        project.roles.insert(s(&flow), "the web app and its API".into());
        let w = World {
            setups: vec![project],
            spaces: vec!["msg-translation".into()],
            accounts: ["default", "orca"].iter().map(|n| crate::accounts::Account { name: n.to_string(), email: String::new() }).collect(),
            worktrees: true,
            projects: vec![flow.clone(), room.clone(), other],
            ..Default::default()
        };
        (w, flow, room)
    }

    #[test]
    fn a_folder_is_described_by_its_own_files_in_a_line() {
        assert_eq!(line_of(Some(r#"{"name":"eyzflow","description":"The EYZFlow web app","version":"1.0.0"}"#), Some("# Other"), None), "The EYZFlow web app");
        // A package.json cut off after a few KB still gives its description.
        assert_eq!(line_of(Some("{\n  \"name\": \"x\",\n  \"description\": \"Media \\\"backend\\\" on AWS\",\n  \"scripts\": {\"a"), None, None), "Media \"backend\" on AWS");
        // No description: the README's heading, with its first sentence when the heading is only a name.
        assert_eq!(line_of(Some(r#"{"name":"x"}"#), Some("[![ci](x)](y)\n\n# EYZFlow\n\nThe platform agencies run their media on. It has more.\n"), None), "EYZFlow: The platform agencies run their media on.");
        assert_eq!(line_of(None, Some("# The tower for Herdr sessions\n\ntext"), None), "The tower for Herdr sessions");
        assert_eq!(line_of(None, Some("<p align=center>\n\nJust a sentence here. And more"), None), "Just a sentence here.");
        // No README: CLAUDE.md; nothing at all: nothing.
        assert_eq!(line_of(None, None, Some("# CLAUDE.md\n## Overview\n")), "CLAUDE.md");
        assert_eq!(line_of(None, None, None), "");
        assert!(line_of(None, Some(&format!("# {}", "word ".repeat(80))), None).chars().count() <= LINE_CHARS);
        let base = temp("ticket-describe");
        let d = repo(&base, "app");
        std::fs::write(d.join("package.json"), r#"{"description": "  The app  "}"#).unwrap();
        assert_eq!(describe(&d), "The app");
        assert_eq!(describe(&base.join("nothing-here")), "");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_prompt_holds_the_projects_the_folders_and_the_ticket() {
        let base = temp("ticket-prompt");
        let (w, flow, room) = world(&base);
        let folders = vec![(flow.to_string_lossy().into_owned(), "The EYZFlow web app".to_string()), (room.to_string_lossy().into_owned(), String::new())];
        let p = prompt(TICKET, &w.setups, &folders, None, None, None);
        assert!(p.contains("- EYZFlow (account orca): the video platform agencies run on\n"), "{p}");
        assert!(p.contains(&format!("  - {}: the web app and its API\n", flow.display())), "{p}");
        assert!(p.contains(&format!("  - {}\n", room.display())), "a repo with no role is listed bare");
        assert!(p.contains(&format!("- {}: The EYZFlow web app\n", flow.display())));
        assert!(p.contains("<<<\nEYZFlow Support – Enable Automatic Message and Reply Translation\n") && p.contains("use the translation lambda.\n>>>\n"));
        assert!(p.contains("\"done_when\"") && p.contains("JSON object and nothing else"));
        assert!(!p.contains("draft before") && !p.contains("wants changed"));
        // A revision: the draft before, in the answer's shape, and the user's words.
        let prior = json!({ "space": "msg-translation-2", "goal": "Translate support messages", "done_when": "", "project": "EYZFlow", "notes": "",
            "assumed": { "account": "orca" }, "sessions": [{ "repo": flow.to_string_lossy(), "why": "the inbox UI" }] });
        let p = prompt(TICKET, &w.setups, &folders, Some(&w.setups[0]), Some(&prior), Some("  leave mediaroom out "));
        assert!(p.contains("The user chose the project \"EYZFlow\""));
        assert!(p.contains("## Your draft before") && p.contains("\"why\": \"the inbox UI\"") && p.contains("\"account\": \"orca\""), "{p}");
        assert!(p.contains("## What the user wants changed\n\nleave mediaroom out\n"));
        assert_eq!(prior_brief(&prior)["repos"][0]["folder"], flow.to_string_lossy().as_ref());
        // No projects yet: it says so.
        assert!(prompt(TICKET, &[], &folders, None, None, None).contains("(none saved yet)"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_answer_is_read_from_what_claude_printed() {
        let inner = r#"{"project":"EYZFlow","account":null,"repos":[{"folder":"/x","why":"y"}],"space":"msg-translation","goal":"G","done_when":"","notes":""}"#;
        let envelope = json!({ "type": "result", "subtype": "success", "is_error": false, "result": inner, "duration_ms": 9000 }).to_string();
        assert_eq!(parse_reply(&envelope).unwrap()["space"], "msg-translation");
        // In a code fence, or with words around it: the object inside.
        let fenced = json!({ "type": "result", "subtype": "success", "is_error": false, "result": format!("Here it is:\n```json\n{inner}\n```") }).to_string();
        assert_eq!(parse_reply(&fenced).unwrap()["goal"], "G");
        // Bare, as another program prints it.
        assert_eq!(parse_reply(inner).unwrap()["project"], "EYZFlow");
        // An error, junk, or no object: said why.
        let failed = json!({ "type": "result", "subtype": "success", "is_error": true, "result": "Credit balance is too low" }).to_string();
        assert!(parse_reply(&failed).unwrap_err().to_string().contains("Credit balance is too low"));
        let turns = json!({ "type": "result", "subtype": "error_max_turns", "is_error": false }).to_string();
        assert!(parse_reply(&turns).unwrap_err().to_string().contains("error_max_turns"));
        assert!(parse_reply("I can't help with that.").unwrap_err().to_string().contains("no JSON"));
        assert!(parse_reply("{not json at all}").unwrap_err().to_string().contains("isn't the JSON"));
        assert!(parse_reply("} backwards {").is_err() && parse_reply("").is_err());
    }

    #[test]
    fn a_draft_keeps_only_what_the_tower_knows() {
        let base = temp("ticket-check");
        let (w, flow, room) = world(&base);
        let f = |p: &Path| p.to_string_lossy().into_owned();
        let reply = json!({
            "project": "eyzflow", "account": "orca",
            "repos": [
                { "folder": format!("{}/", f(&flow)), "why": "  the inbox   UI " },
                { "folder": "/nowhere/eyz-chat", "why": "made up" },
                { "folder": "eyz-mediaroom-serverless", "why": "the translation lambda" },
                { "folder": f(&flow), "why": "twice" },
            ],
            "space": "Msg Translation!", "goal": "EYZFlow Support – Enable Automatic Message and Reply Translation\nmore", "done_when": "", "notes": "Mediaroom has the lambda.",
        });
        let d = check(&reply, TICKET, &w, None).unwrap();
        assert_eq!(d.repos, [(f(&flow), "the inbox UI".to_string()), (f(&room), "the translation lambda".to_string())], "a path with a slash, a name only one folder has, each once");
        assert_eq!(d.dropped, ["/nowhere/eyz-chat: not a project folder here"]);
        assert_eq!((d.project.as_deref(), d.account.as_deref()), (Some("EYZFlow"), Some("orca")), "the project as it is called");
        // The name: kebab, and unique ("msg-translation" is taken).
        assert_eq!((d.space.as_str(), d.space_taken.as_deref()), ("msg-translation-2", Some("msg-translation")));
        assert_eq!(d.goal, "EYZFlow Support – Enable Automatic Message and Reply Translation", "one line");
        // None given: from the ticket's acceptance criteria.
        assert_eq!(d.done_when, "An incoming message shows translated to German beside the original; A reply is sent in the customer's language");
        // As a receipt: the dry run's shape, with the draft's own.
        let r = receipt(&d, TICKET, &w).unwrap();
        assert_eq!((r["draft"].as_bool(), r["dry_run"].as_bool(), r["project"].as_str(), r["space"].as_str()), (Some(true), Some(true), Some("EYZFlow"), Some("msg-translation-2")));
        assert_eq!(r["sessions"].as_array().unwrap().len(), 2);
        assert_eq!((r["sessions"][0]["why"].as_str(), r["sessions"][0]["name"].as_str()), (Some("the inbox UI"), Some("msg-translation-2-eyzflow")));
        assert_eq!(r["sessions"][1]["account"], "orca");
        assert_eq!(r["assumed"]["name_why"], "the brain's name for it; \"msg-translation\" is taken");
        assert_eq!((r["goal"].as_str(), r["notes"].as_str()), (Some(d.goal.as_str()), Some("Mediaroom has the lambda.")));
        assert_eq!(r["dropped"][0], "/nowhere/eyz-chat: not a project folder here");
        assert!(!base.join(".tower-worktrees").exists(), "a draft makes nothing");
        // Launched as confirmed: exactly these flags.
        let flags = confirm_flags(&r);
        assert_eq!(
            flags,
            [
                "--repo", &f(&flow), "--repo", &f(&room), "--name", "msg-translation-2", "--title", &d.goal, "--done-when", &d.done_when, "--account", "orca", "--setup", "EYZFlow",
            ]
        );
        let text = said(&r);
        assert!(text.starts_with("Draft of \"msg-translation-2\": EYZFlow Support") && text.contains("  eyzflow: the inbox UI\n") && text.contains("Project EYZFlow, on the account orca."), "{text}");
        assert!(text.contains("--name msg-translation-2 --title 'EYZFlow Support"), "{text}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_draft_without_a_known_repo_or_with_junk_is_refused_or_mended() {
        let base = temp("ticket-junk");
        let (w, flow, _) = world(&base);
        // Only made-up folders: an error that says what to do.
        let err = check(&json!({ "repos": [{ "folder": "/nowhere/x" }], "space": "x", "goal": "y" }), TICKET, &w, None).unwrap_err().to_string();
        assert!(err.contains("no repo the tower knows") && err.contains("/nowhere/x") && err.contains("name the repos in the first line"), "{err}");
        assert!(check(&json!({}), TICKET, &w, None).unwrap_err().to_string().contains("it named none"));
        assert!(check(&json!({ "repos": "eyzflow" }), TICKET, &w, None).is_err(), "repos that aren't a list");
        // Fields missing or of the wrong kind: the ticket's own title, name and done-when; the default account.
        let d = check(&json!({ "repos": [flow.to_string_lossy()], "goal": 7, "project": "nobody's", "account": "nobody", "notes": ["x"] }), TICKET, &w, None).unwrap();
        assert_eq!(d.repos.len(), 1);
        assert_eq!(d.goal, "EYZFlow Support – Enable Automatic Message and Reply Translation");
        assert_eq!(d.space, "eyzflow-support");
        assert!(d.done_when.starts_with("An incoming message shows translated"));
        assert_eq!((d.project.clone(), d.account.clone(), d.notes.as_str()), (None, None, ""));
        assert_eq!(d.dropped, ["the project \"nobody's\": there is none by that name", "the account \"nobody\": no Claude account by that name"]);
        let r = receipt(&d, TICKET, &w).unwrap();
        assert_eq!((r["assumed"]["account"].as_str(), r["project"].is_null()), (Some("default"), true));
        assert!(!confirm_flags(&r).contains(&"--setup".to_string()));
        // A goal too long is cut to a line; a done-when too; a name too long is cut at a dash.
        let long = json!({ "repos": [flow.to_string_lossy()], "goal": "word ".repeat(60), "done_when": "x ".repeat(400), "space": "a-very-long-space-name-that-goes-on-and-on-forever" });
        let d = check(&long, TICKET, &w, None).unwrap();
        assert!(d.goal.chars().count() <= launch::TITLE_CHARS && d.goal.ends_with('…'));
        assert!(d.done_when.chars().count() <= launch::DONE_WHEN_CHARS);
        assert!(d.space.len() <= 32 && !d.space.ends_with('-'), "{}", d.space);
        // The project the user chose wins, with its own account.
        let d = check(&json!({ "repos": [flow.to_string_lossy()], "project": null, "account": "default" }), TICKET, &w, Some("eyzflow")).unwrap();
        assert_eq!((d.project.as_deref(), d.account.as_deref()), (Some("EYZFlow"), None));
        assert_eq!(receipt(&d, TICKET, &w).unwrap()["assumed"]["account"], "orca");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_brain_runs_on_the_account_asked_for_with_nothing_of_this_terminal() {
        for k in ["CLAUDE_CONFIG_DIR", "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "HC_TERMINAL_ID", "CLAUDE_CODE_SESSION_ID", "CLAUDECODE", "CCA_ACCOUNT"] {
            assert!(left_out(k), "{k}");
        }
        for k in ["PATH", "HOME", "HERDER_TOWER_HOME", "TOWER_CCA", "LANG"] {
            assert!(!left_out(k), "{k}");
        }
        let a = argv(Some(Path::new("/x/cca")), Some("default"), "claude-opus-5-5");
        assert_eq!(&a[..3], ["/x/cca", "default", "-p"]);
        assert!(a.windows(2).any(|w| w == ["--model", "claude-opus-5-5"]) && a.windows(2).any(|w| w == ["--effort", "low"]) && a.windows(2).any(|w| w == ["--output-format", "json"]));
        assert_eq!(a.last().map(String::as_str), Some(""), "no tools: the last, so it takes nothing after it");
        assert_eq!(argv(None, Some("default"), "m")[0], "claude", "no accounts mod: plain claude");
        // The prompt goes in on stdin; one that takes too long is stopped.
        let mut cat = Command::new("/bin/cat");
        cat.arg("-");
        assert_eq!(run_with_input(cat, "{\"a\":1}", Duration::from_secs(5)).unwrap(), (0, "{\"a\":1}".to_string(), String::new()));
        let mut slow = Command::new("/bin/sleep");
        slow.arg("5");
        assert!(run_with_input(slow, "", Duration::from_millis(200)).unwrap_err().to_string().contains("longer than"));
    }
}
