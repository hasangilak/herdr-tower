//! Behaviors: how the tower works, as rules the user reads and changes (see
//! `crate::behaviors`). A view of the full window, reached from Settings
//! (W18 in the design): the categories as sections, each behavior a card
//! (title, one line, where it came from, a switch), its full text one tap
//! away with Edit (a preview of what changes before Save), Turn off, Delete
//! and Undo; Add; a search; the fixed core folded away; the recent changes.
//! The input at the foot tells the tower what to change, by typing or
//! talking. Changes go through `behaviors::apply` and the daemon hears of
//! them (`Request::BehaviorsChanged`), as from the terminal or the phone.

use std::time::SystemTime;

use gpui::{AnyElement, Context, Entity, FontWeight, Hsla, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};

use super::{NAVY, PALE, Panel, ROSE, ago, glass};
use crate::behaviors::{Behavior, Change, Op, Patch};

/// Where a behavior came from, in a word.
pub(super) fn source_label(source: &str) -> &'static str {
    match source {
        "builtin" => "Built in",
        "learned" => "Learned",
        _ => "You",
    }
}

/// A change, as a verb: `Added`, `Turned off`.
pub(super) fn verb(op: &str) -> &'static str {
    match op {
        "add" => "Added",
        "edit" => "Changed",
        "enable" => "Turned on",
        "disable" => "Turned off",
        "remove" => "Removed",
        "undo" => "Undid",
        "seed" => "Set up",
        "migrate" => "Learned",
        _ => "Changed",
    }
}

/// The search: every word somewhere in the title, the line or the rule.
pub(super) fn matches(b: &Behavior, query: &str) -> bool {
    let hay = format!("{} {} {} {}", b.title, b.summary, b.category, b.text).to_lowercase();
    query.to_lowercase().split_whitespace().all(|w| hay.contains(w))
}

/// The behaviors the search lets through, by category in the given order
/// (others last, as Misc), empty categories left out.
pub(super) fn grouped<'a>(list: &'a [Behavior], categories: &[String], query: &str) -> Vec<(String, Vec<&'a Behavior>)> {
    let mut out: Vec<(String, Vec<&Behavior>)> = categories.iter().map(|c| (c.clone(), vec![])).collect();
    for b in list.iter().filter(|b| matches(b, query)) {
        match out.iter_mut().find(|(c, _)| *c == b.category) {
            Some((_, v)) => v.push(b),
            None => match out.iter_mut().find(|(c, _)| c == "Misc") {
                Some((_, v)) => v.push(b),
                None => out.push((b.category.clone(), vec![b])),
            },
        }
    }
    out.retain(|(_, v)| !v.is_empty());
    out
}

/// A diff's lines with their colour: added, removed, the same.
pub(super) fn diff_lines(diff: &str) -> Vec<(char, String)> {
    diff.lines()
        .map(|l| match l.get(..2) {
            Some("+ ") => ('+', l[2..].to_string()),
            Some("- ") => ('-', l[2..].to_string()),
            _ => (' ', l.get(2..).unwrap_or("").to_string()),
        })
        .collect()
}

/// The lines that changed, with one line around each: what a card shows.
pub(super) fn diff_excerpt(diff: &str, around: usize, max: usize) -> Vec<(char, String)> {
    let lines = diff_lines(diff);
    let keep: Vec<bool> = (0..lines.len()).map(|i| (i.saturating_sub(around)..=(i + around).min(lines.len().saturating_sub(1))).any(|j| lines[j].0 != ' ')).collect();
    lines.into_iter().zip(keep).filter(|(_, k)| *k).map(|(l, _)| l).take(max).collect()
}

/// An edit or a new behavior being written.
pub(super) struct Editor {
    /// `None`: a new one.
    id: Option<String>,
    title: Entity<TextareaState>,
    summary: Entity<TextareaState>,
    text: Entity<TextareaState>,
    category: String,
    /// The rule as it was (for the preview).
    before: String,
    /// What Save would change, once asked for.
    preview: Option<String>,
}

