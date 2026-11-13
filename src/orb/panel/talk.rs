//! The conversation of a view with a tower (All spaces: the main tower; a
//! space: its own tower, or the main one asked about it), whole, in a
//! column of its own beside the sidebar, with the input under it: what you
//! asked and every answer, newest last, scrolled to the end as they come.
//! What the tower opened for the latest request (its receipt, with Undo)
//! shows under that request's answer.
//!
//! A space's conversation also has what passed between you or the tower and
//! each of its sessions (`session_said`): what you typed in a session's own
//! terminal, your answers to its questions, and what the tower queued for
//! it, sent it, or didn't send because you had told it yourself; and, at
//! its end, what still waits for its sessions in the send queue. A
//! session's view has the same, for that session alone (`session_thread`).
//!
//! A space launched from New space has, at the top of its conversation,
//! the conversation that made it (`story_of`, the history's `creation`
//! entries): the ticket, the drafts and the changes asked for, the
//! planner's answers, and what the launch opened. New space's own column
//! is that conversation while it is still being had (`launch`).

use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::{h_flex, v_flex};

use super::home::{ActShown, LINE, SOFT, bay_label, cstat, cstat_busy};
use super::{AMBER, CYAN, Panel, glass};
use crate::history::{self, Entry};
use crate::launch::Told;

/// The most messages the column shows (Record has the rest).
const MAX_SAID: usize = 200;

/// One message of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Said {
    /// You asked it (else the tower answered).
    pub you: bool,
    pub text: String,
    pub at: u64,
    /// The request it belongs to (its own time, for a request; 0 for one with a session).
    pub request_at: u64,
    /// Between you or the tower and one session, not the tower's own conversation.
    pub with: Option<With>,
    /// The message it answers, by its `at` (the history's `reply_to`).
    pub reply_to: Option<u64>,
}

impl Said {
    /// A message that waits for an answer: yours (to the tower, or typed
    /// to a session), or one the tower sent a session.
    pub fn asks(&self) -> bool {
        match &self.with {
            None => self.you,
            Some(w) => self.you || matches!(w.how.as_str(), "sent" | "queued"),
        }
    }

    /// A session's reply.
    fn reply(&self) -> bool {
        self.with.as_ref().is_some_and(|w| w.how == "replied")
    }
}

/// How the rows of a conversation pair up: the row a row answers (`asks`)
/// and the first row that answers it (`answer`), by index.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct Pair {
    pub asks: Option<usize>,
    pub answer: Option<usize>,
}

/// Each row's pair, by the `reply_to` its answers carry: an answer of the
/// tower's names the request it answers, a session's reply the message
/// that started its turn. However many messages came in between.
pub(super) fn pairs(said: &[Said]) -> Vec<Pair> {
    let mut out = vec![Pair::default(); said.len()];
    for (i, s) in said.iter().enumerate() {
        let Some(at) = s.reply_to else { continue };
        if let Some(j) = said.iter().position(|q| q.at == at && q.asks()) {
            out[i].asks = Some(j);
            if out[j].answer.is_none() {
                out[j].answer = Some(i);
            }
        }
    }
    out
}

/// The line on an answer that says what it answers: "↳ you asked: …", or
/// for a message the tower sent, "↳ the tower sent it: …".
pub(super) fn asked_line(q: &Said) -> String {
    let words: String = q.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let short = if words.chars().count() > 70 { format!("{}…", words.chars().take(69).collect::<String>().trim_end()) } else { words };
    if q.you { format!("↳ you asked: {short}") } else { format!("↳ the tower sent it: {short}") }
}

/// The session a message passed to, and how it went.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct With {
    pub addr: String,
    pub name: String,
    /// "in its terminal", "answered its question", "queued", "sent", "not sent: …".
    pub how: String,
}

/// What follows the first ": " (`Queued for eyzflow: <message>`).
fn after_colon(text: &str) -> &str {
    text.split_once(": ").map(|(_, m)| m).unwrap_or(text).trim()
}

