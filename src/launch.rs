//! `tower launch "<goal>"`: one sentence opens a space.
//!
//! It names the space from the goal (its first two words that carry meaning,
//! made unique among the spaces there are), takes the repos (those `--repo`
//! names; else those the goal's first line names; else a project's,
//! `setups`: the one named, else the only one, else the first), makes the
//! Herdr space, opens one session per repo on its account (each through
//! `tower start --space`, so each in the space's worktree of its repo with a
//! dev port of its own: see `worktrees`), sets the space's goal (the
//! ticket's title, `--title`) and what counts as done (its acceptance
//! criteria, `--done-when`), and hands the whole text out as the first
//! request: one agenda, one task per session. The sessions are introduced to
//! each other by the voice as they join (`introduce`).
//!
//! It asks nothing. What it chose is in its one receipt (`assumed`), and
//! `--dry-run` prints the same plan without doing any of it. A ticket that
//! names no repo is drafted first (`crate::ticket`, `--draft`): the draft is
//! launched as it was confirmed, with `--repo`, `--name`, `--title` and
//! `--done-when`.

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
    /// Why this repo is in it, in a line (its role in the project, what the draft said).
    pub why: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The space's goal: a line (the ticket's title, or `--title`).
    pub goal: String,
    /// What counts as done ("": nothing said).
    pub done_when: String,
    /// Everything that was given, tidied: the sessions' first task and the request's words.
    pub ticket: String,
    /// Where the repos came from: `first-line`, `setup` or `repos` (`--repo`).
    pub repos_from: &'static str,
    pub space: String,
    /// A space of that name is there already (`--name`): the sessions join it.
    pub joins: bool,
    pub setup: Setup,
    /// The account as the claude-accounts mod knows it (None: no accounts, plain `claude`).
    pub account: Option<String>,
    pub sessions: Vec<Session>,
    /// What it chose by itself, for the receipt.
    pub assumed: Value,
    /// The plan it was planned with (`--plan`): each session's first
    /// instruction carries its link and its part of it.
    pub plan: Option<crate::plans::PlanBrief>,
    /// The PRs its sessions open get a reviewer (`crate::reviews`):
    /// `--review` / `--no-review`, else as the space has it (on for a new one).
    pub review: bool,
}

/// What a launch is asked beside its goal, each as `tower launch` takes it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ask<'a> {
    /// `--setup`: the project.
    pub setup: Option<&'a str>,
    /// `--name`: the space.
    pub name: Option<&'a str>,
    /// `--repo`: exactly these folders (a confirmed draft), not those the goal names nor a project's.
    pub repos: &'a [String],
    /// `--title`: the space's goal (default: the ticket's title).
    pub title: Option<&'a str>,
    /// `--done-when` (default: the ticket's acceptance criteria).
    pub done_when: Option<&'a str>,
    /// `--account` (default: the project's, else the default one).
    pub account: Option<&'a str>,
    /// `--plan`: the plan it was planned with (`tower plan`).
    pub plan: Option<&'a str>,
    /// `--review` (true) or `--no-review` (false): whether the PRs its
    /// sessions open get a reviewer. Neither: as the space has it (on).
    pub review: Option<bool>,
    /// `--story-file`: the conversation that made it (`Told`), kept as the
    /// space's own once it is launched (`record_story`).
    pub story: Option<&'a str>,
}

/// One message of the conversation that made a space (the full window's
/// New space): who said it (`you`, `tower`, `planner`), its words, when.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Told {
    pub who: String,
    pub text: String,
    pub at: u64,
}

/// Keeps `story`, the conversation that made `space`, in the history as
/// that space's own (`story_entries`). How many messages it kept.
pub fn record_story(space: &str, story: &[Told], r: &Value) -> usize {
    let entries = story_entries(space, story, r, crate::history::now_ms());
    let kept = entries.len().saturating_sub(1);
    for e in entries {
        crate::history::append(e);
    }
    kept
}

/// `story` as the history keeps it (`now`: the launch's time): a
/// `creation` entry of `space` for each message (who said it in `status`:
/// `you`, `tower` or `planner`; written down, never spoken), at the time it
/// was said, and what the launch did (`r`, its receipt) as the tower's last
/// word. None for a story with nothing in it.
pub fn story_entries(space: &str, story: &[Told], r: &Value, now: u64) -> Vec<crate::history::Entry> {
    let mut out: Vec<crate::history::Entry> = story
        .iter()
        .filter(|t| !t.text.trim().is_empty())
        .map(|t| {
            let who = if matches!(t.who.as_str(), "you" | "planner") { t.who.as_str() } else { "tower" };
            creation(space, who, t.text.trim(), if t.at == 0 || t.at > now { now } else { t.at })
        })
        .collect();
    if !out.is_empty()
        && let Some(line) = launched_line(r)
    {
        out.push(creation(space, "tower", &line, now));
    }
    out
}

fn creation(space: &str, who: &str, text: &str, at: u64) -> crate::history::Entry {
    crate::history::Entry { kind: "creation".into(), space: Some(space.into()), status: Some(who.into()), text: text.into(), at, record: true, ..Default::default() }
}

