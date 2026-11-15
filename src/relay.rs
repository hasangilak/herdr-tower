//! The tower as the relay between sessions that can't reach each other
//! directly. A session sends with `tower queue <address> "<message>"`
//! (`peers`), whatever its Claude account: every session on this Mac
//! reaches the same daemon. Where that fails, the tower carries the
//! message itself, from what the session said at the end of its turn (its
//! Stop hook: Claude Code and Codex alike):
//!
//! - **`To <address>: <message>`** lines in its final message (a Codex
//!   session whose sandbox keeps it from running `tower`, any session
//!   that writes them): each goes to that session of its space, with the
//!   sender's name, as if it had run `tower queue`.
//! - **Answers go back by themselves.** A turn that a message from another
//!   session started, which asked something (a `?`), and in which the
//!   session wrote nothing back to its sender: its final message goes back
//!   to the sender as the answer (`ANSWER` before it). One hop: an answer
//!   that came back this way is never answered back by itself, so two
//!   sessions can't keep each other going.
//!
//! This file is the reading (pure); the daemon delivers (`daemon::relay_turn`).

/// Before an answer the tower carried back.
pub const ANSWER: &str = "[answer] ";

/// The longest answer carried back whole.
const ANSWER_CHARS: usize = 3000;

/// The `To <address>: <message>` lines of a final message: (who, message),
/// in order. `who` is as written (an address like `t7`, `vps/t2`, or a
/// session's name); the daemon resolves it within the space, and drops what
/// names no session there ("To do: …" names nobody).
pub fn to_lines(said: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    for raw in said.lines() {
        let l = raw.trim().trim_start_matches(['-', '*', '>', ' ']).trim();
        let l = l.strip_prefix("**").unwrap_or(l);
        let Some(rest) = l.strip_prefix("To ").or_else(|| l.strip_prefix("to ")) else { continue };
        let Some((who, msg)) = rest.split_once(':') else { continue };
        let who = who.trim().trim_end_matches("**").trim();
        // "t17k", "vps/t2", "t17k (api)", "api": one word or an address with its name after it.
        let who = who.split_once(" (").map(|(a, _)| a.trim()).unwrap_or(who);
        if who.is_empty() || who.contains(' ') || who.len() > 64 {
            continue;
        }
        let msg = msg.trim().trim_start_matches("**").trim();
        if msg.is_empty() {
            continue;
        }
        out.push((who.to_string(), msg.to_string()));
    }
    out
}

/// What a turn's prompt says of who started it: the session (address) that
/// sent it, the message itself (unsigned), and whether it is an answer the
/// tower carried back. None: not a message from another session.
pub fn started_by(prompt: &str) -> Option<(String, String, bool)> {
    let from = crate::peers::sender_in(prompt)?;
    let body = body_of(prompt);
    let answer = body.trim_start().starts_with(ANSWER.trim_end());
    Some((from, body, answer))
}

/// A signed message's own words: after its "From … (t7): ".
fn body_of(prompt: &str) -> String {
    let t = crate::peers::unwrapped(prompt);
    let first_close = t.find("): ").map(|i| i + 3);
    match first_close {
        Some(i) if i < 200 => t[i..].trim().to_string(),
        _ => t.trim().to_string(),
    }
}

/// The answer to carry back to the session that asked: when the turn was
/// started by a message from another session (`prompt`) that asked
/// something, wasn't itself an answer carried back, and the session wrote
/// its sender nothing (`wrote_back`). (to, text).
pub fn answer_back(prompt: &str, said: &str, wrote_back: bool) -> Option<(String, String)> {
    let (from, body, answer) = started_by(prompt)?;
    let said = said.trim();
    if answer || wrote_back || !body.contains('?') || said.is_empty() {
        return None;
    }
    let text: String = if said.chars().count() > ANSWER_CHARS { format!("{}…", said.chars().take(ANSWER_CHARS).collect::<String>()) } else { said.to_string() };
    Some((from, format!("{ANSWER}{text}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_lines_are_read_as_written_and_nothing_else() {
        let said = "Done with the schema.\nTo t17k: the vault endpoint is on dev, POST /cards.\n- **To api (t17m):** can you add GET /cards?\nTo do: tidy up\nTo summarise, all good.\nto vps/t2: ping";
        assert_eq!(
            to_lines(said),
            [
                ("t17k".to_string(), "the vault endpoint is on dev, POST /cards.".to_string()),
                ("api".to_string(), "can you add GET /cards?".to_string()),
                ("do".to_string(), "tidy up".to_string()),
                ("vps/t2".to_string(), "ping".to_string()),
            ],
            "'To do' parses, and the daemon drops it: no session is called do"
        );
        assert!(to_lines("Nothing to send.").is_empty());
    }

    #[test]
    fn an_answer_goes_back_once_and_only_to_a_question() {
        let asked = "From qfix-mix › api (t17k): What is 6 times 7? Answer me with tower queue t17k.";
        assert_eq!(answer_back(asked, "42", false), Some(("t17k".into(), "[answer] 42".into())));
        // It wrote back itself: nothing more.
        assert_eq!(answer_back(asked, "42", true), None);
        // Only told something: no answer goes back.
        assert_eq!(answer_back("From qfix-mix › api (t17k): FYI the vault is on dev.", "Noted.", false), None);
        // An answer carried back is never answered back by itself (no ping-pong).
        assert_eq!(answer_back("From qfix-mix › web (t17m): [answer] 42, and what about you?", "fine", false), None);
        // Not from a session (the user, the tower): nothing.
        assert_eq!(answer_back("What is 6 times 7?", "42", false), None);
        // Herdr's own wrapper around a message it typed is seen through.
        let wrapped = "<hc from=\"t12w\">From qfix-mix › api (t17k): Which port?</hc>";
        assert_eq!(answer_back(wrapped, "3001", false), Some(("t17k".into(), "[answer] 3001".into())));
    }
}
