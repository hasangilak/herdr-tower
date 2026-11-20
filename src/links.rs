//! Which Claude account each Artifact link is of, so the tower opens it in
//! that account's browser (`accounts::open_link`): an Artifact is private
//! to the account that published it, and the other account's browser isn't
//! signed in to it.
//!
//! - **Noted** by the job watch (`daemon::running`) as it reads a session's
//!   transcript on: a tool result that says "Published … at
//!   https://claude.ai/artifact/…" (the Artifact tool's) is of the account
//!   the session runs on (`accounts::of_pid`). The first account noted for
//!   a link keeps it.
//! - **Kept** in `links.json` in the state folder, the newest `KEPT`.
//! - **Asked** with `account_of`: the link's account when it was noted,
//!   else the account of the session it came with (its terminal's), else
//!   None (`accounts::browser_for` then opens it as the default account's).

use serde::{Deserialize, Serialize};

use crate::config;

/// How many links are kept.
pub const KEPT: usize = 400;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// The link's artifact id (`key`).
    pub key: String,
    pub url: String,
    pub account: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Book {
    pub links: Vec<Link>,
}

/// What names an Artifact, in either form of its link: the id after
/// `/artifact/` in `claude.ai/artifact/<id>` or `claude.ai/code/artifact/<id>`,
/// without its query or fragment. None: not an Artifact link.
pub fn key(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("https://").or_else(|| url.trim().strip_prefix("http://"))?;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("claude.ai/")?;
    let id = path.strip_prefix("code/").unwrap_or(path).strip_prefix("artifact/")?;
    let id: String = id.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    (!id.is_empty()).then_some(id)
}

