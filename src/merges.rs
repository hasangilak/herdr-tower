//! "Merge all N" on a merge card: the tower merges the card's pull
//! requests in order, by itself, once the user confirmed the whole chain
//! (`daemon::merges` runs it, a look every `POLL_MS`).
//!
//! - **In turn.** One at a time, in the card's order: the next one starts
//!   only once the one before has landed (GitHub says merged).
//! - **Waiting is not failing.** Right after a merge GitHub works out again
//!   whether the next one merges cleanly (`mergeable` UNKNOWN), and its
//!   checks may run: both are waited for (`RECOMPUTE_MS`, `CHECKS_MS`).
//! - **Fixed by its session.** One that fell behind its base (the repo
//!   wants branches up to date) or now conflicts with it: its session is
//!   asked, once, to bring the branch up to date or resolve the conflicts
//!   and push (one with no session left that only fell behind is updated on
//!   GitHub, `gh pr update-branch`). Checks that fail are its to fix too,
//!   once. Its push is waited for (`FIX_MS`), then its checks, then it is
//!   merged: the rest of the chain is never put back to the user for it.
//! - **Back to the user, once.** Only what can't be fixed: checks that
//!   still fail after its fix, conflicts still there after it, a draft, a
//!   rule of GitHub's (a required review), a merge GitHub refuses, or a wait
//!   that ran out. The chain stops there; the PRs after it stay on the card.
//! - Merging is still the user's: they confirmed this chain, and a PR that
//!   changed meanwhile is merged only by its own fix and only when its
//!   checks pass. Never `--admin`, always pinned to the head looked at.

use serde::{Deserialize, Serialize};

use crate::prs::{Pr, Refusal};

/// How often the PR in turn is looked at.
pub const POLL_MS: u64 = 15_000;
/// GitHub working out whether it merges cleanly, waited for this long.
pub const RECOMPUTE_MS: u64 = 15 * 60_000;
/// Its checks running, waited for this long.
pub const CHECKS_MS: u64 = 45 * 60_000;
/// Its session updating it, waited for this long.
pub const FIX_MS: u64 = 60 * 60_000;
/// A merge GitHub refuses for a reason the next look doesn't explain: tried this often.
const MERGE_TRIES: u32 = 3;

/// Where one PR of a chain stands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "step", content = "why")]
pub enum Step {
    /// Its turn hasn't come.
    #[default]
    Queued,
    /// Its turn: waiting for GitHub or its checks (why).
    Waiting(String),
    Merging,
    /// Its session (or GitHub) is bringing it up to date, or fixing it (what).
    Fixing(String),
    Merged,
    /// It was closed meanwhile: passed over.
    Skipped(String),
    /// It can't be merged without the user (why): the chain stops here.
    Back(String),
}

impl Step {
    /// It is behind the chain: merged, or passed over.
    pub fn done(&self) -> bool {
        matches!(self, Step::Merged | Step::Skipped(_))
    }

    /// A line for the card.
    pub fn line(&self) -> String {
        match self {
            Step::Queued => "next".into(),
            Step::Waiting(why) | Step::Fixing(why) => why.clone(),
            Step::Merging => "merging…".into(),
            Step::Merged => "merged".into(),
            Step::Skipped(why) => format!("passed over: {why}"),
            Step::Back(why) => format!("back to you: {why}"),
        }
    }
}

/// One PR of a chain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Link {
    /// `owner/repo#n`.
    pub key: String,
    pub number: u64,
    pub base: String,
    pub step: Step,
    /// When its step began: its waits are counted from here.
    pub since_ms: u64,
    /// What it was asked to fix, once (`behind`, `conflicts`, `checks`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// Its head commit when the fix was asked for: a new one is its push.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub head_asked: String,
    /// Merges GitHub refused that the next look didn't explain.
    #[serde(skip_serializing_if = "is_zero")]
    pub refused: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// One "Merge all N", as the user confirmed it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Chain {
    pub id: String,
    /// The merge decision it came from.
    pub card: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// Who confirmed it (`mac`, `phone:<device>`).
    pub by: String,
    pub links: Vec<Link>,
    pub started_ms: u64,
    /// The user stopped it: nothing more is merged.
    pub stopped: bool,
}

