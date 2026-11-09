//! Activity: what is happening now, what finished, what sessions sent.
//!
//! One line of controls: a session filter (one pill; a popover with a
//! search, sessions grouped by space, live or recent ones first, the focused
//! space chosen until you choose) and Active / Done / All. Then the content:
//! what needs you (questions, blocked jobs) and what runs, then what finished
//! by day. Jobs are compact rows (status, title, session and space, account,
//! screenshots, time); a tap opens the card with its summary and pictures.
//! Screenshots have their own tab (Shots), with the same filter.

use std::collections::HashSet;

use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};

use super::{AMBER, Accounts, CYAN, Filter, NAVY, PALE, Panel, ROSE, Scope, activity_row, ago, day_label, glass, job_card, span};
use crate::history::{Entry, Job, JobState};
use crate::spaces::Live;

/// Sessions not heard from for this long are "older" (hidden until asked for).
const RECENT_MS: u64 = 24 * 3600 * 1000;

/// A test session (`hc run --label tower.test=1 --name qfix-…`): never listed.
/// History keeps names only, so the name says it.
pub(super) fn is_test(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n.starts_with("qfix") || n.starts_with("tower.test")
}

/// The same job handed out again (same session, same words): the latest only.
pub(super) fn dedupe(jobs: &[Job]) -> Vec<Job> {
    let mut out: Vec<Job> = vec![];
    for j in jobs.iter().rev() {
        let title = j.title.trim().to_lowercase();
        if !out.iter().any(|o| o.session == j.session && o.title.trim().to_lowercase() == title) {
            out.push(j.clone());
        }
    }
    out.reverse();
    out
}

/// A space and its sessions (address, name), for the filter.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Group {
    pub space: String,
    pub sessions: Vec<(String, String)>,
}

/// The filter's list: live sessions and those heard from in the last day,
/// by space (the others only with `older`, and how many there are), test
/// sessions never; `query` narrows by session or space name.
pub(super) fn picker(live: &[Live], entries: &[Entry], now: u64, query: &str, older: bool) -> (Vec<Group>, usize) {
    // address -> (name, space, recent)
    let mut seen: Vec<(String, String, String, bool)> = vec![];
    for l in live {
        for (addr, name, _) in &l.sessions {
            if !addr.is_empty() && !is_test(name) && !seen.iter().any(|s| &s.0 == addr) {
                seen.push((addr.clone(), name.clone(), l.name.clone(), true));
            }
        }
    }
    for e in entries.iter().rev() {
        let (Some(addr), false) = (e.session.as_ref(), matches!(e.kind.as_str(), "request" | "answer")) else { continue };
        let name = e.name.clone().unwrap_or_else(|| addr.clone());
        if is_test(&name) || seen.iter().any(|s| &s.0 == addr) {
            continue;
        }
        let recent = now.saturating_sub(e.at) < RECENT_MS;
        seen.push((addr.clone(), name, e.space.clone().unwrap_or_default(), recent));
    }
    let q = query.trim().to_lowercase();
    let hidden = seen.iter().filter(|s| !s.3).count();
    let mut groups: Vec<Group> = vec![];
    for (addr, name, space, recent) in seen {
        if !(recent || older) {
            continue;
        }
        if !q.is_empty() && !name.to_lowercase().contains(&q) && !space.to_lowercase().contains(&q) {
            continue;
        }
        let space = if space.is_empty() { "Other".to_string() } else { space };
        match groups.iter_mut().find(|g| g.space == space) {
            Some(g) => g.sessions.push((addr, name)),
            None => groups.push(Group { space, sessions: vec![(addr, name)] }),
        }
    }
    // "Other" last.
    groups.sort_by_key(|g| g.space == "Other");
    (groups, if older { 0 } else { hidden })
}

/// The scope that counts: the one chosen, else the focused space.
pub(super) fn effective_scope(chosen: &Scope, touched: bool, focus: Option<&str>) -> Scope {
    match (chosen, touched, focus) {
        (Scope::Everything, false, Some(f)) => Scope::Space(f.to_string()),
        (s, _, _) => s.clone(),
    }
}

fn fits(scope: &Scope, session: &str, space: &str) -> bool {
    match scope {
        Scope::Everything => true,
        Scope::Space(s) => space == s,
        Scope::Session(s) => session == s,
    }
}

