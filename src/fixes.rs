//! A PR that conflicts with its base, or fell behind it (the repo wants
//! branches up to date), is its session's to fix, not the user's to look at.
//!
//! - **Its session is told**, through the queue, once for each new conflict
//!   (`ask_text`): bring the branch up to date with its base, resolve the
//!   conflicts keeping what both sides meant, run its checks again, push.
//!   A session that is gone: another of the space in the PR's repo, else one
//!   started there (the daemon's `fixes`).
//! - **Only while it is true, once per head**: GitHub's "unknown" (still
//!   working out whether it merges) is no conflict. A notice still waiting
//!   in the session's queue when the PR merges cleanly again is taken back,
//!   and a push made before the session read it (another session's, or its
//!   own fix) is no failed try: the waiting notice stands for the new head
//!   too, and nothing more is sent. A head is asked about once.
//! - **Meanwhile it is not the user's**: in no merge decision
//!   (`Pr::ready_for_user`), its chip says who is fixing it (`line`). Once
//!   GitHub says it merges cleanly again, it comes to the user as any PR
//!   that waits for a merge.
//! - **Back to the user, once**: the session says it can't (`tower pr
//!   cant-fix`), or two tries didn't fix it (it pushed and it still
//!   conflicts), or it pushed nothing within `FIX_MS` of reading it; or
//!   nobody could be asked. Then it is on its merge card, with why.
//!
//! A PR the user's "Merge all N" is merging is the chain's (`crate::merges`,
//! which asks its session itself): left alone here.

use serde::{Deserialize, Serialize};

use crate::prs::Pr;

/// A try with nothing pushed counts as failed after this long.
pub const FIX_MS: u64 = 60 * 60_000;
/// Tries before it is the user's.
pub const TRIES: u32 = 2;

/// Its session asked to fix it, and how that goes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fix {
    /// `conflicts` or `behind`.
    pub what: String,
    /// The session asked (its address) and its name.
    pub to: String,
    pub to_name: String,
    /// Its head commit when asked: a new one is its push.
    pub head_asked: String,
    /// When it was asked (when the notice reached the session, once it waited in its queue).
    pub asked_ms: u64,
    pub tries: u32,
    /// It came back to the user (why): it is theirs now, and isn't asked again for this conflict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back: Option<String>,
    /// The notice's id in the session's queue while it waits there, unread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued: Option<u64>,
}

/// What keeps it from merging that its session fixes: `conflicts` or `behind`.
pub fn trouble(pr: &Pr) -> Option<&'static str> {
    // GitHub still working it out (after a push, or a change of its base) is no conflict yet.
    if !pr.open() || pr.draft || unknown(pr) {
        return None;
    }
    if pr.mergeable == "conflicting" || pr.merge_state == "dirty" {
        Some("conflicts")
    } else if pr.merge_state == "behind" {
        Some("behind")
    } else {
        None
    }
}

/// GitHub is still working out whether it merges cleanly (right after a push, or a change of its base).
fn unknown(pr: &Pr) -> bool {
    pr.mergeable == "unknown" || pr.merge_state == "unknown"
}

/// Its session is on it (asked, not back with the user).
fn fixing(pr: &Pr) -> bool {
    pr.fix.as_ref().is_some_and(|f| f.back.is_none())
}

/// Whether its merge is the user's now: nothing to fix, or the fix came
/// back to them. One its session is fixing stays out until GitHub says it
/// merges cleanly (not while GitHub works its push out).
pub fn with_user(pr: &Pr) -> bool {
    if pr.fix.as_ref().is_some_and(|f| f.back.is_some()) {
        return true;
    }
    trouble(pr).is_none() && !(fixing(pr) && pr.open() && !pr.draft && unknown(pr))
}

/// What to do about it now.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    Nothing,
    /// Ask its session (this many tries now).
    Ask { what: &'static str, tries: u32 },
    /// It merges cleanly again: the fix is done (and a notice still queued is taken back).
    Clear,
    /// A new head that still conflicts, pushed before its session read the
    /// notice: the notice stands for it (no failed try, nothing sent).
    Follow,
    /// It is the user's (why), once.
    Back(String),
}

