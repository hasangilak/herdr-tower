//! The conversation of a view with a tower (All spaces: the main tower; a
//! space: its own tower, or the main one asked about it), whole, in a
//! column of its own beside the sidebar, with the input under it: what you
//! asked and every answer, newest last, scrolled to the end as they come.
//! What the tower opened for the latest request (its receipt, with Undo)
//! shows under that request's answer.

use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::{h_flex, v_flex};

use super::home::{ActShown, LINE, SOFT, bay_label, cstat, cstat_busy};
use super::{AMBER, CYAN, Panel, glass};
use crate::history::{self, Entry};

/// The most messages the column shows (Record has the rest).
const MAX_SAID: usize = 200;

/// One message of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Said {
    /// You asked it (else the tower answered).
    pub you: bool,
    pub text: String,
    pub at: u64,
    /// The request it belongs to (its own time, for a request).
    pub request_at: u64,
}

/// The conversation of `space`'s view (None: the main tower's), oldest
/// first, at most `max`: each request of yours that is this view's, and the
/// answers and receipts that came for it from the tower that was asked
/// (as `home::last_reply_with` reads them). A request that is another
/// view's ends what came before; its answers aren't this view's.
pub(super) fn conversation(entries: &[Entry], space: Option<&str>, max: usize) -> Vec<Said> {
    let of = |e: &Entry| e.space.as_deref().filter(|s| !s.is_empty()).map(String::from);
    let named = space.map(|s| super::cli::in_space(s, ""));
    let mut out: Vec<Said> = vec![];
    // The tower the latest request of this view was asked of (its space), and when.
    let mut from: Option<Option<String>> = None;
    let mut request_at = 0;
    for e in entries {
        if e.record || e.held {
            continue;
        }
        if e.kind == "request" {
            let mine = match (space, of(e)) {
                (None, None) => Some(e.text.trim().to_string()),
                (Some(s), Some(es)) if es == s => Some(e.text.trim().to_string()),
                (Some(_), None) => named.as_deref().and_then(|n| e.text.trim().strip_prefix(n.trim_end())).map(|rest| rest.trim().to_string()),
                _ => None,
            };
            match mine {
                Some(text) => {
                    let text = if text.is_empty() && !e.attachments.is_empty() { "(files)".to_string() } else { text };
                    out.push(Said { you: true, text, at: e.at, request_at: e.at });
                    from = Some(of(e));
                    request_at = e.at;
                }
                None => from = None,
            }
            continue;
        }
        if matches!(e.kind.as_str(), "answer" | "receipt")
            && !e.text.trim().is_empty()
            && let Some(f) = &from
            && (of(e) == *f || (f.is_none() && e.kind == "receipt"))
        {
            out.push(Said { you: false, text: e.text.trim().to_string(), at: e.at, request_at });
        }
    }
    let n = out.len();
    out.split_off(n.saturating_sub(max))
}

/// "7:50 PM" today, else the day and the time.
fn when(at: u64) -> String {
    let day = super::day_label(at);
    if day == "Today" { super::clock(at) } else { format!("{day}, {}", super::clock(at)) }
}

