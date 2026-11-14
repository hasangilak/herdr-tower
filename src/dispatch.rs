//! `tower dispatch`: one plan for a request that holds several tasks. The
//! brain says what the tasks are (each with its project folder, its
//! instruction, and what it must wait for); the tower decides where each
//! goes, the same way every time:
//!
//! - a task names a session (`session`): queued for it;
//! - else the agent session running in the task's project folder: queued
//!   for it (it waits behind what that session is doing);
//! - else, or with `isolate` (work beside another task in the same
//!   project), a new named session in that folder (with `isolate`, in a git
//!   worktree of its own, so two sessions never edit one checkout), started
//!   now, or once what it waits for is done. A new session for a space that
//!   isn't standing works in that space's worktree of the project
//!   (`tower start` makes it: see `worktrees`), and a session already there
//!   counts as the one in the project's folder;
//! - `after`: the other tasks (by their ids in the plan) that must be done
//!   first. The send queue holds it until they are.
//!
//! Every task is watched when it goes in, and the whole plan is one agenda in
//! the ledger (`crate::agenda`), with the user's words (`request`), so no
//! task of it is lost. It prints one line per task, for the brain to tell the
//! user in a sentence.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::hc;
use crate::protocol::{self, Request};
use crate::split::{self, Project};

#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    /// The user's words, as they asked (default: the request the tower heard last).
    #[serde(default)]
    pub request: String,
    /// The agenda it belongs to (`a3`; default: a new one, or the request's).
    #[serde(default)]
    pub agenda: Option<String>,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    /// Its id in the plan (for `after`).
    pub id: String,
    /// A few words, for the user and the history.
    #[serde(default)]
    pub title: String,
    /// The project's folder (`~/Work/app`).
    #[serde(default)]
    pub project: String,
    /// The complete, self-contained instruction.
    pub instruction: String,
    #[serde(default)]
    pub after: Vec<String>,
    /// A session to give it to, by address or name.
    #[serde(default)]
    pub session: Option<String>,
    /// A new session's name (default: from the project and the title).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub space: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    /// Another machine.
    #[serde(default)]
    pub on: Option<String>,
    /// Its own session in a git worktree, beside other work in the project.
    #[serde(default)]
    pub isolate: bool,
    /// Sent from a plan (`p1`) for one of its stages (`s2`): its
    /// instruction goes out as the structured handoff (`plans::handoff_text`),
    /// with the fields below, and its agenda task is linked to the stage.
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub so_far: Option<String>,
    #[serde(default)]
    pub constraints: Option<String>,
    #[serde(default)]
    pub uncertain: Option<String>,
}

/// A plan's tasks go out as the structured handoff: each task with `plan`
/// and `stage` gets its instruction rendered (its words are the `Next`),
/// and its stage (`p1/s2`). The plans are read once.
fn with_handoffs(plan: &mut Plan) -> Result<BTreeMap<String, String>> {
    let mut stages = BTreeMap::new();
    let mut read: BTreeMap<String, crate::plans::Plan> = BTreeMap::new();
    for t in plan.tasks.iter_mut() {
        let (Some(pid), Some(sid)) = (t.plan.clone().filter(|x| !x.trim().is_empty()), t.stage.clone().filter(|x| !x.trim().is_empty())) else {
            if t.plan.is_some() || t.stage.is_some() {
                bail!("task {}: a task from a plan names both its plan and its stage", t.id);
            }
            continue;
        };
        if !read.contains_key(&pid) {
            let v = crate::plans::show(Some(&pid))?;
            read.insert(pid.clone(), serde_json::from_value(v).context("the plan")?);
        }
        let p = &read[&pid];
        let text = crate::plans::handoff_text(
            p,
            &sid,
            &crate::plans::TaskHandoff {
                task: &t.id,
                goal: t.goal.as_deref().unwrap_or(""),
                so_far: t.so_far.as_deref().unwrap_or(""),
                constraints: t.constraints.as_deref().unwrap_or(""),
                uncertain: t.uncertain.as_deref().unwrap_or(""),
                next: &t.instruction,
            },
        )?;
        t.instruction = text;
        stages.insert(t.id.clone(), format!("{}/{}", pid.trim(), sid.trim()));
    }
    Ok(stages)
}

/// Where a task went.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Queued for (or sent to) an existing session.
    Queue { session: String },
    /// A new session in `cwd`.
    Start { name: String, cwd: String, worktree: bool },
}

