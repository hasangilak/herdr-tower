//! What sessions ask (see `crate::questions`), in the queue: the card of a
//! question set the tower leaves to the user (`decision: escalate`), as
//! `ask_card` draws it (C1 to C4 in the design). A set's questions go one at
//! a time (Back, Next, a review, Submit, as the terminal dialog does); each
//! shows the tower's three recommendations as one-tap answers (the first
//! its pick, with its reason; placeholders while they come, never in the
//! way), the dialog's own options, and "Other": your own answer, typed,
//! spoken (Control, as anywhere in the window) or with files. A tap on an
//! answer of a single question sends it; with several questions it goes on
//! to the next, and the last step reviews them all before Submit. The number
//! keys do what the taps do (`keys`). Answers go through the daemon
//! (`crate::answers`), which types them into the session. The policy (how
//! much the tower answers by itself) is in Settings.

use std::collections::{HashMap, HashSet};

use gpui::{AnyElement, App, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::input::{InputContent, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use serde_json::{Value, json};

use super::home::{LINE, SOFT, acts_row, card_box, card_title, cstat, dismiss_mark, kbd, mbtn, meta_row};
use super::{AMBER, NAVY, PALE, Panel, ROSE, glass, message_parts};
use crate::orb::model::{self, Decision};
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
    /// It stopped and the user wants another go: the questions show again.
    again: bool,
    /// Where the dialog was (its tab, and which tabs were answered there)
    /// when the card last followed it.
    followed: Option<(usize, Vec<bool>)>,
}

/// A tab of a set's dialog the user answered in the session's terminal:
/// it stands, and the card asks it no more.
pub(super) fn there(set: &QuestionSet, t: usize) -> bool {
    set.kind == "claude-ask" && set.tabs.len() > 1 && set.tabs.get(t).is_some_and(|tab| tab.answered)
}

/// Every question of the set was answered in the terminal: only Submit is left.
pub(super) fn all_there(set: &QuestionSet) -> bool {
    (0..set.tabs.len()).all(|t| there(set, t))
}

/// The first question still open after `t` (the review: `tabs.len()`).
pub(super) fn next_open(set: &QuestionSet, t: usize) -> usize {
    (t + 1..set.tabs.len()).find(|i| !there(set, *i)).unwrap_or(set.tabs.len())
}

/// The card follows its dialog: when the dialog shows another tab, or other
/// tabs answered (the user answering it in the terminal), the card goes to
/// where the dialog is (its review to the card's). A card the user moves on
/// the Mac stays where they put it while the dialog stays put.
pub(super) fn follow_dialog(card: &mut Card, set: &QuestionSet) {
    let n = set.tabs.len();
    let now = (set.current_tab, set.tabs.iter().map(|t| t.answered).collect::<Vec<bool>>());
    if n < 2 || card.sending || card.followed.as_ref() == Some(&now) {
        return;
    }
    card.followed = Some(now);
    card.step = set.current_tab.min(n);
}

/// What a number key, or the tap it stands for, does on a set's card.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Key {
    /// A recommendation, by its place.
    Rec(usize),
    /// An option of the dialog, by its place.
    Opt(usize),
    Submit,
    /// Back to the first question.
    Change,
    /// Show the session in Herdr.
    Show,
    /// Answer it again.
    Again,
}

/// The keys of a set's card as it shows, key 1 first. On a question: the
/// tower's recommendations, or the dialog's own options while those are
/// still coming. On the review: Submit, then Change. On a card that
/// stopped: the session, then another go. None while an answer goes in.
pub(super) fn keys(set: &QuestionSet, step: usize, busy: bool, again: bool) -> Vec<Key> {
    if busy {
        return vec![];
    }
    if set.status == "failed" && !again {
        return vec![Key::Show, Key::Again];
    }
    let n = set.tabs.len();
    if n > 1 && step >= n {
        return if all_there(set) { vec![Key::Submit] } else { vec![Key::Submit, Key::Change] };
    }
    if there(set, step) {
        return vec![];
    }
    let Some(tab) = set.tabs.get(step) else { return vec![] };
    if unread(set, step) && n > 1 {
        return vec![];
    }
    match set.recommendations.as_ref().and_then(|r| r.iter().find(|r| r.tab == step)) {
        Some(r) if !r.options.is_empty() => (0..r.options.len().min(3)).map(Key::Rec).collect(),
        _ => (0..tab.options.len().min(9)).map(Key::Opt).collect(),
    }
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
    /// The policy just chosen, until the daemon says it too.
    pub policy: Option<String>,
}

