//! Screenshots in the panel: the Shots gallery in Activity (two thumbnails a
//! row, newest first, each with its caption, session and account) and the
//! small ones on a job's card. A click opens the viewer. Thumbnails are made
//! the first time one is drawn, in the background, and kept on disk.

use gpui::{
    AnyElement, Context, FontWeight, ObjectFit, SharedString, StyledImage as _, div, img, prelude::*, px, rgba,
};
use gpui_kit::component::{h_flex, v_flex};

use super::{Accounts, Panel, Thumb, ago, glass, viewer};
use crate::history::Job;
use crate::orb::shots::{self, Shot};

/// A gallery tile's width (two a row in the panel).
const TILE_W: f32 = 162.0;

impl Panel {
    /// Every screenshot: history's and the state's recent list, newest first.
    pub(super) fn all_shots(&self, cx: &gpui::App) -> Vec<Shot> {
        let t = self.tower.read(cx);
        shots::merge(&self.shots, t.state.as_ref().unwrap_or(&serde_json::Value::Null))
    }

    /// The thumbnail's file once it's made; starts making it the first time.
    fn thumb(&mut self, shot: &Shot, cx: &mut Context<Self>) -> Thumb {
        if let Some(t) = self.thumbs.get(&shot.path) {
            return t.clone();
        }
        self.thumbs.insert(shot.path.clone(), Thumb::Making);
        let src = shot.path.clone();
        let task = cx.background_executor().spawn({
            let src = src.clone();
            async move { shots::make_thumb(&shots::thumbs_dir(), &src) }
        });
        cx.spawn(async move |this, cx| {
            let made = task.await;
            let _ = this.update(cx, |this, cx| {
                this.thumbs.insert(src, made.map(Thumb::Ready).unwrap_or(Thumb::Failed));
                cx.notify();
            });
        })
        .detach();
        Thumb::Making
    }

    /// A thumbnail `w` x `h`, cropped to fill it.
    fn thumb_box(&mut self, shot: &Shot, w: f32, h: f32, radius: f32, cx: &mut Context<Self>) -> gpui::Div {
        let thumb = self.thumb(shot, cx);
        let frame = div().w(px(w)).h(px(h)).flex_none().rounded(px(radius)).overflow_hidden().bg(rgba(0xffffff1a));
        match thumb {
            Thumb::Ready(p) => frame.child(img(p).size_full().object_fit(ObjectFit::Cover)),
            Thumb::Making => frame,
            Thumb::Failed => frame.flex().items_center().justify_center().text_xs().text_color(glass().faint).child("No preview"),
        }
    }

    fn open_viewer(&mut self, list: Vec<Shot>, index: usize, cx: &mut Context<Self>) {
        viewer::open(list, index, self.default_account.clone(), cx);
    }

    /// The gallery: two tiles a row.
    pub(super) fn gallery(&mut self, list: &[Shot], acc: &Accounts, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let mut tiles: Vec<AnyElement> = vec![];
        for (i, s) in list.iter().enumerate() {
            let thumb = self.thumb_box(s, TILE_W, TILE_W * 0.625, 12.0, cx);
            let all = list.to_vec();
            let account = s.account.as_deref().map(|a| acc.badge(a, &g));
            tiles.push(
                v_flex()
                    .id(SharedString::from(format!("shot-{i}")))
                    .w(px(TILE_W))
                    .gap(px(5.0))
                    .cursor_pointer()
                    .hover(|st| st.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_viewer(all.clone(), i, cx)))
                    .child(thumb)
                    .when(!s.caption.is_empty(), |el| el.child(div().text_sm().line_clamp(2).child(s.caption.clone())))
                    .child(
                        h_flex()
                            .gap(px(5.0))
                            .items_center()
                            .text_xs()
                            .text_color(g.muted)
                            .child(div().min_w_0().truncate().child(s.who().to_string()))
                            .children(account)
                            .child(div().flex_none().text_color(g.faint).child(ago(s.at))),
                    )
                    .into_any_element(),
            );
        }
        h_flex().flex_wrap().gap_x_2().gap_y_4().children(tiles).into_any_element()
    }

    /// A job's screenshots, small, under its card (`None` without any).
    pub(super) fn job_shots(&mut self, job: &Job, all: &[Shot], cx: &mut Context<Self>) -> Option<AnyElement> {
        let mine: Vec<Shot> = all.iter().filter(|s| of_job(s, job)).cloned().collect();
        if mine.is_empty() {
            return None;
        }
        let more = mine.len().saturating_sub(4);
        let mut row: Vec<AnyElement> = vec![];
        for (i, s) in mine.iter().take(4).enumerate() {
            let thumb = self.thumb_box(s, 60.0, 38.0, 6.0, cx);
            let list = mine.clone();
            row.push(
                div()
                    .id(SharedString::from(format!("job-{}-shot-{i}", job.id)))
                    .cursor_pointer()
                    .hover(|st| st.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_viewer(list.clone(), i, cx);
                    }))
                    .child(thumb)
                    .into_any_element(),
            );
        }
        if more > 0 {
            row.push(div().text_xs().text_color(glass().muted).font_weight(FontWeight::MEDIUM).child(format!("+{more}")).into_any_element());
        }
        Some(h_flex().gap(px(6.0)).items_center().children(row).into_any_element())
    }
}

