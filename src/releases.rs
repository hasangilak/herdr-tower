//! Where a space's changes stand, repo by repo (`docs/quiet-tower.md`,
//! "The release track"): each PR open, merged into dev, on dev (deployed),
//! or released to prod, with when; and each release to prod (the release PR
//! from dev to main), which PRs it carried and when it landed.
//!
//! It is worked out from what GitHub and git say, never from what sessions
//! say: the PRs the tower tracks (their base branches and merges), the
//! release PRs of each repo (`dev` into `main`, listed from GitHub), whether
//! a PR's merge commit is in a release's head (`Git::contains`: GitHub's
//! compare, or `git merge-base --is-ancestor`), and the last successful
//! deployment of dev (GitHub's deployments).
//!
//! The user's rule is dev first, then dev into prod: a change into dev is on
//! prod once a release that contains it has landed; one merged straight
//! into main is said so, as against the rule.
//!
//! This file is the part without side effects; the daemon fills it from
//! GitHub (`daemon::releases`) and shows it in `state.json` (`releases`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::prs::{Pr, is_prod};
use crate::prscore::is_dev;

/// A release PR of a repo, from dev into prod (tracked, or listed from GitHub).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleasePr {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head: String,
    pub base: String,
    /// Its head when it merged (what dev was): what it carries.
    pub head_sha: String,
    /// `open`, `merged`, `closed`.
    pub state: String,
    pub merged_ms: Option<u64>,
}

impl ReleasePr {
    /// A release: from a dev branch into a prod one.
    pub fn is_release(head: &str, base: &str) -> bool {
        is_dev(head) && is_prod(base)
    }

    pub fn of(p: &Pr) -> ReleasePr {
        ReleasePr {
            repo: p.repo.clone(),
            number: p.number,
            title: p.title.clone(),
            url: p.url.clone(),
            head: p.head.clone(),
            base: p.base.clone(),
            head_sha: p.head_sha.clone(),
            state: p.state.clone(),
            merged_ms: p.merged_ms,
        }
    }

    /// From `gh pr list --json number,title,url,headRefName,baseRefName,headRefOid,state,mergedAt`.
    pub fn from_gh(repo: &str, v: &serde_json::Value) -> Option<ReleasePr> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let r = ReleasePr {
            repo: repo.into(),
            number: v["number"].as_u64()?,
            title: s("title"),
            url: s("url"),
            head: s("headRefName"),
            base: s("baseRefName"),
            head_sha: s("headRefOid"),
            state: s("state").to_lowercase(),
            merged_ms: v["mergedAt"].as_str().and_then(ms_of_iso),
        };
        ReleasePr::is_release(&r.head, &r.base).then_some(r)
    }
}

/// The last successful deployment of a repo's dev.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Deployed {
    pub sha: String,
    pub at_ms: u64,
    pub environment: String,
}

/// One change (a PR) of a space, and where it stands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Change {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub base: String,
    /// `open`, `merged` (into dev), `deployed` (on dev), `released` (on prod), `closed` (not merged).
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_ms: Option<u64>,
    /// The release PR that took it to prod.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<u64>,
    /// Merged straight into prod, not through dev (against the user's rule).
    pub skipped_dev: bool,
}

/// A release to prod, and what of the space it carried.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Release {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head: String,
    pub base: String,
    /// `open` or `merged`.
    pub state: String,
    /// When it landed on prod.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_ms: Option<u64>,
    /// The space's changes it carries (by number).
    pub carried: Vec<u64>,
}

/// One repo of a space: its changes, its releases, its dev's last deployment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoTrack {
    pub repo: String,
    pub dev: String,
    pub prod: String,
    pub changes: Vec<Change>,
    /// Newest first; an open one first of all.
    pub releases: Vec<Release>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployed: Option<Deployed>,
}

/// A space's release track.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Track {
    pub space: String,
    pub repos: Vec<RepoTrack>,
    pub checked_ms: u64,
}

/// What GitHub or git says of commits.
pub trait Git {
    /// `commit` is in `tip`'s history (`tip` contains it). None: it can't be told.
    fn contains(&self, repo: &str, commit: &str, tip: &str) -> Option<bool>;
}

/// The commit a merged change is known by: its merge commit, else its head.
fn commit_of(p: &Pr) -> &str {
    if p.merge_commit.is_empty() { &p.head_sha } else { &p.merge_commit }
}