/// What `e` says passed between you or the tower and one session: what you
/// typed in its terminal, your answer to its question, what the tower
/// queued for it and sent it, or didn't send.
pub(super) fn session_said(e: &Entry) -> Option<Said> {
    let addr = e.session.clone().filter(|a| !a.is_empty())?;
    let name = e.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| addr.clone());
    let source = e.source.as_deref().unwrap_or("");
    let text = e.text.trim();
    let (you, how, said): (bool, String, String) = match (e.kind.as_str(), e.status.as_deref()) {
        ("request", Some("answer")) if source.starts_with("terminal:") => (true, "answered its question, in its terminal".into(), text.into()),
        ("request", _) if source.starts_with("terminal:") => (true, "in its terminal".into(), text.into()),
        ("request", _) if source.starts_with("answer:") => (true, "answered its question".into(), text.into()),
        ("queue", Some("queued")) => (false, "queued".into(), after_colon(text).into()),
        ("queue", Some("delivered" | "sent")) => (false, "sent".into(), after_colon(text).into()),
        // Its reply to what started its turn.
        ("reply", _) => (false, "replied".into(), text.into()),
        ("queue", Some("not-sent")) => match text.strip_prefix("Not sent: you had told it yourself (").and_then(|r| r.split_once("\"): ")) {
            Some((_, message)) => (false, "not sent: you had told it yourself".into(), message.trim().into()),
            None => (false, "not sent".into(), text.strip_prefix("Not sent: ").unwrap_or(text).into()),
        },
        _ => return None,
    };
    // Empty, or a session's own notice read as typed by an older version (a subagent's hand-back).
    if said.is_empty() || (you && (said.starts_with("<agent-message ") || said.starts_with("<task-notification>"))) {
        return None;
    }
    Some(Said { you, text: said, at: e.at, request_at: 0, with: Some(With { addr, name, how }), reply_to: e.reply_to })
}

/// `s` into `out`: what went in (or didn't) of a message the tower queued is
/// that message's row, its word changed; anything else, a row of its own.
fn push_session(out: &mut Vec<Said>, s: Said) {
    let Some(w) = &s.with else { return out.push(s) };
    if !s.you
        && w.how != "queued"
        && let Some(q) = out.iter_mut().rev().find(|q| !q.you && q.text == s.text && q.with.as_ref().is_some_and(|qw| qw.addr == w.addr && qw.how == "queued"))
    {
        if let Some(qw) = q.with.as_mut() {
            qw.how = w.how.clone();
        }
        return;
    }
    out.push(s);
}

/// Leaves out of `said` each "queued" row of the tower's that still waits
/// (`waiting`: address and text): its waiting row says where it stands.
pub(super) fn without_waiting(said: Vec<Said>, waiting: &[(String, String)]) -> Vec<Said> {
    said.into_iter()
        .filter(|s| !(s.with.as_ref().is_some_and(|w| w.how == "queued" && waiting.iter().any(|(a, t)| *a == w.addr && t.trim() == s.text))))
        .collect()
}

/// The (address, text) of each message waiting for `addr` (the agent line's `waiting`).
pub(super) fn waiting_texts(addr: &str, waiting: &[serde_json::Value]) -> Vec<(String, String)> {
    waiting.iter().filter_map(|w| Some((addr.to_string(), w["text"].as_str()?.to_string()))).collect()
}

/// What passed between you or the tower and the session `addr`, oldest
/// first, at most `max`.
pub(super) fn session_thread(entries: &[Entry], addr: &str, max: usize) -> Vec<Said> {
    let mut out = vec![];
    for e in entries.iter().filter(|e| !e.held && e.session.as_deref() == Some(addr)) {
        if let Some(s) = session_said(e) {
            push_session(&mut out, s);
        }
    }
    out.sort_by_key(|s| s.at);
    let n = out.len();
    out.split_off(n.saturating_sub(max))
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
        // What passed between you or the tower and one of the space's sessions.
        if let Some(sp) = space
            && !e.held
            && e.space.as_deref() == Some(sp)
            && let Some(s) = session_said(e)
        {
            push_session(&mut out, s);
            continue;
        }
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
                    out.push(Said { you: true, text, at: e.at, request_at: e.at, with: None, reply_to: None });
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
            out.push(Said { you: false, text: e.text.trim().to_string(), at: e.at, request_at, with: None, reply_to: e.reply_to });
        }
    }
    // In time order (a line can be written a little after a later one).
    out.sort_by_key(|s| s.at);
    let n = out.len();
    out.split_off(n.saturating_sub(max))
}

