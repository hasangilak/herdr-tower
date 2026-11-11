//! The phone, in Settings (the full window's "Phone" card): pairing the
//! Android app happens there, and says what the network asks of it.
//!
//! - **Where the tower is reachable now**: "This Mac is on Office ·
//!   10.10.30.116" (a network is told by its address range; "Name this
//!   network" names it in place, `[lan.networks]` in config.toml), and
//!   Tailscale in a line: running (reachable anywhere on the tailnet),
//!   stopped (this network only; "Open Tailscale"), or not installed.
//! - **Pair a phone**, inline: the QR, the 8-digit code to type instead,
//!   the countdown (a fresh code when it runs out), and what the network
//!   asks ("Your phone must be on the same Wi-Fi as this Mac (Office), or
//!   both on Tailscale."). When a phone pairs, or pairs again, the box says
//!   so: "Paired: OnePlus on Office", "Paired again: OnePlus, now knows Office".
//! - **Paired phones**, a row each: its name (renamed in place), whether it
//!   is connected and how, else when it was last seen and where; the
//!   networks it knows; whether it can reach the tower here, and if not,
//!   "Pair it here"; where its replies are heard; Remove (asked twice).
//!
//! Everything goes through `crate::phone`, as `tower phone` does, off the UI
//! thread. The code is never logged. `Scope::Phone` opens Settings at this
//! card (`window::follow_nav`).

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, Bounds, Context, Entity, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, canvas, div, point, prelude::FluentBuilder as _, px,
    rgb, rgba, size,
};
use gpui_kit::component::input::{InputEvent, RopeExt as _, TextareaState};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use serde_json::Value;

use super::home::{CARD, LINE, SOFT, acts_row, bay_label, mbtn, tg};
use super::launch::field;
use super::{AMBER, CYAN, NAVY, PALE, Panel, ROSE, glass};
use crate::daemon::lan::network::{self, Reach};
use crate::phone::Status;

/// What the Phone card holds between frames.
#[derive(Default)]
pub(super) struct PhoneView {
    status: Option<Status>,
    /// A status read is on its way.
    loading: bool,
    /// An action is (turning it on, pairing, removing).
    busy: bool,
    pairing: Option<Pairing>,
    /// What the pairing box says once a phone used it, until it is closed.
    paired: Option<String>,
    /// The device asked to remove, waiting for a second click.
    revoking: Option<String>,
    /// Under the card's first lines, briefly: (it worked, what to say).
    note: Option<(bool, String)>,
    /// When the status was last read.
    read_at: Option<Instant>,
    /// The network's name, being typed.
    naming: Option<Field>,
    /// A phone's name, being typed: its id and the field.
    renaming: Option<(String, Field)>,
}

/// A one-line field and its Enter.
struct Field {
    state: Entity<TextareaState>,
    _enter: Subscription,
}

struct Pairing {
    code: String,
    host: String,
    port: u64,
    /// Every address it carries after the first, as lines (`Tailscale 100.x:7843 (name)`).
    more: Vec<String>,
    fingerprint: String,
    until: Instant,
    qr: Qr,
    /// The paired phones when it opened (id, token hash), to tell when one used it.
    before: Vec<(String, String)>,
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

/// `1234 5678`: the code in two halves, easier to type.
pub(super) fn spaced(code: &str) -> String {
    let mid = code.len() / 2;
    if code.is_char_boundary(mid) && code.len() >= 6 { format!("{} {}", &code[..mid], &code[mid..]) } else { code.to_string() }
}

/// What became of an open pairing code, from the status read after it.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Outcome {
    /// A new phone paired with it.
    Paired(String),
    /// A phone that was paired already paired again with it (its token changed).
    Again(String),
    /// The API went off: the code is gone.
    Gone,
}

/// What the pairing should do now that the status says this (`None`: keep it).
pub(super) fn pairing_outcome(before: &[(String, String)], status: &Status) -> Option<Outcome> {
    for d in &status.devices {
        match before.iter().find(|(id, _)| *id == d.id) {
            None => return Some(Outcome::Paired(d.name.clone())),
            Some((_, token)) if *token != d.token_sha256 => return Some(Outcome::Again(d.name.clone())),
            Some(_) => {}
        }
    }
    status.url.is_none().then_some(Outcome::Gone)
}

/// What the box says once a phone used the code: `Paired: OnePlus on Office`.
pub(super) fn paired_line(outcome: &Outcome, network: Option<&str>) -> Option<String> {
    let net = network.unwrap_or("this network");
    match outcome {
        Outcome::Paired(name) => Some(format!("Paired: {name} on {net}")),
        Outcome::Again(name) => Some(format!("Paired again: {name}, now knows {net}")),
        Outcome::Gone => None,
    }
}