/// What a launch did, from its receipt, in a sentence or two: `Launched
/// fix-login: 2 sessions on orca (eyzflow, mediaroom), each in its own
/// worktree.` None when it isn't a receipt.
pub fn launched_line(receipt: &Value) -> Option<String> {
    let space = receipt["space"].as_str().filter(|s| !s.is_empty())?;
    let sessions = receipt["sessions"].as_array()?;
    let opened: Vec<&Value> = sessions.iter().filter(|s| s["error"].is_null()).collect();
    let account = receipt["assumed"]["account"].as_str().filter(|x| !x.is_empty());
    let on = account.map(|x| format!(" on {x}")).unwrap_or_default();
    let repos = opened.iter().map(|s| repo_name(s["repo"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ");
    let own = opened.iter().filter(|s| s["worktree"].as_str().is_some_and(|w| !w.is_empty())).count();
    let ports: Vec<String> = opened.iter().filter_map(|s| s["port"].as_u64()).map(|p| p.to_string()).collect();
    let place = match (own, opened.len()) {
        (0, _) => String::new(),
        (_, 1) => ", in its own worktree".to_string(),
        (own, n) if own == n => ", each in its own worktree".to_string(),
        (own, n) => format!(", {own} of {n} in their own worktree"),
    };
    let ports = match ports.as_slice() {
        [] => String::new(),
        [one] => format!(" (port {one})"),
        list => format!(" (ports {})", list.join(", ")),
    };
    let head = if receipt["made"] == false { format!("Joined {space}") } else { format!("Launched {space}") };
    let mut parts = vec![match opened.len() {
        0 => format!("{head}."),
        1 => format!("{head}: 1 session{on} ({repos}){place}{ports}."),
        n => format!("{head}: {n} sessions{on} ({repos}){place}{ports}."),
    }];
    for s in sessions.iter().filter(|s| !s["error"].is_null()) {
        parts.push(format!("{} didn’t open: {}.", repo_name(s["repo"].as_str().unwrap_or("A session")), s["error"].as_str().unwrap_or("").trim_end_matches('.')));
    }
    if let Some(setup) = receipt["assumed"]["setup"].as_str().filter(|x| !x.is_empty()) {
        parts.push(format!("It took the setup “{setup}”."));
    }
    Some(parts.join(" "))
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
    /// Spaces whose PRs get no reviewer (`tower space review <space> off`).
    pub no_review: Vec<String>,
    /// The project folders (`projects::candidates`): the repos a goal names.
    pub projects: Vec<std::path::PathBuf>,
}

/// Words of a goal's first line that say how, not what: "open a session at …".
const ASKING: &[&str] = &[
    "open", "opens", "start", "starts", "make", "create", "spin", "up", "launch", "session", "sessions", "at", "into", "please", "this", "is",
    "these", "are", "here", "ticket", "task", "issue", "repo", "repos", "repository", "repositories", "project", "projects", "both", "two", "work",
];

/// A line that asks to open sessions: "open a session at eyzflow and
/// mediaroom", "start eyzflow and mediaroom", "spin up sessions in …".
fn asks_for_sessions(line: &str) -> bool {
    line.split(|c: char| !c.is_alphanumeric()).any(|w| ["open", "opens", "start", "starts", "launch", "spin"].contains(&w.to_lowercase().as_str()))
}

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
    // Only a line that asks to open sessions names repos: a ticket's title
    // ("EYZFlow Support – Enable …") names the product, not what to open.
    if !asks_for_sessions(asking_line(goal)) {
        return vec![];
    }
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

/// The words of `line` that say what, not how: without the small words,
/// the words that ask for sessions and (with `drop`) the repos named.
fn telling(line: &str, drop: &[String], take: usize) -> Vec<String> {
    let plain: String = line.chars().filter(|c| !matches!(c, '\'' | '’' | '`')).collect();
    plain
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !FILLER.contains(&w.as_str()) && !ASKING.contains(&w.as_str()) && !drop.iter().any(|r| r.eq_ignore_ascii_case(w)))
        .take(take)
        .collect()
}

/// The line a goal is about: its first line; or, when that only asks for
/// sessions ("open a session at eyzflow and mediaroom. this is the
/// ticket:"), the ticket's title after it (what follows its colon, else the
/// next line with words), and `true`.
pub fn title_line<'a>(goal: &'a str, repos: &[String]) -> (&'a str, bool) {
    let goal = goal.trim_start();
    let first = goal.lines().next().unwrap_or("");
    if !telling(asking_line(goal), repos, 1).is_empty() {
        return (first.trim(), false);
    }
    let rest = match first.split_once(':') {
        Some((_, after)) if !telling(after, &[], 1).is_empty() => after,
        _ => goal.lines().skip(1).find(|l| !telling(l, &[], 1).is_empty()).unwrap_or(""),
    };
    if rest.trim().is_empty() { (first.trim(), false) } else { (rest.trim(), true) }
}

/// `space_name_of`, and whether the name came from the line after the first
/// (the first only asked for sessions).
fn space_name_why(goal: &str, repos: &[String]) -> (String, bool) {
    match title_line(goal, repos) {
        (line, true) => match title_words(line) {
            t if t.is_empty() => (telling(line, &[], 2).join("-"), true),
            t => (t.join("-"), true),
        },
        (_, false) => match telling(asking_line(goal), repos, 2) {
            t if t.is_empty() => (space_name(goal), false),
            t => (t.join("-"), false),
        },
    }
}

/// Verbs a ticket's title opens with that say nothing of what it is about.
const TITLE_VERBS: &[&str] = &["enable", "add", "implement", "build", "create", "introduce", "allow", "provide", "support", "improve", "update", "make", "set", "setup", "let"];

/// The two words that name a ticket's title: after the product it opens
/// with ("EYZFlow Support – …", "Billing: …") and a leading verb ("Enable
/// …"), its first and last telling words ("Enable Automatic Message and
/// Reply Translation": automatic, translation); a plain title keeps its
/// first two ("Fix the login redirect": fix, login).
fn title_words(title: &str) -> Vec<String> {
    let title = title.trim();
    let after = [" \u{2013} ", " \u{2014} ", " - ", ": "].iter().find_map(|d| title.split_once(d).map(|(_, r)| r.trim())).filter(|r| r.split_whitespace().count() >= 2).unwrap_or(title);
    let mut words: Vec<String> = after
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !FILLER.contains(&w.as_str()) && !ASKING.contains(&w.as_str()))
        .collect();
    let trimmed = after.len() < title.len() || words.first().is_some_and(|w| TITLE_VERBS.contains(&w.as_str()));
    if words.len() > 1 && TITLE_VERBS.contains(&words[0].as_str()) {
        words.remove(0);
    }
    match words.len() {
        0..=2 => words,
        _ if trimmed => vec![words[0].clone(), words[words.len() - 1].clone()],
        _ => words.into_iter().take(2).collect(),
    }
}

