//! A git worktree per space and repo, and a dev port per session.
//!
//! A session the tower opens for a space that isn't standing (`tower
//! launch`, `tower start --space …`, a dispatch into a space) doesn't work
//! in the repo's own checkout: it gets a worktree of it,
//! `<repo>/../.tower-worktrees/<space-slug>/<repo-name>`, on a branch
//! `<space-slug>/<repo-name>` made from the repo's HEAD. Two spaces working
//! on one repo never edit the same files, and closing a space can take its
//! worktrees away again (`remove`: only a clean one; a branch is never
//! deleted). A worktree or branch that is there already is used as it is.
//!
//! A fresh worktree has none of the repo's untracked files, and a dev server
//! needs its `.env*`: the repo's untracked top-level `.env*` files are
//! copied over. Each session also gets a dev port of its own from `[spaces]
//! port_range`, not handed out twice among live sessions.
//!
//! What a session was given is kept on its terminal, as Herdr labels
//! (`tower.worktree`, `tower.branch`, `tower.repo`, `tower.port`): they live
//! and end with the terminal, whether or not the voice runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hc;

/// The folder beside a repo that holds the tower's worktrees of it.
pub const DIR: &str = ".tower-worktrees";

pub const LABEL_WORKTREE: &str = "tower.worktree";
pub const LABEL_BRANCH: &str = "tower.branch";
pub const LABEL_REPO: &str = "tower.repo";
pub const LABEL_PORT: &str = "tower.port";

/// A space's name as a folder and a branch prefix: lower-case, `a-z 0-9 - _`,
/// every run of anything else one dash ("B2C support" is `b2c-support`).
pub fn slug(space: &str) -> String {
    let mut out = String::new();
    for c in space.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if !out.is_empty() {
        return out;
    }
    // A name with none of those letters: still its own folder.
    let hash = space.trim().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    format!("space-{:04x}", hash & 0xffff)
}

/// A repo's folder name as the last part of a branch name.
fn branch_part(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    let s = s.trim_matches(|c| c == '.' || c == '-').trim_end_matches(".lock").to_string();
    if s.is_empty() { "repo".into() } else { s }
}

/// A worktree the tower made (or would make) for a space.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Place {
    /// The repo's own checkout.
    pub repo: String,
    /// The worktree's folder.
    pub path: String,
    pub branch: String,
}

/// Where a space's worktree of `repo` (its top folder) is, and its branch.
pub fn place(repo: &Path, space: &str) -> Place {
    let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let slug = slug(space);
    let path = repo.parent().unwrap_or(repo).join(DIR).join(&slug).join(&name);
    Place { repo: repo.to_string_lossy().into_owned(), path: path.to_string_lossy().into_owned(), branch: format!("{slug}/{}", branch_part(&name)) }
}

/// A folder inside one of the tower's worktrees: the repo it is a worktree
/// of, and the worktree's own top folder (`…/.tower-worktrees/<slug>/<name>`).
pub fn repo_of(folder: &Path) -> Option<(PathBuf, PathBuf)> {
    let parts: Vec<_> = folder.components().collect();
    let i = parts.iter().position(|c| c.as_os_str() == DIR)?;
    let name = parts.get(i + 2)?;
    let root: PathBuf = parts[..i].iter().collect();
    let top: PathBuf = parts[..=i + 2].iter().collect();
    Some((root.join(name), top))
}

fn git(dir: &Path, args: &[&str]) -> Result<hc::Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    hc::output_within(cmd, &format!("git {}", args.first().copied().unwrap_or("")), Duration::from_secs(120))
}

/// The top folder of the git checkout `folder` is in (None: not in one).
pub fn toplevel(folder: &Path) -> Option<PathBuf> {
    if !folder.is_dir() {
        return None;
    }
    let out = git(folder, &["rev-parse", "--show-toplevel"]).ok()?;
    let top = out.stdout.trim();
    (out.code == 0 && !top.is_empty()).then(|| PathBuf::from(top))
}

/// A session's place in a space's worktree.
#[derive(Debug, Clone, PartialEq)]
pub struct Made {
    pub place: Place,
    /// Made now (false: it was there already and is used as it is).
    pub fresh: bool,
    /// The `.env*` files copied from the repo.
    pub copied: Vec<String>,
    /// The folder the session starts in: the worktree, or the same folder
    /// inside it when it was asked for one inside the repo.
    pub cwd: String,
}

