//! `tower progress <session> [question]`: how a session's turn is going,
//! asked of the session itself without interrupting it.
//!
//! A working Claude Code session is asked with its own `/btw` (a side
//! question it answers in an overlay while its turn goes on; nothing of it
//! goes into its conversation). The tower types `/btw <question>` in its
//! terminal, reads the answer off its screen, and closes the overlay with
//! Esc only while the overlay still shows (Esc on a working session without
//! it would interrupt its turn). Anything else (a session that isn't
//! working, another agent, no answer in time) is answered from its
//! transcript (`crate::progress`), and says why.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::hc;
use crate::progress::{self, Progress};

/// What is asked when no question is given.
pub const DEFAULT_QUESTION: &str = "How is it going? In two or three short sentences: what is done so far in this turn, what you are doing now, and what is left.";

/// How long the answer may take to show.
const ANSWER_WITHIN: Duration = Duration::from_secs(90);

/// The overlay's last line, while it shows ("↑/↓ to scroll · c to copy · f to fork · Esc to close").
fn footer(line: &str) -> bool {
    let l = line.trim();
    l.contains("Esc to close") || l.contains("Esc to dismiss")
}

/// The overlay's answer to `/btw <question>` on `screen`, and whether the
/// overlay shows: None when no overlay for it shows. The answer is its lines
/// between the question and the footer, wrapped lines joined.
pub fn btw_answer(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let end = lines.iter().rposition(|l| footer(l))?;
    let start = lines[..end].iter().rposition(|l| l.trim_start().starts_with("/btw"))?;
    let mut paras: Vec<String> = vec![];
    let mut cur: Vec<&str> = vec![];
    for l in &lines[start + 1..end] {
        let t = l.trim();
        if t.is_empty() {
            if !cur.is_empty() {
                paras.push(cur.join(" "));
                cur.clear();
            }
        } else {
            cur.push(t);
        }
    }
    if !cur.is_empty() {
        paras.push(cur.join(" "));
    }
    Some(paras.join("\n"))
}

/// A line that is only the overlay at work ("✻ Answering…", "· Thinking…").
fn at_work(answer: &str) -> bool {
    let a = answer.trim();
    a.is_empty() || (a.lines().count() == 1 && a.ends_with('…') && a.chars().next().is_some_and(|c| !c.is_alphanumeric()))
}

fn screen_of(address: &str) -> String {
    hc::run(&["read", address, "--lines", "0"]).map(|o| o.stdout).unwrap_or_default()
}

/// hc, as the daemon itself (never as a session: from inside one, hc would
/// type the text as that session's message, `<hc from=…>`, and `/btw`
/// wouldn't be a command).
fn hc_plain(args: &[&str], limit: Duration) -> Result<hc::Output> {
    let mut cmd = Command::new(crate::config::hc_bin());
    cmd.args(args).env_remove("HC_TERMINAL_ID");
    hc::output_within(cmd, &format!("hc {}", args.first().copied().unwrap_or("")), limit)
}

/// Asks a working Claude Code session `question` with `/btw`, and closes its
/// overlay. Err: why it couldn't (said to the user with the fallback).
pub fn btw(address: &str, question: &str) -> Result<String, String> {
    if btw_answer(&screen_of(address)).is_some() {
        return Err("a side question is already open on its screen".into());
    }
    let question = question.split_whitespace().collect::<Vec<_>>().join(" ");
    let typed = hc_plain(&["send", address, &format!("/btw {question}")], Duration::from_secs(30)).map_err(|e| format!("it couldn't be typed: {e:#}"))?;
    if typed.code != 0 {
        return Err(format!("it couldn't be typed: {}", typed.stderr.trim().lines().last().unwrap_or("hc send failed")));
    }
    let started = Instant::now();
    let (mut last, mut same) = (String::new(), 0);
    let answer = loop {
        thread::sleep(Duration::from_millis(1000));
        match btw_answer(&screen_of(address)) {
            // The same answer three looks running: written out.
            Some(a) if !at_work(&a) => {
                same = if a == last { same + 1 } else { 0 };
                if same >= 2 {
                    break Some(a);
                }
                last = a;
            }
            Some(_) => {}
            // Shown and gone already (closed by hand): what was read is the answer.
            None if !last.is_empty() => break Some(last.clone()),
            None => {}
        }
        if started.elapsed() > ANSWER_WITHIN {
            break (!last.is_empty()).then_some(last.clone());
        }
    };
    // Closed only while it shows: Esc to a turn without it would stop the turn.
    if btw_answer(&screen_of(address)).is_some() {
        let _ = hc_plain(&["keys", address, "esc"], Duration::from_secs(10));
    }
    answer.ok_or_else(|| format!("no answer showed within {} s", ANSWER_WITHIN.as_secs()))
}

/// What the transcript says of its turn, in a few sentences: the fallback.
pub fn from_transcript(name: &str, status: &str, p: Option<&Progress>, now: u64) -> String {
    let Some(p) = p else {
        return format!("{name} is {status}; the tower has read nothing of its turn yet.");
    };
    let mut out = vec![];
    if status == "working" {
        out.push(format!("{name} is working: {}.", progress::line(p, now)));
    } else {
        out.push(format!("{name} is {status}."));
    }
    if !p.items.is_empty() {
        let next: Vec<&str> = p.items.iter().filter(|i| i.status == "pending").map(|i| i.subject.as_str()).take(3).collect();
        let mut s = format!("Its task list: {} of {} done", p.done, p.items.len());
        if !next.is_empty() {
            s.push_str(&format!("; left: {}", next.join("; ")));
        }
        out.push(format!("{s}."));
    }
    if let Some(said) = &p.said {
        out.push(format!("Its latest words: \"{said}\""));
    }
    out.join(" ")
}

