//! The merge chains in the daemon (see `crate::merges`): "Merge all N",
//! confirmed on a merge card, runs here, each chain on a thread of its own
//! with a look every `POLL_MS`, so it outlives the window and waits as long
//! as GitHub and the PRs' sessions take. The state carries every chain
//! (`merges`), so the cards show where each PR stands.
//!
//! - GitHub: `prs::fetch` to look, `prs::merge` to merge (pinned to the
//!   head looked at, the repo's method, never `--admin`), `prs::update_branch`
//!   for a branch with no session left.
//! - A PR that landed is reported as any merge from the Mac (`Prs::report_merged`):
//!   its chip moves, its session and the brain are told.
//! - A PR's session is asked through the send queue, as an instruction.
//! - Back to the user: a line in the record and on the card; the merge
//!   decision stays open with the PRs that are left.

use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::sendq::SendQueue;
use super::state::{Shared, lock};
use crate::merges::{self, Chain, Tick};
use crate::prs::{Pr, Prs, Refusal};

/// Chains that ended stay in the state this long (the card says how it ended).
const KEEP_OVER_MS: u64 = 10 * 60_000;

#[derive(Clone)]
pub struct Merges {
    chains: Arc<Mutex<Vec<Chain>>>,
    shared: Shared,
    prs: Prs,
    sendq: SendQueue,
}

static RUNNING: OnceLock<Merges> = OnceLock::new();

pub fn get() -> Option<&'static Merges> {
    RUNNING.get()
}

impl Merges {
    pub fn new(shared: Shared, prs: Prs, sendq: SendQueue) -> Merges {
        Merges { chains: Arc::default(), shared, prs, sendq }
    }

    pub fn start(&self) {
        let _ = RUNNING.set(self.clone());
    }

    /// `start` (the PRs `prs`, in order, of the merge card `card`, as `by`
    /// confirmed them), `stop` (`id`: nothing more is merged), `list`.
    pub fn request(&self, action: &str, id: &str, card: &str, prs: &[String], by: &str) -> Result<Value> {
        match action {
            "start" => self.begin(card, prs, by),
            "stop" => {
                let mut l = lock(&self.chains);
                let Some(c) = l.iter_mut().find(|c| c.id == id || (!card.is_empty() && c.card == card && !c.over())) else { bail!("no merge chain {id}") };
                c.stopped = true;
                let v = view(c);
                drop(l);
                self.publish();
                eprintln!("[merges] {id}: stopped by the user");
                Ok(v)
            }
            "list" | "" => Ok(json!({ "merges": lock(&self.chains).iter().map(view).collect::<Vec<_>>() })),
            other => bail!("merge chain {other}: use start, stop or list"),
        }
    }

    fn begin(&self, card: &str, keys: &[String], by: &str) -> Result<Value> {
        if keys.is_empty() {
            bail!("no pull requests to merge");
        }
        let mut prs = vec![];
        for k in keys {
            let pr = self.prs.get(k).with_context(|| format!("{k} isn't a pull request the tower knows"))?;
            if !pr.open() {
                bail!("{k} isn't open");
            }
            prs.push(pr);
        }
        let now = crate::history::now_ms();
        let chain = {
            let mut l = lock(&self.chains);
            if let Some(c) = l.iter().find(|c| !c.over() && (c.card == card || c.links.iter().any(|x| keys.contains(&x.key)))) {
                bail!("{} is merging them already", c.id);
            }
            l.retain(|c| !c.over() || now.saturating_sub(c.started_ms) < KEEP_OVER_MS * 6);
            let id = format!("m{:x}", now & 0xffff_ffff);
            let chain = Chain::new(&id, card, prs[0].space.clone(), by, &prs, now);
            l.push(chain.clone());
            chain
        };
        eprintln!("[merges] {}: merging {} in order, as {by} confirmed", chain.id, keys.join(", "));
        self.publish();
        let (me, id) = (self.clone(), chain.id.clone());
        thread::Builder::new().name(format!("merge {id}")).spawn(move || me.run(&id)).context("starting the merge chain")?;
        Ok(view(&chain))
    }

