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
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};

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

/// How long a question to the tower is waited on before "No answer came".
const WAIT_FOR_MS: u64 = 30 * 60 * 1000;

/// How long a message a click went to stays lit, fading.
const FLASH_SECS: f32 = 1.8;

/// Where a question stands.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Waits {
    /// Answered: its answer is the row given.
    Answered(usize),
    /// Still waiting for its answer.
    Pending,
    /// No answer comes: why, in words (the tower's turn ended without
    /// one, it failed, you took it back, the tower isn't running, too long).
    Ended(String),
    /// Not followed: asked before answers named their questions, an answer
    /// after it that names none (its link failed), or not a question.
    Unknown,
}

/// Where the question at row `i` stands: answered by its pair; else what
/// the history keeps of it (`history::unanswered_of`); else waiting, for
/// half an hour at most. `since`: when answers began naming their
/// questions (one asked before then is followed only while it is new);
/// `broken`: an answer of the tower's came after it that names no
/// question (its link failed); `absent`: it went to the main brain, which
/// isn't running.
#[allow(clippy::too_many_arguments)]
pub(super) fn standing(said: &[Said], pairs: &[Pair], i: usize, entries: &[Entry], since: Option<u64>, broken: bool, absent: bool, now: u64) -> Waits {
    let s = &said[i];
    if let Some(j) = pairs[i].answer {
        return Waits::Answered(j);
    }
    let age = now.saturating_sub(s.at);
    if !s.asks() || (!since.is_some_and(|since| s.at >= since) && age >= WAIT_FOR_MS) || (s.with.is_none() && broken) {
        return Waits::Unknown;
    }
    if let Some(e) = history::unanswered_of(entries, s.at) {
        return Waits::Ended(match e.status.as_deref() {
            Some("taken-back") => "You took it back.".into(),
            _ if !e.text.trim().is_empty() => e.text.trim().to_string(),
            _ => "No answer came.".into(),
        });
    }
    if absent && age > 30_000 {
        return Waits::Ended("No answer: the tower isn't running.".into());
    }
    if s.with.is_some() || age < WAIT_FOR_MS { Waits::Pending } else { Waits::Ended("No answer came.".into()) }
}