/// An agent session that is running, as dispatch needs it.
#[derive(Debug, Clone)]
pub struct Live {
    pub address: String,
    pub name: String,
    pub cwd: String,
    /// `idle`, `working`, ...
    pub status: String,
}

pub fn run(json_text: &str, dry_run: bool) -> Result<()> {
    let mut plan: Plan = serde_json::from_str(json_text).context("the plan: {\"tasks\":[{\"id\",\"project\",\"instruction\",\"after\"}]}")?;
    let stages = with_handoffs(&mut plan)?;
    let order = check(&plan)?;
    let live = live_sessions();
    let routes = route(&plan, &order, &live)?;
    if dry_run {
        let v: Vec<Value> =
            order.iter().map(|&i| json!({ "id": plan.tasks[i].id, "route": format!("{:?}", routes[&plan.tasks[i].id]) })).collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    // One agenda for the whole plan: the user's words, every task in it.
    let agenda = open_agenda(&plan);
    // In order: what a task waits for has a queue id before it does.
    let mut ids: BTreeMap<String, u64> = BTreeMap::new();
    let mut lines = vec![];
    let mut rows = vec![];
    for &i in &order {
        let t = &plan.tasks[i];
        let after: Vec<u64> = t.after.iter().filter_map(|a| ids.get(a).copied()).collect();
        let title = if t.title.is_empty() { t.id.clone() } else { t.title.clone() };
        let tag = crate::agenda::Tag {
            agenda: agenda.clone(),
            title: title.clone(),
            plan_id: t.id.clone(),
            after: t.after.clone(),
            project: t.project.clone(),
            space: t.space.clone(),
            skip: false,
            from: None,
            stage: stages.get(&t.id).cloned().unwrap_or_default(),
        };
        let (line, id, task) = match &routes[&t.id] {
            Route::Queue { session } => queue(session, &t.instruction, &after, tag)?,
            Route::Start { name, cwd, worktree } => {
                let cwd = if *worktree { make_worktree(cwd, name)? } else { cwd.clone() };
                start(t, name, &cwd, *worktree, &after, tag)?
            }
        };
        ids.insert(t.id.clone(), id);
        lines.push(format!("{title}: {line}"));
        rows.push(json!({ "id": t.id, "title": title, "queue_id": id, "agenda_task": task, "what": line }));
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        if let Some(a) = &agenda {
            println!("Agenda {a}: every task below is followed until it's done (`tower agenda show {a}`).");
        }
        for l in &lines {
            println!("{l}");
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&json!({ "agenda": agenda, "tasks": rows }))?);
    }
    Ok(())
}

/// The plan's agenda in the ledger: the one it names, or one opened for the
/// user's words (the request's own, when it has one already). None: no voice to keep it.
fn open_agenda(plan: &Plan) -> Option<String> {
    if let Some(a) = &plan.agenda {
        return Some(a.clone());
    }
    let titles: Vec<&str> = plan.tasks.iter().map(|t| if t.title.is_empty() { t.id.as_str() } else { t.title.as_str() }).collect();
    let spaces: Vec<String> = plan.tasks.iter().filter_map(|t| t.space.clone()).collect();
    let cmd = crate::agenda::Cmd { action: "open".into(), text: plan.request.clone(), title: titles.join("; "), spaces, ..Default::default() };
    match protocol::send(&Request::Agenda { ledger: cmd }) {
        Ok(r) if r.ok => r.state.and_then(|v| v["id"].as_str().map(String::from)),
        Ok(r) => {
            eprintln!("tower: no agenda for the plan: {}", r.error.unwrap_or_default());
            None
        }
        Err(e) => {
            eprintln!("tower: no agenda for the plan: {e:#}");
            None
        }
    }
}

