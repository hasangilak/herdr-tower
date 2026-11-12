//! Projects (still called setups in config.toml and the commands, for the
//! files and phones from before): what a space is launched with. A project
//! is a name, the Claude account its sessions run on, a line on what it is
//! (`about`), the repos (folders) that get a session each, and a line on
//! what each repo is in it (`roles`). `tower launch` opens a space from one,
//! and a ticket's draft (`crate::ticket`) reads them to pick the repos a
//! ticket needs.
//!
//! They are `[[setups]]` tables in config.toml:
//!
//! ```toml
//! [[setups]]
//! name = "EYZFlow"
//! account = "orca"
//! about = "the video platform agencies run their media on"
//! repos = ["~/Work/eyz/eyzflow", "~/Work/eyz/eyz-mediaroom-serverless"]
//! roles = { "~/Work/eyz/eyzflow" = "the web app and its API", "~/Work/eyz/eyz-mediaroom-serverless" = "the media backend" }
//! ```
//!
//! `tower setup add|remove` edits the file's text, so the rest of it (its
//! comments, its order) stays as the user wrote it. A repo is given as
//! `--repo "<folder>=<its role>"` (or `<folder> — <its role>`), the role
//! left out when it has none.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Result, bail};
use serde_json::json;

use crate::config::{self, Setup};

/// The setups in config.toml, in its order.
pub fn list() -> Result<Vec<Setup>> {
    Ok(config::Config::load()?.setups)
}

/// A table header (`[lan]`, `[[setups]]`), not a line of an array.
fn header(line: &str) -> Option<&str> {
    let t = line.split('#').next().unwrap_or("").trim();
    (t.starts_with('[') && t.ends_with(']') && !t.contains('=') && !t.contains(',') && !t.contains('"')).then_some(t)
}

/// The lines of each `[[setups]]` table (from its header up to the next
/// table), with the name it sets.
fn blocks(lines: &[&str]) -> Vec<(usize, usize, String)> {
    let mut out = vec![];
    let mut i = 0;
    while i < lines.len() {
        if header(lines[i]) != Some("[[setups]]") {
            i += 1;
            continue;
        }
        let end = (i + 1..lines.len()).find(|&j| header(lines[j]).is_some()).unwrap_or(lines.len());
        let name = toml::from_str::<Setup>(&lines[i + 1..end].join("\n")).map(|s| s.name).unwrap_or_default();
        out.push((i, end, name));
        i = end;
    }
    out
}

fn block(s: &Setup) -> Vec<String> {
    let q = |v: &str| toml::Value::String(v.to_string()).to_string();
    let mut out = vec!["[[setups]]".to_string(), format!("name = {}", q(&s.name)), format!("account = {}", q(&s.account))];
    if !s.about.trim().is_empty() {
        out.push(format!("about = {}", q(s.about.trim())));
    }
    out.push(format!("repos = [{}]", s.repos.iter().map(|r| q(r)).collect::<Vec<_>>().join(", ")));
    // A role is a repo's: in the order of the repos, none for a repo that isn't one of them.
    let roles: Vec<String> = s.repos.iter().filter_map(|r| Some(s.role_of(r)).filter(|x| !x.is_empty()).map(|x| format!("{} = {}", q(r), q(x)))).collect();
    if !roles.is_empty() {
        out.push(format!("roles = {{ {} }}", roles.join(", ")));
    }
    out
}

