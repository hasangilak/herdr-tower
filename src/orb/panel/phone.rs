//! The panel's Phone view: the phone API for the Android app (on or off,
//! where it listens), pairing a phone with a QR code of the one-time link,
//! and the paired phones, each revocable. Everything goes through
//! `crate::phone`, as `tower phone` does. The code is never logged.

use std::time::{Duration, Instant};

use gpui::{
    App, AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Bounds,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, point, prelude::FluentBuilder as _, px, rgb, rgba, size,
};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use serde_json::Value;

use super::{Panel, glass};
use crate::phone::Status;

/// What the Phone view holds between frames.
#[derive(Default)]
pub(super) struct PhoneView {
    status: Option<Status>,
    /// A status read is on its way.
    loading: bool,
    /// An action is (enable, pair, ...).
    busy: bool,
    pairing: Option<Pairing>,
    /// The device asked to revoke, waiting for a second click.
    revoking: Option<String>,
    /// Under the status, briefly: (it worked, what to say).
    note: Option<(bool, String)>,
    /// When the status was last read.
    read_at: Option<Instant>,
}

struct Pairing {
    code: String,
    host: String,
    port: u64,
    fingerprint: String,
    until: Instant,
    qr: Qr,
    /// The paired phones when it opened, to tell when one used it.
    devices_before: Vec<String>,
}

/// A QR code's modules, row by row (`true`: dark).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Qr {
    pub width: usize,
    pub dark: Vec<bool>,
}

impl Qr {
    pub fn new(text: &str) -> Option<Qr> {
        let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
        let width = code.width();
        let dark = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
        Some(Qr { width, dark })
    }

    /// The dark modules as horizontal runs `(row, first column, length)`:
    /// one rectangle each, so neighbours never show a seam.
    pub fn runs(&self) -> Vec<(usize, usize, usize)> {
        let mut out = vec![];
        for y in 0..self.width {
            let row = &self.dark[y * self.width..(y + 1) * self.width];
            let mut x = 0;
            while x < self.width {
                if row[x] {
                    let start = x;
                    while x < self.width && row[x] {
                        x += 1;
                    }
                    out.push((y, start, x - start));
                } else {
                    x += 1;
                }
            }
        }
        out
    }
}

/// The quiet zone round the code, in modules (the standard asks for 4).
const QUIET: usize = 4;
/// How long a code lives (the daemon's `CODE_TTL`; its answer says).
const CODE_SECS: u64 = 300;

