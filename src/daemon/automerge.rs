//! The tower's own merges (`crate::prscore::auto_path`), behind `[prs]
//! auto_merge_low_risk` (off unless the user turns it on: `tower automerge
//! on`, Settings).
//!
//! Every look (the PR thread's, every ten seconds):
//!
//! - A PR whose score was read at another commit (a push since) is read
//!   again on its own files at its head (`gh pr view --json files`): what it
//!   shows now is added to its score, so the card says it and nothing is
//!   merged on an old reading.
//! - A PR that is `Ready` (a plan predicted it, low risk, into dev, checks
//!   pass, its reviewer agreed without finding anything, clean) is read
//!   once more right before the merge; if its files still score low risk it
//!   is merged pinned to that head commit, with the repo's own method, never
//!   `--admin` (`prs::merge`). Then GitHub is asked, and the session and the
//!   brain are told as after any merge (`Prs::report_merged`, `by` `tower`).
//! - A merge GitHub refuses (branch protection, a required review, a check
//!   that turned) is kept as `auto: failed` with why: that PR is the user's
//!   from then on (a merge decision, when its session waits on it), until a
//!   new commit.

use std::sync::Arc;

use crate::prs::{Pr, Prs, Refusal};
use crate::prscore::{self, Auto, Path};

/// Why the tower merged one, as its card and its session are told.
pub const MERGED_WHY: &str = "low risk, into dev; its checks passed and its reviewer found nothing";

#[derive(Clone)]
pub struct AutoMerger {
    prs: Prs,
    /// The setting as it is now (`[prs] auto_merge_low_risk`).
    on: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl AutoMerger {
    pub fn new(prs: Prs, on: impl Fn() -> bool + Send + Sync + 'static) -> AutoMerger {
        AutoMerger { prs, on: Arc::new(on) }
    }

    pub fn on(&self) -> bool {
        (self.on)()
    }

    /// One look at every open PR: stale scores read again, ready ones
    /// merged. Returns the keys it merged.
    pub fn tick(&self) -> Vec<String> {
        let on = self.on();
        let mut merged = vec![];
        for pr in self.prs.list().into_iter().filter(Pr::open) {
            let k = pr.key();
            if pr.score.as_ref().is_some_and(|s| s.sha != pr.head_sha) && !pr.head_sha.is_empty() {
                self.rescore(&pr);
            }
            let Some(pr) = self.prs.get(&k) else { continue };
            if auto_path(&pr, on) != Path::Ready {
                continue;
            }
            // The user's "Merge all N" has it: that chain merges it, not this.
            if super::merges::get().is_some_and(|m| m.holds(&k)) {
                continue;
            }
            match self.merge(&pr) {
                Ok(()) => merged.push(k),
                Err(why) => eprintln!("[automerge] {k}: not merged: {why}"),
            }
        }
        merged
    }

    /// Its score with what its files at its head show now (GitHub asked).
    /// None when GitHub couldn't say.
    fn rescore(&self, pr: &Pr) -> Option<(prscore::Score, String)> {
        let k = pr.key();
        let (files, sha) = match crate::prs::fetch_files(self.prs.gh(), &pr.repo, pr.number) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("[automerge] {k}: couldn't read its files: {e:#}");
                return None;
            }
        };
        let s = prscore::combine(pr.score.as_ref(), &prscore::real_score(&pr.title, &files), &sha);
        if pr.score.as_ref().is_some_and(|old| !old.needs_review) && s.needs_review {
            eprintln!("[automerge] {k}: at {sha} it {}", s.line.to_lowercase());
        }
        self.prs.set_score(&k, Some(s.clone()));
        Some((s, sha))
    }

    /// Merges a ready PR: its files read once more at its head, then the
    /// merge, pinned to that commit.
    fn merge(&self, pr: &Pr) -> Result<(), String> {
        let k = pr.key();
        let Some((score, sha)) = self.rescore(pr) else { return Err("GitHub couldn't say what it changes".into()) };
        if score.needs_review {
            return Err(score.line);
        }
        if sha != pr.head_sha {
            // A push since the last look: the next look sees it whole.
            return Err(format!("a new commit ({sha}) since it was looked at"));
        }
        eprintln!("[automerge] {k}: merging into {} ({})", pr.base, score.line);
        match crate::prs::merge(self.prs.gh(), &pr.repo, pr.number, &sha, false) {
            Ok(_) => {
                self.prs.set_auto(&k, Some(Auto { state: "merged".into(), why: MERGED_WHY.into(), sha, at_ms: crate::history::now_ms() }));
                if self.prs.report_merged(&k, "tower").is_none() {
                    eprintln!("[automerge] {k}: merged, but GitHub doesn't say so yet");
                }
                Ok(())
            }
            // Its head moved under it: looked at again next time.
            Err(Refusal::Changed(m)) => Err(m),
            Err(r) => {
                let why = r.message().trim_end_matches('.').to_string();
                self.prs.set_auto(&k, Some(Auto { state: "failed".into(), why: why.clone(), sha, at_ms: crate::history::now_ms() }));
                Err(why)
            }
        }
    }
}

