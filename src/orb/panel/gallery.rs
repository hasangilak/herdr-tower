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