/// A repo as `--repo` (or a line of the window's form) gives it: `<folder>=<its
/// role>`, or `<folder> — <its role>` (an em or en dash, or `-`, with a space
/// each side); just the folder when it has no role.
pub fn repo_and_role(given: &str) -> (String, String) {
    let given = given.trim();
    let cut = given.split_once('=').or_else(|| [" — ", " – ", " -- ", " - "].iter().find_map(|sep| given.split_once(sep)));
    match cut {
        Some((folder, role)) => (folder.trim().to_string(), role.split_whitespace().collect::<Vec<_>>().join(" ")),
        None => (given.to_string(), String::new()),
    }
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// A config.toml's text with the setup in it: in the place of the one with
/// that name, else at the end. The rest is kept as it is.
#[cfg(test)]
pub fn with_added(text: &str, s: &Setup) -> String {
    with_added_as(text, s, None)
}

/// The same, in the place of the one called `was` when it is given (a
/// project renamed keeps its place, so the first stays the default).
pub fn with_added_as(text: &str, s: &Setup, was: Option<&str>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = vec![];
    let named = |name: &str| was.is_some_and(|w| same(name, w)) || same(name, &s.name);
    match blocks(&lines).into_iter().find(|(_, _, name)| named(name)) {
        Some((start, end, _)) => {
            // Blank lines that closed the old table stay between it and the next.
            let tail = lines[start..end].iter().rev().take_while(|l| l.trim().is_empty()).count();
            out.extend(lines[..start].iter().map(|l| l.to_string()));
            out.extend(block(s));
            out.extend(lines[end - tail..].iter().map(|l| l.to_string()));
        }
        None => {
            out.extend(lines.iter().map(|l| l.to_string()));
            while out.last().is_some_and(|l| l.trim().is_empty()) {
                out.pop();
            }
            if !out.is_empty() {
                out.push(String::new());
            }
            out.extend(block(s));
        }
    }
    out.join("\n") + "\n"
}

/// A config.toml's text without the setup of that name, and whether there was one.
pub fn with_removed(text: &str, name: &str) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let Some((start, end, _)) = blocks(&lines).into_iter().find(|(_, _, n)| same(n, name)) else { return (text.to_string(), false) };
    let mut out: Vec<String> = lines[..start].iter().map(|l| l.to_string()).collect();
    // The blank line that set it apart goes with it.
    while out.last().is_some_and(|l| l.trim().is_empty()) && end >= lines.len() {
        out.pop();
    }
    out.extend(lines[end..].iter().map(|l| l.to_string()));
    let text = out.join("\n");
    (if text.is_empty() { text } else { text + "\n" }, true)
}

/// Keeps a change to the setups in the config file at `path`, unless the
/// file wouldn't read afterwards.
fn write(path: &Path, text: String) -> Result<()> {
    if let Err(e) = toml::from_str::<config::Config>(&text) {
        bail!("{} wouldn't read after that ({e}): nothing changed", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
pub fn add_at(path: &Path, s: &Setup) -> Result<()> {
    add_as_at(path, s, None)
}

pub fn add_as_at(path: &Path, s: &Setup, was: Option<&str>) -> Result<()> {
    write(path, with_added_as(&std::fs::read_to_string(path).unwrap_or_default(), s, was))
}

pub fn remove_at(path: &Path, name: &str) -> Result<bool> {
    let (text, was) = with_removed(&std::fs::read_to_string(path).unwrap_or_default(), name);
    if was {
        write(path, text)?;
    }
    Ok(was)
}

/// The setup a launch uses: the one named (its whole name, or a part only
/// one has), else the only one, else the first. It never asks; `how` says
/// which it was.
pub fn pick<'a>(setups: &'a [Setup], want: Option<&str>) -> Result<(&'a Setup, String)> {
    let names = || setups.iter().map(|s| format!("\"{}\"", s.name)).collect::<Vec<_>>().join(", ");
    if setups.is_empty() {
        bail!("no setups yet: tower setup add \"<name>\" --account <account> --repo <folder> [--repo <folder>…]");
    }
    match want.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) => {
            if let Some(s) = setups.iter().find(|s| same(&s.name, w)) {
                return Ok((s, "the one named".into()));
            }
            let lw = w.to_lowercase();
            let part: Vec<&Setup> = setups.iter().filter(|s| s.name.to_lowercase().contains(&lw)).collect();
            match part.as_slice() {
                [one] => Ok((one, format!("the only one that fits \"{w}\""))),
                [] => bail!("no setup {w:?} (setups: {})", names()),
                [first, ..] => Ok((first, format!("the first of {} that fit \"{w}\"", part.len()))),
            }
        }
        None if setups.len() == 1 => Ok((&setups[0], "the only one".into())),
        None => Ok((&setups[0], format!("the first of {}", setups.len()))),
    }
}

/// A project as `tower setups --json` and `GET /v1/setups` give it.
pub fn row(s: &Setup) -> serde_json::Value {
    let roles: serde_json::Map<String, serde_json::Value> = s.repos.iter().filter(|r| !s.role_of(r).is_empty()).map(|r| (r.clone(), json!(s.role_of(r)))).collect();
    json!({ "name": s.name, "account": s.account, "about": s.about.trim(), "repos": s.repos, "roles": roles })
}