/// The worktree of `folder`'s repo for `space`, made unless it is there.
/// None: `folder` isn't in a git checkout, and is used as it is.
pub fn ensure(folder: &Path, space: &str) -> Result<Option<Made>> {
    let Some(top) = toplevel(folder) else { return Ok(None) };
    // Asked for a folder that is one of the tower's worktrees already: that one.
    if let Some((repo, wt)) = repo_of(&top) {
        let branch = current_branch(&wt).unwrap_or_default();
        let place = Place { repo: repo.to_string_lossy().into_owned(), path: wt.to_string_lossy().into_owned(), branch };
        return Ok(Some(Made { place, fresh: false, copied: vec![], cwd: folder.to_string_lossy().into_owned() }));
    }
    let place = place(&top, space);
    let path = PathBuf::from(&place.path);
    let fresh = if path.exists() {
        if toplevel(&path).as_deref().map(canon) != Some(canon(&path)) {
            bail!("{} is there already and isn't a git worktree", path.display());
        }
        false
    } else {
        add(&top, &place)?;
        true
    };
    let copied = if fresh { copy_env(&top, &path) } else { vec![] };
    // The same folder inside the worktree as inside the repo.
    let sub = canon(folder).strip_prefix(canon(&top)).map(Path::to_path_buf).unwrap_or_default();
    let cwd = if sub.as_os_str().is_empty() || !path.join(&sub).is_dir() { path.clone() } else { path.join(sub) };
    Ok(Some(Made { place, fresh, copied, cwd: cwd.to_string_lossy().into_owned() }))
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn current_branch(wt: &Path) -> Option<String> {
    let out = git(wt, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    let b = out.stdout.trim();
    (out.code == 0 && !b.is_empty() && b != "HEAD").then(|| b.to_string())
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok_and(|o| o.code == 0)
}

/// `git worktree add`: on the branch when it exists, else on a new one from
/// HEAD. (`tower dispatch` makes a task's own worktree with it too.)
pub fn add(repo: &Path, place: &Place) -> Result<()> {
    let path = Path::new(&place.path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        // The folder ignores itself, should the repo's parent be a checkout too.
        if let Some(root) = dir.parent().filter(|r| r.file_name().is_some_and(|n| n == DIR)) {
            let ignore = root.join(".gitignore");
            if !ignore.exists() {
                let _ = std::fs::write(ignore, "*\n");
            }
        }
    }
    let run = || -> Result<hc::Output> {
        if branch_exists(repo, &place.branch) {
            git(repo, &["worktree", "add", &place.path, &place.branch])
        } else {
            git(repo, &["worktree", "add", "-b", &place.branch, &place.path])
        }
    };
    let mut out = run()?;
    if out.code != 0 {
        // A worktree whose folder was deleted by hand is still on git's list.
        let _ = git(repo, &["worktree", "prune"]);
        out = run()?;
    }
    if out.code != 0 {
        bail!("git worktree add in {}: {}", repo.display(), out.stderr.trim());
    }
    Ok(())
}

// ---- a reviewer's checkout (`crate::reviews`) ----

/// The commit `rev` is in `repo` (its objects: a worktree's are its repo's).
fn has_commit(repo: &Path, rev: &str) -> bool {
    !rev.is_empty() && git(repo, &["cat-file", "-e", &format!("{rev}^{{commit}}")]).is_ok_and(|o| o.code == 0)
}

/// A remote of `repo` that is the GitHub repo `slug` (`owner/name`).
fn remote_for(repo: &Path, slug: &str) -> Option<String> {
    let out = git(repo, &["remote", "-v"]).ok()?;
    let want = slug.to_lowercase();
    out.stdout.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let (name, url) = (parts.next()?, parts.next()?.to_lowercase());
        let path = url.trim_end_matches('/').trim_end_matches(".git").to_string();
        (path.ends_with(&format!("/{want}")) || path.ends_with(&format!(":{want}"))).then(|| name.to_string())
    })
}

/// The commit a reviewer's checkout is made at: the PR's head commit when
/// the repo has it (a session's worktree pushed it from here), else
/// fetched from the remote that is the PR's repo (its `pull/<n>/head`, then
/// its branch). Never from a remote that is another repo.
pub fn pr_commit(repo: &Path, slug: &str, number: u64, head: &str, sha: &str) -> Result<String> {
    if has_commit(repo, sha) {
        return Ok(sha.to_string());
    }
    let Some(remote) = remote_for(repo, slug) else {
        bail!("{} has no remote for {slug}, and not the PR's head commit {}", repo.display(), if sha.is_empty() { "(unknown)" } else { sha });
    };
    let mut tried = vec![format!("pull/{number}/head")];
    if !head.is_empty() {
        tried.push(head.to_string());
    }
    for r in &tried {
        if git(repo, &["fetch", "--quiet", &remote, r]).is_ok_and(|o| o.code == 0) {
            let out = git(repo, &["rev-parse", "FETCH_HEAD"])?;
            let got = out.stdout.trim();
            if out.code == 0 && !got.is_empty() {
                return Ok(got.to_string());
            }
        }
    }
    bail!("couldn't fetch PR #{number} from {remote}")
}

