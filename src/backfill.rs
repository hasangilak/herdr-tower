//! `tower youtrack backfill <plan.json> [--apply|--verify]`: YouTrack
//! tickets for PRs that got none at the time (tickets were off, or the PR
//! is merged already), grouped under epics.
//!
//! - **The plan** (JSON, written and read by people): its `epics`, each
//!   with the PRs that make it (each one's ticket becomes a subtask of it)
//!   and the release PRs that ship it (named on the epic, no ticket of
//!   their own), and its `tasks`, PRs that stand alone (a plain ticket).
//!   An epic may be one YouTrack has already (`id`): it is reused.
//! - **Each PR's ticket** is the tower's own (`crate::youtrack`): the same
//!   summary, description and type, found before it is made (the PR's
//!   comment, else its link in a ticket), and linked on the PR by the
//!   tower's comment. Merged PRs too: this is what they missed.
//! - **Never twice**: what was made is kept beside the plan
//!   (`<plan>.done.json`), so a run that stopped goes on where it was;
//!   a PR listed twice in the plan is refused before anything is made.
//! - Without `--apply` nothing is written: it says what it would do.
//!   `--verify` reads every ticket back: its project, type and epic, and
//!   that each PR has exactly one ticket and one comment linking it.
//! - Tickets stay off (`[youtrack] enabled`): this never switches them on.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::YoutrackCfg;
use crate::prs::Pr;
use crate::youtrack::{self, Remote, Ticket};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Plan {
    pub epics: Vec<EpicPlan>,
    pub tasks: Vec<TaskPlan>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EpicPlan {
    /// The plan's own name for it (`support-inbox-video`).
    #[serde(rename = "ref")]
    pub reference: String,
    /// One YouTrack has already, to reuse ("": made).
    pub id: String,
    /// Its project's short name (`EYF`).
    pub project: String,
    pub summary: String,
    /// What the feature is, in a sentence or two.
    pub description: String,
    /// The PRs that make it (`owner/repo#12`).
    pub prs: Vec<String>,
    /// The release PRs that ship it.
    pub releases: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskPlan {
    pub pr: String,
    /// The release PRs that ship it.
    pub releases: Vec<String>,
}

/// What a run made or found, kept beside the plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Done {
    /// Epic ref → its ticket.
    pub epics: BTreeMap<String, String>,
    /// PR key → its ticket, and how it came (`made`, `found on the PR`, `found on YouTrack`).
    pub tickets: BTreeMap<String, (String, String)>,
    /// PR keys whose ticket is a subtask of its epic.
    pub parented: BTreeSet<String>,
    /// PR keys whose PR has the comment linking its ticket.
    pub commented: BTreeSet<String>,
}

pub fn done_path(plan: &Path) -> PathBuf {
    plan.with_extension("done.json")
}

/// `owner/repo#12` as (repo, number).
pub fn parse_key(k: &str) -> Option<(String, u64)> {
    let (repo, n) = k.trim().rsplit_once('#')?;
    let n = n.parse::<u64>().ok().filter(|n| *n > 0)?;
    (repo.contains('/') && !repo.contains(' ')).then(|| (repo.to_string(), n))
}

/// A plan anything may be made from: every key readable, every PR in it
/// once (in one epic or as one task), no release PR also given a ticket,
/// every epic with its summary and project (or an id to reuse).
pub fn check(plan: &Plan) -> Result<()> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut problems = vec![];
    let mut refs = BTreeSet::new();
    let mut take = |k: &str, whose: String, problems: &mut Vec<String>| {
        if parse_key(k).is_none() {
            problems.push(format!("{k:?} ({whose}) isn't owner/repo#number"));
        } else if let Some(first) = seen.insert(k.to_string(), whose.clone()) {
            problems.push(format!("{k} is in {first} and in {whose}: one ticket per PR"));
        }
    };
    for e in &plan.epics {
        if e.reference.trim().is_empty() || !refs.insert(e.reference.clone()) {
            problems.push(format!("an epic needs a ref of its own ({:?})", e.reference));
        }
        if e.id.trim().is_empty() && (e.summary.trim().is_empty() || e.project.trim().is_empty()) {
            problems.push(format!("epic {} needs a summary and a project, or the id of one to reuse", e.reference));
        }
        if e.prs.is_empty() {
            problems.push(format!("epic {} has no PRs", e.reference));
        }
        for k in &e.prs {
            take(k, format!("epic {}", e.reference), &mut problems);
        }
    }
    for t in &plan.tasks {
        take(&t.pr, "the tasks".into(), &mut problems);
    }
    for k in plan.epics.iter().flat_map(|e| &e.releases).chain(plan.tasks.iter().flat_map(|t| &t.releases)) {
        if seen.contains_key(k) {
            problems.push(format!("{k} is a release PR and has a ticket too"));
        }
        if parse_key(k).is_none() {
            problems.push(format!("release {k:?} isn't owner/repo#number"));
        }
    }
    if !problems.is_empty() {
        bail!("the plan isn't one to make tickets from:\n- {}", problems.join("\n- "));
    }
    Ok(())
}

