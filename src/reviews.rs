//! A reviewer for the PRs a space's sessions open (`docs/quiet-tower.md`,
//! "PR review").
//!
//! - **The switch** is the space's (`Mission::review`, on unless the user
//!   turned it off: `tower space review <space> off`, `tower launch
//!   --no-review`, the New space card, the space's head).
//! - **A PR found** (`crate::prs`: a session's turn printed its link) in a
//!   space with review on gets ONE reviewer session: same space, same repo,
//!   same account, named `<space>-review-<repo>-<n>`, in a checkout of its
//!   own detached at the PR's head. Never for a PR a reviewer made, never
//!   twice for one PR.
//! - **The reviewer** looks for critical issues only, comments on the PR
//!   with `gh`, messages the owner itself (`tower queue`: the tower passes
//!   nothing on) and reports to the tower (`tower review report|agree`).
//! - **The owner** gets one short note from the tower per round that found
//!   something critical (who the reviewer is and what to do; never the
//!   comments themselves), and may say it fixed them (`tower review fixed`).
//! - **Rounds**: three reports with critical issues and no agreement put it
//!   to the user, as one decision; the answer goes to both sessions.
//! - **The end**: agreed, the PR merged or closed, the space closed or its
//!   review turned off. The reviewer session ends; the record stays.
//!
//! This file is the part without side effects: the store (`reviews.json` in
//! the state folder), its steps, the names, the reviewer's instructions,
//! the owner's note, the decision, and what the PR's chip and merge card
//! say. The daemon runs it (`daemon::reviews`).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::prs::Pr;

/// Rounds with critical issues and no agreement before the user decides.
pub const ROUNDS: u32 = 3;
/// Reviews that are over and kept in the store (the newest).
pub const KEEP_OVER: usize = 50;
/// A session's name is cut to this, as `tower launch` cuts one.
const NAME_CHARS: usize = 48;
/// The label a reviewer's terminal carries (`reviewer`), and the PR it reviews.
pub const LABEL_ROLE: &str = "tower.role";
pub const LABEL_REVIEW: &str = "tower.review";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The reviewer reads it (first, or again after the owner's fixes).
    #[default]
    Reviewing,
    /// It found critical issues: the owner fixes them.
    Changes,
    /// Both agree: the reviewer approved it.
    Agreed,
    /// Three rounds and no agreement: the user decides.
    Stuck,
    /// Over without agreement: the PR merged or closed, the space closed,
    /// review turned off, the reviewer gone or never opened.
    Ended,
}

impl Status {
    pub fn word(&self) -> &'static str {
        match self {
            Status::Reviewing => "reviewing",
            Status::Changes => "changes",
            Status::Agreed => "agreed",
            Status::Stuck => "stuck",
            Status::Ended => "ended",
        }
    }

    pub fn parse(w: &str) -> Status {
        match w {
            "changes" => Status::Changes,
            "agreed" => Status::Agreed,
            "stuck" => Status::Stuck,
            "ended" => Status::Ended,
            _ => Status::Reviewing,
        }
    }

    /// Nothing more happens to it.
    pub fn over(&self) -> bool {
        matches!(self, Status::Agreed | Status::Ended)
    }
}

/// One thing that happened to a review, for `tower review show`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Step {
    pub at_ms: u64,
    /// `opened`, `started`, `report`, `fixed`, `agreed`, `stuck`, `ended`.
    pub what: String,
    /// The terminal that said so (empty: the tower).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical: Option<u32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
}

/// One PR's review, as `reviews.json` keeps it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Review {
    /// The PR's key, `owner/name#12`.
    pub key: String,
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub title: String,
    pub base: String,
    pub head: String,
    /// The head commit when the review began.
    pub head_sha: String,
    pub space: String,
    /// The session that opened the PR: its address and name.
    pub owner: String,
    pub owner_name: String,
    /// When the owner's terminal was made: which terminal at that address it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_created_ms: Option<u64>,
    /// The reviewer session ("" until it is open, or when it never opened).
    pub reviewer: String,
    pub reviewer_name: String,
    /// When the reviewer's terminal was made: which terminal at that address it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer_created_ms: Option<u64>,
    /// The Claude account both run on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Where the reviewer works: its own checkout of the PR's head.
    pub folder: String,
    /// The repo that checkout is a worktree of ("": a plain folder).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub checkout_of: String,
    pub status: Status,
    /// Reports that found critical issues.
    pub rounds: u32,
    /// Critical issues in the last report.
    pub critical: u32,
    /// The last word on it: a report's line, the owner's, the agreement's, why it ended.
    pub line: String,
    /// The owner's last word (`tower review fixed`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub fixed: String,
    /// The decision raised when it got stuck.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<u64>,
    pub steps: Vec<Step>,
}

impl Review {
    /// What the PR's chip and merge card know of it.
    pub fn brief(&self) -> Brief {
        let mut b = Brief {
            status: self.status.word().into(),
            rounds: self.rounds,
            critical: self.critical,
            reviewer: self.reviewer.clone(),
            reviewer_name: self.reviewer_name.clone(),
            line: String::new(),
        };
        b.line = chip_line(&b);
        b
    }

    /// As `tower review --json`, the state and the phone carry it.
    pub fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["chip"] = json!(self.brief().line);
        v
    }

    fn step(&mut self, what: &str, by: &str, critical: Option<u32>, text: &str, now: u64) {
        self.updated_ms = now;
        self.steps.push(Step { at_ms: now, what: what.into(), by: by.into(), critical, text: text.into() });
        if self.steps.len() > 40 {
            self.steps.remove(1);
        }
    }

    /// `PR #12 (o/app)`.
    pub fn short(&self) -> String {
        format!("PR #{} ({})", self.number, self.repo)
    }
}