impl Asks {
    /// The "Other" field, if it is open and has the keyboard: dictation and
    /// files go there.
    pub(super) fn other_focused(&self, window: &Window, cx: &App) -> Option<Entity<TextareaState>> {
        let (_, _, field) = self.other.as_ref()?;
        gpui::Focusable::focus_handle(field.read(cx), cx).is_focused(window).then(|| field.clone())
    }

    /// The "Other" field open on the card `id` (a decision's, or a question set's).
    pub(super) fn other_of_card(&self, id: &str) -> Option<Entity<TextareaState>> {
        self.other.as_ref().filter(|(oid, _, _)| oid == id).map(|(_, _, f)| f.clone())
    }

    /// Lets the open "Other" field go (what it held was sent).
    pub(super) fn close_other(&mut self) {
        self.other = None;
    }

}

impl Panel {
    pub(super) fn open_sets(&self, cx: &App) -> Vec<QuestionSet> {
        let t = self.tower.read(cx);
        escalated(t.state.as_ref().unwrap_or(&Value::Null), &t.acts.dismissed)
    }

    /// Looks at the open sets again: a card whose set is gone goes, and one
    /// whose set was read again under a new id carries on.
    pub(super) fn watch_sets(&mut self, cx: &mut Context<Self>) {
        let sets = self.open_sets(cx);
        let mut open: HashSet<String> = sets.iter().map(|s| s.id.clone()).collect();
        // A set read again under a new id: its card carries on (what was chosen, the open field).
        for set in &sets {
            let Some(old) = set.replaces.as_ref().filter(|old| !open.contains(*old)) else { continue };
            if let Some(card) = self.asks.cards.remove(old) {
                self.asks.cards.insert(set.id.clone(), carry_card(card, set));
            }
            if let Some((oid, _, _)) = self.asks.other.as_mut().filter(|(oid, _, _)| oid == old) {
                *oid = set.id.clone();
            }
            // (Without a notice: this runs as the tower's state is observed.)
            if self.acts(cx).dismissed.contains(old) {
                let id = set.id.clone();
                self.tower.update(cx, |t, _| t.acts.dismissed.insert(id));
            }
        }
        // Each card follows its dialog (answered tab by tab in the terminal, it moves on).
        for set in &sets {
            if let Some(card) = self.asks.cards.get_mut(&set.id) {
                follow_dialog(card, set);
            }
        }
        // A decision with no dialog behind it has a card too (its "Other" field).
        open.extend(self.decisions(cx).into_iter().filter(|d| d.live()).map(|d| d.id));
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

    /// Opens "Other" on a question, or on a decision's card (`tab` 0),
    /// keeping what was typed on another.
    pub(super) fn open_other(&mut self, id: &str, tab: usize, hint: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.keep_other(cx);
        let content = self.asks.cards.get(id).and_then(|c| c.others.get(tab).cloned().flatten()).unwrap_or_else(|| InputContent::new(""));
        let field = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 5).placeholder(hint));
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
            // Answered in the terminal: it stands, and nothing is sent for it.
            if there(set, t) {
                continue;
            }
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
        // What the queue says once it has landed.
        let said = match (set.tabs.len(), answers.len()) {
            (1, _) => model::receipt_text(&self.answer_summary(set, 0, cx).unwrap_or_default()),
            (_, 0) => "Submitted the answers given in the terminal.".to_string(),
            (_, 1) => "Answered 1 question.".to_string(),
            (_, k) => format!("Answered {k} questions."),
        };
        // The decision this set stands for. The daemon's own (`d…`) is
        // answered by its id, so every session that asks the same gets the
        // answer; an older daemon's is the set itself.
        let asked = self
            .decisions(cx)
            .into_iter()
            .find(|d| d.question_id.as_deref() == Some(set.id.as_str()) || d.sessions.iter().any(|s| s.question_id.as_deref() == Some(set.id.as_str())));
        // One question, answered in words: "Make it a rule" is offered where its answer may become one.
        let rule = match (set.tabs.len(), &asked) {
            (1, Some(d)) => self.answer_summary(set, 0, cx).and_then(|answer| model::rule_offer(d, &answer)),
            _ => None,
        };
        let decision = asked.map(|d| d.id).unwrap_or_else(|| set.id.clone());
        let target = if crate::decisions::is_id(&decision) { decision.clone() } else { set.id.clone() };
        self.tower.update(cx, |t, cx| {
            t.deck.answered_with(&decision, said, rule);
            // An answer on its way, in either window.
            t.acts.sent.insert(set.id.clone());
            cx.notify();
        });
        let card = self.asks.cards.entry(set.id.clone()).or_default();
        card.sending = true;
        card.again = false;
        card.error = None;
        card.note = None;
        let id = set.id.clone();
        // A decision's answer says which set's card it was given on: the tower types it only
        // where it means the same, and nowhere if that card is gone.
        let body = match crate::decisions::is_id(&target) {
            true => json!({ "answers": answers, "shown": set.id }),
            false => Value::Array(answers),
        };
        let task = cx.background_executor().spawn(async move { crate::answers::answer(&target, body, true) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.tower.update(cx, |t, cx| {
                    t.acts.sent.remove(&id);
                    if result.is_err() {
                        t.deck.retract(&decision);
                    }
                    cx.notify();
                });
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

    /// Takes a card off the queue (`tower dismiss`): nothing is sent. The
    /// daemon's own decision goes by its id, and its sessions' question sets
    /// with it; an older daemon's is the set itself.
    pub(super) fn dismiss_card(&mut self, d: &Decision, cx: &mut Context<Self>) {
        let sets: Vec<String> = d.question_id.iter().cloned().chain(d.sessions.iter().filter_map(|s| s.question_id.clone())).collect();
        self.act(cx, |a| {
            a.dismissed.insert(d.id.clone());
            a.dismissed.extend(sets.iter().cloned());
        });
        let target = if crate::decisions::is_id(&d.id) { d.id.clone() } else { d.question_id.clone().unwrap_or_else(|| d.id.clone()) };
        cx.background_executor().spawn(async move { crate::answers::dismiss(&target) }).detach();
        cx.notify();
    }

    fn answer_summary(&self, set: &QuestionSet, t: usize, cx: &App) -> Option<String> {
        let recs = set.recommendations.as_ref().and_then(|r| r.iter().find(|r| r.tab == t));
        let choice = self.asks.cards.get(&set.id).and_then(|c| c.choices.get(t)).cloned().unwrap_or_default();
        let (text, files) = message_parts(&self.other_of(&set.id, t, cx));
        summary(&set.tabs[t], recs, &choice, &text, files.len())
    }

}


// ---------------------------------------------------------------------------
// A set as the queue's card.

impl Panel {
    /// The number keys of a set's card as it shows now.
    pub(super) fn ask_keys(&self, set: &QuestionSet) -> Vec<Key> {
        let card = self.asks.cards.get(&set.id);
        let n = set.tabs.len();
        let step = card.map(|c| c.step).unwrap_or(0).min(if n > 1 { n } else { 0 });
        let busy = card.is_some_and(|c| c.sending) || matches!(set.status.as_str(), "answering" | "queued");
        keys(set, step, busy, card.is_some_and(|c| c.again))
    }

    /// Does what a key of the card stands for.
    pub(super) fn ask_key(&mut self, set: &QuestionSet, key: Key, cx: &mut Context<Self>) {
        let step = self.asks.cards.get(&set.id).map(|c| c.step).unwrap_or(0);
        match key {
            Key::Rec(i) => self.tap(set, step, Choice::Rec(i), cx),
            Key::Opt(i) => {
                let current = self.asks.cards.get(&set.id).and_then(|c| c.choices.get(step)).cloned().unwrap_or_default();
                let multi = set.tabs.get(step).is_some_and(|t| t.multi);
                self.tap(set, step, toggle(&current, multi, i), cx)
            }
            Key::Submit => self.submit(set, cx),
            Key::Change => self.go(&set.id, if there(set, 0) { next_open(set, 0) } else { 0 }, cx),
            Key::Show => self.show_in_herdr(&set.session, cx),
            Key::Again => {
                self.asks.cards.entry(set.id.clone()).or_default().again = true;
                cx.notify();
            }
        }
    }

    /// A tap on an answer: it is chosen, and unless it only ticks one of
    /// several it is the answer, sent (one question) or kept for the review.
    fn tap(&mut self, set: &QuestionSet, t: usize, choice: Choice, cx: &mut Context<Self>) {
        let ticks = set.tabs.get(t).is_some_and(|tab| tab.multi) && matches!(choice, Choice::Opts(_));
        self.choose(&set.id, t, choice, cx);
        if !ticks {
            self.commit(set, t, cx);
        }
    }

    /// The question is answered: send it, or on to the next one.
    fn commit(&mut self, set: &QuestionSet, t: usize, cx: &mut Context<Self>) {
        if set.tabs.len() == 1 { self.submit(set, cx) } else { self.go(&set.id, next_open(set, t), cx) }
    }

    /// A question set as the queue's open card: who waits and why, then one
    /// question at a time, the review, or what stopped it.
    pub(super) fn ask_card(&mut self, d: &Decision, set: &QuestionSet, tag: &str, now_ms: u64, cx: &mut Context<Self>) -> AnyElement {
        let n = set.tabs.len();
        let id = set.id.clone();
        let (step, sending, error, note, again) = {
            let c = self.asks.cards.entry(id.clone()).or_default();
            follow_dialog(c, set);
            c.step = c.step.min(if n > 1 { n } else { 0 });
            if c.tabs.is_empty() {
                c.tabs = set.tabs.clone();
            }
            if set.status != "failed" {
                c.again = false;
            }
            (c.step, c.sending, c.error.clone(), c.note.clone(), c.again)
        };
        let busy = sending || matches!(set.status.as_str(), "answering" | "queued");
        let stopped = set.status == "failed" && !again;
        let keys = keys(set, step, busy, again);
        let key_of = |k: Key| keys.iter().position(|x| *x == k);
        let (tag, rest) = if stopped {
            (tag.to_string(), "stopped".to_string())
        } else if n > 1 && step >= n && all_there(set) {
            ("Review".to_string(), format!("{n} answered in the terminal"))
        } else if n > 1 && step >= n {
            ("Review".to_string(), format!("{n} answers"))
        } else if n > 1 {
            (format!("{} of {n}", step + 1), format!("{} · {}", d.stake(), d.waited(now_ms)))
        } else {
            (tag.to_string(), format!("{} · {}", d.stake(), d.waited(now_ms)))
        };
        let d2 = d.clone();
        let mut body: Vec<AnyElement> = vec![
            h_flex()
                .gap_2()
                .items_start()
                .child(meta_row(Some(&tag), &d.space, &d.badge(), rest).flex_1().min_w_0())
                .child(dismiss_mark(format!("dismiss-{id}")).on_click(cx.listener(move |this, _, _, cx| this.dismiss_card(&d2, cx))))
                .into_any_element(),
        ];
        if stopped {
            // The answer stopped part-way: nothing more is typed until the user says.
            let why = error.clone().or_else(|| set.error.clone()).unwrap_or_else(|| "It stopped.".into());
            let (s1, s2) = (set.clone(), set.clone());
            body.push(card_title("It stopped"));
            body.push(cstat(format!("{} Nothing more was typed. Try again, or finish in the session.", sentence(&why)), true));
            body.push(
                acts_row()
                    .child(mbtn(format!("show-{id}"), "Open the session", true, key_of(Key::Show)).on_click(cx.listener(move |this, _, _, cx| this.ask_key(&s1, Key::Show, cx))))
                    .child(mbtn(format!("again-{id}"), "Try again", false, key_of(Key::Again)).on_click(cx.listener(move |this, _, _, cx| this.ask_key(&s2, Key::Again, cx))))
                    .into_any_element(),
            );
            return card_box(true).children(body).into_any_element();
        }
        if step < n {
            body.extend(self.ask_step(d, set, step, busy, &keys, cx));
        } else {
            body.extend(self.ask_review(set, busy, &keys, cx));
        }
        if sending {
            body.push(working("Sending…"));
        } else if let Some(e) = error {
            body.push(cstat(sentence(&e), true));
        } else if let Some(n) = note {
            body.push(cstat(n, false));
        }
        card_box(true).children(body).into_any_element()
    }

    /// One question: the tower's answers as rows (the first its pick, with
    /// why), the dialog's own options as chips, and "Other".
    fn ask_step(&mut self, d: &Decision, set: &QuestionSet, t: usize, busy: bool, keys: &[Key], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let tab = &set.tabs[t];
        let (id, n) = (set.id.clone(), set.tabs.len());
        let recs = set.recommendations.as_ref().and_then(|r| r.iter().find(|r| r.tab == t)).cloned();
        let chosen = self.asks.cards.get(&id).and_then(|c| c.choices.get(t)).cloned().unwrap_or_default();
        let now = effective(&chosen, recs.as_ref());
        let key_of = |k: Key| keys.iter().position(|x| *x == k);
        let mut out: Vec<AnyElement> = vec![];
        if there(set, t) {
            out.push(card_title(if tab.question.is_empty() { &tab.title } else { &tab.question }));
            out.push(cstat(answered_there_line(tab), false));
            out.extend(self.ask_nav(set, t, false, busy, cx));
            return out;
        }
        if unread(set, t) && n > 1 {
            out.push(card_title(&tab.title));
            out.push(cstat("The tower hasn't read this question yet. It shows here in a moment; or answer it in the session.", false));
            out.extend(self.ask_nav(set, t, false, busy, cx));
            return out;
        }
        let question = if tab.question.is_empty() { set.title.clone() } else { tab.question.clone() };
        out.push(card_title(&question));
        // A permission is a short card: its answers as buttons, as the dialog has them.
        let short = n == 1 && matches!(set.kind.as_str(), "claude-permission" | "codex-menu");
        if t == 0 {
            // Who asks (every session that asks the same), and about what.
            let who = asking(d, set);
            let line = if set.context.is_empty() { who } else { format!("{who} · {}", set.context) };
            out.push(div().text_size(px(12.0)).text_color(g.faint).truncate().child(line).into_any_element());
            // Why it came to the user, when that takes more than the card's word for it.
            out.extend(d.reason().map(|why| cstat(sentence(&why), false)));
            if !set.detail.is_empty() {
                out.push(
                    div().px_2().py_1().rounded(px(8.0)).bg(rgba(0x0000002e)).font_family("Menlo").text_xs().line_clamp(if short { 4 } else { 8 }).child(set.detail.clone()).into_any_element(),
                );
            }
        }
        // The dialog's own options a short card's buttons don't already stand
        // for: folded away until asked for (or one of them is chosen).
        let stood_for: Vec<usize> = match &recs {
            Some(r) if short => r.options.iter().take(3).filter_map(|rec| rec.choice.as_u64().map(|c| c as usize)).collect(),
            _ => vec![],
        };
        let own: Vec<usize> = (0..tab.options.len()).filter(|i| !stood_for.contains(i)).collect();
        let more_folded = short
            && recs.as_ref().is_some_and(|r| !r.options.is_empty())
            && !own.is_empty()
            && !matches!(&now, Choice::Opts(v) if !v.is_empty())
            && !self.asks.cards.get(&id).is_some_and(|c| c.options_open);
        // The tower's three.
        match &recs {
            // No answers are asked for: say what it waits on, and reply when it's done.
            None if set.reason == crate::questions::WAITS => {
                out.push(cstat("It waits for you to do this. Reply when it's done (\"merged, go on\").", false))
            }
            None => {
                out.push(v_flex().gap(px(6.0)).children((0..3).map(|_| div().h(px(40.0)).rounded(px(12.0)).bg(rgba(SOFT)))).into_any_element());
                out.push(cstat("The tower is working out its three answers.", false));
            }
            Some(r) if short => {
                let mut row = acts_row();
                for (i, rec) in r.options.iter().enumerate().take(3) {
                    let s = set.clone();
                    let mut b = mbtn(format!("pick-{id}-{t}-{i}"), &rec.label, now == Choice::Rec(i), key_of(Key::Rec(i)))
                        .when(!busy, |el| el.on_click(cx.listener(move |this, _, _, cx| this.tap(&s, t, Choice::Rec(i), cx))));
                    if !rec.why.is_empty() {
                        let why = rec.why.clone();
                        b = b.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(why.clone()).build(window, cx));
                    }
                    row = row.child(b);
                }
                // The dialog's other options wait behind one word.
                if more_folded {
                    let id2 = id.clone();
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("more-{id}-{t}")))
                            .px(px(6.0))
                            .py(px(4.0))
                            .rounded_full()
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .text_color(g.muted)
                            .hover(|s| s.bg(rgba(SOFT)))
                            .child("More ›")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.asks.cards.entry(id2.clone()).or_default().options_open = true;
                                cx.notify();
                            })),
                    );
                }
                out.push(row.into_any_element());
            }
            Some(r) => {
                let mut rows = v_flex().gap(px(6.0));
                for (i, rec) in r.options.iter().enumerate().take(3) {
                    let on = now == Choice::Rec(i);
                    let s = set.clone();
                    rows = rows.child(
                        v_flex()
                            .id(SharedString::from(format!("pick-{id}-{t}-{i}")))
                            .px(px(12.0))
                            .py(px(8.0))
                            .gap(px(1.0))
                            .rounded(px(12.0))
                            .border_1()
                            .cursor_pointer()
                            .when(on, |el| el.bg(rgb(AMBER)).border_color(rgb(AMBER)).text_color(rgb(NAVY)))
                            .when(!on, |el| el.border_color(rgba(LINE)).hover(|s| s.bg(rgba(SOFT))))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .children(key_of(Key::Rec(i)).map(kbd))
                                    .child(div().flex_1().min_w_0().text_size(px(13.0)).font_weight(FontWeight::SEMIBOLD).child(rec.label.clone()))
                                    .when(i == 0, |el| el.child(div().flex_none().text_size(px(11.0)).font_weight(FontWeight::BOLD).child("tower’s pick"))),
                            )
                            .when(!rec.why.is_empty(), |el| el.child(div().text_size(px(12.0)).opacity(0.82).line_clamp(2).child(rec.why.clone())))
                            .when(!busy, |el| el.on_click(cx.listener(move |this, _, _, cx| this.tap(&s, t, Choice::Rec(i), cx)))),
                    );
                }
                out.push(rows.into_any_element());
            }
        }
        // The dialog's own options, as chips.
        if !own.is_empty() && !more_folded {
            let mut row = h_flex()
                .flex_wrap()
                .gap(px(6.0))
                .items_center()
                .text_size(px(12.0))
                .text_color(g.muted)
                .child(if tab.multi { "Tick any of the session’s options" } else { "The session’s own options" });
            for (i, o) in own.iter().map(|i| (*i, &tab.options[*i])) {
                let on = matches!(&now, Choice::Opts(v) if v.contains(&i));
                let (s, multi) = (set.clone(), tab.multi);
                let mut chip = h_flex()
                    .id(SharedString::from(format!("own-{id}-{t}-{i}")))
                    .max_w_full()
                    .gap(px(5.0))
                    .items_center()
                    .px(px(9.0))
                    .py(px(2.0))
                    .rounded(px(11.0))
                    .border_1()
                    .cursor_pointer()
                    .when(on, |el| el.bg(rgb(PALE)).border_color(rgb(PALE)).text_color(rgb(NAVY)))
                    .when(!on, |el| el.border_color(rgba(LINE)).hover(|s| s.bg(rgba(SOFT))))
                    .children(key_of(Key::Opt(i)).map(kbd))
                    .child(div().min_w_0().child(o.label.clone()))
                    .when(!busy, |el| {
                        el.on_click(cx.listener(move |this, _, _, cx| {
                            let current = this.asks.cards.get(&s.id).and_then(|c| c.choices.get(t)).cloned().unwrap_or_default();
                            this.tap(&s, t, toggle(&current, multi, i), cx);
                        }))
                    });
                if !o.description.is_empty() {
                    let about = o.description.clone();
                    chip = chip.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(about.clone()).build(window, cx));
                }
                row = row.child(chip);
            }
            out.push(row.into_any_element());
        }
        // Your own answer.
        let mut typing = false;
        if tab.allows_other || tab.options.is_empty() {
            let open = self.asks.other.as_ref().filter(|(oid, ot, _)| *oid == id && *ot == t).map(|(_, _, f)| f.clone());
            match open {
                None => {
                    let kept = self.other_of(&id, t, cx);
                    let kept = kept.text().trim().to_string();
                    typing = !kept.is_empty() && now == Choice::Other;
                    let id2 = id.clone();
                    out.push(
                        div()
                            .id(SharedString::from(format!("other-{id}-{t}")))
                            .px(px(12.0))
                            .py(px(9.0))
                            .rounded(px(12.0))
                            .bg(g.pill)
                            .cursor_pointer()
                            .text_size(px(13.0))
                            .line_clamp(2)
                            .text_color(if kept.is_empty() { g.pill_muted } else { g.pill_text })
                            .child(if kept.is_empty() { "Other… type it, hold Control and say it, or add files".to_string() } else { format!("Other: “{kept}”") })
                            .when(!busy, |el| {
                                el.on_click(cx.listener(move |this, _, window, cx| this.open_other(&id2, t, "Your answer: type it, or talk (Control), or add files", window, cx)))
                            })
                            .into_any_element(),
                    );
                }
                Some(field) => {
                    typing = true;
                    out.push(
                        h_flex()
                            .gap_1()
                            .items_end()
                            .pl_2()
                            .pr_1()
                            .py_1()
                            .rounded(px(12.0))
                            .bg(g.pill)
                            .text_color(g.pill_text)
                            .child(
                                div().flex_1().min_w_0().child(
                                    Textarea::new(&field).appearance(false).bordered(false).small().w_full().text_size(px(14.0)).text_color(g.pill_text),
                                ),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("other-add-{id}")))
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
                }
            }
        }
        // A typed answer and ticks are sent with a button; a tap needs none.
        out.extend(self.ask_nav(set, t, tab.multi || typing, busy, cx));
        out
    }

    /// Back, and Next (several questions) or Answer (ticks, a typed answer).
    fn ask_nav(&mut self, set: &QuestionSet, t: usize, send: bool, busy: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (id, n) = (set.id.clone(), set.tabs.len());
        let back = n > 1 && t > 0;
        let next = n > 1 || send;
        if !back && !next {
            return None;
        }
        let mut row = acts_row();
        if back {
            let id2 = id.clone();
            row = row.child(mbtn(format!("back-{id}"), "Back", false, None).on_click(cx.listener(move |this, _, _, cx| this.go(&id2, t - 1, cx))));
        }
        if next {
            let label = if n == 1 { "Answer" } else if next_open(set, t) == n { "Review" } else { "Next" };
            let s = set.clone();
            row = row.child(
                mbtn(format!("next-{id}"), label, n == 1, None)
                    .when(busy, |el| el.opacity(0.5))
                    .when(!busy, |el| el.on_click(cx.listener(move |this, _, _, cx| this.commit(&s, t, cx)))),
            );
        }
        Some(row.into_any_element())
    }

    /// The last step: every answer, each one tap from changing it, then Submit.
    fn ask_review(&mut self, set: &QuestionSet, busy: bool, keys: &[Key], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let key_of = |k: Key| keys.iter().position(|x| *x == k);
        let mut out: Vec<AnyElement> = vec![card_title("Review your answers")];
        let mut rows = v_flex().gap(px(6.0));
        for t in 0..set.tabs.len() {
            let answer = if there(set, t) { Some(answered_there_line(&set.tabs[t])) } else { self.answer_summary(set, t, cx) };
            let id2 = set.id.clone();
            let tab = &set.tabs[t];
            rows = rows.child(
                v_flex()
                    .id(SharedString::from(format!("answer-{}-{t}", set.id)))
                    .px(px(10.0))
                    .py(px(7.0))
                    .gap(px(1.0))
                    .rounded(px(10.0))
                    .bg(rgba(SOFT))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(LINE)))
                    .text_size(px(13.0))
                    .child(div().font_weight(FontWeight::SEMIBOLD).line_clamp(2).child(if tab.question.is_empty() { tab.title.clone() } else { tab.question.clone() }))
                    .child(match answer {
                        Some(a) => div().text_color(rgb(AMBER)).child(a),
                        None => div().text_color(rgb(ROSE)).child("Not answered yet"),
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.go(&id2, t, cx))),
            );
        }
        out.push(rows.into_any_element());
        let (s1, s2) = (set.clone(), set.clone());
        // Everything answered in the terminal: only Submit is left to do.
        let change = !all_there(set);
        out.push(
            acts_row()
                .child(
                    mbtn(format!("submit-{}", set.id), "Submit", true, key_of(Key::Submit))
                        .when(busy, |el| el.opacity(0.5))
                        .when(!busy, |el| el.on_click(cx.listener(move |this, _, _, cx| this.ask_key(&s1, Key::Submit, cx)))),
                )
                .when(change, |row| {
                    row.child(mbtn(format!("change-{}", set.id), "Change", false, key_of(Key::Change)).on_click(cx.listener(move |this, _, _, cx| this.ask_key(&s2, Key::Change, cx))))
                })
                .into_any_element(),
        );
        out
    }
}