/// The longest a space's goal is (`--title`, a ticket's title), in characters.
pub const TITLE_CHARS: usize = 120;
/// The longest a done-when is.
pub const DONE_WHEN_CHARS: usize = 400;

/// `text` cut to `max` characters at a word, with an ellipsis when it was cut.
pub fn clip_words(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max.saturating_sub(1)).collect();
    let at = cut.rfind(char::is_whitespace).filter(|&i| i > max / 2).unwrap_or(cut.len());
    format!("{}…", cut[..at].trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-' | '–' | '—')))
}

/// A title as a ticket writes it, as a line: no heading marks (`#`, `**`),
/// no `Title:` in front, one space between words, at most `TITLE_CHARS`.
pub fn title_text(line: &str) -> String {
    let mut t = line.split_whitespace().collect::<Vec<_>>().join(" ");
    t = t.trim_start_matches(|c: char| c == '#' || c == '>' || c.is_whitespace()).to_string();
    t = t.trim_matches(|c: char| c == '*' || c == '_' || c == '`' || c.is_whitespace()).to_string();
    for lead in ["title:", "ticket:", "summary:", "subject:"] {
        if t.len() > lead.len() && t[..lead.len()].eq_ignore_ascii_case(lead) {
            t = t[lead.len()..].trim().trim_matches(|c: char| c == '*' || c == '_').trim().to_string();
        }
    }
    clip_words(&t, TITLE_CHARS)
}

/// The space's goal from a goal or a ticket: its title line (`title_line`)
/// as a line of at most `TITLE_CHARS`.
pub fn title_of(goal: &str, repos: &[String]) -> String {
    title_text(title_line(goal, repos).0)
}