/// A PR's link (`https://github.com/<repo>/pull/<n>`).
pub fn url_of(key: &str) -> String {
    parse_key(key).map(|(r, n)| format!("https://github.com/{r}/pull/{n}")).unwrap_or_default()
}

/// The releases line under a ticket's or an epic's description.
fn shipped_line(releases: &[String]) -> String {
    if releases.is_empty() {
        return String::new();
    }
    let list: Vec<String> = releases.iter().map(|k| format!("[{k}]({})", url_of(k))).collect();
    format!("\n\n**Shipped in:** {}", list.join(", "))
}

/// How an epic's description ends: what tells it from a PR's ticket, though it links the PRs too.
pub const EPIC_MARK: &str = "_Grouped by Herder Tower: one ticket per PR, under this epic._";

/// YouTrack's search results without the epics (by id, or by their mark):
/// they link their PRs, and are no PR's ticket.
fn without_epics(found: Vec<Value>, epics: &BTreeSet<String>) -> Vec<Value> {
    found.into_iter().filter(|i| !i["idReadable"].as_str().is_some_and(|id| epics.contains(id)) && !i["description"].as_str().unwrap_or("").contains(EPIC_MARK)).collect()
}

/// The epic's description: what the feature is, its PRs, the releases that ship it.
pub fn epic_description(e: &EpicPlan) -> String {
    let prs: Vec<String> = e.prs.iter().map(|k| format!("- [{k}]({})", url_of(k))).collect();
    format!(
        "{}\n\n**Its pull requests** (each a subtask):\n{}{}\n\n{EPIC_MARK}",
        e.description.trim(),
        prs.join("\n"),
        shipped_line(&e.releases),
    )
}

/// What `POST /api/issues` is sent for a new epic.
pub fn epic_body(project_id: &str, e: &EpicPlan) -> Value {
    json!({
        "project": { "id": project_id },
        "summary": e.summary.trim(),
        "description": epic_description(e),
        "customFields": [{ "name": "Type", "$type": "SingleEnumIssueCustomField", "value": { "name": "Epic" } }],
    })
}

/// The issue's `Type`, as YouTrack answers it.
pub fn type_of(issue: &Value) -> String {
    issue["customFields"].as_array().into_iter().flatten().find(|f| f["name"] == "Type").and_then(|f| f["value"]["name"].as_str()).unwrap_or("").to_string()
}

/// The epics (parents) YouTrack says an issue is a subtask of.
pub fn parents_of(issue: &Value) -> Vec<String> {
    issue["parent"]["issues"].as_array().into_iter().flatten().filter_map(|i| i["idReadable"].as_str().map(String::from)).collect()
}

/// What backfill asks beyond what tickets do.
pub trait Backend: Remote {
    /// A YouTrack command on issues (`subtask of EYF-12`).
    fn command(&self, query: &str, ids: &[&str]) -> Result<()>;
    /// One issue: `idReadable`, `project(shortName)`, `customFields`, `parent`, `description`.
    fn issue(&self, id: &str) -> Result<Value>;
    /// The PR as `gh pr view --json` has it (`crate::prs::FIELDS`).
    fn pr(&self, repo: &str, number: u64) -> Result<Value>;
}

impl Backend for youtrack::Real {
    fn command(&self, query: &str, ids: &[&str]) -> Result<()> {
        let issues: Vec<Value> = ids.iter().map(|id| json!({ "idReadable": id })).collect();
        self.call("POST", "commands", Some(&json!({ "query": query, "issues": issues }))).map(|_| ())
    }