/// The Activity tab's own state.
#[derive(Default)]
pub(super) struct ActivityUi {
    /// The filter row: how far it is scrolled sideways.
    strip: gpui::ScrollHandle,
    /// Dragging it: where the pointer and the row were when it began.
    drag_from: Option<(f32, f32)>,
    /// It was dragged, so the chip under the pointer isn't picked.
    dragged: bool,
    search_open: bool,
    /// The row was measured once (its arrows show from then on).
    measured: bool,
    search: Option<Entity<TextareaState>>,
    older: bool,
    /// Job rows opened to their card.
    expanded: HashSet<String>,
    /// The filter was set (or cleared) by hand: the focus no longer picks it.
    touched: bool,
}

impl Panel {
    pub(super) fn scope_now(&self, cx: &gpui::App) -> Scope {
        let focus = self.tower.read(cx).spaces.focus.clone();
        effective_scope(&self.scope, self.act.touched, focus.as_deref())
    }

    /// What needs you, counted for the tab: questions waiting, blocked jobs.
    pub(super) fn needs_you(&self, cx: &gpui::App) -> usize {
        let blocked = dedupe(&self.jobs).iter().filter(|j| j.status == JobState::Blocked && !is_test(&j.name)).count();
        self.open_sets(cx).len() + blocked
    }