/// A space's track: its PRs (the tower's, in that space), each repo's release
/// PRs (`releases`, from GitHub, and the space's own release PRs), each repo's
/// last dev deployment, and what git says (`git`).
pub fn track(space: &str, prs: &[Pr], releases: &[ReleasePr], deployed: &BTreeMap<String, Deployed>, git: &dyn Git, now: u64) -> Track {
    let mine: Vec<&Pr> = prs.iter().filter(|p| p.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(space))).collect();
    let mut repos: Vec<String> = mine.iter().map(|p| p.repo.clone()).collect();
    repos.sort();
    repos.dedup();
    let mut out = Track { space: space.into(), repos: vec![], checked_ms: now };
    for repo in repos {
        let in_repo: Vec<&Pr> = mine.iter().copied().filter(|p| p.repo == repo).collect();
        // Its releases: listed from GitHub, and the space's own release PRs, once each.
        let mut rels: Vec<ReleasePr> = releases.iter().filter(|r| r.repo == repo).cloned().collect();
        for p in in_repo.iter().filter(|p| ReleasePr::is_release(&p.head, &p.base)) {
            if !rels.iter().any(|r| r.number == p.number) {
                rels.push(ReleasePr::of(p));
            }
        }
        rels.retain(|r| r.state != "closed");
        // Landed first to last; the open ones after.
        rels.sort_by_key(|r| (r.state != "merged", r.merged_ms.unwrap_or(u64::MAX), r.number));
        let features: Vec<&Pr> = in_repo.iter().copied().filter(|p| !ReleasePr::is_release(&p.head, &p.base)).collect();
        let dev = features.iter().map(|p| p.base.as_str()).find(|b| is_dev(b)).or_else(|| rels.first().map(|r| r.head.as_str())).unwrap_or("dev").to_string();
        let prod = rels.first().map(|r| r.base.clone()).unwrap_or_else(|| "main".into());
        let dep = deployed.get(&repo).cloned();
        let mut changes = vec![];
        for p in &features {
            let mut c = Change { number: p.number, title: p.title.clone(), url: p.url.clone(), base: p.base.clone(), merged_ms: p.merged_ms, ..Default::default() };
            c.stage = match p.state.as_str() {
                "open" => "open".into(),
                "merged" if is_prod(&p.base) => {
                    c.skipped_dev = true;
                    c.released_ms = p.merged_ms;
                    "released".into()
                }
                "merged" => {
                    let commit = commit_of(p);
                    // The first release that landed with it in its head.
                    let carried = rels.iter().filter(|r| r.state == "merged").find(|r| !commit.is_empty() && git.contains(&repo, commit, &r.head_sha) == Some(true));
                    match carried {
                        Some(r) => {
                            c.release = Some(r.number);
                            c.released_ms = r.merged_ms;
                            "released".into()
                        }
                        None => match &dep {
                            Some(d) if !commit.is_empty() && git.contains(&repo, commit, &d.sha) == Some(true) => {
                                c.deployed_ms = Some(d.at_ms);
                                "deployed".into()
                            }
                            _ => "merged".into(),
                        },
                    }
                }
                _ => "closed".into(),
            };
            changes.push(c);
        }
        changes.sort_by_key(|c| c.number);
        // Each release with what of the space it carries: landed ones by the
        // changes it took to prod; an open one by what of dev's it holds now.
        let mut releases: Vec<Release> = rels
            .iter()
            .map(|r| {
                let carried = if r.state == "merged" {
                    changes.iter().filter(|c| c.release == Some(r.number)).map(|c| c.number).collect()
                } else {
                    changes
                        .iter()
                        .filter(|c| matches!(c.stage.as_str(), "merged" | "deployed"))
                        .filter(|c| features.iter().find(|p| p.number == c.number).is_some_and(|p| git.contains(&repo, commit_of(p), &r.head_sha) == Some(true)))
                        .map(|c| c.number)
                        .collect()
                };
                Release { number: r.number, title: r.title.clone(), url: r.url.clone(), head: r.head.clone(), base: r.base.clone(), state: r.state.clone(), merged_ms: r.merged_ms, carried }
            })
            .collect();
        releases.reverse();
        // A landed release that carried nothing of this space isn't the space's.
        releases.retain(|r| r.state != "merged" || !r.carried.is_empty());
        out.repos.push(RepoTrack { repo, dev, prod, changes, releases, deployed: dep });
    }
    out
}

