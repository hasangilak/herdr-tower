//! Herdr spaces in the tower, and focus.
//!
//! The user works in several spaces at once ("B2B ticketing", "B2C
//! support"), each with its own sessions. With a space in focus, its news is
//! spoken as always; news from every other space is held: not spoken, not
//! handed to the brain, kept as unread for the panel and for "what did I
//! miss". Only someone needing the user gets through, as a bell and a short
//! "B2C support needs you" (`alerts.held_urgent`). No focus: everything is
//! spoken, as before.
//!
//! `spaces.json` (beside `state.json`, which the orb watches) keeps the
//! focus, each space's agenda, and how far its held news has been read.

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config;
use crate::history::{self, Entry};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Spaces {
    /// The space whose news is spoken; None: every space's.
    pub focus: Option<String>,
    /// What each space is for, set by the user or the brain.
    pub agendas: BTreeMap<String, String>,
    /// Held news up to this time (Unix ms) has been delivered.
    pub read_ms: BTreeMap<String, u64>,
    /// Passed over in a catch-up at this time; it comes round again with newer news.
    pub skipped_ms: BTreeMap<String, u64>,
}

pub fn path_in(ui_dir: &Path) -> PathBuf {
    ui_dir.join("spaces.json")
}

pub fn load_from(ui_dir: &Path) -> Spaces {
    std::fs::read_to_string(path_in(ui_dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn load() -> Spaces {
    load_from(&config::ui_dir())
}

pub fn save_to(ui_dir: &Path, s: &Spaces) {
    let _ = std::fs::create_dir_all(ui_dir);
    // Written beside the watched folder, then moved in: the orb wakes once.
    let tmp = ui_dir.parent().unwrap_or(ui_dir).join(format!(".spaces.json.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path_in(ui_dir));
    }
}

pub fn save(s: &Spaces) {
    save_to(&config::ui_dir(), s)
}

/// What to do with news from a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Say it, as always.
    Speak,
    /// Keep it for later: unread, not spoken.
    Hold,
    /// Keep it, but a bell and "<space> needs you" now.
    Urgent,
}

/// News from `space` (None: a session in no space), with `focus` set or not.
/// `needs_you`: an agent is blocked on the user. `urgent_cue`: such news
/// breaks through the focus (config `alerts.held_urgent`).
pub fn gate(space: Option<&str>, focus: Option<&str>, needs_you: bool, urgent_cue: bool) -> Gate {
    match (focus, space) {
        (None, _) | (_, None) => Gate::Speak,
        (Some(f), Some(s)) if f == s => Gate::Speak,
        _ if needs_you && urgent_cue => Gate::Urgent,
        _ => Gate::Hold,
    }
}

/// The space a terminal is in: Herdr's `where` label reads "<space> / <tab>".
pub fn space_of(t: &Value) -> Option<String> {
    t["labels"]["where"].as_str().and_then(|w| w.split(" / ").next()).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Keeps news from an unfocused space.
pub fn hold(kind: &str, session: &str, name: &str, space: &str, text: &str, status: Option<&str>) {
    history::append(Entry {
        kind: kind.into(),
        session: Some(session.into()),
        name: Some(name.into()).filter(|n: &String| !n.is_empty()),
        space: Some(space.into()),
        status: status.map(String::from),
        text: text.into(),
        held: true,
        ..Default::default()
    });
}

/// Held news not yet delivered, by space, oldest first.
pub fn unread<'a>(entries: &'a [Entry], s: &Spaces) -> BTreeMap<String, Vec<&'a Entry>> {
    let mut out: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.held) {
        let Some(space) = e.space.as_ref() else { continue };
        if e.at > s.read_ms.get(space).copied().unwrap_or(0) {
            out.entry(space.clone()).or_default().push(e);
        }
    }
    out
}

/// How pressing a space's news is: someone needs the user (0), something
/// went wrong (1), work finished (2), anything else (3).
fn urgency(items: &[&Entry]) -> u8 {
    items
        .iter()
        .map(|e| match e.status.as_deref() {
            Some("blocked") => 0,
            Some("error" | "failed" | "exited") => 1,
            Some("done" | "idle") => 2,
            _ => 3,
        })
        .min()
        .unwrap_or(3)
}

/// The order to catch up in: whoever needs the user first, then trouble,
/// then finished work, the most recent first within each. A space passed
/// over waits until it has newer news.
pub fn digest_order(unread: &BTreeMap<String, Vec<&Entry>>, s: &Spaces) -> Vec<String> {
    let mut spaces: Vec<(u8, std::cmp::Reverse<u64>, String)> = unread
        .iter()
        .filter_map(|(space, items)| {
            let latest = items.iter().map(|e| e.at).max()?;
            if s.skipped_ms.get(space).is_some_and(|&k| k >= latest) {
                return None;
            }
            Some((urgency(items), std::cmp::Reverse(latest), space.clone()))
        })
        .collect();
    spaces.sort();
    spaces.into_iter().map(|(_, _, s)| s).collect()
}

/// A space by what someone called it: exactly, by its start, or by a part
/// ("b2c" is "B2C support"), ignoring case.
pub fn resolve(want: &str, known: &[String]) -> Option<String> {
    let w = want.trim().to_lowercase();
    if w.is_empty() {
        return None;
    }
    let by = |f: &dyn Fn(&str) -> bool| known.iter().find(|k| f(&k.to_lowercase())).cloned();
    by(&|k| k == w).or_else(|| by(&|k| k.starts_with(&w))).or_else(|| by(&|k| k.contains(&w)))
}

/// A space as Herdr's window has it now.
#[derive(Debug, Clone, Default)]
pub struct Live {
    pub name: String,
    /// (address, name, status) for each terminal in it.
    pub sessions: Vec<(String, String, String)>,
}

/// Herdr's spaces, from `herdr-app ls` (empty when Herdr isn't open).
pub fn live() -> Vec<Live> {
    let Ok(out) = std::process::Command::new("herdr-app").arg("ls").stdin(std::process::Stdio::null()).output() else { return vec![] };
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    v["spaces"]
        .as_array()
        .map(|spaces| {
            spaces
                .iter()
                .map(|s| Live {
                    name: s["name"].as_str().unwrap_or("").to_string(),
                    sessions: s["tabs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|tab| tab["terminals"].as_array().cloned().unwrap_or_default())
                        .map(|t| {
                            (
                                t["terminal"].as_str().unwrap_or("").to_string(),
                                t["name"].as_str().unwrap_or("").to_string(),
                                t["status"].as_str().unwrap_or("").to_string(),
                            )
                        })
                        .collect(),
                })
                .filter(|l| !l.name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Every space the tower knows: Herdr's, and those in the history or with an agenda.
pub fn known(live: &[Live], entries: &[Entry], s: &Spaces) -> Vec<String> {
    let mut names: Vec<String> = live.iter().map(|l| l.name.clone()).collect();
    for n in entries.iter().filter_map(|e| e.space.clone()).chain(s.agendas.keys().cloned()) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

fn pick(want: &str) -> Result<String> {
    let names = known(&live(), &history::load(), &load());
    match resolve(want, &names) {
        Some(n) => Ok(n),
        None => bail!("no space called {want:?} (spaces: {})", names.join(", ")),
    }
}

// ---- `tower focus`, `tower space agenda`, `tower spaces`, `tower digest` ----

pub fn cmd_focus(space: Option<String>, clear: bool) -> Result<()> {
    let mut s = load();
    if clear {
        s.focus = None;
        save(&s);
        println!("No focus: news from every space is spoken.");
        return Ok(());
    }
    let Some(want) = space else {
        match &s.focus {
            Some(f) => println!("Focused on {f}."),
            None => println!("No focus: news from every space is spoken."),
        }
        return Ok(());
    };
    let name = pick(&want)?;
    s.focus = Some(name.clone());
    save(&s);
    let held: usize = unread(&history::load(), &s).iter().filter(|(k, _)| **k != name).map(|(_, v)| v.len()).sum();
    println!(
        "Focused on {name}: its news is spoken, other spaces' is held{}.",
        if held > 0 { format!(" ({held} held so far; `tower digest` to catch up)") } else { String::new() }
    );
    Ok(())
}

pub fn cmd_agenda(space: String, text: String) -> Result<()> {
    let name = pick(&space)?;
    let mut s = load();
    if text.trim().is_empty() {
        s.agendas.remove(&name);
        println!("{name}: agenda cleared.");
    } else {
        s.agendas.insert(name.clone(), text.trim().to_string());
        println!("{name}: {}", text.trim());
    }
    save(&s);
    Ok(())
}

pub fn cmd_spaces() -> Result<()> {
    let s = load();
    let entries = history::load();
    let live = live();
    let unread = unread(&entries, &s);
    let jobs = history::jobs(&entries);
    let names = known(&live, &entries, &s);
    let rows: Vec<Value> = names
        .iter()
        .map(|n| {
            let sessions = live.iter().find(|l| &l.name == n).map(|l| l.sessions.clone()).unwrap_or_default();
            let running = jobs.iter().filter(|j| &j.space == n && j.open()).count();
            let last = entries.iter().rev().find(|e| e.space.as_ref() == Some(n)).map(|e| e.at).unwrap_or(0);
            json!({
                "space": n,
                "focus": s.focus.as_ref() == Some(n),
                "agenda": s.agendas.get(n),
                "unread": unread.get(n).map(|v| v.len()).unwrap_or(0),
                "running_jobs": running,
                "sessions": sessions.iter().map(|(a, name, st)| json!({ "address": a, "name": name, "status": st })).collect::<Vec<_>>(),
                "last_activity_ms": last,
            })
        })
        .collect();
    if std::io::stdout().is_terminal() {
        for r in &rows {
            let busy = r["sessions"].as_array().map(|a| a.iter().filter(|t| t["status"] == "working").count()).unwrap_or(0);
            println!(
                "{} {:<20} {:>2} unread  {} working  {}",
                if r["focus"] == true { "◉" } else { " " },
                r["space"].as_str().unwrap_or(""),
                r["unread"],
                busy,
                r["agenda"].as_str().unwrap_or("")
            );
        }
        if s.focus.is_none() {
            println!("(no focus: every space's news is spoken)");
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&json!({ "focus": s.focus, "spaces": rows }))?);
    }
    Ok(())
}

/// What one space's held news says, for the brain to read out.
fn digest_text(space: &str, items: &[&Entry], s: &Spaces) -> String {
    let mut out = format!("{space}: {} update{}", items.len(), if items.len() == 1 { "" } else { "s" });
    if let Some(a) = s.agendas.get(space) {
        out += &format!(" (agenda: {a})");
    }
    out.push('\n');
    for e in items {
        let who = e.name.clone().or_else(|| e.session.clone()).unwrap_or_default();
        let t = chrono_hm(e.at);
        out += &format!("- {t} {who} ({}): {}\n", e.session.as_deref().unwrap_or(""), e.text);
    }
    out
}

fn chrono_hm(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r fills the struct we own.
    let t = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format!("{:02}:{:02}", t.tm_hour, t.tm_min)
}

/// `tower digest`: one space's held news at a time, the most pressing first;
/// what is delivered is marked read. `--list` only says which spaces have
/// news; `--skip` passes over the next one (or `--space`).
pub fn cmd_digest(list: bool, skip: bool, space: Option<String>, peek: bool) -> Result<()> {
    let mut s = load();
    let entries = history::load();
    let unread = unread(&entries, &s);
    let order = digest_order(&unread, &s);
    if list {
        if order.is_empty() {
            println!("Nothing held: no news waiting from any space.");
        }
        for sp in &order {
            println!("{sp}: {} update{}", unread[sp].len(), if unread[sp].len() == 1 { "" } else { "s" });
        }
        return Ok(());
    }
    let target = match space {
        Some(w) => {
            let names: Vec<String> = unread.keys().cloned().collect();
            match resolve(&w, &names) {
                Some(n) => n,
                None => {
                    println!("No held news from {w}.");
                    return Ok(());
                }
            }
        }
        None => match order.first() {
            Some(n) => n.clone(),
            None => {
                println!("Nothing held: no news waiting from any space.");
                return Ok(());
            }
        },
    };
    let items = unread.get(&target).cloned().unwrap_or_default();
    if skip {
        let latest = items.iter().map(|e| e.at).max().unwrap_or_else(history::now_ms);
        s.skipped_ms.insert(target.clone(), latest);
        save(&s);
        println!("Skipped {target} for now (still unread).");
    } else {
        print!("{}", digest_text(&target, &items, &s));
        if !peek {
            let latest = items.iter().map(|e| e.at).max().unwrap_or_else(history::now_ms);
            s.read_ms.insert(target.clone(), latest);
            save(&s);
        }
    }
    let after = digest_order(&unread, &s);
    match after.iter().find(|n| **n != target) {
        Some(next) => println!("Next: {next} ({} update{}). Ask the user before going on.", unread[next].len(), if unread[next].len() == 1 { "" } else { "s" }),
        None => println!("That was the last space with news."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(at: u64, space: &str, status: Option<&str>, held: bool) -> Entry {
        Entry { at, kind: "watch".into(), space: Some(space.into()), status: status.map(String::from), text: "x".into(), held, ..Default::default() }
    }

    #[test]
    fn focus_holds_other_spaces() {
        // No focus: everything is spoken.
        assert_eq!(gate(Some("B2C support"), None, false, true), Gate::Speak);
        assert_eq!(gate(Some("B2C support"), None, true, true), Gate::Speak);
        // The focused space is spoken; a session in no space too.
        assert_eq!(gate(Some("B2B ticketing"), Some("B2B ticketing"), false, true), Gate::Speak);
        assert_eq!(gate(None, Some("B2B ticketing"), false, true), Gate::Speak);
        // Another space: held, unless someone there needs the user.
        assert_eq!(gate(Some("B2C support"), Some("B2B ticketing"), false, true), Gate::Hold);
        assert_eq!(gate(Some("B2C support"), Some("B2B ticketing"), true, true), Gate::Urgent);
        assert_eq!(gate(Some("B2C support"), Some("B2B ticketing"), true, false), Gate::Hold);
    }

    #[test]
    fn unread_is_held_news_after_the_read_mark() {
        let entries = vec![e(1, "A", None, true), e(2, "A", None, false), e(3, "A", None, true), e(4, "B", None, true)];
        let mut s = Spaces::default();
        s.read_ms.insert("A".into(), 1);
        let u = unread(&entries, &s);
        assert_eq!(u["A"].iter().map(|e| e.at).collect::<Vec<_>>(), vec![3]);
        assert_eq!(u["B"].len(), 1);
    }

    #[test]
    fn digest_goes_by_urgency_then_recency_and_skips_wait() {
        let entries = vec![
            e(10, "Done space", Some("done"), true),
            e(20, "Newer done", Some("done"), true),
            e(5, "Needs you", Some("blocked"), true),
            e(30, "Chatter", None, true),
            e(15, "Broken", Some("error"), true),
        ];
        let mut s = Spaces::default();
        let order = digest_order(&unread(&entries, &s), &s);
        assert_eq!(order, vec!["Needs you", "Broken", "Newer done", "Done space", "Chatter"]);
        // Skipped: waits until it has news newer than the skip.
        s.skipped_ms.insert("Needs you".into(), 5);
        assert_eq!(digest_order(&unread(&entries, &s), &s)[0], "Broken");
        let mut more = entries.clone();
        more.push(e(40, "Needs you", None, true));
        assert_eq!(digest_order(&unread(&more, &s), &s)[0], "Needs you");
    }

    #[test]
    fn names_resolve_loosely() {
        let known = vec!["B2B ticketing".to_string(), "B2C support".to_string(), "M5 Pro".to_string()];
        assert_eq!(resolve("b2c", &known).as_deref(), Some("B2C support"));
        assert_eq!(resolve("B2B Ticketing", &known).as_deref(), Some("B2B ticketing"));
        assert_eq!(resolve("support", &known).as_deref(), Some("B2C support"));
        assert_eq!(resolve("nothing", &known), None);
    }

    #[test]
    fn space_comes_from_herdrs_where_label() {
        assert_eq!(space_of(&json!({ "labels": { "where": "B2C support / bot" } })).as_deref(), Some("B2C support"));
        assert_eq!(space_of(&json!({ "labels": {} })), None);
    }
}