/// The plan makes sense: ids unique, what tasks wait for exists, no loop,
/// every instruction says something. Returns an order that puts what a
/// task waits for before it.
pub fn check(plan: &Plan) -> Result<Vec<usize>> {
    if plan.tasks.is_empty() {
        bail!("a plan with no tasks");
    }
    let mut seen = BTreeSet::new();
    for t in &plan.tasks {
        if t.id.trim().is_empty() || !seen.insert(t.id.as_str()) {
            bail!("each task needs its own id ({:?} is empty or used twice)", t.id);
        }
        if t.instruction.trim().is_empty() {
            bail!("task {} has no instruction", t.id);
        }
        if t.session.is_none() && t.project.trim().is_empty() {
            bail!("task {} needs a project folder or a session", t.id);
        }
    }
    for t in &plan.tasks {
        for a in &t.after {
            if !seen.contains(a.as_str()) {
                bail!("task {} waits for {a}, which isn't in the plan", t.id);
            }
        }
    }
    // Kahn's order.
    let index: BTreeMap<&str, usize> = plan.tasks.iter().enumerate().map(|(i, t)| (t.id.as_str(), i)).collect();
    let mut waits: Vec<usize> = plan.tasks.iter().map(|t| t.after.len()).collect();
    let mut order = vec![];
    let mut ready: Vec<usize> = (0..plan.tasks.len()).filter(|&i| waits[i] == 0).collect();
    while let Some(i) = ready.first().copied() {
        ready.remove(0);
        order.push(i);
        for (j, t) in plan.tasks.iter().enumerate() {
            if t.after.iter().any(|a| index[a.as_str()] == i) {
                waits[j] -= 1;
                if waits[j] == 0 {
                    ready.push(j);
                }
            }
        }
    }
    if order.len() != plan.tasks.len() {
        bail!("the tasks wait for each other in a loop");
    }
    Ok(order)
}

/// Where each task goes (see the module's rules).
pub fn route(plan: &Plan, order: &[usize], live: &[Live]) -> Result<BTreeMap<String, Route>> {
    let mut out = BTreeMap::new();
    let mut names: BTreeSet<String> = live.iter().map(|l| l.name.clone()).collect();
    for &i in order {
        let t = &plan.tasks[i];
        if let Some(s) = &t.session {
            out.insert(t.id.clone(), Route::Queue { session: s.clone() });
            continue;
        }
        let mut cwd = expand(&t.project);
        if t.on.is_none() && !Path::new(&cwd).is_dir() {
            // A project's name, or a guessed path: the best-matching folder (it's said in the result).
            match crate::projects::best(&t.project) {
                Some((m, _)) => cwd = m.path.to_string_lossy().into_owned(),
                None => bail!("task {}: no folder {cwd}, and no project folder fits {:?}", t.id, t.project),
            }
        }
        // The one in that folder: a free one first, then a working one, then any.
        let rank = |l: &Live| match l.status.as_str() {
            "idle" | "done" => 0,
            "working" => 1,
            _ => 2,
        };
        // A session in one of the tower's worktrees of the project
        // (`worktrees`) is in the project too: any of them when the task
        // names no space, its space's own when it does. A task for a space
        // goes to that space's worktree before the project's own checkout.
        let slug = t.space.as_deref().map(crate::worktrees::slug);
        let in_worktree = |l: &Live| {
            crate::worktrees::repo_of(Path::new(&l.cwd)).is_some_and(|(repo, top)| {
                same_folder(&repo.to_string_lossy(), &cwd)
                    && slug.as_ref().is_none_or(|s| top.parent().and_then(|p| p.file_name()).is_some_and(|d| d.to_string_lossy() == s.as_str()))
            })
        };
        let here = live
            .iter()
            .filter(|l| same_folder(&l.cwd, &cwd) || in_worktree(l))
            .min_by_key(|l| (in_worktree(l) != slug.is_some(), rank(l)));
        match here {
            Some(l) if !t.isolate => {
                out.insert(t.id.clone(), Route::Queue { session: l.address.clone() });
            }
            _ => {
                let base = t.name.clone().unwrap_or_else(|| default_name(&cwd, &t.title, &t.id));
                let mut name = base.clone();
                let mut n = 2;
                while names.contains(&name) {
                    name = format!("{base}-{n}");
                    n += 1;
                }
                names.insert(name.clone());
                // A second session in a folder that has one gets a worktree.
                let worktree =
                    t.isolate && (here.is_some() || plan.tasks.iter().any(|o| o.id != t.id && same_folder(&expand(&o.project), &cwd)));
                out.insert(t.id.clone(), Route::Start { name, cwd, worktree });
            }
        }
    }
    Ok(out)
}

