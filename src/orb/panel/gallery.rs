//! The screenshots sessions sent, in the full window (W15 in the design): a
//! gallery, three a row, newest first, each with its caption, where it came
//! from and when. A click opens the viewer, over the whole screen (W16).
//! Thumbnails are made the first time one is drawn, in the background, and
//! kept on disk.

use gpui::{AnyElement, Context, FontWeight, ObjectFit, SharedString, StyledImage as _, div, img, prelude::*, px, rgba};
use gpui_kit::component::{h_flex, v_flex};

use super::{Panel, Thumb, ago, badge, glass, viewer};
use crate::orb::shots::{self, Shot};

/// A gallery tile's width (three a row in the window's pane), and the gap between them.
const TILE_W: f32 = 218.0;
const GAP: f32 = 14.0;

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
        viewer::open(list, index, self.accounts.as_ref().and_then(|a| a.first().cloned()), cx);
    }

    /// The gallery: three tiles a row.
    pub(super) fn gallery(&mut self, list: &[Shot], cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let default = self.accounts.as_ref().and_then(|a| a.first().cloned());
        let mut tiles: Vec<AnyElement> = vec![];
        for (i, s) in list.iter().enumerate() {
            let thumb = self.thumb_box(s, TILE_W, (TILE_W * 0.625).round(), 10.0, cx);
            let all = list.to_vec();
            let account = s.account.as_deref().map(|a| badge(a, default.as_deref() == Some(a), &g));
            let from = if s.space.is_empty() { s.who().to_string() } else { format!("{} › {}", s.space, s.who()) };
            tiles.push(
                v_flex()
                    .id(SharedString::from(format!("shot-{i}")))
                    .w(px(TILE_W))
                    .gap(px(5.0))
                    .cursor_pointer()
                    .hover(|st| st.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_viewer(all.clone(), i, cx)))
                    .child(thumb)
                    .when(!s.caption.is_empty(), |el| {
                        el.child(div().text_size(px(13.0)).line_height(px(18.0)).font_weight(FontWeight::SEMIBOLD).line_clamp(2).child(s.caption.clone()))
                    })
                    .child(
                        h_flex()
                            .gap(px(5.0))
                            .items_center()
                            .text_size(px(12.0))
                            .text_color(g.faint)
                            .child(div().min_w_0().truncate().child(from))
                            .children(account)
                            .child(div().flex_none().child(format!("· {}", ago(s.at)))),
                    )
                    .into_any_element(),
            );
        }
        h_flex().w_full().flex_wrap().items_start().gap_x(px(GAP)).gap_y(px(18.0)).children(tiles).into_any_element()
    }
}