/// What to do about `pr` at `now`. `unread`: the notice still waits in its session's queue.
pub fn step(pr: &Pr, now: u64, unread: bool) -> Act {
    let trouble = trouble(pr);
    let Some(f) = &pr.fix else {
        return match trouble {
            Some(what) => Act::Ask { what, tries: 1 },
            None => Act::Nothing,
        };
    };
    let Some(what) = trouble else {
        // Pushed, and GitHub hasn't worked it out again yet: wait for it.
        if pr.open() && !pr.draft && unknown(pr) && f.back.is_none() {
            return Act::Nothing;
        }
        // Fixed (or closed, merged, back to a draft): done with; a new conflict is asked anew.
        return Act::Clear;
    };
    if f.back.is_some() {
        return Act::Nothing;
    }
    let pushed = pr.head_sha != f.head_asked;
    if unread {
        // Nothing its session did failed yet: it hasn't read the notice.
        return if pushed { Act::Follow } else { Act::Nothing };
    }
    if pushed {
        let why = match what {
            "conflicts" => format!("it still conflicts with {} after its session's update", pr.base),
            _ => format!("it is still behind {} after its session's update", pr.base),
        };
        return if f.tries >= TRIES { Act::Back(why) } else { Act::Ask { what, tries: f.tries + 1 } };
    }
    // Once per head: nothing pushed in time is no reason to say it again, it is the user's.
    if now.saturating_sub(f.asked_ms) >= FIX_MS {
        return Act::Back(format!("its session didn't push a fix within {} minutes", FIX_MS / 60_000));
    }
    Act::Nothing
}

/// What its session is told (`tries`: this is the second time).
pub fn ask_text(pr: &Pr, what: &str, tries: u32, key: &str) -> String {
    let base = if pr.base.is_empty() { "its base" } else { &pr.base };
    let why = match what {
        "conflicts" => format!("conflicts with {base}"),
        _ => format!("is behind {base}, and the repo wants branches up to date before they merge"),
    };
    let again = if tries > 1 { " Your last update didn't fix it: GitHub still says so." } else { "" };
    let resolve = if what == "conflicts" { ", resolve the conflicts keeping what both sides meant (yours and what landed on the base)" } else { "" };
    format!(
        "Your PR #{n} ({repo}) {why}, so it can't be merged.{again} Bring the branch up to date: fetch and merge origin/{base} into it{resolve}, \
         run its checks and tests again, and push. Don't merge the PR yourself: once GitHub sees it merging cleanly, it goes to the user to merge. \
         If you can't resolve it, say so and why with `tower pr cant-fix {key} \"<why>\"`, and end your turn.",
        n = pr.number,
        repo = pr.repo,
    )
}