/// What the chat shows, in order: an entry as it is, or screenshots a
/// session sent in one go, as one card (indices into the chat's entries).
#[derive(Debug, PartialEq)]
pub(super) enum ChatRow {
    Entry(usize),
    Shots(Vec<usize>),
}

/// The chat's rows: screenshots sent one after another by the same session
/// within two minutes make one card; one the tower attached to a message
/// (`attached`, by id) shows under that message instead.
pub(super) fn chat_rows(items: &[crate::history::Entry], attached: &std::collections::HashSet<String>) -> Vec<ChatRow> {
    let mut rows: Vec<ChatRow> = vec![];
    for (i, e) in items.iter().enumerate() {
        if e.kind != "screenshot" {
            rows.push(ChatRow::Entry(i));
            continue;
        }
        if e.id.as_ref().is_some_and(|id| attached.contains(id)) {
            continue;
        }
        if let Some(ChatRow::Shots(group)) = rows.last_mut() {
            let first = &items[group[0]];
            if first.session == e.session && e.at.saturating_sub(first.at) <= 2 * 60 * 1000 {
                group.push(i);
                continue;
            }
        }
        rows.push(ChatRow::Shots(vec![i]));
    }
    rows
}

/// The chat card's width, and its pictures' (inside its padding).
const CARD_W: f32 = 310.0;
const PIC_W: f32 = CARD_W - 24.0;

impl Panel {
    /// Screenshots in the chat: the pictures (one large, several two a row),
    /// the caption, and, for a card of its own (`header`), who sent them.
    /// `list` is every screenshot in the chat, `start` where these begin in
    /// it, so the viewer's arrows go through all of them.
    pub(super) fn shot_card(&mut self, shots: &[Shot], list: &[Shot], start: usize, header: bool, at: u64, acc: &Accounts, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let mut pics: Vec<AnyElement> = vec![];
        for (k, s) in shots.iter().enumerate() {
            let (w, h) = if shots.len() == 1 {
                // Its height from the file's header, so the chat doesn't jump
                // when the thumbnail comes in.
                let dims = *self.dims.entry(s.path.clone()).or_insert_with(|| image::image_dimensions(&s.path).ok());
                let h = match dims {
                    Some((iw, ih)) if iw > 0 => (PIC_W * ih as f32 / iw as f32).clamp(80.0, 320.0),
                    _ => PIC_W * 0.625,
                };
                (PIC_W, h)
            } else {
                (((PIC_W - 6.0) / 2.0).floor() - 1.0, 92.0)
            };
            let thumb = self.thumb_box(s, w, h, 10.0, cx);
            let all = list.to_vec();
            let index = start + k;
            pics.push(
                div()
                    .id(SharedString::from(format!("chat-shot-{index}")))
                    .cursor_pointer()
                    .hover(|st| st.opacity(0.88))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_viewer(all.clone(), index, cx)))
                    .child(thumb)
                    .into_any_element(),
            );
        }
        let first = &shots[0];
        let mut captions: Vec<String> = vec![];
        for s in shots {
            if !s.caption.is_empty() && !captions.contains(&s.caption) {
                captions.push(s.caption.clone());
            }
        }
        let account = first.account.as_deref().map(|a| acc.badge(a, &g));
        let who = first.who().to_string();
        let line = if header {
            let n = shots.len();
            format!("{who} sent {}", if n == 1 { "a screenshot".to_string() } else { format!("{n} screenshots") })
        } else {
            who
        };
        let card = v_flex()
            .w(px(CARD_W))
            .p_3()
            .gap_2()
            .rounded(px(20.0))
            .bg(g.bubble_tower)
            .border_1()
            .border_color(g.edge)
            .child(h_flex().flex_wrap().gap(px(6.0)).children(pics))
            .children(captions.into_iter().take(3).map(|c| div().text_sm().line_height(px(20.0)).line_clamp(4).child(c)))
            .child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .text_xs()
                    .text_color(g.muted)
                    .child(div().min_w_0().truncate().child(line))
                    .children(account)
                    .child(div().flex_none().text_color(g.faint).child(super::clock(at))),
            );
        h_flex().w_full().child(card).into_any_element()
    }
}