/// What a card says of a question answered in the session's terminal: with
/// its answer when the screen showed it.
pub(super) fn answered_there_line(tab: &Tab) -> String {
    if tab.answer.trim().is_empty() { "Answered in the terminal".to_string() } else { format!("Answered in the terminal: {}", tab.answer.trim()) }
}

/// Who asks a set's question: its session, and with it every other session
/// of its decision (`eyzflow, mediaroom and project ask the same`).
pub(super) fn asking(d: &Decision, set: &QuestionSet) -> String {
    let own = if set.name.is_empty() { set.session.clone() } else { set.name.clone() };
    let others: Vec<&str> = d.sessions.iter().filter(|s| s.address != set.session).map(|s| s.label()).collect();
    match others.as_slice() {
        [] => own,
        [one] => format!("{own} and {one} ask the same"),
        [rest @ .., last] => format!("{own}, {} and {last} ask the same", rest.join(", ")),
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

    fn asked(tabs: usize, recs: Option<usize>, status: &str) -> QuestionSet {
        let one = Tab { title: "DB".into(), question: "Which database?".into(), options: vec![Opt::default(), Opt::default(), Opt::default(), Opt::default()], multi: false, allows_other: true, answered: false, answer: String::new() };
        QuestionSet {
            id: "q".into(),
            kind: "claude-ask".into(),
            tabs: vec![one; tabs],
            recommendations: recs.map(|n| (0..tabs).map(|tab| TabRecs { tab, options: vec![Rec::default(); n] }).collect()),
            status: status.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_number_keys_reach_what_the_card_shows() {
        // The tower's three, the first its pick.
        assert_eq!(keys(&asked(1, Some(3), "open"), 0, false, false), [Key::Rec(0), Key::Rec(1), Key::Rec(2)]);
        // More than three are never shown.
        assert_eq!(keys(&asked(1, Some(5), "open"), 0, false, false).len(), 3);
        // Still coming (or none came): the dialog's own options answer.
        assert_eq!(keys(&asked(1, None, "open"), 0, false, false), [Key::Opt(0), Key::Opt(1), Key::Opt(2), Key::Opt(3)]);
        assert_eq!(keys(&asked(1, Some(0), "open"), 0, false, false).len(), 4);
        // Several questions: each step its own, then the review.
        let several = asked(3, Some(2), "open");
        assert_eq!(keys(&several, 1, false, false), [Key::Rec(0), Key::Rec(1)]);
        assert_eq!(keys(&several, 3, false, false), [Key::Submit, Key::Change]);
        // A question the tower hasn't read yet has nothing to press.
        let mut half = asked(2, None, "open");
        half.tabs[1].question.clear();
        assert!(keys(&half, 1, false, false).is_empty());
        // While the answer goes in, no key does anything.
        assert!(keys(&asked(1, Some(3), "answering"), 0, true, false).is_empty());
        // It stopped: the session, or another go (which shows the answers again).
        assert_eq!(keys(&asked(1, Some(3), "failed"), 0, false, false), [Key::Show, Key::Again]);
        assert_eq!(keys(&asked(1, Some(3), "failed"), 0, false, true), [Key::Rec(0), Key::Rec(1), Key::Rec(2)]);
        // A turn that waits for the user to act offers no answers.
        let waits = QuestionSet { tabs: vec![Tab { question: "Merge it, then say so.".into(), allows_other: true, ..Default::default() }], ..Default::default() };
        assert!(keys(&waits, 0, false, false).is_empty());
    }

    fn tab(multi: bool) -> Tab {
        let opt = |l: &str| Opt { label: l.into(), description: String::new(), key: String::new() };
        Tab { title: "DB".into(), question: "Which database?".into(), options: vec![opt("Postgres"), opt("SQLite"), opt("MySQL")], multi, allows_other: true, answered: false, answer: String::new() }
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
    fn a_card_names_every_session_that_asks_the_same() {
        use crate::orb::model::SessionRef;
        let set = QuestionSet { id: "q1".into(), session: "t1".into(), name: "eyzflow".into(), ..Default::default() };
        let mut d = Decision { sessions: vec![SessionRef::new("t1", "eyzflow")], ..Default::default() };
        assert_eq!(asking(&d, &set), "eyzflow");
        d.sessions.push(SessionRef::new("t2", "mediaroom"));
        assert_eq!(asking(&d, &set), "eyzflow and mediaroom ask the same");
        d.sessions.push(SessionRef::new("t3", ""));
        assert_eq!(asking(&d, &set), "eyzflow, mediaroom and t3 ask the same");
        // A set with no name goes by its address.
        assert_eq!(asking(&Decision::default(), &QuestionSet { session: "t9".into(), ..Default::default() }), "t9");
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

    /// The dialog answered tab by tab in the terminal: the card goes where
    /// the dialog is, asks no more what was answered there, and with all of
    /// it answered offers Submit only. Moved on the Mac, it stays put while
    /// the dialog does.
    #[test]
    fn the_card_follows_its_dialog_answered_in_the_terminal() {
        let mut set = asked(4, Some(3), "open");
        let mut card = Card::default();
        follow_dialog(&mut card, &set);
        assert_eq!(card.step, 0);
        // On the Mac the user looks at the second question; the dialog hasn't moved.
        card.step = 1;
        follow_dialog(&mut card, &set);
        assert_eq!(card.step, 1, "stays where the user put it");
        // In the terminal: three answered, the dialog on the fourth.
        for t in 0..3 {
            set.tabs[t].answered = true;
        }
        set.tabs[1].answer = "Search".into();
        set.current_tab = 3;
        follow_dialog(&mut card, &set);
        assert_eq!(card.step, 3, "the card is where the dialog is: 4 of 4");
        assert!(there(&set, 0) && there(&set, 2) && !there(&set, 3));
        assert_eq!(keys(&set, 3, false, false), [Key::Rec(0), Key::Rec(1), Key::Rec(2)], "the open question's answers");
        assert!(keys(&set, 1, false, false).is_empty(), "nothing to pick on a question answered there");
        assert_eq!((next_open(&set, 0), next_open(&set, 3)), (3, 4), "Next skips what was answered there");
        assert_eq!(answered_there_line(&set.tabs[1]), "Answered in the terminal: Search");
        assert_eq!(answered_there_line(&set.tabs[0]), "Answered in the terminal");
        assert_eq!(keys(&set, 4, false, false), [Key::Submit, Key::Change]);
        // The fourth answered there too, the dialog on its review: Submit only.
        set.tabs[3].answered = true;
        set.current_tab = 4;
        follow_dialog(&mut card, &set);
        assert_eq!(card.step, 4);
        assert!(all_there(&set));
        assert_eq!(keys(&set, 4, false, false), [Key::Submit], "only Submit is left");
        // A single question has no tabs to follow.
        let one = asked(1, Some(3), "open");
        let mut c = Card { step: 0, ..Default::default() };
        follow_dialog(&mut c, &one);
        assert!(!there(&one, 0) && c.step == 0);
    }
}
