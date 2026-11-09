//! What sessions ask (see `crate::questions`), in the panel: a card for each
//! question set the tower leaves to the user (`decision: escalate`), at the
//! foot of the chat. A set's questions go one at a time (Back, Next, a
//! review, Submit, as the terminal dialog does); each shows the tower's three
//! recommendations as large one-tap answers (the first marked Recommended,
//! with its reason; a spinner while they come, never in the way), the
//! dialog's own options, and "Other": your own answer, typed, spoken
//! (Control, as anywhere in the panel) or with files. Answers go through the
//! daemon (`crate::answers`), which types them into the session. The policy
//! (how much the tower answers by itself) is in Settings.

use std::collections::{HashMap, HashSet};

use gpui::{AnyElement, App, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::input::{InputContent, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use serde_json::{Value, json};

use super::{Accounts, AMBER, NAVY, PALE, Panel, ROSE, View, glass, message_parts};
use crate::questions::{QuestionSet, Tab, TabRecs};

/// What the user chose on one question.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) enum Choice {
    /// Nothing yet (the first recommendation, once there are some).
    #[default]
    None,
    /// A recommendation, by its place.
    Rec(usize),
    /// Options of the dialog, by index (one on a single-choice question; on
    /// a multi-choice one, with the typed answer if there is one).
    Opts(Vec<usize>),
    /// Only the typed answer.
    Other,
}

/// The choice that counts: nothing chosen means the first recommendation.
pub(super) fn effective(choice: &Choice, recs: Option<&TabRecs>) -> Choice {
    match choice {
        Choice::None if recs.is_some_and(|r| !r.options.is_empty()) => Choice::Rec(0),
        c => c.clone(),
    }
}

/// The answer for tab `t` as the daemon takes it (`{tab, choice,
/// attachments?}`), or `None` if it has none yet. `other`: the typed
/// answer's text, `files`: the files with it.
pub(super) fn answer_of(t: usize, tab: &Tab, recs: Option<&TabRecs>, choice: &Choice, other: &str, files: &[String]) -> Option<Value> {
    let other = other.trim();
    let typed = |mut a: Value| {
        if !files.is_empty() {
            a["attachments"] = json!(files);
        }
        Some(a)
    };
    match effective(choice, recs) {
        Choice::Rec(i) => recs?.options.get(i).map(|r| json!({ "tab": t, "choice": r.choice })),
        Choice::Opts(v) if tab.multi && !other.is_empty() => typed(json!({ "tab": t, "choice": { "options": v, "other": other } })),
        Choice::Opts(v) if tab.multi && !v.is_empty() => Some(json!({ "tab": t, "choice": { "options": v } })),
        Choice::Opts(v) if !v.is_empty() => Some(json!({ "tab": t, "choice": v[0] })),
        Choice::Other if !other.is_empty() => typed(json!({ "tab": t, "choice": { "other": other } })),
        _ => None,
    }
}

/// The answer in a few words, for the review: `Postgres`, `Tests, Exports`, `“A staging app first” + 1 file`.
pub(super) fn summary(tab: &Tab, recs: Option<&TabRecs>, choice: &Choice, other: &str, files: usize) -> Option<String> {
    let other = other.trim();
    let quoted = || {
        let mut s = format!("“{}”", other.lines().next().unwrap_or(""));
        if files > 0 {
            s += &format!(" + {files} file{}", if files == 1 { "" } else { "s" });
        }
        s
    };
    match effective(choice, recs) {
        Choice::Rec(i) => recs?.options.get(i).map(|r| r.label.clone()),
        Choice::Opts(v) if !v.is_empty() || (tab.multi && !other.is_empty()) => {
            let mut parts: Vec<String> = v.iter().filter_map(|i| tab.options.get(*i).map(|o| o.label.clone())).collect();
            if tab.multi && !other.is_empty() {
                parts.push(quoted());
            }
            Some(parts.join(", "))
        }
        Choice::Other if !other.is_empty() => Some(quoted()),
        _ => None,
    }
}

/// Clicking an option: on a single-choice question it is the answer; on a
/// multi-choice one it is ticked or unticked (keeping a typed answer).
pub(super) fn toggle(choice: &Choice, multi: bool, option: usize) -> Choice {
    if !multi {
        return Choice::Opts(vec![option]);
    }
    let mut v = match choice {
        Choice::Opts(v) => v.clone(),
        _ => vec![],
    };
    if let Some(at) = v.iter().position(|o| *o == option) {
        v.remove(at);
    } else {
        v.push(option);
        v.sort();
    }
    Choice::Opts(v)
}

/// What the kind of question is called on its card.
fn kind_word(kind: &str, tabs: usize) -> &'static str {
    match kind {
        "claude-permission" => "Permission",
        "claude-plan" => "Plan",
        "codex-menu" => "Approval",
        "text" => "Message",
        _ if tabs > 1 => "Questions",
        _ => "Question",
    }
}