    /// Runs chain `id` to its end. Each look is made on a copy, so the
    /// state stays readable while GitHub is asked.
    fn run(&self, id: &str) {
        let world = Real { prs: self.prs.clone(), sendq: self.sendq.clone() };
        loop {
            let Some(mut chain) = lock(&self.chains).iter().find(|c| c.id == id).cloned() else { return };
            let tick = merges::tick(&mut chain, &world, crate::history::now_ms());
            {
                let mut l = lock(&self.chains);
                if let Some(c) = l.iter_mut().find(|c| c.id == id) {
                    // Stopped meanwhile: kept so.
                    chain.stopped |= c.stopped;
                    *c = chain.clone();
                }
            }
            self.publish();
            match tick {
                Tick::Again => {}
                Tick::Later => thread::sleep(Duration::from_millis(merges::POLL_MS)),
                Tick::Over => {
                    let merged: Vec<String> = chain.links.iter().filter(|l| l.step == merges::Step::Merged).map(|l| format!("#{}", l.number)).collect();
                    eprintln!("[merges] {id}: over ({} merged{})", merged.len(), if chain.stopped { ", stopped" } else { "" });
                    return;
                }
            }
        }
    }

    fn publish(&self) {
        let now = crate::history::now_ms();
        let views: Vec<Value> = lock(&self.chains).iter().filter(|c| !c.over() || now.saturating_sub(c.started_ms) < KEEP_OVER_MS * 6).map(view).collect();
        self.shared.update(|s| s.merges = views);
    }
}

fn view(c: &Chain) -> Value {
    let mut v = serde_json::to_value(c).unwrap_or_default();
    v["over"] = c.over().into();
    for (i, l) in c.links.iter().enumerate() {
        v["links"][i]["line"] = l.step.line().into();
    }
    v
}

/// GitHub and the tower, for real.
struct Real {
    prs: Prs,
    sendq: SendQueue,
}

impl merges::World for Real {
    fn look(&self, key: &str) -> Result<Pr> {
        let mut pr = self.prs.get(key).with_context(|| format!("{key} is no longer known"))?;
        let v = crate::prs::fetch(self.prs.gh(), &pr.repo, pr.number)?;
        crate::prs::apply_gh(&mut pr, &v);
        Ok(pr)
    }

    fn merge(&self, pr: &Pr, sha: &str) -> Result<Pr, Refusal> {
        eprintln!("[merges] merging {} at {sha}", pr.key());
        crate::prs::merge(self.prs.gh(), &pr.repo, pr.number, sha, false)
    }

    fn update_branch(&self, pr: &Pr) -> Result<()> {
        eprintln!("[merges] {}: bringing its branch up to date on GitHub", pr.key());
        crate::prs::update_branch(self.prs.gh(), &pr.repo, pr.number)
    }

    fn ask(&self, pr: &Pr, text: &str) -> Result<()> {
        if pr.session.is_empty() || !crate::hc::is_live(&pr.session) {
            bail!("its session isn't running");
        }
        self.sendq.add(&pr.session, text, &[])?;
        eprintln!("[merges] {}: its session ({}) is asked to fix it", pr.key(), pr.session);
        Ok(())
    }

    fn landed(&self, pr: &Pr, by: &str) {
        eprintln!("[merges] {} landed", pr.key());
        if self.prs.report_merged(&pr.key(), by).is_none() {
            eprintln!("[merges] {}: GitHub doesn't say it's merged yet: the chip follows with the next look", pr.key());
        }
    }

