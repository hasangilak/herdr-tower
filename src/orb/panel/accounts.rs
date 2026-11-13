//! The Claude accounts in the full window: the sidebar's strip, and the
//! Accounts view it opens. What it shows is the daemon's (`crate::usage`):
//! each account's usage as the accounts mod (`cca`) last recorded it, what
//! the tower found of its limit (`crate::failover`), and every tower and
//! session with the account it runs on.
//!
//! - **The strip** (the sidebar, over its foot): a line per account, its
//!   badge, and the window nearest its limit as a bar with its percent
//!   (`usage::Line::fullest`: the 5-hour one unless the 7-day one is fuller
//!   or the only one Claude Code reports), and under it which window that
//!   is, when it resets and the other's percent. Amber from `NEAR`, and "out
//!   until 15:00" once the tower found it out. A click opens the view.
//! - **The view**: a card per account (both windows, with when each
//!   resets; out until when, and why; the default; where a new session
//!   starts; how many sessions and towers run on it), then **Who runs
//!   where**: the main tower, each open space (its own tower first, then
//!   its sessions), and the sessions outside any space, each with its
//!   account's badge. One the failover moved says from which account and
//!   when (until it moves again); one moving now, or waiting for an
//!   account, says so.
//! - A space's own view has the same, for its tower and sessions (`seat_line`).
//!
//! It follows the state: the daemon writes the board again when usage
//! changes (it looks every 15 s) and at once when the failover acts.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px, relative, rgb, rgba};
use gpui_kit::component::{h_flex, v_flex};

use super::home::{CARD, LINE, SOFT, bay_label, empty_box};
use super::space::session_mark;
use super::window::{bay, lrow, mark_box, nm};
use super::{AMBER, CYAN, Glass, Panel, badge, glass};
use crate::accounts::Window;
use crate::orb::nav::Scope;
use crate::usage::{self, Board, Group, Line, Seat};

/// From this percent a window is drawn in amber: near its limit.
const NEAR: f64 = 80.0;

/// A window's bar: `used` percent of it filled, amber near its limit.
fn bar(used: f64, h: f32) -> AnyElement {
    let colour = if used >= NEAR { AMBER } else { CYAN };
    let filled = (used / 100.0).clamp(0.0, 1.0) as f32;
    div()
        .w_full()
        .h(px(h))
        .rounded_full()
        .bg(rgba(0xffffff24))
        .child(div().h_full().w(relative(filled)).rounded_full().bg(rgb(colour)))
        .into_any_element()
}

/// "42%" or "–", in amber near the limit.
fn pct_el(w: Option<&Window>, size: f32, g: &Glass) -> AnyElement {
    let el = div().flex_none().text_size(px(size)).font_weight(FontWeight::SEMIBOLD);
    match w {
        Some(w) => el.text_color(if w.used >= NEAR { rgb(AMBER).into() } else { g.text }).child(usage::pct(w.used)),
        None => el.text_color(g.faint).child("–"),
    }
    .into_any_element()
}

/// The badge for an account: the default outlined, others in their colour.
fn account_badge(account: Option<&str>, board: Option<&Board>, g: &Glass) -> AnyElement {
    match account {
        Some(a) => badge(a, board.and_then(|b| b.list.first()).is_some_and(|d| d.name == a), g),
        None => div().flex_none().text_xs().text_color(g.faint).child("account not known").into_any_element(),
    }
}

/// What a seat says beside its name: moved here, moving, waiting, couldn't move.
/// The colour: cyan for a move done, amber for what isn't over.
pub(super) fn seat_note(s: &Seat, now_ms: u64) -> Option<(String, u32)> {
    if s.moving {
        return Some(("moving to another account now".into(), AMBER));
    }
    if s.waiting {
        return Some(("waits for an account with usage".into(), AMBER));
    }
    let m = s.moved.as_ref()?;
    Some((format!("↪ {}", usage::moved_words(m, now_ms)), if m.how == "moved" { CYAN } else { AMBER }))
}

/// The line under a session's (or a tower's) row in a space: its account's
/// badge, and what the failover did with it.
pub(super) fn seat_line(s: &Seat, board: Option<&Board>, now_ms: u64) -> AnyElement {
    let g = glass();
    h_flex()
        .gap(px(7.0))
        .items_center()
        .min_w_0()
        .child(account_badge(s.account.as_deref(), board, &g))
        .children(seat_note(s, now_ms).map(|(t, c)| div().min_w_0().truncate().text_size(px(12.0)).text_color(rgb(c)).child(t)))
        .into_any_element()
}