/// A PR's review in a few words, kept with the PR (`prs.json`): its chip,
/// the merge card and the phone read it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Brief {
    /// `reviewing`, `changes`, `agreed`, `stuck`, `ended`.
    pub status: String,
    pub rounds: u32,
    pub critical: u32,
    pub reviewer: String,
    pub reviewer_name: String,
    /// What the chip says (`chip_line`).
    pub line: String,
}

/// What the PR's chip says of its review: "Reviewing…", "Review: 2
/// critical, round 2", "Review: agreed", "Review: stuck after 3 rounds",
/// "Review: ended without agreement".
pub fn chip_line(b: &Brief) -> String {
    match Status::parse(&b.status) {
        Status::Reviewing if b.rounds == 0 => "Reviewing…".into(),
        Status::Reviewing => format!("Reviewing the fixes… (after round {})", b.rounds),
        Status::Changes => format!("Review: {} critical, round {}", b.critical, b.rounds),
        Status::Agreed => "Review: agreed".into(),
        Status::Stuck => format!("Review: stuck after {} rounds, yours to decide", b.rounds),
        Status::Ended if b.critical > 0 => format!("Review: ended, {} critical left", b.critical),
        Status::Ended => "Review: ended without agreement".into(),
    }
}

/// What the merge card (and a phone's confirm) says when the review isn't
/// agreed: the user may merge all the same, and knows it plainly.
pub fn merge_warning(b: &Brief) -> Option<String> {
    let s = match Status::parse(&b.status) {
        Status::Agreed => return None,
        Status::Reviewing if b.rounds == 0 => "The review isn't done: its reviewer is still reading it.".to_string(),
        Status::Reviewing => format!("The review isn't agreed: the reviewer checks the fixes (after round {}).", b.rounds),
        Status::Changes => format!("The review isn't agreed: {} critical issue{} open, round {}.", b.critical, if b.critical == 1 { "" } else { "s" }, b.rounds),
        Status::Stuck => format!("The review isn't agreed: the reviewer and the owner disagree after {} rounds.", b.rounds),
        Status::Ended => "Its review ended without agreement.".to_string(),
    };
    Some(s)
}

// ---- who reviews, and where ----

/// The reviewer's name: `<space>-review-<repo>-<n>` in Herdr's name
/// alphabet, cut as session names are (the space's part gives way first),
/// never `tower-…` (that is a brain's), unique among `taken`.
pub fn reviewer_name(space: &str, repo: &str, number: u64, taken: &[String]) -> String {
    let repo = crate::worktrees::slug(repo.rsplit('/').next().unwrap_or(repo));
    let mut tail = format!("review-{repo}-{number}");
    if tail.len() > NAME_CHARS {
        let keep = NAME_CHARS - format!("-{number}").len();
        let cut: String = format!("review-{repo}").chars().take(keep).collect();
        tail = format!("{}-{number}", cut.trim_end_matches('-'));
    }
    let space = crate::worktrees::slug(space);
    let room = NAME_CHARS.saturating_sub(tail.len() + 1);
    let head: String = space.chars().take(room).collect();
    let head = head.trim_end_matches('-');
    let base = [format!("{head}-{tail}"), format!("{tail}-{head}")]
        .into_iter()
        .filter(|n| !head.is_empty() || !n.starts_with('-'))
        .map(|n| n.chars().take(NAME_CHARS).collect::<String>().trim_matches('-').to_string())
        .find(|n| !crate::brain::is_brain_name(n) && !n.is_empty())
        .unwrap_or_else(|| tail.clone());
    crate::launch::unique(&base, taken)
}

/// Where a reviewer's checkout of a PR's head goes, beside the space's own
/// worktrees: `<repo>/../.tower-worktrees/<space>/review-<repo>-<n>`.
pub fn checkout_place(repo_dir: &Path, space: &str, number: u64) -> PathBuf {
    let name = repo_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    repo_dir.parent().unwrap_or(repo_dir).join(crate::worktrees::DIR).join(crate::worktrees::slug(space)).join(format!("review-{}-{number}", crate::worktrees::slug(&name)))
}

/// What the reviewer may and may not do in its checkout, as Claude Code
/// reads it there (`.claude/settings.local.json`): read and comment, never
/// push, merge, commit or edit.
pub fn reviewer_settings() -> Value {
    json!({
        "permissions": {
            "allow": [
                "Bash(gh pr view:*)", "Bash(gh pr diff:*)", "Bash(gh pr checkout:*)", "Bash(gh pr review:*)", "Bash(gh pr comment:*)",
                "Bash(gh api:*)", "Bash(git diff:*)", "Bash(git log:*)", "Bash(git show:*)", "Bash(git status:*)", "Bash(git fetch:*)",
                "Bash(tower review:*)", "Bash(tower queue:*)", "Bash(hc read:*)"
            ],
            "deny": [
                "Bash(git push:*)", "Bash(git commit:*)", "Bash(git merge:*)", "Bash(git rebase:*)", "Bash(git reset:*)",
                "Bash(gh pr merge:*)", "Bash(gh pr close:*)", "Bash(gh pr edit:*)", "Bash(gh pr ready:*)",
                "Edit", "Write", "NotebookEdit"
            ]
        }
    })
}