/// Its chip's line while a session fixes it, or once that came back: None otherwise.
pub fn line(pr: &Pr) -> Option<String> {
    let f = pr.fix.as_ref()?;
    trouble(pr)?;
    let who = if f.to_name.is_empty() { f.to.as_str() } else { f.to_name.as_str() };
    let doing = match f.what.as_str() {
        "conflicts" => format!("resolving its conflicts with {}", pr.base),
        _ => format!("bringing it up to date with {}", pr.base),
    };
    Some(match &f.back {
        Some(why) => format!("Not fixed: {why}. Yours to look at."),
        None => format!("Being fixed by {who}: {doing}."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(mergeable: &str, merge_state: &str, sha: &str) -> Pr {
        Pr { repo: "eyzhub/eyzflow".into(), number: 841, base: "dev".into(), state: "open".into(), mergeable: mergeable.into(), merge_state: merge_state.into(), head_sha: sha.into(), session: "t13y".into(), ..Default::default() }
    }

    fn asked(p: Pr, tries: u32, at: u64) -> Pr {
        let fix = Fix { what: "conflicts".into(), to: "t13y".into(), to_name: "eyzflow".into(), head_asked: p.head_sha.clone(), asked_ms: at, tries, ..Default::default() };
        Pr { fix: Some(fix), ..p }
    }

    #[test]
    fn a_conflict_is_its_sessions_to_fix_once_and_the_user_sees_it_only_when_it_merges() {
        let clean = pr("mergeable", "clean", "a1");
        assert_eq!((trouble(&clean), step(&clean, 0, false), with_user(&clean)), (None, Act::Nothing, true));
        // It conflicts: its session is asked; the user isn't shown it.
        let dirty = pr("conflicting", "dirty", "a1");
        assert_eq!((trouble(&dirty), step(&dirty, 0, false), with_user(&dirty)), (Some("conflicts"), Act::Ask { what: "conflicts", tries: 1 }, false));
        assert_eq!(trouble(&pr("mergeable", "behind", "a1")), Some("behind"));
        // Asked: nothing more while it works on it (no second message).
        let p = asked(dirty, 1, 1_000);
        assert_eq!(step(&p, 1_000 + 30 * 60_000, false), Act::Nothing);
        assert_eq!(line(&p).as_deref(), Some("Being fixed by eyzflow: resolving its conflicts with dev."));
        // It pushed; GitHub works it out again: wait.
        let pushed = Pr { head_sha: "a2".into(), mergeable: "unknown".into(), merge_state: "unknown".into(), ..p.clone() };
        assert_eq!((step(&pushed, 2_000, false), with_user(&pushed)), (Act::Nothing, false), "not the user's until GitHub says it merges");
        // Clean now: the fix is done, and it is the user's to merge.
        let fixed = Pr { mergeable: "mergeable".into(), merge_state: "clean".into(), ..pushed };
        assert_eq!((step(&fixed, 3_000, false), with_user(&fixed), line(&fixed)), (Act::Clear, true, None));
        // A new conflict later is asked anew.
        let again = Pr { fix: None, mergeable: "conflicting".into(), ..fixed };
        assert_eq!(step(&again, 4_000, false), Act::Ask { what: "conflicts", tries: 1 });
        // A draft or a closed PR is nobody's to fix.
        assert_eq!(trouble(&Pr { draft: true, ..pr("conflicting", "dirty", "a1") }), None);
        assert_eq!(trouble(&Pr { state: "closed".into(), ..pr("conflicting", "dirty", "a1") }), None);
    }

    #[test]
    fn two_failed_tries_bring_it_to_the_user_once() {
        // Pushed, and still conflicting: a second try.
        let first = asked(pr("conflicting", "dirty", "a1"), 1, 0);
        let still = Pr { head_sha: "a2".into(), ..first.clone() };
        assert_eq!(step(&still, 5 * 60_000, false), Act::Ask { what: "conflicts", tries: 2 });
        // The second fails too: the user's, with why.
        let second = asked(still, 2, 5 * 60_000);
        let again = Pr { head_sha: "a3".into(), ..second.clone() };
        assert_eq!(step(&again, 10 * 60_000, false), Act::Back("it still conflicts with dev after its session's update".into()));
        // Nothing pushed within the hour: the user's (the same head is never asked about twice).
        assert_eq!(step(&first, FIX_MS - 1, false), Act::Nothing);
        assert_eq!(step(&first, FIX_MS, false), Act::Back("its session didn't push a fix within 60 minutes".into()));
        assert_eq!(step(&second, 5 * 60_000 + FIX_MS, false), Act::Back("its session didn't push a fix within 60 minutes".into()));
        // Back with the user: on their queue, said so, never asked again for it.
        let mut back = again;
        back.fix.as_mut().unwrap().back = Some("it can't keep both: the column was removed on dev".into());
        assert_eq!((step(&back, 20 * 60_000, false), with_user(&back)), (Act::Nothing, true));
        assert_eq!(line(&back).as_deref(), Some("Not fixed: it can't keep both: the column was removed on dev. Yours to look at."));
    }

    #[test]
    fn githubs_unknown_is_no_conflict() {
        // Right after a push or a change of its base GitHub says UNKNOWN, with whatever state it had: nothing to ask.
        for state in ["unknown", "dirty", "blocked"] {
            let p = pr("unknown", state, "a2");
            assert_eq!((trouble(&p), step(&p, 0, false), line(&p)), (None, Act::Nothing, None), "{state}");
        }
        // Asked about a1, a new head still unknown: wait, neither a failed try nor done.
        let p = Pr { head_sha: "a2".into(), mergeable: "unknown".into(), merge_state: "dirty".into(), ..asked(pr("conflicting", "dirty", "a1"), 1, 0) };
        assert_eq!((step(&p, 60_000, false), with_user(&p)), (Act::Nothing, false));
    }

    #[test]
    fn a_notice_unread_in_the_queue_is_no_try_and_is_taken_back_once_resolved() {
        // eyzflow #884: asked at a1 while its session was busy; another session pushes a2, still conflicting.
        let first = asked(pr("conflicting", "dirty", "a1"), 1, 0);
        let moved = Pr { head_sha: "a2".into(), ..first.clone() };
        assert_eq!(step(&moved, 60_000, true), Act::Follow, "no second notice for a push the session never asked for");
        assert_eq!(step(&first, 2 * FIX_MS, true), Act::Nothing, "no hour runs out while it waits unread");
        // Then the conflict is resolved on a new head before the notice is read: taken back, nothing sent.
        let resolved = Pr { head_sha: "448ee669".into(), mergeable: "mergeable".into(), merge_state: "clean".into(), ..first };
        assert_eq!(step(&resolved, 120_000, true), Act::Clear);
        assert_eq!(step(&Pr { fix: None, ..resolved }, 130_000, false), Act::Nothing);
    }

    #[test]
    fn its_session_is_told_what_to_do_and_how_to_say_it_cant() {
        let t = ask_text(&pr("conflicting", "dirty", "a1"), "conflicts", 1, "eyzhub/eyzflow#841");
        assert!(t.starts_with("Your PR #841 (eyzhub/eyzflow) conflicts with dev, so it can't be merged. Bring the branch up to date: fetch and merge origin/dev into it, resolve the conflicts keeping what both sides meant"), "{t}");
        assert!(t.contains("run its checks and tests again, and push") && t.contains("`tower pr cant-fix eyzhub/eyzflow#841 \"<why>\"`"), "{t}");
        let t2 = ask_text(&pr("mergeable", "behind", "a1"), "behind", 2, "k");
        assert!(t2.contains("is behind dev, and the repo wants branches up to date") && t2.contains("Your last update didn't fix it") && !t2.contains("resolve the conflicts"), "{t2}");
    }
}