    /// The status (Active / Done / All) and, under it, the filter: one line
    /// of chips that scrolls sideways (a search, All, the focused space, then
    /// each space and its sessions), never a list over the content.
    fn filter_bar(&mut self, with_status: Option<(usize, usize)>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let mut out = v_flex().pb_2().gap_2();
        if let Some((active, done)) = with_status {
            let f = self.filter;
            let seg = |id: &'static str, text: String, on: bool| {
                div()
                    .id(id)
                    .flex_1()
                    .flex()
                    .justify_center()
                    .px_2()
                    .py(px(3.0))
                    .rounded_full()
                    .cursor_pointer()
                    .text_sm()
                    .when(on, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)).font_weight(FontWeight::MEDIUM))
                    .when(!on, |el| el.text_color(g.muted).hover(|s| s.bg(rgba(0xffffff1f))))
                    .child(text)
            };
            let count = |label: &str, n: usize| if n > 0 { format!("{label} {n}") } else { label.to_string() };
            out = out.child(
                h_flex()
                    .mx_5()
                    .p(px(2.0))
                    .gap(px(1.0))
                    .rounded_full()
                    .bg(rgba(0xffffff14))
                    .child(seg("f-active", count("Active", active), f == Filter::Active).on_click(cx.listener(|this, _, _, cx| {
                        this.filter = Filter::Active;
                        cx.notify();
                    })))
                    .child(seg("f-done", count("Done", done), f == Filter::Done).on_click(cx.listener(|this, _, _, cx| {
                        this.filter = Filter::Done;
                        cx.notify();
                    })))
                    .child(seg("f-all", "All".into(), f == Filter::All).on_click(cx.listener(|this, _, _, cx| {
                        this.filter = Filter::All;
                        cx.notify();
                    }))),
            );
        }
        out.child(self.filter_strip(window, cx)).into_any_element()
    }

    /// The sessions as one sideways row of chips.
    fn filter_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let scope = self.scope_now(cx);
        let focus = self.tower.read(cx).spaces.focus.clone();
        let query = self.act.search.as_ref().map(|s| s.read(cx).value().to_string()).unwrap_or_default();
        let (mut groups, hidden) = picker(&self.live, &self.entries, crate::history::now_ms(), &query, self.act.older);
        // The focused space first.
        if let Some(f) = &focus {
            groups.sort_by_key(|gr| gr.space != *f);
        }
        let pick = |target: Scope| {
            move |this: &mut Panel, _: &gpui::ClickEvent, _: &mut Window, cx: &mut Context<Panel>| {
                if std::mem::take(&mut this.act.dragged) {
                    return;
                }
                this.scope = target.clone();
                this.act.touched = true;
                cx.notify();
            }
        };
        let chip = |id: String, text: String, on: bool, space: bool| {
            div()
                .id(SharedString::from(id))
                .flex_none()
                .px_2()
                .py(px(2.0))
                .rounded_full()
                .cursor_pointer()
                .text_xs()
                .whitespace_nowrap()
                .when(on, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)).font_weight(FontWeight::MEDIUM))
                .when(!on && space, |el| el.border_1().border_color(g.edge).text_color(g.text).font_weight(FontWeight::MEDIUM).hover(|s| s.bg(rgba(0xffffff1f))))
                .when(!on && !space, |el| el.bg(rgba(0xffffff12)).text_color(g.muted).hover(|s| s.bg(rgba(0xffffff26))))
                .child(text)
        };
        let mut chips: Vec<AnyElement> = vec![];
        // The search: a magnifier that opens a small field in the row.
        match self.act.search_open {
            false => chips.push(
                div()
                    .id("strip-search")
                    .flex_none()
                    .p(px(4.0))
                    .rounded_full()
                    .cursor_pointer()
                    .text_color(g.muted)
                    .hover(|s| s.bg(rgba(0xffffff1f)))
                    .child(Icon::new(IconName::Search).xsmall())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.act.search_open = true;
                        if let Some(s) = this.act.search.clone() {
                            s.update(cx, |s, cx| s.focus(window, cx));
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            ),
            true => {
                let search = self
                    .act
                    .search
                    .get_or_insert_with(|| cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 1).placeholder("Find…")))
                    .clone();
                chips.push(
                    h_flex()
                        .flex_none()
                        .w(px(120.0))
                        .pl_2()
                        .pr(px(2.0))
                        .rounded_full()
                        .bg(g.pill)
                        .items_center()
                        .child(div().flex_1().min_w_0().child(Textarea::new(&search).appearance(false).bordered(false).xsmall().w_full().text_size(px(12.0)).text_color(g.pill_text)))
                        .child(
                            div()
                                .id("strip-search-close")
                                .p(px(2.0))
                                .rounded_full()
                                .cursor_pointer()
                                .text_color(g.pill_muted)
                                .child(Icon::new(IconName::Close).xsmall())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.act.search_open = false;
                                    if let Some(s) = this.act.search.clone() {
                                        s.update(cx, |s, cx| s.set_value("", window, cx));
                                    }
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
        }
        chips.push(chip("strip-all".into(), "All".into(), scope == Scope::Everything, false).on_click(cx.listener(pick(Scope::Everything))).into_any_element());
        for (i, gr) in groups.iter().enumerate() {
            let target = Scope::Space(gr.space.clone());
            let on = scope == target;
            chips.push(chip(format!("strip-space-{i}"), gr.space.clone(), on, true).on_click(cx.listener(pick(target))).into_any_element());
            for (j, (addr, name)) in gr.sessions.iter().enumerate() {
                let target = Scope::Session(addr.clone());
                let on = scope == target;
                chips.push(chip(format!("strip-{i}-{j}"), name.clone(), on, false).on_click(cx.listener(pick(target))).into_any_element());
            }
        }
        if hidden > 0 {
            chips.push(
                chip("strip-older".into(), format!("+ {hidden} older"), false, false)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.act.older = true;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        // Arrows at the ends while there is more that way; a wheel turned up
        // or down moves it sideways too, and so does dragging it.
        let handle = self.act.strip.clone();
        let (x, max) = (f32::from(handle.offset().x), f32::from(handle.max_offset().x));
        let arrow = |id: &'static str, text: &'static str, step: f32, on: bool| {
            let handle = handle.clone();
            div()
                .id(id)
                .flex_none()
                .w(px(20.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_sm()
                .when(on, |el| el.cursor_pointer().text_color(g.text).hover(|s| s.bg(rgba(0xffffff1f))))
                .when(!on, |el| el.text_color(rgba(0xffffff33)))
                .child(text)
                .when(on, |el| {
                    el.on_click(cx.listener(move |_, _, _, cx| {
                        let o = handle.offset();
                        handle.set_offset(gpui::point(o.x + px(step), o.y));
                        cx.notify();
                    }))
                })
        };
        let scrolls = max > 1.0;
        // Its width is known once laid out: draw once more to show the arrows.
        if !scrolls && !self.act.measured {
            self.act.measured = true;
            let me = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = me.update(cx, |_, cx| cx.notify());
            });
        }
        let h2 = handle.clone();
        let h3 = handle.clone();
        h_flex()
            .px_3()
            .gap(px(2.0))
            .items_center()
            .when(scrolls, |el| el.child(arrow("strip-left", "‹", 180.0, x < -1.0)))
            .child(
                div()
                    .id("strip")
                    .flex_1()
                    .min_w_0()
                    .overflow_x_scroll()
                    .track_scroll(&handle)
                    .on_scroll(cx.listener(move |_, ev: &gpui::ScrollEvent, window, cx| {
                        let d = ev.delta.pixel_delta(window.line_height());
                        // A plain wheel (up/down) moves the row sideways.
                        if d.x.abs() < px(0.5) && d.y.abs() > px(0.0) {
                            let o = h2.offset();
                            h2.set_offset(gpui::point(o.x + d.y, o.y));
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, ev: &gpui::MouseDownEvent, _, _| {
                        this.act.drag_from = Some((f32::from(ev.position.x), f32::from(this.act.strip.offset().x)));
                        this.act.dragged = false;
                    }))
                    .on_mouse_move(cx.listener(move |this, ev: &gpui::MouseMoveEvent, _, cx| {
                        let Some((x0, o0)) = this.act.drag_from else { return };
                        if ev.pressed_button != Some(gpui::MouseButton::Left) {
                            this.act.drag_from = None;
                            return;
                        }
                        let dx = f32::from(ev.position.x) - x0;
                        if dx.abs() > 4.0 {
                            this.act.dragged = true;
                            let o = h3.offset();
                            h3.set_offset(gpui::point(px(o0 + dx), o.y));
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _, _, _| this.act.drag_from = None))
                    .flex()
                    // As wide as its chips, so the row scrolls rather than clips.
                    .child(h_flex().flex_none().gap_1().items_center().py(px(2.0)).children(chips)),
            )
            .when(scrolls, |el| el.child(arrow("strip-right", "›", -180.0, x > -max + 1.0)))
            .into_any_element()
    }

    /// A job in a line: status, title, who and where, account, pictures, time.
    pub(super) fn job_row(&mut self, j: &Job, acc: &Accounts, all_shots: &[crate::orb::shots::Shot], now: u64, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let open = self.act.expanded.contains(&j.id);
        if open {
            // The whole card, which closes on a tap.
            let extra = self.job_extra(j, all_shots, cx);
            let id = j.id.clone();
            return div()
                .id(SharedString::from(format!("job-open-{}", j.id)))
                .cursor_pointer()
                .child(job_card(j, &g, now, acc, extra))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.act.expanded.remove(&id);
                    cx.notify();
                }))
                .into_any_element();
        }
        let (dot, when): (AnyElement, String) = match j.status {
            JobState::Working => (div().size(px(8.0)).rounded_full().bg(rgb(CYAN)).into_any_element(), span(now.saturating_sub(j.started))),
            JobState::Blocked => (div().size(px(8.0)).rounded_full().bg(rgb(AMBER)).into_any_element(), "needs you".into()),
            JobState::Done => (Icon::new(IconName::Check).xsmall().text_color(g.faint).into_any_element(), ago(j.updated)),
            _ => (div().size(px(8.0)).rounded_full().bg(rgb(ROSE)).into_any_element(), ago(j.updated)),
        };
        let shots = all_shots.iter().filter(|s| super::gallery::of_job(s, j)).count();
        let who = if j.name.is_empty() { j.session.clone() } else { j.name.clone() };
        let mut meta = who;
        if !j.space.is_empty() {
            meta = format!("{meta} · {}", j.space);
        }
        let title = if j.title.is_empty() { "Work".to_string() } else { j.title.clone() };
        let id = j.id.clone();
        h_flex()
            .id(SharedString::from(format!("job-{}", j.id)))
            .gap_2()
            .items_start()
            .px_3()
            .py(px(7.0))
            .rounded(px(12.0))
            .cursor_pointer()
            .hover(|s| s.bg(rgba(0xffffff1a)))
            .child(h_flex().w(px(12.0)).h(px(20.0)).flex_none().items_center().justify_center().child(dot))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().min_w_0().truncate().text_sm().child(title))
                            .child(div().flex_none().text_xs().text_color(if j.status == JobState::Working { rgb(CYAN).into() } else { g.faint }).child(when)),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.0))
                            .items_center()
                            .text_xs()
                            .text_color(g.muted)
                            .child(div().min_w_0().truncate().child(meta))
                            .children(j.account.as_deref().or_else(|| acc.of(&j.session)).map(|a| acc.badge(a, &g)))
                            .when(shots > 0, |el| el.child(div().flex_none().text_color(g.faint).child(format!("▣ {shots}")))),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.act.expanded.insert(id.clone());
                cx.notify();
            }))
            .into_any_element()
    }

    pub(super) fn activity_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let acc = Accounts::of_tower(self.tower.read(cx), self.default_account.clone());
        let scope = self.scope_now(cx);
        let now = crate::history::now_ms();
        let jobs: Vec<Job> = dedupe(&self.jobs).into_iter().filter(|j| !is_test(&j.name) && fits(&scope, &j.session, &j.space)).collect();
        let running: Vec<Job> = jobs.iter().filter(|j| j.open()).cloned().rev().collect();
        let mut done: Vec<Job> = jobs.iter().filter(|j| !j.open()).cloned().collect();
        done.sort_by_key(|j| std::cmp::Reverse(j.updated));
        let sets: Vec<_> = self.open_sets(cx).into_iter().filter(|s| fits(&scope, &s.session, &self.space_of(&s.session))).collect();
        let all_shots = self.all_shots(cx);
        let bar = self.filter_bar(Some((running.len() + sets.len(), done.len())), window, cx);

        let section = |t: &str| div().pt_2().text_xs().font_weight(FontWeight::MEDIUM).text_color(g.faint).child(t.to_uppercase()).into_any_element();
        let mut items: Vec<AnyElement> = vec![];
        let filter = self.filter;
        if matches!(filter, Filter::Active | Filter::All) {
            // Now: what needs you, then what runs.
            let blocked: Vec<&Job> = running.iter().filter(|j| j.status == JobState::Blocked).collect();
            if !sets.is_empty() || !blocked.is_empty() {
                items.push(section("Needs you"));
                for s in &sets {
                    items.extend(self.job_asks(&s.session, cx));
                }
                for j in blocked {
                    if !sets.iter().any(|s| s.session == j.session) {
                        items.push(self.job_row(j, &acc, &all_shots, now, cx));
                    }
                }
            }
            let working: Vec<Job> = running.iter().filter(|j| j.status != JobState::Blocked).cloned().collect();
            if !working.is_empty() {
                items.push(section("Running"));
                for j in &working {
                    items.push(self.job_row(j, &acc, &all_shots, now, cx));
                }
            }
            if items.is_empty() && filter == Filter::Active {
                items.push(div().pt_3().text_sm().text_color(g.muted).child("Nothing running right now.").into_any_element());
            }
        }
        if matches!(filter, Filter::Done | Filter::All) {
            // What finished (and, under All, what sessions reported), by day.
            let mut rows: Vec<(u64, Result<Job, Entry>)> = done.iter().map(|j| (j.updated, Ok(j.clone()))).collect();
            if filter == Filter::All {
                for e in self.entries.iter().rev().take(300) {
                    let keep = matches!(e.kind.as_str(), "watch" | "alert" | "summary" | "answered")
                        && !e.name.as_deref().is_some_and(is_test)
                        && fits(&scope, e.session.as_deref().unwrap_or(""), e.space.as_deref().unwrap_or(""))
                        && (scope == Scope::Everything || e.session.is_some());
                    if keep {
                        rows.push((e.at, Err(e.clone())));
                    }
                }
            }
            rows.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
            let mut day = String::new();
            for (at, row) in rows.into_iter().take(150) {
                let d = day_label(at);
                if d != day {
                    items.push(section(&d));
                    day = d;
                }
                items.push(match row {
                    Ok(j) => self.job_row(&j, &acc, &all_shots, now, cx),
                    Err(e) => activity_row(&e, &g, &acc),
                });
            }
            if day.is_empty() && filter == Filter::Done {
                items.push(div().pt_3().text_sm().text_color(g.muted).child("Nothing finished yet.").into_any_element());
            }
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .child(bar)
            .child(div().id("activity").flex_1().min_h_0().overflow_y_scroll().child(v_flex().w_full().px_5().pb_3().gap(px(2.0)).children(items)))
            .into_any_element()
    }

    /// The space a session is in (from the live spaces, else its history).
    pub(super) fn space_of(&self, addr: &str) -> String {
        self.live
            .iter()
            .find(|l| l.sessions.iter().any(|s| s.0 == addr))
            .map(|l| l.name.clone())
            .or_else(|| self.entries.iter().rev().find(|e| e.session.as_deref() == Some(addr)).and_then(|e| e.space.clone()))
            .unwrap_or_default()
    }

    /// The Shots tab: the gallery, with the same filter.
    pub(super) fn shots_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let acc = Accounts::of_tower(self.tower.read(cx), self.default_account.clone());
        let scope = self.scope_now(cx);
        let list: Vec<_> = self.all_shots(cx).into_iter().filter(|s| !is_test(s.who()) && fits(&scope, &s.session, &s.space)).collect();
        let bar = self.filter_bar(None, window, cx);
        let body: AnyElement = if list.is_empty() {
            div().pt_3().text_sm().text_color(g.muted).child("No screenshots here yet.").into_any_element()
        } else {
            self.gallery(&list, &acc, cx)
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(bar)
            .child(div().id("shots").flex_1().min_h_0().overflow_y_scroll().child(v_flex().w_full().px_5().pb_3().child(body)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, session: &str, title: &str, at: u64) -> Job {
        Job {
            id: id.into(),
            session: session.into(),
            name: session.into(),
            space: String::new(),
            account: None,
            title: title.into(),
            status: JobState::Done,
            summary: String::new(),
            started: at,
            updated: at,
        }
    }

    #[test]
    fn a_job_handed_out_again_shows_once() {
        let jobs = vec![job("a", "t1", "Run the tests", 1), job("b", "t2", "Run the tests", 2), job("c", "t1", "run the tests ", 3)];
        let ids: Vec<String> = dedupe(&jobs).into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec!["b", "c"]);
    }

    #[test]
    fn the_filter_lists_live_and_recent_sessions_by_space_never_tests() {
        let live = vec![Live { name: "Storefront".into(), sessions: vec![("t1".into(), "web-app".into(), "working".into()), ("t9".into(), "qfix-two".into(), "idle".into())] }];
        let now = 10 * RECENT_MS;
        let e = |at: u64, session: &str, name: &str, space: &str| Entry {
            at,
            kind: "job".into(),
            session: Some(session.into()),
            name: Some(name.into()),
            space: (!space.is_empty()).then(|| space.into()),
            ..Default::default()
        };
        let entries = vec![e(now - 100, "t2", "api", "Storefront"), e(now - 2 * RECENT_MS, "t3", "old-bot", "Docs"), e(now - 50, "t4", "qfix ask", ""), e(now - 10, "t5", "review", "")];
        let (groups, hidden) = picker(&live, &entries, now, "", false);
        assert_eq!(
            groups,
            vec![
                Group { space: "Storefront".into(), sessions: vec![("t1".into(), "web-app".into()), ("t2".into(), "api".into())] },
                Group { space: "Other".into(), sessions: vec![("t5".into(), "review".into())] },
            ]
        );
        assert_eq!(hidden, 1);
        let (older, _) = picker(&live, &entries, now, "", true);
        assert!(older.iter().any(|g| g.space == "Docs"));
        let (found, _) = picker(&live, &entries, now, "API", false);
        assert_eq!(found[0].sessions, vec![("t2".to_string(), "api".to_string())]);
        assert!(is_test("qfix two") && is_test("qfix-ask") && !is_test("web-app"));
    }

    #[test]
    fn the_focused_space_is_the_filter_until_you_choose() {
        assert_eq!(effective_scope(&Scope::Everything, false, Some("B2C")), Scope::Space("B2C".into()));
        assert_eq!(effective_scope(&Scope::Everything, true, Some("B2C")), Scope::Everything);
        assert_eq!(effective_scope(&Scope::Session("t1".into()), false, Some("B2C")), Scope::Session("t1".into()));
        assert_eq!(effective_scope(&Scope::Everything, false, None), Scope::Everything);
    }
}
