//! The chat keeps to its newest message: while you are at (or near) the
//! bottom, whatever comes in below (an answer, your own message, a card, a
//! screenshot, a question, the working line) scrolls into view with an
//! eased glide. Scrolled up to read, you stay where you are, and a small
//! "New messages" button takes you down.
//!
//! It watches the list's laid-out height rather than its messages, so
//! anything that makes the list taller (a picture that comes in late
//! included) is followed. The list is looked at after each layout (`watch`,
//! a canvas drawn after it), and the offset it asks for takes effect the
//! next frame.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{AnyElement, EntityId, ScrollHandle, canvas, point, prelude::*, px};

/// This close to the bottom counts as at the bottom.
const NEAR: f32 = 48.0;

/// A glide to the bottom, begun at `start` (s) from `from` (px scrolled).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Glide {
    from: f32,
    start: f32,
    secs: f32,
}

/// Where the list is and what it does next. Positions are pixels scrolled
/// from the top (0 at the top, `max` at the bottom).
#[derive(Debug)]
pub struct Follow {
    /// At the bottom: what comes in is followed.
    stuck: bool,
    /// Something came in below while you were scrolled up.
    unseen: bool,
    /// The panel just opened: the first layout with the history in goes
    /// straight to the bottom, no glide.
    opening: bool,
    last_max: f32,
    last_pos: f32,
    /// The list's whole height and its window's, as last laid out: the
    /// list taller means something came in; the window resized (the input
    /// growing as you type, shrinking as it sends) doesn't.
    last_content: f32,
    last_view: f32,
    glide: Option<Glide>,
    /// The position the glide last asked for, to tell your scrolling from it.
    asked: Option<f32>,
}

impl Default for Follow {
    fn default() -> Self {
        Self {
            stuck: true,
            unseen: false,
            opening: true,
            last_max: 0.0,
            last_pos: 0.0,
            last_content: 0.0,
            last_view: 0.0,
            glide: None,
            asked: None,
        }
    }
}

/// Ease out (cubic): quick at first, settling gently.
fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Longer glides take a little longer, never long.
fn glide_secs(dist: f32) -> f32 {
    (0.22 + dist / 2400.0).min(0.42)
}

impl Follow {
    /// Shows the "New messages" button.
    pub fn unseen(&self) -> bool {
        self.unseen && !self.stuck
    }

    /// Takes the list down to the newest message (the button, or a message
    /// of your own): it glides there and follows from then on.
    pub fn go_down(&mut self, now: f32) {
        self.stuck = true;
        self.unseen = false;
        if self.glide.is_none() && self.last_pos < self.last_max - 0.5 {
            self.glide = Some(Glide { from: self.last_pos, start: now, secs: glide_secs(self.last_max - self.last_pos) });
        }
    }

    /// Animating: another frame is wanted.
    pub fn gliding(&self) -> bool {
        self.glide.is_some()
    }

    /// The list as laid out: scrolled `pos` of `max`, in a window `view`
    /// high; `ready` once the history is in. Returns the position to scroll
    /// to, if it should move.
    pub fn observe(&mut self, pos: f32, max: f32, view: f32, ready: bool, now: f32) -> Option<f32> {
        let content = max + view;
        let grew = content > self.last_content + 0.5;
        let resized = (view - self.last_view).abs() > 0.5;
        self.last_content = content;
        self.last_view = view;
        if self.opening {
            self.last_max = max;
            self.last_pos = pos;
            if !ready {
                return None;
            }
            self.opening = false;
            self.stuck = true;
            self.last_pos = max;
            return (pos < max - 0.5).then_some(max);
        }
        // You scrolled during the glide (it isn't where the glide put it,
        // nor pinned there by the list getting shorter): it's yours again.
        if let Some(a) = self.asked
            && (pos - a).abs() > 1.0
            && pos < max - 0.5
        {
            self.glide = None;
            self.asked = None;
        }
        self.last_max = max;
        self.last_pos = pos;
        if self.glide.is_none() {
            if self.stuck && (grew || resized) {
                // Pinned: down to the new bottom.
                if pos < max - 0.5 {
                    self.glide = Some(Glide { from: pos, start: now, secs: glide_secs(max - pos) });
                }
            } else if grew {
                self.unseen = true;
            } else if !resized {
                // Nothing came in and nothing resized: you scrolled (or not).
                self.stuck = pos >= max - NEAR;
                if self.stuck {
                    self.unseen = false;
                }
            }
        }
        let g = self.glide?;
        let t = ((now - g.start) / g.secs).clamp(0.0, 1.0);
        // Toward the bottom as it is now: what comes in mid-glide is followed too.
        let to = g.from + (max - g.from) * ease(t);
        if t >= 1.0 {
            self.glide = None;
            self.asked = None;
        } else {
            self.asked = Some(to);
        }
        self.last_pos = to;
        Some(to)
    }
}

