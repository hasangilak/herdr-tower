//! On hold (see `crate::decisions::Hold`): the Hold on a card's head (with
//! a field for what it waits for, the user's words if they give any), the
//! "On hold" section that lists what is held, each with those words and a
//! way back, and the line on a card that came back from hold.
//!
//! A held decision is out of the queue and the counts by the daemon's word
//! (the state has it in `decisions_held`, not `decisions`); a hold or a
//! "Back to waiting" given here moves the card at once, before the daemon
//! says so (`HoldUi::pending`).

use gpui::{AnyElement, App, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Sizable as _, v_flex};

use super::home::{LINE, SOFT, acts_row, card_box, card_title, mbtn, meta_row};
use super::{AMBER, Panel, glass};
use crate::history;
use crate::orb::model::{self, Decision};

#[derive(Default)]
pub(super) struct HoldUi {
    /// The card whose Hold is open, and its field for what it waits for.
    field: Option<(String, Entity<TextareaState>)>,
    /// Held (true) or let go (false) here, on its way to the daemon.
    pending: Vec<(String, bool)>,
}

impl HoldUi {
    /// Held here a moment ago: out of the queue already.
    pub(super) fn holding(&self, id: &str) -> bool {
        self.pending.iter().any(|(i, held)| i == id && *held)
    }

    /// Let go here a moment ago: out of "On hold" already.
    fn letting_go(&self, id: &str) -> bool {
        self.pending.iter().any(|(i, held)| i == id && !*held)
    }
}