/// Words cut for a log line.
fn short(words: &str) -> String {
    if words.chars().count() > 40 { format!("{}…", words.chars().take(39).collect::<String>()) } else { words.to_string() }
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
        // How it goes (`tower progress`): asked aside, without stopping its turn, and its answer.
        ("progress", Some("asked:brain")) => (false, "the tower asked how it goes".into(), text.into()),
        ("progress", Some(s)) if s.starts_with("asked:") => (true, "asked how it goes".into(), text.into()),
        ("progress", Some("btw")) => (false, "answered aside (/btw), its turn went on".into(), text.into()),
        ("progress", _) => (false, "how it goes, from its transcript".into(), text.into()),
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
pub(super) fn when(at: u64) -> String {
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
        let mut index: Vec<(u64, String, Option<bool>)> = vec![(0, String::new(), None); first];
        let now = history::now_ms();
        // Asked since answers were paired (earlier ones never were): it may still wait.
        let paired_since = self.entries.iter().filter(|e| e.reply_to.is_some()).map(|e| e.at).min();
        // The last of the tower's answers here that names no question (a link that failed).
        let oldest = said.first().map(|s| s.at).unwrap_or(0);
        let unlinked: std::collections::HashSet<u64> = self.entries.iter().filter(|e| e.kind == "answer" && e.reply_to.is_none() && e.at >= oldest).map(|e| e.at).collect();
        let last_unlinked = said.iter().rposition(|a| !a.you && a.with.is_none() && unlinked.contains(&a.at));
        // A question to the main brain while it isn't running gets no answer.
        let main_brain = space.is_none_or(|sp| !self.space_tower_of(sp, cx));
        let absent = main_brain && self.tower.read(cx).s("brain").as_str() == Some("absent");
        // The message a click went to: lit a moment, fading.
        let flash = self.full.as_ref().and_then(|f| f.chat_flash).map(|(at, t)| (at, t.elapsed().as_secs_f32() / FLASH_SECS)).filter(|(_, t)| *t < 1.0);
        if flash.is_some() {
            window.request_animation_frame();
        }
        for (i, s) in said.iter().enumerate() {
            let mut extra = if Some(i) == last_of_latest { act_items.take().unwrap_or_default() } else { vec![] };
            let go = |to: usize| {
                let at = said[to].at;
                cx.listener(move |this: &mut Panel, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Panel>| this.chat_jump(first + to, at, cx))
            };
            let jump = |to: usize, id: String, words: String, color: gpui::Hsla| {
                div().id(SharedString::from(id)).text_size(px(12.0)).text_color(color).cursor_pointer().hover(|st| st.text_color(glass().text)).child(words).on_click(go(to)).into_any_element()
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
            // A question: answered (to the answer), still waiting for one, or given up on.
            let state = if s.asks() { standing(&said, &pairs, i, &self.entries, paired_since, last_unlinked.is_some_and(|j| j > i), absent && s.with.is_none(), now) } else { Waits::Unknown };
            match &state {
                Waits::Answered(j) => extra.push(jump(*j, format!("answered-{i}-{}", s.at), "answered ↓".into(), gpui::Hsla::from(faded(CYAN, 0xee)))),
                // The tower's own: a spinner, and a way to take it back.
                Waits::Pending if s.with.is_none() => {
                    let at = s.at;
                    extra.push(
                        h_flex()
                            .w_full()
                            .gap(px(8.0))
                            .items_center()
                            .child(Spinner::new().xsmall().color(gpui::Hsla::from(rgb(CYAN))))
                            .child(div().flex_1().min_w_0().text_size(px(12.0)).text_color(g.muted).child(format!("waiting for {} · {}", if who == "The tower" { "the tower".to_string() } else { who.to_lowercase() }, super::span(now.saturating_sub(at)))))
                            .child(
                                div()
                                    .id(SharedString::from(format!("take-back-{at}")))
                                    .flex_none()
                                    .text_size(px(12.0))
                                    .text_color(g.faint)
                                    .cursor_pointer()
                                    .hover(|st| st.text_color(glass().text))
                                    .child("Take back")
                                    .on_click(cx.listener(move |this, _, _, cx| this.take_back(at, cx))),
                            )
                            .into_any_element(),
                    )
                }
                // A session's: it may take a while.
                Waits::Pending if now.saturating_sub(s.at) < 12 * 3_600_000 => extra.push(div().text_size(px(12.0)).text_color(g.faint).child("waiting for an answer").into_any_element()),
                Waits::Ended(why) => extra.push(div().text_size(px(12.0)).text_color(gpui::Hsla::from(faded(AMBER, 0xdd))).child(why.clone()).into_any_element()),
                _ => {}
            }
            let row = said_row_with(s, &who, top, extra);
            // An answered question: the whole of it goes to its answer.
            let row = match &state {
                Waits::Answered(j) => div().id(SharedString::from(format!("question-{i}-{}", s.at))).w_full().rounded(px(14.0)).cursor_pointer().hover(|st| st.bg(rgba(0xffffff0a))).on_click(go(*j)).child(row).into_any_element(),
                _ => row,
            };
            // Gone to: lit, fading out.
            let row = match flash {
                Some((at, t)) if at == s.at => {
                    let a = ((1.0 - t * t).clamp(0.0, 1.0) * 255.0) as u32;
                    div()
                        .relative()
                        .w_full()
                        .child(row)
                        .child(div().absolute().top_0().left_0().size_full().rounded(px(14.0)).border_2().border_color(faded(CYAN, a)).bg(faded(CYAN, a * 0x30 / 0xff)))
                        .into_any_element()
                }
                _ => row,
            };
            rows.push(row);
            index.push((s.at, s.text.clone(), if matches!(state, Waits::Answered(_)) { Some(true) } else { pairs[i].asks.map(|_| false) }));
        }
        index.extend(waits.iter().map(|_| (0, String::new(), None)));
        rows.extend(waits);
        if let Some(f) = self.full.as_mut() {
            f.chat_rows = index;
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

    /// A click on a question (or its line on an answer) went to row `ix`
    /// of the column, the message at `at`: it comes to the top and lights
    /// up (the frames of its fading are asked for as it is drawn: a frame
    /// can't be asked for from a click).
    fn chat_jump(&mut self, ix: usize, at: u64, cx: &mut Context<Self>) {
        if let Some(f) = self.full.as_mut() {
            f.chat_scroll.scroll_to_top_of_item(ix);
            f.chat_flash = Some((at, std::time::Instant::now()));
        }
        cx.notify();
    }

    /// "Take back" on a question the tower is still at: the daemon stops
    /// its brain if it is on it, and keeps that no answer comes.
    fn take_back(&mut self, at: u64, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async move { crate::protocol::send(&crate::protocol::Request::Withdraw { at }) });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(r) if r.ok => {}
                    Ok(r) => this.status = Some((false, r.error.unwrap_or_else(|| "It couldn't be taken back.".into()))),
                    Err(e) => this.status = Some((false, format!("It couldn't be taken back: {e:#}"))),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `HERDER_TOWER_ORB_CLICK` (see `crate::orb`): where to click the
    /// column's row whose words hold `words`, once it can be clicked: a
    /// question with its answer (on its head), or an answer with its
    /// question (on its "↳ you asked" line), in the window. None: not yet
    /// (it is brought into view first).
    pub(crate) fn click_point(&mut self, words: &str, cx: &mut Context<Self>) -> Option<gpui::Point<gpui::Pixels>> {
        let f = self.full.as_ref()?;
        let (ix, question) = f.chat_rows.iter().enumerate().find_map(|(ix, (at, text, click))| click.filter(|_| *at > 0 && text.contains(words)).map(|q| (ix, q)))?;
        let (b, view, off) = (f.chat_scroll.bounds_for_item(ix)?, f.chat_scroll.bounds(), f.chat_scroll.offset());
        let top = b.top() + off.y;
        if top < view.top() || top + px(30.0) > view.bottom() {
            f.chat_scroll.scroll_to_item(ix);
            cx.notify();
            return None;
        }
        eprintln!(
            "[orb] HERDER_TOWER_ORB_CLICK: row {ix} ({:?}, {}) is at y {:.0} of the column's {:.0}..{:.0}",
            short(words),
            if question { "a question: its head" } else { "an answer: its line to the question" },
            f32::from(top),
            f32::from(view.top()),
            f32::from(view.bottom())
        );
        Some(gpui::point(b.left() + px(80.0), top + px(if question { 22.0 } else { 47.0 })))
    }

    /// `HERDER_TOWER_ORB_CLICK`: where the click went (the lit message's row) and where it is now.
    pub(crate) fn click_report(&mut self) {
        let Some(f) = self.full.as_ref() else { return };
        let Some((at, t)) = f.chat_flash else {
            eprintln!("[orb] HERDER_TOWER_ORB_CLICK: nothing lit: the click went nowhere");
            return;
        };
        let ix = f.chat_rows.iter().position(|(a, _, _)| *a == at);
        let shown = ix.and_then(|ix| f.chat_scroll.bounds_for_item(ix)).map(|b| f32::from(b.top() + f.chat_scroll.offset().y));
        let words = ix.map(|ix| short(&f.chat_rows[ix].1)).unwrap_or_default();
        eprintln!(
            "[orb] HERDER_TOWER_ORB_CLICK: lit row {ix:?} ({words:?}) {} ms ago, now at y {:.0} (the column's top: {:.0})",
            t.elapsed().as_millis(),
            shown.unwrap_or(-1.0),
            f32::from(f.chat_scroll.bounds().top())
        );
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

    /// A question to the tower waits (the spinner) until its answer is in;
    /// one that will get none says why: the turn ended without one, the
    /// tower failed, you took it back, it isn't running, or too long passed.
    #[test]
    fn a_question_waits_until_answered_or_says_why_no_answer_comes() {
        let min = 60_000;
        let at = |n: u64| 1_000_000 + n * min;
        let mut answer = e("answer", "Yes, on prod.", None, at(2));
        answer.reply_to = Some(at(1));
        let unanswered = |of: u64, why: &str, text: &str, when: u64| Entry { kind: "unanswered".into(), status: Some(why.into()), text: text.into(), reply_to: Some(of), at: when, record: true, ..Default::default() };
        let entries = vec![
            e("request", "is it on prod?", None, at(1)),
            answer,
            e("request", "and the docs?", None, at(3)),
            e("request", "check the logs", None, at(4)),
            unanswered(at(4), "failed", "The tower couldn't answer: it hit its rate limit.", at(5)),
            e("request", "never mind that", None, at(6)),
            unanswered(at(6), "taken-back", "You took it back.", at(7)),
            e("request", "and now?", None, at(8)),
            unanswered(at(8), "empty", "The tower's turn ended without an answer.", at(9)),
            e("request", "the last one", None, at(10)),
        ];
        let said = conversation(&entries, None, 200);
        let p = pairs(&said);
        let since = Some(at(2));
        let stand = |i: usize, now: u64, absent: bool| standing(&said, &p, i, &entries, since, false, absent, now);
        let now = at(11);
        assert_eq!(stand(0, now, false), Waits::Answered(1));
        assert_eq!(stand(1, now, false), Waits::Unknown, "an answer is no question");
        assert_eq!(stand(2, now, false), Waits::Pending, "asked 8 minutes ago: still waiting");
        assert_eq!(stand(2, at(3) + WAIT_FOR_MS, false), Waits::Ended("No answer came.".into()), "half an hour on, it isn't coming");
        assert_eq!(stand(3, now, false), Waits::Ended("The tower couldn't answer: it hit its rate limit.".into()));
        assert_eq!(stand(4, now, false), Waits::Ended("You took it back.".into()));
        assert_eq!(stand(5, now, false), Waits::Ended("The tower's turn ended without an answer.".into()));
        assert_eq!(stand(6, now, false), Waits::Pending);
        assert_eq!(stand(6, now, true), Waits::Ended("No answer: the tower isn't running.".into()), "the main brain is gone");
        assert_eq!(stand(6, at(10) + 10_000, true), Waits::Pending, "just asked: it may be starting");
        // Asked before answers named their questions: followed while new (the
        // first question of all, too), then nothing is claimed.
        assert_eq!(standing(&said, &p, 2, &entries, Some(at(20)), false, false, now), Waits::Pending);
        assert_eq!(standing(&said, &p, 2, &entries, None, false, false, now), Waits::Pending);
        assert_eq!(standing(&said, &p, 2, &entries, Some(at(20)), false, false, at(3) + WAIT_FOR_MS), Waits::Unknown);
        // An answer after it that names no question (its link failed): nothing is claimed either.
        let mut loose = entries.clone();
        loose.push(e("answer", "The logs are clean.", None, at(10) + 5));
        let said2 = conversation(&loose, None, 200);
        let p2 = pairs(&said2);
        let i = said2.iter().position(|s| s.text == "the last one").unwrap();
        assert_eq!(standing(&said2, &p2, i, &loose, since, true, false, now), Waits::Unknown);
        assert_eq!(standing(&said2, &p2, i, &loose, since, false, false, now), Waits::Pending);
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
