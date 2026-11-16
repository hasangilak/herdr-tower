//! Who opened a PR, of the sessions here (`crate::prs::opener`): each
//! Claude session's transcript is read for the PRs its `gh pr create` opened
//! (only what it grew by since the last look), and its repo and branch.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::hc;
use crate::prs::{Claimant, Creates, Pr};

#[derive(Default)]
pub struct Owners {
    /// Each transcript read so far: up to where, and what it opened.
    read: Mutex<HashMap<PathBuf, (u64, Creates)>>,
}

impl Owners {
    /// The PRs the transcript at `path` opened, reading only what it grew by.
    fn created(&self, path: &Path) -> Option<Vec<(String, u64)>> {
        let mut read = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let (at, c) = read.entry(path.to_path_buf()).or_default();
        let mut f = std::fs::File::open(path).ok()?;
        if f.metadata().ok()?.len() < *at {
            (*at, *c) = (0, Creates::default());
        }
        f.seek(SeekFrom::Start(*at)).ok()?;
        let mut r = BufReader::with_capacity(1 << 20, f);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = r.read_until(b'\n', &mut buf).ok()?;
            // A line still being written is read next time.
            if n == 0 || buf.last() != Some(&b'\n') {
                break;
            }
            *at += n as u64;
            c.line(&String::from_utf8_lossy(&buf));
        }
        Some(c.made.clone())
    }

    fn claimant(&self, address: &str, t: &Value, name: String, space: Option<String>) -> Claimant {
        let local_claude = hc::agent_app(t).as_deref() == Some("claude-code") && !address.contains('/');
        let created = if local_claude {
            t["foreground"]["pid"].as_u64().and_then(|p| crate::questions::transcript_path_of(p as u32)).and_then(|p| self.created(&p))
        } else {
            None
        };
        let cwd = t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str()).unwrap_or("");
        let branch = (!cwd.is_empty() && !address.contains('/'))
            .then(|| std::process::Command::new("git").args(["-C", cwd, "branch", "--show-current"]).output().ok())
            .flatten()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|b| !b.is_empty());
        let repo = super::reviews::repo_of_terminal(t).map(|p| p.display().to_string());
        Claimant { address: address.into(), name, space, repo, branch, created }
    }

    /// Who opened `pr`; `mentions`: the session that names it (empty: none).
    /// `only_it`: just whether that one's own `gh pr create` opened it.
    pub fn opener(&self, pr: &Pr, mentions: &str, only_it: bool) -> Option<Claimant> {
        let me = (pr.repo.clone(), pr.number);
        let first = (!mentions.is_empty())
            .then(|| hc::get(mentions))
            .flatten()
            .map(|t| self.claimant(mentions, &t, hc::spoken_name(&t), crate::spaces::space_of(&t)));
        if let Some(c) = &first
            && c.created.as_ref().is_some_and(|m| m.contains(&me))
        {
            return first;
        }
        if only_it {
            return None;
        }
        let others: Vec<Claimant> = self.everyone().into_iter().filter(|c| c.address != mentions).collect();
        crate::prs::opener(pr, first.as_ref(), &others).cloned()
    }

    /// Every session here, as far as who opened a PR goes.
    pub fn everyone(&self) -> Vec<Claimant> {
        crate::missions::scan(&|a| hc::get(a), &|_| None)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| {
                let t = hc::get(&s.address)?;
                let space = Some(s.space.trim().to_string()).filter(|x| !x.is_empty());
                Some(self.claimant(&s.address, &t, s.name, space))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    /// Which known PRs the daemon's start would move, on a copy of the real
    /// `prs.json` and the sessions here now (read only):
    /// `HERDER_TOWER_PRS_COPY=<copy> cargo test owners_on_a_live_copy -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn owners_on_a_live_copy() {
        let Some(copy) = std::env::var_os("HERDER_TOWER_PRS_COPY") else { return };
        let prs = crate::prs::Prs::at(copy.into(), "/nonexistent/gh".into());
        let all = super::Owners::default().everyone();
        for c in &all {
            println!("{} ({}) {:?} {:?} opened {:?}", c.address, c.name, c.repo, c.branch, c.created.as_ref().map(|m| m.len()));
        }
        for k in prs.reown(&all) {
            let p = prs.get(&k).unwrap();
            println!("moved {k} ({}) to {} ({})", p.state, p.session, p.name);
        }
    }
}