/// The sets the user decides, newest last.
pub(super) fn escalated(state: &Value, dismissed: &HashSet<String>) -> Vec<QuestionSet> {
    let mut sets: Vec<QuestionSet> = state["questions"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| serde_json::from_value::<QuestionSet>(v.clone()).ok()).collect())
        .unwrap_or_default();
    sets.retain(|s| s.decision.as_deref() == Some("escalate") && !dismissed.contains(&s.id) && !s.tabs.is_empty());
    sets.sort_by_key(|s| s.at_ms);
    sets
}

/// A card's state, kept while its set is open.
#[derive(Default)]
pub(super) struct Card {
    /// The question showing (`tabs.len()`: the review).
    step: usize,
    choices: Vec<Choice>,
    /// The typed answer per question (text and file tokens).
    others: Vec<Option<InputContent>>,
    sending: bool,
    error: Option<String>,
    /// Said under the card, not an error ("read again: check this one").
    note: Option<String>,
    /// The questions as the card last showed them, to tell what changed
    /// when the set is read again under a new id.
    tabs: Vec<Tab>,
    /// The dialog's own options are showing (folded away while there are
    /// recommendations, so the card fits).
    options_open: bool,
}

/// A tab of a Claude dialog whose question wasn't read yet (only the
/// screen's tab was, before the session's transcript had the rest).
pub(super) fn unread(set: &QuestionSet, t: usize) -> bool {
    set.kind == "claude-ask" && set.tabs.get(t).is_some_and(|tab| tab.question.trim().is_empty() || tab.options.is_empty())
}

/// A card carried over to its set read again (`new.replaces` it): what was
/// chosen stays for the questions that didn't change; the others start
/// over, the card shows the first of them, and says so.
pub(super) fn carry_card(mut card: Card, new: &QuestionSet) -> Card {
    let n = new.tabs.len();
    let changed: Vec<usize> = (0..n).filter(|t| !card.tabs.get(*t).is_some_and(|old| crate::questions::same_tab(old, &new.tabs[*t]))).collect();
    card.choices.resize(n, Choice::None);
    card.others.resize(n, None);
    for t in &changed {
        card.choices[*t] = Choice::None;
        card.others[*t] = None;
    }
    card.error = None;
    card.sending = false;
    card.note = changed.first().map(|t| {
        card.step = *t;
        let title = &new.tabs[*t].title;
        if title.is_empty() { "The session's questions were read again: check this one, then submit.".to_string() } else { format!("The session's questions were read again: check “{title}”, then submit.") }
    });
    card.tabs = new.tabs.clone();
    card
}

/// The panel's question cards.
#[derive(Default)]
pub(super) struct Asks {
    cards: HashMap<String, Card>,
    /// The "Other" field, and the question it is open on (set id, tab).
    other: Option<(String, usize, Entity<TextareaState>)>,
    dismissed: HashSet<String>,
    /// Sets already shown, so a new one brings the chat to the bottom.
    seen: HashSet<String>,
    /// The policy just chosen, until the daemon says it too.
    policy: Option<String>,
}

impl Asks {
    /// The "Other" field, if it is open and has the keyboard: dictation and
    /// files go there.
    pub(super) fn other_focused(&self, window: &Window, cx: &App) -> Option<Entity<TextareaState>> {
        let (_, _, field) = self.other.as_ref()?;
        gpui::Focusable::focus_handle(field.read(cx), cx).is_focused(window).then(|| field.clone())
    }
}

impl Panel {
    pub(super) fn open_sets(&self, cx: &App) -> Vec<QuestionSet> {
        escalated(self.tower.read(cx).state.as_ref().unwrap_or(&Value::Null), &self.asks.dismissed)
    }

    /// New sets bring the chat to its foot (where their cards are).
    pub(super) fn watch_sets(&mut self, cx: &App) {
        let sets = self.open_sets(cx);
        let open: HashSet<String> = sets.iter().map(|s| s.id.clone()).collect();
        // A set read again under a new id: its card carries on (what was chosen, the open field).
        for set in &sets {
            let Some(old) = set.replaces.as_ref().filter(|old| !open.contains(*old)) else { continue };
            if let Some(card) = self.asks.cards.remove(old) {
                self.asks.cards.insert(set.id.clone(), carry_card(card, set));
            }
            if let Some((oid, _, _)) = self.asks.other.as_mut().filter(|(oid, _, _)| oid == old) {
                *oid = set.id.clone();
            }
            if self.asks.dismissed.contains(old) {
                self.asks.dismissed.insert(set.id.clone());
            }
            self.asks.seen.insert(set.id.clone());
        }
        if open.iter().any(|id| !self.asks.seen.contains(id)) {
            self.chat.scroll_to_bottom();
        }
        self.asks.seen = open.clone();
        self.asks.cards.retain(|id, _| open.contains(id));
        if self.asks.other.as_ref().is_some_and(|(id, _, _)| !open.contains(id)) {
            self.asks.other = None;
        }
    }