/// A worktree of `repo` at `path`, detached at `rev` (no branch: nothing
/// it does lands on one); one that is there already moves to `rev`.
pub fn add_detached(repo: &Path, path: &Path, rev: &str) -> Result<()> {
    if path.exists() {
        if toplevel(path).as_deref().map(canon) != Some(canon(path)) {
            bail!("{} is there already and isn't a git worktree", path.display());
        }
        let out = git(path, &["checkout", "--quiet", "--detach", rev])?;
        if out.code != 0 {
            bail!("git checkout --detach {rev} in {}: {}", path.display(), out.stderr.trim());
        }
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        if let Some(root) = dir.parent().filter(|r| r.file_name().is_some_and(|n| n == DIR)) {
            let ignore = root.join(".gitignore");
            if !ignore.exists() {
                let _ = std::fs::write(ignore, "*\n");
            }
        }
    }
    let p = path.to_string_lossy();
    let mut out = git(repo, &["worktree", "add", "--detach", &p, rev])?;
    if out.code != 0 {
        let _ = git(repo, &["worktree", "prune"]);
        out = git(repo, &["worktree", "add", "--detach", &p, rev])?;
    }
    if out.code != 0 {
        bail!("git worktree add --detach in {}: {}", repo.display(), out.stderr.trim());
    }
    Ok(())
}

/// Takes a reviewer's checkout away, whatever is in it: it is a copy of a
/// PR's head to read, and nothing in it is anyone's work.
pub fn remove_detached(repo: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        let _ = git(repo, &["worktree", "prune"]);
        return Ok(());
    }
    let out = git(repo, &["worktree", "remove", "--force", &path.to_string_lossy()])?;
    if out.code != 0 {
        bail!("git worktree remove in {}: {}", repo.display(), out.stderr.trim());
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// The repo's untracked top-level `.env*` files.
fn env_files(repo: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(repo) else { return vec![] };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.starts_with(".env"))
        .filter(|n| !git(repo, &["ls-files", "--error-unmatch", "--", n]).is_ok_and(|o| o.code == 0))
        .collect();
    names.sort();
    names
}

/// Copies the repo's untracked top-level `.env*` files into a worktree that
/// has none of them. Returns the names copied.
pub fn copy_env(repo: &Path, worktree: &Path) -> Vec<String> {
    env_files(repo).into_iter().filter(|n| !worktree.join(n).exists() && std::fs::copy(repo.join(n), worktree.join(n)).is_ok()).collect()
}

/// The copies `copy_env` left in a worktree that are still what the repo
/// has, and still nobody's (one a session committed there is its branch's).
fn env_copies(repo: &Path, worktree: &Path) -> Vec<String> {
    env_files(repo)
        .into_iter()
        .filter(|n| match (std::fs::read(repo.join(n)), std::fs::read(worktree.join(n))) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        })
        .filter(|n| !git(worktree, &["ls-files", "--error-unmatch", "--", n]).is_ok_and(|o| o.code == 0))
        .collect()
}

/// A worktree's own top-level `.env*` files: not in git there, and not
/// what the repo has under that name (a copy the session changed, or one it
/// made). Nobody else has them, and git says nothing of one it ignores.
fn env_own(repo: &Path, worktree: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(worktree) else { return vec![] };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.starts_with(".env"))
        .filter(|n| !matches!((std::fs::read(repo.join(n)), std::fs::read(worktree.join(n))), (Ok(a), Ok(b)) if a == b))
        .filter(|n| !git(worktree, &["ls-files", "--error-unmatch", "--", n]).is_ok_and(|o| o.code == 0))
        .collect();
    names.sort();
    names
}