    fn issue(&self, id: &str) -> Result<Value> {
        self.call("GET", &format!("issues/{}?fields=idReadable,summary,description,project(shortName),customFields(name,value(name)),parent(issues(idReadable))", youtrack::encode(id)), None)
    }

    fn pr(&self, repo: &str, number: u64) -> Result<Value> {
        crate::prs::fetch(&self.gh, repo, number)
    }
}

/// The PR as the tower knows it (its space and session), brought up to date from GitHub.
fn pr_now(key: &str, known: &BTreeMap<String, Pr>, b: &dyn Backend) -> Result<Pr> {
    let (repo, n) = parse_key(key).with_context(|| format!("{key} isn't owner/repo#number"))?;
    let mut pr = known.get(key).cloned().unwrap_or_else(|| Pr { repo: repo.clone(), number: n, ..Default::default() });
    let v = b.pr(&repo, n).with_context(|| format!("reading {key} on GitHub"))?;
    crate::prs::apply_gh(&mut pr, &v);
    pr.body = v["body"].as_str().map(String::from).or(pr.body);
    pr.repo = repo;
    Ok(pr)
}

/// The PR's ticket, as `youtrack::ensure` makes it (the PR's comment, then
/// its link on YouTrack, else a new one), with the releases that ship it
/// under its description.
fn ticket_for(pr: &Pr, cfg: &YoutrackCfg, b: &dyn Backend, releases: &[String], epics: &BTreeSet<String>, apply: bool) -> Result<Option<Ticket>> {
    let base = youtrack::base(cfg);
    let url = |id: &str| format!("{base}/issue/{id}");
    if let Some((t, _)) = youtrack::linked_in(&b.comments(&pr.repo, pr.number).context("reading the PR's comments")?) {
        return Ok(Some(t));
    }
    let found = without_epics(b.search(&youtrack::search_query(pr)).context("looking for its ticket on YouTrack")?, epics);
    if let Some(id) = youtrack::linked_on_youtrack(&found, &pr.url) {
        return Ok(Some(Ticket { url: url(&id), id, how: "found on YouTrack".into(), ..Default::default() }));
    }
    if !apply {
        return Ok(None);
    }
    let project = b.project_id(&youtrack::project_for(cfg, &pr.repo)).context("finding the project")?;
    let mut body = youtrack::issue_body(cfg, &project, pr, &youtrack::what_it_changes(pr));
    if let Some(d) = body["description"].as_str().map(|d| format!("{d}{}", shipped_line(releases))) {
        body["description"] = json!(d);
    }
    let id = b.create(&body).context("making the ticket")?;
    Ok(Some(Ticket { url: url(&id), id, how: "made".into(), ..Default::default() }))
}