    fn back(&self, chain: &Chain, pr: &Pr, why: &str) {
        let merged: Vec<String> = chain.links.iter().filter(|l| l.step == merges::Step::Merged).map(|l| format!("#{}", l.number)).collect();
        let left: Vec<String> = chain.links.iter().filter(|l| !l.step.done()).map(|l| format!("#{}", l.number)).collect();
        let text = format!(
            "Merging in order stopped at #{}: {why}.{} Left on the card for you: {}.",
            pr.number,
            if merged.is_empty() { String::new() } else { format!(" Merged before it: {}.", merged.join(", ")) },
            left.join(", ")
        );
        eprintln!("[merges] {}: back to the user: {why}", chain.id);
        if !cfg!(test) {
            let entry = crate::history::Entry { kind: "merge".into(), status: Some("back".into()), space: chain.space.clone(), text, record: true, ..Default::default() };
            crate::history::later(move || crate::history::append(entry));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merges::{Step, World};

    /// A `gh` that plays a script: each `pr view` of a PR answers its next
    /// look (the last one stays); `pr merge` makes it merged; `pr
    /// update-branch` moves it on to its looks after the update.
    fn fake_gh(dir: &std::path::Path, scenario: Value) -> std::path::PathBuf {
        std::fs::write(dir.join("scenario.json"), scenario.to_string()).unwrap();
        let script = dir.join("gh");
        std::fs::write(
            &script,
            r#"#!/usr/bin/python3 -I
import json, os, sys
d = os.path.dirname(os.path.abspath(__file__))
a = sys.argv[1:]
open(os.path.join(d, "calls.log"), "a").write(" ".join(a) + "\n")
sc = json.load(open(os.path.join(d, "scenario.json")))
stp = os.path.join(d, "state.json")
st = json.load(open(stp)) if os.path.exists(stp) else {}
def save(): json.dump(st, open(stp, "w"))
if a[:2] == ["repo", "view"]:
    print('{"viewerDefaultMergeMethod":"MERGE"}'); sys.exit(0)
n = a[2]
p = sc[n]
if a[:2] == ["pr", "view"]:
    if st.get("merged-" + n):
        v = dict(p["looks"][-1]); v["state"] = "MERGED"; print(json.dumps(v)); sys.exit(0)
    looks = p["after_update"] if st.get("updated-" + n) else p["looks"]
    i = st.get("seen-" + n, 0); st["seen-" + n] = i + 1; save()
    print(json.dumps(looks[min(i, len(looks) - 1)])); sys.exit(0)
if a[:2] == ["pr", "merge"]:
    st["merged-" + n] = True; save(); sys.exit(0)
if a[:2] == ["pr", "update-branch"]:
    st["updated-" + n] = True; st["seen-" + n] = 0; save(); sys.exit(0)
print("fake gh: " + " ".join(a), file=sys.stderr); sys.exit(1)
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn look(n: u64, mergeable: &str, state: &str, sha: &str) -> Value {
        json!({
            "number": n, "title": format!("PR {n}"), "url": format!("https://github.com/acme/api/pull/{n}"), "state": "OPEN", "isDraft": false,
            "mergeable": mergeable, "mergeStateStatus": state, "baseRefName": "dev", "headRefName": format!("b{n}"), "headRefOid": sha,
            "statusCheckRollup": [{ "__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-11-13T12:00:00Z" }],
        })
    }

    /// Against a fake GitHub (nothing real is merged): #1 merges at once; #2
    /// is UNKNOWN twice while GitHub works it out after #1 landed, then
    /// clean; #3 has fallen behind dev, and with no session to ask its
    /// branch is brought up to date on GitHub, then it merges at its new head.
    #[test]
    fn a_three_pr_chain_merges_in_order_against_a_fake_github() {
        let dir = std::env::temp_dir().join(format!("tower-merges-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gh = fake_gh(
            &dir,
            json!({
                "1": { "looks": [look(1, "MERGEABLE", "CLEAN", "a1")] },
                "2": { "looks": [look(2, "UNKNOWN", "UNKNOWN", "b1"), look(2, "UNKNOWN", "UNKNOWN", "b1"), look(2, "MERGEABLE", "CLEAN", "b1")] },
                "3": { "looks": [look(3, "MERGEABLE", "BEHIND", "c1")], "after_update": [look(3, "UNKNOWN", "UNKNOWN", "c2"), look(3, "MERGEABLE", "CLEAN", "c2")] },
            }),
        );
        let known: Vec<Pr> = (1..=3)
            .map(|n| Pr { repo: "acme/api".into(), number: n, base: "dev".into(), state: "open".into(), mergeable: "mergeable".into(), space: Some("shop".into()), waiting: true, ..Default::default() })
            .collect();
        std::fs::write(dir.join("prs.json"), json!({ "prs": known }).to_string()).unwrap();
        let prs = Prs::at(dir.join("prs.json"), gh);
        let world = Real { prs: prs.clone(), sendq: SendQueue::at(dir.join("queue.json")) };
        let mut chain = Chain::new("m1", "d1", Some("shop".into()), "mac", &known, 0);
        let mut now = 0;
        for _ in 0..40 {
            match merges::tick(&mut chain, &world, now) {
                Tick::Again => {}
                Tick::Later => now += merges::POLL_MS,
                Tick::Over => break,
            }
        }
        assert!(chain.links.iter().all(|l| l.step == Step::Merged), "{chain:?}");
        let calls = std::fs::read_to_string(dir.join("calls.log")).unwrap();
        let did: Vec<&str> = calls.lines().filter(|l| l.starts_with("pr merge") || l.starts_with("pr update-branch")).collect();
        assert_eq!(
            did,
            [
                "pr merge 1 --repo acme/api --merge --match-head-commit a1",
                "pr merge 2 --repo acme/api --merge --match-head-commit b1",
                "pr update-branch 3 --repo acme/api",
                "pr merge 3 --repo acme/api --merge --match-head-commit c2",
            ]
        );
        // Each landed as a merge from the Mac: the store has them merged.
        assert!((1..=3).all(|n| prs.get(&format!("acme/api#{n}")).is_some_and(|p| p.state == "merged")));
        assert_eq!(world.look("acme/api#2").unwrap().merge_state, "clean", "GitHub's merge state is read");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
