//! Everything the tower is about to say, in one line: the brain's words to
//! the user come first, alerts (an agent needs you, a watch reported back)
//! after. The speech thread takes a sentence at a time from here, so a reply
//! that comes in jumps the alerts still waiting, and nothing is ever said
//! over something else.
//!
//! What waits is checked again when its turn comes: an alert about an agent
//! that has since moved on, or an old one, is dropped; so is a reply to a
//! question the user has since replaced with a new one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The brain answering the user.
    Reply,
    /// Not asked for: an agent's news, a watch reporting back.
    Alert,
}

pub struct Item {
    pub kind: Kind,
    parts: VecDeque<String>,
    norm: String,
    /// An alert about one agent: its address and the status it had.
    about: Option<(String, String)>,
    at: Instant,
    /// The user's request it came after.
    request: u64,
    started: bool,
}

/// A sentence to speak now.
pub struct Next {
    pub kind: Kind,
    pub text: String,
    /// The first sentence of its item.
    pub first: bool,
}

pub struct Queue {
    items: Vec<Item>,
    /// Requests the user has made so far.
    request: u64,
    max_age: Duration,
}

impl Queue {
    pub fn new(max_age: Duration) -> Self {
        Self { items: vec![], request: 0, max_age }
    }

    pub fn item(&self, kind: Kind, text: &str, about: Option<(String, String)>) -> Item {
        Item {
            kind,
            parts: super::tts::quick_start(text::sentences(text)).into(),
            norm: text::normalize(text),
            about,
            at: Instant::now(),
            request: self.request,
            started: false,
        }
    }

    /// Adds an item, unless the same words are waiting already. Returns
    /// whether it was added.
    pub fn push(&mut self, item: Item) -> bool {
        if item.parts.is_empty() || self.items.iter().any(|i| !i.started && i.norm == item.norm) {
            return false;
        }
        // Newer news about an agent replaces what was waiting about it.
        if let Some((address, _)) = &item.about {
            self.items.retain(|i| i.started || i.about.as_ref().is_none_or(|(a, _)| a != address));
        }
        self.items.push(item);
        true
    }

    /// Puts an item ahead of everything (`tower say --interrupt`).
    pub fn push_front(&mut self, item: Item) {
        if !item.parts.is_empty() {
            self.items.insert(0, item);
        }
    }

    pub fn clear(&mut self) -> usize {
        let n = self.items.len();
        self.items.clear();
        n
    }

