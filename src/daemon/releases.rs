//! The release tracks in the daemon (see `crate::releases`): every 90
//! seconds, for each space that has PRs the tower tracks (found in the last
//! two weeks), each repo's track is worked out from GitHub and shown in
//! `state.json` as `releases` (by space), which the windows and the phones
//! read (`GET /v1/releases`).
//!
//! What it asks GitHub (`gh`), each answer kept as long as it can't change:
//!
//! - the repo's release PRs (`gh pr list --state all --json …`, those from a
//!   dev branch into a prod one);
//! - whether a commit is in another's history (`gh api
//!   repos/<repo>/compare/<commit>...<tip>`: `ahead` or `identical`);
//! - the last successful deployment of dev (`gh api
//!   repos/<repo>/deployments`, then its statuses);
//! - a merged PR's merge commit and time, for one the tower found before it
//!   kept them (`gh pr view --json mergeCommit,mergedAt`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::state::Shared;
use crate::prs::{Pr, Prs};
use crate::releases::{self, Deployed, Git, ReleasePr};

/// How often the tracks are worked out again.
const EVERY: Duration = Duration::from_secs(90);
/// A space's PRs are followed this long after they were found.
const FOLLOW_MS: u64 = 14 * 24 * 3_600_000;

/// GitHub, through `gh`, with what can't change kept.
pub struct Github {
    gh: PathBuf,
    /// (repo, commit, tip) → contained.
    contains: Mutex<HashMap<(String, String, String), bool>>,
    /// (repo, number) → (merge commit, merged at).
    merges: Mutex<HashMap<(String, u64), (String, Option<u64>)>>,
}

