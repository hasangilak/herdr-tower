//! The floating panel (`docs/quiet-tower.md`, "Steps 4 and 6"; F1 to F9 and
//! I1 to I4 in the design): what needs the user, and nothing else. The queue
//! and the cards' parts drawn here are the full window's too (`window`).
//!
//! - **The status line**: how many decisions are the user's and how many
//!   spaces are at work; under it, faintly, what hovering the orb used to
//!   say (what the tower is doing, what it last heard or said, an error),
//!   and "Phone: pair again on this network" while the tower says it
//!   (`phone::phone_hint`).
//! - **The queue** (`crate::orb::model`): one card open, tagged `Next`, or
//!   `Picked` when it was chosen from behind the first; the rest as rows
//!   under "N more waiting", and a click on a row opens it. An answer on its
//!   way into a session is a short card of its own; one that landed leaves a
//!   one-line receipt for about two seconds.
//! - **The spaces**: the open ones as chips (an amber count, an outline when
//!   muted, a pulse at work, a tick at rest) and "+ N quiet". A chip opens
//!   its space in the full window.
//! - **The reply** to what the user last asked the tower, above the input,
//!   and one line while the tower works on it.
//! - **Number keys** answer the open card (1 is its first answer) while the
//!   message is empty and no other field has the keyboard.
//! - **Full window** opens the tower's window, or closes it; **Mute** is the
//!   tower-wide one (`tower mute`), as ⌥-click on the orb.
//!
//! The cards themselves: a question set's is in `questions` (`ask_card`), a
//! merge's in `prchip` (`merge_card`), any other decision's is `plain_card`.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AsyncApp, Context, FontWeight, SharedString, div, prelude::*, pulsating_between,
    px, rgb, rgba,
};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use serde_json::{Value, json};

use super::{AMBER, CYAN, NAVY, PALE, Panel, ROSE, glass, span, working_words};
use crate::history::{self, Entry};
use crate::orb::Tower;
use crate::orb::model::{self, Chip, Decision, Kind, Missions, Stage};
use crate::orb::nav::Scope;
use crate::questions::QuestionSet;

/// The quiet views' surfaces on the glass: a card, a row behind it, a line.
pub(super) const CARD: u32 = 0xffffff1c;
pub(super) const SOFT: u32 = 0xffffff14;
pub(super) const LINE: u32 = 0xffffff2e;

/// The tower's reply to the last request stays above the input this long (ms).
const REPLY_FRESH_MS: u64 = 10 * 60 * 1000;

/// After a number key answered, the next one counts only this much later (ms).
pub(super) const KEY_GAP_MS: u64 = 500;

// ---------------------------------------------------------------------------
// Pieces every card is made of.

/// A section's heading: `SPACES`, `2 MORE WAITING`.
pub(super) fn bay_label(text: &str) -> gpui::Div {
    div().px(px(4.0)).text_size(px(11.0)).font_weight(FontWeight::SEMIBOLD).text_color(glass().faint).child(text.to_uppercase())
}

/// A card: the open one is edged in the orb's pale blue.
pub(super) fn card_box(open: bool) -> gpui::Div {
    v_flex().w_full().p(px(14.0)).gap(px(10.0)).rounded(px(18.0)).bg(rgba(CARD)).border_1().border_color(if open {
        rgb(PALE)
    } else {
        rgba(LINE)
    })
}

/// A card's first line: its tag (`Next`, `Picked`, `2 of 3`), its space, why
/// it is the user's, and `rest` (who waits and for how long, or `answering`).
pub(super) fn meta_row(tag: Option<&str>, space: &str, why: &str, rest: String) -> gpui::Div {
    let g = glass();
    h_flex()
        .flex_wrap()
        .gap_x(px(8.0))
        .gap_y(px(4.0))
        .items_center()
        .text_size(px(12.0))
        .text_color(g.muted)
        .children(tag.filter(|t| !t.is_empty()).map(|t| {
            div()
                .flex_none()
                .px(px(8.0))
                .py(px(1.0))
                .rounded_full()
                .bg(rgb(PALE))
                .text_color(rgb(NAVY))
                .text_size(px(11.0))
                .font_weight(FontWeight::BOLD)
                .child(t.to_string())
        }))
        .when(!space.is_empty(), |el| el.child(div().font_weight(FontWeight::SEMIBOLD).text_color(g.text).child(space.to_string())))
        .when(!why.is_empty(), |el| {
            el.child(
                div()
                    .flex_none()
                    .px(px(8.0))
                    .py(px(1.0))
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(AMBER))
                    .text_color(rgb(AMBER))
                    .text_size(px(11.0))
                    .child(why.to_string()),
            )
        })
        .when(!rest.is_empty(), |el| el.child(div().child(rest)))
}

/// A card's question or headline.
/// More to read on a card: the command, the plan, the message that asks,
/// in the small monospace block, `lines` at most.
pub(super) fn detail_block(text: &str, lines: usize) -> AnyElement {
    div().px_2().py_1().rounded(px(8.0)).bg(rgba(0x0000002e)).font_family("Menlo").text_xs().line_clamp(lines).child(text.trim().to_string()).into_any_element()
}

pub(super) fn card_title(text: &str) -> AnyElement {
    div().text_size(px(16.0)).line_height(px(21.0)).font_weight(FontWeight::SEMIBOLD).child(text.to_string()).into_any_element()
}

/// A line of how it stands, under a card's title; `bad`: something went wrong.
pub(super) fn cstat(text: impl Into<SharedString>, bad: bool) -> AnyElement {
    div()
        .text_size(px(13.0))
        .line_height(px(18.0))
        .text_color(if bad { rgb(ROSE).into() } else { glass().muted })
        .child(text.into())
        .into_any_element()
}

/// The same, with a spinner: something is on its way.
pub(super) fn cstat_busy(text: impl Into<SharedString>) -> AnyElement {
    h_flex()
        .gap_2()
        .items_center()
        .text_size(px(13.0))
        .text_color(glass().muted)
        .child(Spinner::new().xsmall().color(rgb(0xffffff).into()))
        .child(div().min_w_0().child(text.into()))
        .into_any_element()
}

/// The number key that does what the button or row it sits on does (`0` is key 1).
pub(super) fn kbd(n: usize) -> gpui::Div {
    div()
        .flex_none()
        .px(px(5.0))
        .rounded(px(5.0))
        .bg(rgb(NAVY))
        .text_color(rgb(PALE))
        .text_size(px(10.0))
        .line_height(px(15.0))
        .font_weight(FontWeight::BOLD)
        .child((n + 1).to_string())
}

/// A card's button: amber when it is the main one, with its number key.
pub(super) fn mbtn(id: impl Into<SharedString>, label: &str, primary: bool, key: Option<usize>) -> gpui::Stateful<gpui::Div> {
    h_flex()
        .id(id.into())
        .flex_none()
        .max_w_full()
        .gap(px(8.0))
        .items_center()
        .px(px(14.0))
        .py(px(7.0))
        .rounded_full()
        .border_1()
        .cursor_pointer()
        .text_size(px(13.0))
        .font_weight(FontWeight::SEMIBOLD)
        .when(primary, |el| el.bg(rgb(AMBER)).border_color(rgb(AMBER)).text_color(rgb(NAVY)).hover(|s| s.bg(rgb(0xffd98a))))
        .when(!primary, |el| el.border_color(rgba(LINE)).hover(|s| s.bg(rgba(SOFT))))
        .children(key.map(kbd))
        .child(div().min_w_0().child(label.to_string()))
}

/// The buttons of a card, side by side, wrapping.
pub(super) fn acts_row() -> gpui::Div {
    h_flex().flex_wrap().gap(px(8.0)).items_center()
}

/// The little x that takes a card off the queue.
pub(super) fn dismiss_mark(id: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    glyph_mark(id, IconName::Close)
}

/// Minimizes the full window into the orb: a round switch on the glass, as
/// tall as the pills beside it (`tg`), its line in the middle.
pub(super) fn minimize_mark(id: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex_none()
        .size(px(28.0))
        .rounded_full()
        .border_1()
        .border_color(rgba(LINE))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(glass().muted)
        .hover(|s| s.bg(rgba(SOFT)).text_color(glass().text))
        .active(|s| s.bg(rgba(0xffffff2b)))
        .child(Icon::new(IconName::Minus).small())
}

