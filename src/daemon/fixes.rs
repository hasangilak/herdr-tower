//! In the daemon: each PR that waits for the user's merge and conflicts
//! with its base (or fell behind it) has its session told to fix it, once
//! for each new conflict, through the queue (`crate::fixes`). A session
//! that is gone: another of the space working in the PR's repo, else one
//! started there for it. Nobody to ask, the session saying it can't, or two
//! tries that didn't fix it: it is the user's, on its merge card, once.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};

use super::sendq::SendQueue;
use crate::fixes::{self, Act, Fix};
use crate::prs::{Pr, Prs};

/// How often the PRs are looked at (each poll of GitHub changes them at most every 30 s).
const EVERY: Duration = Duration::from_secs(10);

/// What the fixer needs of Herdr, the queue and the merge chains.
pub trait Hands: Send + Sync {
    /// The session is there to be told (not ended, not gone).
    fn alive(&self, address: &str) -> bool;
    /// Another session of `space` working in `repo` (`owner/name`): its address and name.
    fn find(&self, space: &str, repo: &str) -> Option<(String, String)>;
    /// A session started in `space` in `repo`'s folder, with `task`: its address and name.
    fn start(&self, space: &str, repo: &str, task: &str) -> Result<(String, String)>;
    /// `text` into the session's queue.
    fn tell(&self, address: &str, text: &str) -> Result<()>;
    /// The user's "Merge all N" is merging it (and asks its session itself).
    fn in_chain(&self, key: &str) -> bool;
}

/// One look at every PR: what each needs, done.
pub fn tick(prs: &Prs, hands: &dyn Hands, now: u64) {
    for pr in prs.list() {
        // Only those handed to the user (another is still its session's work), and none a chain merges.
        if !pr.waiting || pr.session.is_empty() && pr.fix.is_none() || hands.in_chain(&pr.key()) {
            continue;
        }
        let k = pr.key();
        match fixes::step(&pr, now) {
            Act::Nothing => {}
            Act::Clear => {
                eprintln!("[fixes] {k} merges cleanly again");
                prs.set_fix(&k, None);
            }
            Act::Back(why) => {
                eprintln!("[fixes] {k} is the user's: {why}");
                let mut f = pr.fix.clone().unwrap_or_default();
                f.back = Some(why);
                prs.set_fix(&k, Some(f));
            }
            Act::Ask { what, tries } => prs.set_fix(&k, Some(ask(&pr, what, tries, hands, now))),
        }
    }
}

/// Tells the PR's session (else another of its space in its repo, else one
/// started for it) to fix `what`: the fix as it now stands.
fn ask(pr: &Pr, what: &str, tries: u32, hands: &dyn Hands, now: u64) -> Fix {
    let k = pr.key();
    let text = fixes::ask_text(pr, what, tries, &k);
    let mut fix = Fix { what: what.into(), head_asked: pr.head_sha.clone(), asked_ms: now, tries, ..Default::default() };
    // Whom: the one asked last time if still there, else the PR's own, else another in its repo.
    let asked_before = pr.fix.as_ref().map(|f| (f.to.clone(), f.to_name.clone())).filter(|(a, _)| !a.is_empty());
    let own = (!pr.session.is_empty()).then(|| (pr.session.clone(), pr.name.clone()));
    let space = pr.space.clone().unwrap_or_default();
    let there = asked_before.into_iter().chain(own).find(|(a, _)| hands.alive(a)).or_else(|| (!space.is_empty()).then(|| hands.find(&space, &pr.repo)).flatten());
    let told = match there {
        Some((address, name)) => hands.tell(&address, &text).map(|_| (address, name)),
        None if space.is_empty() => Err(anyhow::anyhow!("it has no space to start a session in")),
        None => hands.start(&space, &pr.repo, &format!("You take over PR #{n}: check out its branch first (`gh pr checkout {n} --repo {repo}`). {text}", n = pr.number, repo = pr.repo)),
    };
    match told {
        Ok((address, name)) => {
            eprintln!("[fixes] {k} {}: asked {address} to fix it (try {tries})", if what == "conflicts" { "conflicts" } else { "is behind" });
            (fix.to, fix.to_name) = (address, name);
        }
        Err(e) => {
            eprintln!("[fixes] {k}: nobody could be asked to fix it: {e:#}");
            fix.back = Some(format!("nobody could be asked to fix it ({e:#})"));
        }
    }
    fix
}