/// Why a worktree can't be taken away without losing something: changes
/// that aren't committed (an `.env*` of its own among them, ignored or
/// not), or commits on its branch that are nowhere else (not pushed, not on
/// another branch). None: it is clean.
pub fn dirty(place: &Place) -> Result<Option<String>> {
    let (repo, wt) = (Path::new(&place.repo), Path::new(&place.path));
    // New files count whatever the user's git is set to show (`status.showUntrackedFiles`).
    let out = git(wt, &["status", "--porcelain", "--untracked-files=normal"])?;
    if out.code != 0 {
        bail!("git status in {}: {}", wt.display(), out.stderr.trim());
    }
    let copies = env_copies(repo, wt);
    let mut changed: Vec<String> = out
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.strip_prefix("?? ").is_some_and(|f| copies.iter().any(|c| c == f.trim_matches('"'))))
        .map(|l| l.get(3..).unwrap_or(l).trim().to_string())
        .collect();
    for n in env_own(repo, wt) {
        if !changed.iter().any(|c| c.trim_matches('"') == n) {
            changed.push(n);
        }
    }
    if !changed.is_empty() {
        let names: Vec<&str> = changed.iter().take(3).map(String::as_str).collect();
        let more = if changed.len() > names.len() { format!(" and {} more", changed.len() - names.len()) } else { String::new() };
        return Ok(Some(format!("{} uncommitted change{} ({}{more})", changed.len(), if changed.len() == 1 { "" } else { "s" }, names.join(", "))));
    }
    // Commits that are only here: on no remote and on no other branch.
    let branch = current_branch(wt);
    let mut args: Vec<String> = ["rev-list", "--count", "HEAD", "--not", "--remotes"].map(String::from).to_vec();
    if let Some(b) = &branch {
        args.push(format!("--exclude={b}"));
    }
    args.push("--branches".into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git(wt, &refs)?;
    let only_here: u64 = out.stdout.trim().parse().unwrap_or(0);
    if out.code == 0 && only_here > 0 {
        let on = branch.map(|b| format!("on {b}")).unwrap_or_else(|| "on no branch".into());
        return Ok(Some(format!("{only_here} commit{} {on} not pushed or merged", if only_here == 1 { "" } else { "s" })));
    }
    Ok(None)
}

/// What came of taking a worktree away.
#[derive(Debug, Clone, PartialEq)]
pub enum Removal {
    Removed,
    /// Left as it is, and why.
    Kept(String),
    /// Its folder was gone already.
    Gone,
}

/// Takes a worktree away when nothing would be lost (`dirty`). Its branch
/// stays in the repo.
pub fn remove(place: &Place) -> Removal {
    let (repo, wt) = (Path::new(&place.repo), Path::new(&place.path));
    if !wt.exists() {
        let _ = git(repo, &["worktree", "prune"]);
        return Removal::Gone;
    }
    match dirty(place) {
        Ok(None) => {}
        Ok(Some(why)) => return Removal::Kept(why),
        Err(e) => return Removal::Kept(format!("{e:#}")),
    }
    // The copied `.env*` files would hold git back: they are the repo's own.
    let copies = env_copies(repo, wt);
    for n in &copies {
        let _ = std::fs::remove_file(wt.join(n));
    }
    match git(repo, &["worktree", "remove", &place.path]) {
        Ok(o) if o.code == 0 => {
            // The space's folder, once it is empty.
            if let Some(dir) = wt.parent() {
                let _ = std::fs::remove_dir(dir);
            }
            Removal::Removed
        }
        other => {
            for n in &copies {
                let _ = std::fs::copy(repo.join(n), wt.join(n));
            }
            Removal::Kept(match other {
                Ok(o) => o.stderr.trim().to_string(),
                Err(e) => format!("{e:#}"),
            })
        }
    }
}

/// The live terminal among `terminals` (each as `hc get` or `hc ls` has it)
/// that works in a worktree: it runs in that folder or under it, or was
/// given it by the tower (its label). Its name, else its address.
pub fn used_by(place: &Place, terminals: &[Value]) -> Option<String> {
    let inside = |cwd: &str| cwd == place.path || cwd.strip_prefix(place.path.as_str()).is_some_and(|rest| rest.starts_with('/'));
    terminals
        .iter()
        .filter(|t| t["state"].as_str().is_none_or(|s| s == "live"))
        .find(|t| {
            given(t).worktree.as_deref() == Some(place.path.as_str())
                || [t["foreground"]["cwd"].as_str(), t["cwd"].as_str()].into_iter().flatten().any(inside)
        })
        .map(|t| hc::name(t).or_else(|| t["address"].as_str()).unwrap_or("a session").to_string())
}

// ---- dev ports ----

/// `"3000-3050"` as its two ends (a range that doesn't read: 3000 to 3050).
pub fn parse_range(s: &str) -> (u16, u16) {
    let mut ends = s.split('-').map(|p| p.trim().parse::<u16>());
    match (ends.next(), ends.next(), ends.next()) {
        (Some(Ok(a)), Some(Ok(b)), None) if a > 0 && a <= b => (a, b),
        (Some(Ok(a)), None, None) if a > 0 => (a, a),
        _ => (3000, 3050),
    }
}

/// The first port of the range nobody has.
pub fn free_port(range: (u16, u16), taken: &BTreeSet<u16>) -> Option<u16> {
    (range.0..=range.1).find(|p| !taken.contains(p))
}