/// `tower progress <session> [question]`: asked aside, or read from its
/// transcript. `by`: who asks (user, brain, phone), kept with it.
pub fn ask(session: &str, question: &str, by: &str) -> Result<Value> {
    let Some(t) = hc::get(session) else { bail!("no session {session}") };
    let address = t["address"].as_str().unwrap_or(session).to_string();
    let name = hc::spoken_name(&t);
    let status = hc::status_word(&t).unwrap_or_else(|| "starting".into());
    let app = hc::agent_app(&t).unwrap_or_default();
    let now = crate::history::now_ms();
    let question = Some(question.trim()).filter(|q| !q.is_empty()).unwrap_or(DEFAULT_QUESTION).to_string();
    let progress = super::running::progress_of(&address).or_else(|| super::running::reader_of(&address).and_then(|r| r.progress(None, now))).map(|mut p| {
        p.line = progress::line(&p, now);
        p
    });
    record(&address, &question, &format!("asked:{by}"));
    let (via, answer, why) = if status == "working" && app == "claude-code" {
        match btw(&address, &question) {
            Ok(a) => ("btw", a, None),
            Err(why) => ("transcript", from_transcript(&name, &status, progress.as_ref(), now), Some(why)),
        }
    } else {
        let why = if status != "working" { format!("it isn't working ({status}): nothing to ask aside") } else { format!("it runs {}, which has no /btw", if app.is_empty() { "no agent the tower knows" } else { &app }) };
        ("transcript", from_transcript(&name, &status, progress.as_ref(), now), Some(why))
    };
    record(&address, &answer, via);
    eprintln!("[progress] {address} ({name}) asked by {by}, answered by {via}{}", why.as_ref().map(|w| format!(" ({w})")).unwrap_or_default());
    Ok(json!({
        "session": address, "name": name, "status": status, "asked": question,
        "via": via, "answer": answer, "why": why, "progress": progress,
    }))
}

/// Into the history, as the session's (`progress`): the question
/// (`asked:<by>`) and its answer (`btw` or `transcript`), shown in its thread.
fn record(address: &str, text: &str, status: &str) {
    if cfg!(test) {
        return;
    }
    crate::history::record_session_as("progress", address, text, Some(status), true);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overlay as a working session showed it (Claude Code 2.1.325, read through Herdr).
    const SCREEN: &str = "✽ Counting lines in src/browsers.rs… (30s · ↓ 2.8k tokens)
  ⎿  ✔ Count the Rust files in src
     ◼ Count the lines in src/browsers.rs
▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔
    /btw how is it going? one line: which item are you on and what is left
      I'm on item 2: src/browsers.rs doesn't exist, so that line count failed. Item 1 is done (66 Rust files in src), and items 3–5 (build-cache wait, tests in src/brain.rs, one-line summary) are
      left; the harness also blocked the sleep 45 waits.
    ↑/↓ to scroll · c to copy · f to fork · Esc to close";

    #[test]
    fn the_btw_answer_is_read_off_the_overlay() {
        assert_eq!(
            btw_answer(SCREEN).as_deref(),
            Some("I'm on item 2: src/browsers.rs doesn't exist, so that line count failed. Item 1 is done (66 Rust files in src), and items 3–5 (build-cache wait, tests in src/brain.rs, one-line summary) are left; the harness also blocked the sleep 45 waits.")
        );
        // No overlay (closed, or never opened): nothing, and no Esc.
        assert_eq!(btw_answer("✽ Counting… (30s)\n❯ \n  ⏸ manual mode on"), None);
        // A /btw in its scrollback with no overlay under it isn't one.
        assert_eq!(btw_answer("❯ /btw old question\n⏺ an answer\n❯ "), None);
        // While it answers.
        assert!(at_work("✻ Answering…") && !at_work("I'm on item 2."));
        assert_eq!(btw_answer("    /btw how is it going?\n\n    ↑/↓ to scroll · Esc to close").as_deref(), Some(""));
    }

    #[test]
    fn the_fallback_says_the_line_the_list_and_its_words() {
        let mut r = crate::progress::Reader::default();
        let line = |v: Value| v.to_string();
        r.read(
            &[
                line(json!({"type":"user","timestamp":"2026-11-14T05:00:00.000Z","message":{"content":"go"}})),
                line(json!({"type":"assistant","timestamp":"2026-11-14T05:00:01.000Z","message":{"content":[{"type":"tool_use","id":"a","name":"TodoWrite","input":{"todos":[
                    {"content":"Read","status":"completed","activeForm":"Reading"},{"content":"Fix","status":"in_progress","activeForm":"Fixing"},{"content":"Test","status":"pending","activeForm":"Testing"}]}}]}})),
                line(json!({"type":"assistant","timestamp":"2026-11-14T05:00:02.000Z","message":{"content":[{"type":"text","text":"Fixing the parser now."}]}})),
            ]
            .join("\n"),
        );
        let now = crate::releases::ms_of_iso("2026-11-14T05:03:00Z").unwrap();
        let p = r.progress(None, now);
        assert_eq!(
            from_transcript("api", "working", p.as_ref(), now),
            "api is working: step 2 of 3: fixing · 3 min in · last update 2 min ago. Its task list: 1 of 3 done; left: Test. Its latest words: \"Fixing the parser now.\""
        );
        assert_eq!(from_transcript("api", "done", None, now), "api is done; the tower has read nothing of its turn yet.");
    }
}