/// `{"setups": [{"name", "account", "about", "repos": […], "roles": {repo: role}}]}`, as `tower setups --json` prints it.
pub fn value() -> Result<serde_json::Value> {
    Ok(json!({ "setups": list()?.iter().map(row).collect::<Vec<_>>() }))
}

/// `tower setups [--json]`.
pub fn cmd_list(as_json: bool) -> Result<()> {
    if as_json || !std::io::stdout().is_terminal() {
        println!("{}", serde_json::to_string_pretty(&value()?)?);
        return Ok(());
    }
    let list = list()?;
    if list.is_empty() {
        println!("No setups yet: tower setup add \"<name>\" --account <account> --repo <folder> [--repo <folder>…]");
    }
    for (i, s) in list.iter().enumerate() {
        let about = if s.about.trim().is_empty() { String::new() } else { format!("  {}", s.about.trim()) };
        println!("{} {}  (account {}){about}", if i == 0 { "*" } else { " " }, s.name, if s.account.is_empty() { "-" } else { &s.account });
        for r in &s.repos {
            match s.role_of(r) {
                "" => println!("    {r}"),
                role => println!("    {r} — {role}"),
            }
        }
    }
    Ok(())
}

/// A repo as the user gave it: a folder that is there is kept as written
/// (made whole when it isn't from `~` or `/`); a name is the best-matching
/// project folder, and the answer says so.
fn repo(given: &str, notes: &mut Vec<String>) -> Result<String> {
    let dir = crate::dispatch::expand(given.trim());
    if Path::new(&dir).is_dir() {
        if given.starts_with('~') || given.starts_with('/') {
            return Ok(given.trim().trim_end_matches('/').to_string());
        }
        return Ok(std::fs::canonicalize(&dir).map(|p| p.to_string_lossy().into_owned()).unwrap_or(dir));
    }
    match crate::projects::best(given) {
        Some((m, _)) => {
            notes.push(format!("no folder {given}: took {}, the best match", m.folder));
            Ok(m.folder)
        }
        None => bail!("no folder {given}, and no project folder fits that name"),
    }
}

/// `tower setup add "<name>" --account <a> [--about "<line>"] --repo "<dir>[=<role>]" [--repo …] [--was "<old name>"]`.
pub fn cmd_add(name: &str, account: Option<&str>, about: Option<&str>, repos: &[String], was: Option<&str>) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || repos.is_empty() {
        bail!("tower setup add \"<name>\" --account <account> [--about \"<what it is>\"] --repo \"<folder>=<its role>\" [--repo …]");
    }
    let was = was.map(str::trim).filter(|w| !w.is_empty() && !same(w, name));
    let have = list()?;
    if let Some(w) = was {
        if !have.iter().any(|s| same(&s.name, w)) {
            bail!("no project {w:?} to rename");
        }
        if have.iter().any(|s| same(&s.name, name)) {
            bail!("a project called {name:?} is there already");
        }
    }
    let mut notes = vec![];
    // The account as the claude-accounts mod knows it (the first one when none is given).
    let known = crate::accounts::list().unwrap_or_default();
    let account = match crate::accounts::resolve(account, &known) {
        Ok(a) => a.unwrap_or_default(),
        Err(e) if known.is_empty() => {
            notes.push(format!("{e:#}: kept as written"));
            account.unwrap_or("").trim().to_string()
        }
        Err(e) => return Err(e),
    };
    let mut folders: Vec<String> = vec![];
    let mut roles = std::collections::BTreeMap::new();
    for given in repos {
        let (folder, role) = repo_and_role(given);
        if folder.is_empty() {
            continue;
        }
        let r = repo(&folder, &mut notes)?;
        if !folders.contains(&r) {
            folders.push(r.clone());
        }
        if !role.is_empty() {
            roles.insert(r, role);
        }
    }
    if folders.is_empty() {
        bail!("tower setup add \"<name>\": no repo given (--repo \"<folder>=<its role>\")");
    }
    let about = about.map(|a| a.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
    let setup = Setup { name: name.to_string(), account, about, repos: folders, roles };
    config::Config::write_default_if_missing()?;
    let replaced = was.is_some() || have.iter().any(|s| same(&s.name, name));
    add_as_at(&config::config_file(), &setup, was)?;
    if std::io::stdout().is_terminal() {
        println!(
            "{} the project \"{}\": account {}, {} repo(s): {}.{}",
            if replaced { "Changed" } else { "Added" },
            setup.name,
            if setup.account.is_empty() { "-" } else { &setup.account },
            setup.repos.len(),
            setup.repos.iter().map(|r| match setup.role_of(r) { "" => r.clone(), role => format!("{r} ({role})") }).collect::<Vec<_>>().join(", "),
            notes.iter().map(|n| format!(" {}{}.", n[..1].to_uppercase(), &n[1..])).collect::<String>()
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&json!({ "setup": row(&setup), "replaced": replaced, "chose": notes }))?);
    }
    Ok(())
}