/// The conversation that made `space` (New space's, kept by its launch as
/// `creation` entries), oldest first.
pub(super) fn story_of(entries: &[Entry], space: &str) -> Vec<Told> {
    let mut out: Vec<Told> = entries
        .iter()
        .filter(|e| e.kind == "creation" && e.space.as_deref() == Some(space) && !e.text.trim().is_empty())
        .map(|e| Told { who: e.status.clone().unwrap_or_else(|| "tower".into()), text: e.text.trim().to_string(), at: e.at })
        .collect();
    out.sort_by_key(|t| t.at);
    out
}

/// A message of the conversation that makes (or made) a space: you, the
/// tower or the planner, when, its words whole.
pub(super) fn told_row(t: &Told) -> AnyElement {
    let (bar, bg, head) = match t.who.as_str() {
        "you" => (rgb(AMBER).into(), rgba(0xffffff24), "You"),
        "planner" => (faded(CYAN, 0xa0), rgba(SOFT), "The planner"),
        _ => (rgb(CYAN).into(), rgba(SOFT), "The tower"),
    };
    message_row(bar, bg, head.to_string(), if t.at > 0 { when(t.at) } else { String::new() }, None, t.text.clone(), vec![])
}

/// "7:50 PM" today, else the day and the time.
fn when(at: u64) -> String {
    let day = super::day_label(at);
    if day == "Today" { super::clock(at) } else { format!("{day}, {}", super::clock(at)) }
}

/// A color at alpha `a` (0-255).
fn faded(color: u32, a: u32) -> gpui::Rgba {
    rgba((color << 8) | (a & 0xff))
}

/// One message's row: who said it to whom, when, how it went, and its words
/// whole; `extra` under them. `who`: the tower's name in this view.
pub(super) fn said_row(s: &Said, who: &str, extra: Vec<AnyElement>) -> AnyElement {
    said_row_with(s, who, None, extra)
}

/// `said_row`, with a line on top (`top`: what it answers, as `asked_line`).
pub(super) fn said_row_with(s: &Said, who: &str, top: Option<AnyElement>, extra: Vec<AnyElement>) -> AnyElement {
    let g = glass();
    let (bar, bg) = match (&s.with, s.you) {
        (None, true) => (rgb(AMBER).into(), rgba(0xffffff24)),
        (None, false) => (rgb(CYAN).into(), rgba(SOFT)),
        (Some(_), true) => (faded(AMBER, 0x99), rgba(0xffffff12)),
        (Some(_), false) => (faded(CYAN, 0x80), rgba(0xffffff0a)),
    };
    let head = match (&s.with, s.you) {
        (None, true) => "You".to_string(),
        (None, false) => who.to_string(),
        (Some(w), true) => format!("You → {}", w.name),
        (Some(w), false) if s.reply() => w.name.clone(),
        (Some(w), false) => format!("Tower → {}", w.name),
    };
    let how = s.with.as_ref().filter(|_| !s.reply()).map(|w| {
        let warn = w.how.starts_with("not sent") || w.how.starts_with("held");
        div().text_size(px(12.0)).text_color(if warn { gpui::Hsla::from(faded(AMBER, 0xdd)) } else { g.faint }).child(w.how.clone()).into_any_element()
    });
    let lines: Vec<AnyElement> = top.into_iter().chain(how).collect();
    let how = (!lines.is_empty()).then(|| v_flex().gap(px(3.0)).children(lines));
    message_row(bar, bg, head, if s.at > 0 { when(s.at) } else { String::new() }, how, s.text.clone(), extra)
}

/// A row: a colored bar, its head (and the time), a line on how it went, the words.
fn message_row(bar: gpui::Rgba, bg: gpui::Rgba, head: String, time: String, how: Option<gpui::Div>, text: String, extra: Vec<AnyElement>) -> AnyElement {
    let g = glass();
    h_flex()
        .w_full()
        .items_start()
        .gap(px(10.0))
        .p(px(12.0))
        .rounded(px(14.0))
        .bg(bg)
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
                        .child(div().flex_1().min_w_0().text_size(px(13.0)).font_weight(FontWeight::SEMIBOLD).child(head))
                        .child(div().flex_none().text_size(px(12.0)).text_color(g.faint).child(time)),
                )
                .children(how)
                .child(div().w_full().text_size(px(13.5)).line_height(px(20.0)).text_color(g.text).child(text))
                .children(extra),
        )
        .into_any_element()
}