/// A tower's mark: a small outlined square (a session's is its status).
pub(super) fn tower_mark(g: &Glass) -> AnyElement {
    mark_box(Some(div().size(px(10.0)).rounded(px(3.0)).border(px(1.5)).border_color(g.muted).into_any_element())).into_any_element()
}

impl Panel {
    /// The board the state carries, and every seat in it (None: the daemon
    /// writes no board, an older one).
    pub(super) fn usage_now(&self, cx: &Context<Self>) -> (Option<Board>, Vec<Group>) {
        let t = self.tower.read(cx);
        let Some(state) = t.state.as_ref() else { return (None, vec![]) };
        let board = usage::of_state(state);
        let groups = usage::seats(state, board.as_ref());
        (board, groups)
    }

    /// The sidebar's strip: a line per account (None until the daemon writes a board).
    pub(super) fn accounts_strip(&mut self, on: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let g = glass();
        let (board, _) = self.usage_now(cx);
        let board = board.filter(|b| !b.list.is_empty())?;
        let now = crate::history::now_ms();
        let rows = board.list.iter().map(|l| {
            // The window nearest its limit is the bar; the line under says which, when it resets, and the other.
            let full = l.fullest();
            let under = if let Some(until) = l.out_until_ms {
                div().text_color(rgb(AMBER)).child(format!("out until {}", crate::failover::when(until, now)))
            } else {
                let mut words: Vec<String> = vec![];
                if let Some((label, w)) = full {
                    words.push(match w.resets_ms {
                        Some(r) => format!("{label} ↻{}", crate::failover::when(r, now)),
                        None => label.to_string(),
                    });
                    let other = if label == "5h" { ("7d", l.seven_day.as_ref()) } else { ("5h", l.five_hour.as_ref()) };
                    if let (o, Some(w)) = other {
                        words.push(format!("{o} {}", usage::pct(w.used)));
                    }
                }
                div().text_color(g.faint).child(if words.is_empty() { "no usage recorded yet".to_string() } else { words.join(" · ") })
            };
            v_flex()
                .gap(px(3.0))
                .child(
                    h_flex()
                        .gap(px(8.0))
                        .items_center()
                        .child(div().w(px(64.0)).flex_none().child(badge(&l.name, l.default, &g)))
                        .child(div().flex_1().min_w_0().child(bar(full.map(|(_, w)| w.used).unwrap_or(0.0), 5.0)))
                        .child(div().w(px(34.0)).flex_none().flex().justify_end().child(pct_el(full.map(|(_, w)| w), 12.0, &g))),
                )
                .child(div().pl(px(72.0)).text_size(px(11.0)).line_height(px(14.0)).truncate().child(under))
        });
        Some(
            v_flex()
                .id("side-accounts")
                .w_full()
                .gap(px(7.0))
                .px(px(8.0))
                .py(px(8.0))
                .rounded(px(12.0))
                .border_1()
                .cursor_pointer()
                .when(on, |el| el.bg(rgba(0xffffff2b)).border_color(rgba(LINE)))
                .when(!on, |el| el.border_color(gpui::transparent_black()).hover(|s| s.bg(rgba(SOFT))))
                .child(bay_label("Accounts").px(px(0.0)))
                .children(rows)
                .on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::Accounts, cx)))
                .into_any_element(),
        )
    }

    /// One account's card: its windows, whether it is out, what runs on it.
    fn account_card(&self, l: &Line, board: &Board, groups: &[Group], now: u64) -> AnyElement {
        let g = glass();
        let out = l.out_until_ms.is_some();
        let pill = |text: &str| div().flex_none().px(px(8.0)).rounded_full().border_1().border_color(rgba(LINE)).text_size(px(11.0)).text_color(g.faint).child(text.to_string());
        let mut tags: Vec<AnyElement> = vec![];
        // (An account called "default" says so by its name.)
        if l.default && !l.name.eq_ignore_ascii_case("default") {
            tags.push(pill("the default").into_any_element());
        }
        if board.starts_on.as_deref() == Some(l.name.as_str()) && board.list.len() > 1 {
            tags.push(pill("new sessions start here").into_any_element());
        }
        let recorded = l.five_hour.is_some() || l.seven_day.is_some();
        // A window: its bar and percent, and when it resets under the bar (or,
        // while the other one is recorded, that Claude Code doesn't report this one now).
        let window = |label: &str, w: Option<&Window>| {
            let reset = match w {
                Some(w) => w.resets_ms.map(|r| format!("resets {}", crate::failover::when(r, now))),
                None if recorded => Some("not reported by Claude Code now".to_string()),
                None => None,
            };
            v_flex()
                .gap(px(2.0))
                .child(
                    h_flex()
                        .gap(px(10.0))
                        .items_center()
                        .child(div().w(px(50.0)).flex_none().text_size(px(12.0)).text_color(g.muted).child(label.to_string()))
                        .child(div().flex_1().min_w_0().child(bar(w.map(|w| w.used).unwrap_or(0.0), 7.0)))
                        .child(div().w(px(42.0)).flex_none().flex().justify_end().child(pct_el(w, 13.0, &g))),
                )
                .children(reset.map(|r| div().pl(px(60.0)).text_size(px(11.5)).line_height(px(15.0)).text_color(g.faint).child(r)))
        };
        let (sessions, towers) = usage::on(groups, &l.name);
        let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
        let runs = match (sessions, towers) {
            (0, 0) => "Nothing runs on it now.".to_string(),
            (s, 0) => format!("Runs {}.", plural(s, "session", "sessions")),
            (0, t) => format!("Runs {}.", plural(t, "tower", "towers")),
            (s, t) => format!("Runs {} and {}.", plural(s, "session", "sessions"), plural(t, "tower", "towers")),
        };
        v_flex()
            .w_full()
            .p(px(14.0))
            .gap(px(9.0))
            .rounded(px(18.0))
            .bg(rgba(CARD))
            .border_1()
            .border_color(if out { gpui::Hsla::from(rgb(AMBER)) } else { gpui::Hsla::from(rgba(LINE)) })
            .child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .flex_wrap()
                    .child(badge(&l.name, l.default, &g))
                    .child(div().min_w_0().truncate().text_size(px(12.0)).text_color(g.faint).child(l.email.clone()))
                    .child(div().flex_1())
                    .children(tags),
            )
            .when(out, |el| {
                el.child(div().text_size(px(13.0)).line_height(px(18.0)).text_color(rgb(AMBER)).child(format!(
                    "{}. What hits its limit moves to the next account with usage.",
                    capital(&usage::out_words(l, now))
                )))
            })
            .child(window("5-hour", l.five_hour.as_ref()))
            .child(window("7-day", l.seven_day.as_ref()))
            .when(!recorded, |el| {
                el.child(div().text_size(px(12.0)).line_height(px(16.0)).text_color(g.faint).child("No usage recorded for it yet: the accounts mod records it from Claude Code's status line."))
            })
            .child(div().text_size(px(12.5)).text_color(g.muted).child(runs))
            .into_any_element()
    }

    /// Who runs where: each group with its seats.
    fn seats_list(&self, groups: &[Group], board: Option<&Board>, now: u64, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let mut out: Vec<AnyElement> = vec![];
        for (gi, group) in groups.iter().enumerate() {
            let title = group.title();
            let (s, t) = group.seats.iter().fold((0, 0), |(s, t), x| if x.tower { (s, t + 1) } else { (s + 1, t) });
            let count = match (s, t) {
                (0, _) => String::new(),
                (1, _) => "1 session".into(),
                (n, _) => format!("{n} sessions"),
            };
            let head = h_flex()
                .id(SharedString::from(format!("acc-group-{gi}")))
                .gap(px(8.0))
                .items_center()
                .px(px(8.0))
                .pt(px(if gi == 0 { 0.0 } else { 6.0 }))
                .child(div().text_size(px(13.0)).font_weight(FontWeight::SEMIBOLD).child(title.clone()))
                .child(div().text_size(px(12.0)).text_color(g.faint).child(count));
            let head = match &group.space {
                Some(space) => {
                    let to = Scope::Space(space.clone());
                    head.cursor_pointer().hover(|st| st.opacity(0.8)).on_click(cx.listener(move |this, _, _, cx| this.nav_to(to.clone(), cx))).into_any_element()
                }
                None => head.into_any_element(),
            };
            out.push(head);
            for (si, seat) in group.seats.iter().enumerate() {
                let label = if seat.name.is_empty() { seat.address.clone() } else { seat.name.clone() };
                let what = if seat.tower {
                    if group.main { "the main tower".to_string() } else { "this space's tower".to_string() }
                } else {
                    super::space::status_word(&seat.status).to_string()
                };
                let mut under = vec![
                    div()
                        .text_size(px(12.0))
                        .line_height(px(16.0))
                        .text_color(g.faint)
                        .child(format!("{what} · {}", seat.address))
                        .into_any_element(),
                ];
                if let Some((text, colour)) = seat_note(seat, now) {
                    under.push(div().text_size(px(12.0)).line_height(px(16.0)).text_color(rgb(colour)).child(text).into_any_element());
                }
                let mark = if seat.tower { tower_mark(&g) } else { session_mark(&seat.status, format!("acc-pulse-{gi}-{si}")) };
                let row = lrow(mark, nm(&label).truncate().into_any_element(), Some(account_badge(seat.account.as_deref(), board, &g)), under)
                    .id(SharedString::from(format!("acc-seat-{gi}-{si}")));
                let row = match (&group.space, seat.tower) {
                    (Some(space), false) => {
                        let to = Scope::Session { space: space.clone(), addr: seat.address.clone() };
                        row.cursor_pointer().hover(|st| st.bg(rgba(SOFT))).on_click(cx.listener(move |this, _, _, cx| this.nav_to(to.clone(), cx))).into_any_element()
                    }
                    _ => row.into_any_element(),
                };
                out.push(row);
            }
        }
        out
    }

    /// The Accounts view: a card per account, then who runs where.
    pub(super) fn accounts_view(&mut self, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let (board, groups) = self.usage_now(cx);
        let now = crate::history::now_ms();
        let mut sub = vec![];
        match &board {
            Some(b) if !b.list.is_empty() => {
                sub.push(format!("{} accounts · usage as the accounts mod (cca) recorded it", b.list.len()));
                if let Some(s) = &b.starts_on {
                    sub.push(format!("A new session starts on {s}; one at its account's limit moves to the next with usage."));
                }
            }
            Some(_) => sub.push("No Claude accounts: the accounts mod (cca) isn't installed.".into()),
            None => sub.push("This tower's daemon doesn't report usage yet (it needs the newer tower).".into()),
        }
        let head = super::window::whead("Accounts", sub.into_iter().map(|s| div().child(s).into_any_element()).collect(), None);
        let mut items: Vec<AnyElement> = vec![];
        if let Some(b) = board.as_ref().filter(|b| !b.list.is_empty()) {
            let pane_w = self.full.as_ref().map(|f| f.pane_w).unwrap_or(600.0);
            let cards: Vec<AnyElement> = b.list.iter().map(|l| self.account_card(l, b, &groups, now)).collect();
            // Two across when there is room.
            let cards = if pane_w >= 640.0 {
                h_flex().w_full().gap(px(12.0)).items_start().flex_wrap().children(cards.into_iter().map(|c| div().w(px((pane_w - 12.0) / 2.0 - 1.0)).child(c))).into_any_element()
            } else {
                v_flex().w_full().gap(px(12.0)).children(cards).into_any_element()
            };
            items.push(cards);
        }
        let seats = self.seats_list(&groups, board.as_ref(), now, cx);
        if seats.is_empty() {
            items.push(bay("Who runs where", vec![empty_box("No tower or session runs now.")]));
        } else {
            items.push(bay("Who runs where", vec![v_flex().gap(px(2.0)).children(seats).into_any_element()]));
        }
        (head, items)
    }
}

