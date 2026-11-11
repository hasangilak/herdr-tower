//! What a session is called, in one place, so the Mac, the phone and the
//! voice agree.
//!
//! A session is its space and what it works on: `sandbox › mediaroom` where
//! it is written, "the mediaroom session in sandbox" where it is said. What
//! it works on is its own name when it has one (`fix-login-eyzflow` in the
//! space "fix-login" is `eyzflow`: the space isn't said twice), else its
//! project: the repo of its worktree, or the folder it runs in.
//!
//! The name kept for a session (in a decision, a mission, a thread, the
//! history) is the short one, `label`: its space is kept beside it, and
//! `shown` and `said` put the two together.

use serde_json::Value;

use crate::hc;

/// Between a space and a session, where they are written.
pub const SEP: &str = " › ";

/// A name's words: `eyz-mediaroom_serverless` is `eyz mediaroom serverless`.
fn words(name: &str) -> Vec<&str> {
    name.split(|c: char| c == '-' || c == '_' || c.is_whitespace()).filter(|w| !w.is_empty()).collect()
}

/// A brain's name (`tower`, `tower-<space>`), as it is kept or as it is read out.
fn is_brain(name: &str) -> bool {
    let n = name.trim();
    n == "tower" || ((n.starts_with("tower-") || n.starts_with("tower ")) && !n.starts_with("tower-voice") && !n.starts_with("tower voice"))
}

/// A session's name within its space, from whatever was kept for it: its
/// name in Herdr (`fix-login-eyzflow`), the same read out (`fix login
/// eyzflow`), or what an older tower called one with no name (`Claude in eyz
/// mediaroom serverless`). The space's own words in front are dropped, and
/// a brain is `tower`.
pub fn label(space: Option<&str>, name: &str) -> String {
    let name = name.trim();
    if is_brain(name) {
        return "tower".into();
    }
    let name = ["Claude in ", "Codex in ", "the agent in "].iter().find_map(|p| name.strip_prefix(p)).unwrap_or(name);
    let all = words(name);
    let lead = space.map(words).unwrap_or_default();
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let rest = match all.len() > lead.len() && !lead.is_empty() && all.iter().zip(&lead).all(|(a, b)| same(a, b)) {
        true => &all[lead.len()..],
        false => &all[..],
    };
    rest.join(" ")
}

/// `label`, of what is known to be a session, whatever it is called. One
/// somebody called `tower-…` is no tower: its name without that word in
/// front (`tower-pr-chips` is `pr chips tower`; in a space "tower-pr",
/// `tower-pr-app` is `app`), so nothing reads it as one. "" when nothing is
/// left to call it by.
pub fn session_label(space: Option<&str>, name: &str) -> String {
    if !is_brain(name) {
        return label(space, name);
    }
    let all = words(name);
    let lead = space.map(words).unwrap_or_default();
    let own: Vec<&str> = match all.len() > lead.len() && !lead.is_empty() && all.iter().zip(&lead).all(|(a, b)| a.eq_ignore_ascii_case(b)) {
        true => all[lead.len()..].to_vec(),
        false => all.iter().skip(1).copied().chain(all.first().copied()).collect(),
    };
    Some(own.join(" ")).filter(|n| !is_brain(n)).unwrap_or_default()
}

/// A session where it is written: `sandbox › mediaroom`, or `mediaroom`
/// when it is in no space.
pub fn shown(space: Option<&str>, name: &str) -> String {
    let short = label(space, name);
    match space.map(str::trim).filter(|s| !s.is_empty()) {
        Some(space) if short.is_empty() => space.to_string(),
        Some(space) => format!("{space}{SEP}{short}"),
        None => short,
    }
}

/// A session where it is said: "the mediaroom session in sandbox". A
/// space's own brain is "the sandbox tower", the main one "the tower".
pub fn said(space: Option<&str>, name: &str) -> String {
    let short = label(space, name);
    let space = space.map(|s| words(s).join(" ")).filter(|s| !s.is_empty());
    if is_brain(name) {
        return match space {
            Some(space) => format!("the {space} tower"),
            None => "the tower".into(),
        };
    }
    // "the orb session", not "the orb session session".
    let who = match short.split(' ').next_back() {
        _ if short.is_empty() => "a session".to_string(),
        Some(last) if last.eq_ignore_ascii_case("session") => format!("the {short}"),
        _ => format!("the {short} session"),
    };
    match space {
        Some(space) => format!("{who} in {space}"),
        None => who,
    }
}