    /// The set's typed answer for a tab: the open field's, else the one kept.
    fn other_of(&self, id: &str, tab: usize, cx: &App) -> InputContent {
        if let Some((oid, ot, field)) = &self.asks.other
            && oid == id
            && *ot == tab
        {
            return field.read(cx).content();
        }
        self.asks.cards.get(id).and_then(|c| c.others.get(tab).cloned().flatten()).unwrap_or_else(|| InputContent::new(""))
    }

    /// Opens "Other" on a question (keeping what was typed on another).
    fn open_other(&mut self, id: &str, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.keep_other(cx);
        let content = self.asks.cards.get(id).and_then(|c| c.others.get(tab).cloned().flatten()).unwrap_or_else(|| InputContent::new(""));
        let field = cx.new(|cx| {
            TextareaState::new(window, cx).auto_grow(1, 5).placeholder("Your answer: type it, or talk (Control), or add files")
        });
        field.update(cx, |s, cx| {
            s.set_value(content, window, cx);
            s.focus(window, cx);
        });
        let card = self.asks.cards.entry(id.to_string()).or_default();
        card.choices.resize(card.choices.len().max(tab + 1), Choice::None);
        if !matches!(card.choices[tab], Choice::Opts(_)) {
            card.choices[tab] = Choice::Other;
        }
        self.asks.other = Some((id.to_string(), tab, field));
        cx.notify();
    }

    /// Keeps the open field's answer with its question.
    fn keep_other(&mut self, cx: &App) {
        if let Some((id, tab, field)) = self.asks.other.take() {
            let content = field.read(cx).content();
            let card = self.asks.cards.entry(id).or_default();
            card.others.resize(card.others.len().max(tab + 1), None);
            card.others[tab] = Some(content);
        }
    }