/// Runs the plan (`apply`: writes; else says what it would do). Each step
/// is kept in `done` (and `save`d) as it is taken. The lines it says.
pub fn run(plan: &Plan, done: &mut Done, cfg: &YoutrackCfg, b: &dyn Backend, known: &BTreeMap<String, Pr>, apply: bool, save: &mut dyn FnMut(&Done)) -> Result<Vec<String>> {
    check(plan)?;
    let mut out = vec![];
    // The epics first: their PRs' tickets become their subtasks.
    let mut epic_of: BTreeMap<String, Option<String>> = BTreeMap::new();
    for e in &plan.epics {
        let id = if !e.id.trim().is_empty() {
            let v = b.issue(e.id.trim()).with_context(|| format!("reading the epic {} to reuse", e.id))?;
            if type_of(&v) != "Epic" {
                bail!("{} is a {}, not an Epic: the plan's epic {} can't reuse it", e.id, type_of(&v), e.reference);
            }
            out.push(format!("epic {}: reuses {}", e.reference, e.id.trim()));
            Some(e.id.trim().to_string())
        } else if let Some(id) = done.epics.get(&e.reference) {
            out.push(format!("epic {}: {id} (made before)", e.reference));
            Some(id.clone())
        } else if apply {
            let project = b.project_id(&e.project).with_context(|| format!("finding the project {}", e.project))?;
            let id = b.create(&epic_body(&project, e)).with_context(|| format!("making the epic {}", e.reference))?;
            done.epics.insert(e.reference.clone(), id.clone());
            save(done);
            out.push(format!("epic {}: made {id} in {} \"{}\"", e.reference, e.project, e.summary));
            Some(id)
        } else {
            out.push(format!("epic {}: would make it in {} \"{}\" ({} PRs)", e.reference, e.project, e.summary, e.prs.len()));
            None
        };
        for k in &e.prs {
            epic_of.insert(k.clone(), id.clone());
        }
    }
    let epics: BTreeSet<String> = epic_of.values().flatten().cloned().collect();
    let members: Vec<(String, Option<(String, Option<String>)>, Vec<String>)> = plan
        .epics
        .iter()
        .flat_map(|e| e.prs.iter().map(move |k| (k.clone(), Some((e.reference.clone(), None)), vec![])))
        .chain(plan.tasks.iter().map(|t| (t.pr.clone(), None, t.releases.clone())))
        .collect();
    for (key, epic, releases) in members {
        let epic_id = epic.as_ref().and_then(|_| epic_of.get(&key).cloned().flatten());
        let pr = pr_now(&key, known, b)?;
        let t = match done.tickets.get(&key) {
            Some((id, how)) => Some(Ticket { url: format!("{}/issue/{id}", youtrack::base(cfg)), id: id.clone(), how: how.clone(), ..Default::default() }),
            None => ticket_for(&pr, cfg, b, &releases, &epics, apply)?,
        };
        let Some(t) = t else {
            out.push(format!("{key}: would make its ticket in {}{}", youtrack::project_for(cfg, &pr.repo), epic.as_ref().map(|(r, _)| format!(", a subtask of {r}")).unwrap_or_default()));
            continue;
        };
        if !done.tickets.contains_key(&key) {
            done.tickets.insert(key.clone(), (t.id.clone(), t.how.clone()));
            save(done);
        }
        let mut did = vec![format!("{} {}", t.how, t.id)];
        if let Some(eid) = &epic_id
            && !done.parented.contains(&key)
        {
            let now = b.issue(&t.id).with_context(|| format!("reading {}", t.id))?;
            if parents_of(&now).iter().any(|p| p == eid) {
                did.push(format!("a subtask of {eid} already"));
            } else if apply {
                b.command(&format!("subtask of {eid}"), &[&t.id]).with_context(|| format!("making {} a subtask of {eid}", t.id))?;
                did.push(format!("a subtask of {eid}"));
            } else {
                did.push(format!("would be a subtask of {eid}"));
            }
            if apply {
                done.parented.insert(key.clone());
                save(done);
            }
        }
        if cfg.link_pr && !done.commented.contains(&key) {
            if apply {
                youtrack::link(&pr, &t, b).with_context(|| format!("linking {} on {key}", t.id))?;
                done.commented.insert(key.clone());
                save(done);
                did.push("linked on the PR".into());
            } else {
                did.push("would be linked on the PR".into());
            }
        }
        out.push(format!("{key}: {}", did.join(", ")));
    }
    Ok(out)
}

