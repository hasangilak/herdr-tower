//! "Active now": the sessions working or waiting for you this moment, as one
//! sideways row of chips under the panel's status line, on every view (the
//! quick glance; Activity's Now is the detail). Blocked ones first, then the
//! longest running; with a focus, its space's first and the rest dimmed.
//! A chip opens the session (what it does, its question, its screen, its
//! jobs and pictures); a right click shows it in Herdr.

use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, FontWeight, MouseButton, ScrollHandle, SharedString, Window, div, prelude::*, pulsating_between, px,
    rgb, rgba,
};
use gpui_kit::component::{h_flex, v_flex};
use serde_json::Value;

use super::activity::{dedupe, is_test};
use super::{AMBER, Accounts, CYAN, NAVY, PALE, Panel, View, glass, span};
use crate::history::Job;

/// A session at work now.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Active {
    pub addr: String,
    pub name: String,
    pub space: String,
    pub account: Option<String>,
    /// `working` or `blocked`.
    pub status: String,
    /// When its open job began, if it has one.
    pub since: Option<u64>,
    /// What it was asked (its open job), if known.
    pub doing: String,
}

/// The sessions working or blocked now (from the state's agents), never test
/// ones: blocked first, then the longest running. `space_of` finds a
/// session's space.
pub(super) fn active_sessions(agents: &Value, jobs: &[Job], space_of: &dyn Fn(&str) -> String) -> Vec<Active> {
    let mut out: Vec<Active> = agents
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let status = a["status"].as_str().unwrap_or("");
            if !matches!(status, "working" | "blocked") {
                return None;
            }
            let addr = a["address"].as_str()?.to_string();
            let name = a["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&addr).to_string();
            if is_test(&name) {
                return None;
            }
            let job = jobs.iter().rev().find(|j| j.session == addr && j.open());
            Some(Active {
                space: space_of(&addr),
                account: a["account"].as_str().map(String::from),
                status: status.to_string(),
                since: job.map(|j| j.started),
                doing: job.map(|j| j.title.clone()).unwrap_or_default(),
                name,
                addr,
            })
        })
        .collect();
    out.sort_by_key(|a| (a.status != "blocked", a.since.unwrap_or(u64::MAX)));
    out
}

/// With a focus, its space's sessions first; the others come after, dimmed.
pub(super) fn focus_first(list: Vec<Active>, focus: Option<&str>) -> Vec<(Active, bool)> {
    let Some(f) = focus else { return list.into_iter().map(|a| (a, false)).collect() };
    let (mine, rest): (Vec<Active>, Vec<Active>) = list.into_iter().partition(|a| a.space == f);
    mine.into_iter().map(|a| (a, false)).chain(rest.into_iter().map(|a| (a, true))).collect()
}

/// The session view's state.
#[derive(Default)]
pub(super) struct SessionUi {
    pub addr: Option<String>,
    /// The view to go back to.
    back: Option<View>,
    /// Its screen, as last read, and when.
    screen: String,
    read_at: Option<Instant>,
    reading: bool,
    /// The row under the status line.
    strip: ScrollHandle,
    measured: bool,
    note: Option<String>,
}

impl Panel {
    fn actives(&self, cx: &gpui::App) -> Vec<Active> {
        let agents = self.tower.read(cx).s("agents").clone();
        let jobs = dedupe(&self.jobs);
        active_sessions(&agents, &jobs, &|a: &str| self.space_of(a))
    }

    pub(super) fn open_session(&mut self, addr: &str, cx: &mut Context<Self>) {
        if self.view != View::Session {
            self.session.back = Some(self.view);
        }
        self.phone_leave();
        self.session.addr = Some(addr.to_string());
        self.session.screen.clear();
        self.session.read_at = None;
        self.session.note = None;
        self.view = View::Session;
        self.session_tick(cx);
        cx.notify();
    }