impl RepoTrack {
    /// The changes on prod, by the release that took them there (newest
    /// first): (since when, their numbers).
    pub fn on_prod(&self) -> Vec<(u64, Vec<u64>)> {
        let mut out: Vec<(u64, Vec<u64>)> = vec![];
        for c in self.changes.iter().filter(|c| c.stage == "released") {
            let at = c.released_ms.unwrap_or(0);
            match out.iter_mut().find(|(t, _)| *t == at) {
                Some((_, list)) => list.push(c.number),
                None => out.push((at, vec![c.number])),
            }
        }
        out.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        out
    }

    /// Merged into dev, not on prod yet.
    pub fn on_dev_not_prod(&self) -> Vec<&Change> {
        self.changes.iter().filter(|c| matches!(c.stage.as_str(), "merged" | "deployed")).collect()
    }

    /// What it says, a line each: "On prod since 14:20: #829, #830", "On
    /// dev, not on prod: #831", the open ones, a release under way, and a
    /// change that skipped dev.
    pub fn lines(&self, now: u64) -> Vec<String> {
        let nums = |list: &[u64]| list.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ");
        let mut out: Vec<String> = self.on_prod().into_iter().map(|(at, list)| format!("On {} since {}: {}", self.prod, when(at, now), nums(&list))).collect();
        let dev: Vec<u64> = self.on_dev_not_prod().iter().map(|c| c.number).collect();
        if !dev.is_empty() {
            out.push(format!("On {}, not on {}: {}", self.dev, self.prod, nums(&dev)));
        }
        for r in self.releases.iter().filter(|r| r.state == "open") {
            let carries = if r.carried.is_empty() { String::new() } else { format!(", carries {}", nums(&r.carried)) };
            out.push(format!("Release under way: #{} {} → {}{carries}", r.number, r.head, r.base));
        }
        let open: Vec<u64> = self.changes.iter().filter(|c| c.stage == "open").map(|c| c.number).collect();
        if !open.is_empty() {
            out.push(format!("Open: {}", nums(&open)));
        }
        for c in self.changes.iter().filter(|c| c.skipped_dev) {
            out.push(format!("#{} went to {} without {} first, against the dev-then-prod rule", c.number, c.base, self.dev));
        }
        out
    }
}

impl Change {
    /// Its stage in words, with when: "on prod since 14:20 (release #840)".
    pub fn stage_line(&self, now: u64, dev: &str, prod: &str) -> String {
        let t = |ms: Option<u64>| ms.map(|m| format!(" since {}", when(m, now))).unwrap_or_default();
        match self.stage.as_str() {
            "open" => format!("open, into {}", self.base),
            "merged" => format!("merged into {dev}{}, not deployed yet", t(self.merged_ms)),
            "deployed" => format!("on {dev} (deployed{})", t(self.deployed_ms)),
            "released" if self.skipped_dev => format!("on {prod}{}, straight from its branch", t(self.released_ms)),
            "released" => format!("on {prod}{}{}", t(self.released_ms), self.release.map(|r| format!(" (release #{r})")).unwrap_or_default()),
            _ => "closed without merging".into(),
        }
    }
}

impl Change {
    /// What its row says beside its stage's chip: "since 14:20 · release
    /// #840", "deployed 15:42", "merged 15:30, not deployed yet", "into dev".
    pub fn detail(&self, now: u64) -> String {
        let at = |ms: Option<u64>| ms.map(|m| when(m, now)).unwrap_or_default();
        match self.stage.as_str() {
            "open" => format!("into {}", self.base),
            "merged" => format!("merged {}, not deployed yet", at(self.merged_ms)).replace("merged , ", "merged, "),
            "deployed" => format!("deployed {}", at(self.deployed_ms)),
            "released" if self.skipped_dev => format!("since {} · skipped dev", at(self.released_ms)),
            "released" => format!("since {}{}", at(self.released_ms), self.release.map(|r| format!(" · release #{r}")).unwrap_or_default()),
            _ => "not merged".into(),
        }
    }
}

