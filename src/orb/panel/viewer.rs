//! A screenshot, full size: a window over the whole screen with a dark scrim,
//! the image fit to it (a click shows it at 100%, scrolling; another click
//! fits it again), its caption and who sent it beneath. ← and → go to the
//! next and previous ones; Esc, the close button or a click outside the
//! image closes it.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    AnyElement, App, Bounds, Context, FocusHandle, FontWeight, KeyDownEvent, ObjectFit, Render, StyledImage as _, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, img, point, prelude::*, px, rgb, rgba, size,
};
use gpui_kit::component::{h_flex, v_flex};

use super::{ago, badge, glass};
use crate::orb::{mac, shots::Shot};

pub struct Viewer {
    shots: Vec<Shot>,
    index: usize,
    /// At 100% (else fit to the screen).
    actual: bool,
    focus: FocusHandle,
    /// Each image's size in pixels, read from its header once.
    dims: HashMap<PathBuf, Option<(u32, u32)>>,
    /// The default Claude account: its badge is the quiet one.
    default_account: Option<String>,
}

/// Opens the viewer on `shots[index]`, over the screen the pointer is on.
pub fn open(shots: Vec<Shot>, index: usize, default_account: Option<String>, cx: &mut App) {
    if shots.is_empty() {
        return;
    }
    let (mx, my) = mac::mouse();
    let Some(screen) = mac::screen_at(mx, my) else { return };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.0), px(0.0)), size(px(screen.w as f32), px(screen.h as f32))))),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    cx.activate(true);
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        if let Some(w) = mac::Win::of(window) {
            w.set_origin(screen.x, screen.y);
            w.plain(false);
            w.make_key();
        }
        cx.new(|cx| Viewer { index: index.min(shots.len() - 1), shots, actual: false, focus: cx.focus_handle(), dims: HashMap::new(), default_account })
    });
    // The keys (Esc, the arrows) go to the view once it is in its window.
    if let Ok((handle, view)) = opened {
        let _ = handle.update(cx, |_, window, cx| {
            if let Some(w) = mac::Win::of(window) {
                w.make_key();
            }
            let focus = view.read(cx).focus.clone();
            window.focus(&focus, cx);
        });
    }
}

impl Viewer {
    fn go(&mut self, step: isize, cx: &mut Context<Self>) {
        let n = self.shots.len() as isize;
        let i = self.index as isize + step;
        if (0..n).contains(&i) {
            self.index = i as usize;
            self.actual = false;
            cx.notify();
        }
    }

    fn dims(&mut self) -> Option<(u32, u32)> {
        let path = self.shots.get(self.index)?.path.clone();
        *self.dims.entry(path.clone()).or_insert_with(|| image::image_dimensions(&path).ok())
    }
}

/// The image's size on screen: its own size in points (pixels over the
/// screen's scale), shrunk to fit `room` if it's bigger, never enlarged.
pub(super) fn fitted(pixels: (u32, u32), scale: f32, room: (f32, f32)) -> ((f32, f32), bool) {
    let (w, h) = (pixels.0 as f32 / scale, pixels.1 as f32 / scale);
    let k = (room.0 / w).min(room.1 / h).min(1.0);
    ((w * k, h * k), k < 1.0)
}

fn round_button(id: &'static str, text: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(44.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .bg(rgba(0xffffff26))
        .hover(|s| s.bg(rgba(0xffffff40)))
        .text_color(rgb(0xffffff))
        .text_xl()
        .child(text)
}