/// The session said it can't resolve it (`tower pr cant-fix`): the user's, once, with its words.
pub fn cant_fix(prs: &Prs, key: &str, why: &str, by: &str) -> Result<Pr> {
    let Some(pr) = prs.get(key) else { bail!("the tower doesn't know {key}") };
    if fixes::trouble(&pr).is_none() {
        bail!("{key} has no conflicts with {} the tower knows of: nothing to give up on", pr.base);
    }
    let mut f = pr.fix.clone().unwrap_or_else(|| Fix { what: fixes::trouble(&pr).unwrap_or("conflicts").into(), to: by.into(), ..Default::default() });
    let why = why.trim();
    f.back = Some(if why.is_empty() { "its session says it can't resolve it".into() } else { format!("its session can't resolve it: {why}") });
    prs.set_fix(key, Some(f));
    eprintln!("[fixes] {key}: {by} can't fix it, the user's now");
    Ok(prs.get(key).unwrap_or(pr))
}

/// Herdr, the queue and the chains, for real.
pub struct Real {
    pub sendq: SendQueue,
}

impl Hands for Real {
    fn alive(&self, address: &str) -> bool {
        crate::hc::is_live(address)
    }

    fn find(&self, space: &str, repo: &str) -> Option<(String, String)> {
        let want = crate::prscore::folder_name(repo).to_lowercase();
        crate::missions::scan(&|a| crate::hc::get(a), &|_| None)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.space.trim().eq_ignore_ascii_case(space.trim()))
            .find(|s| {
                crate::hc::get(&s.address)
                    .and_then(|t| super::reviews::repo_of_terminal(&t))
                    .is_some_and(|r| r.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase() == want))
            })
            .map(|s| (s.address, s.name))
    }

    fn start(&self, space: &str, repo: &str, task: &str) -> Result<(String, String)> {
        let Some(folder) = folder_of(space, repo) else { bail!("no folder of {repo} on this Mac") };
        let name = format!("{space}-{}-fix", crate::prscore::folder_name(repo));
        let folder_s = folder.display().to_string();
        let mut args: Vec<String> = ["start", "--no-act", "--name", &name, "--cwd", &folder_s, "--space", space].map(String::from).to_vec();
        // The space's own worktree of the repo, as its sessions have it.
        if is_worktree(&folder) {
            args.push("--no-worktree".into());
        }
        args.push(task.into());
        let mut cmd = std::process::Command::new(crate::config::tower_bin());
        cmd.args(&args);
        let out = crate::hc::output_within(cmd, "tower start", Duration::from_secs(180))?;
        if out.code != 0 {
            bail!("{}{}", out.stderr.trim().trim_start_matches("tower: "), out.stdout.trim());
        }
        let v: serde_json::Value = serde_json::from_str(out.stdout.trim()).map_err(|_| anyhow::anyhow!("tower start printed {}", out.stdout.trim()))?;
        let Some(address) = v["address"].as_str() else { bail!("tower start named no session") };
        Ok((address.to_string(), name))
    }

    fn tell(&self, address: &str, text: &str) -> Result<()> {
        self.sendq.add(address, text, &[]).map(|_| ())
    }

    fn in_chain(&self, key: &str) -> bool {
        super::merges::get().is_some_and(|m| m.holds(key))
    }
}

/// Where a session for `repo` in `space` works: the space's own worktree of
/// it when it has one, else the repo's checkout under the project roots.
fn folder_of(space: &str, repo: &str) -> Option<PathBuf> {
    let name = crate::prscore::folder_name(repo);
    let spaces = crate::spaces::load();
    let place = crate::missions::of(&spaces, space).and_then(|(_, m)| {
        m.worktrees.iter().find(|p| Path::new(&p.repo).file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(name))).map(|p| PathBuf::from(&p.path))
    });
    place.filter(|p| p.is_dir()).or_else(|| crate::projects::best(name).map(|(m, _)| m.path).filter(|p| p.is_dir()))
}

fn is_worktree(folder: &Path) -> bool {
    folder.components().any(|c| c.as_os_str() == ".tower-worktrees")
}