#[derive(Default)]
pub(super) struct BehaviorsUi {
    list: Vec<Behavior>,
    categories: Vec<String>,
    core: String,
    /// Newest first.
    history: Vec<Change>,
    /// The history file's time when read: a change elsewhere reloads.
    stamp: Option<SystemTime>,
    loading: bool,
    open: Option<String>,
    /// A behavior to open by its title once the list is in (from a chat card).
    want: Option<String>,
    edit: Option<Editor>,
    search: Option<Entity<TextareaState>>,
    confirm_delete: Option<String>,
    core_open: bool,
    history_open: bool,
    note: Option<(bool, String)>,
    busy: bool,
}

impl BehaviorsUi {
    /// Writing one: the editor has the room, not the input.
    pub(super) fn editing(&self) -> bool {
        self.edit.is_some()
    }

    /// How many are on, and how many there are.
    pub(super) fn counts(&self) -> (usize, usize) {
        (self.list.iter().filter(|b| b.enabled).count(), self.list.len())
    }
}

fn history_file() -> std::path::PathBuf {
    crate::behaviors::Store::path().join("history.jsonl")
}

/// Everything the screen shows, read from the store.
fn read_all() -> (Vec<Behavior>, Vec<String>, String, Vec<Change>) {
    let v = crate::behaviors::listing();
    let list: Vec<Behavior> = serde_json::from_value(v["behaviors"].clone()).unwrap_or_default();
    let categories: Vec<String> = serde_json::from_value(v["categories"].clone()).unwrap_or_default();
    let core = v["core"]["text"].as_str().unwrap_or("").to_string();
    let mut history = crate::behaviors::Store::open().history();
    history.retain(|c| !matches!(c.op.as_str(), "seed" | "migrate"));
    history.reverse();
    history.truncate(20);
    (list, categories, core, history)
}

impl Panel {
    /// The view opens (from Settings): on the list, read anew.
    pub(super) fn open_behaviors(&mut self, cx: &mut Context<Self>) {
        self.behaviors.edit = None;
        self.behaviors.confirm_delete = None;
        self.behaviors.open = None;
        self.behaviors.stamp = None;
        self.behaviors_tick(cx);
    }