/// "This Mac is on" what, and at which address.
pub(super) fn where_line(st: &Status) -> Option<(String, Option<String>)> {
    let net = st.here.label()?;
    Some((net, st.here.lan.as_ref().map(|(h, _)| h.clone())))
}

/// What Tailscale means for the phone, in a line, with the dot's colour and
/// whether to offer to open the app.
pub(super) fn tailscale_line(st: &Status, app: bool) -> (u32, String, bool) {
    let t = &st.tailscale;
    let name = t.dns_name.clone().or_else(|| t.ip.clone()).unwrap_or_else(|| "this Mac".into());
    match (t.installed || app, t.running) {
        (false, _) => (0x9aa3b2, "Tailscale isn’t installed: phones reach the tower on this network only.".into(), false),
        _ if t.mode == "off" => (0x9aa3b2, "Tailscale isn’t used ([lan] tailscale = \"off\"): phones reach the tower on this network only.".into(), false),
        (true, true) if t.listening || st.url.is_none() => (CYAN, format!("Tailscale: reachable anywhere on your tailnet as {name}."), false),
        (true, true) => (
            AMBER,
            format!(
                "Tailscale runs, but the tower isn’t listening on it yet{}. It tries again every 20 s.",
                t.error.as_ref().map(|e| format!(" ({e})")).unwrap_or_default()
            ),
            false,
        ),
        (true, false) => {
            let state = if t.state == "stopped" || t.state.is_empty() { "Off".to_string() } else { capitalised(&t.state) };
            (
                0x9aa3b2,
                format!("Tailscale: {state}: phones reach the tower on this network only. Start Tailscale on this Mac and the phone to reach it from anywhere."),
                app,
            )
        }
    }
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The Tailscale app is here to open.
fn tailscale_app() -> bool {
    std::path::Path::new("/Applications/Tailscale.app").exists()
}

/// `HERDER_TOWER_ORB_PAIR=open` (looking at the card with test data,
/// `tower orb --ui-dir`): the first time the window opens at the Phone
/// card, its pairing box opens as its button would.
pub(super) fn pair_from_start() -> bool {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    std::env::var("HERDER_TOWER_ORB_PAIR").is_ok_and(|v| v == "open") && !DONE.swap(true, std::sync::atomic::Ordering::SeqCst)
}

impl PhoneView {
    /// The network line and the requirement, for the pairing box.
    fn network_label(&self) -> Option<String> {
        self.status.as_ref().and_then(|s| s.here.label())
    }
}

impl Panel {
    /// Reads the status (in the background); every 2 s while Settings shows.
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
            match pairing_outcome(&p.before, &status) {
                Some(Outcome::Gone) => self.phone.pairing = None,
                Some(o) => {
                    self.phone.pairing = None;
                    self.phone.paired = paired_line(&o, status.here.label().as_deref());
                }
                None => {}
            }
        }
        self.phone.status = Some(status);
        // The code ran out with nobody pairing: a fresh one.
        if self.phone.pairing.as_ref().is_some_and(|p| Instant::now() >= p.until) && !self.phone.busy {
            self.phone_pair(cx);
        }
    }

    /// Each second while Settings shows: the countdown ticks, the status is
    /// read every other second.
    pub(super) fn phone_tick(&mut self, cx: &mut Context<Self>) {
        if self.phone.read_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(1900)) {
            self.phone_refresh(cx);
        }
        cx.notify();
    }

    /// Leaving Settings: the code stops showing, what was being typed goes.
    pub(super) fn phone_leave(&mut self) {
        let p = &mut self.phone;
        (p.pairing, p.paired, p.revoking, p.note, p.naming, p.renaming) = (None, None, None, None, None, None);
    }

    /// Opens a pairing code in the card (or a fresh one).
    pub(super) fn phone_pair(&mut self, cx: &mut Context<Self>) {
        self.phone.busy = true;
        self.phone.note = None;
        self.phone.paired = None;
        // The phones on file just before the code opens (the card may not have read them yet).
        let task = cx.background_executor().spawn(async {
            let before: Vec<(String, String)> = crate::daemon::lan::auth::Devices::load_from(&crate::daemon::lan::auth::Devices::path())
                .map(|d| d.devices.into_iter().map(|x| (x.id, x.token_sha256)).collect())
                .unwrap_or_default();
            (before, crate::phone::pair())
        });
        cx.spawn(async move |this, cx| {
            let (before, result) = task.await;
            let _ = this.update(cx, |this, cx| {
                this.phone.busy = false;
                match result.as_ref().ok().and_then(|v| pairing_of(v, before)) {
                    Some(p) => this.phone.pairing = Some(p),
                    None => {
                        this.phone.pairing = None;
                        let why = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
                        this.phone.note = Some((false, if why.is_empty() { "Couldn’t open a pairing code.".into() } else { format!("Couldn’t open a pairing code: {why}") }));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn phone_switch(&mut self, on: bool, cx: &mut Context<Self>) {
        self.phone.busy = true;
        self.phone.note = None;
        self.phone.pairing = None;
        let task = cx
            .background_executor()
            .spawn(async move { if on { crate::phone::enable().map(|v| v.is_some()) } else { crate::phone::disable().map(|_| true) } });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.phone.busy = false;
                this.phone.note = match result {
                    Ok(true) => None,
                    Ok(false) => Some((true, "Enabled: it starts with the tower.".into())),
                    Err(e) => Some((false, format!("{e:#}"))),
                };
                this.phone.read_at = None;
                this.phone_refresh(cx);
            });
        })
        .detach();
    }

    /// Where a phone hears the tower's replies: on the phone, or on this Mac.
    fn phone_route(&mut self, id: String, route: &'static str, cx: &mut Context<Self>) {
        // Shown at once; the next read of the status says how it went.
        if let Some(d) = self.phone.status.as_mut().and_then(|s| s.devices.iter_mut().find(|d| d.id == id)) {
            d.audio_route = route.into();
        }
        cx.notify();
        let task = cx.background_executor().spawn(async move { crate::phone::set_route(&id, route) });
        self.phone_after(task, cx);
    }

    fn phone_revoke(&mut self, id: String, cx: &mut Context<Self>) {
        self.phone.busy = true;
        self.phone.revoking = None;
        let task = cx.background_executor().spawn(async move { crate::phone::revoke(&id).map(|_| ()) });
        self.phone_after(task, cx);
    }

    /// After a change: its error shows in the card, and the status is read again.
    fn phone_after(&mut self, task: gpui::Task<anyhow::Result<()>>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx: &mut AsyncApp| {
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

    /// A one-line field holding `text`, with the caret at its end; Enter runs `enter`.
    fn phone_field(
        &mut self,
        text: &str,
        hint: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        enter: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Field {
        let state = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 1).submit_on_enter(true).placeholder(hint));
        let text = text.to_string();
        state.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            let end = s.text().offset_to_position(s.text().len());
            s.set_cursor_position(end, window, cx);
            s.focus(window, cx);
        });
        let _enter = cx.subscribe_in(&state, window, move |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = ev {
                enter(this, cx);
            }
        });
        Field { state, _enter }
    }

    /// "Name this network": the field, in place of the line.
    fn phone_name_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = self.phone.status.as_ref().and_then(|s| s.here.name.clone()).unwrap_or_default();
        let f = self.phone_field(&now, "Office, Home…", window, cx, |this, cx| this.phone_name_save(cx));
        self.phone.renaming = None;
        self.phone.naming = Some(f);
        cx.notify();
    }

    /// Names the network the Mac is on (an empty name forgets it).
    fn phone_name_save(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.phone.naming.take() else { return };
        let name = f.state.read(cx).value().trim().to_string();
        // Shown at once; the next read says what the tower has.
        if let Some(st) = self.phone.status.as_mut() {
            st.here.name = Some(name.clone()).filter(|n| !n.is_empty());
        }
        let task = cx.background_executor().spawn(async move { crate::phone::name_network(&name).map(|_| ()) });
        self.phone_after(task, cx);
        cx.notify();
    }

    /// "Rename" on a phone's row: the field, in place of its name.
    fn phone_rename_start(&mut self, id: String, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let f = self.phone_field(&name, "A name for this phone", window, cx, |this, cx| this.phone_rename_save(cx));
        self.phone.naming = None;
        self.phone.renaming = Some((id, f));
        cx.notify();
    }

    fn phone_rename_save(&mut self, cx: &mut Context<Self>) {
        let Some((id, f)) = self.phone.renaming.take() else { return };
        let name = f.state.read(cx).value().trim().to_string();
        if name.is_empty() {
            return cx.notify();
        }
        if let Some(d) = self.phone.status.as_mut().and_then(|s| s.devices.iter_mut().find(|d| d.id == id)) {
            d.name = name.clone();
        }
        let task = cx.background_executor().spawn(async move { crate::phone::rename(&id, &name).map(|_| ()) });
        self.phone_after(task, cx);
        cx.notify();
    }

    /// "Phone: pair again on this network", faint, under the status of both
    /// windows while the tower says it (`state.phone_hint`): the phones in
    /// use can't reach the tower on this network, and Tailscale isn't
    /// running. A click opens Settings at the Phone card. Never spoken.
    pub(super) fn phone_hint(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let hint = self.tower.read(cx).s("phone_hint").as_str()?.to_string();
        let g = glass();
        Some(
            div()
                .id("phone-hint")
                .flex_none()
                .cursor_pointer()
                .text_size(px(12.0))
                .line_height(px(16.0))
                .text_color(g.faint)
                .hover(|s| s.text_color(g.text))
                .child(hint)
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.full.is_some() {
                        this.nav_to(super::super::nav::Scope::Phone, cx);
                    } else {
                        this.show_full(super::super::nav::Scope::Phone, window, cx);
                    }
                }))
                .into_any_element(),
        )
    }

    /// Settings' Phone card.
    pub(super) fn phone_card(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let busy = self.phone.busy;
        let mut items: Vec<AnyElement> = vec![];
        let Some(st) = self.phone.status.clone() else {
            return phone_box(vec![small("Looking…", g.muted)], None);
        };
        let on = st.url.is_some();
        let switch = if on {
            Some(tg("phone-off", "Turn off", false).when(!busy, |el| el.on_click(cx.listener(|this, _, _, cx| this.phone_switch(false, cx)))).into_any_element())
        } else {
            None
        };

        // Where the tower is reachable now: the network, and Tailscale.
        if !on {
            let (line, button) = match (st.voice, st.enabled) {
                (false, true) => ("The phone API is on, and starts with the tower: phones can’t reach it while the tower is off.", false),
                (false, false) => ("The phone API is off, and the tower isn’t running.", true),
                (true, _) => ("The phone API is off: phones can’t reach the tower.", true),
            };
            items.push(text(line, g.muted));
            if button {
                items.push(
                    acts_row()
                        .child(mbtn("phone-on", if busy { "Turning on…" } else { "Turn on" }, true, None).when(!busy, |el| el.on_click(cx.listener(|this, _, _, cx| this.phone_switch(true, cx)))))
                        .child(small("Phones on this Wi-Fi (and on Tailscale) can pair with it then.", g.faint))
                        .into_any_element(),
                );
            }
        } else {
            items.push(self.network_row(&st, cx));
            let (dot, line, open) = tailscale_line(&st, tailscale_app());
            items.push(
                h_flex()
                    .gap(px(9.0))
                    .items_start()
                    .child(div().mt(px(5.0)).size(px(8.0)).flex_none().rounded_full().bg(rgb(dot)))
                    .child(div().flex_1().min_w_0().text_size(px(13.0)).line_height(px(18.0)).text_color(g.muted).child(line))
                    .when(open, |el| {
                        el.child(mbtn("open-tailscale", "Open Tailscale", false, None).on_click(cx.listener(|_, _, _, cx| {
                            cx.background_executor().spawn(async { std::process::Command::new("open").args(["-a", "Tailscale"]).status() }).detach();
                        })))
                    })
                    .into_any_element(),
            );
        }
        if let Some((ok, t)) = &self.phone.note {
            items.push(text(t.clone(), if *ok { rgb(CYAN).into() } else { rgb(ROSE).into() }));
        }

        // Pairing, inline.
        if on {
            items.push(self.pair_box(cx));
        }

        // The paired phones.
        items.push(
            bay_label(&match st.devices.len() {
                0 => "Paired phones".to_string(),
                n => format!("Paired phones · {n}"),
            })
            .px(px(0.0))
            .pt(px(4.0))
            .into_any_element(),
        );
        if st.devices.is_empty() {
            items.push(text("None yet.", g.muted));
        }
        for d in &st.devices {
            items.push(self.device_row(&st, d, cx));
        }
        phone_box(items, switch)
    }

    /// "This Mac is on Office · 10.10.30.116", and naming it.
    fn network_row(&mut self, st: &Status, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        if let Some(f) = &self.phone.naming {
            let range = st.here.range.clone().unwrap_or_default();
            return v_flex()
                .gap(px(6.0))
                .child(
                    h_flex()
                        .gap(px(8.0))
                        .items_center()
                        .child(div().flex_1().min_w_0().child(field(&f.state)))
                        .child(mbtn("network-save", "Save", true, None).on_click(cx.listener(|this, _, _, cx| this.phone_name_save(cx))))
                        .child(mbtn("network-cancel", "Cancel", false, None).on_click(cx.listener(|this, _, _, cx| {
                            this.phone.naming = None;
                            cx.notify();
                        }))),
                )
                .child(small(format!("The name of {}, kept in config.toml. Phones see it too.", network::label(&range, None)), g.faint))
                .into_any_element();
        }
        let Some((net, host)) = where_line(st) else {
            return text("This Mac has no local network address now: phones reach it over Tailscale only.", g.muted);
        };
        let named = st.here.name.is_some();
        h_flex()
            .gap(px(8.0))
            .items_center()
            .flex_wrap()
            .child(
                h_flex()
                    .min_w_0()
                    .gap(px(5.0))
                    .text_size(px(14.0))
                    .line_height(px(19.0))
                    .child(div().flex_none().text_color(g.muted).child("This Mac is on"))
                    .child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(net))
                    .children(host.map(|h| div().flex_none().text_color(g.muted).child(format!("· {h}")))),
            )
            .child(
                div()
                    .id("network-name")
                    .flex_none()
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded_full()
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(rgb(PALE))
                    .hover(|s| s.bg(rgba(SOFT)))
                    .child(if named { "Rename" } else { "Name this network" })
                    .on_click(cx.listener(|this, _, window, cx| this.phone_name_start(window, cx))),
            )
            .into_any_element()
    }

    /// The pairing box: the button, the code with what the network asks, or what pairing did.
    fn pair_box(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let busy = self.phone.busy;
        let net = self.phone.network_label().unwrap_or_else(|| "this network".into());
        if let Some(done) = &self.phone.paired {
            return h_flex()
                .gap(px(10.0))
                .items_center()
                .p(px(12.0))
                .rounded(px(14.0))
                .bg(rgba(CARD))
                .border_1()
                .border_color(rgb(CYAN))
                .child(div().flex_none().text_color(rgb(CYAN)).font_weight(FontWeight::BOLD).child("✓"))
                .child(div().flex_1().min_w_0().text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).child(done.clone()))
                .child(mbtn("pair-another", "Pair another", false, None).on_click(cx.listener(|this, _, _, cx| this.phone_pair(cx))))
                .child(mbtn("pair-done", "Done", false, None).on_click(cx.listener(|this, _, _, cx| {
                    this.phone.paired = None;
                    cx.notify();
                })))
                .into_any_element();
        }
        let Some(p) = &self.phone.pairing else {
            return h_flex()
                .gap(px(10.0))
                .items_center()
                .flex_wrap()
                .child(
                    mbtn("phone-pair", if busy { "Opening…" } else { "Pair a phone" }, true, None)
                        .when(!busy, |el| el.on_click(cx.listener(|this, _, _, cx| this.phone_pair(cx)))),
                )
                .child(div().flex_1().min_w_0().text_size(px(12.0)).line_height(px(16.0)).text_color(g.faint).child(format!(
                    "Shows a code to scan in the Tower app on Android. {}",
                    network::requirement(&net)
                )))
                .into_any_element();
        };
        let left = p.until.saturating_duration_since(Instant::now());
        let ink = rgb(0x1d2330);
        let soft = rgb(0x5b6475);
        let mut addresses = vec![format!("{} · {}", crate::phone::addr(&p.host, p.port), crate::daemon::lan::tls::grouped(&p.fingerprint[..8.min(p.fingerprint.len())]))];
        addresses.extend(p.more.iter().map(|m| format!("or {m}")));
        h_flex()
            .gap(px(16.0))
            .items_center()
            .p(px(12.0))
            .rounded(px(16.0))
            .bg(rgb(0xffffff))
            .text_color(ink)
            .child(qr_element(&p.qr))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.0))
                    .child(div().text_size(px(13.0)).text_color(soft).child("Scan it in the Tower app, or enter"))
                    .child(div().text_size(px(24.0)).line_height(px(30.0)).font_family("Menlo").font_weight(FontWeight::SEMIBOLD).child(spaced(&p.code)))
                    .children(addresses.into_iter().map(|a| div().text_size(px(11.0)).font_family("Menlo").text_color(soft).truncate().child(a)))
                    .child(div().pt(px(6.0)).text_size(px(13.0)).line_height(px(18.0)).font_weight(FontWeight::MEDIUM).child(network::requirement(&net)))
                    .child(
                        h_flex()
                            .gap(px(6.0))
                            .items_center()
                            .pt(px(6.0))
                            .child(div().text_size(px(12.0)).text_color(soft).child(format!("Works once. New code in {}", countdown(left))))
                            .child(
                                div()
                                    .id("phone-new-code")
                                    .p(px(4.0))
                                    .rounded_full()
                                    .cursor_pointer()
                                    .text_color(soft)
                                    .hover(|s| s.bg(rgb(0xeef1f5)))
                                    .child(Icon::new(IconName::RefreshCw).small())
                                    .on_click(cx.listener(|this, _, _, cx| this.phone_pair(cx))),
                            )
                            .child(
                                div()
                                    .id("phone-pair-done")
                                    .px(px(8.0))
                                    .py(px(2.0))
                                    .rounded_full()
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .text_color(soft)
                                    .hover(|s| s.bg(rgb(0xeef1f5)))
                                    .child("Close")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.phone.pairing = None;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// A paired phone: its name, how it is connected or when it was last,
    /// what it knows, whether it can reach the tower here, its replies, Remove.
    fn device_row(&mut self, st: &Status, d: &crate::daemon::lan::auth::Device, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let id = d.id.clone();
        let asking = self.phone.revoking.as_deref() == Some(id.as_str());
        let reach = st.reach(d);
        let connected = !st.via(&d.id).is_empty();
        let knows = network::knows_line(&d.networks);
        let name = if d.name.is_empty() { d.id.clone() } else { d.name.clone() };
        // Its name, or the field renaming it.
        let head = match self.phone.renaming.as_ref().filter(|(rid, _)| *rid == d.id) {
            Some((_, f)) => h_flex()
                .flex_1()
                .min_w_0()
                .gap(px(8.0))
                .items_center()
                .child(div().flex_1().min_w_0().child(field(&f.state)))
                .child(mbtn(SharedString::from(format!("rename-save-{id}")), "Save", true, None).on_click(cx.listener(|this, _, _, cx| this.phone_rename_save(cx))))
                .child(mbtn(SharedString::from(format!("rename-cancel-{id}")), "Cancel", false, None).on_click(cx.listener(|this, _, _, cx| {
                    this.phone.renaming = None;
                    cx.notify();
                }))),
            None => {
                let (rid, rname) = (id.clone(), name.clone());
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.0))
                    .items_center()
                    .child(div().min_w_0().truncate().text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).child(name.clone()))
                    .when(connected, |el| el.child(div().flex_none().size(px(7.0)).rounded_full().bg(rgb(CYAN))))
                    .child(
                        div()
                            .id(SharedString::from(format!("rename-{id}")))
                            .flex_none()
                            .px(px(6.0))
                            .py(px(1.0))
                            .rounded_full()
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .text_color(g.faint)
                            .hover(|s| s.bg(rgba(SOFT)).text_color(g.text))
                            .child("Rename")
                            .on_click(cx.listener(move |this, _, window, cx| this.phone_rename_start(rid.clone(), rname.clone(), window, cx))),
                    )
            }
        };
        // Replies: Phone | Mac.
        let current = route_of(&d.audio_route);
        let choice = |route: &'static str, label: &'static str| {
            let id = d.id.clone();
            let on = current == route;
            div()
                .id(SharedString::from(format!("route-{}-{route}", d.id)))
                .px(px(10.0))
                .py(px(2.0))
                .rounded_full()
                .text_size(px(12.0))
                .when(on, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)).font_weight(FontWeight::SEMIBOLD))
                .when(!on, |el| {
                    el.text_color(g.muted).cursor_pointer().hover(|s| s.bg(rgba(SOFT))).on_click(cx.listener(move |this, _, _, cx| this.phone_route(id.clone(), route, cx)))
                })
                .child(label)
        };
        let remove = div()
            .id(SharedString::from(format!("revoke-{}", d.id)))
            .flex_none()
            .px(px(10.0))
            .py(px(3.0))
            .rounded_full()
            .cursor_pointer()
            .text_size(px(12.0))
            .when(asking, |el| el.bg(rgb(ROSE)).text_color(rgb(NAVY)).font_weight(FontWeight::SEMIBOLD))
            .when(!asking, |el| el.text_color(g.faint).hover(|s| s.bg(rgba(SOFT)).text_color(g.text)))
            .child(if asking { "Remove it?" } else { "Remove" })
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.phone.revoking.as_deref() == Some(id.as_str()) {
                    this.phone_revoke(id.clone(), cx);
                } else {
                    this.phone.revoking = Some(id.clone());
                }
                cx.notify();
            }));
        // Can it reach the tower here?
        let verdict = match reach {
            Reach::Off => None,
            r if r.ok() => Some(div().text_size(px(12.0)).line_height(px(16.0)).text_color(rgb(CYAN)).child(format!("✓ {}", r.line())).into_any_element()),
            Reach::Elsewhere => Some(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .flex_wrap()
                    .child(div().min_w_0().text_size(px(12.0)).line_height(px(16.0)).text_color(rgb(AMBER)).child(reach.line()))
                    .child(
                        mbtn(SharedString::from(format!("pair-here-{}", d.id)), "Pair it here", false, None)
                            .py(px(3.0))
                            .text_size(px(12.0))
                            .on_click(cx.listener(|this, _, _, cx| this.phone_pair(cx))),
                    )
                    .into_any_element(),
            ),
            r => Some(small(r.line(), g.faint)),
        };
        v_flex()
            .w_full()
            .gap(px(5.0))
            .px(px(12.0))
            .py(px(10.0))
            .rounded(px(14.0))
            .bg(rgba(CARD))
            .border_1()
            .border_color(rgba(LINE))
            .child(h_flex().gap(px(10.0)).items_center().child(glyph(g.muted)).child(head))
            .child(
                v_flex()
                    .pl(px(21.0))
                    .gap(px(3.0))
                    .child(small(st.seen_line(d), g.muted))
                    .child(small(if knows.is_empty() { "Knows no network yet".to_string() } else { format!("Knows {knows}") }, g.faint))
                    .children(verdict)
                    .child(
                        h_flex()
                            .pt(px(4.0))
                            .gap(px(8.0))
                            .items_center()
                            .child(div().text_size(px(12.0)).text_color(g.faint).child("Replies"))
                            .child(
                                h_flex()
                                    .p(px(2.0))
                                    .rounded_full()
                                    .bg(rgba(SOFT))
                                    .border_1()
                                    .border_color(rgba(LINE))
                                    .child(choice("phone", "Phone"))
                                    .child(choice("mac", "Mac")),
                            )
                            .child(div().flex_1())
                            .child(remove),
                    ),
            )
            .into_any_element()
    }
}