impl Chain {
    pub fn new(id: &str, card: &str, space: Option<String>, by: &str, prs: &[Pr], now: u64) -> Chain {
        let links = prs.iter().map(|p| Link { key: p.key(), number: p.number, base: p.base.clone(), since_ms: now, ..Default::default() }).collect();
        Chain { id: id.into(), card: card.into(), space, by: by.into(), links, started_ms: now, stopped: false }
    }

    /// Nothing more to do: all merged (or passed over), one back to the user, or stopped.
    pub fn over(&self) -> bool {
        self.stopped || self.links.iter().all(|l| l.step.done()) || self.links.iter().any(|l| matches!(l.step, Step::Back(_)))
    }

    /// The PR in turn.
    fn current(&mut self) -> Option<&mut Link> {
        self.links.iter_mut().find(|l| !l.step.done())
    }
}

/// What to do about the PR in turn, as GitHub has it now.
#[derive(Debug, Clone, PartialEq)]
pub enum Do {
    Wait(String),
    Merge(String),
    /// Its session is to fix this (`behind`, `conflicts`, `checks`).
    Fix(String),
    /// It is merged (by the chain, or by anyone).
    Landed,
    Skip(String),
    Back(String),
}

/// What to do about `link`, its PR as GitHub has it now (`pr`).
pub fn decide(link: &Link, pr: &Pr, now: u64) -> Do {
    match pr.state.as_str() {
        "merged" => return Do::Landed,
        "closed" => return Do::Skip("it was closed".into()),
        _ => {}
    }
    if pr.draft {
        return Do::Back("it's a draft".into());
    }
    let waited = now.saturating_sub(link.since_ms);
    let base = &pr.base;
    // Asked to fix it, and nothing pushed yet.
    if link.fix.is_some() && pr.head_sha == link.head_asked {
        let what = link.fix.as_deref().unwrap_or("");
        return if waited >= FIX_MS {
            Do::Back(format!("its session didn't push a fix within {} minutes ({})", FIX_MS / 60_000, fix_words(what, base)))
        } else {
            Do::Wait(format!("its session is {}", doing_words(what, base)))
        };
    }
    let conflicts = pr.mergeable == "conflicting" || pr.merge_state == "dirty";
    let behind = pr.merge_state == "behind";
    let unknown = pr.mergeable == "unknown" || pr.merge_state == "unknown";
    if conflicts || behind {
        let what = if conflicts { "conflicts" } else { "behind" };
        return match &link.fix {
            None => Do::Fix(what.into()),
            // Pushed, and still so: GitHub may not have looked again yet.
            Some(_) if waited < RECOMPUTE_MS => Do::Wait("GitHub is looking at its update".into()),
            Some(_) => Do::Back(if conflicts { format!("it still conflicts with {base} after its session's update") } else { format!("it is still behind {base} after its update") }),
        };
    }
    if unknown {
        return if waited >= RECOMPUTE_MS {
            Do::Back("GitHub didn't work out whether it merges cleanly".into())
        } else {
            Do::Wait("GitHub is working out whether it merges cleanly".into())
        };
    }
    match pr.checks.state.as_str() {
        "pending" if waited >= CHECKS_MS => return Do::Back(format!("its checks were still running after {} minutes", CHECKS_MS / 60_000)),
        "pending" => return Do::Wait("its checks are running".into()),
        "failing" if link.fix.is_none() => return Do::Fix("checks".into()),
        "failing" => return Do::Back(format!("{} after its session's fix", pr.checks_line())),
        _ => {}
    }
    if pr.merge_state == "blocked" {
        return Do::Back("GitHub's rules for the branch block it (a required review?)".into());
    }
    Do::Merge(pr.head_sha.clone())
}