fn glyph_mark(id: impl Into<SharedString>, icon: IconName) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex_none()
        .p(px(3.0))
        .rounded_full()
        .cursor_pointer()
        .text_color(glass().faint)
        .hover(|s| s.bg(rgba(SOFT)))
        .child(Icon::new(icon).xsmall())
}

/// A space's mark, `size` points across: an amber count, the same as an
/// outline when it is muted, a pulse while it works, a tick at rest. `key`
/// names the pulse (one animation per mark).
pub(super) fn sign(chip: Chip, size: f32, key: impl Into<SharedString>) -> AnyElement {
    let mark =
        div().size(px(size)).flex_none().rounded_full().flex().items_center().justify_center().text_size(px((size * 0.6).round())).font_weight(FontWeight::BOLD);
    match chip {
        Chip::Yours(n) => mark.bg(rgb(AMBER)).text_color(rgb(NAVY)).child(n.to_string()).into_any_element(),
        Chip::Muted(n) => mark.border_1().border_color(rgb(AMBER)).text_color(rgb(AMBER)).child(n.to_string()).into_any_element(),
        Chip::Working => mark
            .child(div().size(px(9.0)).rounded_full().bg(rgb(CYAN)).with_animation(
                key.into(),
                Animation::new(Duration::from_millis(1800)).repeat().with_easing(pulsating_between(0.3, 1.0)),
                |el, d| el.opacity(d),
            ))
            .into_any_element(),
        Chip::Done => mark.text_color(rgb(CYAN)).child("✓").into_any_element(),
    }
}

/// A small switch on the glass (`Full window`, `Follow`, a setup): pale when it is on.
pub(super) fn tg(id: impl Into<SharedString>, text: &str, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex_none()
        .px(px(12.0))
        .py(px(5.0))
        .rounded_full()
        .border_1()
        .cursor_pointer()
        .text_size(px(12.0))
        .font_weight(FontWeight::SEMIBOLD)
        .when(on, |el| el.bg(rgb(PALE)).border_color(rgb(PALE)).text_color(rgb(NAVY)))
        .when(!on, |el| el.border_color(rgba(LINE)).text_color(glass().muted).hover(|s| s.bg(rgba(SOFT))))
        .child(text.to_string())
}

/// What a card offers, in one string: which decision it is, whose question
/// it shows, its answers in order, its PRs. While this stays the same, a
/// number means the same thing.
pub(super) fn face(d: &Decision) -> String {
    let options: Vec<&str> = d.options.iter().map(|o| o.label.as_str()).collect();
    let prs: Vec<&str> = d.prs.iter().map(|p| p.key.as_str()).collect();
    format!("{}|{}|{}|{}", d.id, d.question_id.as_deref().unwrap_or(""), options.join("\u{1f}"), prs.join("\u{1f}"))
}

/// A card has shown the same answers for long enough (`shown_ms`) that a
/// number key is meant for it, not for the one whose place it just took.
pub(super) fn settled(shown_ms: u64) -> bool {
    shown_ms >= KEY_GAP_MS
}

/// The option a key answers: a bare digit (1 is the first; a Persian or
/// Arabic keyboard's digits too), while the message is empty and no "Other"
/// field has the keyboard.
pub(super) fn answer_key(key: &str, modified: bool, message_empty: bool, other_focused: bool) -> Option<usize> {
    if modified || !message_empty || other_focused {
        return None;
    }
    let mut chars = key.chars();
    let (Some(d), None) = (chars.next(), chars.next()) else { return None };
    [('1', '9'), ('۱', '۹'), ('١', '٩')].iter().find(|(one, nine)| (*one..=*nine).contains(&d)).map(|(one, _)| d as usize - *one as usize)
}

/// What the user last asked the tower, and what it said back.
#[derive(Debug, PartialEq)]
pub(super) struct Reply {
    pub asked: String,
    /// None while the tower still works on it.
    pub reply: Option<String>,
    /// When it was asked (Unix ms): which request this is.
    pub at: u64,
    /// What the tower opened in answer to it (`crate::acts`): its receipt.
    pub act: Option<ActShown>,
}

/// An act as the reply shows it: its line, what was assumed as chips, and
/// whether it can still be undone; once undone, how it closed.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct ActShown {
    pub id: String,
    pub line: String,
    /// What was assumed: (the chip's words, why).
    pub chips: Vec<(String, String)>,
    pub undone: bool,
    /// Undo is offered (false: undone, or work would be lost).
    pub can_undo: bool,
    /// Why undoing it needs more than a tap.
    pub why: Option<String>,
    /// What undoing it came to.
    pub closing: Option<String>,
}

impl ActShown {
    /// An act as the tower holds it now (`state.json`'s `acts`).
    pub fn of_view(v: &Value) -> Option<ActShown> {
        Some(ActShown {
            id: v["id"].as_str()?.to_string(),
            line: v["line"].as_str().unwrap_or("").to_string(),
            chips: v["assumed"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| Some((c["value"].as_str().filter(|v| !v.is_empty())?.to_string(), c["why"].as_str().unwrap_or("").to_string())))
                .collect(),
            undone: v["state"] == "undone",
            can_undo: v["state"] != "undone" && v["undo"]["possible"] == true,
            why: v["undo"]["why"].as_str().map(String::from),
            closing: v["closing"].as_str().map(String::from),
        })
    }

    /// The act a request's history entries tell of (`kind: act`, in order):
    /// as the tower holds it now (`acts`), else as its last entry left it,
    /// with nothing to undo.
    pub fn of_entries(entries: &[&Entry], acts: &[Value]) -> Option<ActShown> {
        let last = entries.last()?;
        let id = last.id.as_deref()?;
        if let Some(now) = acts.iter().find(|a| a["id"].as_str() == Some(id)) {
            return Self::of_view(now);
        }
        let mut v = crate::chat::act_of_entry(last);
        if v["line"].is_null() {
            v["line"] = json!(entries.iter().rev().find(|e| e.status.as_deref() != Some("undone")).map(|e| e.text.clone()).unwrap_or_default());
        }
        Self::of_view(&v)
    }
}

/// The latest request and the first answer after it, while they are news:
/// until `REPLY_FRESH_MS` after the answer (or after asking, with none yet).
/// `space`: what was asked of that space's tower and what it said; `None`:
/// of the main tower.
///
/// - The answer is the tower's reply or the receipt it was told (`answer`,
///   `receipt`); never what was only written down (`record`: a note, a
///   receipt kept for the record).
/// - While spaces have no tower of their own, what is typed in a space's
///   view goes to the main tower with the space named in front (`cli::
///   in_space`): that request, and the reply to it, are that space's here.
#[cfg(test)]
pub(super) fn last_reply(entries: &[Entry], now_ms: u64, space: Option<&str>) -> Option<Reply> {
    last_reply_with(entries, now_ms, space, &[])
}

/// `last_reply`, with the acts as the tower holds them now (`state.json`'s
/// `acts`): what it opened in answer to the request shows with the reply,
/// as it is now (undone, or no longer to be undone lightly).
pub(super) fn last_reply_with(entries: &[Entry], now_ms: u64, space: Option<&str>, acts: &[Value]) -> Option<Reply> {
    let of = |e: &Entry| e.space.as_deref().filter(|s| !s.is_empty()).map(String::from);
    let named = space.map(|s| super::cli::in_space(s, ""));
    // What the request says, if it is this view's.
    let mine = |e: &Entry| -> Option<String> {
        if e.kind != "request" || e.held || e.record {
            return None;
        }
        match (space, of(e)) {
            (None, None) => Some(e.text.trim().to_string()),
            (Some(s), Some(es)) if es == s => Some(e.text.trim().to_string()),
            (Some(_), None) => e.text.trim().strip_prefix(named.as_deref()?.trim_end()).map(|rest| rest.trim().to_string()),
            _ => None,
        }
    };
    let (i, asked, text) = entries.iter().enumerate().rev().find_map(|(i, e)| mine(e).map(|text| (i, e, text)))?;
    // Its answer comes from the tower that was asked: a space's own answers
    // as that space; the main one's carry no space (a receipt may name the
    // space its request was about).
    let from = of(asked);
    let answer = entries[i + 1..].iter().find(|e| {
        matches!(e.kind.as_str(), "answer" | "receipt") && !e.record && !e.held && !e.text.trim().is_empty() && (of(e) == from || (from.is_none() && e.kind == "receipt"))
    });
    // What the tower opened for it: its receipt, whenever its words come.
    let opened: Vec<&Entry> = entries[i + 1..]
        .iter()
        .filter(|e| e.kind == "act" && !e.record && e.act.as_ref().is_some_and(|a| a["request_at"].as_u64() == Some(asked.at)))
        .collect();
    let act = ActShown::of_entries(&opened, acts);
    let latest = answer.map(|a| a.at).into_iter().chain(opened.last().map(|e| e.at)).max().unwrap_or(asked.at);
    if now_ms.saturating_sub(latest) >= REPLY_FRESH_MS {
        return None;
    }
    Some(Reply {
        asked: if text.is_empty() && !asked.attachments.is_empty() { "(files)".into() } else { text },
        reply: answer.map(|a| a.text.trim().to_string()),
        at: asked.at,
        act,
    })
}