/// What waits in the send queue for a session (`name`), as the state's
/// agent line has it (`waiting`): a row each; notes (introductions) aside.
pub(super) fn waiting_rows(name: &str, waiting: &[serde_json::Value]) -> Vec<AnyElement> {
    waiting
        .iter()
        .filter(|w| w["note"] != true)
        .filter_map(|w| {
            let text = w["text"].as_str().map(str::trim).filter(|t| !t.is_empty())?.to_string();
            let head = match w["from"].as_str() {
                Some(from) => format!("{from} → {name}"),
                None => format!("Tower → {name}"),
            };
            let how = if w["checking"] == true {
                "held: checking it against what you typed there".to_string()
            } else if let Some(why) = w["parked"].as_str() {
                format!("parked ({why}): it won't go on its own")
            } else {
                "waiting: it goes in when its turn ends".to_string()
            };
            let warn = w["checking"] == true || w["parked"].is_string();
            let g = glass();
            let how = div().text_size(px(12.0)).text_color(if warn { gpui::Hsla::from(faded(AMBER, 0xdd)) } else { g.faint }).child(how);
            Some(message_row(faded(CYAN, 0x55), rgba(0x00000000), head, String::new(), Some(how), text, vec![]).into_any_element())
        })
        .map(|row| div().w_full().rounded(px(14.0)).border_1().border_dashed().border_color(rgba(LINE)).child(row).into_any_element())
        .collect()
}