    /// Each second while the screen shows: a change from the brain, the
    /// phone or the terminal shows at once.
    pub(super) fn behaviors_tick(&mut self, cx: &mut Context<Self>) {
        let stamp = std::fs::metadata(history_file()).and_then(|m| m.modified()).ok();
        if self.behaviors.loading || (self.behaviors.stamp.is_some() && stamp == self.behaviors.stamp) {
            return;
        }
        self.behaviors.loading = true;
        let task = cx.background_executor().spawn(async { read_all() });
        cx.spawn(async move |this, cx| {
            let (list, categories, core, history) = task.await;
            let _ = this.update(cx, |this, cx| {
                let b = &mut this.behaviors;
                b.loading = false;
                b.stamp = stamp.or(Some(SystemTime::UNIX_EPOCH));
                b.list = list;
                b.categories = categories;
                b.core = core;
                b.history = history;
                if b.open.as_ref().is_some_and(|id| !b.list.iter().any(|x| &x.id == id)) {
                    b.open = None;
                }
                if let Some(t) = b.want.take() {
                    b.open = b.list.iter().find(|x| x.title == t).map(|x| x.id.clone());
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A change, as from the terminal or the phone: applied, then the
    /// daemon tells the brain, the phones and the chat.
    fn behaviors_apply(&mut self, op: Op, done: &'static str, cx: &mut Context<Self>) {
        self.behaviors.busy = true;
        self.behaviors.note = None;
        let task = cx.background_executor().spawn(async move {
            let (_, change) = crate::behaviors::apply(op, "user")?;
            let view = change.view();
            let _ = crate::protocol::send(&crate::protocol::Request::BehaviorsChanged { by: "user".into(), change: view, from: None });
            anyhow::Ok(change.id)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let b = &mut this.behaviors;
                b.busy = false;
                match result {
                    Ok(id) => {
                        b.note = Some((true, done.to_string()));
                        b.edit = None;
                        b.confirm_delete = None;
                        if done == "Added." {
                            b.open = Some(id);
                        }
                        if done == "Deleted." {
                            b.open = None;
                        }
                    }
                    Err(e) => b.note = Some((false, format!("{e:#}"))),
                }
                b.stamp = None;
                this.behaviors_tick(cx);
            });
        })
        .detach();
    }

    fn start_edit(&mut self, b: Option<Behavior>, window: &mut Window, cx: &mut Context<Self>) {
        let field = |text: &str, lines: usize, hint: &str, window: &mut Window, cx: &mut Context<Self>| {
            let f = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, lines).placeholder(hint.to_string()));
            let text = text.to_string();
            f.update(cx, |s, cx| s.set_value(text, window, cx));
            f
        };
        let b = b.unwrap_or_else(|| Behavior { category: "Misc".into(), ..Default::default() });
        let title = field(&b.title, 2, "A short name", window, cx);
        let summary = field(&b.summary, 3, "In one plain sentence, what it does", window, cx);
        let text = field(&b.text, 14, "The rule, as the tower should follow it", window, cx);
        title.update(cx, |s, cx| s.focus(window, cx));
        self.behaviors.edit = Some(Editor {
            id: (!b.id.is_empty()).then(|| b.id.clone()),
            title,
            summary,
            text,
            category: b.category.clone(),
            before: b.text.clone(),
            preview: None,
        });
        self.behaviors.note = None;
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let Some(e) = &self.behaviors.edit else { return };
        let value = |f: &Entity<TextareaState>| f.read(cx).value().trim().to_string();
        let (title, summary, text) = (value(&e.title), value(&e.summary), value(&e.text));
        if title.is_empty() || text.is_empty() {
            self.behaviors.note = Some((false, "It needs a name and the rule.".into()));
            cx.notify();
            return;
        }
        let category = e.category.clone();
        match e.id.clone() {
            None => self.behaviors_apply(Op::Add { title, category, summary, text }, "Added.", cx),
            Some(id) => {
                let patch = Patch { title: Some(title), category: Some(category), summary: Some(summary), text: Some(text), enabled: None };
                self.behaviors_apply(Op::Edit { id, patch }, "Saved.", cx)
            }
        }
    }

    /// The view's body: the list, one behavior, or the editor.
    pub(super) fn behaviors_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.behaviors.search.is_none() {
            self.behaviors.search = Some(cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 1).placeholder("Search behaviors")));
        }
        let items = if self.behaviors.edit.is_some() {
            self.behavior_editor(cx)
        } else if let Some(id) = self.behaviors.open.clone() {
            self.behavior_detail(&id, cx)
        } else {
            self.behavior_list(cx)
        };
        vec![v_flex().w_full().gap_2().children(items).into_any_element()]
    }

    fn note_line(&self) -> Option<AnyElement> {
        self.behaviors.note.as_ref().map(|(ok, t)| div().text_sm().text_color(if *ok { rgb(0x6eeaa8) } else { rgb(ROSE) }).child(t.clone()).into_any_element())
    }

    fn behavior_list(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let mut out: Vec<AnyElement> = vec![];
        // The search, with Undo and Add beside it (the view's head says the rest).
        let tools = h_flex()
            .flex_none()
            .gap_2()
            .items_center()
            .when(!self.behaviors.history.is_empty(), |el| {
                el.child(link("behaviors-undo", "Undo").on_click(cx.listener(|this, _, _, cx| this.behaviors_apply(Op::Undo, "Undone.", cx))))
            })
            .child(pill("behaviors-add", "+ Add", true).on_click(cx.listener(|this, _, window, cx| this.start_edit(None, window, cx))));
        if let Some(search) = &self.behaviors.search {
            out.push(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_3()
                            .py(px(4.0))
                            .rounded_full()
                            .bg(g.pill)
                            .child(Textarea::new(search).appearance(false).bordered(false).small().w_full().text_size(px(14.0)).text_color(g.pill_text)),
                    )
                    .child(tools)
                    .into_any_element(),
            );
        }
        out.extend(self.note_line());
        let query = self.behaviors.search.as_ref().map(|s| s.read(cx).value().to_string()).unwrap_or_default();
        let list = self.behaviors.list.clone();
        let groups = grouped(&list, &self.behaviors.categories, &query);
        if groups.is_empty() {
            out.push(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(g.muted)
                    .child(if self.behaviors.list.is_empty() { "Looking…" } else { "Nothing matches." })
                    .into_any_element(),
            );
        }
        for (category, bs) in groups {
            out.push(section(&category));
            for b in bs {
                out.push(self.behavior_card(b, cx));
            }
        }
        // The fixed part.
        let core_open = self.behaviors.core_open;
        out.push(section("Fixed"));
        out.push(
            v_flex()
                .id("behaviors-core")
                .px_3()
                .py_2()
                .gap_1()
                .rounded(px(14.0))
                .cursor_pointer()
                .bg(rgba(0xffffff14))
                .border_1()
                .border_color(g.edge)
                .child(
                    h_flex()
                        .child(div().flex_1().text_sm().font_weight(FontWeight::MEDIUM).child("How the tower is wired (fixed)"))
                        .child(div().text_sm().child(if core_open { "⌄" } else { "›" })),
                )
                .child(div().text_xs().text_color(g.muted).child("Who it is, how messages reach it, its tools. It can't be changed here."))
                .when(core_open, |el| el.child(div().pt_1().text_xs().line_height(px(17.0)).text_color(g.muted).child(self.behaviors.core.clone())))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.behaviors.core_open = !this.behaviors.core_open;
                    cx.notify();
                }))
                .into_any_element(),
        );
        // What changed lately.
        if !self.behaviors.history.is_empty() {
            let open = self.behaviors.history_open;
            out.push(
                h_flex()
                    .id("behaviors-history")
                    .pt_2()
                    .cursor_pointer()
                    .child(div().flex_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(g.faint).child("RECENT CHANGES"))
                    .child(div().text_xs().text_color(g.faint).child(if open { "Hide" } else { "Show" }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.behaviors.history_open = !this.behaviors.history_open;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            if open {
                for c in self.behaviors.history.clone().iter().take(10) {
                    let view = c.view();
                    let diff = view["diff"].as_str().unwrap_or("").to_string();
                    let who = if c.by == "brain" { "the tower" } else { "you" };
                    out.push(
                        v_flex()
                            .px_3()
                            .py_2()
                            .gap_1()
                            .rounded(px(12.0))
                            .bg(rgba(0xffffff14))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().flex_1().min_w_0().truncate().text_sm().child(format!("{} {}", verb(&c.op), c.title)))
                                    .child(div().flex_none().text_xs().text_color(g.faint).child(format!("{who} · {}", ago(c.at)))),
                            )
                            .children(diff_block(&diff_excerpt(&diff, 1, 6)))
                            .into_any_element(),
                    );
                }
            }
        }
        out
    }

    fn behavior_card(&mut self, b: &Behavior, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let id = b.id.clone();
        let enabled = b.enabled;
        v_flex()
            .id(SharedString::from(format!("behavior-{}", b.id)))
            .px_3()
            .py_2()
            .gap(px(3.0))
            .rounded(px(14.0))
            .cursor_pointer()
            .bg(g.bubble_tower)
            .hover(|s| s.bg(rgba(0xffffff33)))
            .when(!enabled, |el| el.opacity(0.6))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().min_w_0().text_sm().font_weight(FontWeight::MEDIUM).child(b.title.clone()))
                    .child(source_badge(&b.source))
                    .child(self.switch(&b.id, enabled, cx)),
            )
            .when(!b.summary.is_empty(), |el| el.child(div().text_xs().text_color(g.muted).line_clamp(2).child(b.summary.clone())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.behaviors.open = Some(id.clone());
                this.behaviors.note = None;
                cx.notify();
            }))
            .into_any_element()
    }

    /// On or off, one tap.
    fn switch(&mut self, id: &str, on: bool, cx: &mut Context<Self>) -> AnyElement {
        let id2 = id.to_string();
        div()
            .id(SharedString::from(format!("switch-{id}")))
            .flex_none()
            .w(px(36.0))
            .h(px(21.0))
            .p(px(2.0))
            .rounded_full()
            .flex()
            .items_center()
            .when(on, |el| el.justify_end().bg(rgb(0x6eeaa8)))
            .when(!on, |el| el.justify_start().bg(rgba(0xffffff40)))
            .child(div().size(px(17.0)).rounded_full().bg(rgb(0xffffff)))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if this.behaviors.busy {
                    return;
                }
                // Shown at once; the store says how it went.
                if let Some(b) = this.behaviors.list.iter_mut().find(|b| b.id == id2) {
                    b.enabled = !on;
                }
                let patch = Patch { enabled: Some(!on), ..Default::default() };
                this.behaviors_apply(Op::Edit { id: id2.clone(), patch }, if on { "Turned off." } else { "Turned on." }, cx);
            }))
            .into_any_element()
    }

    fn behavior_detail(&mut self, id: &str, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let Some(b) = self.behaviors.list.iter().find(|b| b.id == id).cloned() else {
            self.behaviors.open = None;
            return self.behavior_list(cx);
        };
        let mut out: Vec<AnyElement> = vec![];
        out.push(
            h_flex()
                .gap_2()
                .items_center()
                .child(link("behavior-back", "‹ Behaviors").on_click(cx.listener(|this, _, _, cx| {
                    this.behaviors.open = None;
                    this.behaviors.confirm_delete = None;
                    this.behaviors.note = None;
                    cx.notify();
                })))
                .child(div().flex_1())
                .child(link("behavior-undo", "Undo").on_click(cx.listener(|this, _, _, cx| this.behaviors_apply(Op::Undo, "Undone.", cx))))
                .into_any_element(),
        );
        out.push(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().flex_1().min_w_0().text_xl().font_weight(FontWeight::MEDIUM).child(b.title.clone()))
                .child(self.switch(&b.id, b.enabled, cx))
                .into_any_element(),
        );
        out.push(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().text_xs().text_color(g.muted).child(b.category.clone()))
                .child(source_badge(&b.source))
                .child(div().text_xs().text_color(g.faint).child(if b.updated_ms > 0 { format!("changed {}", ago(b.updated_ms)) } else { String::new() }))
                .into_any_element(),
        );
        if !b.summary.is_empty() {
            out.push(div().text_sm().child(b.summary.clone()).into_any_element());
        }
        out.extend(self.note_line());
        out.push(
            div()
                .px_3()
                .py_2()
                .rounded(px(12.0))
                .bg(rgba(0x0000002e))
                .text_xs()
                .line_height(px(18.0))
                .child(b.text.clone())
                .into_any_element(),
        );
        let asking = self.behaviors.confirm_delete.as_deref() == Some(id);
        let (id1, id2, bb) = (b.id.clone(), b.id.clone(), b.clone());
        let on = b.enabled;
        out.push(
            h_flex()
                .gap_2()
                .pt_1()
                .child(pill("behavior-edit", "Edit", true).on_click(cx.listener(move |this, _, window, cx| this.start_edit(Some(bb.clone()), window, cx))))
                .child(pill("behavior-toggle", if on { "Turn off" } else { "Turn on" }, false).on_click(cx.listener(move |this, _, _, cx| {
                    let patch = Patch { enabled: Some(!on), ..Default::default() };
                    this.behaviors_apply(Op::Edit { id: id1.clone(), patch }, if on { "Turned off." } else { "Turned on." }, cx);
                })))
                .child(div().flex_1())
                .child(
                    div()
                        .id("behavior-delete")
                        .px_3()
                        .py(px(5.0))
                        .rounded_full()
                        .cursor_pointer()
                        .text_sm()
                        .when(asking, |el| el.bg(rgb(ROSE)).text_color(rgb(NAVY)).font_weight(FontWeight::MEDIUM))
                        .when(!asking, |el| el.text_color(rgb(ROSE)).hover(|s| s.bg(rgba(0xffffff1f))))
                        .child(if asking { "Delete it?" } else { "Delete" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.behaviors.confirm_delete.as_deref() == Some(id2.as_str()) {
                                this.behaviors_apply(Op::Remove { id: id2.clone() }, "Deleted.", cx);
                            } else {
                                this.behaviors.confirm_delete = Some(id2.clone());
                                cx.notify();
                            }
                        })),
                )
                .into_any_element(),
        );
        out
    }

    fn behavior_editor(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let g = glass();
        let Some(e) = &self.behaviors.edit else { return vec![] };
        let new = e.id.is_none();
        let mut out: Vec<AnyElement> = vec![];
        let busy = self.behaviors.busy;
        out.push(
            h_flex()
                .gap_2()
                .child(link("edit-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| {
                    this.behaviors.edit = None;
                    this.behaviors.note = None;
                    cx.notify();
                })))
                .child(div().flex_1().text_base().font_weight(FontWeight::MEDIUM).text_center().child(if new { "Add a behavior" } else { "Edit" }))
                .when(!new, |el| {
                    el.child(pill("edit-preview", "Preview", false).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(e) = this.behaviors.edit.as_mut() {
                            let after = e.text.read(cx).value().to_string();
                            e.preview = Some(crate::behaviors::diff(e.before.trim(), after.trim()));
                        }
                        cx.notify();
                    })))
                })
                .child(pill("edit-save", if busy { "Saving…" } else { "Save" }, true).when(!busy, |el| el.on_click(cx.listener(|this, _, _, cx| this.save_edit(cx)))))
                .into_any_element(),
        );
        out.extend(self.note_line());
        if let Some(d) = &e.preview {
            out.push(div().text_xs().text_color(g.faint).child("WHAT CHANGES").into_any_element());
            out.push(
                v_flex()
                    .px_3()
                    .py_2()
                    .rounded(px(12.0))
                    .bg(rgba(0x0000002e))
                    .children(diff_block(&diff_lines(d)))
                    .when(d.lines().all(|l| l.starts_with("  ")), |el| el.child(div().text_xs().text_color(g.muted).child("The rule is the same.")))
                    .into_any_element(),
            );
        }
        let field = |label: &str, f: &Entity<TextareaState>| {
            v_flex()
                .gap_1()
                .child(div().text_xs().text_color(g.faint).child(label.to_uppercase()))
                .child(
                    div()
                        .px_3()
                        .py(px(6.0))
                        .rounded(px(12.0))
                        .bg(g.pill)
                        .child(Textarea::new(f).appearance(false).bordered(false).small().w_full().text_size(px(14.0)).text_color(g.pill_text)),
                )
                .into_any_element()
        };
        out.push(field("Name", &e.title));
        // The category, as chips.
        let mut chips: Vec<AnyElement> = vec![];
        for (i, c) in self.behaviors.categories.iter().enumerate() {
            let on = *c == e.category;
            let c2 = c.clone();
            chips.push(
                div()
                    .id(SharedString::from(format!("category-{i}")))
                    .px_2()
                    .py(px(3.0))
                    .rounded_full()
                    .cursor_pointer()
                    .text_xs()
                    .when(on, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)))
                    .when(!on, |el| el.bg(rgba(0xffffff14)).text_color(g.muted).hover(|s| s.bg(rgba(0xffffff2e))))
                    .child(c.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(e) = this.behaviors.edit.as_mut() {
                            e.category = c2.clone();
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        out.push(
            v_flex()
                .gap_1()
                .child(div().text_xs().text_color(g.faint).child("CATEGORY"))
                .child(h_flex().flex_wrap().gap_1().children(chips))
                .into_any_element(),
        );
        out.push(field("In a sentence", &e.summary));
        out.push(field("The rule", &e.text));
        out
    }

}