/// Something on this Mac listens on the port now.
fn listening(port: u16) -> bool {
    let reach = |addr: std::net::SocketAddr| std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok();
    reach(([127, 0, 0, 1], port).into()) || reach((std::net::Ipv6Addr::LOCALHOST, port).into())
}

/// A dev port for a new session: not one a live session was given, and
/// rather not one something listens on already.
pub fn pick_port(range: &str, handed: &BTreeSet<u16>) -> Option<u16> {
    let range = parse_range(range);
    let mut busy = handed.clone();
    while let Some(p) = free_port(range, &busy) {
        if !listening(p) {
            return Some(p);
        }
        busy.insert(p);
    }
    free_port(range, handed)
}

// ---- kept on the terminal ----

/// What a terminal's labels say it was given: its worktree, branch, repo and port.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Given {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// What a terminal (as `hc get` has it) was given.
pub fn given(t: &Value) -> Given {
    let label = |k: &str| t["labels"][k].as_str().filter(|v| !v.is_empty()).map(String::from);
    Given { worktree: label(LABEL_WORKTREE), branch: label(LABEL_BRANCH), repo: label(LABEL_REPO), port: label(LABEL_PORT).and_then(|p| p.parse().ok()) }
}

/// Keeps what a session was given on its terminal.
pub fn label(address: &str, place: Option<&Place>, port: Option<u16>) {
    let mut labels: Vec<String> = vec![];
    if let Some(p) = place {
        labels.push(format!("{LABEL_WORKTREE}={}", p.path));
        labels.push(format!("{LABEL_BRANCH}={}", p.branch));
        labels.push(format!("{LABEL_REPO}={}", p.repo));
    }
    if let Some(p) = port {
        labels.push(format!("{LABEL_PORT}={p}"));
    }
    if labels.is_empty() {
        return;
    }
    let mut args = vec!["label", address];
    args.extend(labels.iter().map(String::as_str));
    let _ = hc::run(&args);
}

/// The dev ports the live sessions here were given.
pub fn handed_ports() -> BTreeSet<u16> {
    hc::list_on(None)
        .unwrap_or_default()
        .iter()
        .filter(|t| t["state"].as_str().is_none_or(|s| s == "live"))
        .filter_map(|t| t["address"].as_str())
        .filter_map(hc::get)
        .filter_map(|t| given(&t).port)
        .collect()
}

