//! Which folder a project's name means. The user says "the sandbox
//! project" or "mediaroom", not `~/Work/eyz/eyzflow-sandbox`; opening a
//! session must not stop to ask which folder. So the tower picks the best
//! match among the project folders (every folder in a projects root, and
//! one level below it: `~/Work/eyz/eyzflow-sandbox`), and says what it chose
//! and what else came close, for the user to correct afterwards.
//!
//! Roots: `HERDER_TOWER_PROJECTS` (folders separated by `:`), else `~/Work`.

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Words that say "a project" without naming one.
const FILLER: &[&str] =
    &["the", "a", "an", "my", "our", "project", "projects", "repo", "repository", "folder", "dir", "directory", "session", "sessions", "claude", "codex", "one", "for", "in", "on", "of", "and"];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Match {
    /// The folder, `~/...` when it's under the home folder.
    pub folder: String,
    #[serde(skip)]
    pub path: PathBuf,
    #[serde(skip)]
    pub score: i64,
}

pub fn roots() -> Vec<PathBuf> {
    match std::env::var("HERDER_TOWER_PROJECTS") {
        Ok(v) if !v.trim().is_empty() => v.split(':').filter(|s| !s.is_empty()).map(|s| PathBuf::from(crate::dispatch::expand(s))).collect(),
        _ => vec![crate::config::home().join("Work")],
    }
}

/// The words of a name: `eyzflow-sandbox` -> [eyzflow, sandbox].
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// What was asked, without the filler: "the sandbox project" -> [sandbox].
/// A path counts by its last part: `~/Work/eyz/sandbox` -> [sandbox].
pub fn wanted(asked: &str) -> Vec<String> {
    let asked = asked.trim().trim_end_matches('/');
    let last = if asked.contains('/') { asked.rsplit('/').next().unwrap_or(asked) } else { asked };
    words(last).into_iter().filter(|w| !FILLER.contains(&w.as_str())).collect()
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// How well the folder `name` fits the words asked (0: not at all).
/// `git`: Some(true) for a checkout's own folder, Some(false) for a git
/// worktree (its `.git` is a file), None for no git.
pub fn score(want: &[String], name: &str, git: Option<bool>) -> i64 {
    if want.is_empty() {
        return 0;
    }
    let have = words(name);
    let (w, h) = (want.concat(), have.concat());
    let mut s = if h == w {
        1000
    } else {
        // Each word asked: the same word in the name (1), or inside one (a half).
        let mut hit = 0.0;
        let mut used = vec![false; have.len()];
        for x in want {
            if let Some(i) = have.iter().position(|y| y == x) {
                hit += 1.0;
                used[i] = true;
            } else if x.len() >= 3
                && let Some(i) = have.iter().position(|y| y.contains(x.as_str()) || (y.len() >= 3 && x.contains(y.as_str())))
            {
                hit += 0.5;
                used[i] = true;
            }
        }
        let extra = used.iter().filter(|u| !**u).count() as i64;
        if hit >= want.len() as f64 || h.contains(&w) {
            500 - 10 * extra
        } else if w.len() >= 5 && distance(&w, &h) <= 2 {
            // A voice slip: "eyes flow" for eyzflow.
            450 - 10 * extra
        } else if hit > 0.0 {
            (100.0 * hit / want.len() as f64) as i64 - 10 * extra
        } else {
            return 0;
        }
    };
    // A side copy (wt-…, …-worktree, a git worktree) loses to the project itself.
    if have.first().is_some_and(|f| f == "wt") || have.iter().any(|x| x == "worktree" || x == "wt") {
        s -= 150;
    }
    match git {
        Some(true) => s += 20,
        Some(false) => s -= 30,
        None => {}
    }
    s.max(1)
}

fn git_of(dir: &Path) -> Option<bool> {
    let g = dir.join(".git");
    if g.is_dir() {
        Some(true)
    } else if g.exists() {
        Some(false)
    } else {
        None
    }
}

fn tilde(p: &Path) -> String {
    let home = crate::config::home();
    match p.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut v: Vec<PathBuf> =
        rd.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'))).collect();
    v.sort();
    v
}

/// The project folders under `roots`: each folder in a root, and each folder
/// inside one of those that isn't itself a checkout (`~/Work/eyz/…`).
pub fn candidates(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = vec![];
    for r in roots {
        for d in subdirs(r) {
            let group = git_of(&d).is_none();
            out.push(d.clone());
            if group {
                out.extend(subdirs(&d));
            }
        }
    }
    out
}

/// Every folder that fits `asked`, the best first.
pub fn rank(asked: &str, folders: &[PathBuf]) -> Vec<Match> {
    let want = wanted(asked);
    let mut v: Vec<(Match, std::time::SystemTime)> = folders
        .iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?;
            let score = score(&want, name, git_of(p));
            let when = p.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            (score > 0).then(|| (Match { folder: tilde(p), path: p.clone(), score }, when))
        })
        .collect();
    // Equal scores: the one worked on last.
    v.sort_by(|a, b| b.0.score.cmp(&a.0.score).then(b.1.cmp(&a.1)));
    v.into_iter().map(|(m, _)| m).collect()
}

