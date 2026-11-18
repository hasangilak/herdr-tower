//! The YouTrack tickets of the PRs sessions open (`crate::youtrack`), made
//! in the daemon: every 20 s while `[youtrack] enabled` is on (read as it
//! goes: `tower youtrack off` stops it without a restart), and at once on
//! `tower youtrack now`. One pass at a time (`PASS`), so two never make a
//! ticket for the same PR. A PR that failed is tried again five minutes on.

use std::collections::HashMap;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::YoutrackCfg;
use crate::prs::Prs;
use crate::youtrack::{self, Need, Remote};

const EVERY: Duration = Duration::from_secs(20);
const RETRY_MS: u64 = 5 * 60_000;

/// One pass at a time; and the PRs that failed: key → not before.
static PASS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// What one PR's turn came to, for the log and `tower youtrack now`.
fn line(key: &str, what: &str) -> Value {
    json!({ "pr": key, "did": what })
}

/// What each PR due now needs, done. `summarize`: the merge card's summary
/// of a description that has none yet (a model call). `force`: failed ones
/// too, now. `only`: that PR alone.
pub fn pass(prs: &Prs, cfg: &YoutrackCfg, remote: &dyn Remote, summarize: &dyn Fn(&crate::prs::Pr) -> Option<String>, force: bool) -> Vec<Value> {
    pass_of(prs, cfg, remote, summarize, force, None)
}

pub fn pass_of(prs: &Prs, cfg: &YoutrackCfg, remote: &dyn Remote, summarize: &dyn Fn(&crate::prs::Pr) -> Option<String>, force: bool, only: Option<&str>) -> Vec<Value> {
    let mut guard = PASS.lock().unwrap_or_else(|e| e.into_inner());
    let failed = guard.get_or_insert_with(HashMap::new);
    let now = crate::history::now_ms();
    let mut out = vec![];
    for (k, need) in youtrack::due(&prs.list(), cfg, now) {
        if only.is_some_and(|o| o != k) {
            continue;
        }
        if !force && failed.get(&k).is_some_and(|until| *until > now) {
            continue;
        }
        let Some(pr) = prs.get(&k) else { continue };
        let result = match need {
            Need::Ticket => {
                let body = pr.body.as_deref().unwrap_or("").trim();
                if !body.is_empty() && crate::describe::needs_summary(&pr) && let Some(summary) = summarize(&pr) {
                    prs.set_described(&k, crate::describe::Described { summary, of: crate::describe::hash(body) });
                }
                let pr = prs.get(&k).unwrap_or(pr);
                youtrack::ensure(&pr, cfg, remote, &youtrack::what_it_changes(&pr), now).map(|t| {
                    let did = format!("{} {} ({})", t.how, t.id, t.url);
                    prs.set_ticket(&k, Some(t.clone()));
                    // The link back, at once (or at the next pass).
                    let linked = cfg.link_pr && t.comment == 0 && pr.open();
                    match linked.then(|| youtrack::link(&pr, &t, remote)) {
                        Some(Ok(cid)) => {
                            prs.set_ticket(&k, Some(youtrack::Ticket { comment: cid, ..t }));
                            format!("{did}, linked on the PR")
                        }
                        Some(Err(e)) => format!("{did}; the PR's link to it: not yet ({e:#})"),
                        None => did,
                    }
                })
            }
            Need::Link => {
                let t = pr.ticket.clone().unwrap_or_default();
                youtrack::link(&pr, &t, remote).map(|cid| {
                    prs.set_ticket(&k, Some(youtrack::Ticket { comment: cid, ..t.clone() }));
                    format!("linked {} on the PR", t.id)
                })
            }
        };
        match result {
            Ok(did) => {
                failed.remove(&k);
                eprintln!("[youtrack] {k}: {did}");
                out.push(line(&k, &did));
            }
            Err(e) => {
                failed.insert(k.clone(), now + RETRY_MS);
                eprintln!("[youtrack] {k}: no ticket yet: {e:#}");
                out.push(line(&k, &format!("failed: {e:#}")));
            }
        }
    }
    out
}

/// The settings and the token, when tickets are on and nothing is missing.
pub fn ready(cfg: &YoutrackCfg) -> Result<String, String> {
    if !cfg.enabled {
        return Err("tickets are off (`tower youtrack on`)".into());
    }
    let token = youtrack::token();
    match youtrack::missing(cfg, token.is_some()) {
        Some(m) => Err(format!("YouTrack isn't set up: {m}")),
        None => Ok(token.unwrap_or_default()),
    }
}