/// What a session is told about where it works, after its task.
pub fn briefing(made: &Made, port: Option<u16>) -> String {
    let p = &made.place;
    let repo = Path::new(&p.repo).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.repo.clone());
    let mut s = if made.fresh {
        format!("You work in a fresh worktree of {repo} at {} on branch {}; dependencies may need installing", p.path, p.branch)
    } else {
        format!("You work in this space's worktree of {repo} at {} on branch {}, which was there already", p.path, p.branch)
    };
    if !made.copied.is_empty() {
        s += &format!(" ({} copied from the repo)", made.copied.join(", "));
    }
    match port {
        Some(port) => s += &format!("; use port {port} for a dev server."),
        None => s.push('.'),
    }
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// git, for a throwaway repo: nobody's own settings, nothing signed.
    pub(crate) fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=tower", "-c", "user.email=tower@example.invalid", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo with one commit, an ignored `.env.local` and a tracked `.env.example`.
    pub(crate) fn repo(base: &Path, name: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        sh(&dir, &["init", "-q"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        std::fs::write(dir.join("src/main.txt"), "code\n").unwrap();
        std::fs::write(dir.join(".gitignore"), ".env.local\nnode_modules/\n").unwrap();
        std::fs::write(dir.join(".env.example"), "KEY=\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join(".env.local"), "KEY=secret\n").unwrap();
        std::fs::write(dir.join(".env"), "PORT=3000\n").unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    pub(crate) fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tower-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn a_space_and_a_repo_name_the_worktree_and_its_branch() {
        assert_eq!(slug("B2C support"), "b2c-support");
        assert_eq!(slug("  Fix: the login!! "), "fix-the-login");
        assert_eq!(slug("m5_pro"), "m5_pro");
        assert!(slug("!!!").starts_with("space-") && slug("!!!") != slug("???"), "a name with no letters still has its own");
        let p = place(Path::new("/w/eyz/eyzflow"), "Fix login");
        assert_eq!(p, Place { repo: "/w/eyz/eyzflow".into(), path: "/w/eyz/.tower-worktrees/fix-login/eyzflow".into(), branch: "fix-login/eyzflow".into() });
        // A repo's name with odd characters still makes a branch git takes.
        assert_eq!(place(Path::new("/w/my app.v2"), "Shop").branch, "shop/my-app.v2");
        // A folder in one of the tower's worktrees says which repo it is of.
        assert_eq!(
            repo_of(Path::new("/w/eyz/.tower-worktrees/fix-login/eyzflow/src")),
            Some((PathBuf::from("/w/eyz/eyzflow"), PathBuf::from("/w/eyz/.tower-worktrees/fix-login/eyzflow")))
        );
        assert_eq!(repo_of(Path::new("/w/eyz/eyzflow")), None);
        assert_eq!(repo_of(Path::new("/w/eyz/.tower-worktrees/fix-login")), None);
    }

    #[test]
    fn a_worktree_is_made_once_with_the_repos_env_files_and_used_again() {
        let base = temp("worktree-make");
        let repo = repo(&base, "eyzflow");
        let made = ensure(&repo, "Fix login").unwrap().expect("a git repo");
        let wt = base.join(".tower-worktrees/fix-login/eyzflow");
        assert_eq!((made.place.path.as_str(), made.place.branch.as_str(), made.fresh), (wt.to_str().unwrap(), "fix-login/eyzflow", true));
        assert_eq!(made.cwd, made.place.path);
        assert_eq!(sh(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]), "fix-login/eyzflow");
        assert_eq!(sh(&wt, &["rev-parse", "HEAD"]), sh(&repo, &["rev-parse", "HEAD"]), "from the repo's HEAD");
        // The untracked `.env*` files came along; the tracked one is the checkout's own.
        assert_eq!(made.copied, [".env", ".env.local"]);
        assert_eq!(std::fs::read_to_string(wt.join(".env.local")).unwrap(), "KEY=secret\n");
        assert!(wt.join(".env.example").exists());
        assert_eq!(std::fs::read_to_string(base.join(".tower-worktrees/.gitignore")).unwrap(), "*\n");
        // Asked again (another session of the space): the same worktree, untouched.
        std::fs::write(wt.join(".env.local"), "KEY=changed\n").unwrap();
        let again = ensure(&repo, "fix LOGIN").unwrap().unwrap();
        assert_eq!((again.place.clone(), again.fresh, again.copied.len()), (made.place.clone(), false, 0));
        assert_eq!(std::fs::read_to_string(wt.join(".env.local")).unwrap(), "KEY=changed\n");
        // A folder inside the repo: the same folder inside the worktree.
        assert_eq!(ensure(&repo.join("src"), "Fix login").unwrap().unwrap().cwd, wt.join("src").to_str().unwrap());
        // Asked for the worktree itself: it is used as it is, not nested.
        let inside = ensure(&wt, "Other space").unwrap().unwrap();
        assert_eq!((inside.place.path.as_str(), inside.place.repo.as_str(), inside.fresh), (wt.to_str().unwrap(), repo.to_str().unwrap(), false));
        // Another space gets its own.
        let other = ensure(&repo, "Docs").unwrap().unwrap();
        assert_eq!((other.place.branch.as_str(), other.fresh), ("docs/eyzflow", true));
        // Not a git repo: used as it is.
        let plain = base.join("notes");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(ensure(&plain, "Fix login").unwrap(), None);
        assert_eq!(ensure(&base.join("nowhere"), "Fix login").unwrap(), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_branch_that_is_there_is_used_again() {
        let base = temp("worktree-branch");
        let repo = repo(&base, "api");
        sh(&repo, &["branch", "shop/api"]);
        std::fs::write(repo.join("later.txt"), "x\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "second"]);
        let made = ensure(&repo, "Shop").unwrap().unwrap();
        assert!(made.fresh);
        let wt = PathBuf::from(&made.place.path);
        assert_eq!(sh(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]), "shop/api");
        assert!(!wt.join("later.txt").exists(), "the branch as it was, not a new one from HEAD");
        // Its folder deleted by hand: made again on the same branch.
        std::fs::remove_dir_all(&wt).unwrap();
        let back = ensure(&repo, "Shop").unwrap().unwrap();
        assert!(back.fresh && wt.join("README.md").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_a_clean_worktree_is_taken_away() {
        let base = temp("worktree-remove");
        let repo = repo(&base, "web");
        let made = ensure(&repo, "Shop").unwrap().unwrap();
        let wt = PathBuf::from(&made.place.path);
        // Fresh, with only the copied env files and an ignored folder: clean.
        std::fs::create_dir_all(wt.join("node_modules/x")).unwrap();
        assert_eq!(dirty(&made.place).unwrap(), None);
        // An edit that isn't committed keeps it.
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();
        let why = dirty(&made.place).unwrap().unwrap();
        assert!(why.contains("1 uncommitted change") && why.contains("README.md"), "{why}");
        assert!(matches!(remove(&made.place), Removal::Kept(_)) && wt.exists());
        // Committed, but nowhere else: still kept.
        sh(&wt, &["commit", "-q", "-am", "work"]);
        let why = dirty(&made.place).unwrap().unwrap();
        assert_eq!(why, "1 commit on shop/web not pushed or merged");
        assert!(matches!(remove(&made.place), Removal::Kept(_)) && wt.join(".env.local").exists());
        // Merged into the repo's branch: nothing would be lost, so it goes; its branch stays.
        sh(&repo, &["merge", "-q", "shop/web"]);
        assert_eq!(dirty(&made.place).unwrap(), None);
        assert_eq!(remove(&made.place), Removal::Removed);
        assert!(!wt.exists() && !base.join(".tower-worktrees/shop").exists());
        assert!(sh(&repo, &["branch", "--list", "shop/web"]).contains("shop/web"));
        assert!(!sh(&repo, &["worktree", "list"]).contains(".tower-worktrees"));
        assert_eq!(remove(&made.place), Removal::Gone);
        // A new file nobody added, or an env file the session changed: kept.
        let made = ensure(&repo, "Docs").unwrap().unwrap();
        let wt = PathBuf::from(&made.place.path);
        std::fs::write(wt.join("notes.txt"), "x\n").unwrap();
        assert!(dirty(&made.place).unwrap().unwrap().contains("notes.txt"));
        std::fs::remove_file(wt.join("notes.txt")).unwrap();
        std::fs::write(wt.join(".env"), "PORT=3009\n").unwrap();
        assert!(dirty(&made.place).unwrap().unwrap().contains(".env"), "an untracked env file that isn't the repo's copy");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_new_file_or_an_env_file_of_its_own_keeps_a_worktree_whatever_git_shows() {
        let base = temp("worktree-unseen");
        let repo = repo(&base, "api");
        // The repo ignores its env files, as most do; the fixture's `.env.local` and `.env` are copied in.
        std::fs::write(repo.join(".gitignore"), "node_modules\n.env*\n").unwrap();
        sh(&repo, &["commit", "-q", "-am", "ignore env files"]);
        let made = ensure(&repo, "Shop").unwrap().unwrap();
        let wt = PathBuf::from(&made.place.path);
        assert_eq!(made.copied, [".env", ".env.local"]);
        assert_eq!(dirty(&made.place).unwrap(), None, "the copies are the repo's own");
        // A key added to the copy during the work is nowhere else, though git ignores the file.
        std::fs::write(wt.join(".env.local"), "KEY=secret\nSTRIPE=sk_live\n").unwrap();
        let why = dirty(&made.place).unwrap().unwrap();
        assert_eq!(why, "1 uncommitted change (.env.local)");
        assert!(matches!(remove(&made.place), Removal::Kept(_)) && wt.join(".env.local").exists());
        // One the session made there, which the repo never had: the same.
        std::fs::write(wt.join(".env.local"), "KEY=secret\n").unwrap();
        std::fs::write(wt.join(".env.test"), "DB=test\n").unwrap();
        assert!(dirty(&made.place).unwrap().unwrap().contains(".env.test"));
        std::fs::remove_file(wt.join(".env.test")).unwrap();
        assert_eq!(dirty(&made.place).unwrap(), None);
        // A git set to show no untracked files (`status.showUntrackedFiles = no`): a new file is still a change.
        sh(&repo, &["config", "status.showUntrackedFiles", "no"]);
        std::fs::write(wt.join("notes.txt"), "x\n").unwrap();
        assert!(dirty(&made.place).unwrap().unwrap().contains("notes.txt"));
        assert!(matches!(remove(&made.place), Removal::Kept(_)) && wt.join("notes.txt").exists());
        std::fs::remove_file(wt.join("notes.txt")).unwrap();
        assert_eq!(remove(&made.place), Removal::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_pushed_branch_is_clean() {
        let base = temp("worktree-pushed");
        let repo = repo(&base, "svc");
        let remote = base.join("remote.git");
        sh(&base, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        sh(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
        let made = ensure(&repo, "Shop").unwrap().unwrap();
        let wt = PathBuf::from(&made.place.path);
        std::fs::write(wt.join("a.txt"), "a\n").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "work"]);
        assert!(dirty(&made.place).unwrap().is_some());
        sh(&wt, &["push", "-q", "origin", "shop/svc"]);
        assert_eq!(dirty(&made.place).unwrap(), None, "its commits are on the remote");
        assert_eq!(remove(&made.place), Removal::Removed);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_worktree_someone_works_in_is_known_by_its_folder_or_its_label() {
        use serde_json::json;
        let p = Place { repo: "/w/app".into(), path: "/w/.tower-worktrees/shop/app".into(), branch: "shop/app".into() };
        // Given it by the tower; running in it; running in a folder under it.
        let given = json!({ "address": "t3", "state": "live", "name": "shop-app", "cwd": "/w", "labels": { "tower.worktree": "/w/.tower-worktrees/shop/app" } });
        let inside = json!({ "address": "t4", "state": "live", "cwd": "/w/.tower-worktrees/shop/app" });
        let under = json!({ "address": "t5", "foreground": { "cwd": "/w/.tower-worktrees/shop/app/web" }, "cwd": "/w" });
        assert_eq!(used_by(&p, std::slice::from_ref(&given)).as_deref(), Some("shop-app"));
        assert_eq!(used_by(&p, std::slice::from_ref(&inside)).as_deref(), Some("t4"));
        assert_eq!(used_by(&p, std::slice::from_ref(&under)).as_deref(), Some("t5"));
        // Not one that has ended, one elsewhere, or one in a folder that only begins alike.
        let ended = json!({ "address": "t6", "state": "exited", "cwd": "/w/.tower-worktrees/shop/app" });
        let elsewhere = json!({ "address": "t7", "state": "live", "cwd": "/w/app" });
        let alike = json!({ "address": "t8", "state": "live", "cwd": "/w/.tower-worktrees/shop/app-2" });
        assert_eq!(used_by(&p, &[ended, elsewhere, alike]), None);
        assert_eq!(used_by(&p, &[]), None);
    }

    #[test]
    fn each_session_gets_a_port_nobody_has() {
        assert_eq!(parse_range("3000-3050"), (3000, 3050));
        assert_eq!(parse_range(" 4000 - 4002 "), (4000, 4002));
        assert_eq!(parse_range("5173"), (5173, 5173));
        for bad in ["", "x", "3050-3000", "0-10", "1-2-3"] {
            assert_eq!(parse_range(bad), (3000, 3050), "{bad}");
        }
        let mut taken = BTreeSet::new();
        assert_eq!(free_port((3000, 3002), &taken), Some(3000));
        taken.extend([3000, 3001]);
        assert_eq!(free_port((3000, 3002), &taken), Some(3002));
        taken.insert(3002);
        assert_eq!(free_port((3000, 3002), &taken), None, "all handed out");
        // A port something listens on is passed over while another is free.
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = l.local_addr().unwrap().port();
        if busy < u16::MAX && !listening(busy + 1) {
            let got = pick_port(&format!("{busy}-{}", busy + 1), &BTreeSet::new());
            assert_eq!(got, Some(busy + 1));
        }
        assert_eq!(pick_port(&format!("{busy}"), &BTreeSet::new()), Some(busy), "the only one of the range, in use or not");
        assert_eq!(pick_port(&format!("{busy}"), &BTreeSet::from([busy])), None);
    }

    #[test]
    fn what_a_session_was_given_is_read_from_its_labels_and_said() {
        let t = serde_json::json!({ "labels": { "tower.worktree": "/w/.tower-worktrees/shop/api", "tower.branch": "shop/api", "tower.repo": "/w/api", "tower.port": "3004", "where": "Shop / api" } });
        let g = given(&t);
        assert_eq!((g.worktree.as_deref(), g.branch.as_deref(), g.repo.as_deref(), g.port), (Some("/w/.tower-worktrees/shop/api"), Some("shop/api"), Some("/w/api"), Some(3004)));
        assert_eq!(given(&serde_json::json!({ "labels": { "where": "Shop / api" } })), Given::default());
        let made = Made {
            place: Place { repo: "/w/api".into(), path: "/w/.tower-worktrees/shop/api".into(), branch: "shop/api".into() },
            fresh: true,
            copied: vec![],
            cwd: "/w/.tower-worktrees/shop/api".into(),
        };
        assert_eq!(
            briefing(&made, Some(3004)),
            "You work in a fresh worktree of api at /w/.tower-worktrees/shop/api on branch shop/api; dependencies may need installing; use port 3004 for a dev server."
        );
        let again = Made { fresh: false, copied: vec![], ..made.clone() };
        assert_eq!(briefing(&again, None), "You work in this space's worktree of api at /w/.tower-worktrees/shop/api on branch shop/api, which was there already.");
        assert!(briefing(&Made { copied: vec![".env.local".into()], ..made }, Some(3001)).contains("(.env.local copied from the repo); use port 3001"));
    }
}
