//! The daemon's side of a space's goal following the user's requests (see
//! `crate::goals`): requests are heard here and weighed one at a time on a
//! thread of their own, so a request is never kept waiting on the call.
//!
//! - **Heard** from `brains::ask` (what the user asks about a space: of its
//!   own tower, or of the main one) once a brain took it, and from
//!   `agendas::register` (what the tower hands out into a space for a
//!   request of the user's: `tower dispatch`, `queue`, `start`).
//! - **Once**: the same words in the same space within `SEEN_MS` are
//!   weighed once (a request asked of a space's tower comes back as its
//!   dispatch).
//! - An updated goal is in the missions at once (`missions::kick`), so
//!   the windows and the phone show it.

use std::collections::VecDeque;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, OnceLock};

/// The same request in the same space is weighed once this long.
const SEEN_MS: u64 = 6 * 3_600_000;
/// How many requests are remembered for that.
const SEEN_KEPT: usize = 200;

struct Heard {
    space: String,
    words: String,
    /// When it was asked.
    at_ms: u64,
    from: String,
}

static RUNNING: OnceLock<Sender<Heard>> = OnceLock::new();
/// The requests weighed lately: when, and the space and words.
static SEEN: Mutex<VecDeque<(u64, String)>> = Mutex::new(VecDeque::new());

/// Starts the thread that weighs them.
pub fn start() {
    let (tx, rx) = channel::<Heard>();
    if RUNNING.set(tx).is_err() {
        return;
    }
    std::thread::spawn(move || {
        for h in rx {
            match crate::goals::follow(&h.space, &h.words, Some(h.at_ms), false) {
                Ok(out) => {
                    let v = out.json(&h.space);
                    eprintln!("[goals] {} ({}): {}{}", h.space, h.from, v["outcome"].as_str().unwrap_or(""), v["why"].as_str().map(|w| format!(": {w}")).unwrap_or_default());
                    if let crate::goals::Outcome::Updated(c) | crate::goals::Outcome::Written(c) = out {
                        eprintln!("[goals] {}: goal now: {}", h.space, c.goal);
                        super::missions::kick();
                    }
                }
                Err(e) => eprintln!("[goals] {} ({}): not weighed: {e:#}", h.space, h.from),
            }
        }
    });
}

/// The words were not weighed for that space lately; now they are marked as weighed.
fn first_time(space: &str, words: &str, now: u64) -> bool {
    let key = format!("{}\n{}", space.trim().to_lowercase(), words.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.retain(|(at, _)| now.saturating_sub(*at) < SEEN_MS);
    if seen.iter().any(|(_, k)| *k == key) {
        return false;
    }
    seen.push_back((now, key));
    while seen.len() > SEEN_KEPT {
        seen.pop_front();
    }
    true
}

/// A request of the user's in `space`, asked at `at_ms` (`from`: where it
/// was heard): weighed against the space's goal, once. Nothing without a
/// daemon running this.
pub fn heard(space: &str, words: &str, at_ms: u64, from: &str) {
    let Some(tx) = RUNNING.get() else { return };
    let words = crate::goals::request_words(space, words);
    if space.trim().is_empty() || !crate::goals::worth_weighing(&words) || !first_time(space, &words, crate::history::now_ms()) {
        return;
    }
    let _ = tx.send(Heard { space: space.trim().to_string(), words, at_ms, from: from.to_string() });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_request_in_a_space_is_weighed_once() {
        assert!(first_time("Shop-x1", "Also export them as CSV", 1_000));
        assert!(!first_time("shop-x1", "also   export them as csv", 2_000), "the same words, in any case and spacing");
        assert!(first_time("Other-x1", "Also export them as CSV", 2_000), "another space weighs them too");
        assert!(first_time("Shop-x1", "Also export them as CSV", 1_000 + SEEN_MS), "and again after a while");
    }
}