/// The card itself: "Phone", the switch beside it, and what is in it.
fn phone_box(items: Vec<AnyElement>, switch: Option<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .min_w_0()
        .p(px(14.0))
        .gap(px(10.0))
        .rounded(px(16.0))
        .bg(rgba(SOFT))
        .child(
            h_flex()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_1().text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).child("Phone"))
                .children(switch),
        )
        .children(items)
        .into_any_element()
}

/// A line of the card.
fn text(t: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div().text_size(px(13.0)).line_height(px(18.0)).text_color(color).child(t.into()).into_any_element()
}

/// A small line of the card.
fn small(t: impl Into<SharedString>, color: gpui::Hsla) -> AnyElement {
    div().min_w_0().text_size(px(12.0)).line_height(px(16.0)).text_color(color).child(t.into()).into_any_element()
}

/// Where a phone hears replies: `mac`, else the phone (the default, and what
/// an unknown value means).
pub(super) fn route_of(route: &str) -> &'static str {
    if route == "mac" { "mac" } else { "phone" }
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

/// The daemon's answer to `pair`, ready to show.
fn pairing_of(v: &Value, before: Vec<(String, String)>) -> Option<Pairing> {
    let link = v["link"].as_str()?;
    Some(Pairing {
        code: v["code"].as_str()?.to_string(),
        host: v["host"].as_str().unwrap_or("").to_string(),
        port: v["port"].as_u64().unwrap_or(0),
        more: v["hosts"].as_array().map(|a| a.iter().skip(1).filter_map(crate::phone::Host::of).map(|h| h.line()).collect()).unwrap_or_default(),
        fingerprint: v["fingerprint"].as_str().unwrap_or("").to_string(),
        until: Instant::now() + Duration::from_secs(v["expires_in_s"].as_u64().unwrap_or(CODE_SECS)),
        qr: Qr::new(link)?,
        before,
    })
}