    /// Files for the typed answer (the + by it, or dropped while it has the keyboard).
    pub(super) fn pick_other_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Attach".into()) });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    if let Some((_, _, field)) = this.asks.other.clone() {
                        this.attach_to(&field, paths, window, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn choose(&mut self, id: &str, tab: usize, choice: Choice, cx: &mut Context<Self>) {
        let card = self.asks.cards.entry(id.to_string()).or_default();
        card.choices.resize(card.choices.len().max(tab + 1), Choice::None);
        card.choices[tab] = choice;
        card.error = None;
        cx.notify();
    }

    fn go(&mut self, id: &str, step: usize, cx: &mut Context<Self>) {
        self.keep_other(cx);
        self.asks.cards.entry(id.to_string()).or_default().step = step;
        cx.notify();
    }

    /// The answers for every question, or the first one still unanswered.
    fn answers(&self, set: &QuestionSet, cx: &App) -> Result<Vec<Value>, usize> {
        let card = self.asks.cards.get(&set.id);
        let mut out = vec![];
        for (t, tab) in set.tabs.iter().enumerate() {
            if unread(set, t) {
                return Err(t);
            }
            let recs = set.recommendations.as_ref().and_then(|r| r.iter().find(|r| r.tab == t));
            let choice = card.and_then(|c| c.choices.get(t)).cloned().unwrap_or_default();
            let (text, files) = message_parts(&self.other_of(&set.id, t, cx));
            let files: Vec<String> = files.iter().map(|p| crate::history::keep_attachment(p).display().to_string()).collect();
            out.push(answer_of(t, tab, recs, &choice, &text, &files).ok_or(t)?);
        }
        Ok(out)
    }

    fn submit(&mut self, set: &QuestionSet, cx: &mut Context<Self>) {
        self.keep_other(cx);
        let answers = match self.answers(set, cx) {
            Ok(a) => a,
            Err(t) => {
                let card = self.asks.cards.entry(set.id.clone()).or_default();
                card.step = t;
                card.note = None;
                card.error = Some(if unread(set, t) {
                    "This question hasn't been read yet: it shows here in a moment, or answer it in the session.".into()
                } else {
                    "This one needs an answer first.".into()
                });
                cx.notify();
                return;
            }
        };
        let card = self.asks.cards.entry(set.id.clone()).or_default();
        card.sending = true;
        card.error = None;
        card.note = None;
        let id = set.id.clone();
        let task = cx.background_executor().spawn({
            let id = id.clone();
            async move { crate::answers::answer(&id, Value::Array(answers), true) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(card) = this.asks.cards.get_mut(&id) {
                    card.sending = false;
                    if let Err(e) = result {
                        card.error = Some(format!("{e:#}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn dismiss_set(&mut self, id: &str, cx: &mut Context<Self>) {
        self.asks.dismissed.insert(id.to_string());
        let id = id.to_string();
        cx.background_executor().spawn(async move { crate::answers::dismiss(&id) }).detach();
        cx.notify();
    }

    /// The cards for the chat's foot.
    pub(super) fn question_cards(&mut self, acc: &Accounts, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.open_sets(cx).iter().map(|s| self.question_card(s, acc, cx)).collect()
    }

    pub(super) fn question_card(&mut self, set: &QuestionSet, acc: &Accounts, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let n = set.tabs.len();
        let id = set.id.clone();
        let (step, sending, error, note) = {
            let c = self.asks.cards.entry(id.clone()).or_default();
            c.step = c.step.min(if n > 1 { n } else { 0 });
            if c.tabs.is_empty() {
                c.tabs = set.tabs.clone();
            }
            (c.step, c.sending, c.error.clone(), c.note.clone())
        };
        let busy = sending || matches!(set.status.as_str(), "answering" | "queued");
        let who = if set.name.is_empty() { set.session.clone() } else { set.name.clone() };
        // A turn that stopped for the user to act (merge, test): no answers are coming.
        let waits = set.reason == crate::questions::WAITS;

        let mut body: Vec<AnyElement> = vec![];
        // Who asks, and about what.
        body.push(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().size(px(8.0)).flex_none().rounded_full().bg(rgb(AMBER)))
                .child(div().min_w_0().truncate().text_sm().font_weight(FontWeight::MEDIUM).child(if waits {
                    format!("{who} waits for you")
                } else {
                    format!("{who} asks")
                }))
                .children(acc.of(&set.session).map(|a| acc.badge(a, &g)))
                .child(div().flex_1())
                .child(div().flex_none().text_xs().text_color(g.faint).child(kind_word(&set.kind, n)))
                .child(
                    div()
                        .id(SharedString::from(format!("dismiss-{id}")))
                        .p(px(3.0))
                        .rounded_full()
                        .cursor_pointer()
                        .text_color(g.muted)
                        .hover(|s| s.bg(rgba(0xffffff26)))
                        .child(Icon::new(IconName::Close).xsmall())
                        .on_click({
                            let id = id.clone();
                            cx.listener(move |this, _, _, cx| this.dismiss_set(&id, cx))
                        }),
                )
                .into_any_element(),
        );
        if !set.context.is_empty() {
            body.push(div().text_xs().text_color(g.muted).line_clamp(2).child(format!("Working on: {}", set.context)).into_any_element());
        }
        // The steps, as the dialog's tabs.
        if n > 1 {
            let mut pills: Vec<AnyElement> = vec![];
            for t in 0..=n {
                let title = if t == n { "Review".to_string() } else { set.tabs[t].title.clone() };
                let done = t < n && self.answer_summary(set, t, cx).is_some();
                let on = t == step;
                let id2 = id.clone();
                pills.push(
                    div()
                        .id(SharedString::from(format!("step-{id}-{t}")))
                        .px_2()
                        .py(px(2.0))
                        .rounded_full()
                        .text_xs()
                        .cursor_pointer()
                        .when(on, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)).font_weight(FontWeight::MEDIUM))
                        .when(!on, |el| el.bg(rgba(0xffffff14)).text_color(g.muted).hover(|s| s.bg(rgba(0xffffff2e))))
                        .child(if done { format!("✓ {title}") } else { title })
                        .on_click(cx.listener(move |this, _, _, cx| this.go(&id2, t, cx)))
                        .into_any_element(),
                );
            }
            body.push(h_flex().flex_wrap().gap_1().children(pills).into_any_element());
        }
        if step < n {
            body.extend(self.question_step(set, step, busy, cx));
        } else {
            body.extend(self.review_step(set, cx));
        }
        // How it's going.
        let status: Option<AnyElement> = if sending {
            Some(working("Sending…"))
        } else if set.status == "answering" {
            Some(working("Answering in the session…"))
        } else if set.status == "queued" {
            Some(div().text_xs().text_color(g.muted).child("Waiting for the session to stop; then it goes in.").into_any_element())
        } else if let Some(e) = error.or_else(|| (set.status == "failed").then(|| set.error.clone().unwrap_or_else(|| "It stopped.".into()))) {
            Some(
                div()
                    .text_xs()
                    .text_color(rgb(ROSE))
                    .child(format!("{} You can try again, or finish in the terminal.", sentence(&e)))
                    .into_any_element(),
            )
        } else {
            note.map(|n| div().text_xs().text_color(g.muted).child(n).into_any_element())
        };
        body.extend(status);
        // Back, Next, Submit.
        let mut nav = h_flex().gap_2().items_center();
        if n > 1 && step > 0 {
            let id2 = id.clone();
            nav = nav.child(button(&format!("back-{id}"), "Back", false, busy).on_click(cx.listener(move |this, _, _, cx| this.go(&id2, step - 1, cx))));
        }
        nav = nav.child(div().flex_1());
        if n > 1 && step < n {
            let id2 = id.clone();
            let label = if step + 1 == n { "Review" } else { "Next" };
            nav = nav.child(button(&format!("next-{id}"), label, true, busy).on_click(cx.listener(move |this, _, _, cx| this.go(&id2, step + 1, cx))));
        } else {
            let s = set.clone();
            let label = if set.status == "failed" { "Try again" } else if n > 1 { "Submit" } else { "Answer" };
            nav = nav.child(button(&format!("submit-{id}"), label, true, busy).when(!busy, |el| el.on_click(cx.listener(move |this, _, _, cx| this.submit(&s, cx)))));
        }
        body.push(nav.into_any_element());

        h_flex()
            .w_full()
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .rounded(px(20.0))
                    .bg(rgba(0xffcf7024))
                    .border_1()
                    .border_color(rgba(0xffcf7099))
                    .children(body),
            )
            .into_any_element()
    }

    fn answer_summary(&self, set: &QuestionSet, t: usize, cx: &App) -> Option<String> {
        let recs = set.recommendations.as_ref().and_then(|r| r.iter().find(|r| r.tab == t));
        let choice = self.asks.cards.get(&set.id).and_then(|c| c.choices.get(t)).cloned().unwrap_or_default();
        let (text, files) = message_parts(&self.other_of(&set.id, t, cx));
        summary(&set.tabs[t], recs, &choice, &text, files.len())
    }

    /// One question: the recommendations, the dialog's options, your own answer.
    fn question_step(&mut self, set: &QuestionSet, t: usize, busy: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let tab = &set.tabs[t];
        let id = set.id.clone();
        let recs = set.recommendations.as_ref().and_then(|r| r.iter().find(|r| r.tab == t)).cloned();
        let chosen = self.asks.cards.get(&id).and_then(|c| c.choices.get(t)).cloned().unwrap_or_default();
        let now = effective(&chosen, recs.as_ref());
        let mut out: Vec<AnyElement> = vec![];
        if unread(set, t) && set.tabs.len() > 1 {
            out.push(div().text_base().font_weight(FontWeight::MEDIUM).child(tab.title.clone()).into_any_element());
            out.push(
                div()
                    .text_sm()
                    .text_color(g.muted)
                    .child("The tower hasn't read this question yet. It shows here in a moment; or answer it in the session.")
                    .into_any_element(),
            );
            return out;
        }
        let question = if tab.question.is_empty() { set.title.clone() } else { tab.question.clone() };
        out.push(div().text_base().font_weight(FontWeight::MEDIUM).line_height(px(22.0)).child(question).into_any_element());
        if t == 0 && !set.detail.is_empty() {
            out.push(
                div()
                    .px_2()
                    .py_1()
                    .rounded(px(8.0))
                    .bg(rgba(0x0000002e))
                    .font_family("Menlo")
                    .text_xs()
                    .line_clamp(8)
                    .child(set.detail.clone())
                    .into_any_element(),
            );
        }
        // The tower's three.
        match &recs {
            // No answers are asked for: say what it waits on, and reply when it's done.
            None if set.reason == crate::questions::WAITS => out.push(
                div().text_sm().text_color(g.muted).child("It waits for you to do this. Reply when it's done (\"merged, go on\").").into_any_element(),
            ),
            None => out.push(working("Thinking of answers… you can answer already.")),
            Some(r) => {
                for (i, rec) in r.options.iter().enumerate().take(3) {
                    let on = now == Choice::Rec(i);
                    let id2 = id.clone();
                    out.push(
                        v_flex()
                            .id(SharedString::from(format!("rec-{id}-{t}-{i}")))
                            .px_3()
                            .py_2()
                            .gap(px(2.0))
                            .rounded(px(14.0))
                            .cursor_pointer()
                            .when(on, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)))
                            .when(!on, |el| el.bg(rgba(0xffffff1f)).hover(|s| s.bg(rgba(0xffffff33))))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().flex_1().min_w_0().text_sm().font_weight(FontWeight::MEDIUM).child(rec.label.clone()))
                                    .when(i == 0, |el| {
                                        el.child(
                                            div()
                                                .flex_none()
                                                .px(px(7.0))
                                                .rounded_full()
                                                .text_xs()
                                                .bg(if on { rgba(0x16233a24) } else { rgba(0xffcf7059) })
                                                .child("Recommended"),
                                        )
                                    }),
                            )
                            .when(!rec.why.is_empty(), |el| el.child(div().text_xs().opacity(0.8).line_clamp(2).child(rec.why.clone())))
                            .when(!busy, |el| el.on_click(cx.listener(move |this, _, _, cx| this.choose(&id2, t, Choice::Rec(i), cx))))
                            .into_any_element(),
                    );
                }
            }
        }
        // The dialog's own options: folded behind one line while there are
        // recommendations (unless one of them is chosen).
        let picked_option = matches!(&now, Choice::Opts(v) if !v.is_empty());
        let folded = recs.is_some() && !picked_option && !self.asks.cards.get(&id).is_some_and(|c| c.options_open);
        if !tab.options.is_empty() && folded {
            let id2 = id.clone();
            out.push(
                div()
                    .id(SharedString::from(format!("options-{id}-{t}")))
                    .px_2()
                    .py(px(5.0))
                    .rounded(px(10.0))
                    .cursor_pointer()
                    .text_sm()
                    .text_color(g.muted)
                    .hover(|s| s.bg(rgba(0xffffff1a)))
                    .child(format!("The session's choices ({}) ›", tab.options.len()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.asks.cards.entry(id2.clone()).or_default().options_open = true;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        } else if !tab.options.is_empty() {
            out.push(div().pt_1().text_xs().text_color(g.faint).child(if tab.multi { "Or tick any of these" } else { "Or pick one of these" }).into_any_element());
            for (i, o) in tab.options.iter().enumerate() {
                let on = matches!(&now, Choice::Opts(v) if v.contains(&i));
                let id2 = id.clone();
                let multi = tab.multi;
                let mark = if multi { if on { "☑" } else { "☐" } } else if on { "●" } else { "○" };
                out.push(
                    h_flex()
                        .id(SharedString::from(format!("opt-{id}-{t}-{i}")))
                        .gap_2()
                        .items_start()
                        .px_2()
                        .py(px(5.0))
                        .rounded(px(10.0))
                        .cursor_pointer()
                        .when(on, |el| el.bg(rgba(0xffffff33)))
                        .when(!on, |el| el.hover(|s| s.bg(rgba(0xffffff1a))))
                        .child(div().flex_none().w(px(16.0)).text_sm().child(mark))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().child(o.label.clone()))
                                .when(!o.description.is_empty(), |el| el.child(div().text_xs().text_color(g.muted).line_clamp(2).child(o.description.clone()))),
                        )
                        .when(!busy, |el| {
                            el.on_click(cx.listener(move |this, _, _, cx| {
                                let current = this.asks.cards.get(&id2).and_then(|c| c.choices.get(t)).cloned().unwrap_or_default();
                                this.choose(&id2, t, toggle(&current, multi, i), cx);
                            }))
                        })
                        .into_any_element(),
                );
            }
        }
        // Your own answer.
        if tab.allows_other || tab.options.is_empty() {
            let open = self.asks.other.as_ref().filter(|(oid, ot, _)| *oid == id && *ot == t).map(|(_, _, f)| f.clone());
            match open {
                None => {
                    let kept = self.other_of(&id, t, cx);
                    let label = if kept.text().trim().is_empty() { "Other: my own answer…".to_string() } else { format!("Other: “{}”", kept.text().trim()) };
                    let on = now == Choice::Other;
                    let id2 = id.clone();
                    out.push(
                        div()
                            .id(SharedString::from(format!("other-{id}-{t}")))
                            .px_2()
                            .py(px(5.0))
                            .rounded(px(10.0))
                            .cursor_pointer()
                            .text_sm()
                            .truncate()
                            .when(on, |el| el.bg(rgba(0xffffff33)))
                            .when(!on, |el| el.text_color(g.muted).hover(|s| s.bg(rgba(0xffffff1a))))
                            .child(label)
                            .when(!busy, |el| el.on_click(cx.listener(move |this, _, window, cx| this.open_other(&id2, t, window, cx))))
                            .into_any_element(),
                    );
                }
                Some(field) => {
                    out.push(
                        h_flex()
                            .gap_1()
                            .items_end()
                            .pl_2()
                            .pr_1()
                            .py_1()
                            .rounded(px(14.0))
                            .bg(g.pill)
                            .text_color(g.pill_text)
                            .child(
                                div().flex_1().min_w_0().child(
                                    Textarea::new(&field).appearance(false).bordered(false).small().w_full().text_size(px(14.0)).text_color(g.pill_text),
                                ),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("other-files-{id}")))
                                    .size(px(26.0))
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .text_color(g.pill_muted)
                                    .hover(|s| s.bg(rgb(0xeef1f5)))
                                    .child(Icon::new(IconName::Plus).small())
                                    .on_click(cx.listener(|this, _, window, cx| this.pick_other_files(window, cx))),
                            )
                            .into_any_element(),
                    );
                    out.push(div().text_xs().text_color(g.faint).child("Hold Control to say it; drop or + to add files.").into_any_element());
                }
            }
        }
        out
    }

    /// The review: every answer, each one tap from changing it.
    fn review_step(&mut self, set: &QuestionSet, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let mut out: Vec<AnyElement> = vec![div().text_sm().font_weight(FontWeight::MEDIUM).child("Review your answers").into_any_element()];
        for t in 0..set.tabs.len() {
            let answer = self.answer_summary(set, t, cx);
            let id2 = set.id.clone();
            out.push(
                v_flex()
                    .id(SharedString::from(format!("review-{}-{t}", set.id)))
                    .px_2()
                    .py(px(5.0))
                    .rounded(px(10.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(0xffffff1a)))
                    .child(div().text_xs().text_color(g.muted).child(set.tabs[t].question.clone()))
                    .child(match answer {
                        Some(a) => div().text_sm().child(format!("→ {a}")),
                        None => div().text_sm().text_color(rgb(AMBER)).child("Not answered yet"),
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.go(&id2, t, cx)))
                    .into_any_element(),
            );
        }
        out
    }

    /// A job's line when its session asks something: one tap to the card.
    pub(super) fn job_asks(&mut self, session: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let set = self.open_sets(cx).into_iter().find(|s| s.session == session)?;
        Some(
            h_flex()
                .id(SharedString::from(format!("asks-{}", set.id)))
                .gap_2()
                .items_center()
                .px_2()
                .py(px(4.0))
                .rounded(px(10.0))
                .cursor_pointer()
                .bg(rgba(0xffcf7024))
                .hover(|s| s.bg(rgba(0xffcf7040)))
                .text_xs()
                .child(div().size(px(6.0)).flex_none().rounded_full().bg(rgb(AMBER)))
                .child(div().flex_1().min_w_0().truncate().child({
                    let who = if set.name.is_empty() { set.session.clone() } else { set.name.clone() };
                    format!("{who} asks: {}", set.title)
                }))
                .child(div().flex_none().font_weight(FontWeight::MEDIUM).child("Answer ›"))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.phone_leave();
                    this.view = View::Chat;
                    this.chat.scroll_to_bottom();
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// Settings: how much the tower answers by itself.
    pub(super) fn policy_items(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let now = self.asks.policy.clone().or_else(|| self.tower.read(cx).s("question_policy").as_str().map(String::from)).unwrap_or_else(|| "normal".into());
        let mut out: Vec<AnyElement> = vec![div().pt_2().text_xs().font_weight(FontWeight::MEDIUM).text_color(g.faint).child("QUESTIONS").into_any_element()];
        for (p, title, about) in [
            ("normal", "Normal", "The tower answers routine questions and asks you the rest (about 9 in 10 it handles)."),
            ("keep-moving", "Keep moving", "It answers the borderline ones too; never deploys, pushes, deletes, secrets or money."),
            ("ask-everything", "Ask me everything", "Nothing is answered for you: every question comes here."),
        ] {
            let on = now == p;
            out.push(
                v_flex()
                    .id(SharedString::from(format!("policy-{p}")))
                    .px_3()
                    .py_2()
                    .gap(px(2.0))
                    .rounded(px(14.0))
                    .cursor_pointer()
                    .when(on, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)))
                    .when(!on, |el| el.bg(g.bubble_tower).hover(|s| s.bg(rgba(0xffffff33))))
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                    .child(div().text_xs().opacity(0.8).child(about))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.asks.policy = Some(p.to_string());
                        let task = cx.background_executor().spawn(async move { crate::answers::set_policy(p) });
                        cx.spawn(async move |this, cx| {
                            let failed = task.await.err();
                            let _ = this.update(cx, |this, cx| {
                                if let Some(e) = failed {
                                    this.asks.policy = None;
                                    this.phone.note_error(format!("{e:#}"));
                                }
                                cx.notify();
                            });
                        })
                        .detach();
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        out
    }
}

/// `the dialog changed` as a sentence: `The dialog changed.`
pub(super) fn sentence(text: &str) -> String {
    let t = text.trim();
    let mut c = t.chars();
    let mut s = match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => return String::new(),
    };
    if !s.ends_with(['.', '!', '?']) {
        s.push('.');
    }
    s
}