fn section(title: &str) -> AnyElement {
    div().pt_2().text_xs().font_weight(FontWeight::MEDIUM).text_color(glass().faint).child(title.to_uppercase()).into_any_element()
}

fn source_badge(source: &str) -> AnyElement {
    let g = glass();
    let el = div().flex_none().px(px(7.0)).rounded_full().text_xs();
    match source {
        "builtin" => el.border_1().border_color(g.edge).text_color(g.muted),
        "learned" => el.bg(rgb(0xc9b3ff)).text_color(rgb(NAVY)),
        _ => el.bg(rgb(0x7fe8e0)).text_color(rgb(NAVY)),
    }
    .child(source_label(source))
    .into_any_element()
}

/// Diff lines: added in green, removed in red, the rest faint.
fn diff_block(lines: &[(char, String)]) -> Vec<AnyElement> {
    let g = glass();
    lines
        .iter()
        .map(|(k, l)| {
            let (color, mark): (Hsla, &str) = match k {
                '+' => (rgb(0x9ff0c4).into(), "+ "),
                '-' => (rgb(0xffb3b3).into(), "− "),
                _ => (g.faint, "  "),
            };
            div().text_xs().font_family("Menlo").text_color(color).line_clamp(3).child(format!("{mark}{l}")).into_any_element()
        })
        .collect()
}