/// The reviewer's first task: who it is, where, and the review, critical issues only.
pub fn instructions(r: &Review, goal: &str, done_when: &str, checkout: bool) -> String {
    let (n, repo, key) = (r.number, r.repo.as_str(), r.key.as_str());
    let title = if r.title.trim().is_empty() { String::new() } else { format!(" \"{}\"", r.title.trim()) };
    let branches = match (r.head.as_str(), r.base.as_str()) {
        ("", "") => String::new(),
        (h, b) => format!(", {} into {}", if h.is_empty() { "?" } else { h }, if b.is_empty() { "?" } else { b }),
    };
    let goal = match (goal.trim(), done_when.trim()) {
        ("", _) => String::new(),
        (g, "") => format!(" The space's goal: \"{g}\"."),
        (g, d) => format!(" The space's goal: \"{g}\". It is done when: {}.", d.trim_end_matches('.')),
    };
    let place = if checkout {
        format!(
            "You work in a checkout of the PR's head at {}, detached: it is no branch of anyone's. To see the PR as it is now: gh pr checkout {n} --repo {repo} --detach (or gh pr diff {n} --repo {repo}).",
            r.folder
        )
    } else {
        format!("You work in {}, a folder of your own: read the PR with gh pr diff {n} --repo {repo} and gh pr view {n} --repo {repo}.", r.folder)
    };
    let sha = if r.head_sha.is_empty() { "<the head commit>".to_string() } else { r.head_sha.clone() };
    format!(
        "[tower] You are the reviewer of PR #{n} in {repo}{title}{branches}: {url}. The session that opened it is its owner: {owner} ({addr}), in the space \"{space}\".{goal}\n\n\
{place}\n\n\
Review it for CRITICAL issues only:\n\
- wrong behaviour against the ticket or the space's goal;\n\
- a crash;\n\
- security;\n\
- data loss or corruption;\n\
- a build or test failure the PR introduces;\n\
- an API or contract break with another repo.\n\
Nothing else: no style, naming, nits or suggestions. What isn't critical, leave out.\n\n\
1. Post each critical issue as a review comment on the PR, inline where you can, opening with \"Critical:\": gh api repos/{repo}/pulls/{n}/comments -f body='Critical: …' -f commit_id={sha} -f path=<file> -F line=<line> -f side=RIGHT (one that has no line: gh pr comment {n} --repo {repo} --body 'Critical: …').\n\
2. Message the owner yourself, with the list and the links: tower queue {addr} \"<the critical issues, each with its link>\". The tower doesn't pass your messages on: you two talk directly.\n\
3. Report to the tower: tower review report {key} --critical <n> \"<one line>\".\n\n\
None critical: approve it at once (gh pr review {n} --repo {repo} --approve --body \"<a short note>\"), tell the owner in a line (tower queue {addr} \"…\"), then tower review agree {key} \"<one line>\".\n\
When the owner says it's fixed: check again (gh pr checkout {n} --repo {repo} --detach first), then agree (approve as above, then tower review agree {key}), or post what is still critical, tell the owner, and report again. After {ROUNDS} rounds without agreement the tower puts it to the user, and their word comes to both of you.\n\
If GitHub refuses the approval (a PR of your own account can't be approved), post it as a comment instead (gh pr review {n} --repo {repo} --comment --body \"Approved: …\") and agree all the same.\n\
Never merge, close or push; never edit, commit or change the owner's code or branch: what needs changing, the owner changes. Don't ask the user anything: what you two can't agree on, the tower puts to them. Once you have agreed, your session ends.",
        url = r.url,
        owner = if r.owner_name.is_empty() { "its session".to_string() } else { r.owner_name.clone() },
        addr = r.owner,
        space = r.space,
    )
}

/// The tower's one note to the owner after a round that found critical
/// issues: who the reviewer is and what to do. Nothing of the comments
/// themselves: the reviewer sent those to the owner directly.
pub fn owner_note(r: &Review) -> String {
    let who = if r.reviewer_name.is_empty() { r.reviewer.clone() } else { format!("{} {}", r.reviewer_name, r.reviewer) };
    format!(
        "[tower] The reviewer ({who}) left {n} critical comment{s} on PR #{num} ({repo}): fix {them}, push, answer each thread, then tell the reviewer (tower queue {addr} \"…\"). If you like, tell the tower too: tower review fixed {key} \"<one line>\".",
        n = r.critical,
        s = if r.critical == 1 { "" } else { "s" },
        them = if r.critical == 1 { "it" } else { "them" },
        num = r.number,
        repo = r.repo,
        addr = r.reviewer,
        key = r.key,
    )
}

/// The decision for a review stuck after `ROUNDS` rounds: its line, what
/// to read, and the three answers (each one's words go to both sessions).
pub fn stuck_decision(r: &Review) -> (String, String, Vec<crate::decisions::Opt>) {
    let on = r.line.trim().trim_end_matches('.');
    let on = if on.is_empty() { format!("{} critical issue{}", r.critical, if r.critical == 1 { "" } else { "s" }) } else { crate::agenda::clip(on, 160) };
    let title = format!("The reviewer and the owner of PR #{} ({}) don't agree on: {on}", r.number, r.repo);
    let mut detail = format!(
        "{}\nRound {}: {} critical. The reviewer is {} ({}); the owner is {} ({}).",
        r.url,
        r.rounds,
        r.critical,
        if r.reviewer_name.is_empty() { "its session" } else { &r.reviewer_name },
        r.reviewer,
        if r.owner_name.is_empty() { "its session" } else { &r.owner_name },
        r.owner
    );
    if !r.fixed.is_empty() {
        detail += &format!(" The owner said: {}", r.fixed);
    }
    let about = format!("About the review of PR #{} ({})", r.number, r.repo);
    let opt = |label: &str, why: &str, text: String| crate::decisions::Opt { label: label.into(), why: why.into(), pick: false, answers: Value::Null, text };
    let options = vec![
        opt(
            "Side with the owner",
            "the change stands as it is",
            format!(
                "{about}: the user sides with the owner, and the change stands as it is. Reviewer: approve it (gh pr review {n} --repo {repo} --approve, or --comment if GitHub refuses) and run tower review agree {key}.",
                n = r.number,
                repo = r.repo,
                key = r.key
            ),
        ),
        opt(
            "Side with the reviewer",
            "the owner fixes what is still critical",
            format!(
                "{about}: the user sides with the reviewer. Owner: fix what the reviewer still calls critical, push, answer the threads and tell the reviewer. Reviewer: check it again, then agree (tower review agree {key}).",
                key = r.key
            ),
        ),
        opt("I'll look", "you read the PR yourself", format!("{about}: the user will look at it themselves. Owner and reviewer: leave it as it is until they say more.")),
    ];
    (title, detail, options)
}