pub fn start(prs: Prs, hands: Arc<dyn Hands>) {
    let spawned = thread::Builder::new().name("fixes".into()).spawn(move || {
        loop {
            tick(&prs, hands.as_ref(), crate::history::now_ms());
            thread::sleep(EVERY);
        }
    });
    if let Err(e) = spawned {
        eprintln!("[fixes] couldn't start: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Sessions that are there or not, and what each was told.
    #[derive(Default)]
    struct Fake {
        alive: Vec<String>,
        other: Option<(String, String)>,
        can_start: bool,
        chain: Vec<String>,
        told: Mutex<Vec<(String, String)>>,
        started: Mutex<Vec<(String, String, String)>>,
    }

    impl Hands for Fake {
        fn alive(&self, a: &str) -> bool {
            self.alive.iter().any(|x| x == a)
        }
        fn find(&self, _: &str, _: &str) -> Option<(String, String)> {
            self.other.clone()
        }
        fn start(&self, space: &str, repo: &str, task: &str) -> Result<(String, String)> {
            if !self.can_start {
                bail!("no folder of {repo} on this Mac");
            }
            self.started.lock().unwrap().push((space.into(), repo.into(), task.into()));
            Ok(("t77".into(), "sooner-eyzflow-fix".into()))
        }
        fn tell(&self, a: &str, text: &str) -> Result<()> {
            self.told.lock().unwrap().push((a.into(), text.into()));
            Ok(())
        }
        fn in_chain(&self, key: &str) -> bool {
            self.chain.iter().any(|k| k == key)
        }
    }

    pub(crate) fn store(name: &str) -> Prs {
        let dir = std::env::temp_dir().join(format!("tower-fixes-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Prs::at(dir.join("prs.json"), PathBuf::from("/nonexistent/gh"))
    }

    pub(crate) fn pr(mergeable: &str, state: &str, sha: &str) -> Pr {
        Pr {
            repo: "eyzhub/eyzflow".into(),
            number: 841,
            base: "dev".into(),
            state: "open".into(),
            mergeable: mergeable.into(),
            merge_state: state.into(),
            head_sha: sha.into(),
            checks: crate::prs::Checks { state: "passing".into(), ..Default::default() },
            session: "t13y".into(),
            name: "sooner eyzflow".into(),
            space: Some("sooner-chatbot-answers".into()),
            waiting: true,
            ..Default::default()
        }
    }

    #[test]
    fn its_own_session_is_told_once_and_gone_another_is_found_or_started() {
        let prs = store("own");
        prs.put(pr("conflicting", "dirty", "a1"), "found");
        let hands = Fake { alive: vec!["t13y".into()], ..Default::default() };
        tick(&prs, &hands, 1_000);
        tick(&prs, &hands, 2_000);
        let told = hands.told.lock().unwrap().clone();
        assert_eq!(told.len(), 1, "once: {told:?}");
        assert_eq!(told[0].0, "t13y");
        let f = prs.get("eyzhub/eyzflow#841").unwrap().fix.unwrap();
        assert_eq!((f.what.as_str(), f.to.as_str(), f.head_asked.as_str(), f.tries, f.back), ("conflicts", "t13y", "a1", 1, None));

        // Its session gone: another of the space in the repo.
        let prs = store("other");
        prs.put(pr("mergeable", "behind", "a1"), "found");
        let hands = Fake { other: Some(("t20".into(), "eyzflow 2".into())), ..Default::default() };
        tick(&prs, &hands, 1_000);
        assert_eq!(hands.told.lock().unwrap()[0].0, "t20");
        assert_eq!(prs.get("eyzhub/eyzflow#841").unwrap().fix.unwrap().to_name, "eyzflow 2");

        // None in the repo: one started there, told to check the branch out first.
        let prs = store("start");
        prs.put(pr("conflicting", "dirty", "a1"), "found");
        let hands = Fake { can_start: true, ..Default::default() };
        tick(&prs, &hands, 1_000);
        let started = hands.started.lock().unwrap().clone();
        assert_eq!((started.len(), started[0].0.as_str(), started[0].1.as_str()), (1, "sooner-chatbot-answers", "eyzhub/eyzflow"));
        assert!(started[0].2.starts_with("You take over PR #841: check out its branch first (`gh pr checkout 841 --repo eyzhub/eyzflow`)."), "{}", started[0].2);
        assert_eq!(prs.get("eyzhub/eyzflow#841").unwrap().fix.unwrap().to, "t77");

        // Nobody at all: the user's at once, with why.
        let prs = store("nobody");
        prs.put(pr("conflicting", "dirty", "a1"), "found");
        tick(&prs, &Fake::default(), 1_000);
        let p = prs.get("eyzhub/eyzflow#841").unwrap();
        assert!(p.fix.as_ref().unwrap().back.as_deref().is_some_and(|b| b.starts_with("nobody could be asked to fix it")) && p.ready_for_user());
    }

    #[test]
    fn a_pr_not_handed_to_the_user_or_in_a_chain_is_left_alone() {
        let prs = store("alone");
        prs.put(Pr { waiting: false, ..pr("conflicting", "dirty", "a1") }, "found");
        let hands = Fake { alive: vec!["t13y".into()], ..Default::default() };
        tick(&prs, &hands, 1_000);
        assert!(hands.told.lock().unwrap().is_empty());
        let prs = store("chain");
        prs.put(pr("conflicting", "dirty", "a1"), "found");
        let hands = Fake { alive: vec!["t13y".into()], chain: vec!["eyzhub/eyzflow#841".into()], ..Default::default() };
        tick(&prs, &hands, 1_000);
        assert!(hands.told.lock().unwrap().is_empty(), "the chain asks it itself");
    }

    /// The whole way, with a stubbed `gh`: eyzflow #841 conflicts with dev
    /// (14 Nov, 40 minutes in the user's queue). Its session is told once;
    /// meanwhile it is on no merge card; it pushes, GitHub works it out,
    /// then it merges cleanly and comes to the user as a card.
    #[test]
    fn a_conflict_goes_to_its_session_and_the_card_comes_once_it_merges() {
        let gh = crate::prs::tests::fake_gh("fix-flow");
        let serve = |mergeable: &str, state: &str, sha: &str| {
            let v = serde_json::json!({
                "number": 841, "title": "Knowledge gaps: Draft help page", "url": "https://github.com/eyzhub/eyzflow/pull/841", "state": "OPEN",
                "isDraft": false, "mergeable": mergeable, "mergeStateStatus": state, "baseRefName": "dev", "headRefName": "gaps-draft",
                "headRefOid": sha, "body": "Drafts a help page.",
                "statusCheckRollup": [{"__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS"}],
            });
            std::fs::write(gh.with_file_name("pr-841.json"), v.to_string()).unwrap();
        };
        let prs = Prs::at(gh.with_file_name("prs.json"), gh.clone());
        let hands = Fake { alive: vec!["t13y".into()], ..Default::default() };
        let book = std::cell::RefCell::new(crate::decisions::Book::default());
        let k = "eyzhub/eyzflow#841";
        let look = |now: u64| -> Vec<u64> {
            prs.poll(k);
            tick(&prs, &hands, now);
            book.borrow_mut().sync_merges(&prs.list(), now);
            book.borrow().open.iter().filter(|d| d.kind == crate::decisions::Kind::Merge).flat_map(|d| d.prs.iter().map(|p| p.number)).collect()
        };
        serve("MERGEABLE", "CLEAN", "a1");
        prs.found(&[("eyzhub/eyzflow".to_string(), 841)], "t13y", "sooner eyzflow", Some("sooner-chatbot-answers"), true);
        assert_eq!(look(60_000), [841], "it merges cleanly: the user's card");
        // dev moved on: it conflicts. Off the user's queue at the same look, and its session told.
        serve("CONFLICTING", "DIRTY", "a1");
        assert_eq!(look(2 * 60_000), Vec::<u64>::new());
        assert!(book.borrow().closed.iter().any(|d| d.kind == crate::decisions::Kind::Merge), "its card went");
        for i in 3..8 {
            assert_eq!(look(i * 60_000), Vec::<u64>::new(), "no card while its session fixes it");
        }
        let told = hands.told.lock().unwrap().clone();
        assert_eq!(told.len(), 1, "told once: {told:?}");
        assert!(told[0].0 == "t13y" && told[0].1.starts_with("Your PR #841 (eyzhub/eyzflow) conflicts with dev"), "{told:?}");
        assert_eq!(crate::fixes::line(&prs.get(k).unwrap()).as_deref(), Some("Being fixed by sooner eyzflow: resolving its conflicts with dev."));
        // It pushed; GitHub works it out: still not the user's.
        serve("UNKNOWN", "UNKNOWN", "a2");
        assert_eq!(look(8 * 60_000), Vec::<u64>::new());
        // Clean: the fix is done and the card comes, once.
        serve("MERGEABLE", "CLEAN", "a2");
        assert_eq!(look(9 * 60_000), [841]);
        assert_eq!((prs.get(k).unwrap().fix, hands.told.lock().unwrap().len()), (None, 1));
    }

    #[test]
    fn its_session_saying_it_cant_brings_it_to_the_user() {
        let prs = store("cant");
        prs.put(pr("conflicting", "dirty", "a1"), "found");
        let hands = Fake { alive: vec!["t13y".into()], ..Default::default() };
        tick(&prs, &hands, 1_000);
        assert!(!prs.get("eyzhub/eyzflow#841").unwrap().ready_for_user());
        let p = cant_fix(&prs, "eyzhub/eyzflow#841", "the column it edits was removed on dev", "t13y").unwrap();
        assert_eq!(p.fix.as_ref().unwrap().back.as_deref(), Some("its session can't resolve it: the column it edits was removed on dev"));
        assert!(p.ready_for_user());
        // Not asked again for it.
        tick(&prs, &hands, 3 * 60 * 60_000);
        assert_eq!(hands.told.lock().unwrap().len(), 1);
        // One without conflicts has nothing to give up on.
        let clean = store("cant-clean");
        clean.put(pr("mergeable", "clean", "a1"), "found");
        assert!(cant_fix(&clean, "eyzhub/eyzflow#841", "x", "t13y").is_err());
    }
}