fn link(id: &'static str, text: &'static str) -> gpui::Stateful<gpui::Div> {
    div().id(id).px_2().py(px(3.0)).rounded_full().cursor_pointer().text_sm().text_color(glass().muted).hover(|s| s.bg(rgba(0xffffff1f))).child(text)
}

fn pill(id: &'static str, text: &'static str, filled: bool) -> gpui::Stateful<gpui::Div> {
    let g = glass();
    div()
        .id(id)
        .flex_none()
        .px_3()
        .py(px(5.0))
        .rounded_full()
        .cursor_pointer()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .when(filled, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)))
        .when(!filled, |el| el.border_1().border_color(g.edge))
        .hover(|s| s.opacity(0.85))
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(title: &str, category: &str, summary: &str) -> Behavior {
        Behavior { id: title.to_lowercase().replace(' ', "-"), title: title.into(), category: category.into(), summary: summary.into(), enabled: true, ..Default::default() }
    }

    #[test]
    fn behaviors_group_by_category_and_search_finds_them() {
        let list = vec![b("Be brief", "Talking", "Short answers"), b("Release dev then prod", "Releases and safety", "Dev first"), b("Odd one", "Elsewhere", "")];
        let cats: Vec<String> = ["Talking", "Sessions", "Releases and safety", "Misc"].iter().map(|s| s.to_string()).collect();
        let g = grouped(&list, &cats, "");
        assert_eq!(g.iter().map(|(c, v)| (c.as_str(), v.len())).collect::<Vec<_>>(), vec![("Talking", 1), ("Releases and safety", 1), ("Misc", 1)]);
        let g = grouped(&list, &cats, "DEV prod");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].1[0].title, "Release dev then prod");
        assert!(grouped(&list, &cats, "nothing like this").is_empty());
    }

    #[test]
    fn a_diff_shows_what_changed_and_a_line_around_it() {
        let d = "  one\n  two\n- three\n+ three, now\n  four\n  five\n  six";
        assert_eq!(diff_lines(d)[2], ('-', "three".to_string()));
        let x = diff_excerpt(d, 1, 10);
        assert_eq!(x.iter().map(|(k, l)| format!("{k}{l}")).collect::<Vec<_>>(), vec![" two", "-three", "+three, now", " four"]);
        assert_eq!(source_label("builtin"), "Built in");
        assert_eq!(source_label("user"), "You");
        assert_eq!(verb("disable"), "Turned off");
    }
}