/// A screenshot belongs to a job by its id, or (none given) by coming from
/// the job's session while it ran.
pub(super) fn of_job(s: &Shot, job: &Job) -> bool {
    match &s.job {
        Some(id) => *id == job.id,
        None => s.session == job.session && s.at >= job.started && (job.open() || s.at <= job.updated),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn screenshots_sent_together_make_one_chat_card() {
        use crate::history::Entry;
        let e = |at: u64, kind: &str, session: &str, id: &str| Entry {
            at,
            kind: kind.into(),
            session: (!session.is_empty()).then(|| session.to_string()),
            id: (!id.is_empty()).then(|| id.to_string()),
            ..Default::default()
        };
        let items = vec![
            e(0, "request", "", ""),
            e(10_000, "screenshot", "t26", "a"),
            e(11_000, "screenshot", "t26", "b"),
            e(12_000, "screenshot", "t3", "c"),
            e(500_000, "screenshot", "t3", "d"),
            e(600_000, "answer", "", ""),
            e(700_000, "screenshot", "t3", "e"),
        ];
        let none = std::collections::HashSet::new();
        assert_eq!(
            chat_rows(&items, &none),
            vec![ChatRow::Entry(0), ChatRow::Shots(vec![1, 2]), ChatRow::Shots(vec![3]), ChatRow::Shots(vec![4]), ChatRow::Entry(5), ChatRow::Shots(vec![6])]
        );
        // The tower attached b and d to a message: they show there, not here.
        let attached: std::collections::HashSet<String> = ["b", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            chat_rows(&items, &attached),
            vec![ChatRow::Entry(0), ChatRow::Shots(vec![1]), ChatRow::Shots(vec![3]), ChatRow::Entry(5), ChatRow::Shots(vec![6])]
        );
    }

    #[test]
    fn a_screenshot_goes_with_its_job() {
        let job = Job {
            id: "t3-100".into(),
            session: "t3".into(),
            name: String::new(),
            space: String::new(),
            account: None,
            title: String::new(),
            status: "done".into(),
            summary: String::new(),
            started: 100,
            updated: 200,
        };
        let shot = |job: Option<&str>, session: &str, at: u64| Shot {
            id: String::new(),
            at,
            session: session.into(),
            name: String::new(),
            space: String::new(),
            account: None,
            caption: String::new(),
            path: PathBuf::from("/x.png"),
            job: job.map(String::from),
        };
        assert!(of_job(&shot(Some("t3-100"), "t3", 999), &job));
        assert!(!of_job(&shot(Some("t3-50"), "t3", 150), &job));
        assert!(of_job(&shot(None, "t3", 150), &job));
        assert!(!of_job(&shot(None, "t3", 250), &job));
        assert!(!of_job(&shot(None, "t4", 150), &job));
    }
}