fn auto_path(pr: &Pr, on: bool) -> Path {
    prscore::auto_path(pr, on)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prs::tests::{calls, fake_gh};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A PR as the fake `gh` serves it: into `base`, checks passing, these files.
    fn serve(gh: &std::path::Path, n: u64, base: &str, files: &[&str], sha: &str) {
        let v = json!({
            "number": n, "title": format!("PR {n}"), "url": format!("https://github.com/o/app/pull/{n}"), "state": "OPEN", "isDraft": false,
            "mergeable": "MERGEABLE", "baseRefName": base, "headRefName": format!("feat-{n}"), "headRefOid": sha,
            "statusCheckRollup": [{"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"SUCCESS"}],
            "files": files.iter().map(|f| json!({"path": f, "additions": 1, "deletions": 0})).collect::<Vec<Value>>(),
        });
        std::fs::write(gh.with_file_name(format!("pr-{n}.json")), serde_json::to_string(&v).unwrap()).unwrap();
    }

    /// A store with one PR found as the daemon finds it: scored as its plan
    /// predicted (`predicted`), its reviewer agreed with nothing found.
    fn found(gh: &std::path::Path, n: u64, predicted: &prscore::Evidence, planned: bool) -> Prs {
        let prs = Prs::at(gh.with_file_name(format!("prs-{n}.json")), gh.to_path_buf());
        let pred = prscore::score(predicted);
        let gh2 = gh.to_path_buf();
        prs.on_score(Box::new(move |pr| {
            let (files, sha) = crate::prs::fetch_files(&gh2, &pr.repo, pr.number).ok()?;
            let mut s = prscore::combine(Some(&pred), &prscore::real_score(&pr.title, &files), &sha);
            s.planned = planned.then(|| "p1#1".to_string());
            Some(s)
        }));
        prs.found(&[("o/app".into(), n)], "t7", "web", Some("shop"), true);
        prs.set_review(&crate::prs::key("o/app", n), Some(crate::reviews::Brief { status: "agreed".into(), ..Default::default() }));
        prs
    }

    fn ui() -> prscore::Evidence {
        prscore::Evidence { title: "Show the total on the cart".into(), files: vec!["src/Cart.tsx".into()], ..Default::default() }
    }

    fn migration() -> prscore::Evidence {
        prscore::Evidence { title: "Orders table".into(), files: vec!["db/migrations/0042_orders.sql".into()], migration: true, ..Default::default() }
    }

    #[test]
    fn a_low_risk_pr_into_dev_merges_by_itself_only_when_the_setting_is_on() {
        // Off: nothing is merged, whatever the PR.
        let gh = fake_gh("auto-off");
        serve(&gh, 12, "dev", &["src/Cart.tsx"], "abc");
        let prs = found(&gh, 12, &ui(), true);
        let pr = prs.get("o/app#12").unwrap();
        assert_eq!((pr.score.as_ref().unwrap().needs_review, pr.score.as_ref().unwrap().sha.as_str()), (false, "abc"));
        let on = Arc::new(AtomicBool::new(false));
        let o = on.clone();
        let m = AutoMerger::new(prs.clone(), move || o.load(Ordering::SeqCst));
        assert!(m.tick().is_empty());
        assert!(!calls(&gh).contains("pr merge"), "the setting is off: nothing merged\n{}", calls(&gh));
        assert_eq!(prs.get("o/app#12").unwrap().state, "open");
        // On: merged, pinned to the head it read, with the repo's own method, never --admin.
        on.store(true, Ordering::SeqCst);
        let told = Arc::new(std::sync::Mutex::new(vec![]));
        let t2 = told.clone();
        prs.on_merged(Box::new(move |pr, by| t2.lock().unwrap().push(format!("{} {by}", pr.key()))));
        assert_eq!(m.tick(), ["o/app#12"]);
        let log = calls(&gh);
        assert!(log.contains("pr merge 12 --repo o/app --squash --match-head-commit abc"), "{log}");
        assert!(!log.contains("--admin") && !log.contains("--auto"), "{log}");
        let pr = prs.get("o/app#12").unwrap();
        assert_eq!((pr.state.as_str(), pr.merged_by.as_deref(), pr.auto.as_ref().map(|a| a.state.as_str())), ("merged", Some("tower"), Some("merged")));
        assert_eq!(*told.lock().unwrap(), ["o/app#12 tower"], "its session and the brain are told, once");
        assert!(m.tick().is_empty(), "merged once");
    }

    #[test]
    fn a_migration_a_pr_into_main_or_an_unplanned_one_never_merges_by_itself() {
        let gh = fake_gh("auto-never");
        // A migration, as the plan predicted it.
        serve(&gh, 21, "dev", &["db/migrations/0042_orders.sql"], "m1");
        let a = found(&gh, 21, &migration(), true);
        // A plain UI PR, but into main.
        serve(&gh, 22, "main", &["src/Cart.tsx"], "m2");
        let b = found(&gh, 22, &ui(), true);
        // Predicted as UI, but its real files have a migration.
        serve(&gh, 23, "dev", &["src/Cart.tsx", "db/migrations/0043_flags.sql"], "m3");
        let c = found(&gh, 23, &ui(), true);
        // Low risk, into dev, but no plan predicted it.
        serve(&gh, 24, "dev", &["src/Cart.tsx"], "m4");
        let d = found(&gh, 24, &ui(), false);
        for (prs, k) in [(&a, "o/app#21"), (&b, "o/app#22"), (&c, "o/app#23"), (&d, "o/app#24")] {
            let m = AutoMerger::new(prs.clone(), || true);
            assert!(m.tick().is_empty(), "{k}");
            let pr = prs.get(k).unwrap();
            assert_eq!(pr.state, "open", "{k}");
            assert!(matches!(prscore::auto_path(&pr, true), Path::Yours(_)), "{k}: {:?}", prscore::auto_path(&pr, true));
        }
        assert!(!calls(&gh).contains("pr merge"), "never merged\n{}", calls(&gh));
        assert!(a.get("o/app#21").unwrap().score.unwrap().line.starts_with("Needs review: a database migration"));
        assert_eq!(c.get("o/app#23").unwrap().score.unwrap().line, "Needs review: a database migration (db/migrations/0043_flags.sql), which the plan didn't predict");
        // And all of them stay the user's merge decision.
        let all: Vec<Pr> = [&a, &b, &c, &d].iter().flat_map(|p| p.list()).collect();
        assert_eq!(prscore::for_the_user(&all, true).iter().filter(|p| p.waiting).count(), 4);
    }

    #[test]
    fn a_push_that_brings_a_migration_is_read_before_the_merge_and_stops_it() {
        let gh = fake_gh("auto-push");
        serve(&gh, 31, "dev", &["src/Cart.tsx"], "s1");
        let prs = found(&gh, 31, &ui(), true);
        // A new commit with a migration, seen by the poll (head s2), not yet read.
        serve(&gh, 31, "dev", &["src/Cart.tsx", "prisma/migrations/2/migration.sql"], "s2");
        prs.poll("o/app#31");
        assert_eq!(prscore::auto_path(&prs.get("o/app#31").unwrap(), true), Path::Waits("its files read at its head commit".into()));
        let m = AutoMerger::new(prs.clone(), || true);
        assert!(m.tick().is_empty());
        let pr = prs.get("o/app#31").unwrap();
        assert!(pr.score.as_ref().unwrap().needs_review && pr.score.as_ref().unwrap().sha == "s2", "{:?}", pr.score);
        assert!(!calls(&gh).contains("pr merge"));
        // A merge GitHub refuses is kept, and the PR is the user's at that commit.
        let gh = fake_gh("auto-refused");
        serve(&gh, 32, "dev", &["src/Cart.tsx"], "r1");
        let prs = found(&gh, 32, &ui(), true);
        // The fake gh refuses: its merge needs a review (as a protected branch would say).
        let script = std::fs::read_to_string(&gh).unwrap().replace(r#""pr merge") sed"#, r#""pr merge") echo "a review is required" >&2; exit 1; sed"#);
        std::fs::write(&gh, script).unwrap();
        let m = AutoMerger::new(prs.clone(), || true);
        assert!(m.tick().is_empty());
        let pr = prs.get("o/app#32").unwrap();
        assert_eq!(pr.auto.as_ref().map(|a| (a.state.as_str(), a.sha.as_str())), Some(("failed", "r1")));
        assert!(matches!(prscore::auto_path(&pr, true), Path::Yours(w) if w.contains("a review is required")), "{:?}", prscore::auto_path(&pr, true));
        assert!(m.tick().is_empty(), "not tried again at the same commit");
        assert_eq!(calls(&gh).matches("pr merge").count(), 1);
    }
}