impl Track {
    /// What a space's row in the sidebar says: "prod 14:20 · 1 on dev".
    pub fn summary(&self, now: u64) -> String {
        let last_prod = self.repos.iter().flat_map(|r| r.on_prod()).map(|(t, _)| t).max();
        let on_dev: usize = self.repos.iter().map(|r| r.on_dev_not_prod().len()).sum();
        let open: usize = self.repos.iter().map(|r| r.changes.iter().filter(|c| c.stage == "open").count()).sum();
        let mut parts = vec![];
        if let Some(t) = last_prod {
            parts.push(format!("prod {}", when(t, now)));
        }
        if on_dev > 0 {
            parts.push(format!("{on_dev} on dev"));
        }
        if open > 0 && parts.is_empty() {
            parts.push(format!("{open} PR{} open", if open == 1 { "" } else { "s" }));
        }
        parts.join(" · ")
    }

    /// As `state.json` (`releases.<space>`) and the phone carry it: the
    /// track, and each repo's lines and each change's stage in words.
    pub fn view(&self, now: u64) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["summary"] = self.summary(now).into();
        for (i, r) in self.repos.iter().enumerate() {
            v["repos"][i]["lines"] = serde_json::json!(r.lines(now));
            v["repos"][i]["on_prod"] = serde_json::json!(r.on_prod().into_iter().map(|(at, list)| serde_json::json!({ "since_ms": at, "prs": list })).collect::<Vec<_>>());
            v["repos"][i]["on_dev_not_prod"] = serde_json::json!(r.on_dev_not_prod().iter().map(|c| c.number).collect::<Vec<_>>());
            for (j, c) in r.changes.iter().enumerate() {
                v["repos"][i]["changes"][j]["stage_line"] = c.stage_line(now, &r.dev, &r.prod).into();
            }
        }
        v
    }
}

// ---- time ----

/// Unix milliseconds of an ISO time GitHub gives (`2026-11-13T13:20:05Z`, an offset allowed).
pub fn ms_of_iso(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, mo, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let time: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == ':' || *c == '.').collect();
    let zone = &rest[time.len()..];
    let mut t = time.split(':');
    let (h, mi) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let sec: f64 = t.next().unwrap_or("0").parse().ok()?;
    // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
    let (yy, m) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let offset = match zone {
        "" | "Z" | "z" => 0,
        z => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let z = z.trim_start_matches(['+', '-']);
            let (zh, zm) = z.split_once(':').unwrap_or((z.get(..2)?, z.get(2..).unwrap_or("0")));
            sign * (zh.parse::<i64>().ok()? * 60 + zm.parse::<i64>().unwrap_or(0))
        }
    };
    let secs = days * 86_400 + h * 3600 + mi * 60 - offset * 60;
    (secs >= 0).then(|| (secs as f64 * 1000.0 + sec * 1000.0) as u64)
}

fn local(ms: u64) -> libc::tm {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r fills the struct we own.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    }
}