fn queue(session: &str, text: &str, after: &[u64], tag: crate::agenda::Tag) -> Result<(String, u64, Value)> {
    let r = protocol::send(&Request::Queue {
        action: "add".into(),
        session: session.into(),
        text: text.into(),
        id: None,
        after: after.to_vec(),
        start: vec![],
        tag: Some(tag),
        from: None,
    })?;
    if !r.ok {
        bail!("queueing for {session}: {}", r.error.unwrap_or_default());
    }
    let v = r.state.unwrap_or_default();
    let id = v["id"].as_u64().unwrap_or(0);
    let who = format!("{} ({})", v["session"].as_str().unwrap_or(session), v["name"].as_str().unwrap_or(""));
    let line = if v["sent"] == true {
        format!("sent to {who}, watched (#{id})")
    } else if after.is_empty() {
        format!("queued for {who}, position {} (#{id}): it goes in when the session stops working", v["position"])
    } else {
        format!("queued for {who} (#{id}), after {}", after.iter().map(|a| format!("#{a}")).collect::<Vec<_>>().join(", "))
    };
    Ok((line, id, v["task"].clone()))
}

fn start(t: &Task, name: &str, cwd: &str, own_worktree: bool, after: &[u64], tag: crate::agenda::Tag) -> Result<(String, u64, Value)> {
    let mut args: Vec<String> = vec!["--name".into(), name.into(), "--cwd".into(), cwd.into()];
    // It has a worktree of its own already: not the space's as well.
    if own_worktree {
        args.push("--no-worktree".into());
    }
    for (flag, v) in [("--space", &t.space), ("--account", &t.account), ("--on", &t.on)] {
        if let Some(v) = v {
            args.extend([flag.to_string(), v.clone()]);
        }
    }
    // Its place in the agenda (a start held back is the queue's item already: `--no-track`).
    if let Some(a) = &tag.agenda {
        args.extend(["--agenda".into(), a.clone()]);
    }
    args.extend(["--title".into(), tag.title.clone(), "--plan-id".into(), tag.plan_id.clone()]);
    if !tag.stage.is_empty() {
        args.extend(["--plan-stage".into(), tag.stage.clone()]);
    }
    if !after.is_empty() {
        let mut held = args.clone();
        held.push("--no-track".into());
        held.push(t.instruction.clone());
        let r = protocol::send(&Request::Queue {
            action: "start".into(),
            session: name.into(),
            text: t.instruction.clone(),
            id: None,
            after: after.to_vec(),
            start: held,
            tag: Some(tag),
            from: None,
        })?;
        if !r.ok {
            bail!("queueing the start of {name}: {}", r.error.unwrap_or_default());
        }
        let v = r.state.unwrap_or_default();
        let id = v["id"].as_u64().unwrap_or(0);
        return Ok((
            format!(
                "a new session {name} in {cwd}, started once {} is done (#{id})",
                after.iter().map(|a| format!("#{a}")).collect::<Vec<_>>().join(", ")
            ),
            id,
            v["task"].clone(),
        ));
    }
    args.push(t.instruction.clone());
    let me = std::env::current_exe()?;
    let mut cmd = Command::new(me);
    cmd.arg("start").args(&args);
    let out = hc::output_within(cmd, "tower start", Duration::from_secs(180))?;
    if out.code != 0 {
        bail!("starting {name}: {}{}", out.stderr.trim(), out.stdout.trim());
    }
    let v: Value = serde_json::from_str(out.stdout.trim()).unwrap_or_default();
    let address = v["address"].as_str().unwrap_or(name).to_string();
    // `tower start` gave it an id later tasks can wait for (and its place in the agenda).
    let (id, task) = match v["queue_id"].as_u64() {
        Some(id) if id > 0 => (id, v["task"].clone()),
        _ => {
            let r = protocol::send(&Request::Queue {
                action: "track".into(),
                session: address.clone(),
                text: t.instruction.clone(),
                id: None,
                after: vec![],
                start: vec![],
                tag: Some(tag),
                from: None,
            })?;
            let s = r.state.unwrap_or_default();
            (s["id"].as_u64().unwrap_or(0), s["task"].clone())
        }
    };
    let space = v["space"].as_str().map(|s| format!(" in space \"{s}\"")).unwrap_or_default();
    Ok((format!("started {name} as {address}{space}, in {cwd}, watched (#{id})"), id, task))
}

