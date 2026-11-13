//! The release tracks in the daemon (see `crate::releases`): every 90
//! seconds, for each space that has PRs the tower tracks (found in the last
//! two weeks), each repo's track is worked out from GitHub and shown in
//! `state.json` as `releases` (by space), which the windows and the phones
//! read (`GET /v1/releases`).
//!
//! What it asks GitHub (`gh`), as little as it can:
//!
//! - **one list per repo** (`gh pr list --state all --limit 100 --json …`):
//!   its release PRs (from a dev branch into a prod one), and the merge
//!   commit and time of every recent PR, so a PR the tower found before it
//!   kept them needs no call of its own (`gh pr view` only for one older
//!   than the list);
//! - whether a commit is in a release's head (`gh api
//!   repos/<repo>/compare/<commit>...<tip>`: `ahead` or `identical`), never
//!   for a release that landed before the PR merged (`crate::releases::track`);
//! - the last successful deployment of dev (`gh api
//!   repos/<repo>/deployments`, then its statuses).
//!
//! What can't change (a commit in another's history, a merged PR's merge
//! commit) is kept in `releases-cache.json` in the state folder, so a
//! restart starts warm: its first refresh asks only what is new.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::state::Shared;
use crate::prs::{Pr, Prs};
use crate::releases::{self, Deployed, Git, ReleasePr};

/// How often the tracks are worked out again.
const EVERY: Duration = Duration::from_secs(90);
/// A space's PRs are followed this long after they were found.
const FOLLOW_MS: u64 = 14 * 24 * 3_600_000;
/// How many answers the cache file keeps, of each kind (the newest).
const KEEP: usize = 5_000;

/// What is kept across restarts: answers that can't change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cache {
    /// (repo, commit, tip, the tip contains the commit).
    pub contains: Vec<(String, String, String, bool)>,
    /// (repo, number, merge commit, merged at).
    pub merges: Vec<(String, u64, String, Option<u64>)>,
}

/// The facts of one PR's merge: its merge commit and when.
type Merge = (String, Option<u64>);

/// GitHub, through `gh`, with what can't change kept (in memory, and in its cache file).
pub struct Github {
    gh: PathBuf,
    /// Where the cache is kept (none: in memory only).
    file: Option<PathBuf>,
    contains: Mutex<HashMap<(String, String, String), bool>>,
    merges: Mutex<HashMap<(String, u64), Merge>>,
    /// Something new to write.
    dirty: Mutex<bool>,
}