impl Render for Viewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Nothing else in here takes keys: keep them ours (the kit's root
        // may take focus as the window opens).
        if !self.focus.is_focused(window) {
            window.focus(&self.focus, cx);
        }
        let g = glass();
        let screen = window.viewport_size();
        let (sw, sh) = (f32::from(screen.width), f32::from(screen.height));
        let dims = self.dims();
        let shot = self.shots[self.index].clone();
        let (n, i) = (self.shots.len(), self.index);
        // Room for the arrows at the sides and the caption below.
        let room = ((sw - 2.0 * 84.0).max(100.0), (sh - 56.0 - 120.0).max(100.0));
        let ((w, h), shrunk) = dims.map(|d| fitted(d, window.scale_factor(), room)).unwrap_or(((room.0, room.1), false));
        let full = dims.map(|d| (d.0 as f32 / window.scale_factor(), d.1 as f32 / window.scale_factor()));

        let picture = img(shot.path.clone()).size_full().object_fit(ObjectFit::Contain);
        let zoomable = shrunk || self.actual;
        let image: AnyElement = if self.actual && let Some((fw, fh)) = full {
            // 100%: the whole screen scrolls over it.
            div()
                .id("actual")
                .absolute()
                .inset_0()
                .overflow_scroll()
                .child(
                    div()
                        .id("actual-image")
                        .w(px(fw.max(sw)))
                        .h(px(fh.max(sh)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .id("image")
                                .w(px(fw))
                                .h(px(fh))
                                .flex_none()
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.actual = false;
                                    cx.notify();
                                }))
                                .child(picture),
                        ),
                )
                .into_any_element()
        } else {
            div()
                .id("image")
                .w(px(w))
                .h(px(h))
                .flex_none()
                .rounded(px(6.0))
                .overflow_hidden()
                .shadow_lg()
                .when(zoomable, |el| el.cursor_pointer())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    if zoomable {
                        this.actual = true;
                        cx.notify();
                    }
                }))
                .child(picture)
                .into_any_element()
        };

        let mut meta: Vec<AnyElement> = vec![div().child(shot.who().to_string()).into_any_element()];
        if let Some(a) = &shot.account {
            meta.push(badge(a, self.default_account.as_deref() == Some(a), &g));
        }
        meta.push(div().child(ago(shot.at)).into_any_element());
        if n > 1 {
            meta.push(div().child(format!("{} of {n}", i + 1)).into_any_element());
        }
        if self.actual {
            meta.push(div().child("100%").into_any_element());
        } else if shrunk {
            meta.push(div().child("Click to see it at 100%").into_any_element());
        }
        let caption = v_flex()
            .id("caption")
            .absolute()
            .bottom(px(28.0))
            .left(px(84.0))
            .right(px(84.0))
            .items_center()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                // Its own dark backing: at 100% it sits over the image.
                v_flex()
                    .max_w(px(900.0))
                    .items_center()
                    .gap_1()
                    .px_4()
                    .py_2()
                    .rounded(px(14.0))
                    .bg(rgba(0x05080ecc))
                    .when(!shot.caption.is_empty(), |el| {
                        el.child(div().text_center().text_base().font_weight(FontWeight::MEDIUM).line_clamp(2).child(shot.caption.clone()))
                    })
                    .child(h_flex().gap_2().items_center().text_sm().text_color(g.muted).children(meta)),
            );

        div()
            .id("viewer")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .pb(px(96.0))
            .bg(rgba(0x05080ee6))
            .text_color(rgb(0xffffff))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| match ev.keystroke.key.as_str() {
                "escape" => window.remove_window(),
                "left" => this.go(-1, cx),
                "right" => this.go(1, cx),
                _ => {}
            }))
            // Outside the image: close.
            .on_click(|_, window, _| window.remove_window())
            .child(image)
            .child(caption)
            .child(
                round_button("close", "✕")
                    .absolute()
                    .top(px(20.0))
                    .right(px(20.0))
                    .on_click(|_, window, cx| {
                        cx.stop_propagation();
                        window.remove_window();
                    }),
            )
            .when(i > 0, |el| {
                el.child(round_button("previous", "‹").absolute().left(px(20.0)).top(px(sh / 2.0 - 70.0)).on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.go(-1, cx);
                })))
            })
            .when(i + 1 < n, |el| {
                el.child(round_button("next", "›").absolute().right(px(20.0)).top(px(sh / 2.0 - 70.0)).on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.go(1, cx);
                })))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::fitted;

    #[test]
    fn a_big_screenshot_fits_and_a_small_one_stays_as_it_is() {
        // A Retina screenshot of a 1512x982 screen, on the same screen.
        let ((w, h), shrunk) = fitted((3024, 1964), 2.0, (1344.0, 800.0));
        assert!(shrunk);
        assert!((w - 1231.8).abs() < 1.0 && (h - 800.0).abs() < 0.01, "{w}x{h}");
        // A small one isn't blown up.
        assert_eq!(fitted((600, 400), 2.0, (1344.0, 800.0)), ((300.0, 200.0), false));
    }
}
