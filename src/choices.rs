//! The buttons of a card the tower leaves to the user with nothing to tap:
//! a session's turn that ended waiting for them (`questions::WAITS`), or a
//! question at the end of a turn they typed in its terminal
//! (`questions::TYPED_THERE`). The brain isn't asked about those, so they
//! came with no answers at all, only the field for the user's own words.
//!
//! - **At once**: `generic` ("Go ahead", "Not yet"), so the card is never
//!   without a button.
//! - **A moment later**: two or three choices of its own (`derive`), read
//!   from what the session said by one headless call (as `covered::judge`
//!   reads: the default account, `[brains] model` at low effort, no tools,
//!   nothing kept, 60 s at most): a short label for the button and the
//!   instruction it sends the session, the tower's pick first with why.
//!   They take the generic ones' place while nobody has answered yet.
//! - Each is a recommendation of the set's one question (its reply is the
//!   instruction), so the card, the queue's decision (its `options`, the
//!   phone's buttons) and the answer that is typed in all work as for any
//!   question the tower reads. "Other" stays below.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::questions::Rec;

/// The generic buttons, while nothing more fitting is known.
pub fn generic() -> Vec<Rec> {
    vec![
        Rec { label: "Go ahead".into(), why: String::new(), choice: json!({ "other": "Go ahead with what you proposed." }) },
        Rec { label: "Not yet".into(), why: String::new(), choice: json!({ "other": "Not yet: hold off and wait for me." }) },
    ]
}

/// They are the generic ones (a set's own may take their place).
pub fn is_generic(recs: &[Rec]) -> bool {
    recs == generic().as_slice()
}

/// Two or three choices of its own for what the session (`name`) said
/// (`message`, the end of its turn) and waits for (`request`). Err: the
/// model couldn't be asked, or answered nothing usable.
pub fn derive(name: &str, message: &str, request: &str) -> Result<Vec<Rec>> {
    parse(&crate::covered::ask(&prompt(name, message, request))?)
}

/// What the model is asked.
fn prompt(name: &str, message: &str, request: &str) -> String {
    format!(
        "A coding agent's terminal session (\"{}\") ended its turn waiting for the user. Offer the user two or three concrete answers \
         they could give, as buttons. Each has a short label (two to six words, what the user decides, like \"Push and open the PR into dev\" \
         or \"Not yet\") and the instruction the button sends the session: one or two plain sentences, complete on their own, telling it \
         exactly what to do now. Only what the session proposed or asked about, or holding off; never something the user must do by hand, \
         and never more than it asked for. The first one is the answer you would pick, with a short reason why; include a way to hold off \
         when that is sensible.\n\n\
         <what_it_waits_for>{}</what_it_waits_for>\n<its_message>{}</its_message>\n\n\
         Answer with JSON only, no other words: {{\"options\": [{{\"label\": \"…\", \"why\": \"…\", \"instruction\": \"…\"}}]}}",
        if name.is_empty() { "the session" } else { name },
        clip(request, 600),
        clip(message, 3000)
    )
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n { s.to_string() } else { format!("…{}", s.chars().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect::<String>()) }
}

/// The choices of the model's answer: two or three with a label (cut to a
/// button's length) and an instruction; the first one's why is kept.
fn parse(out: &str) -> Result<Vec<Rec>> {
    let v: Value = crate::ticket::parse_reply(out)?;
    let Some(list) = v["options"].as_array() else { bail!("the answer has no \"options\"") };
    let mut recs: Vec<Rec> = vec![];
    for o in list {
        let label = o["label"].as_str().unwrap_or("").trim();
        let instruction = o["instruction"].as_str().unwrap_or("").trim();
        if label.is_empty() || instruction.is_empty() || recs.iter().any(|r| r.label.eq_ignore_ascii_case(label)) {
            continue;
        }
        let label = if label.chars().count() > 48 { format!("{}…", label.chars().take(47).collect::<String>().trim_end()) } else { label.to_string() };
        let why = if recs.is_empty() { o["why"].as_str().unwrap_or("").trim().to_string() } else { String::new() };
        recs.push(Rec { label, why, choice: json!({ "other": instruction }) });
        if recs.len() == 3 {
            break;
        }
    }
    if recs.len() < 2 {
        bail!("fewer than two choices came back");
    }
    Ok(recs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_models_choices_become_buttons_with_their_instructions() {
        let out = concat!(
            r#"{"type":"result","result":"{\"options\": ["#,
            r#"{\"label\": \"Push and open the PR into dev\", \"why\": \"production confirmed the count\", \"instruction\": \"Push both commits and open a PR into dev.\"}, "#,
            r#"{\"label\": \"Push, no PR yet\", \"why\": \"x\", \"instruction\": \"Push both commits to the branch, but don't open a PR yet.\"}, "#,
            r#"{\"label\": \"Not yet\", \"instruction\": \"Not yet: keep the commits local and wait for me.\"}, "#,
            r#"{\"label\": \"A fourth\", \"instruction\": \"Never shown.\"}]}"}"#
        );
        let recs = parse(out).unwrap();
        assert_eq!(recs.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["Push and open the PR into dev", "Push, no PR yet", "Not yet"]);
        assert_eq!(recs[0].why, "production confirmed the count");
        assert_eq!(recs[1].why, "", "only the pick says why");
        assert_eq!(recs[0].choice, json!({ "other": "Push both commits and open a PR into dev." }));
        // Too few, or none usable: the generic ones stay.
        assert!(parse(r#"{"options": [{"label": "Go", "instruction": "Go."}]}"#).is_err());
        assert!(parse(r#"{"options": [{"label": "", "instruction": "x"}, {"label": "Y", "instruction": ""}]}"#).is_err());
        assert!(parse("I'd push it").is_err());
        // A label too long for a button is cut.
        let long = parse(r#"{"options": [{"label": "Push both of the commits now and then open the pull request into dev please", "instruction": "Push."}, {"label": "No", "instruction": "Wait."}]}"#).unwrap();
        assert!(long[0].label.chars().count() <= 48 && long[0].label.ends_with('…'), "{}", long[0].label);
    }

    #[test]
    fn the_generic_buttons_go_ahead_or_hold_off() {
        let g = generic();
        assert_eq!(g.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["Go ahead", "Not yet"]);
        assert!(is_generic(&g) && !is_generic(&g[..1]));
        let p = prompt("eyzflow", "Both commits are still local.", "your call on a push and PR");
        assert!(p.contains("(\"eyzflow\")") && p.contains("<what_it_waits_for>your call on a push and PR</what_it_waits_for>") && p.contains("{\"options\""));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// One real call (`cargo test live_choices -- --ignored`): the push/PR wait of the screenshot.
    #[test]
    #[ignore]
    fn live_choices() {
        let message = "Production confirms the number: Isa's filter returns 22 offers once these commits ship. Nothing else changes on my side. Both commits (1a235c3a, 8d2d649d) are still local on isa-requests-insight/eyzflow, waiting for your call on a push and PR.";
        let started = std::time::Instant::now();
        let recs = derive("eyzflow", message, "Both commits (1a235c3a, 8d2d649d) are still local on isa-requests-insight/eyzflow, waiting for your call on a push and PR.").unwrap();
        eprintln!("took {} ms: {recs:#?}", started.elapsed().as_millis());
        assert!((2..=3).contains(&recs.len()));
    }
}