/// "14:20" today, "Nov 12 14:20" another day (local time).
pub fn when(ms: u64, now: u64) -> String {
    let (t, n) = (local(ms), local(now));
    let hm = format!("{:02}:{:02}", t.tm_hour, t.tm_min);
    if t.tm_year == n.tm_year && t.tm_yday == n.tm_yday {
        return hm;
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {hm}", MONTHS[t.tm_mon.clamp(0, 11) as usize], t.tm_mday)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// Git itself, in a scratch checkout: what `merge-base --is-ancestor` says.
    pub(crate) struct LocalGit(pub PathBuf);

    impl Git for LocalGit {
        fn contains(&self, _repo: &str, commit: &str, tip: &str) -> Option<bool> {
            let st = Command::new("git").arg("-C").arg(&self.0).args(["merge-base", "--is-ancestor", commit, tip]).stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?;
            match st.code() {
                Some(0) => Some(true),
                Some(1) => Some(false),
                _ => None,
            }
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A feature branch off dev with one commit, merged into dev (no fast-forward): the merge commit.
    fn feature(dir: &Path, name: &str) -> String {
        git(dir, &["checkout", "-q", "-b", name, "dev"]);
        std::fs::write(dir.join(format!("{name}.txt")), name).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", name]);
        git(dir, &["checkout", "-q", "dev"]);
        git(dir, &["merge", "-q", "--no-ff", "-m", &format!("Merge {name}"), name]);
        git(dir, &["rev-parse", "HEAD"])
    }

    /// dev released into main (a merge commit on main): dev's head, which the release PR carried.
    fn release(dir: &Path) -> String {
        let head = git(dir, &["rev-parse", "dev"]);
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["merge", "-q", "--no-ff", "-m", "release", "dev"]);
        head
    }

    pub(crate) const MIN: u64 = 60_000;

    pub(crate) fn pr(n: u64, base: &str, state: &str, merge_commit: &str, merged_ms: Option<u64>) -> Pr {
        Pr {
            repo: "eyzhub/shop-api".into(),
            number: n,
            title: format!("Change {n}"),
            url: format!("https://github.com/eyzhub/shop-api/pull/{n}"),
            base: base.into(),
            head: format!("feat-{n}"),
            head_sha: format!("h{n}"),
            state: state.into(),
            space: Some("checkout".into()),
            merge_commit: merge_commit.into(),
            merged_ms,
            ..Default::default()
        }
    }

    pub(crate) fn rel(n: u64, head_sha: &str, state: &str, merged_ms: Option<u64>) -> ReleasePr {
        ReleasePr { repo: "eyzhub/shop-api".into(), number: n, title: "release: dev into main".into(), url: String::new(), head: "dev".into(), base: "main".into(), head_sha: head_sha.into(), state: state.into(), merged_ms }
    }

    /// The validation: two changes into dev, one release carrying the first,
    /// then a second release; at each point every change's stage, and what
    /// is on dev but not on prod.
    pub(crate) fn scratch() -> (PathBuf, [String; 4]) {
        // Its own folder each time: tests run side by side.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tower-releases-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("README"), "shop").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        git(&dir, &["branch", "dev"]);
        let m829 = feature(&dir, "f829");
        let r1 = release(&dir);
        let m830 = feature(&dir, "f830");
        let r2 = release(&dir);
        (dir, [m829, r1, m830, r2])
    }

    #[test]
    fn two_changes_into_dev_one_release_then_another_each_at_its_stage() {
        let (dir, [m829, r1, m830, r2]) = scratch();
        let g = LocalGit(dir);
        let t0 = 1_794_560_000_000;
        let now = t0 + 120 * MIN;
        let no_deploy = BTreeMap::new();
        // Both merged into dev; the first release (#840) landed carrying #829 only.
        let prs = vec![pr(829, "dev", "merged", &m829, Some(t0)), pr(830, "dev", "merged", &m830, Some(t0 + 30 * MIN)), pr(831, "dev", "open", "", None)];
        let first = vec![rel(840, &r1, "merged", Some(t0 + 20 * MIN))];
        let t = track("checkout", &prs, &first, &no_deploy, &g, now);
        let r = &t.repos[0];
        let stages: Vec<(u64, &str)> = r.changes.iter().map(|c| (c.number, c.stage.as_str())).collect();
        assert_eq!(stages, [(829, "released"), (830, "merged"), (831, "open")]);
        assert_eq!((r.changes[0].release, r.changes[0].released_ms), (Some(840), Some(t0 + 20 * MIN)));
        assert_eq!(r.on_dev_not_prod().iter().map(|c| c.number).collect::<Vec<_>>(), [830]);
        assert_eq!(r.releases.iter().map(|x| (x.number, x.carried.clone())).collect::<Vec<_>>(), [(840, vec![829])]);
        let lines = r.lines(now);
        assert_eq!(lines[0], format!("On main since {}: #829", when(t0 + 20 * MIN, now)));
        assert_eq!(lines[1], "On dev, not on main: #830");
        assert_eq!(lines[2], "Open: #831");
        // Dev is deployed with #830 in it: on dev, deployed.
        let deployed = BTreeMap::from([("eyzhub/shop-api".to_string(), Deployed { sha: m830.clone(), at_ms: t0 + 35 * MIN, environment: "dev".into() })]);
        let t = track("checkout", &prs, &first, &deployed, &g, now);
        assert_eq!((t.repos[0].changes[1].stage.as_str(), t.repos[0].changes[1].deployed_ms), ("deployed", Some(t0 + 35 * MIN)));
        assert_eq!(t.repos[0].on_dev_not_prod().len(), 1, "deployed on dev is still not on prod");
        // A release under way (#841, dev's head now): it carries #830.
        let both = vec![rel(840, &r1, "merged", Some(t0 + 20 * MIN)), rel(841, &r2, "open", None)];
        let t = track("checkout", &prs, &both, &no_deploy, &g, now);
        assert!(t.repos[0].lines(now).contains(&"Release under way: #841 dev → main, carries #830".to_string()), "{:?}", t.repos[0].lines(now));
        // The second release landed: both on prod, each since its own release; nothing on dev alone.
        let both = vec![rel(840, &r1, "merged", Some(t0 + 20 * MIN)), rel(841, &r2, "merged", Some(t0 + 60 * MIN))];
        let t = track("checkout", &prs, &both, &no_deploy, &g, now);
        let r = &t.repos[0];
        assert_eq!(r.changes.iter().map(|c| (c.number, c.stage.as_str(), c.release)).collect::<Vec<_>>(), [(829, "released", Some(840)), (830, "released", Some(841)), (831, "open", None)]);
        assert!(r.on_dev_not_prod().is_empty());
        assert_eq!(r.on_prod(), [(t0 + 60 * MIN, vec![830]), (t0 + 20 * MIN, vec![829])]);
        assert_eq!(r.releases.iter().map(|x| (x.number, x.carried.clone())).collect::<Vec<_>>(), [(841, vec![830]), (840, vec![829])], "newest first");
        assert_eq!(t.summary(now), format!("prod {}", when(t0 + 60 * MIN, now)));
        assert_eq!(r.changes[1].stage_line(now, "dev", "main"), format!("on main since {} (release #841)", when(t0 + 60 * MIN, now)));
        assert_eq!(r.changes[1].detail(now), format!("since {} · release #841", when(t0 + 60 * MIN, now)));
        assert_eq!(r.changes[2].detail(now), "into dev");
        // As the state and the phone carry it.
        let v = t.view(now);
        assert_eq!(v["repos"][0]["on_dev_not_prod"], serde_json::json!([]));
        assert_eq!(v["repos"][0]["on_prod"][0]["prs"], serde_json::json!([830]));
        assert_eq!(v["repos"][0]["changes"][2]["stage_line"], "open, into dev");
        assert_eq!(v["summary"], t.summary(now));
    }

    #[test]
    fn a_change_straight_into_main_is_on_prod_and_said_against_the_rule() {
        let (dir, [m829, ..]) = scratch();
        let g = LocalGit(dir);
        let now = 1_794_560_000_000;
        let prs = vec![pr(829, "dev", "merged", &m829, Some(now - 60 * MIN)), pr(850, "main", "merged", "x", Some(now - 5 * MIN)), pr(851, "dev", "closed", "", None)];
        let t = track("checkout", &prs, &[], &BTreeMap::new(), &g, now);
        let r = &t.repos[0];
        assert_eq!(r.changes.iter().map(|c| (c.number, c.stage.as_str(), c.skipped_dev)).collect::<Vec<_>>(), [(829, "merged", false), (850, "released", true), (851, "closed", false)]);
        let lines = r.lines(now);
        assert!(lines.contains(&"#850 went to main without dev first, against the dev-then-prod rule".to_string()), "{lines:?}");
        assert!(lines.contains(&"On dev, not on main: #829".to_string()), "{lines:?}");
        assert_eq!(t.summary(now), format!("prod {} · 1 on dev", when(now - 5 * MIN, now)));
        // Another space's PRs aren't this one's.
        assert!(track("other", &prs, &[], &BTreeMap::new(), &g, now).repos.is_empty());
    }

    #[test]
    fn github_times_and_release_prs_read() {
        assert_eq!(ms_of_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(ms_of_iso("2026-11-13T13:20:05Z"), Some(1_794_576_005_000));
        assert_eq!(ms_of_iso("2026-11-13T14:20:05+01:00"), Some(1_794_576_005_000));
        assert_eq!(ms_of_iso("not a time"), None);
        let v = serde_json::json!({ "number": 840, "title": "release", "url": "u", "headRefName": "dev", "baseRefName": "main", "headRefOid": "abc", "state": "MERGED", "mergedAt": "2026-11-13T13:20:05Z" });
        let r = ReleasePr::from_gh("o/r", &v).unwrap();
        assert_eq!((r.number, r.state.as_str(), r.merged_ms), (840, "merged", Some(1_794_576_005_000)));
        assert!(ReleasePr::from_gh("o/r", &serde_json::json!({ "number": 1, "headRefName": "feat-x", "baseRefName": "dev" })).is_none(), "a feature PR isn't a release");
        let now = 1_794_576_005_000;
        assert_eq!(when(now, now).len(), 5, "today: 14:20");
        assert!(when(now - 3 * 86_400_000, now).contains(' '), "another day: its date");
    }
}