/// A git worktree beside the project for a session of its own: `<project>-<name>`
/// on a new branch `tower/<name>`.
fn make_worktree(project: &str, name: &str) -> Result<String> {
    let dir = format!("{}-{name}", project.trim_end_matches('/'));
    if Path::new(&dir).exists() {
        return Ok(dir);
    }
    let place = crate::worktrees::Place { repo: project.to_string(), path: dir.clone(), branch: format!("tower/{name}") };
    crate::worktrees::add(Path::new(project), &place)?;
    Ok(dir)
}

/// `~/x` as a shell would have it.
pub fn expand(folder: &str) -> String {
    match folder.strip_prefix("~/") {
        Some(rest) => crate::config::home().join(rest).to_string_lossy().into_owned(),
        None if folder == "~" => crate::config::home().to_string_lossy().into_owned(),
        None => folder.to_string(),
    }
}

fn same_folder(a: &str, b: &str) -> bool {
    let canon = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p.trim_end_matches('/')));
    !a.is_empty() && !b.is_empty() && canon(a) == canon(b)
}

/// `<project>-<a word or two of the title>`, in Herdr's name alphabet.
fn default_name(cwd: &str, title: &str, id: &str) -> String {
    let project = split::normalize(cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("task")).replace(' ', "-");
    let words: Vec<String> = split::normalize(if title.is_empty() { id } else { title })
        .split(' ')
        .filter(|w| w.len() > 2 && !["the", "and", "for", "add", "fix"].contains(w))
        .take(2)
        .map(String::from)
        .collect();
    let mut name = if words.is_empty() { project } else { format!("{project}-{}", words.join("-")) };
    name.truncate(40);
    name.trim_end_matches('-').to_string()
}

/// The agent sessions running here, with their folders.
pub fn live_sessions() -> Vec<Live> {
    hc::list_on(None)
        .unwrap_or_default()
        .iter()
        .filter(|t| t["state"].as_str().is_none_or(|s| s == "live") && hc::status_word(t).is_some())
        // (A brain, the main one or a space's own, takes no task.)
        .filter(|t| crate::brain::brain_of(t).is_none())
        .map(|t| Live {
            address: t["address"].as_str().unwrap_or("").to_string(),
            name: hc::name(t).unwrap_or("").to_string(),
            cwd: t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str()).unwrap_or("").to_string(),
            status: hc::status_word(t).unwrap_or_default(),
        })
        .collect()
}