/// The folder `asked` means: the best match under the projects roots.
pub fn best(asked: &str) -> Option<(Match, Vec<Match>)> {
    let mut all = rank(asked, &candidates(&roots()));
    if all.is_empty() {
        return None;
    }
    let first = all.remove(0);
    all.truncate(3);
    Some((first, all))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Root(PathBuf);
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tree(names: &[(&str, Option<bool>)]) -> (Root, Vec<PathBuf>) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!("tower-projects-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        for (n, git) in names {
            let d = root.join(n);
            std::fs::create_dir_all(&d).unwrap();
            match git {
                Some(true) => std::fs::create_dir(d.join(".git")).unwrap(),
                Some(false) => std::fs::write(d.join(".git"), "gitdir: elsewhere").unwrap(),
                None => {}
            }
        }
        let c = candidates(&[root.clone()]);
        (Root(root), c)
    }

    fn names(m: &[Match]) -> Vec<String> {
        m.iter().map(|m| m.path.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_sandbox_project_is_the_sandbox_sibling_not_a_worktree() {
        // What the user had: the project and six worktrees named after sandbox.
        let (_r, c) = tree(&[
            ("eyz", None),
            ("eyz/eyzflow", Some(true)),
            ("eyz/eyzflow-sandbox", Some(true)),
            ("eyz/eyz-mediaroom-serverless", Some(true)),
            ("eyz/eyzflow-admin-dashboard", Some(true)),
            ("eyz/wt-sandbox-design", Some(false)),
            ("eyz/wt-sandbox-extension", Some(false)),
            ("eyz/wt-sandbox-files", Some(false)),
            ("eyz/wt-sandbox-genui", Some(false)),
            ("eyz/wt-sandbox-pg", Some(false)),
            ("eyz/wt-sandbox-vitest", Some(false)),
        ]);
        for asked in ["sandbox project", "the sandbox", "sandbox", "~/Work/eyz/sandbox", "Sandbox project folder"] {
            let r = rank(asked, &c);
            assert_eq!(names(&r)[0], "eyzflow-sandbox", "{asked}: {:?}", names(&r));
        }
        assert_eq!(names(&rank("mediaroom", &c))[0], "eyz-mediaroom-serverless");
        assert_eq!(names(&rank("eyzflow", &c))[0], "eyzflow");
        // Whisper's slip.
        assert_eq!(names(&rank("eyes flow", &c))[0], "eyzflow");
        // A worktree is still found when it's what was named.
        assert_eq!(names(&rank("sandbox genui", &c))[0], "wt-sandbox-genui");
    }

    #[test]
    fn nothing_close_is_none_and_filler_alone_names_nothing() {
        let (_r, c) = tree(&[("herder-tower", Some(true)), ("herder-tower-android", Some(true))]);
        assert!(rank("kubernetes", &c).is_empty());
        assert!(rank("the project", &c).is_empty());
        assert_eq!(names(&rank("herder tower", &c))[0], "herder-tower");
        assert_eq!(names(&rank("tower android", &c))[0], "herder-tower-android");
    }
}
