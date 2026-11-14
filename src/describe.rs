//! What a PR on a merge card does, in words the user reads at a glance.
//!
//! - **Its description** is what GitHub has for it (`gh pr view --json
//!   body`, `Pr::body`; `None` until the tower has looked). The card shows
//!   it in full on request.
//! - **A summary** of it, one or two plain sentences: what it changes for
//!   the people who use the product, and any risk worth knowing before a
//!   merge (`summarize`: one headless call, as `covered::ask` makes them).
//!   Kept with the PR (`Pr::described`) for the description it was made
//!   from (`hash`): an edited description is summarized again.
//! - **None at all**: the card says so, and the PR's session is asked,
//!   once (`Pr::asked_body`), to write one (`ask_text`). What it writes is
//!   read at the next look and summarized then.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::prs::Pr;

/// A PR's description summarized, and for which text of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Described {
    /// One or two plain sentences: what it changes for users, and any risk.
    pub summary: String,
    /// The description it was made from (`hash`).
    pub of: u64,
}

/// A description's fingerprint (FNV-1a of its trimmed text): whether the
/// summary kept is still the one of the description GitHub has now.
pub fn hash(body: &str) -> u64 {
    body.trim().bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// It has a description whose summary isn't made yet (or is of an older text).
pub fn needs_summary(pr: &Pr) -> bool {
    let Some(body) = pr.body.as_deref().filter(|b| !b.trim().is_empty()) else { return false };
    pr.described.as_ref().is_none_or(|d| d.of != hash(body))
}

/// GitHub says it has no description, and its session wasn't asked yet.
pub fn needs_asking(pr: &Pr) -> bool {
    pr.body.as_deref().is_some_and(|b| b.trim().is_empty()) && !pr.asked_body && !pr.session.is_empty()
}

/// What its session is told when it has no description: write one, on GitHub.
pub fn ask_text(pr: &Pr) -> String {
    format!(
        "Your PR #{n} ({repo}) into {base} has no description, and the user can't tell from its title alone what merging it changes. \
         Write one now and put it on the PR with `gh pr edit {n} --repo {repo} --body-file <file>`: in plain words, what it changes for \
         the people who use the product, why, how you checked it, and any risk or follow-up. Don't change the code for this.",
        n = pr.number,
        repo = pr.repo,
        base = if pr.base.is_empty() { "its base" } else { &pr.base },
    )
}

/// One or two plain sentences of what the PR changes for users, and any
/// risk. Err: the model couldn't be asked, or said nothing usable.
pub fn summarize(pr: &Pr) -> Result<String> {
    parse(&crate::covered::ask(&prompt(pr))?)
}

/// What a merge card says a PR does, first thing under its title.
#[derive(Debug, Clone, PartialEq)]
pub enum About {
    /// Its description in one or two sentences.
    Summary(String),
    /// Its description's opening words, while the summary is being made.
    Opening(String),
    /// GitHub says it has none (`asked`: its session was asked to write one).
    Missing { asked: bool },
    /// The tower hasn't looked yet.
    Unknown,
}

/// What the card says the PR does: its summary, of the description as it is now.
pub fn about(pr: &Pr) -> About {
    match pr.body.as_deref().map(str::trim) {
        None => About::Unknown,
        Some("") => About::Missing { asked: pr.asked_body },
        Some(body) => match pr.described.as_ref().filter(|d| d.of == hash(body) && !d.summary.is_empty()) {
            Some(d) => About::Summary(d.summary.clone()),
            None => About::Opening(opening(body)),
        },
    }
}

/// A description's first paragraph of prose (headings skipped), its markdown marks left out, cut short.
pub fn opening(body: &str) -> String {
    // Headings ("## Summary") aren't prose: the paragraph under one is.
    let para: Vec<String> = body
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(plain)
        .skip_while(|l| l.is_empty())
        .take_while(|l| !l.is_empty())
        .collect();
    clip(&para.join(" "), 220)
}

/// A line of markdown as plain words: no heading, list or quote marks, no emphasis or code ticks.
pub fn plain(line: &str) -> String {
    let t = line.trim().trim_start_matches(['#', '>', ' ']);
    let t = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).unwrap_or(t);
    t.replace("**", "").replace('`', "").trim().to_string()
}