/// When it was held, in words ("5 min ago").
fn since(ms: u64) -> String {
    let mins = history::now_ms().saturating_sub(ms) / 60_000;
    match mins {
        0 => "just now".into(),
        m if m < 60 => format!("{m} min ago"),
        m if m < 48 * 60 => format!("{} h ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

impl Panel {
    /// The open decisions on hold, newest hold first (none while the tower is off).
    pub(super) fn held_decisions(&self, cx: &App) -> Vec<Decision> {
        let t = self.tower.read(cx);
        if !t.alive {
            return vec![];
        }
        let mut held = model::held(t.state.as_ref().unwrap_or(&serde_json::Value::Null));
        held.retain(|d| !self.hold.letting_go(&d.id) && !t.acts.dismissed.contains(&d.id));
        held
    }

    /// The Hold on a card's head: opens the field for what it waits for
    /// (again: closes it).
    pub(super) fn hold_mark(&self, d: &Decision, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let id = d.id.clone();
        let open = self.hold.field.as_ref().is_some_and(|(i, _)| *i == d.id);
        div()
            .id(SharedString::from(format!("hold-{id}")))
            .flex_none()
            .px(px(9.0))
            .py(px(1.0))
            .rounded_full()
            .border_1()
            .border_color(rgba(LINE))
            .cursor_pointer()
            .text_size(px(12.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(g.muted)
            .when(open, |el| el.bg(rgba(SOFT)).text_color(g.text))
            .hover(|s| s.text_color(glass().text))
            .child("Hold")
            .on_click(cx.listener(move |this, _, window, cx| this.open_hold(&id, window, cx)))
            .into_any_element()
    }

    fn open_hold(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.hold.field.as_ref().is_some_and(|(i, _)| i == id) {
            self.hold.field = None;
            cx.notify();
            return;
        }
        // Enter holds it, as its button does; Shift+Enter is a new line.
        let field = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 4).submit_on_enter(true).placeholder("What should it wait for? (optional)"));
        field.update(cx, |s, cx| s.focus(window, cx));
        let id2 = id.to_string();
        cx.subscribe_in(&field, window, move |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = ev {
                this.hold_entered(&id2, cx);
            }
        })
        .detach();
        self.hold.field = Some((id.to_string(), field));
        cx.notify();
    }

    /// `HERDER_TOWER_ORB_KEYS=hold-<id>:…` (see `crate::orb`): its Hold's
    /// field opened as a click on Hold opens it, for keys to type into.
    pub(crate) fn keys_hold(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.decisions(cx).iter().any(|d| d.id == id) {
            eprintln!("[orb] HERDER_TOWER_ORB_KEYS: no open card {id} to hold");
            return;
        }
        self.hold.field = None;
        self.open_hold(id, window, cx);
        eprintln!("[orb] HERDER_TOWER_ORB_KEYS: hold-{id}: its Hold field is open");
    }

    /// `HERDER_TOWER_ORB_KEYS`: what the Hold field holds (None: closed), and whether it is on hold (here, or by the daemon's word).
    pub(crate) fn hold_report(&self, id: &str, cx: &App) -> (Option<String>, bool) {
        let field = self.hold.field.as_ref().filter(|(i, _)| i == id).map(|(_, f)| f.read(cx).value().to_string());
        (field, self.hold.holding(id) || self.held_decisions(cx).iter().any(|d| d.id == id))
    }

    /// The field's Hold (or Enter): on hold, with what it says.
    fn hold_entered(&mut self, id: &str, cx: &mut Context<Self>) {
        let reason = self.hold.field.as_ref().filter(|(i, _)| i == id).map(|(_, f)| f.read(cx).value().to_string()).unwrap_or_default();
        self.hold.field = None;
        self.send_hold(id, reason.trim(), false, cx);
    }

    /// Holds `id` with `words` (or, `unhold`, lets it go), through the
    /// daemon; the card moves at once. A refusal says why above the input.
    pub(super) fn send_hold(&mut self, id: &str, words: &str, unhold: bool, cx: &mut Context<Self>) {
        self.hold.pending.retain(|(i, _)| i != id);
        self.hold.pending.push((id.to_string(), !unhold));
        // The orb's number too, not only this window's queue.
        let held_id = id.to_string();
        self.act(cx, |a| if unhold { a.held.remove(&held_id) } else { a.held.insert(held_id) });
        let (id2, words) = (id.to_string(), words.to_string());
        let task = cx.background_executor().spawn(async move {
            crate::protocol::send(&crate::protocol::Request::Hold { id: id2, reason: words, unhold, by: "mac".into(), from: None })
        });
        let id3 = id.to_string();
        cx.spawn(async move |this, cx| {
            let r = task.await;
            // A moment for the state to have it, so the card doesn't come back meanwhile.
            cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
            let _ = this.update(cx, |this, cx| {
                this.hold.pending.retain(|(i, _)| *i != id3);
                // The state has it by now (or it didn't go): the daemon's word again.
                this.act(cx, |a| a.held.remove(&id3));
                match r {
                    Ok(r) if r.ok => {}
                    Ok(r) => this.status = Some((false, r.error.unwrap_or_else(|| "It couldn't be put on hold.".into()))),
                    Err(e) => this.status = Some((false, format!("It couldn't be put on hold: {e:#}"))),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Under a card whose Hold is open: what it waits for, and the buttons.
    pub(super) fn hold_strip(&mut self, d: &Decision, cx: &mut Context<Self>) -> Option<AnyElement> {
        let g = glass();
        let field = self.hold.field.as_ref().filter(|(i, _)| *i == d.id).map(|(_, f)| f.clone())?;
        let (id, id2) = (d.id.clone(), d.id.clone());
        Some(
            v_flex()
                .w_full()
                .gap(px(8.0))
                .p(px(12.0))
                .rounded(px(14.0))
                .bg(rgba(SOFT))
                .child(div().text_size(px(12.0)).text_color(g.muted).child("On hold: it stays listed, isn't counted or said, and comes back when it changes or what it waits for is done."))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(px(12.0))
                        .bg(g.pill)
                        .text_color(g.pill_text)
                        .child(Textarea::new(&field).appearance(false).bordered(false).small().w_full().text_size(px(14.0)).text_color(g.pill_text)),
                )
                .child(
                    acts_row()
                        .child(mbtn(format!("hold-go-{id}"), "Hold", true, None).on_click(cx.listener(move |this, _, _, cx| this.hold_entered(&id, cx))))
                        .child(mbtn(format!("hold-cancel-{id2}"), "Cancel", false, None).on_click(cx.listener(|this, _, _, cx| {
                            this.hold.field = None;
                            cx.notify();
                        }))),
                )
                .into_any_element(),
        )
    }

    /// The line on a card that came back from hold: why.
    pub(super) fn back_line(d: &Decision) -> Option<AnyElement> {
        let b = d.back.as_ref().filter(|b| !b.why.trim().is_empty())?;
        Some(div().text_size(px(12.0)).line_height(px(17.0)).text_color(rgb(AMBER)).child(format!("Back from hold: {}", b.why.trim())).into_any_element())
    }

    /// The "On hold" section's cards: what each is, the user's words, what
    /// brings it back, and "Back to waiting".
    pub(super) fn held_items(&mut self, held: &[Decision], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        held.iter()
            .map(|d| {
                let h = d.hold.clone().unwrap_or_default();
                let id = d.id.clone();
                let words = if h.reason.trim().is_empty() { "No reason given.".to_string() } else { format!("“{}”", h.reason.trim()) };
                let mut back = vec!["it changes".to_string()];
                if !h.until_prs.is_empty() {
                    let names: Vec<String> = h.until_prs.iter().map(|k| k.rsplit_once('#').map(|(_, n)| format!("#{n}")).unwrap_or_else(|| k.clone())).collect();
                    back.push(format!("{} merges", names.join(" or ")));
                } else if !h.reason.trim().is_empty() {
                    back.push("what it waits for looks done".into());
                }
                let prs: Vec<AnyElement> = d
                    .prs
                    .iter()
                    .map(|p| {
                        let checks = match p.checks.as_str() {
                            "pass" => "checks pass",
                            "fail" => "checks fail",
                            "pending" => "checks running",
                            _ => "no checks",
                        };
                        div().text_size(px(12.0)).text_color(g.faint).truncate().child(format!("#{} into {} · {checks}", p.number, p.base)).into_any_element()
                    })
                    .collect();
                card_box(false)
                    .child(meta_row(None, &d.space, &d.badge(), format!("On hold · {}", since(h.since_ms))))
                    .child(card_title(&d.title))
                    .children(prs)
                    .child(div().text_size(px(13.5)).line_height(px(19.0)).text_color(g.text).child(words))
                    .child(div().text_size(px(12.0)).text_color(g.faint).child(format!("Held by {}. Back when {}, or when you say.", h.who(), back.join(", "))))
                    .child(acts_row().child(mbtn(format!("unhold-{id}"), "Back to waiting", false, None).on_click(cx.listener(move |this, _, _, cx| this.send_hold(&id, "", true, cx)))))
                    .into_any_element()
            })
            .collect()
    }

    /// The "On hold" section under a queue (none when nothing is held there).
    pub(super) fn held_bay(&mut self, held: &[Decision], cx: &mut Context<Self>) -> Option<AnyElement> {
        if held.is_empty() {
            return None;
        }
        let label = format!("On hold · {} · not counted", held.len());
        Some(v_flex().w_full().gap(px(10.0)).pt(px(8.0)).child(super::home::bay_label(&label)).children(self.held_items(held, cx)).into_any_element())
    }
}