// ---- the store ----

/// What a report changed, for the daemon to act on.
#[derive(Debug, Clone, PartialEq)]
pub struct Reported {
    pub review: Review,
    /// The owner gets the tower's note (a round with critical issues while no decision about it is open).
    pub note_owner: bool,
    /// It got stuck now: the user is asked, once.
    pub raise: bool,
}

/// Every review the tower keeps: `reviews.json` in the state folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub reviews: Vec<Review>,
}

pub fn path() -> PathBuf {
    crate::config::state_dir().join("reviews.json")
}

/// A PR as `tower review` is given it: its key (`owner/name#12`), its link,
/// or its number (`#12`, `12`).
pub fn pr_of(text: &str) -> Option<(Option<String>, u64)> {
    let t = text.trim();
    if let Some((repo, n)) = crate::prs::links(t).into_iter().next() {
        return Some((Some(repo), n));
    }
    if let Some((repo, n)) = t.rsplit_once('#')
        && let Ok(n) = n.trim().parse::<u64>()
    {
        let repo = repo.trim();
        return Some(((!repo.is_empty()).then(|| repo.to_string()), n));
    }
    t.parse::<u64>().ok().map(|n| (None, n))
}

impl Book {
    pub fn load_from(path: &Path) -> Book {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// Written whole through a temporary file and a rename.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&Review> {
        self.reviews.iter().find(|r| r.key == key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Review> {
        self.reviews.iter_mut().find(|r| r.key == key)
    }

    /// The reviews not over yet.
    pub fn active(&self) -> impl Iterator<Item = &Review> {
        self.reviews.iter().filter(|r| !r.status.over())
    }

    /// The review `text` names (`pr_of`). A bare number is the review of
    /// that number `by` reviews or owns, else the only open one with it.
    pub fn find(&self, text: &str, by: Option<&str>) -> Result<&Review> {
        let Some((repo, n)) = pr_of(text) else { bail!("give the PR: its key (owner/repo#12), its link, or #12") };
        let fits: Vec<&Review> = self.reviews.iter().filter(|r| r.number == n && repo.as_deref().is_none_or(|x| x.eq_ignore_ascii_case(&r.repo))).collect();
        if fits.is_empty() {
            bail!("no review of {} (tower review lists them)", text.trim());
        }
        // The caller's own (it reviews or owns it), else the only one not over, else the newest.
        if let Some(r) = fits.iter().find(|r| by.is_some_and(|b| r.reviewer == b || r.owner == b)) {
            return Ok(r);
        }
        let open: Vec<&&Review> = fits.iter().filter(|r| !r.status.over()).collect();
        match open.as_slice() {
            [one] => Ok(one),
            [] => Ok(fits.iter().max_by_key(|r| r.updated_ms).copied().unwrap_or(fits[0])),
            many => bail!("several PRs #{n} are under review ({}): give its key", many.iter().map(|r| r.key.as_str()).collect::<Vec<_>>().join(", ")),
        }
    }

    /// `address` is a reviewer's terminal: of a review not over, or of any
    /// review when it is the same terminal (`created_ms`).
    pub fn is_reviewer(&self, address: &str, created_ms: Option<u64>) -> bool {
        !address.is_empty()
            && self.reviews.iter().any(|r| {
                r.reviewer == address && (!r.status.over() || created_ms.is_some_and(|c| r.reviewer_created_ms == Some(c)))
            })
    }

    /// Whether a PR found now gets a reviewer. `review_on`: its space's
    /// switch (a closed space: off); `by_reviewer`: the session that opened
    /// it is a reviewer (by the store, or its terminal's label).
    pub fn wants_reviewer(&self, pr: &Pr, review_on: bool, by_reviewer: bool) -> Result<(), String> {
        if pr.space.as_deref().is_none_or(|s| s.trim().is_empty()) {
            return Err("its session is in no space".into());
        }
        if !pr.open() {
            return Err(format!("it is {}", if pr.state.is_empty() { "not open" } else { &pr.state }));
        }
        if !review_on {
            return Err("review is off for its space".into());
        }
        if by_reviewer || self.is_reviewer(&pr.session, None) {
            return Err("a reviewer opened it".into());
        }
        if self.get(&pr.key()).is_some() {
            return Err("it has a review already".into());
        }
        Ok(())
    }

    /// A review begins for `pr` (its owner's account and terminal): kept before its reviewer opens.
    pub fn begin(&mut self, pr: &Pr, account: Option<String>, owner_created_ms: Option<u64>, now: u64) -> Review {
        let mut r = Review {
            key: pr.key(),
            repo: pr.repo.clone(),
            number: pr.number,
            url: pr.url.clone(),
            title: pr.title.clone(),
            base: pr.base.clone(),
            head: pr.head.clone(),
            head_sha: pr.head_sha.clone(),
            space: pr.space.clone().unwrap_or_default(),
            owner: pr.session.clone(),
            owner_name: pr.name.clone(),
            owner_created_ms,
            account,
            created_ms: now,
            ..Default::default()
        };
        r.step("opened", "", None, "", now);
        self.reviews.retain(|x| x.key != r.key);
        self.reviews.push(r.clone());
        self.prune();
        r
    }

    /// Its reviewer is about to open, as `name` in `folder` (a checkout of `checkout_of`).
    pub fn placed(&mut self, key: &str, name: &str, folder: &str, checkout_of: &str) -> Option<Review> {
        let r = self.get_mut(key)?;
        (r.reviewer_name, r.folder, r.checkout_of) = (name.into(), folder.into(), checkout_of.into());
        Some(r.clone())
    }

    /// Its reviewer opened at `address`.
    pub fn started(&mut self, key: &str, address: &str, created_ms: Option<u64>, now: u64) -> Option<Review> {
        let r = self.get_mut(key)?;
        r.reviewer = address.into();
        r.reviewer_created_ms = created_ms;
        let line = format!("{} ({address}) reviews it", r.reviewer_name);
        r.step("started", "", None, &line, now);
        Some(r.clone())
    }

    /// `tower review report`: a round with `critical` issues (0: none found
    /// this time). `decision_open`: the decision about it waits for the user.
    pub fn report(&mut self, key: &str, critical: u32, text: &str, by: &str, decision_open: bool, now: u64) -> Result<Reported> {
        let Some(r) = self.get_mut(key) else { bail!("no review of {key}") };
        if r.status.over() {
            bail!("the review of {} is over ({}): nothing to report", r.short(), r.status.word());
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        r.critical = critical;
        r.line = text.clone();
        r.step("report", by, Some(critical), &text, now);
        let mut raise = false;
        if critical == 0 {
            // Nothing critical: it agrees next (`agree`); a stuck one is no longer stuck.
            r.status = Status::Reviewing;
        } else {
            r.rounds += 1;
            if r.rounds >= ROUNDS {
                raise = r.status != Status::Stuck && r.decision.is_none();
                r.status = Status::Stuck;
                if raise {
                    r.step("stuck", "", Some(critical), "", now);
                }
            } else {
                r.status = Status::Changes;
            }
        }
        let note_owner = critical > 0 && !raise && !decision_open;
        Ok(Reported { review: r.clone(), note_owner, raise })
    }

    /// `tower review fixed`: the owner's word that it fixed what was critical.
    pub fn fixed(&mut self, key: &str, text: &str, by: &str, now: u64) -> Result<Review> {
        let Some(r) = self.get_mut(key) else { bail!("no review of {key}") };
        if r.status.over() {
            bail!("the review of {} is over ({})", r.short(), r.status.word());
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        r.fixed = text.clone();
        if r.status == Status::Changes {
            r.status = Status::Reviewing;
        }
        r.step("fixed", by, None, &text, now);
        Ok(r.clone())
    }

    /// `tower review agree`: both agree; the reviewer approved it.
    pub fn agree(&mut self, key: &str, text: &str, by: &str, now: u64) -> Result<Review> {
        let Some(r) = self.get_mut(key) else { bail!("no review of {key}") };
        if r.status.over() {
            bail!("the review of {} is over already ({})", r.short(), r.status.word());
        }
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        r.status = Status::Agreed;
        r.critical = 0;
        r.line = if text.is_empty() { "agreed".into() } else { text.clone() };
        r.ended_ms = Some(now);
        r.step("agreed", by, None, &text, now);
        Ok(r.clone())
    }

    /// It is over without agreement (`why`). None when it was over already.
    pub fn end(&mut self, key: &str, why: &str, now: u64) -> Option<Review> {
        let r = self.get_mut(key)?;
        if r.status.over() {
            return None;
        }
        r.status = Status::Ended;
        r.line = why.to_string();
        r.ended_ms = Some(now);
        r.step("ended", "", None, why, now);
        Some(r.clone())
    }

    /// The decision raised about it.
    pub fn decided(&mut self, key: &str, id: &str) {
        if let Some(r) = self.get_mut(key) {
            r.decision = Some(id.to_string());
        }
    }

    /// The newest `KEEP_OVER` that are over, and every one that isn't.
    fn prune(&mut self) {
        let over = self.reviews.iter().filter(|r| r.status.over()).count();
        if over <= KEEP_OVER {
            return;
        }
        let mut ends: Vec<u64> = self.reviews.iter().filter(|r| r.status.over()).map(|r| r.ended_ms.unwrap_or(r.updated_ms)).collect();
        ends.sort_unstable();
        let cut = ends[over - KEEP_OVER];
        let mut dropped = 0;
        self.reviews.retain(|r| {
            let old = r.status.over() && r.ended_ms.unwrap_or(r.updated_ms) < cut && dropped < over - KEEP_OVER;
            dropped += usize::from(old);
            !old
        });
    }

    /// What the state carries: every review not over, and those over in the last day.
    pub fn views(&self, now: u64) -> Vec<Value> {
        let day = 24 * 3_600_000;
        let mut list: Vec<&Review> = self.reviews.iter().filter(|r| !r.status.over() || now.saturating_sub(r.ended_ms.unwrap_or(r.updated_ms)) < day).collect();
        list.sort_by_key(|r| std::cmp::Reverse(r.updated_ms));
        list.iter().map(|r| r.view()).collect()
    }
}

/// A review in a line, for `tower review`.
pub fn row(r: &Review) -> String {
    let who = if r.reviewer.is_empty() { "no reviewer yet".to_string() } else { format!("{} ({})", r.reviewer_name, r.reviewer) };
    let line = if r.line.is_empty() { String::new() } else { format!(": {}", r.line) };
    format!("{:<22} {:<16} {} · by {} · {who}{line}", r.key, r.space, r.brief().line, if r.owner_name.is_empty() { &r.owner } else { &r.owner_name })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(n: u64, session: &str, space: Option<&str>) -> Pr {
        Pr {
            repo: "acme/shop".into(),
            number: n,
            url: format!("https://github.com/acme/shop/pull/{n}"),
            title: "Cart totals".into(),
            base: "dev".into(),
            head: "fix-login/shop".into(),
            head_sha: "abc123".into(),
            state: "open".into(),
            session: session.into(),
            name: "shop".into(),
            space: space.map(String::from),
            ..Default::default()
        }
    }

    fn begun(b: &mut Book, n: u64) -> String {
        let r = b.begin(&pr(n, "t3", Some("fix-login")), Some("orca".into()), None, 10);
        b.placed(&r.key, "fix-login-review-shop-12", "/w/.tower-worktrees/fix-login/review-shop-12", "/w/shop");
        b.started(&r.key, "t9", Some(500), 20);
        r.key
    }

    #[test]
    fn a_review_goes_round_by_round_and_is_stuck_after_three() {
        let mut b = Book::default();
        let k = begun(&mut b, 12);
        let r = b.get(&k).unwrap();
        assert_eq!((r.status, r.reviewer.as_str(), r.owner.as_str(), r.account.as_deref()), (Status::Reviewing, "t9", "t3", Some("orca")));
        assert_eq!(r.brief().line, "Reviewing…");
        // Round 1: two critical issues: the owner gets the tower's note.
        let rep = b.report(&k, 2, "a crash on empty carts; totals lose cents", "t9", false, 30).unwrap();
        assert_eq!((rep.review.status, rep.review.rounds, rep.note_owner, rep.raise), (Status::Changes, 1, true, false));
        assert_eq!(rep.review.brief().line, "Review: 2 critical, round 1");
        // The owner fixed them: the reviewer checks again.
        let f = b.fixed(&k, "fixed both, pushed", "t3", 40).unwrap();
        assert_eq!((f.status, f.fixed.as_str()), (Status::Reviewing, "fixed both, pushed"));
        assert_eq!(f.brief().line, "Reviewing the fixes… (after round 1)");
        let rep = b.report(&k, 1, "totals still lose cents", "t9", false, 50).unwrap();
        assert_eq!((rep.review.rounds, rep.note_owner, rep.raise), (2, true, false));
        // Round 3 without agreement: stuck, the user is asked once, and the owner isn't noted (the user's word comes).
        let rep = b.report(&k, 1, "totals still lose cents", "t9", false, 60).unwrap();
        assert_eq!((rep.review.status, rep.review.rounds, rep.note_owner, rep.raise), (Status::Stuck, 3, false, true));
        assert_eq!(rep.review.brief().line, "Review: stuck after 3 rounds, yours to decide");
        b.decided(&k, "d000000000001");
        // While that decision is open, another round raises nothing and notes nobody.
        let rep = b.report(&k, 1, "still", "t9", true, 70).unwrap();
        assert_eq!((rep.review.status, rep.review.rounds, rep.note_owner, rep.raise), (Status::Stuck, 4, false, false));
        // Once the user answered, a round notes the owner again; still no second decision.
        let rep = b.report(&k, 1, "still", "t9", false, 80).unwrap();
        assert_eq!((rep.note_owner, rep.raise), (true, false));
        // Agreed: over, and nothing more is reported.
        let a = b.agree(&k, "approved", "t9", 90).unwrap();
        assert_eq!((a.status, a.critical, a.ended_ms, a.brief().line.as_str()), (Status::Agreed, 0, Some(90), "Review: agreed"));
        assert!(b.report(&k, 1, "x", "t9", false, 100).is_err());
        assert!(b.agree(&k, "", "t9", 100).is_err());
        assert_eq!(b.end(&k, "the PR was merged", 100), None, "agreed stays agreed");
        let whats: Vec<&str> = b.get(&k).unwrap().steps.iter().map(|s| s.what.as_str()).collect();
        assert_eq!(whats, ["opened", "started", "report", "fixed", "report", "report", "stuck", "report", "report", "agreed"]);
    }

    #[test]
    fn none_critical_is_agreed_at_once_and_an_ended_review_says_so() {
        let mut b = Book::default();
        let k = begun(&mut b, 12);
        let rep = b.report(&k, 0, "nothing critical", "t9", false, 30).unwrap();
        assert_eq!((rep.review.status, rep.review.rounds, rep.note_owner, rep.raise), (Status::Reviewing, 0, false, false));
        assert_eq!(b.agree(&k, "", "t9", 31).unwrap().line, "agreed");
        // Another PR: it ends when its PR closes, with what was left.
        let k2 = begun(&mut b, 13);
        b.report(&k2, 2, "two", "t9", false, 40).unwrap();
        let e = b.end(&k2, "the PR was closed", 50).unwrap();
        assert_eq!((e.status, e.line.as_str(), e.brief().line.as_str()), (Status::Ended, "the PR was closed", "Review: ended, 2 critical left"));
        assert_eq!(b.end(&k2, "again", 60), None, "once");
        assert!(b.fixed(&k2, "x", "t3", 60).is_err());
        // The state carries both (over today), the newest first.
        let v = b.views(60);
        assert_eq!((v.len(), v[0]["key"].as_str(), v[0]["chip"].as_str()), (2, Some("acme/shop#13"), Some("Review: ended, 2 critical left")));
        assert_eq!(b.views(60 + 25 * 3_600_000).len(), 0, "a day later: only the record");
        // Kept across restarts.
        let dir = std::env::temp_dir().join(format!("tower-reviews-{}", std::process::id()));
        let path = dir.join("reviews.json");
        b.save_to(&path).unwrap();
        assert_eq!(Book::load_from(&path), b);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_reviewer_opens_once_per_pr_only_where_review_is_on_and_never_for_a_reviewers_pr() {
        let mut b = Book::default();
        let p = pr(12, "t3", Some("fix-login"));
        assert_eq!(b.wants_reviewer(&p, true, false), Ok(()));
        assert_eq!(b.wants_reviewer(&p, false, false), Err("review is off for its space".into()));
        assert!(b.wants_reviewer(&pr(12, "t3", None), true, false).is_err(), "no space");
        assert!(b.wants_reviewer(&Pr { state: "merged".into(), ..p.clone() }, true, false).is_err());
        // Its terminal says it's a reviewer.
        assert_eq!(b.wants_reviewer(&p, true, true), Err("a reviewer opened it".into()));
        let k = begun(&mut b, 12);
        // Not twice for one PR, even once it's over.
        assert_eq!(b.wants_reviewer(&p, true, false), Err("it has a review already".into()));
        b.agree(&k, "", "t9", 30).unwrap();
        assert_eq!(b.wants_reviewer(&p, true, false), Err("it has a review already".into()));
        // A PR the reviewer (t9) itself made: none, while its review runs.
        let k2 = begun(&mut b, 13);
        assert_eq!(b.wants_reviewer(&pr(40, "t9", Some("fix-login")), true, false), Err("a reviewer opened it".into()));
        b.end(&k2, "the space closed", 40);
        // Over: the address may be another terminal's now; the same terminal still counts.
        assert!(!b.is_reviewer("t9", None) && !b.is_reviewer("t9", Some(7)) && b.is_reviewer("t9", Some(500)));
        assert_eq!(b.wants_reviewer(&pr(40, "t9", Some("fix-login")), true, false), Ok(()));
    }

    #[test]
    fn a_pr_is_named_by_its_key_its_link_or_its_number() {
        assert_eq!(pr_of("acme/shop#12"), Some((Some("acme/shop".into()), 12)));
        assert_eq!(pr_of("https://github.com/acme/shop/pull/12/files"), Some((Some("acme/shop".into()), 12)));
        assert_eq!(pr_of("#12"), Some((None, 12)));
        assert_eq!(pr_of("12"), Some((None, 12)));
        assert_eq!(pr_of("shop"), None);
        let mut b = Book::default();
        begun(&mut b, 12);
        assert_eq!(b.find("#12", None).unwrap().key, "acme/shop#12");
        assert_eq!(b.find("https://github.com/acme/shop/pull/12", Some("t9")).unwrap().key, "acme/shop#12");
        assert!(b.find("other/repo#12", None).is_err());
        assert!(b.find("#99", None).is_err());
        // Two open PRs #12: the caller's own, else its key is needed.
        let mut other = pr(12, "t4", Some("cart"));
        other.repo = "acme/api".into();
        let r = b.begin(&other, None, None, 50);
        b.placed(&r.key, "cart-review-api-12", "/x", "");
        b.started(&r.key, "t10", None, 51);
        assert_eq!(b.find("#12", Some("t10")).unwrap().key, "acme/api#12");
        assert_eq!(b.find("12", Some("t4")).unwrap().key, "acme/api#12");
        assert!(b.find("#12", None).is_err());
    }

    #[test]
    fn the_reviewer_is_named_for_its_space_repo_and_pr_and_never_like_a_tower() {
        assert_eq!(reviewer_name("fix-login", "acme/shop", 12, &[]), "fix-login-review-shop-12");
        assert_eq!(reviewer_name("B2C Support", "eyzhub/eyz-mediaroom-serverless", 1250, &[]), "b2c-support-review-eyz-mediaroom-serverless-1250");
        // Too long: the space gives way, the PR's number stays.
        let n = reviewer_name("support-translation-of-messages", "eyzhub/eyz-mediaroom-serverless", 1250, &[]);
        assert!(n.len() <= NAME_CHARS && n.ends_with("-review-eyz-mediaroom-serverless-1250"), "{n}");
        // A space called tower-…: never a brain's name.
        let n = reviewer_name("tower-pr", "acme/shop", 7, &[]);
        assert_eq!(n, "review-shop-7-tower-pr");
        assert!(!crate::brain::is_brain_name(&n));
        // Taken: numbered.
        assert_eq!(reviewer_name("fix-login", "acme/shop", 12, &["fix-login-review-shop-12".into()]), "fix-login-review-shop-12-2");
        let place = checkout_place(Path::new("/w/eyz/shop"), "Fix Login", 12);
        assert_eq!(place, PathBuf::from("/w/eyz/.tower-worktrees/fix-login/review-shop-12"));
        // Never taken for a space's own worktree of a repo by `tower dispatch`.
        assert_eq!(crate::worktrees::repo_of(&place).map(|(r, _)| r), Some(PathBuf::from("/w/eyz/review-shop-12")));
    }

    #[test]
    fn the_reviewer_looks_for_critical_issues_only_and_knows_its_commands() {
        let mut b = Book::default();
        let k = begun(&mut b, 12);
        let r = b.get(&k).unwrap().clone();
        let t = instructions(&r, "Fix the login redirect", "users land on their dashboard", true);
        for want in [
            "You are the reviewer of PR #12 in acme/shop \"Cart totals\", fix-login/shop into dev: https://github.com/acme/shop/pull/12",
            "its owner: shop (t3), in the space \"fix-login\"",
            "The space's goal: \"Fix the login redirect\". It is done when: users land on their dashboard.",
            "/w/.tower-worktrees/fix-login/review-shop-12, detached",
            "CRITICAL issues only",
            "wrong behaviour against the ticket",
            "a crash",
            "security",
            "data loss or corruption",
            "a build or test failure the PR introduces",
            "an API or contract break with another repo",
            "no style, naming, nits",
            "opening with \"Critical:\"",
            "gh api repos/acme/shop/pulls/12/comments -f body='Critical: …' -f commit_id=abc123",
            "tower queue t3 \"<the critical issues, each with its link>\"",
            "The tower doesn't pass your messages on",
            "tower review report acme/shop#12 --critical <n> \"<one line>\"",
            "None critical: approve it at once (gh pr review 12 --repo acme/shop --approve",
            "tower review agree acme/shop#12",
            "gh pr checkout 12 --repo acme/shop --detach",
            "After 3 rounds without agreement",
            "--comment --body \"Approved: …\"",
            "Never merge, close or push; never edit, commit",
        ] {
            assert!(t.contains(want), "missing {want:?} in:\n{t}");
        }
        // No checkout: it reads the PR with gh.
        assert!(instructions(&r, "", "", false).contains("read the PR with gh pr diff 12 --repo acme/shop"));
        // It may comment, never push, merge or edit.
        let s = reviewer_settings();
        let deny: Vec<&str> = s["permissions"]["deny"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert!(deny.contains(&"Bash(git push:*)") && deny.contains(&"Bash(gh pr merge:*)") && deny.contains(&"Edit"));
    }

    #[test]
    fn the_owners_note_says_who_and_what_to_do_and_nothing_of_the_comments() {
        let mut b = Book::default();
        let k = begun(&mut b, 12);
        let rep = b.report(&k, 2, "SQL injection in the search; a crash on empty carts", "t9", false, 30).unwrap();
        let note = owner_note(&rep.review);
        assert_eq!(
            note,
            "[tower] The reviewer (fix-login-review-shop-12 t9) left 2 critical comments on PR #12 (acme/shop): fix them, push, answer each thread, then tell the reviewer (tower queue t9 \"…\"). If you like, tell the tower too: tower review fixed acme/shop#12 \"<one line>\"."
        );
        assert!(!note.contains("SQL") && !note.contains("crash"), "the content is the reviewer's to send");
        let one = owner_note(&Review { critical: 1, ..rep.review.clone() });
        assert!(one.contains("left 1 critical comment on PR #12 (acme/shop): fix it, push"));
    }

    #[test]
    fn a_stuck_review_is_one_decision_whose_answers_reach_both() {
        let mut b = Book::default();
        let k = begun(&mut b, 12);
        for at in [30, 40, 50] {
            b.report(&k, 1, "totals still lose cents", "t9", false, at).unwrap();
        }
        b.fixed(&k, "they don't: cents are rounded on purpose", "t3", 55).unwrap();
        let (title, detail, options) = stuck_decision(b.get(&k).unwrap());
        assert_eq!(title, "The reviewer and the owner of PR #12 (acme/shop) don't agree on: totals still lose cents");
        assert!(detail.starts_with("https://github.com/acme/shop/pull/12\nRound 3: 1 critical."), "{detail}");
        assert!(detail.contains("The owner said: they don't"));
        let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Side with the owner", "Side with the reviewer", "I'll look"]);
        assert!(options.iter().all(|o| !o.pick && o.text.starts_with("About the review of PR #12 (acme/shop): the user")));
        assert!(options[0].text.contains("tower review agree acme/shop#12"));
    }

    #[test]
    fn the_chip_and_the_merge_card_say_where_the_review_stands() {
        let b = |status: &str, rounds: u32, critical: u32| Brief { status: status.into(), rounds, critical, ..Default::default() };
        let cases = [
            (b("reviewing", 0, 0), "Reviewing…", Some("The review isn't done: its reviewer is still reading it.")),
            (b("changes", 2, 2), "Review: 2 critical, round 2", Some("The review isn't agreed: 2 critical issues open, round 2.")),
            (b("changes", 1, 1), "Review: 1 critical, round 1", Some("The review isn't agreed: 1 critical issue open, round 1.")),
            (b("reviewing", 1, 2), "Reviewing the fixes… (after round 1)", Some("The review isn't agreed: the reviewer checks the fixes (after round 1).")),
            (b("stuck", 3, 1), "Review: stuck after 3 rounds, yours to decide", Some("The review isn't agreed: the reviewer and the owner disagree after 3 rounds.")),
            (b("agreed", 2, 0), "Review: agreed", None),
            (b("ended", 1, 0), "Review: ended without agreement", Some("Its review ended without agreement.")),
        ];
        for (brief, line, warning) in cases {
            assert_eq!(chip_line(&brief), line);
            assert_eq!(merge_warning(&brief).as_deref(), warning, "{line}");
        }
    }

    #[test]
    fn the_store_keeps_every_open_review_and_the_newest_over() {
        let mut b = Book::default();
        for n in 0..(KEEP_OVER as u64 + 5) {
            let r = b.begin(&pr(n, "t3", Some("s")), None, None, n);
            b.end(&r.key, "closed", n);
        }
        let open = b.begin(&pr(999, "t3", Some("s")), None, None, 1);
        b.begin(&pr(1000, "t3", Some("s")), None, None, 2);
        assert_eq!(b.reviews.iter().filter(|r| r.status.over()).count(), KEEP_OVER);
        assert!(b.get(&open.key).is_some() && b.get("acme/shop#0").is_none() && b.get(&format!("acme/shop#{}", KEEP_OVER + 4)).is_some());
    }
}