    fn show_in_herdr(&mut self, addr: &str, cx: &mut Context<Self>) {
        let addr = addr.to_string();
        let task = cx.background_executor().spawn(async move { crate::show_terminal(&addr) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.session.note = result.err().map(|e| format!("{e:#}"));
                cx.notify();
            });
        })
        .detach();
    }

    /// Every couple of seconds while a session shows: its screen again.
    pub(super) fn session_tick(&mut self, cx: &mut Context<Self>) {
        let Some(addr) = self.session.addr.clone() else { return };
        if self.session.reading || self.session.read_at.is_some_and(|t| t.elapsed() < Duration::from_millis(1900)) {
            return;
        }
        self.session.reading = true;
        let task = cx.background_executor().spawn(async move {
            crate::hc::run(&["read", &addr, "--lines", "40"]).map(|o| if o.code == 0 { o.stdout } else { String::new() }).unwrap_or_default()
        });
        cx.spawn(async move |this, cx| {
            let screen = task.await;
            let _ = this.update(cx, |this, cx| {
                this.session.reading = false;
                this.session.read_at = Some(Instant::now());
                this.session.screen = screen.trim_end().to_string();
                cx.notify();
            });
        })
        .detach();
    }

    /// The row of active sessions, under the status line.
    pub(super) fn now_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let acc = Accounts::of_tower(self.tower.read(cx), self.default_account.clone());
        let focus = self.tower.read(cx).spaces.focus.clone();
        let list = focus_first(self.actives(cx), focus.as_deref());
        if list.is_empty() {
            return div().text_xs().text_color(g.faint).child("Nothing running").into_any_element();
        }
        let now = crate::history::now_ms();
        let mut chips: Vec<AnyElement> = vec![];
        for (a, dim) in &list {
            let blocked = a.status == "blocked";
            let dot = div().size(px(7.0)).flex_none().rounded_full().bg(rgb(if blocked { AMBER } else { CYAN }));
            // Working: a soft pulse; blocked: steady.
            let dot: AnyElement = if blocked {
                dot.into_any_element()
            } else {
                dot.with_animation(
                    SharedString::from(format!("now-dot-{}", a.addr)),
                    Animation::new(Duration::from_millis(1800)).repeat().with_easing(pulsating_between(0.35, 1.0)),
                    |el, d| el.opacity(d),
                )
                .into_any_element()
            };
            let selected = self.view == View::Session && self.session.addr.as_deref() == Some(a.addr.as_str());
            let (addr1, addr2) = (a.addr.clone(), a.addr.clone());
            let mut chip = h_flex()
                .id(SharedString::from(format!("now-{}", a.addr)))
                .flex_none()
                .gap(px(5.0))
                .items_center()
                .pl_2()
                .pr(px(6.0))
                .py(px(2.0))
                .rounded_full()
                .cursor_pointer()
                .text_xs()
                .when(selected, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)))
                .when(!selected && blocked, |el| el.bg(rgba(0xffcf7033)).hover(|s| s.bg(rgba(0xffcf704d))))
                .when(!selected && !blocked, |el| el.bg(rgba(0xffffff1a)).hover(|s| s.bg(rgba(0xffffff2e))))
                .when(*dim, |el| el.opacity(0.55))
                .child(dot)
                .child(div().font_weight(FontWeight::MEDIUM).child(a.name.clone()))
                .when(!a.space.is_empty() && focus.as_deref() != Some(a.space.as_str()), |el| el.child(div().opacity(0.7).child(a.space.clone())))
                .children(a.account.as_deref().map(|x| acc.badge(x, &g)))
                .child(div().opacity(0.8).child(if blocked { "needs you".to_string() } else { a.since.map(|s| span(now.saturating_sub(s))).unwrap_or_default() }))
                .on_click(cx.listener(move |this, _, _, cx| this.open_session(&addr1, cx)))
                .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, _, cx| this.show_in_herdr(&addr2, cx)));
            if !a.doing.is_empty() {
                let doing = a.doing.clone();
                chip = chip.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(doing.clone()).build(window, cx));
            }
            chips.push(chip.into_any_element());
        }
        // Sideways, with arrows when it overflows.
        let handle = self.session.strip.clone();
        let (x, max) = (f32::from(handle.offset().x), f32::from(handle.max_offset().x));
        let scrolls = max > 1.0;
        if !scrolls && !self.session.measured {
            self.session.measured = true;
            let me = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = me.update(cx, |_, cx| cx.notify());
            });
        }
        let arrow = |id: &'static str, text: &'static str, step: f32, on: bool| {
            let handle = handle.clone();
            div()
                .id(id)
                .flex_none()
                .w(px(16.0))
                .text_sm()
                .flex()
                .justify_center()
                .when(on, |el| {
                    el.cursor_pointer().text_color(g.text).on_click(cx.listener(move |_, _, _, cx| {
                        let o = handle.offset();
                        handle.set_offset(gpui::point(o.x + px(step), o.y));
                        cx.notify();
                    }))
                })
                .when(!on, |el| el.text_color(rgba(0xffffff33)))
                .child(text)
        };
        let h2 = handle.clone();
        h_flex()
            .gap(px(2.0))
            .items_center()
            .when(scrolls, |el| el.child(arrow("now-left", "‹", 160.0, x < -1.0)))
            .child(
                div()
                    .id("now-strip")
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .overflow_x_scroll()
                    .track_scroll(&handle)
                    .on_scroll(cx.listener(move |_, ev: &gpui::ScrollEvent, window, cx| {
                        let d = ev.delta.pixel_delta(window.line_height());
                        if d.x.abs() < px(0.5) && d.y.abs() > px(0.0) {
                            let o = h2.offset();
                            h2.set_offset(gpui::point(o.x + d.y, o.y));
                            cx.notify();
                        }
                    }))
                    .child(h_flex().flex_none().gap_1().children(chips)),
            )
            .when(scrolls, |el| el.child(arrow("now-right", "›", -160.0, x > -max + 1.0)))
            .into_any_element()
    }

    /// One session: what it does, its question, its screen, jobs and pictures.
    pub(super) fn session_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let _ = window;
        let g = glass();
        let Some(addr) = self.session.addr.clone() else { return div().into_any_element() };
        let acc = Accounts::of_tower(self.tower.read(cx), self.default_account.clone());
        let agents = self.tower.read(cx).s("agents").clone();
        let agent = agents.as_array().and_then(|a| a.iter().find(|x| x["address"] == addr.as_str())).cloned().unwrap_or_default();
        let jobs: Vec<Job> = dedupe(&self.jobs).into_iter().filter(|j| j.session == addr).collect();
        let name = agent["name"].as_str().map(String::from).or_else(|| jobs.last().map(|j| j.name.clone())).filter(|n| !n.is_empty()).unwrap_or_else(|| addr.clone());
        let status = agent["status"].as_str().unwrap_or("gone").to_string();
        let space = self.space_of(&addr);
        let account = agent["account"].as_str().map(String::from).or_else(|| jobs.last().and_then(|j| j.account.clone()));
        let section = |t: &str| div().pt_2().text_xs().font_weight(FontWeight::MEDIUM).text_color(g.faint).child(t.to_uppercase()).into_any_element();
        let mut items: Vec<AnyElement> = vec![];
        let a1 = addr.clone();
        items.push(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .id("session-back")
                        .px_2()
                        .py(px(3.0))
                        .rounded_full()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(g.muted)
                        .hover(|s| s.bg(rgba(0xffffff1f)))
                        .child("‹ Back")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = this.session.back.take().unwrap_or(View::Chat);
                            this.session.addr = None;
                            cx.notify();
                        })),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("session-show")
                        .px_3()
                        .py(px(4.0))
                        .rounded_full()
                        .cursor_pointer()
                        .text_sm()
                        .border_1()
                        .border_color(g.edge)
                        .hover(|s| s.bg(rgba(0xffffff1f)))
                        .child("Show in Herdr")
                        .on_click(cx.listener(move |this, _, _, cx| this.show_in_herdr(&a1, cx))),
                )
                .into_any_element(),
        );
        let word = match status.as_str() {
            "working" => "working",
            "blocked" => "needs you",
            "done" | "idle" => "idle",
            s => s,
        };
        items.push(div().text_xl().font_weight(FontWeight::MEDIUM).child(name).into_any_element());
        items.push(
            h_flex()
                .gap_2()
                .items_center()
                .text_sm()
                .text_color(g.muted)
                .child(div().size(px(7.0)).rounded_full().bg(rgb(match status.as_str() {
                    "working" => CYAN,
                    "blocked" => AMBER,
                    _ => super::GREY,
                })))
                .child(word.to_string())
                .when(!space.is_empty(), |el| el.child(format!("· {space}")))
                .children(account.as_deref().map(|x| acc.badge(x, &g)))
                .into_any_element(),
        );
        if let Some(n) = &self.session.note {
            items.push(div().text_xs().text_color(rgb(super::ROSE)).child(n.clone()).into_any_element());
        }
        if let Some(j) = jobs.iter().rev().find(|j| j.open()) {
            items.push(section("Doing"));
            items.push(
                div()
                    .text_sm()
                    .child(format!("{} · {}", j.title, span(crate::history::now_ms().saturating_sub(j.started))))
                    .into_any_element(),
            );
        }
        // Its question, answerable here.
        if let Some(set) = self.open_sets(cx).into_iter().find(|s| s.session == addr) {
            items.push(self.question_card(&set, &acc, cx));
        }
        items.push(section("Screen"));
        items.push(
            div()
                .px_3()
                .py_2()
                .rounded(px(12.0))
                .bg(rgba(0x0000003d))
                .font_family("Menlo")
                .text_xs()
                .line_height(px(15.0))
                .text_color(g.muted)
                .child(if self.session.screen.is_empty() { "Reading…".to_string() } else { self.session.screen.clone() })
                .into_any_element(),
        );
        if !jobs.is_empty() {
            items.push(section("Jobs"));
            let all_shots = self.all_shots(cx);
            let now = crate::history::now_ms();
            let mut recent = jobs.clone();
            recent.sort_by_key(|j| std::cmp::Reverse(j.updated));
            for j in recent.iter().take(8) {
                items.push(self.job_row(j, &acc, &all_shots, now, cx));
            }
        }
        let shots: Vec<_> = self.all_shots(cx).into_iter().filter(|s| s.session == addr).take(6).collect();
        if !shots.is_empty() {
            items.push(section("Screenshots"));
            items.push(self.gallery(&shots, &acc, cx));
        }
        div()
            .id("session")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(v_flex().w_full().px_5().pb_3().gap_2().children(items))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(session: &str, title: &str, started: u64) -> Job {
        Job {
            id: format!("{session}-{started}"),
            session: session.into(),
            name: session.into(),
            space: String::new(),
            account: None,
            title: title.into(),
            status: "working".into(),
            summary: String::new(),
            started,
            updated: started,
        }
    }

    #[test]
    fn active_sessions_are_working_or_blocked_blocked_first_then_longest() {
        let agents = json!([
            { "address": "t1", "name": "web-app", "status": "working", "account": "default" },
            { "address": "t2", "name": "api", "status": "blocked" },
            { "address": "t3", "name": "docs", "status": "idle" },
            { "address": "t4", "name": "etl", "status": "working" },
            { "address": "t5", "name": "qfix-two", "status": "working" },
            { "address": "t6", "name": "", "status": "working" }
        ]);
        let jobs = vec![job("t1", "Add the chart", 500), job("t4", "Nightly ETL", 100)];
        let space = |a: &str| if a == "t1" { "Storefront".to_string() } else { String::new() };
        let list = active_sessions(&agents, &jobs, &space);
        assert_eq!(list.iter().map(|a| a.addr.as_str()).collect::<Vec<_>>(), vec!["t2", "t4", "t1", "t6"]);
        assert_eq!(list[2].doing, "Add the chart");
        assert_eq!(list[2].space, "Storefront");
        assert_eq!(list[3].name, "t6");
        // A focus puts its space first; the rest are dimmed.
        let f = focus_first(list.clone(), Some("Storefront"));
        assert_eq!(f[0].0.addr, "t1");
        assert!(!f[0].1 && f[1].1);
        assert!(focus_first(list, None).iter().all(|(_, dim)| !dim));
    }
}