fn gh_json(bin: &Path, args: &[&str]) -> Option<Value> {
    let out = Command::new(bin).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

impl Github {
    pub fn new(gh: PathBuf) -> Github {
        Github { gh, contains: Mutex::default(), merges: Mutex::default() }
    }

    /// The repo's release PRs (dev into prod), the latest 30.
    pub fn release_prs(&self, repo: &str) -> Vec<ReleasePr> {
        let v = gh_json(&self.gh, &["pr", "list", "--repo", repo, "--state", "all", "--limit", "60", "--json", "number,title,url,headRefName,baseRefName,headRefOid,state,mergedAt"]);
        let mut out: Vec<ReleasePr> = v.as_ref().and_then(Value::as_array).into_iter().flatten().filter_map(|x| ReleasePr::from_gh(repo, x)).collect();
        out.truncate(30);
        out
    }

    /// The last successful deployment of `dev` (its branch, or an
    /// environment called dev, development or staging).
    pub fn deployed(&self, repo: &str, dev: &str) -> Option<Deployed> {
        let list = gh_json(&self.gh, &["api", &format!("repos/{repo}/deployments?per_page=20")])?;
        let dev_like = |d: &Value| {
            let env = d["environment"].as_str().unwrap_or("").to_lowercase();
            d["ref"].as_str() == Some(dev) || (!env.contains("prod") && ["dev", "development", "develop", "staging"].iter().any(|e| env == *e || env.starts_with(&format!("{e}-")) || env.ends_with(&format!("-{e}"))))
        };
        for d in list.as_array().into_iter().flatten().filter(|d| dev_like(d)).take(3) {
            let id = d["id"].as_u64()?;
            let st = gh_json(&self.gh, &["api", &format!("repos/{repo}/deployments/{id}/statuses?per_page=1")]);
            let first = st.as_ref().and_then(|s| s.as_array()).and_then(|a| a.first().cloned());
            if let Some(s) = first.filter(|s| s["state"] == "success") {
                return Some(Deployed {
                    sha: d["sha"].as_str().unwrap_or("").to_string(),
                    at_ms: s["created_at"].as_str().and_then(releases::ms_of_iso).unwrap_or(0),
                    environment: d["environment"].as_str().unwrap_or("").to_string(),
                });
            }
        }
        None
    }

    /// A merged PR's merge commit and when it merged (GitHub asked once).
    pub fn merge_of(&self, repo: &str, number: u64) -> Option<(String, Option<u64>)> {
        let k = (repo.to_string(), number);
        if let Some(m) = self.merges.lock().unwrap_or_else(|e| e.into_inner()).get(&k) {
            return Some(m.clone());
        }
        let v = gh_json(&self.gh, &["pr", "view", &number.to_string(), "--repo", repo, "--json", "mergeCommit,mergedAt"])?;
        let m = (v["mergeCommit"]["oid"].as_str().unwrap_or("").to_string(), v["mergedAt"].as_str().and_then(releases::ms_of_iso));
        if !m.0.is_empty() {
            self.merges.lock().unwrap_or_else(|e| e.into_inner()).insert(k, m.clone());
        }
        Some(m)
    }
}

impl Git for Github {
    fn contains(&self, repo: &str, commit: &str, tip: &str) -> Option<bool> {
        if commit == tip {
            return Some(true);
        }
        let k = (repo.to_string(), commit.to_string(), tip.to_string());
        if let Some(b) = self.contains.lock().unwrap_or_else(|e| e.into_inner()).get(&k) {
            return Some(*b);
        }
        let v = gh_json(&self.gh, &["api", &format!("repos/{repo}/compare/{commit}...{tip}")])?;
        // `ahead`: the tip is ahead of the commit, which is in its history.
        let b = matches!(v["status"].as_str()?, "ahead" | "identical");
        self.contains.lock().unwrap_or_else(|e| e.into_inner()).insert(k, b);
        Some(b)
    }
}

#[derive(Clone)]
pub struct Releases {
    prs: Prs,
    shared: Shared,
    github: Arc<Github>,
}

impl Releases {
    pub fn new(prs: Prs, shared: Shared, gh: PathBuf) -> Releases {
        Releases { prs, shared, github: Arc::new(Github::new(gh)) }
    }

    /// Works the tracks out again from now on, every 90 seconds.
    pub fn start(&self) {
        let me = self.clone();
        let _ = thread::Builder::new().name("releases".into()).spawn(move || {
            loop {
                me.refresh();
                thread::sleep(EVERY);
            }
        });
    }

    /// The PRs followed: found in the last two weeks, in a space; a merged
    /// one without its merge commit (found before it was kept) gets it from GitHub.
    fn followed(&self, now: u64) -> Vec<Pr> {
        self.prs
            .list()
            .into_iter()
            .filter(|p| p.space.as_deref().is_some_and(|s| !s.trim().is_empty()) && now.saturating_sub(p.found_ms) < FOLLOW_MS)
            .map(|mut p| {
                if p.state == "merged" && (p.merge_commit.is_empty() || p.merged_ms.is_none())
                    && let Some((c, at)) = self.github.merge_of(&p.repo, p.number)
                {
                    if p.merge_commit.is_empty() {
                        p.merge_commit = c;
                    }
                    p.merged_ms = p.merged_ms.or(at).or(p.closed_ms);
                }
                p
            })
            .collect()
    }

    /// Every space's track, worked out now and shown in the state.
    pub fn refresh(&self) -> BTreeMap<String, Value> {
        let now = crate::history::now_ms();
        let prs = self.followed(now);
        let mut spaces: Vec<String> = prs.iter().filter_map(|p| p.space.clone()).collect();
        spaces.sort_by_key(|s| s.to_lowercase());
        spaces.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        let mut repos: Vec<String> = prs.iter().map(|p| p.repo.clone()).collect();
        repos.sort();
        repos.dedup();
        let mut rels: Vec<ReleasePr> = vec![];
        let mut deployed: BTreeMap<String, Deployed> = BTreeMap::new();
        for r in &repos {
            rels.extend(self.github.release_prs(r));
            let dev = prs.iter().filter(|p| &p.repo == r).map(|p| p.base.as_str()).find(|b| crate::prscore::is_dev(b)).unwrap_or("dev");
            if let Some(d) = self.github.deployed(r, dev) {
                deployed.insert(r.clone(), d);
            }
        }
        let out: BTreeMap<String, Value> = spaces.iter().map(|s| (s.clone(), releases::track(s, &prs, &rels, &deployed, &*self.github, now).view(now))).collect();
        let v = serde_json::to_value(&out).unwrap_or_default();
        self.shared.update(|s| s.releases = v.clone());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::State;
    use crate::prs::tests::fake_gh;

    /// The fake `gh` answers what the tracks ask: `pr list` from `list.json`,
    /// `api …/compare/A...B` from git in the scratch checkout, deployments from `deployments.json`.
    fn with_github(gh: &Path, scratch: &Path) {
        let script = std::fs::read_to_string(gh).unwrap().replace(
            "  \"pr diff\")",
            &format!(
                "  \"pr list\") cat \"$D/list.json\" ;;\n  \"api repos/eyzhub/shop-api/deployments?per_page=20\") cat \"$D/deployments.json\" 2>/dev/null || echo '[]' ;;\n  \"api repos/eyzhub/shop-api/deployments/7/statuses?per_page=1\") echo '[{{\"state\":\"success\",\"created_at\":\"2026-11-13T13:40:00Z\"}}]' ;;\n  \"api repos/eyzhub/shop-api/compare/\"*) r=\"${{2##*/}}\"; a=\"${{r%%...*}}\"; b=\"${{r##*...}}\"; if git -C '{}' merge-base --is-ancestor \"$a\" \"$b\"; then echo '{{\"status\":\"ahead\"}}'; else echo '{{\"status\":\"diverged\"}}'; fi ;;\n  \"pr diff\")",
                scratch.display()
            ),
        );
        std::fs::write(gh, script).unwrap();
    }

    #[test]
    fn a_spaces_track_is_worked_out_from_github_and_shown_in_the_state() {
        let (scratch, [m829, r1, m830, _r2]) = crate::releases::tests::scratch();
        let gh = fake_gh("releases");
        with_github(&gh, &scratch);
        // Two changes merged into dev; one release landed, carrying the first.
        let list = serde_json::json!([
            { "number": 840, "title": "release: dev into main", "url": "u840", "headRefName": "dev", "baseRefName": "main", "headRefOid": r1, "state": "MERGED", "mergedAt": "2026-11-13T13:20:00Z" },
            { "number": 829, "title": "a feature", "headRefName": "feat-829", "baseRefName": "dev", "headRefOid": "x", "state": "MERGED" },
        ]);
        std::fs::write(gh.with_file_name("list.json"), list.to_string()).unwrap();
        std::fs::write(gh.with_file_name("deployments.json"), serde_json::json!([{ "id": 7, "sha": m830, "ref": "dev", "environment": "dev" }]).to_string()).unwrap();
        let prs = Prs::at(gh.with_file_name("prs.json"), gh.clone());
        let now = crate::history::now_ms();
        for p in [crate::releases::tests::pr(829, "dev", "merged", &m829, Some(now - 60 * 60_000)), crate::releases::tests::pr(830, "dev", "merged", &m830, Some(now - 30 * 60_000))] {
            prs.insert_for_test(Pr { found_ms: now, ..p });
        }
        let shared = Shared::new(State::new("off", "test", "test"));
        let r = Releases::new(prs, shared.clone(), gh.clone());
        let out = r.refresh();
        let t = &out["checkout"]["repos"][0];
        assert_eq!(t["changes"][0]["stage"], "released");
        assert_eq!(t["changes"][0]["release"], 840);
        assert_eq!(t["changes"][1]["stage"], "deployed", "on dev, deployed there: {t}");
        assert_eq!(t["on_dev_not_prod"], serde_json::json!([830]));
        let lines: Vec<&str> = t["lines"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(lines[0].starts_with("On main since ") && lines[0].ends_with(": #829"), "{lines:?}");
        assert_eq!(lines[1], "On dev, not on main: #830");
        assert_eq!(shared.get().releases["checkout"]["repos"][0]["changes"][1]["stage"], "deployed", "in the state");
        // The answers that can't change are kept: compare isn't asked twice for the same commits.
        let asked = crate::prs::tests::calls(&gh).matches("compare/").count();
        r.refresh();
        assert_eq!(crate::prs::tests::calls(&gh).matches("compare/").count(), asked);
    }
}