    /// The user asked something new: replies to what they asked before
    /// are no longer wanted. Returns how many were dropped.
    pub fn asked(&mut self) -> usize {
        self.request += 1;
        let before = self.items.len();
        self.items.retain(|i| i.kind != Kind::Reply);
        before - self.items.len()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn has_alerts(&self) -> bool {
        self.items.iter().any(|i| i.kind == Kind::Alert)
    }

    /// The next sentence to say. An item once begun is finished first;
    /// then the oldest reply; then, if `alerts` (nobody is waiting for an
    /// answer), the oldest alert still worth saying. `status` gives an
    /// agent's status now, if the radar knows it.
    pub fn next(&mut self, alerts: bool, status: impl Fn(&str) -> Option<String>) -> Option<Next> {
        let max_age = self.max_age;
        let request = self.request;
        self.items.retain(|i| {
            if i.started {
                return true;
            }
            match i.kind {
                Kind::Reply => i.request == request,
                Kind::Alert => {
                    let fresh = i.at.elapsed() < max_age;
                    let current = i.about.as_ref().is_none_or(|(a, said)| status(a).is_none_or(|now| same_news(said, &now)));
                    if !(fresh && current) {
                        eprintln!("[speech] dropped, no longer news: {:?}", i.parts.front().map(String::as_str).unwrap_or(""));
                    }
                    fresh && current
                }
            }
        });
        let at = self
            .items
            .iter()
            .position(|i| i.started)
            .or_else(|| self.items.iter().position(|i| i.kind == Kind::Reply))
            .or_else(|| if alerts { self.items.iter().position(|i| i.kind == Kind::Alert) } else { None })?;
        let item = &mut self.items[at];
        let first = !item.started;
        item.started = true;
        let text = item.parts.pop_front()?;
        let kind = item.kind;
        if item.parts.is_empty() {
            self.items.remove(at);
        }
        Some(Next { kind, text, first })
    }
}

/// An alert still says what is true: the status hasn't changed (done and
/// idle are both finished).
fn same_news(said: &str, now: &str) -> bool {
    let finished = |s: &str| matches!(s, "done" | "idle");
    said == now || finished(said) && finished(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> Queue {
        Queue::new(Duration::from_secs(60))
    }

    fn texts(q: &mut Queue, alerts: bool, status: impl Fn(&str) -> Option<String>) -> Vec<String> {
        std::iter::from_fn(|| q.next(alerts, &status)).map(|n| n.text).collect()
    }

    #[test]
    fn replies_go_first_and_alerts_wait() {
        let mut q = q();
        let a = q.item(Kind::Alert, "The orb session needs you.", Some(("t1b".into(), "blocked".into())));
        q.push(a);
        let r = q.item(Kind::Reply, "Nothing else is running.", None);
        q.push(r);
        let blocked = |_: &str| Some("blocked".to_string());
        // Someone waits for an answer: only the reply.
        assert_eq!(texts(&mut q, false, blocked), ["Nothing else is running."]);
        assert_eq!(texts(&mut q, true, blocked), ["The orb session needs you."]);
    }

    #[test]
    fn a_begun_item_is_finished_first() {
        let mut q = q();
        let a = q.item(Kind::Alert, "The orb session hit an error. It can't build the shader any more.", None);
        q.push(a);
        let first = q.next(true, |_| None).unwrap();
        assert!(first.first);
        let r = q.item(Kind::Reply, "Yes.", None);
        q.push(r);
        let second = q.next(true, |_| None).unwrap();
        assert_eq!((second.kind, second.first), (Kind::Alert, false));
        assert_eq!(q.next(true, |_| None).unwrap().text, "Yes.");
    }

    #[test]
    fn drops_duplicates_and_stale_news() {
        let mut q = q();
        let a = q.item(Kind::Alert, "Eyzflow finished.", Some(("t22".into(), "done".into())));
        assert!(q.push(a));
        let again = q.item(Kind::Alert, "Eyzflow finished.", Some(("t22".into(), "done".into())));
        assert!(!q.push(again));
        // It started working again: "finished" is no longer true.
        assert!(texts(&mut q, true, |_| Some("working".into())).is_empty());

        let a = q.item(Kind::Alert, "Eyzflow is working.", Some(("t22".into(), "working".into())));
        q.push(a);
        let b = q.item(Kind::Alert, "Eyzflow needs you.", Some(("t22".into(), "blocked".into())));
        q.push(b);
        // The newer news about the same agent replaced the older.
        assert_eq!(texts(&mut q, true, |_| Some("blocked".into())), ["Eyzflow needs you."]);

        let mut old = Queue::new(Duration::ZERO);
        let a = old.item(Kind::Alert, "Old news.", None);
        old.push(a);
        assert!(texts(&mut old, true, |_| None).is_empty());
    }

    #[test]
    fn a_new_request_drops_old_replies() {
        let mut q = q();
        let r = q.item(Kind::Reply, "The answer to the old question.", None);
        q.push(r);
        let a = q.item(Kind::Alert, "Codex finished.", None);
        q.push(a);
        assert_eq!(q.asked(), 1);
        assert_eq!(texts(&mut q, true, |_| None), ["Codex finished."]);
        // What comes after the new request is its answer.
        let r = q.item(Kind::Reply, "The new answer.", None);
        q.push(r);
        assert_eq!(texts(&mut q, true, |_| None), ["The new answer."]);
    }
}