/// The Artifact links a piece of a transcript (JSON lines) says were
/// published: in a tool result whose words begin "Published " (or
/// "Created ", an Artifact made from a type), the first claude.ai Artifact
/// link in them.
pub fn published_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for line in text.lines().filter(|l| l.contains("\"tool_result\"")) {
        for needle in ["\"content\":\"Published ", "\"text\":\"Published ", "\"content\":\"Created ", "\"text\":\"Created "] {
            let mut from = 0;
            while let Some(i) = line[from..].find(needle) {
                let start = from + i + needle.len();
                from = start;
                // The JSON string's words: up to its closing quote (not an escaped one).
                let s = &line[start..];
                let mut end = s.len();
                let mut escaped = false;
                for (j, c) in s.char_indices() {
                    match c {
                        '\\' if !escaped => escaped = true,
                        '"' if !escaped => {
                            end = j;
                            break;
                        }
                        _ => escaped = false,
                    }
                }
                let words = &s[..end];
                let Some(at) = words.find("https://claude.ai/") else { continue };
                let url: String = words[at..].chars().take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\\' | ')' | ',' | '`' | '>' | '<')).collect();
                let url = url.trim_end_matches(['.', ';', ':', '!', '?']).to_string();
                if key(&url).is_some() && !out.contains(&url) {
                    out.push(url);
                }
            }
        }
    }
    out
}

impl Book {
    /// `url` is of `account` (a link already noted keeps its account). True when it was new.
    pub fn note(&mut self, url: &str, account: &str, at_ms: u64) -> bool {
        let Some(k) = key(url) else { return false };
        if account.trim().is_empty() || self.links.iter().any(|l| l.key == k) {
            return false;
        }
        self.links.push(Link { key: k, url: url.trim().to_string(), account: account.trim().to_string(), at_ms });
        if self.links.len() > KEPT {
            self.links.sort_by_key(|l| l.at_ms);
            let n = self.links.len() - KEPT;
            self.links.drain(..n);
        }
        true
    }

    /// The account `url` was published from, when it was noted.
    pub fn owner(&self, url: &str) -> Option<&str> {
        let k = key(url)?;
        self.links.iter().find(|l| l.key == k).map(|l| l.account.as_str())
    }
}

pub fn path() -> std::path::PathBuf {
    // Tests keep theirs in a temporary folder, never in the user's state folder.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("herder-tower-test-{}", std::process::id())).join("links.json");
    }
    config::state_dir().join("links.json")
}

pub fn load() -> Book {
    std::fs::read_to_string(path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Notes each of `urls` as `account`'s, saved at once when one was new.
pub fn note_all(urls: &[String], account: &str, at_ms: u64) {
    if urls.is_empty() {
        return;
    }
    let mut b = load();
    let mut new = false;
    for u in urls {
        new |= b.note(u, account, at_ms);
    }
    if new {
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_string(&b).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// The account a link opens as: the one it was published from (`book`),
/// else the account the session it came with runs on (`session`'s, found
/// by `session_account`), else None.
pub fn account_of(url: &str, book: &Book, session: Option<&str>, session_account: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    book.owner(url).map(String::from).or_else(|| session.filter(|s| !s.trim().is_empty()).and_then(session_account))
}

/// `account_of` with the book as saved and a session's account asked of its terminal.
pub fn account_now(url: &str, session: Option<&str>) -> Option<String> {
    account_of(url, &load(), session, &crate::accounts::of_address)
}

/// The web links in some words, in order, each once (what a message's link chips open).
pub fn in_words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for (i, _) in text.match_indices("http") {
        let s = &text[i..];
        if !(s.starts_with("https://") || s.starts_with("http://")) {
            continue;
        }
        // (Not inside a word: "xhttps://" is not a link's start.)
        if text[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let url: String = s.chars().take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '<' | '>' | '`' | '|')).collect();
        // Trailing punctuation belongs to the sentence; a ")" only when the link has no "(".
        let mut url = url.trim_end_matches(['.', ',', ';', ':', '!', '?']).to_string();
        while url.ends_with(')') && url.matches('(').count() < url.matches(')').count() {
            url.pop();
            url = url.trim_end_matches(['.', ',', ';', ':', '!', '?']).to_string();
        }
        let host = url.split("://").nth(1).unwrap_or("").split(['/', '?', '#']).next().unwrap_or("");
        if !host.is_empty() && host.contains('.') && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// A link as a chip names it: its host and the start of its path
/// ("claude.ai/artifact/DYqd…").
pub fn short(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest).trim_end_matches('/');
    if rest.chars().count() <= 30 {
        return rest.to_string();
    }
    format!("{}…", rest.chars().take(29).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_artifact_is_named_by_its_id_in_either_link() {
        assert_eq!(key("https://claude.ai/artifact/DYqdy4uvNXRTWy1sapVmXW").as_deref(), Some("DYqdy4uvNXRTWy1sapVmXW"));
        assert_eq!(key("https://claude.ai/code/artifact/9bea6336-0eba-435c?x=1#top").as_deref(), Some("9bea6336-0eba-435c"));
        assert_eq!(key("https://claude.ai/chat/abc"), None);
        assert_eq!(key("https://example.com/artifact/x"), None);
        assert_eq!(key("claude.ai/artifact/x"), None);
    }

    #[test]
    fn a_published_artifact_is_seen_in_its_tool_result() {
        let result = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"Published /tmp/s/fine-tune.html at https://claude.ai/artifact/DYqdy4uvNXRTWy1sapVmXW (Version 1, version id 1794594520-cd99) Icon: \"book\".\n\nLive subscription: …"}]}}"#;
        let listed = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"Created \"Plan\" at https://claude.ai/code/artifact/0f1e-22.\nIts instructions…"}]}]}}"#;
        // Only a tool result's words saying it was published: not a link the user pasted, nor one a listing or a read showed.
        let said = r#"{"type":"user","message":{"content":"Published at https://claude.ai/artifact/MINE"}}"#;
        let read = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"Read https://claude.ai/artifact/OTHER: …"}]}}"#;
        let text = [result, listed, said, read, result].join("\n");
        assert_eq!(published_in(&text), ["https://claude.ai/artifact/DYqdy4uvNXRTWy1sapVmXW", "https://claude.ai/code/artifact/0f1e-22"], "each once; a full stop after it is no part of it");
    }

    #[test]
    fn a_link_keeps_the_account_it_was_first_noted_with() {
        let mut b = Book::default();
        assert!(b.note("https://claude.ai/artifact/A1", "orca", 1));
        assert!(!b.note("https://claude.ai/code/artifact/A1", "default", 2), "the same artifact");
        assert!(!b.note("https://example.com/x", "orca", 3), "not an artifact");
        assert!(!b.note("https://claude.ai/artifact/B2", " ", 3), "no account");
        assert_eq!(b.owner("https://claude.ai/artifact/A1?v=2"), Some("orca"));
        assert_eq!(b.owner("https://claude.ai/artifact/B2"), None);
        // The newest kept.
        for i in 0..KEPT + 5 {
            b.note(&format!("https://claude.ai/artifact/n{i}"), "default", 10 + i as u64);
        }
        assert_eq!(b.links.len(), KEPT);
        assert_eq!(b.owner("https://claude.ai/artifact/A1"), None, "the oldest went");
        assert!(b.owner(&format!("https://claude.ai/artifact/n{}", KEPT + 4)).is_some());
    }

    #[test]
    fn a_link_opens_as_its_account_else_as_its_sessions() {
        let mut b = Book::default();
        b.note("https://claude.ai/artifact/A1", "orca", 1);
        let of = |s: &str| (s == "t5").then(|| "default".to_string());
        assert_eq!(account_of("https://claude.ai/artifact/A1", &b, Some("t5"), &of).as_deref(), Some("orca"), "its own account wins");
        assert_eq!(account_of("https://claude.ai/artifact/new", &b, Some("t5"), &of).as_deref(), Some("default"), "not noted: its session's");
        assert_eq!(account_of("https://github.com/o/r/pull/1", &b, Some("t9"), &of), None);
        assert_eq!(account_of("https://claude.ai/artifact/new", &b, None, &of), None);
    }

    #[test]
    fn links_in_words() {
        let text = "The plan (https://claude.ai/artifact/x) is ready. See https://github.com/o/r/pull/12, and https://en.wikipedia.org/wiki/Foo_(bar). Also https://claude.ai/artifact/x again; not xhttps://no.pe or https://localhost.";
        assert_eq!(in_words(text), ["https://claude.ai/artifact/x", "https://github.com/o/r/pull/12", "https://en.wikipedia.org/wiki/Foo_(bar)"]);
        assert!(in_words("no links here, only http talk").is_empty());
        assert_eq!(short("https://claude.ai/artifact/DYqdy4uvNXRTWy1sapVmXW?x=1"), "claude.ai/artifact/DYqdy4uvNX…");
        assert_eq!(short("https://www.github.com/o/r/pull/12/"), "github.com/o/r/pull/12");
    }
}