/// Follows a list: shared by the panel and the canvas that watches the list.
#[derive(Clone)]
pub struct Follower {
    state: Rc<RefCell<Follow>>,
    epoch: Instant,
}

impl Default for Follower {
    fn default() -> Self {
        Self { state: Rc::default(), epoch: Instant::now() }
    }
}

impl Follower {
    fn now(&self) -> f32 {
        self.epoch.elapsed().as_secs_f32()
    }

    pub fn unseen(&self) -> bool {
        self.state.borrow().unseen()
    }

    pub fn go_down(&self) {
        let now = self.now();
        self.state.borrow_mut().go_down(now);
    }

    /// Drawn after the list (a sibling following it): looks at the list as
    /// just laid out, scrolls it, and asks `view` to draw again while it
    /// glides or when the button should come or go.
    pub fn watch(&self, handle: &ScrollHandle, ready: bool, view: EntityId) -> AnyElement {
        let me = self.clone();
        let handle = handle.clone();
        canvas(
            move |_, window, _| {
                let pos = -handle.offset().y.as_f32();
                let max = handle.max_offset().y.as_f32();
                let height = handle.bounds().size.height.as_f32();
                let now = me.now();
                let mut f = me.state.borrow_mut();
                let shown = f.unseen();
                if let Some(to) = f.observe(pos, max, height, ready, now) {
                    let x = handle.offset().x;
                    handle.set_offset(point(x, px(-to)));
                }
                if f.gliding() || f.unseen() != shown {
                    window.on_next_frame(move |_, cx| cx.notify(view));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_0()
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chat's window height in these tests.
    const V: f32 = 500.0;

    /// Runs frames until the glide ends; returns where it ended.
    fn settle(f: &mut Follow, max: f32, mut now: f32) -> (f32, f32) {
        let mut pos = f.last_pos;
        for _ in 0..200 {
            if let Some(p) = f.observe(pos, max, V, true, now) {
                pos = p;
            }
            if !f.gliding() {
                break;
            }
            now += 1.0 / 60.0;
        }
        (pos, now)
    }

    fn opened(max: f32) -> Follow {
        let mut f = Follow::default();
        assert_eq!(f.observe(0.0, max, V, true, 0.0), Some(max));
        f
    }

    #[test]
    fn opening_waits_for_the_history_then_jumps_to_the_bottom() {
        let mut f = Follow::default();
        assert_eq!(f.observe(0.0, 0.0, V, false, 0.0), None);
        // History in: straight down, no glide.
        assert_eq!(f.observe(0.0, 900.0, V, true, 0.1), Some(900.0));
        assert!(!f.gliding());
        // A short history that fits: nothing to do.
        let mut f = Follow::default();
        assert_eq!(f.observe(0.0, 0.0, V, true, 0.0), None);
    }

    #[test]
    fn at_the_bottom_new_content_glides_into_view_eased() {
        let mut f = opened(900.0);
        // An answer comes in: the list is 120 px taller.
        let first = f.observe(900.0, 1020.0, V, true, 1.0).unwrap();
        assert!(f.gliding());
        assert!((900.0..1020.0).contains(&first));
        // Eased out: the first half of the time covers well over half the way.
        let mid = f.observe(first, 1020.0, V, true, 1.0 + glide_secs(120.0) / 2.0).unwrap();
        assert!(mid > 900.0 + 120.0 * 0.75, "{mid}");
        let (end, _) = settle(&mut f, 1020.0, 1.0 + glide_secs(120.0) / 2.0);
        assert_eq!(end, 1020.0);
        assert!(!f.unseen());
    }

    #[test]
    fn near_the_bottom_counts_as_at_it() {
        let mut f = opened(900.0);
        // Scrolled up a little (less than NEAR): still following.
        assert_eq!(f.observe(870.0, 900.0, V, true, 1.0), None);
        assert!(f.observe(870.0, 1000.0, V, true, 2.0).is_some());
        assert!(f.gliding());
    }

    #[test]
    fn scrolled_up_stays_put_and_shows_the_button() {
        let mut f = opened(900.0);
        // You scroll up to read.
        assert_eq!(f.observe(300.0, 900.0, V, true, 1.0), None);
        assert!(!f.unseen());
        // A message comes in: you aren't moved.
        assert_eq!(f.observe(300.0, 1020.0, V, true, 2.0), None);
        assert!(f.unseen());
        // The button: down, gliding, then following again.
        f.go_down(3.0);
        assert!(!f.unseen());
        let (end, now) = settle(&mut f, 1020.0, 3.0);
        assert_eq!(end, 1020.0);
        assert!(f.observe(1020.0, 1100.0, V, true, now + 1.0).is_some());
    }

    #[test]
    fn scrolling_back_down_by_hand_hides_the_button() {
        let mut f = opened(900.0);
        f.observe(300.0, 900.0, V, true, 1.0);
        f.observe(300.0, 1020.0, V, true, 2.0);
        assert!(f.unseen());
        assert_eq!(f.observe(1000.0, 1020.0, V, true, 3.0), None);
        assert!(!f.unseen());
    }

    #[test]
    fn scrolling_during_a_glide_stops_it() {
        let mut f = opened(900.0);
        let p = f.observe(900.0, 1500.0, V, true, 1.0).unwrap();
        // You flick up mid-glide.
        assert_eq!(f.observe(p - 400.0, 1500.0, V, true, 1.05), None);
        assert!(!f.gliding());
        // And more comes in: you stay.
        assert_eq!(f.observe(p - 400.0, 1600.0, V, true, 1.5), None);
        assert!(f.unseen());
    }

    #[test]
    fn content_growing_mid_glide_is_followed_to_the_new_bottom() {
        let mut f = opened(900.0);
        let p = f.observe(900.0, 1000.0, V, true, 1.0).unwrap();
        // A picture comes in: taller again before the glide ends.
        let p = f.observe(p, 1200.0, V, true, 1.05).unwrap();
        let (end, _) = settle(&mut f, 1200.0, 1.05);
        assert!(p < 1200.0);
        assert_eq!(end, 1200.0);
    }

    #[test]
    fn a_list_getting_shorter_keeps_following() {
        let mut f = opened(900.0);
        // The working line goes as the answer comes: shorter, pinned at the bottom.
        assert_eq!(f.observe(850.0, 850.0, V, true, 1.0), None);
        assert!(f.observe(850.0, 1000.0, V, true, 2.0).is_some());
    }

    #[test]
    fn your_own_message_brings_you_down_even_scrolled_up() {
        let mut f = opened(900.0);
        f.observe(200.0, 900.0, V, true, 1.0);
        f.go_down(2.0);
        let (end, _) = settle(&mut f, 1000.0, 2.0);
        assert_eq!(end, 1000.0);
    }

    /// Runs frames with the list as `max` (window `view`) until the glide ends.
    fn settle_in(f: &mut Follow, max: f32, view: f32, mut now: f32) -> (f32, f32) {
        let mut pos = f.last_pos;
        for _ in 0..200 {
            if let Some(p) = f.observe(pos, max, view, true, now) {
                pos = p;
            }
            if !f.gliding() {
                break;
            }
            now += 1.0 / 60.0;
        }
        (pos, now)
    }

    /// The send as the panel sees it: the input grows as you type a long
    /// message, Enter shows "Sending…", the input empties, your message
    /// comes in, the working line ("Thinking"), a screenshot, the answer.
    /// From scrolled up or at the bottom, it ends at the bottom, followed
    /// throughout, never with the button.
    #[test]
    fn sending_glides_to_your_message_and_keeps_following_the_reply() {
        for scrolled_up in [false, true] {
            let mut f = opened(9000.0);
            let mut pos = 9000.0;
            let mut now = 1.0;
            if scrolled_up {
                pos = 4000.0;
                assert_eq!(f.observe(pos, 9000.0, V, true, now), None);
            }
            // Typing: the input grows 60 px, the chat's window shrinks.
            now += 1.0;
            let (p, t) = settle_in(&mut f, 9060.0, V - 60.0, now);
            if scrolled_up {
                assert_eq!(p, pos, "typing doesn't move you");
                assert!(!f.unseen(), "nor is it news");
            } else {
                assert_eq!(p, 9060.0, "at the bottom, typing keeps you there");
            }
            pos = p;
            now = t + 0.5;
            f.go_down(now);
            // (content, view) as each step lays out: Sending…, the input
            // empties, your message, Thinking, a screenshot, the answer.
            let steps = [(9530.0, V - 60.0), (9530.0, V), (9650.0, V), (9690.0, V), (9990.0, V), (10150.0, V)];
            for (content, view) in steps {
                let max = content - view;
                // A few frames between each: the glide is underway, not over.
                for _ in 0..4 {
                    if let Some(p) = f.observe(pos.min(max), max, view, true, now) {
                        pos = p;
                    }
                    now += 1.0 / 60.0;
                }
                assert!(!f.unseen(), "no button while following");
            }
            let (end, _) = settle_in(&mut f, 10150.0 - V, V, now);
            assert_eq!(end, 10150.0 - V, "scrolled_up={scrolled_up}: ends at the newest");
            // And it still follows: more of the answer comes in.
            assert!(f.observe(end, end + 80.0, V, true, now + 1.0).is_some());
        }
    }

    #[test]
    fn a_resized_window_while_scrolled_up_is_not_news() {
        let mut f = opened(900.0);
        f.observe(300.0, 900.0, V, true, 1.0);
        // The input grows, then shrinks: you stay, no button.
        assert_eq!(f.observe(300.0, 960.0, V - 60.0, true, 2.0), None);
        assert_eq!(f.observe(300.0, 900.0, V, true, 3.0), None);
        assert!(!f.unseen());
        // Then a message does come in.
        assert_eq!(f.observe(300.0, 1000.0, V, true, 4.0), None);
        assert!(f.unseen());
    }
}