/// The code, black on white with its quiet zone, each module a whole
/// number of pixels so it stays crisp.
fn qr_element(qr: &Qr) -> impl IntoElement {
    let n = qr.width + 2 * QUIET;
    // About 170 px, whatever the version (a version 7 link: 3 px a module,
    // 6 on a Retina screen).
    let module = (172.0 / n as f32).floor().max(3.0);
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
    use crate::phone::Tailscale;

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
    fn replies_go_to_the_phone_unless_it_says_mac() {
        assert_eq!(route_of("mac"), "mac");
        assert_eq!(route_of("phone"), "phone");
        assert_eq!(route_of(""), "phone");
        assert_eq!(route_of("speaker"), "phone");
    }

    #[test]
    fn the_code_counts_down_and_reads_easily() {
        assert_eq!(countdown(Duration::from_secs(300)), "5:00");
        assert_eq!(countdown(Duration::from_millis(247_900)), "4:07");
        assert_eq!(countdown(Duration::ZERO), "0:00");
        assert_eq!(spaced("12345678"), "1234 5678");
    }

    fn dev(id: &str, token: &str) -> Device {
        Device {
            id: id.into(),
            name: format!("Pixel {id}"),
            token_sha256: token.into(),
            created_ms: 1,
            last_seen_ms: 0,
            audio_route: "phone".into(),
            install_id: None,
            networks: vec![],
        }
    }

    #[test]
    fn a_phone_that_pairs_or_pairs_again_ends_the_code() {
        let st = |url: Option<&str>, devs: Vec<Device>| Status { enabled: true, voice: true, url: url.map(String::from), devices: devs, ..Default::default() };
        let before = vec![("d1".to_string(), "t1".to_string())];
        assert_eq!(pairing_outcome(&before, &st(Some("https://x:1"), vec![dev("d1", "t1")])), None);
        let new = pairing_outcome(&before, &st(Some("https://x:1"), vec![dev("d1", "t1"), dev("d2", "t2")])).unwrap();
        assert_eq!(new, Outcome::Paired("Pixel d2".into()));
        assert_eq!(paired_line(&new, Some("Office")).as_deref(), Some("Paired: Pixel d2 on Office"));
        // The same phone again (its install id): the same id, a new token.
        let again = pairing_outcome(&before, &st(Some("https://x:1"), vec![dev("d1", "t9")])).unwrap();
        assert_eq!(paired_line(&again, Some("the network 10.10.30.x")).as_deref(), Some("Paired again: Pixel d1, now knows the network 10.10.30.x"));
        assert_eq!(pairing_outcome(&before, &st(None, vec![dev("d1", "t1")])), Some(Outcome::Gone));
        assert_eq!(paired_line(&Outcome::Gone, None), None);
    }

    #[test]
    fn a_pairing_answer_becomes_a_code_to_show() {
        let v = serde_json::json!({ "code": "12345678", "host": "10.0.0.2", "port": 7843, "fingerprint": "ff".repeat(32),
            "link": "herdertower://pair?h=10.0.0.2&p=7843&c=12345678&fp=ff", "expires_in_s": 300 });
        let p = pairing_of(&v, vec![]).unwrap();
        assert_eq!((p.code.as_str(), p.host.as_str(), p.port), ("12345678", "10.0.0.2", 7843));
        assert!(p.until > Instant::now() + Duration::from_secs(299));
        assert!(pairing_of(&serde_json::json!({ "error": "off" }), vec![]).is_none());
        // With the tailnet too: its line under the first address.
        let v = serde_json::json!({ "code": "12345678", "host": "10.0.0.2", "port": 7843, "fingerprint": "ff".repeat(32),
            "hosts": [{"kind": "lan", "host": "10.0.0.2", "port": 7843}, {"kind": "tailscale", "host": "100.68.110.33", "port": 7843, "dns_name": "mac.ts.net"}],
            "link": "herdertower://pair?h=10.0.0.2&p=7843&c=12345678&fp=ff&hosts=lan:10.0.0.2:7843,tailscale:100.68.110.33:7843", "expires_in_s": 300 });
        assert_eq!(pairing_of(&v, vec![]).unwrap().more, vec!["Tailscale 100.68.110.33:7843 (mac.ts.net)".to_string()]);
    }

    #[test]
    fn the_card_says_where_the_tower_is_and_what_tailscale_does() {
        let here = network::Here { range: Some("10.10.30".into()), name: None, lan: Some(("10.10.30.116".into(), 7843)), tailnet: None };
        let ts = |installed, running, listening, mode: &str, state: &str| Tailscale {
            installed,
            running,
            state: state.into(),
            ip: Some("100.68.110.33".into()),
            dns_name: Some("mac.ts.net".into()),
            mode: mode.into(),
            listening,
            error: None,
        };
        let st = |t: Tailscale| Status { url: Some("https://10.10.30.116:7843".into()), tailscale: t, here: here.clone(), ..Default::default() };
        assert_eq!(where_line(&st(Tailscale::default())), Some(("the network 10.10.30.x".to_string(), Some("10.10.30.116".to_string()))));
        let named = Status { here: network::Here { name: Some("Office".into()), ..here.clone() }, ..st(Tailscale::default()) };
        assert_eq!(where_line(&named).unwrap().0, "Office");
        // Running: anywhere on the tailnet, by its name.
        let (dot, line, open) = tailscale_line(&st(ts(true, true, true, "auto", "running")), true);
        assert_eq!((dot, line.as_str(), open), (CYAN, "Tailscale: reachable anywhere on your tailnet as mac.ts.net.", false));
        // Stopped: this network only, and the app to open.
        let (_, line, open) = tailscale_line(&st(ts(true, false, false, "auto", "stopped")), true);
        assert!(line.starts_with("Tailscale: Off: phones reach the tower on this network only. Start Tailscale on this Mac and the phone"), "{line}");
        assert!(open);
        assert!(!tailscale_line(&st(ts(true, false, false, "auto", "stopped")), false).2, "no app to open: no button");
        assert!(tailscale_line(&st(ts(true, false, false, "auto", "needs login")), true).1.starts_with("Tailscale: Needs login:"));
        // Not installed: said, with no button.
        let (_, line, open) = tailscale_line(&st(ts(false, false, false, "auto", "not installed")), false);
        assert_eq!((line.as_str(), open), ("Tailscale isn’t installed: phones reach the tower on this network only.", false));
        assert!(tailscale_line(&st(ts(true, true, false, "off", "running")), true).1.contains("isn’t used"));
        assert_eq!(tailscale_line(&st(ts(true, true, false, "auto", "running")), true).0, AMBER, "running but not listened on");
    }
}