/// "out until 15:00" -> "Out until 15:00".
fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failover::Moved;

    #[test]
    fn a_seat_says_what_the_failover_did_with_it() {
        let moved = Moved { at_ms: 1, from: "default".into(), to: "orca".into(), how: "moved".into(), ..Default::default() };
        let s = Seat { address: "t1".into(), account: Some("orca".into()), moved: Some(moved.clone()), ..Default::default() };
        let (text, colour) = seat_note(&s, 2).unwrap();
        assert!(text.starts_with("↪ moved from default at "), "{text}");
        assert_eq!(colour, CYAN);
        let failed = Seat { moved: Some(Moved { how: "failed".into(), why: "no shell".into(), ..moved }), ..s.clone() };
        assert_eq!(seat_note(&failed, 2).map(|(t, c)| (t.contains("couldn't move to orca") && t.ends_with(": no shell"), c)), Some((true, AMBER)));
        assert_eq!(seat_note(&Seat { moving: true, ..s.clone() }, 2).map(|(_, c)| c), Some(AMBER), "moving says so before the last move");
        assert!(seat_note(&Seat { waiting: true, moved: None, ..s.clone() }, 2).unwrap().0.contains("waits"));
        assert_eq!(seat_note(&Seat::default(), 2), None);
        assert_eq!(capital("out until 15:00"), "Out until 15:00");
    }
}