/// `4:07`.
pub(super) fn countdown(left: Duration) -> String {
    let s = left.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// `ABCD EFGH`: the code in two halves, easier to type.
pub(super) fn spaced(code: &str) -> String {
    let mid = code.len() / 2;
    if code.is_char_boundary(mid) && code.len() >= 6 { format!("{} {}", &code[..mid], &code[mid..]) } else { code.to_string() }
}

/// What the pairing should do now that the status says this: `None` keep
/// it, `Some(Ok(name))` a phone paired with it, `Some(Err(()))` it's gone
/// (the API went off, or the code was used by nobody we know).
pub(super) fn pairing_outcome(before: &[String], status: &Status) -> Option<Result<String, ()>> {
    if let Some(d) = status.devices.iter().find(|d| !before.contains(&d.id)) {
        return Some(Ok(d.name.clone()));
    }
    if status.url.is_none() {
        return Some(Err(()));
    }
    None
}

impl Panel {
    /// Reads the status (in the background); every 2 s while the view is open.
    pub(super) fn phone_refresh(&mut self, cx: &mut Context<Self>) {
        if self.phone.loading {
            return;
        }
        self.phone.loading = true;
        let task = cx.background_executor().spawn(async { crate::phone::status() });
        cx.spawn(async move |this, cx| {
            let status = task.await;
            let _ = this.update(cx, |this, cx| {
                this.phone.loading = false;
                this.phone.read_at = Some(Instant::now());
                this.phone_status(status, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn phone_status(&mut self, status: Status, cx: &mut Context<Self>) {
        if let Some(p) = &self.phone.pairing {
            match pairing_outcome(&p.devices_before, &status) {
                Some(Ok(name)) => {
                    self.phone.pairing = None;
                    self.phone.note = Some((true, format!("Paired {name}.")));
                }
                Some(Err(())) => self.phone.pairing = None,
                None => {}
            }
        }
        self.phone.status = Some(status);
        // The code ran out with nobody pairing: a fresh one.
        if self.phone.pairing.as_ref().is_some_and(|p| Instant::now() >= p.until) && !self.phone.busy {
            self.phone_pair(cx);
        }
    }

    /// Each second while the view is open: the countdown ticks, the status
    /// is read every other second.
    pub(super) fn phone_tick(&mut self, cx: &mut Context<Self>) {
        if self.phone.read_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(1900)) {
            self.phone_refresh(cx);
        }
        cx.notify();
    }

    /// Leaving the view: the code stops showing.
    pub(super) fn phone_leave(&mut self) {
        self.phone.pairing = None;
        self.phone.revoking = None;
        self.phone.note = None;
    }

    fn phone_pair(&mut self, cx: &mut Context<Self>) {
        self.phone.busy = true;
        self.phone.note = None;
        let before: Vec<String> = self.phone.status.as_ref().map(|s| s.devices.iter().map(|d| d.id.clone()).collect()).unwrap_or_default();
        let task = cx.background_executor().spawn(async { crate::phone::pair() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.phone.busy = false;
                match result.ok().and_then(|v| pairing_of(&v, before)) {
                    Some(p) => this.phone.pairing = Some(p),
                    None => {
                        this.phone.pairing = None;
                        this.phone.note = Some((false, "Couldn't open a pairing code. Is the phone API on?".into()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn phone_switch(&mut self, on: bool, cx: &mut Context<Self>) {
        self.phone.busy = true;
        self.phone.note = None;
        self.phone.pairing = None;
        let task = cx.background_executor().spawn(async move {
            if on { crate::phone::enable().map(|v| v.is_some()) } else { crate::phone::disable().map(|_| true) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.phone.busy = false;
                this.phone.note = match result {
                    Ok(true) => None,
                    Ok(false) => Some((true, "Enabled: it starts with the voice.".into())),
                    Err(e) => Some((false, format!("{e:#}"))),
                };
                this.phone.read_at = None;
                this.phone_refresh(cx);
            });
        })
        .detach();
    }

    fn phone_revoke(&mut self, id: String, cx: &mut Context<Self>) {
        self.phone.busy = true;
        self.phone.revoking = None;
        let task = cx.background_executor().spawn(async move { crate::phone::revoke(&id) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.phone.busy = false;
                if let Err(e) = result {
                    this.phone.note = Some((false, format!("{e:#}")));
                }
                this.phone.read_at = None;
                this.phone_refresh(cx);
            });
        })
        .detach();
    }

    pub(super) fn phone_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let section = |t: &str| div().pt_2().text_xs().font_weight(FontWeight::MEDIUM).text_color(g.faint).child(t.to_uppercase());
        let mut items: Vec<AnyElement> = vec![];
        let busy = self.phone.busy;
        let Some(st) = self.phone.status.clone() else {
            items.push(div().pt_4().text_sm().text_color(g.muted).child("Looking…").into_any_element());
            return body(items);
        };
        let on = st.url.is_some();

        // On or off, and where.
        let (dot, line, sub) = match (&st.url, st.voice, st.enabled) {
            (Some(url), _, _) => (0x6eeaa8, "On".to_string(), url.trim_start_matches("https://").to_string()),
            (None, false, true) => (0xffcf70, "Enabled".to_string(), "It starts with the voice (tower up).".to_string()),
            (None, false, false) => (0x9aa3b2, "Off".to_string(), "The voice isn't running.".to_string()),
            (None, true, _) => (0x9aa3b2, "Off".to_string(), "Phones can't reach the tower.".to_string()),
        };
        items.push(section("Phone app").into_any_element());
        items.push(
            h_flex()
                .gap_3()
                .items_center()
                .p_3()
                .rounded(px(14.0))
                .bg(g.bubble_tower)
                .child(div().size(px(9.0)).flex_none().rounded_full().bg(rgb(dot)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_base().font_weight(FontWeight::MEDIUM).child(line))
                        .child(div().text_sm().text_color(g.muted).truncate().child(sub)),
                )
                .child(
                    pill("phone-switch", if on { "Turn off" } else { "Turn on" }, !on, busy)
                        .on_click(cx.listener(move |this, _, _, cx| this.phone_switch(!on, cx))),
                )
                .into_any_element(),
        );
        if let Some((ok, text)) = &self.phone.note {
            items.push(div().text_sm().text_color(if *ok { rgb(0x6eeaa8) } else { rgb(0xff8f8f) }).child(text.clone()).into_any_element());
        }

        // Pairing.
        if on {
            items.push(section("Pair a phone").into_any_element());
            match &self.phone.pairing {
                None => items.push(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(g.muted)
                                .child("Shows a QR code to scan in the Tower app on Android. The code works once, for 5 minutes."),
                        )
                        .child(
                            h_flex().child(
                                pill("phone-pair", if busy { "Opening…" } else { "Pair a phone" }, true, busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.phone_pair(cx))),
                            ),
                        )
                        .into_any_element(),
                ),
                Some(p) => {
                    let left = p.until.saturating_duration_since(Instant::now());
                    items.push(
                        v_flex()
                            .items_center()
                            .gap_1()
                            .p_3()
                            .rounded(px(18.0))
                            .bg(rgb(0xffffff))
                            .text_color(rgb(0x1d2330))
                            .child(qr_element(&p.qr))
                            .child(div().text_sm().text_color(rgb(0x5b6475)).child("Scan in the Tower app, or enter"))
                            .child(
                                div()
                                    .text_lg()
                                    .font_family("Menlo")
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(spaced(&p.code)),
                            )
                            .child(
                                div().text_xs().font_family("Menlo").text_color(rgb(0x5b6475)).child(format!(
                                    "{}:{} · {}",
                                    p.host,
                                    p.port,
                                    crate::daemon::lan::tls::grouped(&p.fingerprint[..8.min(p.fingerprint.len())])
                                )),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .pt_1()
                                    .child(div().text_sm().text_color(rgb(0x5b6475)).child(format!("New code in {}", countdown(left))))
                                    .child(
                                        div()
                                            .id("phone-new-code")
                                            .p(px(4.0))
                                            .rounded_full()
                                            .cursor_pointer()
                                            .text_color(rgb(0x5b6475))
                                            .hover(|s| s.bg(rgb(0xeef1f5)))
                                            .child(Icon::new(IconName::RefreshCw).small())
                                            .on_click(cx.listener(|this, _, _, cx| this.phone_pair(cx))),
                                    )
                                    .child(
                                        div()
                                            .id("phone-pair-done")
                                            .px_2()
                                            .py(px(2.0))
                                            .rounded_full()
                                            .cursor_pointer()
                                            .text_sm()
                                            .text_color(rgb(0x5b6475))
                                            .hover(|s| s.bg(rgb(0xeef1f5)))
                                            .child("Done")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.phone.pairing = None;
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .into_any_element(),
                    );
                }
            }
        }

        // Paired phones.
        items.push(section("Paired phones").into_any_element());
        if st.devices.is_empty() {
            items.push(div().text_sm().text_color(g.muted).child("None yet.").into_any_element());
        }
        for d in &st.devices {
            let asking = self.phone.revoking.as_deref() == Some(d.id.as_str());
            let id = d.id.clone();
            let seen = if d.last_seen_ms == 0 { "never seen".to_string() } else { format!("last seen {}", crate::phone::ago(d.last_seen_ms)) };
            items.push(
                h_flex()
                    .gap_3()
                    .items_center()
                    .px_3()
                    .py_2()
                    .rounded(px(14.0))
                    .bg(g.bubble_tower)
                    .child(glyph(g.muted.into()))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).truncate().child(if d.name.is_empty() { d.id.clone() } else { d.name.clone() }))
                            .child(div().text_xs().text_color(g.faint).child(seen)),
                    )
                    .child(
                        div()
                            .id(gpui::SharedString::from(format!("revoke-{}", d.id)))
                            .px_2()
                            .py(px(3.0))
                            .rounded_full()
                            .cursor_pointer()
                            .text_sm()
                            .when(asking, |el| el.bg(rgb(0xff8f8f)).text_color(rgb(0x16233a)).font_weight(FontWeight::MEDIUM))
                            .when(!asking, |el| el.text_color(g.muted).hover(|s| s.bg(rgba(0xffffff1f))))
                            .child(if asking { "Revoke?" } else { "Revoke" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.phone.revoking.as_deref() == Some(id.as_str()) {
                                    this.phone_revoke(id.clone(), cx);
                                } else {
                                    this.phone.revoking = Some(id.clone());
                                }
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        body(items)
    }
}

/// A phone, drawn (the kit's bundled icons have none): a rounded outline
/// with a home bar.
pub(super) fn glyph(color: gpui::Hsla) -> gpui::Div {
    div()
        .flex_none()
        .w(px(11.0))
        .h(px(17.0))
        .rounded(px(3.0))
        .border(px(1.5))
        .border_color(color)
        .flex()
        .flex_col()
        .justify_end()
        .items_center()
        .pb(px(1.5))
        .child(div().w(px(3.5)).h(px(1.5)).rounded_full().bg(color))
}

fn body(items: Vec<AnyElement>) -> AnyElement {
    div()
        .id("phone")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(v_flex().w_full().px_5().pb_4().gap_2().children(items))
        .into_any_element()
}

/// A rounded button: filled (the action to take) or outlined.
fn pill(id: &'static str, text: &'static str, filled: bool, busy: bool) -> gpui::Stateful<gpui::Div> {
    let g = glass();
    div()
        .id(id)
        .flex_none()
        .px_3()
        .py(px(5.0))
        .rounded_full()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .when(filled, |el| el.bg(rgb(super::PALE)).text_color(rgb(super::NAVY)))
        .when(!filled, |el| el.border_1().border_color(g.edge))
        .when(busy, |el| el.opacity(0.5))
        .when(!busy, |el| el.cursor_pointer().hover(|s| s.opacity(0.85)))
        .child(text)
}

/// The daemon's answer to `pair`, ready to show.
fn pairing_of(v: &Value, devices_before: Vec<String>) -> Option<Pairing> {
    let link = v["link"].as_str()?;
    Some(Pairing {
        code: v["code"].as_str()?.to_string(),
        host: v["host"].as_str().unwrap_or("").to_string(),
        port: v["port"].as_u64().unwrap_or(0),
        fingerprint: v["fingerprint"].as_str().unwrap_or("").to_string(),
        until: Instant::now() + Duration::from_secs(v["expires_in_s"].as_u64().unwrap_or(CODE_SECS)),
        qr: Qr::new(link)?,
        devices_before,
    })
}

/// The code, black on white with its quiet zone, each module a whole
/// number of pixels so it stays crisp.
fn qr_element(qr: &Qr) -> impl IntoElement {
    let n = qr.width + 2 * QUIET;
    // About 180 px, whatever the version (a version 7 link: 3 px a module,
    // 6 on a Retina screen).
    let module = (186.0 / n as f32).floor().max(3.0);
    let side = module * n as f32;
    let runs = qr.runs();
    canvas(
        |_, _, _| (),
        move |bounds: Bounds<Pixels>, _, window: &mut Window, _: &mut App| {
            let o = point(bounds.origin.x.round(), bounds.origin.y.round());
            window.paint_quad(gpui::fill(Bounds::new(o, size(px(side), px(side))), rgb(0xffffff)));
            for &(y, x, len) in &runs {
                let at = point(o.x + px((x + QUIET) as f32 * module), o.y + px((y + QUIET) as f32 * module));
                window.paint_quad(gpui::fill(Bounds::new(at, size(px(len as f32 * module), px(module))), rgb(0x000000)));
            }
        },
    )
    .w(px(side))
    .h(px(side))
    .flex_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::lan::auth::Device;

    #[test]
    fn the_qr_code_is_whole() {
        let link = "herdertower://pair?h=192.168.1.20&p=7843&c=ABCD2345&fp=".to_string() + &"ab".repeat(32);
        let qr = Qr::new(&link).unwrap();
        assert_eq!(qr.dark.len(), qr.width * qr.width);
        // A finder pattern in three corners: a dark 7x7 ring round a light one.
        let at = |x: usize, y: usize| qr.dark[y * qr.width + x];
        for (cx, cy) in [(0, 0), (qr.width - 7, 0), (0, qr.width - 7)] {
            assert!((0..7).all(|i| at(cx + i, cy) && at(cx + i, cy + 6) && at(cx, cy + i) && at(cx + 6, cy + i)));
            assert!(!at(cx + 1, cy + 1) && at(cx + 3, cy + 3));
        }
        // The runs paint exactly the dark modules.
        let mut painted = vec![false; qr.dark.len()];
        for (y, x, len) in qr.runs() {
            for i in x..x + len {
                assert!(!painted[y * qr.width + i]);
                painted[y * qr.width + i] = true;
            }
        }
        assert_eq!(painted, qr.dark);
    }

    #[test]
    fn the_code_counts_down_and_reads_easily() {
        assert_eq!(countdown(Duration::from_secs(300)), "5:00");
        assert_eq!(countdown(Duration::from_millis(247_900)), "4:07");
        assert_eq!(countdown(Duration::ZERO), "0:00");
        assert_eq!(spaced("ABCD2345"), "ABCD 2345");
    }

    #[test]
    fn a_new_phone_ends_the_pairing() {
        let dev = |id: &str| Device { id: id.into(), name: format!("Pixel {id}"), token_sha256: String::new(), created_ms: 1, last_seen_ms: 0, audio_route: "phone".into() };
        let st = |url: Option<&str>, ids: &[&str]| Status {
            enabled: true,
            voice: true,
            url: url.map(String::from),
            pairing_expires_in_s: None,
            devices: ids.iter().map(|i| dev(i)).collect(),
        };
        let before = vec!["d1".to_string()];
        assert_eq!(pairing_outcome(&before, &st(Some("https://x:1"), &["d1"])), None);
        assert_eq!(pairing_outcome(&before, &st(Some("https://x:1"), &["d1", "d2"])), Some(Ok("Pixel d2".into())));
        assert_eq!(pairing_outcome(&before, &st(None, &["d1"])), Some(Err(())));
    }

    #[test]
    fn a_pairing_answer_becomes_a_code_to_show() {
        let v = serde_json::json!({ "code": "ABCD2345", "host": "10.0.0.2", "port": 7843, "fingerprint": "ff".repeat(32),
            "link": "herdertower://pair?h=10.0.0.2&p=7843&c=ABCD2345&fp=ff", "expires_in_s": 300 });
        let p = pairing_of(&v, vec![]).unwrap();
        assert_eq!((p.code.as_str(), p.host.as_str(), p.port), ("ABCD2345", "10.0.0.2", 7843));
        assert!(p.until > Instant::now() + Duration::from_secs(299));
        assert!(pairing_of(&serde_json::json!({ "error": "off" }), vec![]).is_none());
    }
}