fn working(text: &str) -> AnyElement {
    h_flex()
        .gap_2()
        .items_center()
        .text_xs()
        .text_color(glass().muted)
        .child(Spinner::new().xsmall().color(rgb(0xffffff).into()))
        .child(text.to_string())
        .into_any_element()
}

fn button(id: &str, text: &str, filled: bool, busy: bool) -> gpui::Stateful<gpui::Div> {
    let g = glass();
    div()
        .id(SharedString::from(id.to_string()))
        .flex_none()
        .px_3()
        .py(px(5.0))
        .rounded_full()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .when(filled, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)))
        .when(!filled, |el| el.border_1().border_color(g.edge))
        .when(busy, |el| el.opacity(0.5))
        .when(!busy, |el| el.cursor_pointer().hover(|s| s.opacity(0.85)))
        .child(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::questions::{Opt, Rec};

    #[test]
    fn a_card_carries_over_to_its_dialog_read_again() {
        let known = Tab { title: "Commit".into(), ..tab(false) };
        let unread_tab = Tab { title: "Unmute test".into(), allows_other: true, ..Default::default() };
        let read = Tab { title: "Unmute test".into(), question: "Run it on the real tower?".into(), ..tab(false) };
        let half = QuestionSet { id: "qhalf".into(), kind: "claude-ask".into(), tabs: vec![known.clone(), unread_tab], ..Default::default() };
        assert!(!unread(&half, 0) && unread(&half, 1));
        let whole = QuestionSet { id: "qwhole".into(), kind: "claude-ask".into(), tabs: vec![known, read], replaces: Some("qhalf".into()), ..Default::default() };
        let card = Card {
            step: 2,
            choices: vec![Choice::Opts(vec![1]), Choice::Rec(0)],
            error: Some("the questions changed since they were shown".into()),
            tabs: half.tabs.clone(),
            ..Default::default()
        };
        let c = carry_card(card, &whole);
        assert_eq!(c.choices, [Choice::Opts(vec![1]), Choice::None], "tab 0's answer stays; tab 1 starts over");
        assert_eq!(c.step, 1, "it shows the question that is new");
        assert!(c.error.is_none());
        assert!(c.note.as_deref().is_some_and(|n| n.contains("Unmute test")), "{:?}", c.note);
        assert!(!unread(&whole, 1));
        // Nothing changed: all kept, no note.
        let same = carry_card(Card { step: 2, choices: vec![Choice::Opts(vec![0]), Choice::Opts(vec![2])], tabs: whole.tabs.clone(), ..Default::default() }, &whole);
        assert_eq!((same.step, same.note.is_none(), same.choices.len()), (2, true, 2));
    }

    fn tab(multi: bool) -> Tab {
        let opt = |l: &str| Opt { label: l.into(), description: String::new(), key: String::new() };
        Tab { title: "DB".into(), question: "Which database?".into(), options: vec![opt("Postgres"), opt("SQLite"), opt("MySQL")], multi, allows_other: true, answered: false }
    }

    fn recs() -> TabRecs {
        TabRecs {
            tab: 0,
            options: vec![
                Rec { label: "SQLite".into(), why: "one file".into(), choice: json!(1) },
                Rec { label: "Postgres".into(), why: String::new(), choice: json!(0) },
            ],
        }
    }

    #[test]
    fn a_failure_reads_as_a_sentence() {
        assert_eq!(sentence("stopped: the dialog changed"), "Stopped: the dialog changed.");
        assert_eq!(sentence("Already said."), "Already said.");
        assert_eq!(sentence(""), "");
    }

    #[test]
    fn the_first_recommendation_is_the_default() {
        let r = recs();
        assert_eq!(answer_of(0, &tab(false), Some(&r), &Choice::None, "", &[]), Some(json!({ "tab": 0, "choice": 1 })));
        // While the brain thinks there is no default: nothing to send yet.
        assert_eq!(answer_of(0, &tab(false), None, &Choice::None, "", &[]), None);
        assert_eq!(summary(&tab(false), Some(&r), &Choice::None, "", 0).as_deref(), Some("SQLite"));
    }

    #[test]
    fn options_and_your_own_words_become_answers() {
        let t = tab(false);
        assert_eq!(answer_of(2, &t, None, &Choice::Opts(vec![2]), "", &[]), Some(json!({ "tab": 2, "choice": 2 })));
        let files = vec!["/a/sketch.png".to_string()];
        assert_eq!(
            answer_of(0, &t, None, &Choice::Other, " [Image #1] like this\n", &files),
            Some(json!({ "tab": 0, "choice": { "other": "[Image #1] like this" }, "attachments": ["/a/sketch.png"] }))
        );
        // A typed answer with nothing typed is no answer.
        assert_eq!(answer_of(0, &t, Some(&recs()), &Choice::Other, "  ", &[]), None);
        assert_eq!(summary(&t, None, &Choice::Other, "Railway, then\nmore", 1).as_deref(), Some("“Railway, then” + 1 file"));

        let m = tab(true);
        let ticked = toggle(&toggle(&Choice::None, true, 2), true, 0);
        assert_eq!(ticked, Choice::Opts(vec![0, 2]));
        assert_eq!(toggle(&ticked, true, 2), Choice::Opts(vec![0]));
        assert_eq!(answer_of(1, &m, None, &ticked, "", &[]), Some(json!({ "tab": 1, "choice": { "options": [0, 2] } })));
        assert_eq!(answer_of(1, &m, None, &ticked, "CSV", &[]), Some(json!({ "tab": 1, "choice": { "options": [0, 2], "other": "CSV" } })));
        assert_eq!(summary(&m, None, &ticked, "CSV", 0).as_deref(), Some("Postgres, MySQL, “CSV”"));
        // On a single-choice question a click replaces the choice.
        assert_eq!(toggle(&Choice::Opts(vec![0]), false, 1), Choice::Opts(vec![1]));
    }

    #[test]
    fn only_what_the_tower_leaves_to_you_gets_a_card() {
        let set = |id: &str, decision: Option<&str>, at: u64| {
            json!({ "id": id, "session": "t1", "kind": "claude-ask", "tabs": [{ "question": "Q?" }], "decision": decision, "at_ms": at })
        };
        let state = json!({ "questions": [set("b", Some("escalate"), 2), set("a", Some("escalate"), 1), set("c", Some("auto"), 3), set("d", None, 4)] });
        let ids: Vec<String> = escalated(&state, &HashSet::new()).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["a", "b"]);
        let gone: HashSet<String> = ["a".to_string()].into();
        assert_eq!(escalated(&state, &gone).len(), 1);
        assert!(escalated(&json!({}), &HashSet::new()).is_empty());
    }
}
