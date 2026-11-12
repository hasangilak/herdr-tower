//! Whether what the user typed in a session themselves already covers a
//! message the tower has waiting for it (`sendq`, `Check`): the user told
//! the session the same thing, gave it the same instruction, or made the
//! message moot. A covered message is not sent.
//!
//! - **A rule first** (`by_rule`): the tower's word that a PR was merged
//!   ("PR #787 (…) into dev was merged on GitHub.", "The user merged PR
//!   #787 …") is covered when the user said "merged" there themselves,
//!   unless the PR numbers they named are all other PRs'.
//! - **Else one headless call** (as a ticket draft is read, `ticket::argv`:
//!   the default account, `[brains] model` at low effort, no tools, no MCP,
//!   no hooks, nothing kept), the prompt on its stdin, cut off after 60 s.
//!   It answers JSON only: `{"covered": [{"id": 812, "said": 2}]}`, `said`
//!   the number of the user's message that covers it. When unsure, nothing
//!   is covered; an id or a number it wasn't given is left out.

use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::daemon::sendq::{Check, Said};

/// How long the model has to answer.
const LIMIT: Duration = Duration::from_secs(60);

/// The messages of `c` the user's words cover: each one's id and the words
/// that cover it.
pub fn judge(c: &Check) -> Result<Vec<(u64, String)>> {
    let mut out = vec![];
    let mut rest = vec![];
    for (id, text) in &c.items {
        match by_rule(&c.said, text) {
            Some(words) => out.push((*id, words)),
            None => rest.push((*id, text.clone())),
        }
    }
    if rest.is_empty() {
        return Ok(out);
    }
    let reply = ask(&prompt(&c.name, &c.said, &rest))?;
    out.extend(parse_reply(&reply, &c.said, &rest)?);
    Ok(out)
}

/// Whether `words` the user typed in a session (`name`) answer what `reply`,
/// their answer on a card that the tower holds for that session, answers:
/// the same answer, or another one to the same question (then the held
/// reply is moot). Words about something else don't.
pub fn answers_held(name: &str, words: &str, reply: &str) -> Result<bool> {
    let c = Check {
        session: String::new(),
        name: name.to_string(),
        said: vec![Said { words: words.to_string(), at_ms: 0 }],
        items: vec![(1, format!("(The user's own answer to the session's question, given earlier on a card, waiting to be typed in:) {reply}"))],
    };
    Ok(!judge(&c)?.is_empty())
}

/// The PR a note of the tower's says was merged (its number), if it is one.
fn merged_note(text: &str) -> Option<u64> {
    let t = text.trim();
    let rest = t.strip_prefix("The user merged PR #").or_else(|| t.strip_prefix("PR #").filter(|_| t.contains(" was merged on GitHub")))?;
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// The PR numbers `words` name (`#787`).
fn pr_numbers(words: &str) -> Vec<u64> {
    words.split('#').skip(1).filter_map(|r| r.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).collect()
}

/// A merge note the user's words cover: they said "merged" in that session
/// themselves (and named no PR, or this one). The words that cover it.
fn by_rule(said: &[Said], text: &str) -> Option<String> {
    let pr = merged_note(text)?;
    said.iter()
        .rev()
        .find(|s| {
            let lower = s.words.to_lowercase();
            let merged = lower.split(|c: char| !c.is_alphanumeric()).any(|w| w == "merged");
            let named = pr_numbers(&lower);
            merged && (named.is_empty() || named.contains(&pr))
        })
        .map(|s| s.words.clone())
}

/// What the model is asked.
fn prompt(name: &str, said: &[Said], items: &[(u64, String)]) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "A tower types messages into a coding agent's terminal session (\"{}\") when its turn ends. \
         The user has also typed into that session themselves. Decide which of the tower's waiting messages \
         the user's own messages already cover: the user told the session the same thing, gave it the same \
         instruction, or their words make the message moot, so typing it in now would only repeat them or \
         contradict them. A message that adds anything the user didn't say is NOT covered. When unsure, it is NOT covered.\n\n",
        if name.is_empty() { "the session" } else { name }
    ));
    p.push_str("What the user typed into the session, oldest first:\n");
    for (n, s) in said.iter().enumerate() {
        p.push_str(&format!("<user_message n=\"{}\">{}</user_message>\n", n + 1, clip(&s.words, 2000)));
    }
    p.push_str("\nThe tower's waiting messages:\n");
    for (id, text) in items {
        p.push_str(&format!("<tower_message id=\"{id}\">{}</tower_message>\n", clip(text, 3000)));
    }
    p.push_str(
        "\nAnswer with JSON only, no other words: {\"covered\": [{\"id\": <a tower_message id>, \"said\": <the n of the user_message that covers it>}]}. \
         An empty list when none is covered.",
    );
    p
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// The covered ids of the model's answer, each with the user's words that
/// cover it; only ids and numbers it was given.
fn parse_reply(out: &str, said: &[Said], items: &[(u64, String)]) -> Result<Vec<(u64, String)>> {
    let v: Value = crate::ticket::parse_reply(out)?;
    let Some(list) = v["covered"].as_array() else { bail!("the answer has no \"covered\" list") };
    let mut got = vec![];
    for c in list {
        let Some(id) = c["id"].as_u64() else { continue };
        if !items.iter().any(|(i, _)| *i == id) || got.iter().any(|(g, _)| *g == id) {
            continue;
        }
        let words = c["said"].as_u64().and_then(|n| said.get((n as usize).checked_sub(1)?)).or(said.last());
        if let Some(w) = words {
            got.push((id, w.words.clone()));
        }
    }
    Ok(got)
}