fn summarize(pr: &crate::prs::Pr) -> Option<String> {
    crate::describe::summarize(pr).map_err(|e| eprintln!("[youtrack] {}: no summary, its description's opening instead: {e:#}", pr.key())).ok()
}

/// A pass now, as asked (`tower youtrack now`).
pub fn now(prs: &Prs) -> Result<Vec<Value>, String> {
    let cfg = crate::config::Config::live().youtrack;
    let token = ready(&cfg)?;
    Ok(pass(prs, &cfg, &youtrack::Real::new(&cfg, token), &summarize, true))
}

/// One PR's ticket now, with tickets on or off (`tower youtrack try <PR>
/// [--no-link]`): to see it work before switching it on.
pub fn try_one(prs: &Prs, what: &str, link: bool) -> Result<Vec<Value>, String> {
    let mut cfg = crate::config::Config::live().youtrack;
    cfg.enabled = true;
    cfg.link_pr = cfg.link_pr && link;
    let token = ready(&cfg)?;
    let all = prs.list();
    let key = crate::prs::links(what).first().map(|(r, n)| crate::prs::key(r, *n)).unwrap_or_else(|| what.trim().to_string());
    let pr = all.iter().find(|p| p.key().eq_ignore_ascii_case(&key)).ok_or_else(|| format!("the tower doesn't know {what} (owner/name#12 or its link)"))?;
    if !youtrack::owner_wanted(&cfg, &pr.repo) {
        return Err(format!("{} isn't in {}'s repos", pr.key(), cfg.github_owners.join(", ")));
    }
    if pr.ticket.as_ref().is_some_and(|t| !t.id.is_empty() && (t.comment != 0 || !cfg.link_pr)) {
        return Ok(vec![line(&pr.key(), &format!("has {} already", pr.ticket.as_ref().map(|t| t.id.as_str()).unwrap_or("")))]);
    }
    if !pr.open() {
        return Err(format!("{} is {}: only an open PR gets a ticket", pr.key(), pr.state));
    }
    let k = pr.key();
    // Its description not looked at yet: taken as it is.
    let mut cfg_now = cfg.clone();
    cfg_now.github_owners = vec![k.split('/').next().unwrap_or("").to_string()];
    let did = pass_of(prs, &cfg_now, &youtrack::Real::new(&cfg, token), &summarize, true, Some(&k));
    if did.is_empty() {
        return Err(format!("{k} isn't due a ticket yet (the tower hasn't read its description: try again in a minute)"));
    }
    Ok(did)
}

/// Takes back a ticket the tower made (`tower youtrack remove <id>`): the
/// ticket deleted on YouTrack, its comment off the PR, forgotten by the PR.
/// Only one the tower keeps with a PR: nothing else on YouTrack is touched.
pub fn remove(prs: &Prs, id: &str) -> Result<Value, String> {
    let cfg = crate::config::Config::live().youtrack;
    let token = youtrack::token().ok_or("no YouTrack token (`tower youtrack token`)")?;
    let all = prs.list();
    let pr = youtrack::pr_of_ticket(&all, id).ok_or_else(|| format!("{id} isn't a ticket the tower made for a PR"))?;
    let t = pr.ticket.clone().unwrap_or_default();
    let remote = youtrack::Real::new(&cfg, token);
    let _guard = PASS.lock().unwrap_or_else(|e| e.into_inner());
    remote.delete(&t.id).map_err(|e| format!("couldn't delete {}: {e:#}", t.id))?;
    let comment = if t.comment != 0 {
        match remote.delete_comment(&pr.repo, t.comment) {
            Ok(()) => "its comment taken off the PR".to_string(),
            Err(e) => format!("its comment on the PR is still there ({e:#})"),
        }
    } else {
        "no comment on the PR".to_string()
    };
    prs.set_ticket(&pr.key(), None);
    eprintln!("[youtrack] {}: deleted {}, {comment}", pr.key(), t.id);
    Ok(json!({ "pr": pr.key(), "deleted": t.id, "comment": comment }))
}