/// Reads every ticket back. The lines it says and how many problems it found.
pub fn verify(plan: &Plan, done: &Done, cfg: &YoutrackCfg, b: &dyn Backend) -> Result<(Vec<String>, usize)> {
    check(plan)?;
    let mut out = vec![];
    let mut bad = 0;
    let mut epic_ids: BTreeMap<String, String> = BTreeMap::new();
    for e in &plan.epics {
        let id = if e.id.trim().is_empty() { done.epics.get(&e.reference).cloned() } else { Some(e.id.trim().to_string()) };
        let Some(id) = id else {
            out.push(format!("PROBLEM epic {}: never made", e.reference));
            bad += 1;
            continue;
        };
        let v = b.issue(&id)?;
        let (project, ty) = (v["project"]["shortName"].as_str().unwrap_or("").to_string(), type_of(&v));
        let ok = ty == "Epic" && (e.project.is_empty() || !e.id.is_empty() || project.eq_ignore_ascii_case(&e.project));
        bad += usize::from(!ok);
        out.push(format!("{} epic {} {id}: {project}, {ty}", if ok { "ok" } else { "PROBLEM" }, e.reference));
        epic_ids.insert(e.reference.clone(), id);
    }
    let members: Vec<(String, Option<String>)> = plan
        .epics
        .iter()
        .flat_map(|e| e.prs.iter().map(|k| (k.clone(), epic_ids.get(&e.reference).cloned())))
        .chain(plan.tasks.iter().map(|t| (t.pr.clone(), None)))
        .collect();
    let epics: BTreeSet<String> = epic_ids.values().cloned().collect();
    let mut ids_seen: BTreeMap<String, String> = BTreeMap::new();
    for (key, epic) in members {
        let Some((id, _)) = done.tickets.get(&key) else {
            out.push(format!("PROBLEM {key}: no ticket"));
            bad += 1;
            continue;
        };
        let (repo, n) = parse_key(&key).unwrap_or_default();
        let url = url_of(&key);
        let v = b.issue(id)?;
        let project = v["project"]["shortName"].as_str().unwrap_or("").to_string();
        let ty = type_of(&v);
        let parents = parents_of(&v);
        // Exactly one ticket on YouTrack links it, and one comment on the PR links that one.
        let linking: Vec<String> = without_epics(b.search(&format!("\"{url}\""))?, &epics).iter().filter(|i| youtrack::names_pr(i["description"].as_str().unwrap_or(""), &url)).filter_map(|i| i["idReadable"].as_str().map(String::from)).collect();
        let comments: Vec<String> = b.comments(&repo, n)?.iter().filter_map(|(_, body)| youtrack::linked_in(&[(0, body.clone())]).map(|(t, _)| t.id)).collect();
        let mut why = vec![];
        if !project.eq_ignore_ascii_case(&youtrack::project_for(cfg, &repo)) {
            why.push(format!("in {project}, not {}", youtrack::project_for(cfg, &repo)));
        }
        if !cfg.issue_type.trim().is_empty() && ty != cfg.issue_type.trim() {
            why.push(format!("a {ty}, not a {}", cfg.issue_type.trim()));
        }
        match &epic {
            Some(e) if !parents.contains(e) => why.push(format!("not a subtask of {e} ({})", if parents.is_empty() { "none".into() } else { parents.join(", ") })),
            None if !parents.is_empty() => why.push(format!("a subtask of {}, but it stands alone", parents.join(", "))),
            _ => {}
        }
        if linking != [id.clone()] {
            why.push(format!("{} tickets link the PR ({})", linking.len(), linking.join(", ")));
        }
        if cfg.link_pr && comments != [id.clone()] {
            why.push(format!("{} comments on the PR link a ticket ({})", comments.len(), comments.join(", ")));
        }
        if let Some(other) = ids_seen.insert(id.clone(), key.clone()) {
            why.push(format!("{id} is {other}'s ticket too"));
        }
        bad += usize::from(!why.is_empty());
        let epic_word = epic.map(|e| format!(", subtask of {e}")).unwrap_or_default();
        out.push(format!("{} {key} {id}: {project}, {ty}{epic_word}{}", if why.is_empty() { "ok" } else { "PROBLEM" }, if why.is_empty() { String::new() } else { format!(": {}", why.join("; ")) }));
    }
    Ok((out, bad))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// YouTrack and GitHub in memory.
    #[derive(Default)]
    struct Fake {
        issues: RefCell<BTreeMap<String, Value>>,
        comments: RefCell<BTreeMap<String, Vec<(u64, String)>>>,
        made: RefCell<u32>,
    }

    impl Remote for Fake {
        fn project_id(&self, project: &str) -> Result<String> {
            Ok(format!("id-{project}"))
        }
        fn search(&self, query: &str) -> Result<Vec<Value>> {
            let url = query.trim_matches('"');
            Ok(self.issues.borrow().values().filter(|i| i["description"].as_str().unwrap_or("").contains(url)).cloned().collect())
        }
        fn create(&self, body: &Value) -> Result<String> {
            *self.made.borrow_mut() += 1;
            let project = body["project"]["id"].as_str().unwrap_or("").trim_start_matches("id-").to_string();
            let id = format!("{project}-{}", 100 + self.issues.borrow().len());
            let mut v = body.clone();
            v["idReadable"] = json!(id);
            v["project"] = json!({ "shortName": project });
            if v["customFields"].is_null() {
                v["customFields"] = json!([]);
            }
            self.issues.borrow_mut().insert(id.clone(), v);
            Ok(id)
        }
        fn delete(&self, id: &str) -> Result<()> {
            self.issues.borrow_mut().remove(id);
            Ok(())
        }
        fn comments(&self, repo: &str, number: u64) -> Result<Vec<(u64, String)>> {
            Ok(self.comments.borrow().get(&format!("{repo}#{number}")).cloned().unwrap_or_default())
        }
        fn comment(&self, repo: &str, number: u64, body: &str) -> Result<u64> {
            let mut c = self.comments.borrow_mut();
            let list = c.entry(format!("{repo}#{number}")).or_default();
            list.push((list.len() as u64 + 1, body.to_string()));
            Ok(list.len() as u64)
        }
        fn delete_comment(&self, _: &str, _: u64) -> Result<()> {
            Ok(())
        }
    }

    impl Backend for Fake {
        fn command(&self, query: &str, ids: &[&str]) -> Result<()> {
            let epic = query.strip_prefix("subtask of ").context("only subtask of")?;
            for id in ids {
                let mut all = self.issues.borrow_mut();
                let i = all.get_mut(*id).context("no such issue")?;
                i["parent"] = json!({ "issues": [{ "idReadable": epic }] });
            }
            Ok(())
        }
        fn issue(&self, id: &str) -> Result<Value> {
            self.issues.borrow().get(id).cloned().with_context(|| format!("no {id}"))
        }
        fn pr(&self, repo: &str, number: u64) -> Result<Value> {
            Ok(json!({ "number": number, "url": format!("https://github.com/{repo}/pull/{number}"), "title": format!("Change {number}"), "state": "MERGED", "baseRefName": "dev", "headRefName": format!("feat/{number}"), "headRefOid": "abc", "body": format!("What {number} does.") }))
        }
    }

    fn cfg() -> YoutrackCfg {
        let mut c = YoutrackCfg { url: "https://yt.example/youtrack".into(), project: "EYF".into(), issue_type: "Task".into(), link_pr: true, ..Default::default() };
        c.repo_projects.insert("shop-web".into(), "SOO".into());
        c
    }

    fn plan() -> Plan {
        Plan {
            epics: vec![EpicPlan {
                reference: "videos".into(),
                project: "EYF".into(),
                summary: "Support inbox videos".into(),
                description: "Viewers and the team send videos in support chats.".into(),
                prs: vec!["o/api#12".into(), "o/shop-web#3".into()],
                releases: vec!["o/api#13".into()],
                ..Default::default()
            }],
            tasks: vec![TaskPlan { pr: "o/api#20".into(), releases: vec!["o/api#21".into()] }],
        }
    }

    #[test]
    fn a_plan_makes_one_epic_and_one_ticket_per_pr_as_subtasks_once() {
        let (b, c, p) = (Fake::default(), cfg(), plan());
        let mut done = Done::default();
        let mut saves = 0;
        // A dry run writes nothing.
        let said = run(&p, &mut done, &c, &b, &BTreeMap::new(), false, &mut |_| saves += 1).unwrap();
        assert_eq!((*b.made.borrow(), saves, b.comments.borrow().len()), (0, 0, 0), "{said:?}");
        assert!(said[0].starts_with("epic videos: would make it in EYF") && said[1].starts_with("o/api#12: would make its ticket in EYF, a subtask of videos"), "{said:?}");
        // Applied: the epic, then each PR's ticket in its repo's project, a subtask of it, linked on its PR.
        let said = run(&p, &mut done, &c, &b, &BTreeMap::new(), true, &mut |_| saves += 1).unwrap();
        assert_eq!(*b.made.borrow(), 4, "{said:?}");
        let epic = done.epics["videos"].clone();
        let e = b.issue(&epic).unwrap();
        assert_eq!((type_of(&e), e["summary"].as_str()), ("Epic".into(), Some("Support inbox videos")));
        let ed = e["description"].as_str().unwrap();
        assert!(ed.starts_with("Viewers and the team send videos") && ed.contains("- [o/shop-web#3](https://github.com/o/shop-web/pull/3)") && ed.contains("**Shipped in:** [o/api#13](https://github.com/o/api/pull/13)"), "{ed}");
        let (api, _) = done.tickets["o/api#12"].clone();
        let (web, _) = done.tickets["o/shop-web#3"].clone();
        let (task, _) = done.tickets["o/api#20"].clone();
        assert!(api.starts_with("EYF-") && web.starts_with("SOO-") && task.starts_with("EYF-"), "{api} {web} {task}");
        // The tower's own ticket: its summary, description and type.
        let t = b.issue(&api).unwrap();
        assert_eq!((t["summary"].as_str(), type_of(&t)), (Some("Change 12"), "Task".into()));
        assert!(t["description"].as_str().unwrap().starts_with("**Pull request:** [o/api#12](https://github.com/o/api/pull/12)"));
        assert_eq!((parents_of(&t), parents_of(&b.issue(&web).unwrap()), parents_of(&b.issue(&task).unwrap())), (vec![epic.clone()], vec![epic.clone()], vec![]));
        assert!(b.issue(&task).unwrap()["description"].as_str().unwrap().ends_with("**Shipped in:** [o/api#21](https://github.com/o/api/pull/21)"));
        assert!(b.comments.borrow()["o/api#12"][0].1.contains(&format!("<!-- herder-tower-youtrack {api} https://yt.example/youtrack/issue/{api} -->")));
        assert!(!b.comments.borrow().contains_key("o/api#13"), "a release PR gets no ticket and no comment");
        // Run again (or after a stop): nothing new, nothing twice.
        run(&p, &mut done, &c, &b, &BTreeMap::new(), true, &mut |_| {}).unwrap();
        assert_eq!((*b.made.borrow(), b.comments.borrow()["o/api#12"].len()), (4, 1));
        // A fresh record (the done file lost): each ticket is found on its PR, not made again.
        let mut lost = Done { epics: done.epics.clone(), ..Default::default() };
        let said = run(&p, &mut lost, &c, &b, &BTreeMap::new(), true, &mut |_| {}).unwrap();
        assert_eq!(*b.made.borrow(), 4, "{said:?}");
        assert!(said.iter().any(|l| l.starts_with("o/api#12: found on the PR") && l.contains("a subtask of EYF-100 already")), "{said:?}");
        // Read back: every PR exactly one ticket, where it belongs.
        let (lines, bad) = verify(&p, &done, &c, &b).unwrap();
        assert_eq!(bad, 0, "{lines:?}");
        assert_eq!(lines.len(), 4);
        // A second ticket linking a PR is a problem the check names.
        b.create(&json!({ "project": { "id": "id-EYF" }, "summary": "dup", "description": "https://github.com/o/api/pull/12" })).unwrap();
        let (lines, bad) = verify(&p, &done, &c, &b).unwrap();
        assert_eq!(bad, 1);
        assert!(lines.iter().any(|l| l.starts_with("PROBLEM o/api#12") && l.contains("2 tickets link the PR")), "{lines:?}");
    }

    #[test]
    fn an_epic_youtrack_has_is_reused_and_a_bad_plan_makes_nothing() {
        let (b, c) = (Fake::default(), cfg());
        let have = b.create(&json!({ "project": { "id": "id-EYF" }, "summary": "Old epic", "description": "", "customFields": [{ "name": "Type", "value": { "name": "Epic" } }] })).unwrap();
        let task = b.create(&json!({ "project": { "id": "id-EYF" }, "summary": "A task", "description": "", "customFields": [{ "name": "Type", "value": { "name": "Task" } }] })).unwrap();
        let mut p = plan();
        p.epics[0].id = have.clone();
        let mut done = Done::default();
        run(&p, &mut done, &c, &b, &BTreeMap::new(), true, &mut |_| {}).unwrap();
        assert!(done.epics.is_empty());
        assert_eq!(parents_of(&b.issue(&done.tickets["o/api#12"].0).unwrap()), vec![have.clone()]);
        assert_eq!(verify(&p, &done, &c, &b).unwrap().1, 0);
        // Not an epic: refused.
        p.epics[0].id = task;
        assert!(run(&p, &mut Done::default(), &c, &b, &BTreeMap::new(), true, &mut |_| {}).unwrap_err().to_string().contains("not an Epic"));
        // A PR twice, a release with a ticket, a bad key: nothing is made.
        let before = *b.made.borrow();
        let mut bad = plan();
        bad.tasks.push(TaskPlan { pr: "o/api#12".into(), releases: vec![] });
        bad.tasks.push(TaskPlan { pr: "o/api#13".into(), releases: vec![] });
        bad.tasks.push(TaskPlan { pr: "nope".into(), releases: vec![] });
        let e = run(&bad, &mut Done::default(), &c, &b, &BTreeMap::new(), true, &mut |_| {}).unwrap_err().to_string();
        assert!(e.contains("o/api#12 is in epic videos and in the tasks") && e.contains("o/api#13 is a release PR and has a ticket too") && e.contains("\"nope\""), "{e}");
        assert_eq!(*b.made.borrow(), before);
    }
}