impl Panel {
    /// The conversation column of `space`'s view (None: the main tower's),
    /// `width` points wide, its input at the foot.
    pub(super) fn chat_column(&mut self, space: Option<&str>, width: f32, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let said = conversation(&self.entries, space, MAX_SAID);
        // A new message: the column goes to its end.
        if let Some(f) = self.full.as_mut() {
            let key = (space.map(String::from), said.len(), said.last().map(|s| s.at).unwrap_or(0));
            if f.chat_seen.as_ref() != Some(&key) {
                f.chat_seen = Some(key);
                f.chat_scroll.scroll_to_bottom();
                window.request_animation_frame();
            }
        }
        let tower = match space {
            Some(s) if self.space_tower_of(s, cx) => format!("the {s} tower"),
            Some(s) => format!("the main tower, about {s}"),
            None => "the main tower".to_string(),
        };
        let who = match space {
            Some(s) if self.space_tower_of(s, cx) => format!("The {s} tower"),
            _ => "The tower".to_string(),
        };
        // What it opened for the latest request: under that request's last message.
        let latest = said.iter().rev().find(|s| s.you).map(|s| s.at);
        let acts = self.tower.read(cx).state.as_ref().and_then(|s| s["acts"].as_array().cloned()).unwrap_or_default();
        let act = latest.and_then(|at| {
            let opened: Vec<&Entry> = self.entries.iter().filter(|e| e.kind == "act" && !e.record && e.act.as_ref().is_some_and(|a| a["request_at"].as_u64() == Some(at))).collect();
            ActShown::of_entries(&opened, &acts)
        });
        let act_items = act.map(|a| self.act_items(&a, cx)).unwrap_or_default();
        let last_of_latest = latest.and_then(|at| said.iter().rposition(|s| s.request_at == at));
        let mut rows: Vec<AnyElement> = vec![];
        let mut act_items = Some(act_items);
        for (i, s) in said.iter().enumerate() {
            let bar = if s.you { rgb(AMBER) } else { rgb(CYAN) };
            rows.push(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap(px(10.0))
                    .p(px(12.0))
                    .rounded(px(14.0))
                    .bg(rgba(if s.you { 0xffffff24 } else { SOFT }))
                    .child(div().w(px(3.0)).flex_none().self_stretch().rounded_full().bg(bar))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(5.0))
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap(px(8.0))
                                    .child(div().flex_1().min_w_0().text_size(px(13.0)).font_weight(FontWeight::SEMIBOLD).child(if s.you { "You".to_string() } else { who.clone() }))
                                    .child(div().flex_none().text_size(px(12.0)).text_color(g.faint).child(when(s.at))),
                            )
                            .child(div().w_full().text_size(px(13.5)).line_height(px(20.0)).text_color(g.text).child(s.text.clone()))
                            .when(Some(i) == last_of_latest, |el| el.children(act_items.take().into_iter().flatten())),
                    )
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .w_full()
                    .p(px(14.0))
                    .rounded(px(14.0))
                    .border_1()
                    .border_dashed()
                    .border_color(rgba(LINE))
                    .text_size(px(13.0))
                    .line_height(px(19.0))
                    .text_color(g.muted)
                    .child(format!("Nothing asked of {tower} yet. What you write below goes there, and its answers show here."))
                    .into_any_element(),
            );
        }
        let status = self.status_items(space, cx);
        let scroll = self.full.as_ref().map(|f| f.chat_scroll.clone());
        v_flex()
            .w(px(width))
            .flex_none()
            .h_full()
            .px(px(14.0))
            .pb(px(16.0))
            .gap(px(10.0))
            .border_r_1()
            .border_color(rgba(LINE))
            .child(div().pt(px(22.0)).px(px(4.0)).child(bay_label(&format!("Conversation · {tower}"))))
            .child(
                div()
                    .id(SharedString::from(format!("chat-{}", space.unwrap_or("main"))))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when_some(scroll, |el, s| el.track_scroll(&s))
                    .child(v_flex().w_full().gap(px(8.0)).pb(px(4.0)).children(rows)),
            )
            .children(status)
            .child(self.input(window, cx))
            .into_any_element()
    }

    /// While the brain works, what it is doing; a message on its way; what
    /// the last command said (as under the floating panel's reply).
    pub(super) fn status_items(&mut self, space: Option<&str>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let now = history::now_ms();
        let mut items: Vec<AnyElement> = vec![];
        if let Some(w) = self.tower.read(cx).work.clone().filter(|_| space.is_none()) {
            let answered = self.entries.iter().rev().take(20).any(|e| e.kind == "answer" && e.at >= w.since_ms);
            if !answered && now.saturating_sub(w.since_ms) < 30 * 60 * 1000 {
                let (word, detail, elapsed) = super::working_words(&w, now);
                let line = if detail.is_empty() { format!("{word}… · {}", super::span(elapsed)) } else { format!("{word} {detail} · {}", super::span(elapsed)) };
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: &str, text: &str, space: Option<&str>, at: u64) -> Entry {
        Entry { kind: kind.into(), text: text.into(), space: space.map(String::from), at, ..Default::default() }
    }

    #[test]
    fn the_conversation_column_leaves_the_pane_room_at_every_width() {
        use super::super::window::talk_width;
        let side = 236.0;
        for w in [960.0_f32, 1000.0, 1150.0, 1400.0, 1728.0, 2400.0] {
            let t = talk_width(w);
            assert!((280.0..=460.0).contains(&t), "{w}: {t}");
            assert!(w - side - t - 36.0 >= 384.0, "{w}: the pane keeps room ({})", w - side - t);
        }
        assert!(talk_width(1728.0) > talk_width(1150.0), "it grows with the window");
    }

    #[test]
    fn a_views_conversation_is_its_requests_and_their_answers_whole() {
        let long = "word ".repeat(300);
        let entries = vec![
            e("request", "fix the login", None, 1),
            e("answer", "On it.", None, 2),
            e("request", "the session you opened is in the wrong project", Some("support-translation"), 3),
            e("answer", &long, Some("support-translation"), 4),
            e("summary", "a session's summary", Some("support-translation"), 5),
            e("request", "and the mediaroom deploy?", Some("support-translation"), 6),
            e("answer", "It works on dev.", Some("support-translation"), 7),
            e("request", "what about the sandbox?", Some("sandbox"), 8),
            e("answer", "Sandbox is idle.", Some("sandbox"), 9),
        ];
        let s = conversation(&entries, Some("support-translation"), 200);
        assert_eq!(s.iter().map(|x| (x.you, x.at)).collect::<Vec<_>>(), [(true, 3), (false, 4), (true, 6), (false, 7)]);
        assert_eq!(s[1].text, long.trim(), "an answer is whole, not cut");
        assert_eq!(s[1].request_at, 3);
        let main = conversation(&entries, None, 200);
        assert_eq!(main.iter().map(|x| x.at).collect::<Vec<_>>(), [1, 2]);
        // Asked of the main tower about a space ("In <space>: …"): that space's.
        let asked = super::super::cli::in_space("sandbox", "is it done?");
        let more = [entries.clone(), vec![e("request", &asked, None, 10), e("receipt", "Done in sandbox.", None, 11)]].concat();
        let sb = conversation(&more, Some("sandbox"), 200);
        assert_eq!(sb.iter().map(|x| (x.at, x.text.as_str())).collect::<Vec<_>>(), [(8, "what about the sandbox?"), (9, "Sandbox is idle."), (10, "is it done?"), (11, "Done in sandbox.")]);
        // At most `max`, the latest.
        assert_eq!(conversation(&entries, Some("support-translation"), 2).iter().map(|x| x.at).collect::<Vec<_>>(), [6, 7]);
    }
}