/// `tower setup remove "<name>"`.
pub fn cmd_remove(name: &str) -> Result<()> {
    if !remove_at(&config::config_file(), name)? {
        let names: Vec<String> = list()?.iter().map(|s| format!("\"{}\"", s.name)).collect();
        bail!("no setup {name:?} (setups: {})", if names.is_empty() { "none".into() } else { names.join(", ") });
    }
    if std::io::stdout().is_terminal() {
        println!("Removed the setup \"{}\".", name.trim());
    } else {
        println!("{}", serde_json::to_string_pretty(&json!({ "removed": name.trim() }))?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(name: &str, account: &str, repos: &[&str]) -> Setup {
        Setup { name: name.into(), account: account.into(), repos: repos.iter().map(|r| r.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn a_project_keeps_what_it_is_and_what_each_repo_is_through_the_file() {
        let mut p = setup("EYZFlow", "orca", &["~/Work/eyz/eyzflow", "~/Work/eyz/eyz-mediaroom-serverless", "/w/docs"]);
        p.about = "the video platform \"agencies\" run on".into();
        p.roles.insert("~/Work/eyz/eyzflow".into(), "the web app and its API".into());
        p.roles.insert("~/Work/eyz/eyz-mediaroom-serverless".into(), "the media backend: lambdas, S3".into());
        let text = with_added(CONFIG, &p);
        assert!(
            text.ends_with(
                "[[setups]]\nname = \"EYZFlow\"\naccount = \"orca\"\nabout = 'the video platform \"agencies\" run on'\nrepos = [\"~/Work/eyz/eyzflow\", \"~/Work/eyz/eyz-mediaroom-serverless\", \"/w/docs\"]\nroles = { \"~/Work/eyz/eyzflow\" = \"the web app and its API\", \"~/Work/eyz/eyz-mediaroom-serverless\" = \"the media backend: lambdas, S3\" }\n"
            ),
            "{text}"
        );
        // It reads back as it was, beside everything else; a repo with no role has none.
        let cfg: config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.setups, std::slice::from_ref(&p));
        assert_eq!((cfg.setups[0].role_of("~/Work/eyz/eyzflow"), cfg.setups[0].role_of("/w/docs")), ("the web app and its API", ""));
        // The same folder written another way finds its role.
        let whole = crate::dispatch::expand("~/Work/eyz/eyzflow/");
        assert_eq!(cfg.setups[0].role_of(&whole), "the web app and its API");
        // As the phone and the window read it.
        let shown = row(&cfg.setups[0]);
        assert_eq!(shown["about"], "the video platform \"agencies\" run on");
        assert_eq!(shown["roles"], json!({ "~/Work/eyz/eyzflow": "the web app and its API", "~/Work/eyz/eyz-mediaroom-serverless": "the media backend: lambdas, S3" }));
        assert_eq!(shown["repos"].as_array().map(|a| a.len()), Some(3));
        // A table from before has neither, and is written back without them.
        let old = "[[setups]]\nname = \"web\"\naccount = \"orca\"\nrepos = [\"~/w/web\"]\n";
        let cfg: config::Config = toml::from_str(old).unwrap();
        assert_eq!((cfg.setups[0].about.as_str(), cfg.setups[0].roles.len()), ("", 0));
        assert_eq!(with_added("", &cfg.setups[0]), old);
        assert_eq!(row(&cfg.setups[0])["roles"], json!({}));
        // Renamed, it keeps its place (the first stays the default); the other is untouched.
        let both = with_added(&with_added("", &cfg.setups[0]), &p);
        let renamed = with_added_as(&both, &setup("website", "orca", &["~/w/web"]), Some("WEB"));
        let cfg: config::Config = toml::from_str(&renamed).unwrap();
        assert_eq!(cfg.setups.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["website", "EYZFlow"]);
    }

    #[test]
    fn a_repo_is_given_with_its_role() {
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(repo_and_role("~/Work/eyz/eyzflow=the web app"), pair("~/Work/eyz/eyzflow", "the web app"));
        assert_eq!(repo_and_role(" ~/Work/eyz/eyzflow = the web app,  and its API "), pair("~/Work/eyz/eyzflow", "the web app, and its API"));
        assert_eq!(repo_and_role("~/Work/eyz/eyz-mediaroom-serverless — the media backend"), pair("~/Work/eyz/eyz-mediaroom-serverless", "the media backend"));
        assert_eq!(repo_and_role("eyzflow – the app"), pair("eyzflow", "the app"));
        assert_eq!(repo_and_role("eyzflow - the app - and more"), pair("eyzflow", "the app - and more"));
        // A dash inside a folder's name is the name's.
        assert_eq!(repo_and_role("~/Work/eyz/eyz-mediaroom-serverless"), pair("~/Work/eyz/eyz-mediaroom-serverless", ""));
        assert_eq!(repo_and_role("  "), pair("", ""));
    }

    const CONFIG: &str = "# herder-tower settings.\n\nvoice = \"af_bella\"  # the voice\nwake_words = [\n    \"hey tower\",\n    \"tower\",\n]\n\n[tell]\nlevel = \"results\"\n\n[lan]\nenabled = true\nport = 7843\n";

    #[test]
    fn a_setup_is_added_and_removed_without_touching_the_rest() {
        let a = setup("eyzflow + mediaroom on orca", "orca", &["~/Work/eyz/eyzflow", "~/Work/eyz/eyz-mediaroom-serverless"]);
        let with = with_added(CONFIG, &a);
        assert_eq!(
            with,
            format!(
                "{CONFIG}\n[[setups]]\nname = \"eyzflow + mediaroom on orca\"\naccount = \"orca\"\nrepos = [\"~/Work/eyz/eyzflow\", \"~/Work/eyz/eyz-mediaroom-serverless\"]\n"
            )
        );
        // It reads back as it was written, beside everything else.
        let cfg: config::Config = toml::from_str(&with).unwrap();
        assert_eq!(cfg.setups, std::slice::from_ref(&a));
        assert_eq!((cfg.voice.as_str(), cfg.tell.level, cfg.lan.enabled, cfg.wake_words.len()), ("af_bella", crate::tell::Level::Results, true, 2));
        // A second one goes after it; the first stays the first.
        let b = setup("the \"tower\" itself", "default", &["/w/herder tower"]);
        let both = with_added(&with, &b);
        let cfg: config::Config = toml::from_str(&both).unwrap();
        assert_eq!(cfg.setups, [a.clone(), b.clone()]);
        // The same name again (any case) changes it in place.
        let a2 = setup("Eyzflow + Mediaroom on Orca", "work", &["~/Work/eyz/eyzflow"]);
        let changed = with_added(&both, &a2);
        let cfg: config::Config = toml::from_str(&changed).unwrap();
        assert_eq!(cfg.setups, [a2.clone(), b.clone()]);
        assert!(changed.starts_with(CONFIG));
        // Removing the last one leaves the file as it was with the other.
        let (text, was) = with_removed(&both, "the \"tower\" itself");
        assert!(was);
        assert_eq!(text, with);
        // Removing the first keeps what is after it.
        let (text, was) = with_removed(&both, "EYZFLOW + mediaroom on orca");
        assert!(was);
        let cfg: config::Config = toml::from_str(&text).unwrap();
        assert_eq!((cfg.setups, cfg.lan.port, cfg.tell.level), (vec![b.clone()], 7843, crate::tell::Level::Results));
        assert!(text.starts_with(CONFIG));
        // Both gone: the file as it was before any setup.
        assert_eq!(with_removed(&with, "eyzflow + mediaroom on orca"), (CONFIG.to_string(), true));
        assert_eq!(with_removed(CONFIG, "nothing"), (CONFIG.to_string(), false));
        // An empty file gets just the table.
        assert_eq!(with_added("", &setup("x", "a", &["/w"])), "[[setups]]\nname = \"x\"\naccount = \"a\"\nrepos = [\"/w\"]\n");
    }

    #[test]
    fn a_setup_between_other_tables_is_changed_in_its_place() {
        let text = "voice = \"x\"\n\n[[setups]]\nname = \"one\"\naccount = \"a\"\nrepos = [\n  \"/w/a\",\n  \"/w/b\",\n]\n\n[lan]\nenabled = true\n\n[[setups]]\nname = \"two\"\naccount = \"b\"\nrepos = [\"/w/c\"]\n";
        let cfg: config::Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.setups.iter().map(|s| (s.name.as_str(), s.repos.len())).collect::<Vec<_>>(), [("one", 2), ("two", 1)]);
        let changed = with_added(text, &setup("one", "c", &["/w/z"]));
        assert_eq!(
            changed,
            "voice = \"x\"\n\n[[setups]]\nname = \"one\"\naccount = \"c\"\nrepos = [\"/w/z\"]\n\n[lan]\nenabled = true\n\n[[setups]]\nname = \"two\"\naccount = \"b\"\nrepos = [\"/w/c\"]\n"
        );
        let (gone, was) = with_removed(text, "one");
        assert!(was);
        assert_eq!(gone, "voice = \"x\"\n\n[lan]\nenabled = true\n\n[[setups]]\nname = \"two\"\naccount = \"b\"\nrepos = [\"/w/c\"]\n");
        let cfg: config::Config = toml::from_str(&gone).unwrap();
        assert!(cfg.lan.enabled && cfg.setups.len() == 1);
    }

    #[test]
    fn the_file_keeps_its_setups_through_tower_setup() {
        let dir = std::env::temp_dir().join(format!("tower-setups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        add_at(&path, &setup("one", "a", &["/w/a"])).unwrap();
        std::fs::write(&path, format!("{CONFIG}{}", std::fs::read_to_string(&path).unwrap())).unwrap();
        add_at(&path, &setup("two", "b", &["/w/b", "/w/c"])).unwrap();
        let cfg: config::Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.setups.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        assert!(remove_at(&path, "one").unwrap() && !remove_at(&path, "one").unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(CONFIG) && text.contains("name = \"two\"") && !text.contains("name = \"one\""), "{text}");
        // A file that wouldn't read with it is left alone.
        std::fs::write(&path, "setups = []\n").unwrap();
        assert!(add_at(&path, &setup("x", "a", &["/w"])).unwrap_err().to_string().contains("nothing changed"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "setups = []\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_launch_takes_the_named_setup_else_the_only_one_else_the_first() {
        let one = [setup("eyzflow + mediaroom on orca", "orca", &["/w/a"])];
        let three = [one[0].clone(), setup("tower", "default", &["/w/t"]), setup("tower android", "default", &["/w/ta"])];
        assert!(pick(&[], None).unwrap_err().to_string().contains("tower setup add"));
        assert_eq!(pick(&one, None).map(|(s, how)| (s.name.as_str(), how)).unwrap(), ("eyzflow + mediaroom on orca", "the only one".to_string()));
        assert_eq!(pick(&three, None).map(|(s, how)| (s.name.as_str(), how)).unwrap(), ("eyzflow + mediaroom on orca", "the first of 3".to_string()));
        assert_eq!(pick(&three, Some(" TOWER ")).unwrap().0.name, "tower", "its whole name, any case");
        assert_eq!(pick(&three, Some("android")).unwrap().0.name, "tower android");
        assert_eq!(pick(&three, Some("mediaroom")).unwrap().1, "the only one that fits \"mediaroom\"");
        // Several fit: the first, and it says so. It never asks.
        let (s, how) = pick(&three, Some("o")).unwrap();
        assert_eq!((s.name.as_str(), how.as_str()), ("eyzflow + mediaroom on orca", "the first of 3 that fit \"o\""));
        assert!(pick(&three, Some("nothing")).unwrap_err().to_string().contains("\"tower android\""));
    }
}