/// What the model is asked.
fn prompt(pr: &Pr) -> String {
    format!(
        "Summarize this pull request for the person who decides whether to merge it. They are not reading the code. In one or two \
         short, plain sentences (40 words at most, no markdown, no lists, no PR number): what merging it changes for the people who use \
         the product, then any risk worth knowing before merging (a migration, a breaking change, something to watch after it ships), \
         only when there is one. Use the description's own facts; don't invent any.\n\n\
         <title>{}</title>\n<into>{}</into>\n<description>{}</description>\n\n\
         Answer with JSON only, no other words: {{\"summary\": \"…\"}}",
        pr.title.trim(),
        pr.base,
        clip(pr.body.as_deref().unwrap_or(""), 6000),
    )
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// The summary of the model's answer, as one line.
fn parse(out: &str) -> Result<String> {
    let v = crate::ticket::parse_reply(out)?;
    let s = v["summary"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() {
        bail!("the answer has no \"summary\"");
    }
    // Two sentences are short: one that ran on is cut, never left half a page long.
    Ok(if s.chars().count() > 320 { format!("{}…", s.chars().take(319).collect::<String>().trim_end()) } else { s })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(body: Option<&str>) -> Pr {
        Pr { repo: "eyzhub/eyzflow".into(), number: 839, base: "dev".into(), title: "Support Studio".into(), session: "t13x".into(), body: body.map(String::from), ..Default::default() }
    }

    #[test]
    fn a_description_is_summarized_once_for_each_text_of_it() {
        let mut p = pr(Some("Adds the Knowledge gaps column.\n\nRisk: none."));
        assert!(needs_summary(&p));
        p.described = Some(Described { summary: "x".into(), of: hash("  Adds the Knowledge gaps column.\n\nRisk: none.\n") });
        assert!(!needs_summary(&p), "the same text, its spaces aside");
        p.body = Some("Adds the Knowledge gaps column. Edited.".into());
        assert!(needs_summary(&p), "edited: summarized again");
        // None looked at yet, or none on GitHub: nothing to summarize.
        assert!(!needs_summary(&pr(None)) && !needs_summary(&pr(Some(" \n"))));
    }

    #[test]
    fn a_pr_without_a_description_has_its_session_asked_once() {
        assert!(needs_asking(&pr(Some(""))));
        assert!(!needs_asking(&pr(None)), "not looked at yet: not known to be empty");
        assert!(!needs_asking(&pr(Some("Has one."))));
        assert!(!needs_asking(&Pr { asked_body: true, ..pr(Some("")) }), "once");
        assert!(!needs_asking(&Pr { session: String::new(), ..pr(Some("")) }), "nobody to ask");
        let t = ask_text(&pr(Some("")));
        assert!(t.contains("PR #839 (eyzhub/eyzflow) into dev has no description") && t.contains("gh pr edit 839 --repo eyzhub/eyzflow --body-file"), "{t}");
    }

    #[test]
    fn the_card_says_what_it_knows_of_the_description() {
        assert_eq!(about(&pr(None)), About::Unknown);
        assert_eq!(about(&pr(Some(""))), About::Missing { asked: false });
        assert_eq!(about(&Pr { asked_body: true, ..pr(Some(" ")) }), About::Missing { asked: true });
        let body = "## Summary\n\n**Support Studio** gets a `Knowledge gaps` column:\n- counts open gaps\n\n## Testing\nran it";
        assert_eq!(about(&pr(Some(body))), About::Opening("Support Studio gets a Knowledge gaps column: counts open gaps".into()), "under its heading");
        assert_eq!(opening("\n**Support Studio** gets a `Knowledge gaps` column:\n- counts open gaps\n\nmore"), "Support Studio gets a Knowledge gaps column: counts open gaps");
        let mut p = pr(Some(body));
        p.described = Some(Described { summary: "Support staff see open gaps.".into(), of: hash(body) });
        assert_eq!(about(&p), About::Summary("Support staff see open gaps.".into()));
        p.body = Some("Edited.".into());
        assert_eq!(about(&p), About::Opening("Edited.".into()), "a summary of an older text isn't shown");
    }

    #[test]
    fn the_models_summary_is_one_short_line() {
        let out = r#"{"type":"result","result":"{\"summary\": \"Support staff see a new\\n Knowledge gaps column.  No risk.\"}"}"#;
        assert_eq!(parse(out).unwrap(), "Support staff see a new Knowledge gaps column. No risk.");
        assert!(parse(r#"{"summary": ""}"#).is_err());
        assert!(parse("It adds a column.").is_err());
        let long = parse(&format!(r#"{{"summary": "{}"}}"#, "word ".repeat(100))).unwrap();
        assert!(long.chars().count() <= 320 && long.ends_with('…'));
        let p = prompt(&pr(Some("Body text")));
        assert!(p.contains("<description>Body text</description>") && p.contains("<into>dev</into>") && p.contains("{\"summary\""));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// One real call on a real PR (`TOWER_LIVE_PR=eyzhub/eyzflow#839 cargo test live_summary -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn live_summary() {
        let key = std::env::var("TOWER_LIVE_PR").unwrap_or_else(|_| "eyzhub/eyzflow#839".into());
        let (repo, n) = key.split_once('#').unwrap();
        let out = std::process::Command::new("gh").args(["pr", "view", n, "--repo", repo, "--json", "title,baseRefName,body"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let p = Pr {
            repo: repo.into(),
            number: n.parse().unwrap(),
            title: v["title"].as_str().unwrap_or("").into(),
            base: v["baseRefName"].as_str().unwrap_or("").into(),
            body: v["body"].as_str().map(String::from),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let s = summarize(&p).unwrap();
        eprintln!("{key} ({} chars of description) in {} ms:\n{s}", p.body.as_deref().unwrap_or("").len(), started.elapsed().as_millis());
    }
}