fn gh_json(bin: &Path, args: &[&str]) -> Option<Value> {
    let out = Command::new(bin).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Github {
    /// `file`: the cache to start from and keep (none: in memory only).
    pub fn new(gh: PathBuf, file: Option<PathBuf>) -> Github {
        let cache: Cache = file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Github {
            gh,
            file,
            contains: Mutex::new(cache.contains.into_iter().map(|(r, c, t, b)| ((r, c, t), b)).collect()),
            merges: Mutex::new(cache.merges.into_iter().map(|(r, n, c, at)| ((r, n), (c, at))).collect()),
            dirty: Mutex::new(false),
        }
    }

    /// The cache file written again, when something new was learned.
    pub fn save(&self) {
        let Some(file) = &self.file else { return };
        let mut dirty = lock(&self.dirty);
        if !*dirty {
            return;
        }
        let mut contains: Vec<(String, String, String, bool)> = lock(&self.contains).iter().map(|((r, c, t), b)| (r.clone(), c.clone(), t.clone(), *b)).collect();
        let mut merges: Vec<(String, u64, String, Option<u64>)> = lock(&self.merges).iter().map(|((r, n), (c, at))| (r.clone(), *n, c.clone(), *at)).collect();
        contains.sort();
        merges.sort_by_key(|m| std::cmp::Reverse(m.3));
        let skip = contains.len().saturating_sub(KEEP);
        contains.drain(..skip);
        merges.truncate(KEEP);
        let text = serde_json::to_vec(&Cache { contains, merges }).unwrap_or_default();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, file).is_ok() {
            *dirty = false;
        }
    }

    fn learned(&self) {
        *lock(&self.dirty) = true;
    }

    /// One list of the repo's recent PRs: its release PRs (dev into prod,
    /// the latest 30), and every merged one's merge commit and time (kept).
    pub fn repo_prs(&self, repo: &str) -> Vec<ReleasePr> {
        let v = gh_json(&self.gh, &["pr", "list", "--repo", repo, "--state", "all", "--limit", "100", "--json", "number,title,url,headRefName,baseRefName,headRefOid,state,mergedAt,mergeCommit"]);
        let list: Vec<Value> = v.as_ref().and_then(Value::as_array).cloned().unwrap_or_default();
        {
            let mut merges = lock(&self.merges);
            for x in &list {
                let (Some(n), Some(c)) = (x["number"].as_u64(), x["mergeCommit"]["oid"].as_str().filter(|c| !c.is_empty())) else { continue };
                let k = (repo.to_string(), n);
                if !merges.contains_key(&k) {
                    merges.insert(k, (c.to_string(), x["mergedAt"].as_str().and_then(releases::ms_of_iso)));
                    *lock(&self.dirty) = true;
                }
            }
        }
        let mut out: Vec<ReleasePr> = list.iter().filter_map(|x| ReleasePr::from_gh(repo, x)).collect();
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

    /// A merged PR's merge commit and when it merged: from the repo's list
    /// or the cache; else GitHub asked for that one PR (kept).
    pub fn merge_of(&self, repo: &str, number: u64) -> Option<Merge> {
        let k = (repo.to_string(), number);
        if let Some(m) = lock(&self.merges).get(&k) {
            return Some(m.clone());
        }
        let v = gh_json(&self.gh, &["pr", "view", &number.to_string(), "--repo", repo, "--json", "mergeCommit,mergedAt"])?;
        let m = (v["mergeCommit"]["oid"].as_str().unwrap_or("").to_string(), v["mergedAt"].as_str().and_then(releases::ms_of_iso));
        if !m.0.is_empty() {
            lock(&self.merges).insert(k, m.clone());
            self.learned();
        }
        Some(m)
    }
}

impl Github {
    /// Asks GitHub about these (repo, commit, tip) at once, a few at a time,
    /// those not known yet: the track then finds them known.
    pub fn prefetch(&self, pairs: &[(String, String, String)], at_once: usize) {
        let todo: Vec<&(String, String, String)> = {
            let known = lock(&self.contains);
            let mut v: Vec<&(String, String, String)> = pairs.iter().filter(|k| k.1 != k.2 && !known.contains_key(*k)).collect();
            v.sort();
            v.dedup();
            v
        };
        if todo.is_empty() {
            return;
        }
        let at_once = at_once.max(1);
        std::thread::scope(|s| {
            for part in todo.chunks(todo.len().div_ceil(at_once)) {
                s.spawn(move || {
                    for (repo, commit, tip) in part {
                        self.contains(repo, commit, tip);
                    }
                });
            }
        });
    }
}

impl Git for Github {
    fn contains(&self, repo: &str, commit: &str, tip: &str) -> Option<bool> {
        if commit == tip {
            return Some(true);
        }
        let k = (repo.to_string(), commit.to_string(), tip.to_string());
        if let Some(b) = lock(&self.contains).get(&k) {
            return Some(*b);
        }
        let v = gh_json(&self.gh, &["api", &format!("repos/{repo}/compare/{commit}...{tip}")])?;
        // `ahead`: the tip is ahead of the commit, which is in its history.
        let b = matches!(v["status"].as_str()?, "ahead" | "identical");
        lock(&self.contains).insert(k, b);
        self.learned();
        Some(b)
    }
}

#[derive(Clone)]
pub struct Releases {
    prs: Prs,
    shared: Shared,
    github: Arc<Github>,
}

/// Where the daemon keeps what GitHub said that can't change.
pub fn cache_path() -> PathBuf {
    crate::config::state_dir().join("releases-cache.json")
}

impl Releases {
    /// `cache`: the file what can't change is kept in (none: in memory only).
    pub fn new(prs: Prs, shared: Shared, gh: PathBuf, cache: Option<PathBuf>) -> Releases {
        Releases { prs, shared, github: Arc::new(Github::new(gh, cache)) }
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

    /// Every space's track, worked out now and shown in the state.
    pub fn refresh(&self) -> BTreeMap<String, Value> {
        let now = crate::history::now_ms();
        let started = std::time::Instant::now();
        let mut prs: Vec<Pr> = self
            .prs
            .list()
            .into_iter()
            .filter(|p| p.space.as_deref().is_some_and(|s| !s.trim().is_empty()) && now.saturating_sub(p.found_ms) < FOLLOW_MS)
            .collect();
        let mut repos: Vec<String> = prs.iter().map(|p| p.repo.clone()).collect();
        repos.sort();
        repos.dedup();
        // One list per repo: its releases, and its PRs' merge commits.
        let mut rels: Vec<ReleasePr> = vec![];
        let mut deployed: BTreeMap<String, Deployed> = BTreeMap::new();
        for r in &repos {
            rels.extend(self.github.repo_prs(r));
            let dev = prs.iter().filter(|p| &p.repo == r).map(|p| p.base.as_str()).find(|b| crate::prscore::is_dev(b)).unwrap_or("dev").to_string();
            if let Some(d) = self.github.deployed(r, &dev) {
                deployed.insert(r.clone(), d);
            }
        }
        // A merged PR found before the tower kept its merge commit: from the list (or GitHub, once).
        for p in prs.iter_mut().filter(|p| p.state == "merged" && (p.merge_commit.is_empty() || p.merged_ms.is_none())) {
            if let Some((c, at)) = self.github.merge_of(&p.repo, p.number) {
                if p.merge_commit.is_empty() {
                    p.merge_commit = c;
                }
                p.merged_ms = p.merged_ms.or(at).or(p.closed_ms);
            }
        }
        // The compare each merged change most likely needs (the first release that
        // landed after it merged, and dev's deployment), asked a few at a time.
        let mut pairs: Vec<(String, String, String)> = vec![];
        for p in prs.iter().filter(|p| p.state == "merged" && crate::prscore::is_dev(&p.base)) {
            let commit = if p.merge_commit.is_empty() { &p.head_sha } else { &p.merge_commit };
            if commit.is_empty() {
                continue;
            }
            let next = rels
                .iter()
                .filter(|r| r.repo == p.repo && r.state == "merged" && r.merged_ms.zip(p.merged_ms).is_none_or(|(rm, pm)| rm >= pm))
                .min_by_key(|r| r.merged_ms.unwrap_or(u64::MAX));
            match (next, deployed.get(&p.repo)) {
                (Some(r), _) => pairs.push((p.repo.clone(), commit.clone(), r.head_sha.clone())),
                // Nothing released since it merged: whether dev's deployment has it.
                (None, Some(d)) => pairs.push((p.repo.clone(), commit.clone(), d.sha.clone())),
                (None, None) => {}
            }
        }
        self.github.prefetch(&pairs, 8);
        let mut spaces: Vec<String> = prs.iter().filter_map(|p| p.space.clone()).collect();
        spaces.sort_by_key(|s| s.to_lowercase());
        spaces.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        let out: BTreeMap<String, Value> = spaces.iter().map(|s| (s.clone(), releases::track(s, &prs, &rels, &deployed, &*self.github, now).view(now))).collect();
        self.github.save();
        let v = serde_json::to_value(&out).unwrap_or_default();
        self.shared.update(|s| s.releases = v.clone());
        eprintln!("[releases] {} space(s), {} repo(s) in {} ms", out.len(), repos.len(), started.elapsed().as_millis());
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
        // #829 merged before the release landed (13:20 UTC); #830 after it.
        let landed = crate::releases::ms_of_iso("2026-11-13T13:20:00Z").unwrap();
        for p in [crate::releases::tests::pr(829, "dev", "merged", &m829, Some(landed - 30 * 60_000)), crate::releases::tests::pr(830, "dev", "merged", &m830, Some(landed + 30 * 60_000))] {
            prs.insert_for_test(Pr { found_ms: now, ..p });
        }
        let shared = Shared::new(State::new("off", "test", "test"));
        let r = Releases::new(prs, shared.clone(), gh.clone(), None);
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

    /// "2026-11-13T<h>:<m>:00Z", minutes past 08:00.
    fn iso(minutes: u64) -> String {
        format!("2026-11-13T{:02}:{:02}:00Z", 8 + minutes / 60, minutes % 60)
    }

    #[test]
    fn the_first_refresh_asks_github_one_list_per_repo_and_a_restart_asks_nothing_known() {
        use crate::releases::tests::{feature, git, release};
        // 14 changes into dev; a release after every 4 (3 releases); the last 2 not released.
        let scratch = std::env::temp_dir().join(format!("tower-releases-fast-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        git(&scratch, &["init", "-q", "-b", "main"]);
        std::fs::write(scratch.join("README"), "shop").unwrap();
        git(&scratch, &["add", "-A"]);
        git(&scratch, &["commit", "-q", "-m", "init"]);
        git(&scratch, &["branch", "dev"]);
        let mut list: Vec<Value> = vec![];
        let mut minute = 0;
        for i in 0..14u64 {
            let n = 900 + i;
            let mc = feature(&scratch, &format!("f{n}"));
            minute += 10;
            list.push(serde_json::json!({ "number": n, "title": format!("change {n}"), "headRefName": format!("feat-{n}"), "baseRefName": "dev", "headRefOid": "x", "state": "MERGED", "mergedAt": iso(minute), "mergeCommit": { "oid": mc } }));
            if i % 4 == 3 && i < 12 {
                let head = release(&scratch);
                minute += 5;
                list.push(serde_json::json!({ "number": 800 + i, "title": "release", "headRefName": "dev", "baseRefName": "main", "headRefOid": head, "state": "MERGED", "mergedAt": iso(minute) }));
            }
        }
        let gh = fake_gh("releases-fast");
        with_github(&gh, &scratch);
        std::fs::write(gh.with_file_name("list.json"), Value::Array(list.clone()).to_string()).unwrap();
        // The tower's PRs, found before it kept merge commits: none has one.
        let prs = Prs::at(gh.with_file_name("prs.json"), gh.clone());
        let now = crate::history::now_ms();
        for x in list.iter().filter(|x| x["baseRefName"] == "dev") {
            let n = x["number"].as_u64().unwrap();
            prs.insert_for_test(Pr { found_ms: now, ..crate::releases::tests::pr(n, "dev", "merged", "", None) });
        }
        let cache = gh.with_file_name("releases-cache.json");
        let count = |what: &str| crate::prs::tests::calls(&gh).lines().filter(|l| l.contains(what)).count();
        // The first refresh: one list per repo, no PR asked for on its own, one compare per released change at most.
        let t0 = std::time::Instant::now();
        let first = Releases::new(prs.clone(), Shared::new(State::new("off", "test", "test")), gh.clone(), Some(cache.clone())).refresh();
        let took = t0.elapsed();
        let r = &first["checkout"]["repos"][0];
        let stages: Vec<&str> = r["changes"].as_array().unwrap().iter().map(|c| c["stage"].as_str().unwrap()).collect();
        assert_eq!(stages.iter().filter(|s| **s == "released").count(), 12, "{stages:?}");
        assert_eq!(r["on_dev_not_prod"], serde_json::json!([912, 913]));
        assert_eq!(r["changes"][4]["release"], 807, "the second release carried 904");
        assert_eq!((count("pr list"), count("pr view")), (1, 0), "one list, no PR on its own:\n{}", crate::prs::tests::calls(&gh));
        let compares = count("compare/");
        assert!(compares <= 12, "{compares} compares: one per released change at most (a release that landed before a change merged is never asked)");
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert!(cache.exists(), "what can't change is kept");
        // A restart (a new daemon, the same cache file): nothing it knew is asked again.
        let again = Releases::new(prs, Shared::new(State::new("off", "test", "test")), gh.clone(), Some(cache)).refresh();
        assert_eq!(again["checkout"]["repos"][0]["changes"], first["checkout"]["repos"][0]["changes"]);
        assert_eq!((count("compare/"), count("pr view")), (compares, 0), "no compare, no PR asked again after the restart");
    }
}