/// `space › session` (the session that waits on it), or whichever of the two is known.
pub(super) fn whereabouts(d: &Decision) -> String {
    let name = d.lead().map(|s| s.label()).unwrap_or("");
    match (d.space.is_empty(), name.is_empty()) {
        (false, false) => format!("{} › {name}", d.space),
        (false, true) => d.space.clone(),
        (true, _) => name.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The model, as this panel sees it.

impl Panel {
    /// Every decision, in queue order (none while the tower is off). What
    /// was dismissed or merged here is gone at once, and an answer given
    /// here counts as going in, before the daemon says so.
    pub(super) fn decisions(&self, cx: &App) -> Vec<Decision> {
        let t = self.tower.read(cx);
        if !t.alive {
            return vec![];
        }
        let state = t.state.as_ref().unwrap_or(&Value::Null);
        let acts = &t.acts;
        let mut all = model::decisions(state, &t.prs, &|addr| self.space_of(addr), history::now_ms());
        all.retain_mut(|d| {
            if acts.dismissed.contains(&d.id) || d.question_id.as_ref().is_some_and(|q| acts.dismissed.contains(q)) {
                return false;
            }
            // Put on hold here a moment ago: out of the queue already.
            if self.hold.holding(&d.id) {
                return false;
            }
            if d.kind == Kind::Merge && d.prs.iter().any(|p| acts.merged.contains(&p.key())) {
                d.prs.retain(|p| !acts.merged.contains(&p.key()));
                if d.prs.is_empty() {
                    return false;
                }
                d.title = model::merge_title(d.prs.len());
            }
            let sending = acts.sent.contains(&d.id) || d.question_id.as_ref().is_some_and(|q| acts.sent.contains(q));
            if d.state == Stage::Open && sending {
                d.state = Stage::Answering;
            }
            true
        });
        all
    }

    /// The spaces as missions, with what each needs and its switches as
    /// they stand (what was just set here counts before the state says so).
    pub(super) fn missions(&self, all: &[Decision], cx: &App) -> Missions {
        let t = self.tower.read(cx);
        let known = crate::spaces::known(&self.live, &self.entries, &t.spaces);
        // While the tower is off its last word on the missions is stale:
        // Herdr's own spaces and `spaces.json` are read instead.
        let state = t.state.as_ref().filter(|_| t.alive).unwrap_or(&Value::Null);
        let mut m = model::missions(state, &t.spaces, &self.live, &known, all);
        m.mark(&t.follows, &model::muted_spaces(all, state, &t.mutes, &t.marks), &t.marks);
        m
    }

    /// The question set behind a decision, if it has one that is still open.
    pub(super) fn set_of(&self, d: &Decision, cx: &App) -> Option<QuestionSet> {
        let id = d.question_id.as_ref()?;
        self.open_sets(cx).into_iter().find(|s| &s.id == id)
    }

    pub(super) fn pick(&mut self, id: &str, cx: &mut Context<Self>) {
        self.tower.update(cx, |t, cx| {
            t.deck.pick(id);
            cx.notify();
        });
        cx.notify();
    }

    /// A decision that isn't a question set or a merge: one of its options,
    /// by its place (`tower answer <id> --choice <n>`).
    fn answer_plain(&mut self, id: &str, choice: usize, label: &str, cx: &mut Context<Self>) {
        // With what the card showed at that number: the tower refuses it if that changed meanwhile.
        self.answer_decision(id, json!({ "choice": choice, "label": label }), label, label, cx);
    }

    /// The same, in the user's own words (`tower answer <id> --other "…"`):
    /// what the card's "Other" field holds, with the files dropped on it.
    pub(super) fn answer_words(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(field) = self.asks.other_of_card(id) else { return };
        let (text, files) = super::message_parts(&field.read(cx).content());
        if text.is_empty() && files.is_empty() {
            return;
        }
        let kept: Vec<String> = files.iter().map(|p| history::keep_attachment(p).display().to_string()).collect();
        let said = text.lines().next().unwrap_or("").to_string();
        self.asks.close_other();
        // It went: what was typed for it is not kept.
        crate::unsent::clear(&crate::unsent::Key::others_of(id));
        // Words with files are for this once; plain words can be a rule.
        let answer = if kept.is_empty() { text.trim().to_string() } else { String::new() };
        self.answer_decision(id, json!({ "other": history::compose(&text, &kept) }), &format!("“{said}”"), &answer, cx);
    }

    /// Sends a decision's answer through the daemon; `said` is what its
    /// receipt names, `answer` the answer in words (what a rule would say).
    fn answer_decision(&mut self, id: &str, body: Value, said: &str, answer: &str, cx: &mut Context<Self>) {
        if !self.act(cx, |a| a.sent.insert(id.to_string())) {
            return;
        }
        let text = model::receipt_text(said);
        // "Make it a rule", where the answer may become one.
        let rule = self.decisions(cx).iter().find(|d| d.id == id).and_then(|d| model::rule_offer(d, answer));
        self.tower.update(cx, |t, cx| {
            t.deck.answered_with(id, text, rule);
            cx.notify();
        });
        let id = id.to_string();
        let task = cx.background_executor().spawn({
            let id = id.clone();
            async move { crate::answers::answer(&id, body, true) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.act(cx, |a| a.sent.remove(&id));
                if let Err(e) = result {
                    this.tower.update(cx, |t, _| t.deck.retract(&id));
                    this.status = Some((false, format!("{e:#}")));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A number key on the open card (`n`: 0 for key 1). False when the
    /// card has no such answer: the key is typed as any other.
    pub(super) fn press_number(&mut self, n: usize, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.shown_open.clone() else { return false };
        let all = self.decisions(cx);
        let Some(d) = all.iter().find(|d| d.id == id && d.yours()) else { return false };
        if d.kind == Kind::Merge && !d.prs.is_empty() {
            let Some((_, _, act)) = self.merge_acts(d, cx).into_iter().nth(n) else { return false };
            self.merge_act(d, act, cx);
            return true;
        }
        if let Some(set) = self.set_of(d, cx) {
            let Some(key) = self.ask_keys(&set).into_iter().nth(n) else { return false };
            self.ask_key(&set, key, cx);
            return true;
        }
        // Blocked on what the tower can't read: 1 shows it in Herdr, as its button does.
        if d.kind == Kind::Blocked {
            let Some(a) = d.sessions.iter().find(|s| s.waiting).or(d.sessions.first()).map(|s| s.address.clone()) else { return false };
            if n != 0 {
                return false;
            }
            self.show_in_herdr(&a, cx);
            return true;
        }
        let Some(o) = d.options.get(n) else { return false };
        self.answer_plain(&d.id, n, &o.label, cx);
        true
    }

    /// `HERDER_TOWER_ORB_KEYS` (see `crate::orb`): the card's field for its
    /// own words, opened as a click on it does, with the keyboard.
    /// `hold-<id>`: its Hold's field instead, as a click on Hold opens it.
    pub(crate) fn keys_open(&mut self, id: &str, window: &mut gpui::Window, cx: &mut Context<Self>) {
        if let Some(held) = id.strip_prefix("hold-") {
            self.keys_hold(held, window, cx);
            return;
        }
        if !self.decisions(cx).iter().any(|d| d.id == id) && !self.open_sets(cx).iter().any(|s| s.id == id) {
            eprintln!("[orb] HERDER_TOWER_ORB_KEYS: no open card {id}");
            return;
        }
        self.open_other(id, 0, "Your answer: type it, or talk (Control)", window, cx);
        eprintln!("[orb] HERDER_TOWER_ORB_KEYS: {id}: its field is open");
    }

    /// `HERDER_TOWER_ORB_KEYS`: what the card's field holds now, and whether its answer went.
    pub(crate) fn keys_report(&mut self, id: &str, after: &str, cx: &mut Context<Self>) {
        if let Some(held) = id.strip_prefix("hold-") {
            let (field, on_hold) = self.hold_report(held, cx);
            eprintln!("[orb] HERDER_TOWER_ORB_KEYS: {id} after {after}: field {:?}, on hold: {on_hold}", field.as_deref().unwrap_or("(closed)"));
            return;
        }
        let field = self.asks.other_of_card(id).map(|f| f.read(cx).value().to_string());
        let sent = self.tower.read(cx).acts.sent.contains(id);
        eprintln!("[orb] HERDER_TOWER_ORB_KEYS: {id} after {after}: field {:?}, answer sent: {sent}", field.as_deref().unwrap_or("(closed)"));
    }

    /// `HERDER_TOWER_ORB_ANSWER` (see `crate::orb`): answers the card `id`
    /// with its option `choice` as a click on it does; `rule`: then takes its
    /// receipt's "Make it a rule", two seconds after.
    pub(crate) fn answer_from_start(&mut self, id: &str, choice: usize, rule: bool, cx: &mut Context<Self>) {
        let Some(d) = self.decisions(cx).into_iter().find(|d| d.id == id && d.yours()) else {
            eprintln!("[orb] HERDER_TOWER_ORB_ANSWER: no open card {id}");
            return;
        };
        // A merge card: its button `choice` (the first asks about a PR; nothing merges before Confirm).
        if d.kind == Kind::Merge && !d.prs.is_empty() {
            match self.merge_acts(&d, cx).into_iter().nth(choice) {
                Some((_, _, act)) => self.merge_act(&d, act, cx),
                None => eprintln!("[orb] HERDER_TOWER_ORB_ANSWER: {id} has no button {choice}"),
            }
            return;
        }
        let Some(o) = d.options.get(choice) else {
            eprintln!("[orb] HERDER_TOWER_ORB_ANSWER: {id} has no option {choice}");
            return;
        };
        self.answer_plain(&d.id, choice, &o.label.clone(), cx);
        if rule {
            let id = d.id.clone();
            cx.spawn(async move |this, cx| {
                // Its receipt shows once the answer has landed.
                for _ in 0..20 {
                    cx.background_executor().timer(Duration::from_millis(500)).await;
                    let taken = this.update(cx, |this, cx| {
                        let offer = this.tower.read(cx).deck.receipts(history::now_ms()).into_iter().find(|r| r.id == id).and_then(|r| r.rule.clone());
                        match offer {
                            Some(offer) => {
                                this.make_rule(&id, offer, cx);
                                true
                            }
                            None => false,
                        }
                    });
                    if taken.unwrap_or(true) {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    /// "Make it a rule" on a receipt: asks the tower of that space, as a
    /// typed request would, to make the answer a standing rule. The tower's
    /// own behavior flow does the rest (it shows what changed, and can be undone).
    fn make_rule(&mut self, id: &str, offer: model::RuleOffer, cx: &mut Context<Self>) {
        let now = history::now_ms();
        self.tower.update(cx, |t, cx| {
            t.deck.rule_asked(id, now);
            cx.notify();
        });
        let task = cx.background_executor().spawn(async move { super::cli::ask(offer.space.as_deref(), &offer.text) });
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                let _ = this.update(cx, |this, cx| {
                    this.status = Some((false, e));
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// Undo on a reply's receipt (`tower undo <id>`; `force`: though work
    /// would be lost). A refusal shows why, with "Undo anyway".
    fn undo_act(&mut self, id: &str, force: bool, cx: &mut Context<Self>) {
        if self.undoing.is_some() {
            return;
        }
        self.undoing = Some(id.to_string());
        self.undo_note = None;
        let act = id.to_string();
        let task = cx.background_executor().spawn({
            let act = act.clone();
            async move {
                let mut args = vec!["undo", act.as_str(), "--json"];
                if force {
                    args.push("--force");
                }
                super::cli::tower(&args)
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.undoing = None;
                if let Err(e) = result {
                    this.undo_note = Some((act, undo_refusal(&e)));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// While a receipt shows: draw again soon, so it goes on time.
    fn tick_receipts(&mut self, cx: &mut Context<Self>) {
        if self.receipt_tick.is_some() {
            return;
        }
        self.receipt_tick = Some(cx.spawn(async move |this, cx: &mut AsyncApp| {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let _ = this.update(cx, |this, cx| {
                this.receipt_tick = None;
                cx.notify();
            });
        }));
    }
}

// ---------------------------------------------------------------------------
// Drawing it.

impl Panel {
    /// How things stand, in a line: what is the user's (in amber) and how
    /// many spaces work; or that the tower is off, starting or listening.
    pub(super) fn status_el(&self, all: &[Decision], missions: &Missions, cx: &App) -> AnyElement {
        let t = self.tower.read(cx);
        let phase = if t.starting.is_some() { "starting" } else { t.phase() };
        match phase {
            "off" => div().child("Off").into_any_element(),
            "starting" => div().child("Starting…").into_any_element(),
            "listening" | "armed" => div().child("Listening…").into_any_element(),
            _ => {
                let (mine, rest) = model::status_line(model::yours(&model::main_queue(all, &missions.muted())), missions.working());
                h_flex()
                    .gap(px(5.0))
                    .children(mine.map(|n| div().flex_none().font_weight(FontWeight::SEMIBOLD).text_color(rgb(AMBER)).child(n)))
                    .child(div().min_w_0().truncate().child(rest))
                    .into_any_element()
            }
        }
    }

    /// "Tower", Mute and Full window, and the status line.
    pub(super) fn home_header(&mut self, all: &[Decision], missions: &Missions, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let status = self.status_el(all, missions, cx);
        let t = self.tower.read(cx);
        let phase = if t.starting.is_some() { "starting" } else { t.phase() };
        // What hovering the orb said, without what the status line says already.
        let mut glance = t.glance();
        if matches!(phase, "off" | "starting" | "listening" | "armed" | "idle") && !glance.is_empty() {
            glance.remove(0);
        }
        let (alive, muted, open) = (t.alive, t.muted(), t.full.is_some() && !t.full_hidden);
        v_flex()
            .px(px(20.0))
            .pt(px(20.0))
            .pb(px(10.0))
            .gap(px(3.0))
            .child(
                h_flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().flex_1().text_size(px(20.0)).font_weight(FontWeight::SEMIBOLD).child("Tower"))
                    // The tower-wide mute (`tower mute`): nothing is said, on the Mac or a phone.
                    .when(alive, |el| {
                        el.child(tg("mute", if muted { "Muted" } else { "Mute" }, muted).on_click(cx.listener(|_, _, _, cx| crate::orb::toggle_mute(cx))))
                    })
                    .child(tg("full-window", "Full window", open).on_click(cx.listener(|this, _, window, cx| {
                        let beside = crate::orb::mac::Win::of(window).map(|w| w.frame());
                        crate::orb::toggle_full(&this.tower.clone(), beside, cx);
                    }))),
            )
            .child(div().text_sm().text_color(g.muted).child(status))
            .when(!glance.is_empty(), |el| el.child(div().text_size(px(12.0)).text_color(g.faint).truncate().child(glance.join(" · "))))
            .children(self.phone_hint(cx))
            .into_any_element()
    }

    /// A queue as its cards: what was just answered (for a moment), the
    /// answers on their way into a session, the open card (tagged `Next` or
    /// `Picked` when `tagged`), and the rest as rows under `rest_label`.
    /// `empty` is said when there is nothing. The open card is the one the
    /// number keys answer.
    pub(super) fn queue_items(
        &mut self,
        queue: &[Decision],
        tagged: bool,
        rest_label: &dyn Fn(usize) -> String,
        empty: &str,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let g = glass();
        let now = history::now_ms();
        let (focus, receipts): (Option<String>, Vec<model::Receipt>) = {
            let deck = &self.tower.read(cx).deck;
            (deck.focus.clone(), deck.receipts(now).into_iter().cloned().collect())
        };
        let open = model::open_card(queue, focus.as_deref()).cloned();
        self.shown_open = open.as_ref().map(|d| d.id.clone());
        // Since when it shows just these answers (see `settled`).
        let now_face = open.as_ref().map(face);
        if self.shown_face.as_ref().map(|(f, _)| f) != now_face.as_ref() {
            self.shown_face = now_face.map(|f| (f, std::time::Instant::now()));
        }
        let mut items: Vec<AnyElement> = vec![];
        if !receipts.is_empty() {
            self.tick_receipts(cx);
        }
        // What was just answered, for a moment; with "Make it a rule" where the answer may become one.
        for r in receipts {
            let rule = r.rule.clone().map(|offer| {
                let id = r.id.clone();
                div()
                    .id(SharedString::from(format!("rule-{}", r.id)))
                    .flex_none()
                    .px(px(10.0))
                    .py(px(2.0))
                    .rounded_full()
                    .border_1()
                    .border_color(rgba(LINE))
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(g.text)
                    .hover(|s| s.bg(rgba(CARD)))
                    .child("Make it a rule")
                    .on_click(cx.listener(move |this, _, _, cx| this.make_rule(&id, offer.clone(), cx)))
            });
            items.push(
                h_flex()
                    .w_full()
                    .gap(px(10.0))
                    .items_center()
                    .px(px(14.0))
                    .py(px(11.0))
                    .rounded(px(18.0))
                    .bg(rgba(SOFT))
                    .border_1()
                    .border_color(rgba(LINE))
                    .text_size(px(13.0))
                    .text_color(g.muted)
                    .child(div().flex_none().font_weight(FontWeight::BOLD).text_color(rgb(CYAN)).child("✓"))
                    .child(div().flex_1().min_w_0().child(r.text.clone()))
                    .children(rule)
                    .into_any_element(),
            );
        }
        // Answers on their way into a session.
        for d in queue.iter().filter(|d| d.state == Stage::Answering) {
            items.push(self.sending_card(d, cx));
        }
        if let Some(d) = &open {
            let tag = if tagged { model::tag(queue, d) } else { "" };
            items.push(self.decision_card(d, tag, now, cx));
            let rest: Vec<&Decision> = queue.iter().filter(|x| x.yours() && x.id != d.id).collect();
            if !rest.is_empty() {
                items.push(bay_label(&rest_label(rest.len())).pt(px(6.0)).into_any_element());
            }
            for r in rest {
                let id = r.id.clone();
                let mut sub = vec![];
                if !r.space.is_empty() {
                    sub.push(r.space.clone());
                }
                sub.push(r.stake().to_lowercase());
                sub.push(r.waited(now));
                // Its answer stopped half way: it is the user's again.
                if self.set_of(r, cx).is_some_and(|s| s.status == "failed") {
                    sub.push("stopped".into());
                }
                items.push(
                    v_flex()
                        .id(SharedString::from(format!("queue-{id}")))
                        .w_full()
                        .gap(px(2.0))
                        .px(px(14.0))
                        .py(px(10.0))
                        .rounded(px(14.0))
                        .bg(rgba(SOFT))
                        .border_1()
                        .border_color(rgba(LINE))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgba(CARD)))
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).line_clamp(2).child(r.title.clone()))
                        .child(div().text_size(px(13.0)).text_color(g.muted).child(sub.join(" · ")))
                        .on_click(cx.listener(move |this, _, _, cx| this.pick(&id, cx)))
                        .into_any_element(),
                );
            }
        } else if items.is_empty() {
            items.push(empty_box(empty));
        }
        items
    }

    /// Where the queue would be while the tower is off: that it is, and a
    /// way to start it. `None` while it runs.
    pub(super) fn off_items(&mut self, cx: &mut Context<Self>) -> Option<Vec<AnyElement>> {
        let (alive, starting) = {
            let t = self.tower.read(cx);
            (t.alive, t.starting.is_some())
        };
        if alive {
            return None;
        }
        (self.shown_open, self.shown_face) = (None, None);
        // Nothing is asked, and nothing is lost.
        let mut items = vec![empty_box(if starting { "The tower is starting…" } else { "The tower is off. Your sessions keep running." })];
        if !starting {
            items.push(
                acts_row()
                    .child(mbtn("start-tower", "Start the tower", true, None).on_click(cx.listener(|this, _, _, cx| Tower::start(&this.tower, cx))))
                    .into_any_element(),
            );
        }
        Some(items)
    }

    /// The queue and the spaces.
    pub(super) fn home_view(&mut self, all: &[Decision], missions: &Missions, cx: &mut Context<Self>) -> AnyElement {
        let queue = model::main_queue(all, &missions.muted());
        let items = match self.off_items(cx) {
            Some(off) => off,
            None => self.queue_items(&queue, true, &|n| format!("{n} more waiting"), "Nothing needs you.", cx),
        };
        let spaces = self.space_chips(missions, cx);
        div()
            .id("home")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(v_flex().w_full().px(px(16.0)).pb(px(8.0)).gap(px(16.0)).child(v_flex().gap(px(10.0)).children(items)).children(spaces))
            .into_any_element()
    }

    /// The open spaces, each with how it stands; a chip opens its space in
    /// the full window.
    fn space_chips(&mut self, missions: &Missions, cx: &mut Context<Self>) -> Option<AnyElement> {
        let g = glass();
        if missions.open.is_empty() && missions.quiet.is_empty() {
            return None;
        }
        let mut row = h_flex().flex_wrap().gap(px(6.0)).items_center();
        for (i, (m, chip)) in missions.chips().into_iter().enumerate() {
            let name = m.name.clone();
            row = row.child(
                h_flex()
                    .id(("space-chip", i))
                    .max_w_full()
                    .gap(px(6.0))
                    .items_center()
                    .pl(px(5.0))
                    .pr(px(11.0))
                    .py(px(3.0))
                    .rounded_full()
                    .border_1()
                    .border_color(rgba(LINE))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(SOFT)))
                    .text_size(px(12.5))
                    .child(sign(chip, 18.0, format!("space-pulse-{i}")))
                    .child(div().min_w_0().truncate().child(m.name.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| this.show_full(Scope::Space(name.clone()), window, cx))),
            );
        }
        if !missions.quiet.is_empty() {
            row = row.child(
                div()
                    .id("quiet-spaces")
                    .pl(px(4.0))
                    .text_size(px(12.5))
                    .text_color(g.faint)
                    .cursor_pointer()
                    .hover(|s| s.text_color(g.muted))
                    .child(format!("+ {} quiet", missions.quiet.len()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.tower.update(cx, |t, _| t.nav.quiet_open = true);
                        this.show_full(Scope::All, window, cx);
                    })),
            );
        }
        Some(v_flex().gap(px(10.0)).child(bay_label("Spaces")).child(row).into_any_element())
    }

    /// Above the input: the reply of the tower that was last asked (the
    /// main one, or `space`'s own), the line of what the tower is doing
    /// about it, and a message that didn't go. The whole reply is in the
    /// record: a click on it leads there.
    pub(super) fn foot_items(&mut self, space: Option<&str>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let now = history::now_ms();
        let mut items: Vec<AnyElement> = vec![];
        let (closed, acts) = {
            let t = self.tower.read(cx);
            (t.deck.reply_closed, t.state.as_ref().and_then(|s| s["acts"].as_array().cloned()).unwrap_or_default())
        };
        if let Some(r) = last_reply_with(&self.entries, now, space, &acts).filter(|r| r.at != closed) {
            let at = r.at;
            let receipt = r.act.clone().map(|a| self.act_items(&a, cx));
            // A receipt is the reply; the tower's own words follow it, smaller.
            let (has_act, undone) = (r.act.is_some(), r.act.as_ref().is_some_and(|a| a.undone));
            let to = match space {
                Some(s) if self.space_tower_of(s, cx) => format!("You, to the {s} tower: {}", r.asked),
                Some(s) => format!("You, about {s}: {}", r.asked),
                None => format!("You, to the main tower: {}", r.asked),
            };
            items.push(
                v_flex()
                    .w_full()
                    .px(px(14.0))
                    .py(px(12.0))
                    .gap(px(7.0))
                    .rounded(px(16.0))
                    .bg(rgba(0xffffff2b))
                    .border_1()
                    .border_color(rgba(LINE))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_start()
                            .child(div().flex_1().min_w_0().text_size(px(12.0)).text_color(g.faint).line_clamp(2).child(to))
                            .child(dismiss_mark("reply-close").on_click(cx.listener(move |this, _, _, cx| {
                                this.tower.update(cx, |t, cx| {
                                    t.deck.reply_closed = at;
                                    cx.notify();
                                });
                                cx.notify();
                            }))),
                    )
                    .children(receipt.into_iter().flatten())
                    .children(r.reply.filter(|_| !undone).map(|text| {
                        div()
                            .id("reply-text")
                            .when(!has_act, |el| el.text_sm().line_height(px(20.0)).line_clamp(6))
                            .when(has_act, |el| el.text_size(px(12.5)).line_height(px(18.0)).line_clamp(3).text_color(g.muted))
                            .cursor_pointer()
                            .child(text)
                            .on_click(cx.listener(|this, _, window, cx| this.show_full(Scope::Record, window, cx)))
                    }))
                    .into_any_element(),
            );
        }
        // While the brain works: one line of what it is doing, until its answer is in.
        if let Some(w) = self.tower.read(cx).work.clone().filter(|_| space.is_none()) {
            let answered = self.entries.iter().rev().take(20).any(|e| e.kind == "answer" && e.at >= w.since_ms);
            if !answered && now.saturating_sub(w.since_ms) < 30 * 60 * 1000 {
                let (word, detail, elapsed) = working_words(&w, now);
                let line = if detail.is_empty() {
                    format!("{word}… · {}", span(elapsed))
                } else {
                    format!("{word} {detail} · {}", span(elapsed))
                };
                items.push(div().px(px(4.0)).child(cstat_busy(line)).into_any_element());
            }
        }
        if self.sending {
            items.push(div().px(px(4.0)).child(cstat("Sending…", false)).into_any_element());
        }
        if let Some((ok, msg)) = self.status.clone() {
            items.push(div().px(px(4.0)).child(cstat(msg, !ok)).into_any_element());
        }
        items
    }

    /// A reply's receipt: what the tower opened, what it assumed as chips,
    /// and Undo while that is still possible; once undone, how it closed.
    pub(super) fn act_items(&mut self, a: &ActShown, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if a.undone {
            let (head, rest) = closing_parts(a.closing.as_deref().unwrap_or("Closed it."));
            return vec![
                h_flex()
                    .flex_wrap()
                    .gap_x(px(5.0))
                    .text_sm()
                    .line_height(px(20.0))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(head))
                    .when(!rest.is_empty(), |el| el.child(div().min_w_0().child(rest)))
                    .into_any_element(),
            ];
        }
        let mut items = vec![div().text_sm().line_height(px(20.0)).font_weight(FontWeight::SEMIBOLD).child(a.line.clone()).into_any_element()];
        items.extend(self.act_undo(a, cx));
        items
    }

    /// Undo for what a launch from the full window opened, under what the
    /// tower said of it there: what was assumed and Undo, as a reply's
    /// receipt has them; once undone, how it closed.
    pub(super) fn receipt_undo(&mut self, a: &ActShown, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if a.undone {
            return vec![cstat(a.closing.clone().unwrap_or_else(|| "Closed it.".into()), false)];
        }
        self.act_undo(a, cx)
    }

    /// What an act assumed as chips, with Undo ("Undo anyway" and why, where work would be lost).
    fn act_undo(&mut self, a: &ActShown, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let mut items = vec![];
        let busy = self.undoing.as_deref() == Some(a.id.as_str());
        let refused = self.undo_note.as_ref().filter(|(id, _)| id == &a.id).map(|(_, why)| why.clone());
        // Work would be lost: said before the tap, or by the refusal after it.
        let held = refused.clone().or_else(|| a.why.clone().filter(|_| !a.can_undo));
        let mut row = h_flex().flex_wrap().gap(px(6.0)).items_center();
        for (i, (value, why)) in a.chips.iter().enumerate() {
            row = row.child(
                div()
                    .id(("assumed", i))
                    .flex_none()
                    .max_w_full()
                    .px(px(9.0))
                    .py(px(2.0))
                    .rounded_full()
                    .border_1()
                    .border_color(rgba(LINE))
                    .text_size(px(12.0))
                    .text_color(g.muted)
                    .truncate()
                    .child(value.clone())
                    .when(!why.is_empty(), |el| {
                        let why = why.clone();
                        el.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(why.clone()).build(window, cx))
                    }),
            );
        }
        let id = a.id.clone();
        row = row.child(if busy {
            cstat_busy("Undoing…")
        } else if held.is_some() {
            mbtn(format!("undo-{id}"), "Undo anyway", false, None)
                .px(px(12.0))
                .py(px(4.0))
                .text_size(px(12.0))
                .on_click(cx.listener(move |this, _, _, cx| this.undo_act(&id, true, cx)))
                .into_any_element()
        } else {
            mbtn(format!("undo-{id}"), "Undo", false, None)
                .px(px(12.0))
                .py(px(4.0))
                .text_size(px(12.0))
                .on_click(cx.listener(move |this, _, _, cx| this.undo_act(&id, false, cx)))
                .into_any_element()
        });
        items.push(row.into_any_element());
        if let Some(why) = held {
            items.push(cstat(why, refused.is_some()));
        }
        items
    }

    /// The same, as the floating panel has it above its input.
    pub(super) fn home_foot(&mut self, space: Option<&str>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let items = self.foot_items(space, cx);
        (!items.is_empty()).then(|| v_flex().px(px(16.0)).pt(px(8.0)).gap(px(8.0)).children(items).into_any_element())
    }

    /// The open card, by what the decision is.
    /// Back from hold: why, above it; its Hold open: the field under it.
    fn decision_card(&mut self, d: &Decision, tag: &str, now: u64, cx: &mut Context<Self>) -> AnyElement {
        let card = if d.kind == Kind::Merge && !d.prs.is_empty() {
            self.merge_card(d, tag, now, cx)
        } else {
            match self.set_of(d, cx) {
                Some(set) => self.ask_card(d, &set, tag, now, cx),
                None => self.plain_card(d, tag, now, cx),
            }
        };
        let back = Panel::back_line(d);
        let strip = self.hold_strip(d, cx);
        if back.is_none() && strip.is_none() {
            return card;
        }
        v_flex().w_full().gap(px(8.0)).children(back.map(|b| div().px(px(4.0)).child(b))).child(card).children(strip).into_any_element()
    }

    /// A decision with no dialog behind it (raised by the tower, a task that
    /// failed twice): its line, who it is about, its answers as buttons, and
    /// "Other" for the user's own words (the only answer when it offers none).
    fn plain_card(&mut self, d: &Decision, tag: &str, now: u64, cx: &mut Context<Self>) -> AnyElement {
        if d.kind == Kind::Blocked {
            return self.blocked_card(d, tag, now, cx);
        }
        let g = glass();
        let id = d.id.clone();
        // What was typed for it before (a restart, a window closed since).
        self.hydrate_card(&id);
        let kept = self.other_kept(&id);
        let mut acts = acts_row();
        for (i, o) in d.options.iter().enumerate() {
            let (id2, label) = (d.id.clone(), o.label.clone());
            acts = acts.child(
                mbtn(format!("act-{id}-{i}"), &o.label, o.pick, (i < 9).then_some(i))
                    .on_click(cx.listener(move |this, _, _, cx| this.answer_plain(&id2, i, &label, cx))),
            );
        }
        // Who it is about: its sessions, those that wait first.
        let mut who: Vec<&str> = d.sessions.iter().filter(|s| s.waiting).map(|s| s.label()).collect();
        who.extend(d.sessions.iter().filter(|s| !s.waiting).map(|s| s.label()));
        // Your own words.
        let other: AnyElement = match self.asks.other_of_card(&id) {
            None => {
                let id2 = id.clone();
                div()
                    .id(SharedString::from(format!("other-{id}")))
                    .px(px(12.0))
                    .py(px(9.0))
                    .rounded(px(12.0))
                    .bg(g.pill)
                    .cursor_pointer()
                    .text_size(px(13.0))
                    .text_color(if kept.is_some() { g.pill_text } else { g.pill_muted })
                    .truncate()
                    // Words typed and not sent: they show, and come back into the field.
                    .child(match kept.clone() {
                        Some(words) => words,
                        None if d.options.is_empty() => "Answer in your own words…".to_string(),
                        None => "Other… in your own words".to_string(),
                    })
                    .on_click(cx.listener(move |this, _, window, cx| this.open_other(&id2, 0, "Your answer: type it, or talk (Control)", window, cx)))
                    .into_any_element()
            }
            Some(field) => {
                let id2 = id.clone();
                v_flex()
                    .gap(px(8.0))
                    .child(
                        div().px_2().py_1().rounded(px(12.0)).bg(g.pill).text_color(g.pill_text).child(
                            gpui_kit::component::input::Textarea::new(&field).appearance(false).bordered(false).small().w_full().text_size(px(14.0)).text_color(g.pill_text),
                        ),
                    )
                    .child(acts_row().child(mbtn(format!("send-{id}"), "Answer", true, None).on_click(cx.listener(move |this, _, _, cx| this.answer_words(&id2, cx)))))
                    .into_any_element()
            }
        };
        let d2 = d.clone();
        card_box(true)
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(meta_row(Some(tag), &d.space, &d.badge(), format!("{} · {}", d.stake(), d.waited(now))).flex_1().min_w_0())
                    .child(self.hold_mark(d, cx))
                    .child(dismiss_mark(format!("dismiss-{id}")).on_click(cx.listener(move |this, _, _, cx| this.dismiss_card(&d2, cx)))),
            )
            .child(card_title(&d.title))
            .when(!who.is_empty(), |el| el.child(div().text_size(px(12.0)).text_color(g.faint).truncate().child(who.join(", "))))
            .children(d.reason().map(|why| cstat(why, false)))
            // What it is about (the command, the plan), as a question's card shows its dialog's.
            .when(!d.detail.trim().is_empty(), |el| el.child(detail_block(&d.detail, 6)))
            .children(d.options.iter().find(|o| o.pick && !o.why.is_empty()).map(|o| cstat(format!("The tower would pick {}: {}", o.label, o.why), false)))
            .when(!d.options.is_empty(), |el| el.child(acts))
            .child(other)
            .into_any_element()
    }

    /// A session blocked on what the tower can't read: what its terminal
    /// says, who it is, and Herdr one tap away (nothing typed here can reach
    /// it). The card goes by itself once the session goes on.
    fn blocked_card(&mut self, d: &Decision, tag: &str, now: u64, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let id = d.id.clone();
        let who: Vec<&str> = d.sessions.iter().map(|s| s.label()).collect();
        let address = d.sessions.iter().find(|s| s.waiting).or(d.sessions.first()).map(|s| s.address.clone());
        let mut acts = acts_row();
        if let Some(a) = address {
            acts = acts.child(mbtn(format!("show-{id}"), "Show in Herdr", true, Some(0)).on_click(cx.listener(move |this, _, _, cx| this.show_in_herdr(&a, cx))));
        }
        let d2 = d.clone();
        card_box(true)
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(meta_row(Some(tag), &d.space, &d.badge(), format!("{} · {}", d.stake(), d.waited(now))).flex_1().min_w_0())
                    .child(self.hold_mark(d, cx))
                    .child(dismiss_mark(format!("dismiss-{id}")).on_click(cx.listener(move |this, _, _, cx| this.dismiss_card(&d2, cx)))),
            )
            .child(card_title(&d.title))
            .when(!who.is_empty(), |el| el.child(div().text_size(px(12.0)).text_color(g.faint).truncate().child(who.join(", "))))
            .child(cstat("The tower can't read what it shows: answer it in the session.", false))
            .child(acts)
            .into_any_element()
    }

    /// An answer on its way into a session (C4): what is being sent, and where.
    fn sending_card(&mut self, d: &Decision, cx: &mut Context<Self>) -> AnyElement {
        let set = self.set_of(d, cx);
        let n = set.as_ref().map(|s| s.tabs.len()).unwrap_or(1);
        let title = match n {
            0 | 1 => "Sending your answer".to_string(),
            n => format!("Sending your {n} answers"),
        };
        let place = whereabouts(d);
        let line = if set.as_ref().is_some_and(|s| s.status == "queued") {
            "Waiting for the session to stop; then it goes in.".to_string()
        } else if place.is_empty() {
            "Typing it into the session…".to_string()
        } else {
            format!("Typing it into {place}…")
        };
        card_box(false)
            .child(meta_row(None, &d.space, &d.badge(), "answering".into()))
            .child(card_title(&title))
            .child(cstat_busy(line))
            .into_any_element()
    }
}

/// "Closed it." and the rest of an undo's closing line.
fn closing_parts(closing: &str) -> (String, String) {
    match closing.split_once(". ") {
        Some((head, rest)) => (format!("{head}."), rest.trim().to_string()),
        None => (closing.trim().to_string(), String::new()),
    }
}

/// Why `tower undo` refused, without the command's name in front and
/// without how to force it (the button says that).
fn undo_refusal(error: &str) -> String {
    let e = error.strip_prefix("tower undo: ").unwrap_or(error);
    let e = e.split(" Undo it anyway with --force").next().unwrap_or(e).trim();
    let e = e.strip_prefix("not undone: ").unwrap_or(e);
    let mut c = e.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// "Nothing needs you.": a dashed box where the queue would be.
pub(super) fn empty_box(text: &str) -> AnyElement {
    div()
        .w_full()
        .p(px(14.0))
        .rounded(px(18.0))
        .border_1()
        .border_dashed()
        .border_color(rgba(LINE))
        .text_sm()
        .text_color(glass().muted)
        .child(text.to_string())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orb::model::SessionRef;

    #[test]
    fn a_bare_digit_answers_the_open_card() {
        assert_eq!(answer_key("1", false, true, false), Some(0));
        assert_eq!(answer_key("9", false, true, false), Some(8));
        assert_eq!((answer_key("۲", false, true, false), answer_key("٣", false, true, false)), (Some(1), Some(2)));
        // Nothing to answer with: zero, letters, keys with names.
        for key in ["0", "a", "enter", "escape", "10", ""] {
            assert_eq!(answer_key(key, false, true, false), None, "{key:?}");
        }
        // A digit with a modifier is a shortcut or another character.
        assert_eq!(answer_key("1", true, true, false), None);
        // A message under way, or a typed answer: the digit is text.
        assert_eq!(answer_key("2", false, false, false), None);
        assert_eq!(answer_key("2", false, true, true), None);
    }

    #[test]
    fn a_number_is_for_the_card_that_has_been_showing() {
        use crate::orb::model::Opt;
        let card = |id: &str, set: &str, labels: &[&str]| Decision {
            id: id.into(),
            question_id: Some(set.into()),
            options: labels.iter().map(|l| Opt { label: l.to_string(), ..Default::default() }).collect(),
            ..Default::default()
        };
        let a = card("d1", "qa", &["Keep the table", "Drop the table"]);
        // The same card drawn again is the same face: a redraw doesn't hold the keys back.
        assert_eq!(face(&a), face(&a.clone()));
        // Another card in its place, the same decision showing another session's question, or its
        // answers in another order: what "1" means has changed, and the keys wait for the eye.
        assert_ne!(face(&a), face(&card("d2", "qa", &["Keep the table", "Drop the table"])));
        assert_ne!(face(&a), face(&card("d1", "qb", &["Keep the table", "Drop the table"])));
        assert_ne!(face(&a), face(&card("d1", "qa", &["Drop the table", "Keep the table"])));
        // A key that comes right behind the change is for what was there before; half a second on, it counts.
        assert!(!settled(0) && !settled(KEY_GAP_MS - 1) && settled(KEY_GAP_MS) && settled(5_000));
    }

    fn entry(kind: &str, text: &str, at: u64) -> Entry {
        Entry { kind: kind.into(), text: text.into(), at, ..Default::default() }
    }

    #[test]
    fn the_reply_to_what_you_last_asked_shows_while_it_is_news() {
        let min = 60_000;
        let entries = vec![
            entry("request", "what was that?", 1 * min),
            entry("answer", "An old answer.", 2 * min),
            entry("request", " what’s left? ", 20 * min),
            entry("watch", "t3 finished its turn", 21 * min),
            entry("answer", "3 need you, 2 working.", 22 * min),
            entry("answer", "Later news the tower told by itself.", 25 * min),
        ];
        let r = last_reply(&entries, 23 * min, None).expect("fresh");
        assert_eq!((r.asked.as_str(), r.reply.as_deref(), r.at), ("what’s left?", Some("3 need you, 2 working."), 20 * min));
        // Ten minutes after the reply it is the record's, not the panel's.
        assert!(last_reply(&entries, 31 * min, None).is_some());
        assert!(last_reply(&entries, 32 * min, None).is_none());
        // Asked, not answered yet: the question shows, the working line says the rest.
        let asked = vec![entry("answer", "Before.", min), entry("request", "open a session", 5 * min)];
        assert_eq!(last_reply(&asked, 6 * min, None), Some(Reply { asked: "open a session".into(), reply: None, at: 5 * min, act: None }));
        // News held for another space is not what you asked.
        let held = vec![Entry { held: true, ..entry("request", "held", min) }];
        assert!(last_reply(&held, min, None).is_none());
        assert!(last_reply(&[], 0, None).is_none());
        // What was asked of a space's tower shows in that space, and there only.
        let both = vec![
            entry("request", "what's left?", 1 * min),
            Entry { space: Some("isa".into()), ..entry("request", "what's left here?", 2 * min) },
            entry("answer", "3 need you.", 3 * min),
            Entry { space: Some("isa".into()), ..entry("answer", "1 needs you here.", 4 * min) },
        ];
        let main = last_reply(&both, 5 * min, None).unwrap();
        assert_eq!((main.asked.as_str(), main.reply.as_deref()), ("what's left?", Some("3 need you.")));
        let isa = last_reply(&both, 5 * min, Some("isa")).unwrap();
        assert_eq!((isa.asked.as_str(), isa.reply.as_deref()), ("what's left here?", Some("1 needs you here.")));
        assert!(last_reply(&both, 5 * min, Some("sandbox")).is_none());
        // Only files: said so.
        let files = vec![Entry { attachments: vec!["/a/b.png".into()], ..entry("request", "", min) }];
        assert_eq!(last_reply(&files, min, None).unwrap().asked, "(files)");
    }

    #[test]
    fn what_was_only_written_down_is_not_a_reply() {
        let min = 60_000;
        // A quiet tower: the brain's note and a receipt kept for the record sit between the request and its answer.
        let entries = vec![
            entry("request", "fix the login redirect", min),
            Entry { record: true, ..entry("note", "The api session finished the login fix.", 2 * min) },
            Entry { record: true, ..entry("receipt", "The docs are updated.", 3 * min) },
            Entry { record: true, ..entry("answer", "kept", 3 * min) },
            Entry { space: Some("isa".into()), agenda: Some("a3".into()), ..entry("receipt", "The login fix is merged into dev.", 4 * min) },
        ];
        let r = last_reply(&entries, 5 * min, None).unwrap();
        assert_eq!(r.reply.as_deref(), Some("The login fix is merged into dev."), "the receipt the user was told, though it names a space");
        assert_eq!(last_reply(&entries[..4], 5 * min, None).unwrap().reply, None, "nothing told yet");
        // Typed in a space's view while spaces have no tower of their own: asked of the main one, the space named in front.
        let typed = vec![
            entry("request", "what's left?", min),
            entry("answer", "Nothing.", 2 * min),
            entry("request", "In sandbox: what did mediaroom answer?", 3 * min),
            entry("answer", "It answered on stale writes.", 4 * min),
        ];
        let here = last_reply(&typed, 5 * min, Some("sandbox")).unwrap();
        assert_eq!((here.asked.as_str(), here.reply.as_deref()), ("what did mediaroom answer?", Some("It answered on stale writes.")));
        assert!(last_reply(&typed, 5 * min, Some("isa")).is_none());
        assert_eq!(last_reply(&typed, 5 * min, None).unwrap().asked, "In sandbox: what did mediaroom answer?", "the main tower was asked it");
    }

    #[test]
    fn a_commands_reply_is_its_receipt_with_what_was_assumed() {
        use crate::acts::{Act, Asked, Assumed, Opened};
        let min = 60_000;
        let act = Act {
            id: "r0123456789ab".into(),
            kind: "start".into(),
            at_ms: 2 * min,
            by: "tower".into(),
            space: Some("M5 Pro".into()),
            sessions: vec![Opened { address: "t9".into(), name: "fix-login".into(), ..Default::default() }],
            assumed: vec![
                Assumed { what: "account".into(), value: "orca".into(), why: "what \"second\" names".into() },
                Assumed { what: "model".into(), value: "default model".into(), why: "none was asked for".into() },
                Assumed { what: "folder".into(), value: "herder-tower".into(), why: "the best match".into() },
            ],
            request: Some(Asked { at: min, text: "open a session in M5 Pro on my second account".into(), space: None }),
            state: "done".into(),
            ..Default::default()
        };
        let at = |mut e: Entry, at: u64| {
            e.at = at;
            e
        };
        let mut entries = vec![entry("request", "open a session in M5 Pro on my second account", min), at(act.entry(), 2 * min)];
        // The act is there before the tower's words: the receipt shows at once, with Undo.
        let r = last_reply_with(&entries, 2 * min, None, &[act.view()]).unwrap();
        assert_eq!(r.reply, None);
        let a = r.act.unwrap();
        assert_eq!((a.line.as_str(), a.can_undo, a.undone, a.why.clone()), ("Opened fix login in M5 Pro.", true, false, None));
        assert_eq!(a.chips.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>(), ["orca", "default model", "herder-tower"]);
        assert_eq!(a.chips[0].1, "what \"second\" names");
        // With the tower's words; fresh for ten minutes after the last of the two.
        entries.push(entry("answer", "Opened it on orca.", 3 * min));
        let r = last_reply_with(&entries, 4 * min, None, &[act.view()]).unwrap();
        assert_eq!((r.reply.as_deref(), r.act.as_ref().map(|a| a.id.as_str())), (Some("Opened it on orca."), Some("r0123456789ab")));
        // Its session was given more work: no plain Undo, and why.
        let mut held = act.clone();
        held.sessions[0].more = Some("also fix the logout redirect".into());
        let a = last_reply_with(&entries, 4 * min, None, &[held.view()]).unwrap().act.unwrap();
        assert_eq!((a.can_undo, a.why.as_deref()), (false, Some("fix-login was given more work since (\"also fix the logout redirect\")")));
        // Undone: the closing line, and nothing more to undo.
        let mut undone = act.clone();
        (undone.state, undone.closing) = ("undone".into(), Some("Closed it. Nothing else changed.".into()));
        entries.push(at(undone.entry(), 5 * min));
        let a = last_reply_with(&entries, 6 * min, None, &[undone.view()]).unwrap().act.unwrap();
        assert_eq!((a.undone, a.can_undo, a.closing.as_deref()), (true, false, Some("Closed it. Nothing else changed.")));
        assert_eq!(closing_parts("Closed it. Nothing else changed."), ("Closed it.".to_string(), "Nothing else changed.".to_string()));
        // The tower no longer holds it (an older state): what its entries say, and no Undo.
        let a = last_reply(&entries[..3], 4 * min, None).unwrap().act.unwrap();
        assert_eq!((a.line.as_str(), a.can_undo, a.undone, a.chips.len()), ("Opened fix login in M5 Pro.", false, false, 3));
        let a = last_reply(&entries, 6 * min, None).unwrap().act.unwrap();
        assert_eq!((a.undone, a.closing.as_deref(), a.line.as_str()), (true, Some("Closed it. Nothing else changed."), "Opened fix login in M5 Pro."));
        // Another request's act, and one that answered none, are not this reply's.
        let other = vec![entry("request", "what's left?", 7 * min), at(Act { request: None, ..act.clone() }.entry(), 8 * min), entry("answer", "Nothing.", 9 * min)];
        assert_eq!(last_reply_with(&other, 9 * min, None, &[act.view()]).unwrap().act, None);
        // A refusal as the reply shows it.
        assert_eq!(
            undo_refusal("tower undo: not undone: fix-login (t9) was given more work since (\"also fix it\"). Undo it anyway with --force (the sessions are ended; a worktree with changes is kept)."),
            "Fix-login (t9) was given more work since (\"also fix it\")."
        );
    }

    #[test]
    fn a_card_says_where_its_answer_goes() {
        let mut d = Decision {
            space: "isa".into(),
            sessions: vec![SessionRef::new("t3", "mediaroom")],
            ..Default::default()
        };
        assert_eq!(whereabouts(&d), "isa › mediaroom");
        // Of several, the one that waits on it.
        d.sessions.insert(0, SessionRef { waiting: false, ..SessionRef::new("t1", "eyzflow") });
        assert_eq!(whereabouts(&d), "isa › mediaroom");
        d.sessions.remove(0);
        d.sessions[0].name.clear();
        assert_eq!(whereabouts(&d), "isa › t3");
        d.space.clear();
        assert_eq!(whereabouts(&d), "t3");
        d.sessions.clear();
        assert_eq!(whereabouts(&d), "");
    }
}