/// Asks the model once: what it printed.
fn ask(prompt: &str) -> Result<String> {
    let cfg = crate::config::Config::load()?;
    let cca = crate::accounts::cca_bin();
    let account = crate::accounts::list().ok().and_then(|l| l.first().map(|a| a.name.clone()));
    let argv = crate::ticket::argv(cca.as_deref().filter(|_| account.is_some()), account.as_deref(), &cfg.brains.model);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(std::env::temp_dir());
    for (key, _) in std::env::vars_os() {
        if crate::ticket::left_out(&key.to_string_lossy()) {
            cmd.env_remove(&key);
        }
    }
    let (code, out, err) = crate::ticket::run_with_input(cmd, prompt, LIMIT)?;
    if code != 0 && !out.trim_start().starts_with('{') {
        let why = err.lines().chain(out.lines()).map(str::trim).find(|l| !l.is_empty()).unwrap_or("it failed");
        bail!("{}: {why}", argv[0].rsplit('/').next().unwrap_or("claude"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(words: &[&str]) -> Vec<Said> {
        words.iter().enumerate().map(|(i, w)| Said { words: w.to_string(), at_ms: i as u64 }).collect()
    }

    #[test]
    fn a_merge_note_is_covered_by_the_users_own_merged() {
        let note = "PR #787 (eyzhub/eyzflow) into dev was merged on GitHub.";
        assert_eq!(merged_note(note), Some(787));
        assert_eq!(merged_note("The user merged PR #12 (a/b) into main."), Some(12));
        assert_eq!(merged_note("Release PR #787 to prod"), None);
        assert_eq!(by_rule(&said(&["fix the buttons", "merged on dev"]), note).as_deref(), Some("merged on dev"));
        assert_eq!(by_rule(&said(&["I merged #787, carry on"]), note).as_deref(), Some("I merged #787, carry on"));
        // Another PR's merge, or no merge said: not by the rule.
        assert_eq!(by_rule(&said(&["merged #786"]), note), None);
        assert_eq!(by_rule(&said(&["don't merge it yet"]), note), None);
        assert_eq!(by_rule(&said(&["merged on dev"]), "Run the e2e tests on dev"), None);
    }

    #[test]
    fn the_models_answer_names_only_what_it_was_given() {
        let s = said(&["merged on dev", "now release it to prod"]);
        let items = vec![(812, "Release #787 to prod".to_string()), (813, "Write the changelog".to_string())];
        let out = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"covered\": [{\"id\": 812, \"said\": 2}, {\"id\": 999, \"said\": 1}, {\"id\": 812, \"said\": 1}]}"}"#;
        assert_eq!(parse_reply(out, &s, &items).unwrap(), vec![(812, "now release it to prod".to_string())]);
        assert_eq!(parse_reply(r#"{"covered": []}"#, &s, &items).unwrap(), vec![]);
        // A number it wasn't given: the latest words.
        assert_eq!(parse_reply(r#"{"covered": [{"id": 813, "said": 7}]}"#, &s, &items).unwrap(), vec![(813, "now release it to prod".to_string())]);
        assert!(parse_reply("I think none", &s, &items).is_err());
    }

    #[test]
    fn the_prompt_numbers_the_users_words_and_names_each_message() {
        let p = prompt("eyzflow", &said(&["merged on dev"]), &[(812, "Release #787".into())]);
        assert!(p.contains("(\"eyzflow\")") && p.contains("<user_message n=\"1\">merged on dev</user_message>") && p.contains("<tower_message id=\"812\">Release #787</tower_message>"));
        assert!(p.contains("When unsure, it is NOT covered"));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// One real call (`cargo test live_check -- --ignored`): the user's own
    /// correction covers the tower's same correction, not new work.
    #[test]
    #[ignore]
    fn live_check() {
        let c = Check {
            session: "t8w".into(),
            name: "eyzflow".into(),
            said: vec![Said { words: "you open a case at sandbox.sooner.de not sooner.de !".into(), at_ms: 1 }],
            items: vec![
                (1, "The test case has to be opened on sandbox.sooner.de, not on sooner.de. Open it there.".into()),
                (2, "When the case is open, run the e2e tests on prod and send me the screenshots.".into()),
            ],
        };
        let started = std::time::Instant::now();
        let got = judge(&c).unwrap();
        eprintln!("took {} ms: {got:?}", started.elapsed().as_millis());
        assert_eq!(got.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![1]);
    }

    /// Real calls: a link to keep in mind is no answer to the question the
    /// held card answer is for; another answer to it is.
    #[test]
    #[ignore]
    fn live_held_reply() {
        let reply = "Yes, open the release PR from dev into main with #788 and #790. Do not merge it. After the user merges, watch the production deploy and check it on sandbox case EYZ-1032.";
        let link = answers_held("eyzflow", "https://claude.ai/artifact/CLUA9bK6zEZ4RLk19P8yda keep in mind to update this artifact", reply).unwrap();
        let other = answers_held("eyzflow", "no, don't open the release PR yet, I want to test more on dev first", reply).unwrap();
        eprintln!("link answers it: {link}; another answer: {other}");
        assert!(!link && other);
    }
}