impl Panel {
    /// The conversation column of `space`'s view (None: the main tower's),
    /// `width` points wide, its input at the foot.
    pub(super) fn chat_column(&mut self, space: Option<&str>, width: f32, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let mut said = conversation(&self.entries, space, MAX_SAID);
        // What still waits in the send queue for the space's sessions: at the
        // end; its "queued" rows above are left out (the waiting row says it).
        let mut waits: Vec<AnyElement> = vec![];
        if let Some(sp) = space {
            let agents = self.tower.read(cx).s("agents").as_array().cloned().unwrap_or_default();
            // Its sessions, as its mission has them (Herdr's own list besides).
            let all = self.decisions(cx);
            let mut members: Vec<(String, String)> =
                self.missions(&all, cx).open.iter().filter(|m| m.name == sp).flat_map(|m| m.sessions.iter().map(|s| (s.address.clone(), s.name.clone()))).collect();
            for (a, n, _) in self.live.iter().filter(|l| l.name == sp).flat_map(|l| l.sessions.iter()) {
                if !members.iter().any(|(m, _)| m == a) {
                    members.push((a.clone(), n.clone()));
                }
            }
            let mut texts = vec![];
            for (addr, name) in members {
                let Some(a) = agents.iter().find(|a| a["address"] == addr.as_str()) else { continue };
                let name = a["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&name).to_string();
                let w = a["waiting"].as_array().map(Vec::as_slice).unwrap_or_default();
                texts.extend(waiting_texts(&addr, w));
                waits.extend(waiting_rows(&name, w));
            }
            said = without_waiting(said, &texts);
        }
        // How it was created (launched from New space): at its top, under its own label, folded as asked.
        let story = space.map(|sp| story_of(&self.entries, sp)).unwrap_or_default();
        let fold_key = space.map(|sp| format!("story-folded:{sp}")).unwrap_or_default();
        let folded = self.full.as_ref().is_some_and(|f| f.thread_open.contains(&fold_key));
        // A new message: the column goes to its end.
        if let Some(f) = self.full.as_mut() {
            let key = (space.map(String::from), said.len() + story.len(), said.last().map(|s| s.at).unwrap_or(0));
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
        let latest = said.iter().rev().find(|s| s.you && s.with.is_none()).map(|s| s.at);
        let acts = self.tower.read(cx).state.as_ref().and_then(|s| s["acts"].as_array().cloned()).unwrap_or_default();
        let act = latest.and_then(|at| {
            let opened: Vec<&Entry> = self.entries.iter().filter(|e| e.kind == "act" && !e.record && e.act.as_ref().is_some_and(|a| a["request_at"].as_u64() == Some(at))).collect();
            ActShown::of_entries(&opened, &acts)
        });
        let act_items = act.map(|a| self.act_items(&a, cx)).unwrap_or_default();
        let last_of_latest = latest.and_then(|at| said.iter().rposition(|s| s.with.is_none() && s.request_at == at));
        let mut rows: Vec<AnyElement> = vec![];
        if !story.is_empty() {
            let n = story.len();
            let label = if n == 1 { "How this space was created · 1 message".to_string() } else { format!("How this space was created · {n} messages") };
            let key = fold_key.clone();
            rows.push(
                h_flex()
                    .w_full()
                    .px(px(4.0))
                    .pt(px(2.0))
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().min_w_0().child(bay_label(&label).px(px(0.0))))
                    .child(
                        div()
                            .id("story-fold")
                            .flex_none()
                            .text_size(px(12.0))
                            .text_color(g.faint)
                            .cursor_pointer()
                            .hover(|s| s.text_color(g.text))
                            .child(if folded { "Show" } else { "Fold" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(f) = this.full.as_mut()
                                    && !f.thread_open.remove(&key)
                                {
                                    f.thread_open.insert(key.clone());
                                }
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
            if !folded {
                rows.extend(story.iter().map(told_row));
            }
            if !said.is_empty() || !waits.is_empty() {
                rows.push(div().px(px(4.0)).pt(px(6.0)).child(bay_label("Since it was launched").px(px(0.0))).into_any_element());
            }
        }
        let mut act_items = Some(act_items);
        // Each answer with the message it answers, and each question with its answer.
        let pairs = pairs(&said);
        let first = rows.len();
        let now = history::now_ms();
        // Asked since answers were paired (earlier ones never were): it may still wait.
        let paired_since = self.entries.iter().filter(|e| e.reply_to.is_some()).map(|e| e.at).min();
        for (i, s) in said.iter().enumerate() {
            let mut extra = if Some(i) == last_of_latest { act_items.take().unwrap_or_default() } else { vec![] };
            let jump = |to: usize, id: String, words: String, color: gpui::Hsla| {
                div()
                    .id(SharedString::from(id))
                    .text_size(px(12.0))
                    .text_color(color)
                    .cursor_pointer()
                    .hover(|st| st.text_color(glass().text))
                    .child(words)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = this.full.as_ref() {
                            f.chat_scroll.scroll_to_top_of_item(first + to);
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            };
            // What it answers: a line on top, to the question.
            let top = match (pairs[i].asks, s.reply_to) {
                (Some(j), _) => Some(jump(j, format!("asked-{i}-{}", s.at), asked_line(&said[j]), g.faint)),
                // Its question is further back than the column goes: said, not linked.
                (None, Some(at)) => self.entries.iter().find(|e| e.at == at).map(|q| {
                    let you = q.kind == "request";
                    let q = Said { you, text: q.text.split_once(": ").filter(|_| !you).map(|(_, m)| m.to_string()).unwrap_or_else(|| q.text.clone()), at, request_at: 0, with: None, reply_to: None };
                    div().text_size(px(12.0)).text_color(g.faint).child(asked_line(&q)).into_any_element()
                }),
                _ => None,
            };
            // A question: answered (to the answer), or still waiting for one.
            if s.asks() {
                match pairs[i].answer {
                    Some(j) => extra.push(jump(j, format!("answered-{i}-{}", s.at), "answered ↓".into(), gpui::Hsla::from(faded(CYAN, 0xee)))),
                    None if paired_since.is_some_and(|since| s.at >= since) && now.saturating_sub(s.at) < 12 * 3_600_000 => {
                        extra.push(div().text_size(px(12.0)).text_color(g.faint).child("waiting for an answer").into_any_element())
                    }
                    None => {}
                }
            }
            rows.push(said_row_with(s, &who, top, extra));
        }
        rows.extend(waits);
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
                    .child(if space.is_some() {
                        format!("Nothing asked of {tower} yet. What you write below goes there; its answers, what you type in this space's sessions and what the tower sends them show here.")
                    } else {
                        format!("Nothing asked of {tower} yet. What you write below goes there, and its answers show here.")
                    })
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
                    // Each row a child of its own: a link scrolls to one (`scroll_to_top_of_item`).
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .pb(px(4.0))
                    .children(rows),
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

    fn of(kind: &str, status: Option<&str>, source: Option<&str>, session: &str, text: &str, at: u64) -> Entry {
        Entry {
            kind: kind.into(),
            status: status.map(String::from),
            source: source.map(String::from),
            session: Some(session.into()),
            name: Some("eyzflow".into()),
            space: Some("support-translation".into()),
            text: text.into(),
            at,
            record: kind == "request",
            ..Default::default()
        }
    }

    /// Two questions in a row, their answers out of order: each answer quotes
    /// the right one, and each question says it was answered, by which.
    #[test]
    fn answers_out_of_order_are_paired_with_their_questions() {
        let mut first = of("request", Some("answer"), Some("terminal:t7"), "t7", "we tested on ingest filter and offer filters?", 10);
        first.name = Some("eyzflow".into());
        let second = of("request", None, Some("terminal:t8"), "t8", "is the API on dev yet?", 20);
        let mut sent = of("queue", Some("queued"), None, "t7", "Queued for eyzflow: Run the e2e tests.", 25);
        sent.record = false;
        // The API session answers first, then eyzflow, then eyzflow on the tower's message.
        let mut api = of("reply", None, None, "t8", "Yes: deployed at 14:02.", 30);
        api.reply_to = Some(20);
        let mut ingest = of("reply", None, None, "t7", "Both filters pass on dev.", 40);
        ingest.reply_to = Some(10);
        let mut e2e = of("reply", None, None, "t7", "The e2e tests pass.", 50);
        e2e.reply_to = Some(25);
        let entries = vec![first, second, sent, api, ingest, e2e];
        let said = conversation(&entries, Some("support-translation"), 200);
        assert_eq!(said.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), [
            "we tested on ingest filter and offer filters?", "is the API on dev yet?", "Run the e2e tests.", "Yes: deployed at 14:02.", "Both filters pass on dev.", "The e2e tests pass."
        ]);
        let p = pairs(&said);
        assert_eq!((p[3].asks, p[4].asks, p[5].asks), (Some(1), Some(0), Some(2)), "each answer to its own question");
        assert_eq!((p[0].answer, p[1].answer, p[2].answer), (Some(4), Some(3), Some(5)), "each question answered, by its answer");
        assert_eq!(asked_line(&said[0]), "↳ you asked: we tested on ingest filter and offer filters?");
        assert_eq!(asked_line(&said[2]), "↳ the tower sent it: Run the e2e tests.");
        assert!(said[3].with.as_ref().is_some_and(|w| w.how == "replied" && w.name == "eyzflow"));
        // The tower's answer to a request in its own conversation pairs the same way.
        let mut answer = e("answer", "Yes, it is on prod.", None, 80);
        answer.reply_to = Some(60);
        let main = conversation(&[e("request", "is it on prod?", None, 60), e("request", "and the docs?", None, 70), answer], None, 200);
        let p = pairs(&main);
        assert_eq!((p[2].asks, p[0].answer, p[1].answer), (Some(0), Some(2), None), "the second still waits");
        assert!(main[0].asks() && main[1].asks() && !main[2].asks());
    }

    /// What you type in a session, and what the tower queues for it and sends
    /// it (or doesn't), are in its space's conversation and its own thread.
    #[test]
    fn a_spaces_conversation_has_what_passed_with_its_sessions() {
        let entries = vec![
            e("request", "how is the translation going?", Some("support-translation"), 1),
            e("answer", "It works on dev.", Some("support-translation"), 2),
            of("request", None, Some("terminal:t8w"), "t8w", "merged on dev", 3),
            of("queue", Some("queued"), None, "t8w", "Queued for eyzflow: PR #787 (eyzhub/eyzflow) into dev was merged on GitHub.", 4),
            of("queue", Some("not-sent"), None, "t8w", "Not sent: you had told it yourself (\"merged on dev\"): PR #787 (eyzhub/eyzflow) into dev was merged on GitHub.", 5),
            of("queue", Some("queued"), None, "t8w", "Queued for eyzflow: Release #787 to prod", 6),
            of("queue", Some("delivered"), None, "t8w", "Delivered to eyzflow: Release #787 to prod", 7),
            of("request", None, Some("answer:t8w"), "t8w", "im logged in", 8),
            of("queue", Some("note"), None, "t8w", "Told eyzflow: [tower] You are Claude in eyzflow", 9),
            of("request", None, Some("terminal:t9"), "t9", "in another space", 10),
        ];
        let mut other = entries[9].clone();
        other.space = Some("sandbox".into());
        let entries = [&entries[..9], &[other]].concat();
        let s = conversation(&entries, Some("support-translation"), 200);
        let rows: Vec<(bool, &str, Option<&str>)> = s.iter().map(|x| (x.you, x.text.as_str(), x.with.as_ref().map(|w| w.how.as_str()))).collect();
        assert_eq!(
            rows,
            [
                (true, "how is the translation going?", None),
                (false, "It works on dev.", None),
                (true, "merged on dev", Some("in its terminal")),
                (false, "PR #787 (eyzhub/eyzflow) into dev was merged on GitHub.", Some("not sent: you had told it yourself")),
                (false, "Release #787 to prod", Some("sent")),
                (true, "im logged in", Some("answered its question")),
            ]
        );
        assert!(s[2..].iter().all(|x| x.request_at == 0 && x.with.as_ref().is_some_and(|w| w.addr == "t8w" && w.name == "eyzflow")));
        // The main tower's conversation has none of it.
        assert!(conversation(&entries, None, 200).is_empty());
        // The session's own thread: the same, for it alone.
        let t = session_thread(&entries, "t8w", 200);
        assert_eq!(t.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), ["merged on dev", "PR #787 (eyzhub/eyzflow) into dev was merged on GitHub.", "Release #787 to prod", "im logged in"]);
        assert_eq!(session_thread(&entries, "t8w", 1).len(), 1);
        // A message that still waits shows as waiting, not also as queued.
        let w = without_waiting(conversation(&entries, Some("support-translation"), 200), &[("t8w".into(), "Release #787 to prod".into()), ("t8w".into(), "never queued".into())]);
        assert_eq!(w.len(), 6, "a sent one stays");
        let queued = [entries.clone(), vec![of("queue", Some("queued"), None, "t8w", "Queued for eyzflow: Run the e2e tests", 11)]].concat();
        assert_eq!(without_waiting(session_thread(&queued, "t8w", 200), &[("t8w".into(), "Run the e2e tests".into())]).len(), 4);
        // A line written after a later one still shows in time order.
        let late = [entries.clone(), vec![of("request", None, Some("terminal:t8w"), "t8w", "keep this link in mind", 4)]].concat();
        let t = session_thread(&late, "t8w", 200);
        assert_eq!(t.iter().map(|x| x.at).collect::<Vec<_>>(), [3, 4, 4, 6, 8]);
        assert_eq!(conversation(&late, Some("support-translation"), 200).iter().map(|x| x.at).collect::<Vec<_>>(), [1, 2, 3, 4, 4, 6, 8]);
        // The tower's held reply, not sent because you answered in the terminal.
        assert_eq!(session_said(&of("request", None, Some("terminal:t7p"), "t7p", "<agent-message from='ab4'> [Subagent hand-back] done", 1)), None);
        let answer = of("request", Some("answer"), Some("terminal:t8w"), "t8w", "no, keep it on dev", 2);
        assert_eq!(session_said(&answer).map(|x| (x.you, x.with.unwrap().how)), Some((true, "answered its question, in its terminal".to_string())));
        let held = of("queue", Some("not-sent"), None, "t8t", "Not sent: you answered it in the terminal (the reply was \"open it\")", 1);
        assert_eq!(session_said(&held).map(|x| (x.text, x.with.unwrap().how)), Some(("you answered it in the terminal (the reply was \"open it\")".to_string(), "not sent".to_string())));
    }

    /// A space launched from New space has the conversation that made it,
    /// its own `creation` entries, oldest first; it is in no other
    /// conversation, its tower's or the main one.
    #[test]
    fn a_launched_space_has_the_conversation_that_made_it() {
        let made = |who: &str, text: &str, space: &str, at: u64| Entry { kind: "creation".into(), status: Some(who.into()), text: text.into(), space: Some(space.into()), at, record: true, ..Default::default() };
        let entries = vec![
            made("tower", "Drafted scratch-check: 1 session (tower-scratch).", "ns-check", 2),
            made("you", "Throwaway check: open one session in tower-scratch.", "ns-check", 1),
            made("you", "another space's ticket", "sandbox", 3),
            made("you", "Name the space ns-check instead.", "ns-check", 4),
            e("request", "how is it going?", Some("ns-check"), 9),
            e("answer", "It answered ok.", Some("ns-check"), 10),
        ];
        let story = story_of(&entries, "ns-check");
        assert_eq!(story.iter().map(|t| (t.who.as_str(), t.at)).collect::<Vec<_>>(), [("you", 1), ("tower", 2), ("you", 4)]);
        assert_eq!(story_of(&entries, "sandbox").len(), 1);
        assert!(story_of(&entries, "elsewhere").is_empty());
        // Not in the space's conversation with its tower, nor in the main one.
        assert_eq!(conversation(&entries, Some("ns-check"), 200).iter().map(|x| x.at).collect::<Vec<_>>(), [9, 10]);
        assert!(conversation(&entries, None, 200).is_empty());
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