/// A notice that begins with a session's name ("mediaroom asks: …"), as it
/// is written and as it is said: `sandbox › mediaroom asks: …` and "The
/// mediaroom session in sandbox asks: …". One that begins otherwise is both
/// as it is.
pub fn notice(text: &str, space: Option<&str>, name: &str) -> (String, String) {
    let rest = match text.strip_prefix(name) {
        Some(rest) if !name.trim().is_empty() && rest.chars().next().is_none_or(|c| !c.is_alphanumeric()) => rest,
        _ => return (text.to_string(), text.to_string()),
    };
    let spoken = said(space, name);
    let mut c = spoken.chars();
    let spoken: String = c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default();
    (format!("{}{rest}", shown(space, name)), format!("{spoken}{rest}"))
}

/// The space a terminal is in, from its labels (`hc get`) or its row (`hc ls`).
fn space_of(t: &Value) -> Option<String> {
    let place = t["labels"]["where"].as_str().or_else(|| t["where"].as_str())?;
    place.split(" / ").next().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// The project a session with no name works on: the repo of the worktree
/// the tower gave it, else the folder it runs in. A folder that begins with
/// the name of the folder it is in loses that (`eyz/eyz-mediaroom-serverless`
/// is `mediaroom serverless`).
pub fn project(t: &Value) -> Option<String> {
    let folder = t["labels"][crate::worktrees::LABEL_REPO]
        .as_str()
        .filter(|r| !r.is_empty())
        .or_else(|| t["foreground"]["cwd"].as_str())
        .or_else(|| t["cwd"].as_str())
        .filter(|c| !c.is_empty())?;
    let mut parts = folder.trim_end_matches('/').rsplit('/').filter(|p| !p.is_empty());
    let (last, parent) = (parts.next()?, parts.next().unwrap_or(""));
    let all = words(last);
    let own = match all.first() {
        Some(first) if all.len() > 1 && first.eq_ignore_ascii_case(parent) => &all[1..],
        _ => &all[..],
    };
    Some(own.join(" ")).filter(|p| !p.is_empty())
}

/// What a terminal's session is called within its space: its own name, else
/// its project, else its address.
pub fn short_of(t: &Value) -> String {
    let space = space_of(t);
    match hc::name(t) {
        // A brain keeps its name (`tower sandbox`): `shown` and `said` say whose tower it is.
        Some(name) if crate::brain::brain_of(t).is_some() => words(name).join(" "),
        Some(name) => Some(session_label(space.as_deref(), name))
            .filter(|n| !n.is_empty())
            .or_else(|| project(t))
            .or_else(|| t["address"].as_str().map(String::from))
            .unwrap_or_else(|| "a session".into()),
        None => project(t).or_else(|| t["address"].as_str().map(String::from)).unwrap_or_else(|| "a terminal".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_session_is_its_space_and_what_it_works_on() {
        // One with a name of its own, in a space.
        let named = json!({ "address": "t4", "name": "fix-the-eyzflow", "labels": { "where": "sandbox / fix-the-eyzflow" }, "cwd": "/w/eyz/eyzflow" });
        assert_eq!(short_of(&named), "fix the eyzflow");
        assert_eq!(shown(Some("sandbox"), &short_of(&named)), "sandbox › fix the eyzflow");
        assert_eq!(said(Some("sandbox"), &short_of(&named)), "the fix the eyzflow session in sandbox");
        // One `tower launch` named for its space and repo: the space isn't said twice.
        let launched = json!({ "address": "t5", "labels": { "where": "fix-login / fix-login-eyzflow", "tower.name": "fix-login-eyzflow" } });
        assert_eq!(short_of(&launched), "eyzflow");
        assert_eq!((shown(Some("fix-login"), "fix-login-eyzflow"), said(Some("fix-login"), "fix login eyzflow")), ("fix-login › eyzflow".to_string(), "the eyzflow session in fix login".to_string()));
        // One with no name, in a space: its project, where an older tower said "Claude in eyz mediaroom serverless".
        let unnamed = json!({
            "address": "t6", "labels": { "where": "sandbox / Claude Code" }, "status": { "app": "claude-code", "state": "idle" },
            "foreground": { "cwd": "/Users/me/Work/eyz/eyz-mediaroom-serverless" },
        });
        assert_eq!(short_of(&unnamed), "mediaroom serverless");
        assert_eq!(shown(Some("sandbox"), &short_of(&unnamed)), "sandbox › mediaroom serverless");
        assert_eq!(said(Some("sandbox"), &short_of(&unnamed)), "the mediaroom serverless session in sandbox");
        assert_eq!(shown(Some("sandbox"), "Claude in eyz mediaroom serverless"), "sandbox › eyz mediaroom serverless", "what an older tower kept reads the new way");
        // In the space's worktree: the repo, not the worktree's folder.
        let placed = json!({ "address": "t7", "cwd": "/w/.tower-worktrees/fix-login/wt", "labels": { "where": "fix-login / x", "tower.repo": "/w/eyz/eyzflow" } });
        assert_eq!(short_of(&placed), "eyzflow");
        // One in no space: a row of `hc ls` with a folder only.
        let loose = json!({ "address": "t8", "cwd": "/Users/me/Work/herder-tower", "status": "working" });
        assert_eq!(short_of(&loose), "herder tower");
        assert_eq!((shown(None, "herder tower"), said(None, "herder tower")), ("herder tower".to_string(), "the herder tower session".to_string()));
        // Nothing to go by: its address.
        assert_eq!(short_of(&json!({ "address": "t9" })), "t9");
        // A brain: the main one, and a space's own.
        assert_eq!((label(None, "tower"), shown(None, "tower"), said(None, "tower")), ("tower".to_string(), "tower".to_string(), "the tower".to_string()));
        assert_eq!((shown(Some("sandbox"), "tower-sandbox"), said(Some("sandbox"), "tower sandbox")), ("sandbox › tower".to_string(), "the sandbox tower".to_string()));
        assert_eq!(short_of(&json!({ "address": "t2", "name": "tower-sandbox", "labels": { "tower.space": "sandbox" } })), "tower sandbox", "kept as it is named");
        // A session in a tab that somebody called `tower-…` (its name is a label) is a session, not a tower.
        let called = json!({ "address": "t3", "name": null, "labels": { "where": "M5 Pro / tower-pr-chips", "tower.name": "tower-pr-chips" }, "cwd": "/w/herder-tower" });
        assert_eq!(short_of(&called), "pr chips tower");
        assert_eq!((shown(Some("M5 Pro"), &short_of(&called)), said(Some("M5 Pro"), &short_of(&called))), ("M5 Pro › pr chips tower".to_string(), "the pr chips tower session in M5 Pro".to_string()));
        let in_its_space = json!({ "address": "t4", "labels": { "where": "tower-pr / x", "tower.name": "tower-pr-herder-tower" } });
        assert_eq!(short_of(&in_its_space), "herder tower");
        assert_eq!(short_of(&json!({ "address": "t5", "labels": { "tower.name": "tower" }, "cwd": "/w/eyz/eyzflow" })), "eyzflow");
        // A name that is the space's own name is kept, and "session" isn't said twice.
        assert_eq!(shown(Some("sandbox"), "sandbox"), "sandbox › sandbox");
        assert_eq!(said(Some("M5 Pro"), "orb-session"), "the orb session in M5 Pro");
        assert_eq!(shown(Some("M5 Pro"), "M5-Pro-api"), "M5 Pro › api", "the space's words in any case");
        assert_eq!(shown(Some(" "), "api"), "api");
    }

    #[test]
    fn a_notice_names_its_session_for_the_eye_and_for_the_ear() {
        let (text, speech) = notice("mediaroom asks: Which database? I'd say: Postgres.", Some("sandbox"), "mediaroom");
        assert_eq!(text, "sandbox › mediaroom asks: Which database? I'd say: Postgres.");
        assert_eq!(speech, "The mediaroom session in sandbox asks: Which database? I'd say: Postgres.");
        let (text, speech) = notice("fix login eyzflow waits for you: merge PR #12", Some("fix-login"), "fix login eyzflow");
        assert_eq!((text.as_str(), speech.as_str()), ("fix-login › eyzflow waits for you: merge PR #12", "The eyzflow session in fix login waits for you: merge PR #12"));
        // No space: its name alone.
        assert_eq!(notice("api finished", None, "api"), ("api finished".to_string(), "The api session finished".to_string()));
        // A notice that begins otherwise, or with a longer word, is left as it is.
        assert_eq!(notice("Merge PR #12 into dev?", Some("sandbox"), "api").0, "Merge PR #12 into dev?");
        assert_eq!(notice("apiary needs you", Some("sandbox"), "api").1, "apiary needs you");
        assert_eq!(notice("x", Some("sandbox"), "").0, "x");
    }
}