/// The projects the tower knows: the folders its agent sessions run in,
/// and the aliases in `~/.config/herder-tower/aliases.json`
/// (`{"dashboard": "~/Work/eyz/eyzflow"}`).
pub fn known_projects() -> Vec<Project> {
    let mut out: Vec<Project> = vec![];
    let (home, brain) = (crate::config::home(), crate::config::brain_dir());
    for l in live_sessions() {
        // Home and the tower's own folders are no project.
        if same_folder(&l.cwd, &home.to_string_lossy())
            || Path::new(&l.cwd).starts_with(&brain)
            || Path::new(&l.cwd).starts_with(crate::config::config_dir())
        {
            continue;
        }
        if let Some(p) = Project::of_folder(&l.cwd)
            && !out.iter().any(|o| o.folder == p.folder)
        {
            out.push(p);
        }
    }
    let aliases: BTreeMap<String, String> = std::fs::read_to_string(crate::config::config_dir().join("aliases.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    for (alias, folder) in aliases {
        let folder = expand(&folder);
        match out.iter_mut().find(|p| same_folder(&p.folder, &folder)) {
            Some(p) => p.names.push(split::normalize(&alias)),
            None => {
                if let Some(mut p) = Project::of_folder(&folder) {
                    p.names.push(split::normalize(&alias));
                    out.push(p);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, project: &str, after: &[&str]) -> Task {
        Task {
            id: id.into(),
            title: format!("task {id}"),
            project: project.into(),
            instruction: format!("do {id}"),
            after: after.iter().map(|s| s.to_string()).collect(),
            session: None,
            name: None,
            space: None,
            account: None,
            on: None,
            isolate: false,
            plan: None,
            stage: None,
            goal: None,
            so_far: None,
            constraints: None,
            uncertain: None,
        }
    }

    #[test]
    fn a_plan_is_checked_and_ordered() {
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let plan = Plan { request: String::new(), agenda: None, tasks: vec![task("c", &tmp, &["a", "b"]), task("a", &tmp, &[]), task("b", &tmp, &["a"])] };
        let order = check(&plan).unwrap();
        let ids: Vec<&str> = order.iter().map(|&i| plan.tasks[i].id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        let looped = Plan { request: String::new(), agenda: None, tasks: vec![task("a", &tmp, &["b"]), task("b", &tmp, &["a"])] };
        assert!(check(&looped).unwrap_err().to_string().contains("loop"));
        let unknown = Plan { request: String::new(), agenda: None, tasks: vec![task("a", &tmp, &["zz"])] };
        assert!(check(&unknown).is_err());
        let twice = Plan { request: String::new(), agenda: None, tasks: vec![task("a", &tmp, &[]), task("a", &tmp, &[])] };
        assert!(check(&twice).is_err());
    }

    #[test]
    fn tasks_go_to_the_session_in_their_folder_or_a_new_one() {
        let base = std::env::temp_dir().join(format!("tower-dispatch-{}", std::process::id()));
        let (api, web) = (base.join("api"), base.join("webapp"));
        std::fs::create_dir_all(&api).unwrap();
        std::fs::create_dir_all(&web).unwrap();
        let (api, web) = (api.to_string_lossy().into_owned(), web.to_string_lossy().into_owned());
        let live = vec![
            Live { address: "t4".into(), name: "api-busy".into(), cwd: api.clone(), status: "blocked".into() },
            Live { address: "t5".into(), name: "api-main".into(), cwd: api.clone(), status: "idle".into() },
        ];
        let mut iso = task("c", &api, &[]);
        iso.isolate = true;
        iso.title = "Rewrite the auth module".into();
        let plan = Plan { request: String::new(), agenda: None, tasks: vec![task("a", &api, &[]), task("b", &web, &["a"]), iso] };
        let order = check(&plan).unwrap();
        let r = route(&plan, &order, &live).unwrap();
        assert_eq!(r["a"], Route::Queue { session: "t5".into() });
        assert!(matches!(&r["b"], Route::Start { name, worktree: false, .. } if name == "webapp-task"));
        assert!(matches!(&r["c"], Route::Start { name, worktree: true, .. } if name == "api-rewrite-auth"));
        let missing = Plan { request: String::new(), agenda: None, tasks: vec![task("a", "/no/such/folder", &[])] };
        assert!(route(&missing, &[0], &live).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_session_in_a_spaces_worktree_is_in_its_project() {
        let base = std::env::temp_dir().join(format!("tower-dispatch-wt-{}", std::process::id()));
        let api = base.join("api");
        let (shop, docs) = (base.join(".tower-worktrees/shop/api"), base.join(".tower-worktrees/docs/api"));
        for d in [&api, &shop, &docs] {
            std::fs::create_dir_all(d).unwrap();
        }
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let l = |address: &str, cwd: &std::path::Path, status: &str| Live { address: address.into(), name: address.into(), cwd: s(cwd), status: status.into() };
        let live = vec![l("t1", &api, "working"), l("t2", &shop, "working"), l("t3", &docs, "idle")];
        let plan = |space: Option<&str>| {
            let mut t = task("a", &s(&api), &[]);
            t.space = space.map(String::from);
            Plan { request: String::new(), agenda: None, tasks: vec![t] }
        };
        // A task for a space goes to that space's session, though another is free.
        assert_eq!(route(&plan(Some("Shop")), &[0], &live).unwrap()["a"], Route::Queue { session: "t2".into() });
        assert_eq!(route(&plan(Some("docs")), &[0], &live).unwrap()["a"], Route::Queue { session: "t3".into() });
        // No space named: the project's own checkout first, then a worktree of it.
        assert_eq!(route(&plan(None), &[0], &live).unwrap()["a"], Route::Queue { session: "t1".into() });
        assert_eq!(route(&plan(None), &[0], &live[1..]).unwrap()["a"], Route::Queue { session: "t3".into() }, "the free one of the two worktrees");
        // A space with no session in the project yet: the project's own session if there is one, else a new one.
        assert_eq!(route(&plan(Some("Billing")), &[0], &live).unwrap()["a"], Route::Queue { session: "t1".into() });
        assert!(matches!(route(&plan(Some("Billing")), &[0], &live[1..]).unwrap()["a"], Route::Start { worktree: false, .. }));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn names_are_short_and_herdr_safe() {
        assert_eq!(default_name("/w/herder-tower", "Add a mute button to the orb", "a"), "herder-tower-mute-button");
        assert_eq!(default_name("/w/x_y", "", "t1"), "x-y");
    }
}