/// What counts as done, from a ticket's "Acceptance criteria", "Definition
/// of done" or "Done when" section: its bullet lines (or, with none, its
/// lines up to the next empty one), joined with "; ", at most
/// `DONE_WHEN_CHARS`. A heading with the words after its colon is those
/// words. "" when the ticket has no such section.
pub fn done_when_of(ticket: &str) -> String {
    const HEADS: &[&str] = &["acceptance criteria", "definition of done", "done when"];
    let plain = |l: &str| l.trim().trim_start_matches(|c: char| matches!(c, '#' | '*' | '_' | '>') || c.is_whitespace()).to_string();
    let bullet = |l: &str| -> Option<String> {
        let t = l.trim();
        let rest = ["- [ ] ", "- [x] ", "- [X] ", "* [ ] ", "[ ] ", "[x] ", "- ", "* ", "• ", "+ ", "– "]
            .iter()
            .find_map(|m| t.strip_prefix(m))
            .or_else(|| {
                let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
                (digits > 0).then(|| t[digits..].strip_prefix(". ").or_else(|| t[digits..].strip_prefix(") "))).flatten()
            })?;
        Some(rest.trim().to_string()).filter(|r| !r.is_empty())
    };
    let heading = |l: &str| {
        let t = l.trim();
        t.starts_with('#') || (t.ends_with(':') && bullet(t).is_none()) || (t.starts_with("**") && t.ends_with("**"))
    };
    let lines: Vec<&str> = ticket.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let p = plain(line);
        let lower = p.to_lowercase();
        let Some(head) = HEADS.iter().find(|h| lower.starts_with(**h)) else { continue };
        let after = &p[head.len()..];
        // "done whenever" is no heading: the words must end there.
        if after.chars().next().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let inline = after.trim_start_matches(|c: char| matches!(c, ':' | '*' | '_' | ')' | '(') || c.is_whitespace()).trim_end_matches(['*', '_']).trim();
        if !inline.is_empty() && !inline.ends_with(':') {
            return clip_words(inline, DONE_WHEN_CHARS);
        }
        let mut items: Vec<String> = vec![];
        let mut plain_lines = true;
        for (j, l) in lines.iter().enumerate().skip(i + 1) {
            let t = l.trim();
            if t.is_empty() {
                // An empty line ends lines that aren't a list; a list goes on after one only as a list.
                if !items.is_empty() && plain_lines {
                    break;
                }
                continue;
            }
            match bullet(t) {
                Some(b) => {
                    if plain_lines && !items.is_empty() {
                        break;
                    }
                    plain_lines = false;
                    items.push(b);
                }
                None if heading(t) => break,
                None if !plain_lines => {
                    // A list's item that goes on to a line of its own; an empty line before it ends the list.
                    if lines[j - 1].trim().is_empty() {
                        break;
                    }
                    if let Some(last) = items.last_mut() {
                        last.push(' ');
                        last.push_str(t);
                    }
                }
                None => items.push(t.to_string()),
            }
        }
        let joined = items.iter().map(|x| x.trim().trim_end_matches(['.', ';']).trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>().join("; ");
        if !joined.is_empty() {
            return clip_words(&joined, DONE_WHEN_CHARS);
        }
    }
    String::new()
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

/// What a launch would do (`plan_with`, asked only for a project and a name).
pub fn plan(goal: &str, setup: Option<&str>, name: Option<&str>, w: &World) -> Result<Plan> {
    plan_with(goal, &Ask { setup, name, ..Default::default() }, w)
}

/// What a launch would do, asked `ask`.
pub fn plan_with(goal: &str, ask: &Ask, w: &World) -> Result<Plan> {
    // Each line's spaces made one, the lines kept (a pasted ticket keeps its paragraphs and lists).
    let ticket = tidy(goal);
    if ticket.is_empty() {
        bail!("tower launch \"<the goal, in a sentence>\" [--setup <name>] [--name <space>] [--dry-run]");
    }
    let given = |v: Option<&str>| v.map(str::trim).filter(|v| !v.is_empty()).map(String::from);
    // The repos: those `--repo` gives (a confirmed draft); else a setup that
    // was named; else those the goal's first line names, on the default
    // account; else a saved setup.
    let named = if ask.repos.is_empty() { repos_named(&ticket, &w.projects) } else { vec![] };
    let same = |a: &str, b: &Path| Path::new(crate::dispatch::expand(a.trim()).trim_end_matches('/')) == b;
    let (setup, setup_why, repos_from): (Setup, String, &'static str) = match given(ask.setup) {
        _ if !ask.repos.is_empty() => {
            let mut repos: Vec<String> = vec![];
            for r in ask.repos.iter().map(|r| r.trim()).filter(|r| !r.is_empty()) {
                let folder = crate::dispatch::expand(r).trim_end_matches('/').to_string();
                if !Path::new(&folder).is_dir() {
                    bail!("--repo {r}: no such folder here");
                }
                if !repos.contains(&folder) {
                    repos.push(folder);
                }
            }
            if repos.is_empty() {
                bail!("--repo: no folder given");
            }
            match given(ask.setup) {
                // The project it is for keeps its name, account and roles; the repos are those asked for.
                Some(asked) => {
                    let (s, _) = crate::setups::pick(&w.setups, Some(&asked))?;
                    (Setup { repos, ..s.clone() }, "the project named, with the repos asked for".to_string(), "repos")
                }
                None => (Setup { name: "the repos asked for".into(), repos, ..Default::default() }, "the repos asked for".to_string(), "repos"),
            }
        }
        Some(asked) => {
            let (s, why) = crate::setups::pick(&w.setups, Some(&asked))?;
            (s.clone(), why, "setup")
        }
        None if !named.is_empty() => {
            // A saved setup that has every repo the goal names keeps its account.
            match w.setups.iter().find(|s| named.iter().all(|(_, f)| s.repos.iter().any(|r| same(r, f)))) {
                Some(s) => {
                    let repos = named.iter().map(|(_, f)| s.repos.iter().find(|r| same(r, f)).cloned().unwrap_or_else(|| f.to_string_lossy().into_owned())).collect();
                    (Setup { repos, ..s.clone() }, "the one that has the repos the goal names".to_string(), "first-line")
                }
                None => {
                    let words: Vec<&str> = named.iter().map(|(w, _)| w.as_str()).collect();
                    (
                        Setup { name: format!("the goal's {}", words.join(" and ")), repos: named.iter().map(|(_, f)| f.to_string_lossy().into_owned()).collect(), ..Default::default() },
                        "the repos its first line names".to_string(),
                        "first-line",
                    )
                }
            }
        }
        None if w.setups.is_empty() => bail!(
            "name the repos in the goal's first line (\"open sessions in eyzflow and mediaroom\"): no project folder fits the words of \"{}\". Or save a setup: tower setup add \"<name>\" --account <account> --repo <folder>",
            asking_line(&ticket).trim()
        ),
        None => {
            let (s, why) = crate::setups::pick(&w.setups, None)?;
            (s.clone(), why, "setup")
        }
    };
    let from_goal = repos_from == "first-line" && setup_why == "the repos its first line names";
    let setup = &setup;
    if setup.repos.is_empty() {
        bail!("the setup \"{}\" has no repos: tower setup add \"{}\" --account <account> --repo <folder>", setup.name, setup.name);
    }
    let repo_words: Vec<String> = named.iter().map(|(w, _)| w.clone()).collect();
    let (space, name_why, joins) = match given(ask.name) {
        Some(n) => {
            let there = w.open.iter().find(|s| s.eq_ignore_ascii_case(&n));
            (there.cloned().unwrap_or_else(|| n.to_string()), "as given".to_string(), there.is_some())
        }
        None => {
            let (base, from_next) = space_name_why(&ticket, &repo_words);
            let base = if base.is_empty() { "space".to_string() } else { base };
            let name = unique(&base, &w.spaces);
            let rule = if from_next { NAME_NEXT_WHY } else { NAME_WHY };
            let why = if name == base { rule.to_string() } else { format!("{rule}; \"{base}\" is taken") };
            (name, why, false)
        }
    };
    // The goal is a line: the title given, else the ticket's own. What counts
    // as done: as given (even ""), else its acceptance criteria.
    let goal = match given(ask.title) {
        Some(t) => title_text(&t),
        None => title_of(&ticket, &repo_words),
    };
    let done_when = match ask.done_when {
        Some(d) => clip_words(&d.split_whitespace().collect::<Vec<_>>().join(" "), DONE_WHEN_CHARS),
        None => done_when_of(&ticket),
    };
    // The account as the mod knows it; none known: plain `claude`, and it says so.
    let want = given(ask.account).or_else(|| given(Some(&setup.account)));
    let (account, account_why) = match crate::accounts::resolve(want.as_deref(), &w.accounts) {
        Ok(a) if given(ask.account).is_some() => (a, "as given".to_string()),
        Ok(Some(a)) if setup.account.trim().is_empty() && from_goal => (Some(a), "the default one: the goal names none".to_string()),
        Ok(Some(a)) if setup.account.trim().is_empty() && repos_from == "repos" => (Some(a), "the default one: none was given".to_string()),
        Ok(Some(a)) if setup.account.trim().is_empty() => (Some(a), "the default one: the setup names none".to_string()),
        Ok(a) => (a, "the setup's".to_string()),
        Err(_) if w.accounts.is_empty() => (None, format!("the setup's is \"{}\", but no accounts are known (cca isn't installed): plain claude", want.unwrap_or_default())),
        Err(e) => return Err(e),
    };
    let standing = w.standing.iter().any(|s| s.eq_ignore_ascii_case(&space));
    let review = ask.review.unwrap_or_else(|| !w.no_review.iter().any(|s| s.eq_ignore_ascii_case(&space)));
    let mut taken: BTreeSet<String> = w.names.iter().cloned().collect();
    let mut sessions = vec![];
    for repo in &setup.repos {
        let folder = crate::dispatch::expand(repo.trim());
        if !Path::new(&folder).is_dir() {
            bail!("the setup \"{}\" names {repo}, which isn't a folder here", setup.name);
        }
        let worktree = (w.worktrees && !standing).then(|| worktrees::toplevel(Path::new(&folder))).flatten().map(|top| worktrees::place(&top, &space));
        // Why it is in it: its role in the project, else how it came in.
        let why = match (setup.role_of(repo), repos_from) {
            ("", "first-line") => "the goal's first line names it".to_string(),
            ("", "repos") => "asked for".to_string(),
            ("", _) => format!("a repo of {}", setup.name),
            (role, _) => role.to_string(),
        };
        sessions.push(Session { name: session_name(&space, &folder, &mut taken), repo: repo.clone(), folder, worktree, why });
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
    Ok(Plan { goal, done_when, ticket, repos_from, space, joins, setup: setup.clone(), account, sessions, assumed, plan: None, review })
}

/// What a session of the plan is asked first: the whole ticket (with its
/// title in front when the ticket doesn't have it), and its part in it.
pub fn instruction(p: &Plan, me: &Session) -> String {
    let others: Vec<String> = p.sessions.iter().filter(|s| s.name != me.name).map(|s| format!("{} ({})", s.name, repo_name(&s.folder))).collect();
    let with = match others.as_slice() {
        [] => String::new(),
        list => format!(" The space's other sessions each have their own repository: {}. The tower introduces you to each other; work the seams out with them.", list.join(", ")),
    };
    let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let text = if p.goal.is_empty() || squash(&p.ticket).contains(&squash(p.goal.trim_end_matches('…'))) { p.ticket.clone() } else { format!("{}\n\n{}", p.goal, p.ticket) };
    let done = if p.done_when.is_empty() { String::new() } else { format!(" It is done when: {}.", p.done_when.trim_end_matches('.')) };
    // The plan it was planned with: its link, and this session's part of it.
    let plan = match &p.plan {
        Some(b) => {
            let others: Vec<String> = p.sessions.iter().filter(|s| s.name != me.name).map(|s| repo_name(&s.folder)).collect();
            format!("\n\n{}", crate::plans::brief_text(b, &repo_name(&me.folder), &others))
        }
        None => String::new(),
    };
    format!(
        "{text}\n\nThat is the goal of the space \"{}\".{done} You are its {} session: do the part of it that belongs in this repository.{with} When your part is done, say what you did and what is left.{plan}",
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
                "why": s.why,
            })
        })
        .collect();
    json!({
        "space": p.space,
        "goal": p.goal,
        "done_when": p.done_when,
        "made": !p.joins,
        "dry_run": dry_run,
        "agenda": agenda,
        "sessions": sessions,
        "repos_from": p.repos_from,
        "assumed": p.assumed,
        "plan": p.plan.as_ref().map(|b| json!({ "id": b.id, "url": b.url })),
        "review": p.review,
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
    if let Some(d) = r["done_when"].as_str().filter(|d| !d.is_empty()) {
        out += &format!("Done when: {d}\n");
    }
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
    if r["review"] == false {
        out += "Its PRs get no reviewer (--no-review).\n";
    } else if r["review"] == true {
        out += "Each PR its sessions open gets a reviewer session, for critical issues only (--no-review: none).\n";
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
        no_review: kept.missions.iter().filter(|(_, m)| !m.review).map(|(n, _)| n.clone()).collect(),
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
    // Its goal is the title, and what counts as done is set as `tower space goal --done-when` sets it.
    crate::spaces::update(|s| {
        s.agendas.insert(p.space.clone(), p.goal.clone());
        let m = s.missions.entry(p.space.clone()).or_insert_with(|| crate::missions::Mission::born(now));
        if m.state == crate::missions::State::Closed || m.archived_ms.is_some() {
            m.reopen(now);
        }
        (m.born_ms, m.goal_ms) = (now, now);
        m.setup = p.setup.name.clone();
        if !p.done_when.is_empty() {
            m.done_when = p.done_when.clone();
        }
        // Whether its PRs get a reviewer: as asked, else as it was (on for a new space).
        m.review = p.review;
    });
    // One request: the whole ticket, under its title; each session's first task joins it.
    let agenda = match crate::protocol::is_running() {
        true => {
            let cmd = crate::agenda::Cmd { action: "open".into(), text: p.ticket.clone(), title: crate::agenda::clip(&p.goal, 80), spaces: vec![p.space.clone()], ..Default::default() };
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

/// What there is now, for a plan (`world`), for the draft of a ticket.
pub fn world_now() -> Result<World> {
    world()
}

/// What a launch did (or, with `dry_run`, would do): its receipt.
pub fn launch(goal: &str, ask: &Ask, dry_run: bool) -> Result<Value> {
    let mut p = plan_with(goal, ask, &world()?)?;
    if let Some(id) = ask.plan.map(str::trim).filter(|i| !i.is_empty()) {
        p.plan = Some(crate::plans::brief_of(id)?);
    }
    if dry_run {
        return Ok(receipt(&p, &[], None, true));
    }
    let mut r = open(&p, None, &mut |_| ())?;
    // The conversation that made it is the space's own from now on (only once something opened).
    if let Some(path) = ask.story.map(str::trim).filter(|s| !s.is_empty()) {
        let opened = r["sessions"].as_array().is_some_and(|a| a.iter().any(|s| s["error"].is_null()));
        let story: Vec<Told> = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("tower: the story in {path} isn't one ({e}): not kept");
                vec![]
            }),
            Err(e) => {
                eprintln!("tower: no story in {path}: {e}");
                vec![]
            }
        };
        r["story"] = json!(if opened { record_story(&p.space, &story, &r) } else { 0 });
    }
    // Planned and launched: the plan is done, and its planner ends.
    if let Some(b) = &p.plan
        && crate::protocol::is_running()
    {
        let close = crate::plans::Args { action: "close".into(), id: Some(b.id.clone()), launched: Some(p.space.clone()), ..Default::default() };
        if let Err(e) = crate::plans::request(&close) {
            eprintln!("tower: the plan {} stays open: {e:#}", b.id);
        }
    }
    Ok(r)
}

/// `tower launch "<goal>" [--setup <name>] [--name <space>] [--repo <folder>…]
/// [--title "<goal>"] [--done-when "<…>"] [--account <a>] [--dry-run] [--json]`.
pub fn cmd(goal: &str, ask: &Ask, dry_run: bool, as_json: bool) -> Result<()> {
    let r = launch(goal, ask, dry_run)?;
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
        Setup { name: name.into(), account: account.into(), repos: repos.iter().map(|r| r.to_string()).collect(), ..Default::default() }
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
        assert_eq!(p.space, "automatic-translation", "named from the ticket's title, past its product and verb, not from \"open a session\"");
        // A ticket's title names the product, not what to open: no repos are read from it (it is drafted).
        let titled = "EYZFlow Support \u{2013} Enable Automatic Message and Reply Translation\n\nDescription: we need translation";
        assert!(repos_named(titled, &w.projects).is_empty());
        assert_eq!(title_words("Billing: add CSV export for invoices"), ["csv", "invoices"]);
        assert_eq!(title_words("Fix the login redirect"), ["fix", "login"]);
        assert_eq!(p.account.as_deref(), Some("default"));
        assert_eq!(p.assumed["setup_why"], "the repos its first line names");
        assert_eq!(p.assumed["account_why"], "the default one: the goal names none");
        assert!(p.assumed["name_why"].as_str().unwrap().contains("the ticket's title"));
        // The ticket keeps its paragraphs and list for the sessions; spaces inside a line are one.
        assert!(p.ticket.contains("languages. Support answers in German.\n\n- Detect the language\n- Translate"), "{}", p.ticket);
        // The space's goal is the ticket's title, a line; it has no acceptance criteria, so no done-when.
        assert_eq!((p.goal.as_str(), p.done_when.as_str(), p.repos_from), ("EYZFlow Support \u{2013} Enable Automatic Message and Reply Translation", "", "first-line"));
        assert_eq!(p.sessions.iter().map(|x| x.why.as_str()).collect::<Vec<_>>(), ["the goal's first line names it"; 2]);
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
            no_review: vec![],
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
                "why": "a repo of eyzflow + mediaroom on orca",
            })
        );
        assert_eq!(r["assumed"], p.assumed);
        assert_eq!((r["done_when"].as_str(), r["repos_from"].as_str()), (Some(""), Some("setup")));
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

    #[test]
    fn a_tickets_title_is_its_goal_and_its_acceptance_criteria_say_when_it_is_done() {
        let repos = ["eyzflow".to_string(), "mediaroom".to_string()];
        // The line after the one that asks for sessions; what follows a colon on it; else the first line.
        assert_eq!(title_of("open a session at eyzflow and mediaroom. this is the ticket:\nEYZFlow Support – Translation\n\nBody", &repos), "EYZFlow Support – Translation");
        assert_eq!(title_of("open sessions in eyzflow and mediaroom: Translate support messages\nBody", &repos), "Translate support messages");
        assert_eq!(title_of("Fix the login redirect in eyzflow\n\nIt loops.", &repos), "Fix the login redirect in eyzflow");
        assert_eq!(title_of("Here is the ticket:\n\n## **Title:** Translate the inbox\n", &[]), "Translate the inbox", "marks and a Title: taken off");
        assert_eq!(title_of("open a session at eyzflow", &repos), "open a session at eyzflow", "nothing after it: the line itself");
        let long = title_of(&format!("Translate {}", "everything ".repeat(30)), &[]);
        assert!(long.chars().count() <= TITLE_CHARS && long.ends_with('…'), "{long}");
        // Done when: the section's bullets, joined.
        let ticket = "Translate support messages\n\nCustomers write in many languages.\n\n### Acceptance Criteria\n\n- [ ] Incoming messages show translated.\n- Replies go out in the customer's language;\n  and the original stays visible\n\n## Notes\n- not this";
        assert_eq!(done_when_of(&tidy(ticket)), "Incoming messages show translated; Replies go out in the customer's language; and the original stays visible");
        assert_eq!(done_when_of("**Definition of Done**\n1. Tests pass\n2) Deployed to staging\n\nOther text"), "Tests pass; Deployed to staging");
        assert_eq!(done_when_of("Title\n\nDone when: the inbox shows both languages."), "the inbox shows both languages.");
        assert_eq!(done_when_of("Acceptance criteria:\nThe inbox shows both.\nReplies are translated.\n\nMore"), "The inbox shows both; Replies are translated");
        assert_eq!(done_when_of("Acceptance criteria:\n* one\n\n* two\nNotes:\n- three"), "one; two", "a list goes on after an empty line, and stops at a heading");
        assert_eq!(done_when_of("We are done whenever it ships.\n- a"), "", "no such section");
        assert_eq!(done_when_of("Translate it"), "");
        assert!(done_when_of(&format!("Done when:\n- {}", "x ".repeat(300))).chars().count() <= DONE_WHEN_CHARS);
        assert_eq!(clip_words("short", 10), "short");
        assert_eq!(clip_words("one two three four", 12), "one two…");
    }

    #[test]
    fn a_confirmed_draft_opens_exactly_its_repos_with_its_name_goal_and_done_when() {
        let base = temp("launch-confirmed");
        let (flow, room) = (repo(&base, "eyzflow"), repo(&base, "eyz-mediaroom-serverless"));
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut project = setup("EYZFlow", "orca", &[&s(&flow), &s(&room)]);
        project.roles.insert(s(&room), "the media backend".into());
        let w = World { setups: vec![project, setup("other", "default", &["/w"])], accounts: accounts(&["default", "orca"]), worktrees: true, spaces: vec!["msg".into()], ..Default::default() };
        let ticket = "EYZFlow Support – Enable Automatic Message and Reply Translation\n\nAcceptance criteria:\n- Messages show translated";
        let repos = vec![s(&room)];
        let ask = Ask { repos: &repos, name: Some("msg-translation"), title: Some("Translate support messages"), done_when: Some("both languages show"), account: None, setup: Some("EYZFlow"), plan: None, review: None, story: None };
        let p = plan_with(ticket, &ask, &w).unwrap();
        // Only the repo asked for, though the project has two; its account and its role come with the project.
        assert_eq!((p.sessions.len(), p.sessions[0].folder.as_str(), p.sessions[0].why.as_str()), (1, s(&room).as_str(), "the media backend"));
        assert_eq!((p.space.as_str(), p.goal.as_str(), p.done_when.as_str(), p.account.as_deref(), p.repos_from), ("msg-translation", "Translate support messages", "both languages show", Some("orca"), "repos"));
        assert_eq!((p.setup.name.as_str(), p.assumed["setup_why"].as_str(), p.assumed["name_why"].as_str()), ("EYZFlow", Some("the project named, with the repos asked for"), Some("as given")));
        // The sessions get the whole ticket, the title in front when the ticket doesn't have it, and what counts as done.
        let first = instruction(&p, &p.sessions[0]);
        assert!(first.starts_with("Translate support messages\n\nEYZFlow Support – Enable Automatic Message and Reply Translation\n\nAcceptance criteria:"), "{first}");
        assert!(first.contains("That is the goal of the space \"msg-translation\". It is done when: both languages show. You are its eyz-mediaroom-serverless session"), "{first}");
        let r = receipt(&p, &[], None, true);
        assert_eq!((r["goal"].as_str(), r["done_when"].as_str(), r["sessions"][0]["why"].as_str()), (Some("Translate support messages"), Some("both languages show"), Some("the media backend")));
        assert!(r["plan"].is_null() && !first.contains("The space's plan"), "no plan: none said");
        // Planned first (`--plan`): each session's instruction carries the plan's link and its part of it, after its brief.
        let two = vec![s(&flow), s(&room)];
        let mut p2 = plan_with(ticket, &Ask { repos: &two, name: Some("msg-translation"), setup: Some("EYZFlow"), ..Default::default() }, &w).unwrap();
        let md = format!("# Plan\n\n## Sessions\n\n### eyzflow\n\nThe web app's part.\n{}\n### eyz-mediaroom-serverless\n\nThe media backend's part.\n", "More of the web app.\n".repeat(400));
        p2.plan = Some(crate::plans::PlanBrief { id: "p3".into(), url: Some("https://claude.ai/artifact/plan".into()), md, ..Default::default() });
        let room_first = instruction(&p2, &p2.sessions[1]);
        let (brief, part) = room_first.split_once("\n\nThe space's plan (https://claude.ai/artifact/plan)").expect("the plan follows the brief");
        assert!(brief.ends_with("say what you did and what is left."), "{brief}");
        assert!(part.contains("The media backend's part.") && !part.contains("The web app's part."), "its own part only: {part}");
        assert!(part.chars().count() <= crate::plans::BRIEF_CHARS + 300, "bounded");
        assert!(instruction(&p2, &p2.sessions[0]).contains("The web app's part."));
        assert_eq!(receipt(&p2, &[], None, true)["plan"], serde_json::json!({ "id": "p3", "url": "https://claude.ai/artifact/plan" }));
        // No title, no done-when: the ticket's own; an account given wins; no project: none.
        let ask = Ask { repos: &repos, account: Some("default"), ..Default::default() };
        let p = plan_with(ticket, &ask, &w).unwrap();
        assert_eq!((p.goal.as_str(), p.done_when.as_str()), ("EYZFlow Support – Enable Automatic Message and Reply Translation", "Messages show translated"));
        assert_eq!((p.account.as_deref(), p.assumed["account_why"].as_str(), p.setup.name.as_str(), p.sessions[0].why.as_str()), (Some("default"), Some("as given"), "the repos asked for", "asked for"));
        assert_eq!(p.space, "eyzflow-support", "named from the ticket");
        assert!(instruction(&p, &p.sessions[0]).starts_with("EYZFlow Support – Enable"), "the ticket has its title: not twice");
        // An empty done-when is kept empty, not filled from the ticket.
        let ask = Ask { repos: &repos, done_when: Some(""), ..Default::default() };
        assert_eq!(plan_with(ticket, &ask, &w).unwrap().done_when, "");
        // Its PRs get a reviewer: on unless --no-review, or the space was turned off before.
        let p = plan_with(ticket, &Ask { repos: &repos, ..Default::default() }, &w).unwrap();
        assert!(p.review && receipt(&p, &[], None, true)["review"] == true);
        assert!(said(&receipt(&p, &[], None, true)).contains("Each PR its sessions open gets a reviewer session"));
        let off = plan_with(ticket, &Ask { repos: &repos, review: Some(false), ..Default::default() }, &w).unwrap();
        assert!(!off.review && receipt(&off, &[], None, true)["review"] == false);
        assert!(said(&receipt(&off, &[], None, true)).contains("Its PRs get no reviewer (--no-review)."));
        let kept_off = World { no_review: vec!["Msg-Translation".into()], ..w.clone() };
        let ask = Ask { repos: &repos, name: Some("msg-translation"), ..Default::default() };
        assert!(!plan_with(ticket, &ask, &kept_off).unwrap().review, "as the space has it");
        assert!(plan_with(ticket, &Ask { review: Some(true), ..ask }, &kept_off).unwrap().review, "--review turns it on");
        // A folder that isn't there, an unknown project or account: an error, nothing guessed.
        let gone = vec!["/no/such/folder".to_string()];
        assert!(plan_with(ticket, &Ask { repos: &gone, ..Default::default() }, &w).unwrap_err().to_string().contains("no such folder"));
        assert!(plan_with(ticket, &Ask { repos: &repos, setup: Some("nobody"), ..Default::default() }, &w).is_err());
        assert!(plan_with(ticket, &Ask { repos: &repos, account: Some("nobody"), ..Default::default() }, &w).unwrap_err().to_string().contains("no Claude account"));
        // The goal's first line naming a project's repos opens those, not the rest of the project.
        let named = World { projects: vec![flow.clone(), room.clone()], ..w.clone() };
        let p = plan("open a session at mediaroom: Translate it\n\nDone when: it is translated", None, None, &named).unwrap();
        assert_eq!((p.sessions.len(), p.account.as_deref(), p.goal.as_str(), p.done_when.as_str()), (1, Some("orca"), "Translate it", "it is translated"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The conversation that made a space is kept as that space's own: a
    /// `creation` entry per message, who said it, when, written down only;
    /// and what the launch opened as the tower's last word.
    #[test]
    fn the_conversation_that_made_a_space_is_kept_as_its_own() {
        let receipt = json!({ "space": "ns-check", "made": true, "sessions": [{ "repo": "/w/tower-scratch", "worktree": "/w/.worktrees/ns-check/tower-scratch", "error": null }], "assumed": { "account": "default" } });
        let story = vec![
            Told { who: "you".into(), text: " the ticket \n line two ".into(), at: 100 },
            Told { who: "tower".into(), text: "Drafted scratch-check".into(), at: 200 },
            Told { who: "planner".into(), text: "One per repo.".into(), at: 250 },
            Told { who: "you".into(), text: "  ".into(), at: 260 },
            Told { who: "someone".into(), text: "From later".into(), at: 9_999 },
        ];
        let e = story_entries("ns-check", &story, &receipt, 1_000);
        let rows: Vec<(&str, &str, &str, u64)> = e.iter().map(|e| (e.kind.as_str(), e.status.as_deref().unwrap_or(""), e.text.as_str(), e.at)).collect();
        assert_eq!(
            rows,
            [
                ("creation", "you", "the ticket \n line two", 100),
                ("creation", "tower", "Drafted scratch-check", 200),
                ("creation", "planner", "One per repo.", 250),
                ("creation", "tower", "From later", 1_000),
                ("creation", "tower", "Launched ns-check: 1 session on default (tower-scratch), in its own worktree.", 1_000),
            ]
        );
        assert!(e.iter().all(|e| e.record && e.space.as_deref() == Some("ns-check") && !crate::chat::shown(e)), "the space's, written down, never a chat message");
        assert!(story_entries("ns-check", &[], &receipt, 1_000).is_empty(), "nothing said: nothing kept, not even the launch");
        // `tower launch --story-file` takes it, and not with a draft or a dry run.
        use clap::Parser;
        let parse = |args: &[&str]| crate::Cli::try_parse_from([&["tower", "launch", "Fix the login"], args].concat()).map(|c| c.cmd);
        assert!(matches!(parse(&["--story-file", "/tmp/s.json"]), Ok(crate::Cmd::Launch { story_file: Some(ref f), .. }) if f == "/tmp/s.json"));
        assert!(parse(&["--story-file", "/tmp/s.json", "--draft"]).is_err());
        assert!(parse(&["--story-file", "/tmp/s.json", "--dry-run"]).is_err());
    }
}