pub fn start(prs: Prs) {
    let spawned = thread::Builder::new().name("youtrack".into()).spawn(move || {
        let mut said = String::new();
        loop {
            let cfg = crate::config::Config::live().youtrack;
            match ready(&cfg) {
                Ok(token) => {
                    said.clear();
                    pass(&prs, &cfg, &youtrack::Real::new(&cfg, token), &summarize, false);
                }
                // Said once when it changes (it is on but something is missing).
                Err(why) if cfg.enabled && why != said => {
                    eprintln!("[youtrack] no tickets: {why}");
                    said = why;
                }
                Err(_) => {}
            }
            thread::sleep(EVERY);
        }
    });
    if let Err(e) = spawned {
        eprintln!("[youtrack] couldn't start: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prs::Pr;
    use anyhow::{Result, bail};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        made: RefCell<Vec<String>>,
        comments: RefCell<Vec<(u64, String)>>,
        gh_down: std::cell::Cell<bool>,
    }

    impl Remote for Fake {
        fn project_id(&self, _: &str) -> Result<String> {
            Ok("0-1".into())
        }
        fn search(&self, _: &str) -> Result<Vec<Value>> {
            Ok(self.made.borrow().iter().map(|d| json!({ "idReadable": "EYZ-1", "description": d })).collect())
        }
        fn create(&self, body: &Value) -> Result<String> {
            self.made.borrow_mut().push(body["description"].as_str().unwrap_or("").to_string());
            Ok(format!("EYZ-{}", self.made.borrow().len()))
        }
        fn delete(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn comments(&self, _: &str, _: u64) -> Result<Vec<(u64, String)>> {
            Ok(self.comments.borrow().clone())
        }
        fn comment(&self, _: &str, _: u64, body: &str) -> Result<u64> {
            if self.gh_down.get() {
                bail!("gh: HTTP 502");
            }
            self.comments.borrow_mut().push((77, body.into()));
            Ok(77)
        }
        fn delete_comment(&self, _: &str, _: u64) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn passes_make_one_ticket_per_pr_and_link_it_once() {
        let dir = std::env::temp_dir().join(format!("tower-youtrack-pass-{}", std::process::id()));
        let prs = Prs::at(dir.join("prs.json"), "gh".into());
        let open = |n: u64, repo: &str| Pr { repo: repo.into(), number: n, url: format!("https://github.com/{repo}/pull/{n}"), state: "open".into(), title: format!("PR {n}"), body: Some("Does a thing.".into()), session: "t1".into(), ..Default::default() };
        prs.insert_for_test(open(1, "eyzhub/eyzflow"));
        prs.insert_for_test(open(2, "someone/else"));
        prs.insert_for_test(Pr { state: "merged".into(), ..open(3, "eyzhub/eyzflow") });
        let cfg = YoutrackCfg { enabled: true, url: "https://y.test".into(), project: "EYZ".into(), ..Default::default() };
        let fake = Fake::default();
        // GitHub is down when its link is to go on: the ticket is kept, the link waits.
        fake.gh_down.set(true);
        let did = pass(&prs, &cfg, &fake, &|_| Some("It does a thing.".into()), true);
        assert_eq!(did.len(), 1, "{did:?}");
        assert!(did[0]["did"].as_str().unwrap().contains("made EYZ-1"), "{did:?}");
        let t = prs.get("eyzhub/eyzflow#1").unwrap().ticket.unwrap();
        assert_eq!((t.id.as_str(), t.comment), ("EYZ-1", 0));
        // The summary the ticket carries is the merge card's, now kept with the PR.
        assert!(fake.made.borrow()[0].contains("It does a thing."));
        assert_eq!(prs.get("eyzhub/eyzflow#1").unwrap().described.unwrap().summary, "It does a thing.");
        // Next pass: only the link, no second ticket.
        fake.gh_down.set(false);
        let did = pass(&prs, &cfg, &fake, &|_| None, true);
        assert_eq!(did[0]["did"], "linked EYZ-1 on the PR");
        assert_eq!(prs.get("eyzhub/eyzflow#1").unwrap().ticket.unwrap().comment, 77);
        // And then nothing, however often.
        for _ in 0..3 {
            assert!(pass(&prs, &cfg, &fake, &|_| None, true).is_empty());
        }
        assert_eq!((fake.made.borrow().len(), fake.comments.borrow().len()), (1, 1));
        // A poll's fresh look at the PR (`gh pr view`) keeps its ticket.
        prs.insert_for_test(Pr { ticket: None, title: "PR 1, renamed".into(), ..prs.get("eyzhub/eyzflow#1").unwrap() });
        assert_eq!(prs.get("eyzhub/eyzflow#1").unwrap().ticket.map(|t| t.id).as_deref(), Some("EYZ-1"));
        assert!(pass(&prs, &cfg, &fake, &|_| None, true).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn off_or_unset_makes_none() {
        assert!(ready(&YoutrackCfg::default()).unwrap_err().contains("off"));
        let on = YoutrackCfg { enabled: true, ..Default::default() };
        assert!(ready(&on).unwrap_err().contains("isn't set up"));
    }
}
