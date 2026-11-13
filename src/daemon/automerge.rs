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
    /// Where a PR's dependents are looked for (`crate::impact::roots`).
    roots: Arc<dyn Fn() -> Vec<std::path::PathBuf> + Send + Sync>,
}

impl AutoMerger {
    pub fn new(prs: Prs, on: impl Fn() -> bool + Send + Sync + 'static) -> AutoMerger {
        AutoMerger { prs, on: Arc::new(on), roots: Arc::new(Vec::new) }
    }

    /// Where its dependents are looked for (the daemon's: `[prs] impact_roots`).
    pub fn with_roots(mut self, roots: impl Fn() -> Vec<std::path::PathBuf> + Send + Sync + 'static) -> AutoMerger {
        self.roots = Arc::new(roots);
        self
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
            let stale = pr.score.as_ref().is_some_and(|s| s.sha != pr.head_sha) || pr.impact.as_ref().is_some_and(|i| i.sha != pr.head_sha);
            if stale && !pr.head_sha.is_empty() {
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
        // Who depends on what it changes, at that commit (the reports given before stay).
        let mut real = prscore::real_score(&pr.title, &files);
        match crate::impact::of_pr(self.prs.gh(), &pr.repo, pr.number, &sha, &(self.roots)()) {
            Ok(mut i) => {
                i.reports = pr.impact.as_ref().map(|o| o.reports.clone()).unwrap_or_default();
                real = prscore::union(&real, &i.score());
                self.prs.set_impact(&k, Some(i));
            }
            Err(e) => {
                eprintln!("[automerge] {k}: couldn't read its diff: {e:#}");
                return None;
            }
        }
        let s = prscore::combine(pr.score.as_ref(), &real, &sha);
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

    /// The validation a plan gives a PR: two checks and an edge case.
    fn planned_checks() -> prscore::Validation {
        let check = |id: &str, what: &str, how: &str| prscore::Check { id: id.into(), what: what.into(), how: how.into(), evidence: "test output".into(), status: prscore::PLANNED.into(), ..Default::default() };
        prscore::Validation {
            checks: vec![check("c1", "The cart shows the total", "npm test -- Cart.test.tsx"), check("c2", "The total on dev matches the order", "open /cart on dev, screenshot")],
            edge_cases: vec![prscore::EdgeCase { case: "An empty cart".into(), checks: vec!["c1".into()] }],
        }
    }

    /// A store with one PR found as the daemon finds it at its session's
    /// turn end: scored as its plan predicted (`predicted`), with the plan's
    /// checks when `planned` (none reported: they are marked not run), its
    /// reviewer agreed with nothing found.
    fn found(gh: &std::path::Path, n: u64, predicted: &prscore::Evidence, planned: bool) -> Prs {
        let prs = Prs::at(gh.with_file_name(format!("prs-{n}.json")), gh.to_path_buf());
        let pred = prscore::score(predicted);
        let gh2 = gh.to_path_buf();
        prs.on_score(Box::new(move |pr| {
            let Ok((files, sha)) = crate::prs::fetch_files(&gh2, &pr.repo, pr.number) else { return Default::default() };
            let mut s = prscore::combine(Some(&pred), &prscore::real_score(&pr.title, &files), &sha);
            s.planned = planned.then(|| "p1#1".to_string());
            // Nothing outside depends on it (no repos searched here).
            let impact = crate::impact::Impact { sha, ..Default::default() };
            prscore::Scored { score: Some(s), validation: planned.then(planned_checks), impact: Some(impact) }
        }));
        prs.found(&[("o/app".into(), n)], "t7", "web", Some("shop"), true);
        prs.set_review(&crate::prs::key("o/app", n), Some(crate::reviews::Brief { status: "agreed".into(), ..Default::default() }));
        prs
    }

    /// Its session reports every planned check passed, with its evidence.
    fn pass_all(prs: &Prs, k: &str) {
        for c in ["c1", "c2"] {
            prs.report_check(k, c, "passed", "12 tests passed", "t7").unwrap();
        }
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
        pass_all(&prs, "o/app#12");
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
        pass_all(&prs, "o/app#31");
        // A new commit with a migration, seen by the poll (head s2), not yet read.
        serve(&gh, 31, "dev", &["src/Cart.tsx", "prisma/migrations/2/migration.sql"], "s2");
        prs.poll("o/app#31");
        assert_eq!(prscore::auto_path(&prs.get("o/app#31").unwrap(), true), Path::Waits("its dependents checked at its head commit".into()));
        let m = AutoMerger::new(prs.clone(), || true);
        assert!(m.tick().is_empty());
        let pr = prs.get("o/app#31").unwrap();
        assert!(pr.score.as_ref().unwrap().needs_review && pr.score.as_ref().unwrap().sha == "s2", "{:?}", pr.score);
        assert!(!calls(&gh).contains("pr merge"));
        // A merge GitHub refuses is kept, and the PR is the user's at that commit.
        let gh = fake_gh("auto-refused");
        serve(&gh, 32, "dev", &["src/Cart.tsx"], "r1");
        let prs = found(&gh, 32, &ui(), true);
        pass_all(&prs, "o/app#32");
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

    #[test]
    fn a_pr_whose_planned_checks_did_not_all_pass_never_merges_by_itself_and_its_card_says_which() {
        let gh = fake_gh("auto-checks");
        serve(&gh, 41, "dev", &["src/Cart.tsx"], "v1");
        // Found before its session finished: its checks are waited for, and it is nobody's decision meanwhile.
        let prs = Prs::at(gh.with_file_name("prs-41.json"), gh.clone());
        let gh2 = gh.clone();
        prs.on_score(Box::new(move |pr| {
            let (files, sha) = crate::prs::fetch_files(&gh2, &pr.repo, pr.number).unwrap();
            let mut s = prscore::combine(Some(&prscore::score(&ui())), &prscore::real_score(&pr.title, &files), &sha);
            s.planned = Some("p1#2".into());
            prscore::Scored { score: Some(s), validation: Some(planned_checks()), impact: Some(crate::impact::Impact { sha, ..Default::default() }) }
        }));
        let told = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let t2 = told.clone();
        prs.on_unreported(Box::new(move |pr, missing| t2.lock().unwrap().push(format!("{} {}", pr.key(), missing.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(",")))));
        prs.found(&[("o/app".into(), 41)], "t7", "web", Some("shop"), false);
        let k = "o/app#41";
        prs.set_review(k, Some(crate::reviews::Brief { status: "agreed".into(), ..Default::default() }));
        let m = AutoMerger::new(prs.clone(), || true);
        assert_eq!(prscore::auto_path(&prs.get(k).unwrap(), true), Path::Waits("its planned checks (c1, c2)".into()));
        assert!(m.tick().is_empty());
        // Its session reports one; then finishes its turn on the PR without the other: not run, and it is told once.
        prs.report_check(k, "c1", "passed", "Cart.test.tsx: 6 passed", "t7").unwrap();
        prs.found(&[("o/app".into(), 41)], "t7", "web", Some("shop"), true);
        assert_eq!(*told.lock().unwrap(), ["o/app#41 c2"]);
        let pr = prs.get(k).unwrap();
        let v = pr.validation.clone().unwrap();
        assert_eq!((v.checks[0].status.as_str(), v.checks[1].status.as_str()), ("passed", "not_run"));
        assert!(m.tick().is_empty(), "a missing check: never merged by the tower");
        assert!(!calls(&gh).contains("pr merge"));
        // Where it shows: the auto path, the card's line, the merge warnings, the decision's option.
        assert_eq!(prscore::auto_path(&pr, true), Path::Yours("planned checks missing: c2 The total on dev matches the order (not run)".into()));
        assert_eq!(v.line(), "Checks missing: c2 The total on dev matches the order (not run) (1 of 2 passed)");
        assert_eq!(prscore::auto_line(&pr, true).as_deref(), Some("Yours to merge: planned checks missing: c2 The total on dev matches the order (not run)."));
        assert!(crate::prs::gate(&pr).warnings.contains(&"Planned checks not all passed: c2 The total on dev matches the order (not run).".to_string()), "{:?}", crate::prs::gate(&pr).warnings);
        let user = prscore::for_the_user(&prs.list(), true);
        assert!(user[0].waiting, "the user's merge decision");
        let mut b = crate::decisions::Book::default();
        let id = b.sync_merges(&user, 1).fresh[0].clone();
        let d = b.find(&id).unwrap();
        assert!(d.pick().is_none(), "never the tower's pick");
        assert!(d.options[0].why.ends_with("Checks missing: c2 The total on dev matches the order (not run) (1 of 2 passed)"), "{}", d.options[0].why);
        assert_eq!(d.view(false)["prs"][0]["validation"]["checks"][1]["status"], "not_run");
        // It failed: still the user's, and said so.
        prs.report_check(k, "c2", "failed", "the total is off by the shipping cost", "t7").unwrap();
        assert!(matches!(prscore::auto_path(&prs.get(k).unwrap(), true), Path::Yours(w) if w.contains("(failed)")));
        assert!(m.tick().is_empty());
        // A report needs its evidence and a check the plan has.
        assert!(prs.report_check(k, "c2", "passed", "  ", "t7").is_err());
        assert!(prs.report_check(k, "c9", "passed", "x", "t7").unwrap_err().to_string().contains("its checks are c1, c2"));
        assert!(prs.report_check(k, "c2", "maybe", "x", "t7").is_err());
        // Fixed and passed, with its evidence: now it merges by itself.
        prs.report_check(k, "c2", "passed", "/tmp/cart-dev.png", "t7").unwrap();
        assert_eq!(prscore::auto_path(&prs.get(k).unwrap(), true), Path::Ready);
        assert_eq!(m.tick(), [k]);
        assert!(calls(&gh).contains("pr merge 41 --repo o/app --squash --match-head-commit v1"));
        // A planned PR whose plan lists no checks is the user's too.
        let no_checks = crate::prs::Pr { validation: Some(prscore::Validation::default()), ..prs.get(k).unwrap() };
        assert_eq!(prscore::auto_path(&crate::prs::Pr { state: "open".into(), ..no_checks }, true), Path::Yours("its plan lists no checks for it".into()));
    }

    #[test]
    fn a_session_reports_its_checks_by_the_prs_key_link_or_number() {
        let gh = fake_gh("validate");
        serve(&gh, 51, "dev", &["src/Cart.tsx"], "w1");
        let prs = found(&gh, 51, &ui(), true);
        let validate = super::super::validate;
        // By its key; then its link; then #51 (the caller's own).
        let v = validate(&prs, "o/app#51", "c1", "passed", "Cart.test.tsx: 6 passed", Some("t7")).unwrap();
        assert_eq!((v["pr"].as_str(), v["validation"]["checks"][0]["status"].as_str(), v["validation"]["checks"][0]["by"].as_str()), (Some("o/app#51"), Some("passed"), Some("t7")));
        validate(&prs, "https://github.com/o/app/pull/51", "c2", "not-run", "dev is down", Some("t7")).unwrap();
        assert_eq!(prs.get("o/app#51").unwrap().validation.unwrap().checks[1].status, "not_run");
        let v = validate(&prs, "#51", "c2", "passed", "/tmp/dev.png", Some("t7")).unwrap();
        assert_eq!(v["line"], "Both planned checks passed");
        // Shown without a check named; refused without its evidence or for a PR nobody knows.
        assert_eq!(validate(&prs, "#51", "", "", "", Some("t7")).unwrap()["validation"]["checks"].as_array().unwrap().len(), 2);
        assert!(validate(&prs, "#51", "c1", "passed", "", Some("t7")).unwrap_err().to_string().contains("evidence"));
        assert!(validate(&prs, "#99", "c1", "passed", "x", Some("t7")).unwrap_err().to_string().contains("no open PR #99"));
        assert!(validate(&prs, "o/app#77", "c1", "passed", "x", None).unwrap_err().to_string().contains("run this from the session that opened it"));
        assert!(!calls(&gh).contains("pr merge"));
    }

    /// A store with one PR scored as the daemon scores it: its plan's
    /// prediction (low risk), its own files, and who depends on what its
    /// diff changes in the repos under `root`; its checks passed, its reviewer agreed.
    fn found_with_diff(gh: &std::path::Path, n: u64, diff: &str, root: &std::path::Path) -> Prs {
        std::fs::write(gh.with_file_name(format!("diff-{n}.patch")), diff).unwrap();
        let prs = Prs::at(gh.with_file_name(format!("prs-{n}.json")), gh.to_path_buf());
        let (gh2, roots) = (gh.to_path_buf(), vec![root.to_path_buf()]);
        prs.on_score(Box::new(move |pr| {
            let (files, sha) = crate::prs::fetch_files(&gh2, &pr.repo, pr.number).unwrap();
            let impact = crate::impact::of_pr(&gh2, &pr.repo, pr.number, &sha, &roots).unwrap();
            let real = prscore::union(&prscore::real_score(&pr.title, &files), &impact.score());
            let mut s = prscore::combine(Some(&prscore::score(&ui())), &real, &sha);
            s.planned = Some("p1#1".into());
            prscore::Scored { score: Some(s), validation: Some(planned_checks()), impact: Some(impact) }
        }));
        prs.found(&[("o/shop-api".into(), n)], "t7", "api", Some("shop"), true);
        let k = crate::prs::key("o/shop-api", n);
        pass_all(&prs, &k);
        prs.set_review(&k, Some(crate::reviews::Brief { status: "agreed".into(), ..Default::default() }));
        prs
    }

    fn serve_api(gh: &std::path::Path, n: u64, files: &[&str], sha: &str) {
        serve(gh, n, "dev", files, sha);
        let f = gh.with_file_name(format!("pr-{n}.json"));
        let text = std::fs::read_to_string(&f).unwrap().replace("o/app/pull", "o/shop-api/pull");
        std::fs::write(f, text).unwrap();
    }

    #[test]
    fn a_pr_that_breaks_another_repos_code_is_flagged_and_never_merged_by_the_tower() {
        let root = crate::impact::tests::scratch("auto-breaking");
        let gh = fake_gh("auto-breaking");
        serve_api(&gh, 61, &["src/money.ts", "src/api/messages.ts"], "b1");
        let prs = found_with_diff(&gh, 61, crate::impact::tests::BREAKING, &root);
        let k = "o/shop-api#61";
        let pr = prs.get(k).unwrap();
        // Its score: a breaking change, naming what breaks and who uses it.
        let s = pr.score.clone().unwrap();
        assert!(s.needs_review && s.flags.contains(&"breaking".to_string()), "{s:?}");
        assert!(s.line.contains("renames formatTotal to formatAmount, used by shop-web src/cart/Total.tsx:4 in renderTotal"), "{}", s.line);
        let i = pr.impact.clone().unwrap();
        assert!(i.dependents.iter().any(|d| d.said() == "shop-web src/cart/Total.tsx:4 in renderTotal"), "{:?}", i.dependents);
        assert!(i.dependents.iter().any(|d| d.said() == "shop-web src/inbox/api.ts:3 in loadMessages"), "{:?}", i.dependents);
        // The warning on the card, and before a merge; never the tower's merge, never its pick.
        assert!(crate::prs::gate(&pr).warnings.iter().any(|w| w.starts_with("Breaking change: renames formatTotal to formatAmount")), "{:?}", crate::prs::gate(&pr).warnings);
        assert!(matches!(prscore::auto_path(&pr, true), Path::Yours(w) if w.contains("a breaking change")));
        let m = AutoMerger::new(prs.clone(), || true).with_roots({
            let root = root.clone();
            move || vec![root.clone()]
        });
        assert!(m.tick().is_empty());
        assert!(!calls(&gh).contains("pr merge"));
        let mut b = crate::decisions::Book::default();
        let id = b.sync_merges(&prscore::for_the_user(&prs.list(), true), 1).fresh[0].clone();
        let d = b.find(&id).unwrap();
        assert!(d.pick().is_none());
        assert_eq!(d.view(false)["prs"][0]["impact"]["dependents"][0]["repo"], "shop-api");
    }

    #[test]
    fn a_purely_internal_change_breaks_nothing_and_still_merges_by_itself() {
        let root = crate::impact::tests::scratch("auto-internal");
        let gh = fake_gh("auto-internal");
        serve_api(&gh, 62, &["src/util.ts"], "i1");
        let prs = found_with_diff(&gh, 62, crate::impact::tests::INTERNAL, &root);
        let k = "o/shop-api#62";
        let pr = prs.get(k).unwrap();
        assert!(!pr.score.as_ref().unwrap().needs_review, "{:?}", pr.score);
        assert_eq!(pr.impact.as_ref().unwrap().line(), "Nothing outside depends on what it changes (no name others can use changes; searched 3 repos)");
        assert_eq!(prscore::auto_path(&pr, true), Path::Ready);
        // A push that renames something others use is read before the merge, and stops it.
        std::fs::write(gh.with_file_name("diff-62.patch"), crate::impact::tests::BREAKING).unwrap();
        serve_api(&gh, 62, &["src/util.ts", "src/money.ts"], "i2");
        prs.poll(k);
        let m = AutoMerger::new(prs.clone(), || true).with_roots({
            let root = root.clone();
            move || vec![root.clone()]
        });
        assert!(m.tick().is_empty(), "the push breaks shop-web: not merged");
        let pr = prs.get(k).unwrap();
        assert!(pr.impact.as_ref().unwrap().breaks() && pr.impact.as_ref().unwrap().sha == "i2");
        assert!(!calls(&gh).contains("pr merge"));
        // Back to the internal change: merged by the tower.
        std::fs::write(gh.with_file_name("diff-62.patch"), crate::impact::tests::INTERNAL).unwrap();
        serve_api(&gh, 62, &["src/util.ts"], "i3");
        prs.poll(k);
        // (Its score keeps what it once found: the user's now, as a once-breaking PR should be.)
        assert!(m.tick().is_empty());
        assert!(matches!(prscore::auto_path(&prs.get(k).unwrap(), true), Path::Yours(_)));
    }

    #[test]
    fn the_reviewer_reports_a_break_it_found_by_running_the_dependents_tests() {
        let root = crate::impact::tests::scratch("impact-report");
        let gh = fake_gh("impact-report");
        serve_api(&gh, 63, &["src/util.ts"], "r1");
        let prs = found_with_diff(&gh, 63, crate::impact::tests::INTERNAL, &root);
        let report = super::super::impact_report;
        let k = "o/shop-api#63";
        assert_eq!(prscore::auto_path(&prs.get(k).unwrap(), true), Path::Ready);
        // Shown; a clean run noted; then a break the search couldn't see: a breaking change, never merged by the tower.
        let v = report(&prs, "#63", "", "", Some("t7")).unwrap();
        assert_eq!((v["breaks"].as_bool(), v["impact"]["searched"].as_array().map(Vec::len)), (Some(false), Some(3)));
        report(&prs, k, "clean", "tsc --noEmit in shop-web: no errors", Some("t9")).unwrap();
        assert!(prs.get(k).unwrap().impact.unwrap().line().ends_with("checked: tsc --noEmit in shop-web: no errors)"));
        let v = report(&prs, k, "breaks", "the mobile app's sync reads clean() (shop-mobile src/sync.kt syncAll): its tests fail", Some("t9")).unwrap();
        assert_eq!(v["breaks"], true);
        let pr = prs.get(k).unwrap();
        let s = pr.score.clone().unwrap();
        assert!(s.needs_review && s.line == "Needs review: a breaking change (the mobile app's sync reads clean() (shop-mobile src/sync.kt syncAll): its tests fail)", "{s:?}");
        assert_eq!(s.planned.as_deref(), Some("p1#1"), "still its plan's");
        assert!(matches!(prscore::auto_path(&pr, true), Path::Yours(w) if w.starts_with("Needs review: a breaking change (the mobile app's sync reads clean()")));
        assert!(crate::prs::gate(&pr).warnings.iter().any(|w| w.starts_with("Breaking change: the mobile app's sync")));
        let m = AutoMerger::new(prs.clone(), || true);
        assert!(m.tick().is_empty());
        assert!(!calls(&gh).contains("pr merge"));
        assert!(report(&prs, k, "breaks", " ", Some("t9")).is_err());
    }
}