fn fix_words(what: &str, base: &str) -> String {
    match what {
        "behind" => format!("bring its branch up to date with {base}"),
        "conflicts" => format!("resolve its conflicts with {base}"),
        _ => "fix its failing checks".into(),
    }
}

fn doing_words(what: &str, base: &str) -> String {
    match what {
        "behind" => format!("bringing its branch up to date with {base}"),
        "conflicts" => format!("resolving its conflicts with {base}"),
        _ => "fixing its failing checks".into(),
    }
}

/// What its session is asked, to fix `what` before the chain merges it.
pub fn fix_request(pr: &Pr, what: &str, before: Option<u64>) -> String {
    let why = match what {
        "behind" => format!("its branch is behind {} and the repo wants it up to date", pr.base),
        "conflicts" => format!("it now conflicts with {}", pr.base),
        _ => pr.checks_line(),
    };
    let after = before.map(|n| format!(" after #{n} went in")).unwrap_or_default();
    format!(
        "The user asked the tower to merge PR #{} ({}) into {} in order with the others{after}, but {why}. {} (merge {} into the branch{}, run the tests), push, and end your turn. \
         The tower merges it by itself once GitHub sees it clean: don't merge it yourself.",
        pr.number,
        pr.repo,
        pr.base,
        capital(&fix_words(what, &pr.base)),
        pr.base,
        if what == "conflicts" { ", resolve the conflicts" } else { "" },
    )
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// What a chain needs of GitHub and the tower.
pub trait World {
    /// The PR as GitHub has it now.
    fn look(&self, key: &str) -> anyhow::Result<Pr>;
    /// Merges it, pinned to `sha` (`prs::merge`).
    fn merge(&self, pr: &Pr, sha: &str) -> Result<Pr, Refusal>;
    /// Brings its branch up to date on GitHub (`gh pr update-branch`).
    fn update_branch(&self, pr: &Pr) -> anyhow::Result<()>;
    /// Hands its session the request to fix it (Err: it has none to ask).
    fn ask(&self, pr: &Pr, text: &str) -> anyhow::Result<()>;
    /// It landed: the session and the brain are told, as for any merge.
    fn landed(&self, pr: &Pr, by: &str);
    /// It is back with the user (once): why.
    fn back(&self, chain: &Chain, pr: &Pr, why: &str);
}

/// What a tick did.
#[derive(Debug, Clone, PartialEq)]
pub enum Tick {
    /// Look again at once (the next PR is in turn).
    Again,
    /// Look again after `POLL_MS`.
    Later,
    Over,
}

fn set(link: &mut Link, step: Step, now: u64) {
    if link.step != step {
        link.step = step;
        link.since_ms = now;
    }
}

/// One look at the PR in turn, and what it calls for.
pub fn tick(chain: &mut Chain, w: &dyn World, now: u64) -> Tick {
    if chain.over() {
        return Tick::Over;
    }
    let before = chain.links.iter().rev().find(|l| l.step == Step::Merged).map(|l| l.number);
    let by = chain.by.clone();
    let snapshot = chain.clone();
    let Some(link) = chain.current() else { return Tick::Over };
    // Its turn has come: its waits count from now.
    if link.step == Step::Queued {
        link.since_ms = now;
    }
    let pr = match w.look(&link.key) {
        Ok(pr) => pr,
        Err(e) => {
            // GitHub out of reach: as any other wait.
            if now.saturating_sub(link.since_ms) >= RECOMPUTE_MS && !matches!(link.step, Step::Fixing(_)) {
                let why = format!("GitHub couldn't be asked about it ({e:#})");
                set(link, Step::Back(why.clone()), now);
                w.back(&snapshot, &Pr { number: link.number, base: link.base.clone(), ..Default::default() }, &why);
                return Tick::Over;
            }
            return Tick::Later;
        }
    };
    match decide(link, &pr, now) {
        Do::Landed => {
            let ours = link.step == Step::Merging;
            set(link, Step::Merged, now);
            w.landed(&pr, if ours { &by } else { "github" });
            Tick::Again
        }
        Do::Skip(why) => {
            set(link, Step::Skipped(why), now);
            Tick::Again
        }
        Do::Wait(why) => {
            // Asked to fix it: its step says so until it pushes.
            if link.fix.is_some() && pr.head_sha == link.head_asked {
                set(link, Step::Fixing(why), now);
            } else {
                set(link, Step::Waiting(why), now);
            }
            Tick::Later
        }
        Do::Fix(what) => {
            let text = fix_request(&pr, &what, before);
            let asked = match w.ask(&pr, &text) {
                Ok(()) => Ok(format!("its session is {}", doing_words(&what, &pr.base))),
                // No session to ask: GitHub brings a branch that only fell behind up to date.
                Err(_) if what == "behind" => w.update_branch(&pr).map(|_| format!("GitHub is bringing its branch up to date with {}", pr.base)),
                Err(e) => Err(e),
            };
            match asked {
                Ok(doing) => {
                    link.fix = Some(what);
                    link.head_asked = pr.head_sha.clone();
                    set(link, Step::Fixing(doing), now);
                    Tick::Later
                }
                Err(e) => {
                    let why = format!("it needs its session to {}, and that couldn't be asked ({e:#})", fix_words(&what, &pr.base));
                    set(link, Step::Back(why.clone()), now);
                    w.back(&snapshot, &pr, &why);
                    Tick::Over
                }
            }
        }
        Do::Back(why) => {
            set(link, Step::Back(why.clone()), now);
            w.back(&snapshot, &pr, &why);
            Tick::Over
        }
        Do::Merge(sha) => {
            set(link, Step::Merging, now);
            match w.merge(&pr, &sha) {
                Ok(after) if after.state == "merged" => {
                    set(link, Step::Merged, now);
                    w.landed(&after, &by);
                    Tick::Again
                }
                // Merged, and GitHub doesn't say so yet; or it changed meanwhile: the next look tells.
                Ok(_) | Err(Refusal::Changed(_)) | Err(Refusal::Blocked(_)) | Err(Refusal::NeedsOverride(_)) => Tick::Later,
                Err(Refusal::Failed(e)) => {
                    link.refused += 1;
                    if link.refused >= MERGE_TRIES {
                        let why = format!("GitHub refused to merge it: {e}");
                        set(link, Step::Back(why.clone()), now);
                        w.back(&snapshot, &pr, &why);
                        return Tick::Over;
                    }
                    set(link, Step::Waiting(format!("GitHub refused it once ({e}): looking again")), now);
                    Tick::Later
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};

    /// GitHub as a script: what each look at a PR answers, in turn (the last one stays).
    #[derive(Default)]
    struct Fake {
        looks: RefCell<BTreeMap<u64, VecDeque<Pr>>>,
        did: RefCell<Vec<String>>,
        no_session: Vec<u64>,
    }

    fn pr(n: u64, state: &str, mergeable: &str, merge_state: &str, checks: &str, sha: &str) -> Pr {
        Pr {
            repo: "acme/api".into(),
            number: n,
            base: "dev".into(),
            head_sha: sha.into(),
            state: state.into(),
            mergeable: mergeable.into(),
            merge_state: merge_state.into(),
            checks: crate::prs::Checks { state: checks.into(), failing: if checks == "failing" { vec!["test".into()] } else { vec![] }, ..Default::default() },
            session: format!("t{n}"),
            ..Default::default()
        }
    }

    impl Fake {
        fn script(&self, n: u64, looks: Vec<Pr>) {
            self.looks.borrow_mut().insert(n, looks.into());
        }
        fn now(&self, n: u64) -> Pr {
            let mut l = self.looks.borrow_mut();
            let q = l.get_mut(&n).expect("scripted");
            if q.len() > 1 { q.pop_front().unwrap() } else { q[0].clone() }
        }
    }

    impl World for Fake {
        fn look(&self, key: &str) -> anyhow::Result<Pr> {
            let n: u64 = key.rsplit('#').next().unwrap().parse().unwrap();
            Ok(self.now(n))
        }
        fn merge(&self, pr: &Pr, sha: &str) -> Result<Pr, Refusal> {
            self.did.borrow_mut().push(format!("merge #{} at {sha}", pr.number));
            // After it, GitHub says merged.
            let merged = Pr { state: "merged".into(), ..pr.clone() };
            self.script(pr.number, vec![merged.clone()]);
            Ok(merged)
        }
        fn update_branch(&self, pr: &Pr) -> anyhow::Result<()> {
            self.did.borrow_mut().push(format!("update-branch #{}", pr.number));
            Ok(())
        }
        fn ask(&self, pr: &Pr, text: &str) -> anyhow::Result<()> {
            if self.no_session.contains(&pr.number) {
                anyhow::bail!("no session");
            }
            self.did.borrow_mut().push(format!("ask {} to fix #{}: {}", pr.session, pr.number, text.split(" (merge").next().unwrap().rsplit(". ").next().unwrap()));
            Ok(())
        }
        fn landed(&self, pr: &Pr, by: &str) {
            self.did.borrow_mut().push(format!("landed #{} by {by}", pr.number));
        }
        fn back(&self, _: &Chain, pr: &Pr, why: &str) {
            self.did.borrow_mut().push(format!("back #{}: {why}", pr.number));
        }
    }

    /// Runs the chain to its end as the daemon does, a look every `POLL_MS`.
    fn run(chain: &mut Chain, w: &Fake, mut now: u64) -> u64 {
        for _ in 0..500 {
            match tick(chain, w, now) {
                Tick::Again => {}
                Tick::Later => now += POLL_MS,
                Tick::Over => return now,
            }
        }
        panic!("the chain never ended: {chain:?}");
    }

    #[test]
    fn merge_all_three_waits_for_github_and_has_a_behind_branch_updated_by_its_session() {
        let w = Fake::default();
        // #1 merges cleanly.
        w.script(1, vec![pr(1, "open", "mergeable", "clean", "passing", "a1")]);
        // #2: right after #1 lands GitHub works it out again (UNKNOWN twice), then it is clean.
        w.script(2, vec![pr(2, "open", "unknown", "unknown", "passing", "b1"), pr(2, "open", "unknown", "unknown", "passing", "b1"), pr(2, "open", "mergeable", "clean", "none", "b1")]);
        // #3 fell behind dev: its session is asked; it pushes b… then its checks run, then it is clean.
        w.script(3, vec![
            pr(3, "open", "mergeable", "behind", "passing", "c1"),
            pr(3, "open", "mergeable", "behind", "passing", "c1"),
            pr(3, "open", "mergeable", "behind", "passing", "c1"),
            pr(3, "open", "unknown", "unknown", "pending", "c2"),
            pr(3, "open", "mergeable", "blocked", "pending", "c2"),
            pr(3, "open", "mergeable", "clean", "passing", "c2"),
        ]);
        let prs: Vec<Pr> = (1..=3).map(|n| pr(n, "open", "mergeable", "clean", "passing", "x")).collect();
        let mut chain = Chain::new("m1", "d1", Some("shop".into()), "mac", &prs, 0);
        let end = run(&mut chain, &w, 0);
        assert!(chain.links.iter().all(|l| l.step == Step::Merged), "{chain:?}");
        assert_eq!(
            w.did.borrow().as_slice(),
            [
                "merge #1 at a1",
                "landed #1 by mac",
                "merge #2 at b1",
                "landed #2 by mac",
                "ask t3 to fix #3: Bring its branch up to date with dev",
                "merge #3 at c2",
                "landed #3 by mac",
            ]
        );
        assert!(end >= 4 * POLL_MS, "it waited for GitHub and for the session");
        assert_eq!(chain.links[2].fix.as_deref(), Some("behind"));
        assert!(chain.over());
    }

    #[test]
    fn checks_that_still_fail_after_the_fix_come_back_to_the_user_once() {
        let w = Fake::default();
        w.script(1, vec![pr(1, "open", "mergeable", "clean", "passing", "a1")]);
        // #2 conflicts once #1 is in: its session resolves them, and its checks then fail.
        w.script(2, vec![pr(2, "open", "conflicting", "dirty", "passing", "b1"), pr(2, "open", "conflicting", "dirty", "passing", "b1"), pr(2, "open", "mergeable", "unstable", "failing", "b2")]);
        w.script(3, vec![pr(3, "open", "mergeable", "clean", "passing", "c1")]);
        let prs: Vec<Pr> = (1..=3).map(|n| pr(n, "open", "mergeable", "clean", "passing", "x")).collect();
        let mut chain = Chain::new("m2", "d1", None, "mac", &prs, 0);
        run(&mut chain, &w, 0);
        let did = w.did.borrow().clone();
        assert_eq!(did[2], "ask t2 to fix #2: Resolve its conflicts with dev");
        assert_eq!(did.iter().filter(|d| d.starts_with("back")).count(), 1, "once: {did:?}");
        assert_eq!(did.last().unwrap(), "back #2: 1 check failing: test after its session's fix");
        assert!(!did.iter().any(|d| d.contains("#3")), "the rest stays on the card");
        assert_eq!(chain.links[2].step, Step::Queued);
        assert!(matches!(chain.links[1].step, Step::Back(_)));
        // Ticking again does nothing more.
        assert_eq!(tick(&mut chain, &w, 10 * 60_000), Tick::Over);
        assert_eq!(w.did.borrow().len(), did.len());
    }

    #[test]
    fn a_branch_with_no_session_left_is_brought_up_to_date_on_github_and_waits_have_an_end() {
        let w = Fake { no_session: vec![1], ..Default::default() };
        w.script(1, vec![pr(1, "open", "mergeable", "behind", "passing", "a1"), pr(1, "open", "mergeable", "clean", "passing", "a2")]);
        let mut chain = Chain::new("m3", "d1", None, "mac", &[pr(1, "open", "mergeable", "clean", "passing", "x")], 0);
        run(&mut chain, &w, 0);
        assert_eq!(w.did.borrow().as_slice(), ["update-branch #1", "merge #1 at a2", "landed #1 by mac"]);
        // GitHub never works it out: back to the user after RECOMPUTE_MS, not before.
        let w = Fake::default();
        w.script(5, vec![pr(5, "open", "unknown", "unknown", "passing", "e1")]);
        let mut chain = Chain::new("m4", "d1", None, "mac", &[pr(5, "open", "mergeable", "clean", "passing", "x")], 0);
        let end = run(&mut chain, &w, 0);
        assert!(end >= RECOMPUTE_MS && end < RECOMPUTE_MS + 2 * POLL_MS, "{end}");
        assert_eq!(w.did.borrow().as_slice(), ["back #5: GitHub didn't work out whether it merges cleanly"]);
        // Merged by someone else meanwhile, or closed: passed on.
        let w = Fake::default();
        w.script(6, vec![pr(6, "merged", "unknown", "", "passing", "f1")]);
        w.script(7, vec![pr(7, "closed", "unknown", "", "passing", "g1")]);
        let mut chain = Chain::new("m5", "d1", None, "mac", &[pr(6, "open", "", "", "", "x"), pr(7, "open", "", "", "", "x")], 0);
        run(&mut chain, &w, 0);
        assert_eq!(w.did.borrow().as_slice(), ["landed #6 by github"]);
        assert_eq!(chain.links[1].step, Step::Skipped("it was closed".into()));
    }

    #[test]
    fn its_session_is_asked_in_words_it_can_act_on() {
        let p = pr(1375, "open", "mergeable", "behind", "passing", "x");
        let t = fix_request(&Pr { repo: "eyzhub/api".into(), ..p }, "behind", Some(1374));
        assert_eq!(
            t,
            "The user asked the tower to merge PR #1375 (eyzhub/api) into dev in order with the others after #1374 went in, but its branch is behind dev and the repo wants it up to date. \
             Bring its branch up to date with dev (merge dev into the branch, run the tests), push, and end your turn. \
             The tower merges it by itself once GitHub sees it clean: don't merge it yourself."
        );
    }
}
