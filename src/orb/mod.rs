//! `tower orb`: one orb, floating over everything, in its own window (GPUI,
//! Zed's UI framework). It shows what the voice control tower is doing by how
//! it moves; hover for what it heard or said.
//!
//!   click        the panel: type to the tower (with pictures, videos,
//!                links), and its history of jobs and activity
//!   right-click  show the tower's own terminal
//!   ⌥-click      mute the tower, or unmute it (the panel's Mute does the same)
//!   drag         put it anywhere; it stays there next time
//!   ⌘⇧K          show or hide it, from anywhere
//!   ⌘⇧Space      talk, from anywhere (as a click)
//!   ⌘⇧Esc        stop the tower talking, from anywhere
//!
//! It reads the state the daemon writes (`~/.local/state/herder-tower/ui/state.json`)
//! and runs `tower <cmd>`, like the Herdr mod in `mod/tower` it replaces.

mod liquid;
mod mac;
mod panel;
mod render;
mod shots;
mod sim;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    AnyWindowHandle, App, AsyncApp, Bounds, Context, DispatchPhase, Entity, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Render, SharedString, Subscription, Task, Window, WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, canvas, div, font, point, prelude::*, px, rgb, rgba, size,
};
use serde_json::Value;

use crate::config;

/// The window is this square; the orb fills most of it, with room to slosh.
const BOX: f32 = 40.0;
/// A press that moves less than this is a click, not a drag (points).
const DRAG_SLOP: f64 = 3.0;

/// What hovering calls each phase. (How the orb moves in each is in `render::Drive`.)
fn label(phase: &str) -> &'static str {
    match phase {
        "speaking" => "Speaking",
        "announcing" => "Heads up",
        "listening" => "Listening",
        "transcribing" => "Understanding",
        "thinking" => "Thinking",
        "armed" => "Yes?",
        "blocked" => "Waiting for your answer",
        "standby" => "Listening for “tower”",
        "off" => "Off · click to start",
        "idle" => "Ready",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// What the tower is doing (read from state.json).

struct Tower {
    file: PathBuf,
    state: Option<Value>,
    /// What the brain is doing right now (`work.json`, beside state.json).
    work: Option<crate::activity::Work>,
    /// Focus, agendas and what's been read (`spaces.json`, beside state.json).
    spaces: crate::spaces::Spaces,
    alive: bool,
    /// We ran `tower up` and wait for the daemon to say it runs.
    starting: Option<Instant>,
}

impl Tower {
    fn new(ui_dir: &Path) -> Self {
        let mut t = Self { file: ui_dir.join("state.json"), state: None, work: None, spaces: Default::default(), alive: false, starting: None };
        t.read();
        t
    }

    /// Reads state.json again. A half-written file keeps what we had.
    fn read(&mut self) {
        match std::fs::read_to_string(&self.file) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => self.state = Some(v),
                Err(_) => return,
            },
            Err(_) => self.state = None,
        }
        self.work = std::fs::read_to_string(self.file.with_file_name("work.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
        if let Some(dir) = self.file.parent() {
            self.spaces = crate::spaces::load_from(dir);
        }
        self.check_alive();
    }

    /// Running, by its own word and by its process (a daemon that died
    /// without saying so leaves `running: true` behind).
    fn check_alive(&mut self) {
        let says = self.state.as_ref().is_some_and(|s| s["running"] != Value::Bool(false));
        let pid = self.state.as_ref().and_then(|s| s["pid"].as_i64()).unwrap_or(0);
        self.alive = says && (pid <= 0 || pid_alive(pid as i32));
        if self.alive {
            self.starting = None;
        }
    }

    /// Changes focus, agendas or read marks, for the daemon and the brain too.
    fn save_spaces(&mut self) {
        if let Some(dir) = self.file.parent() {
            crate::spaces::save_to(dir, &self.spaces);
        }
    }

    fn s(&self, key: &str) -> &Value {
        self.state.as_ref().map(|s| &s[key]).unwrap_or(&Value::Null)
    }

    fn phase(&self) -> &str {
        if self.starting.is_some() {
            return "thinking";
        }
        if !self.alive {
            return "off";
        }
        self.s("phase").as_str().unwrap_or("idle")
    }

    fn blocked_agents(&self) -> usize {
        self.s("agents").as_array().map(|a| a.iter().filter(|a| a["status"] == "blocked").count()).unwrap_or(0)
    }

    /// The red dot: the brain or any agent is waiting for a person.
    /// Muted (`tower mute`): it says nothing.
    fn muted(&self) -> bool {
        self.alive && self.s("muted").as_bool() == Some(true)
    }

    fn needs_you(&self) -> bool {
        self.alive && (self.phase() == "blocked" || self.blocked_agents() > 0)
    }

    /// What hovering says: the phase, then the latest line either way.
    fn caption(&self) -> String {
        let phase = self.phase();
        let label = if self.starting.is_some() {
            "Starting…".to_string()
        } else {
            let l = label(phase);
            if l.is_empty() { phase.to_string() } else { l.to_string() }
        };
        let mut parts = vec![label];
        if self.alive {
            let ms = |k: &str| self.s(k).as_u64().unwrap_or(0);
            let text = |k: &str| self.s(k).as_str().unwrap_or("").to_string();
            let last = if ms("said_ms") >= ms("heard_ms") {
                text("said")
            } else {
                let heard = text("heard");
                if heard.is_empty() { heard } else { format!("“{heard}”") }
            };
            let error = text("error");
            if !error.is_empty() {
                parts.push(clip(&error, 140));
            } else if !last.is_empty() {
                parts.push(clip(&last, 140));
            }
            if text("talk_key") == "needs-permission" {
                parts.push("Allow Input Monitoring for Herdr Core to talk with Control".into());
            }
            let n = self.blocked_agents();
            if n > 0 {
                parts.push(if n == 1 { "1 agent needs you".into() } else { format!("{n} agents need you") });
            }
        }
        parts.join(" · ")
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// ---------------------------------------------------------------------------
// Running `tower` and `hc`, off the UI thread.

/// Mutes the tower, or unmutes it.
fn toggle_mute(cx: &mut App) {
    tower(&["mute", "toggle"], cx);
}

/// `tower <args>`; a failure becomes a notification.
fn tower(args: &'static [&'static str], cx: &mut App) {
    cx.background_executor()
        .spawn(async move {
            match Command::new(config::tower_bin()).args(args).stdin(Stdio::null()).output() {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    let msg = String::from_utf8_lossy(if o.stderr.is_empty() { &o.stdout } else { &o.stderr }).trim().to_string();
                    notify_error(msg.strip_prefix("tower: ").unwrap_or(&msg));
                    false
                }
                Err(e) => {
                    notify_error(&format!("{}: {e}", config::tower_bin().display()));
                    false
                }
            }
        })
        .detach();
}

/// A macOS notification (the orb has no room for errors).
fn notify_error(msg: &str) {
    eprintln!("[orb] {msg}");
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let script = format!("display notification {} with title \"Tower\"", quote(&clip(msg, 200)));
    let _ = Command::new("osascript").args(["-e", &script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

impl Tower {
    /// Click: talk, or start the tower when it's off.
    fn talk(this: &Entity<Self>, cx: &mut App) {
        if this.read(cx).alive {
            tower(&["talk"], cx);
        } else {
            Self::start(this, cx);
        }
    }

    /// `tower up` waits for the models: run it in a Herdr terminal so it may
    /// take as long as a first download needs (and macOS asks for the
    /// microphone on Herdr Core's behalf, as for `tower up` by hand).
    fn start(this: &Entity<Self>, cx: &mut App) {
        if this.read(cx).starting.is_some() {
            return;
        }
        this.update(cx, |t, cx| {
            t.starting = Some(Instant::now());
            cx.notify();
        });
        let this = this.downgrade();
        let job = cx.background_executor().spawn(async move {
            let bin = config::tower_bin().display().to_string();
            crate::hc::run(&["run", "--bg", "--no-parent", "--no-notify", "--rm", "--name", "tower-up", "--", &bin, "up"])
        });
        cx.spawn(async move |cx: &mut AsyncApp| {
            let failed = match job.await {
                Ok(o) if o.code == 0 => None,
                Ok(o) => Some(format!("{}{}", o.stderr.trim(), o.stdout.trim())),
                Err(e) => Some(format!("{e:#}")),
            };
            if let Some(msg) = failed {
                notify_error(&msg);
                let _ = this.update(cx, |t, cx| {
                    t.starting = None;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Right-click: the brain's terminal, on screen in Herdr.
    fn show(cx: &mut App) {
        cx.background_executor()
            .spawn(async move {
                let ok = Command::new(config::tower_bin()).arg("show").stdin(Stdio::null()).output();
                match ok {
                    Ok(o) if o.status.success() => {
                        let _ = Command::new("open").args(["-a", "Herdr"]).status();
                    }
                    Ok(o) => {
                        let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
                        notify_error(msg.strip_prefix("tower: ").unwrap_or(&msg));
                    }
                    Err(e) => notify_error(&e.to_string()),
                }
            })
            .detach();
    }
}

// ---------------------------------------------------------------------------
// The orb's window.

struct Press {
    mouse: (f64, f64),
    origin: (f64, f64),
    moved: bool,
}

struct Orb {
    tower: Entity<Tower>,
    win: Option<mac::Win>,
    /// Draws the liquid orb under this view; None if Metal isn't there.
    renderer: Option<render::Renderer>,
    phase: String,
    /// The brain's latest step, to give the orb a small nudge on each new one.
    step_ms: u64,
    press: Option<Press>,
    /// The hover caption's window, and the words it shows.
    caption: Option<(WindowHandle<Caption>, String)>,
    /// The panel, when it's open.
    panel: Option<AnyWindowHandle>,
    /// The pointer is over the orb.
    inside: bool,
    hover: Option<Task<()>>,
    _watch_pointer: Task<()>,
    _subs: Vec<Subscription>,
}

impl Orb {
    fn new(tower: Entity<Tower>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.observe(&tower, |this, _, cx| {
                let phase = this.tower.read(cx).phase().to_string();
                if phase != this.phase {
                    if let Some(r) = &this.renderer {
                        r.phase(&phase);
                    }
                    this.phase = phase;
                }
                // Each real step the brain takes (delegating, reading, ...) nudges the liquid.
                let step = this.tower.read(cx).work.as_ref().map(|w| w.at_ms).unwrap_or(0);
                if step > this.step_ms {
                    if this.step_ms > 0 {
                        if let Some(r) = &this.renderer {
                            r.nudge();
                        }
                    }
                    this.step_ms = step;
                }
                // A caption up with other words: open it again, sized for them.
                if this.caption.as_ref().is_some_and(|(_, text)| *text != this.tower.read(cx).caption()) {
                    this.close_caption(cx);
                    this.open_caption(cx);
                }
                cx.notify();
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                if let Some(r) = &this.renderer {
                    r.set_dark(is_dark(window.appearance()));
                }
                cx.notify();
            }),
        ];
        // Hover, by where the pointer is: AppKit sends no pointer moves to a
        // panel of an app that isn't active, which the orb nearly always is.
        let watch_pointer = cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let alive = this.update(cx, |this, cx| {
                    let Some(win) = this.win else { return };
                    let (x, y) = mac::mouse();
                    let frame = win.frame();
                    if let Some(r) = &this.renderer {
                        r.moved(frame, false);
                    }
                    let inside = frame.contains(x, y);
                    if inside != this.inside {
                        this.inside = inside;
                        this.hovered(inside, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let win = mac::Win::of(window);
        let phase = tower.read(cx).phase().to_string();
        let renderer = win.and_then(|w| render::Renderer::attach(w, is_dark(window.appearance()), &phase));
        if renderer.is_none() {
            eprintln!("[orb] no Metal: the orb can't be drawn");
        }
        Self {
            tower,
            win,
            renderer,
            phase,
            step_ms: 0,
            press: None,
            caption: None,
            panel: None,
            inside: false,
            hover: None,
            _watch_pointer: watch_pointer,
            _subs: subs,
        }
    }

    fn hovered(&mut self, on: bool, cx: &mut Context<Self>) {
        if on && self.press.is_none() {
            self.hover = Some(cx.spawn(async move |this, cx: &mut AsyncApp| {
                cx.background_executor().timer(Duration::from_millis(CAPTION_DELAY_MS)).await;
                let _ = this.update(cx, |this, cx| this.open_caption(cx));
            }));
        } else {
            self.hover = None;
            self.close_caption(cx);
        }
    }

    fn open_caption(&mut self, cx: &mut Context<Self>) {
        let Some(win) = self.win else { return };
        if self.caption.is_some() {
            return;
        }
        let text = self.tower.read(cx).caption();
        let orb = win.frame();
        let Some(screen) = mac::screen_at(orb.x + orb.w / 2.0, orb.y + orb.h / 2.0) else { return };
        let f = caption_frame(&text, orb, screen, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.0), px(0.0)), size(px(f.w as f32), px(f.h as f32))))),
            titlebar: None,
            focus: false,
            show: false,
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };
        let shown = text.clone();
        let Ok(handle) = cx.open_window(options, |window, cx| {
            if let Some(w) = mac::Win::of(window) {
                w.plain(true);
                w.set_origin(f.x, f.y);
                w.order_front();
            }
            cx.new(|cx| Caption::new(shown, window, cx))
        }) else {
            return;
        };
        self.caption = Some((handle, text));
    }

    fn close_caption(&mut self, cx: &mut App) {
        if let Some((h, _)) = self.caption.take() {
            let _ = h.update(cx, |_, window, _| window.remove_window());
        }
    }

    fn mouse_down(&mut self) {
        // The liquid gives where it's pressed.
        if let Some(r) = &self.renderer {
            r.press();
        }
        if let Some(win) = self.win {
            let f = win.frame();
            self.press = Some(Press { mouse: mac::mouse(), origin: (f.x, f.y), moved: false });
        }
    }

    fn mouse_moved(&mut self, cx: &mut Context<Self>) {
        let (Some(p), Some(win)) = (self.press.as_mut(), self.win) else { return };
        let m = mac::mouse();
        let (dx, dy) = (m.0 - p.mouse.0, m.1 - p.mouse.1);
        if !p.moved && dx.hypot(dy) < DRAG_SLOP {
            return;
        }
        if !p.moved {
            if let Some(r) = &self.renderer {
                r.carry(true);
            }
        }
        p.moved = true;
        win.set_origin(p.origin.0 + dx, p.origin.1 + dy);
        // Carried, the liquid lags behind and sloshes.
        if let Some(r) = &self.renderer {
            r.moved(win.frame(), true);
        }
        self.hover = None;
        self.close_caption(cx);
    }

    fn mouse_up(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.press.take() else { return };
        if let Some(r) = &self.renderer {
            r.carry(false);
        }
        if p.moved {
            if let Some(win) = self.win {
                let f = win.frame();
                save_position(f.x, f.y);
            }
        } else {
            self.toggle_panel(cx);
        }
    }

    /// Opens the panel beside the orb (on whichever side has room), or closes it.
    fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(h) = self.panel.take() {
            if h.update(cx, |_, window, _| window.remove_window()).is_ok() {
                mac::give_back_focus();
                return;
            }
        }
        let Some(win) = self.win else { return };
        self.hover = None;
        self.close_caption(cx);
        let orb = win.frame();
        let Some(screen) = mac::screen_at(orb.x + orb.w / 2.0, orb.y + orb.h / 2.0) else { return };
        let (w, h) = (panel::W as f64, panel::H as f64);
        let x = if orb.x - w - 8.0 >= screen.x { orb.x - w - 8.0 } else { (orb.x + orb.w + 8.0).min(screen.x + screen.w - w) };
        let y = (orb.y + orb.h - h).max(screen.y).min(screen.y + screen.h - h);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.0), px(0.0)), size(px(panel::W), px(panel::H))))),
            titlebar: None,
            focus: true,
            show: false,
            // A panel that takes the keyboard without bringing the app forward, like Spotlight.
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            // Glass: macOS blurs what's behind; the panel tints it.
            window_background: WindowBackgroundAppearance::Blurred,
            ..Default::default()
        };
        let tower = self.tower.clone();
        // Typing goes to the panel: bring the orb's app forward while it's open.
        cx.activate(true);
        match gpui_kit::open_window(options, cx, |window, cx| {
            if let Some(w) = mac::Win::of(window) {
                w.set_origin(x, y);
                w.glass(panel::RADIUS as f64);
                w.make_key();
            }
            cx.new(|cx| panel::Panel::new(tower, window, cx))
        }) {
            Ok((handle, _)) => self.panel = Some(handle),
            Err(e) => eprintln!("[orb] opening the panel: {e:#}"),
        }
    }
}

impl Render for Orb {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = is_dark(window.appearance());
        let needs_you = self.tower.read(cx).needs_you();
        let muted = self.tower.read(cx).muted();
        let colors = palette(dark);
        let me = cx.entity();

        div()
            .id("tower-orb")
            .relative()
            .size(px(BOX))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    // ⌥-click: mute or unmute, nothing else.
                    if e.modifiers.alt {
                        cx.stop_propagation();
                        toggle_mute(cx);
                        return;
                    }
                    this.mouse_down()
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    Tower::show(cx);
                }),
            )
            // The orb itself is drawn underneath, by `render`.
            // Muted: a small mark at the bottom left (the red dot keeps the top right).
            .when(muted, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom(px(2.0))
                        .left(px(2.0))
                        .size(px(16.0))
                        .rounded(px(8.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(colors.popover)
                        .border_1()
                        .border_color(colors.border)
                        .text_size(px(9.0))
                        .child("🔇"),
                )
            })
            .when(needs_you, |el| {
                el.child(
                    div()
                        .absolute()
                        .top(px(4.0))
                        .right(px(4.0))
                        .size(px(8.0))
                        .rounded(px(4.0))
                        .bg(colors.blocked)
                        .border_2()
                        .border_color(colors.background),
                )
            })
            // A drag keeps going wherever the pointer goes; the window follows it.
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        let m = me.clone();
                        window.on_mouse_event(move |_: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble {
                                m.update(cx, |this, cx| this.mouse_moved(cx));
                            }
                        });
                        let m = me.clone();
                        window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                                m.update(cx, |this, cx| this.mouse_up(cx));
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

/// Herdr's colours: its dark theme ("graphite"), read from a running Herdr.
/// The light one ("paper") isn't readable from outside Herdr: these are close.
struct Palette {
    /// The red dot.
    blocked: Hsla,
    /// The ring around the dot.
    background: Hsla,
    popover: Hsla,
    foreground: Hsla,
    border: Hsla,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            blocked: rgb(0xf0705f).into(),
            background: rgb(0x17191d).into(),
            popover: rgb(0x25272b).into(),
            foreground: rgb(0xeeeeef).into(),
            border: rgba(0xffffff18).into(),
        }
    } else {
        Palette {
            blocked: rgb(0xe0533f).into(),
            background: rgb(0xffffff).into(),
            popover: rgb(0xffffff).into(),
            foreground: rgb(0x1d1e20).into(),
            border: rgba(0x00000018).into(),
        }
    }
}

fn is_dark(a: WindowAppearance) -> bool {
    matches!(a, WindowAppearance::Dark | WindowAppearance::VibrantDark)
}

// ---------------------------------------------------------------------------
// The hover caption: its own little window, since the orb's is only 58 points.

struct Caption {
    text: String,
    _subs: Vec<Subscription>,
}

/// Herdr's tooltip (gpui-component's): popover colours, 1px border, 6px
/// radius, 2×8 px padding, `text_sm` (14px at Herdr's 16px rem) in the UI font,
/// one line, 12px from what it labels; it shows after half a second.
const CAPTION_DELAY_MS: u64 = 500;
const CAPTION_FONT: f32 = 14.0;
const CAPTION_PAD_X: f32 = 8.0;
const CAPTION_PAD_Y: f32 = 2.0;
const CAPTION_MARGIN: f32 = 12.0;

/// Herdr's UI font when it is installed, else the system's.
fn caption_font(cx: &App) -> SharedString {
    if cx.text_system().all_font_names().iter().any(|n| n == "Geist") { "Geist".into() } else { ".SystemUIFont".into() }
}

/// The caption's window for `text`: as wide as the line (wrapping only past
/// the screen's width), under the orb or above it when there's no room
/// below, kept on the orb's screen.
fn caption_frame(text: &str, orb: mac::Rect, screen: mac::Rect, cx: &App) -> mac::Rect {
    let ts = cx.text_system();
    let id = ts.resolve_font(&font(caption_font(cx)));
    let text_w: f32 =
        text.chars().map(|c| ts.advance(id, px(CAPTION_FONT), c).map(|s| f32::from(s.width)).unwrap_or(7.0)).sum::<f32>().ceil() + 8.0;
    let chrome_w = 2.0 * (CAPTION_PAD_X + 1.0 + CAPTION_MARGIN);
    let max_text_w = (screen.w as f32 - chrome_w).max(100.0);
    let lines = (text_w / max_text_w).ceil().max(1.0);
    let line_h = (CAPTION_FONT * 1.618).ceil();
    let w = (text_w.min(max_text_w) + chrome_w) as f64;
    let h = (lines * line_h + 2.0 * (CAPTION_PAD_Y + 1.0 + CAPTION_MARGIN)) as f64;
    let x = (orb.x + orb.w / 2.0 - w / 2.0).min(screen.x + screen.w - w).max(screen.x);
    let below = orb.y - h;
    let y = if below >= screen.y { below } else { orb.y + orb.h };
    mac::Rect { x, y, w, h }
}

impl Caption {
    fn new(text: String, window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { text, _subs: vec![window.observe_window_appearance(|window, _| window.refresh())] }
    }
}

impl Render for Caption {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = palette(is_dark(window.appearance()));
        div().size_full().p(px(CAPTION_MARGIN)).child(
            div()
                .size_full()
                .flex()
                .items_center()
                .px(px(CAPTION_PAD_X))
                .py(px(CAPTION_PAD_Y))
                .rounded(px(6.0))
                .bg(colors.popover)
                .border_1()
                .border_color(colors.border)
                .shadow_md()
                .font_family(caption_font(cx))
                .text_size(px(CAPTION_FONT))
                .text_color(colors.foreground)
                .child(self.text.clone()),
        )
    }
}

// ---------------------------------------------------------------------------
// Where the orb was, and the app around it.

fn position_file() -> PathBuf {
    config::state_dir().join("orb.json")
}

fn pid_file() -> PathBuf {
    config::state_dir().join("orb.pid")
}

fn save_position(x: f64, y: f64) {
    let _ = std::fs::create_dir_all(config::state_dir());
    let _ = std::fs::write(position_file(), serde_json::json!({ "x": x, "y": y }).to_string());
}

/// The saved spot, if it is still on a screen.
fn saved_position() -> Option<(f64, f64)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(position_file()).ok()?).ok()?;
    let (x, y) = (v["x"].as_f64()?, v["y"].as_f64()?);
    let c = BOX as f64 / 2.0;
    mac::screens().iter().any(|s| s.contains(x + c, y + c)).then_some((x, y))
}

/// Top right of the main screen, under the menu bar.
fn default_position() -> (f64, f64) {
    let s = mac::screens().first().copied().unwrap_or(mac::Rect { x: 0.0, y: 0.0, w: 1440.0, h: 900.0 });
    (s.x + s.w - BOX as f64 - 16.0, s.y + s.h - BOX as f64 - 16.0)
}

fn open_orb(tower: Entity<Tower>, cx: &mut App) -> Option<WindowHandle<Orb>> {
    let (x, y) = saved_position().unwrap_or_else(default_position);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.0), px(0.0)), size(px(BOX), px(BOX))))),
        titlebar: None,
        focus: false,
        show: false,
        // A non-activating panel above other windows, on every Space; it
        // gets the pointer even while another app is active.
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("herder-tower.orb".into()),
        ..Default::default()
    };
    let r = cx.open_window(options, |window, cx| {
        if let Some(w) = mac::Win::of(window) {
            w.plain(false);
            w.set_origin(x, y);
            w.order_front();
        }
        cx.new(|cx| Orb::new(tower, window, cx))
    });
    match r {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("[orb] opening the window: {e:#}");
            None
        }
    }
}

fn close_orb(h: WindowHandle<Orb>, cx: &mut App) {
    let _ = h.update(cx, |orb, window, cx| {
        orb.close_caption(cx);
        window.remove_window();
    });
}

/// `tower orb`. `ui_dir`: read state.json from there instead (for trying it out).
pub fn run(ui_dir: Option<PathBuf>) -> Result<()> {
    let ui_dir = ui_dir.unwrap_or_else(config::ui_dir);
    std::fs::create_dir_all(&ui_dir).with_context(|| format!("creating {}", ui_dir.display()))?;
    if let Some(pid) = running_pid() {
        bail!("the orb is already running (pid {pid})");
    }
    std::fs::create_dir_all(config::state_dir())?;
    std::fs::write(pid_file(), std::process::id().to_string())?;

    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        mac::accessory_app();
        let tower = cx.new(|_| Tower::new(&ui_dir));
        let orb = Rc::new(RefCell::new(open_orb(tower.clone(), cx)));

        // The daemon rewrites one file whenever something changes.
        let (tx, mut changed) = mpsc::unbounded::<()>();
        let watcher = notify::recommended_watcher(move |r: notify::Result<notify::Event>| {
            if r.is_ok() {
                let _ = tx.unbounded_send(());
            }
        })
        .and_then(|mut w| {
            use notify::Watcher;
            w.watch(&ui_dir, notify::RecursiveMode::NonRecursive).map(|_| w)
        });
        if let Err(e) = &watcher {
            eprintln!("[orb] can't watch {}: {e}; checking every second instead", ui_dir.display());
        }
        let polling = watcher.is_err();
        {
            let tower = tower.clone();
            cx.spawn(async move |cx: &mut AsyncApp| {
                let _watcher = watcher;
                loop {
                    if polling {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                    } else if changed.next().await.is_none() {
                        break;
                    }
                    while changed.try_recv().is_ok() {}
                    let _ = tower.update(cx, |t, cx| {
                        t.read();
                        cx.notify();
                    });
                }
            })
            .detach();
        }

        // A daemon that died without saying so; a start that went nowhere.
        {
            let tower = tower.clone();
            cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(15)).await;
                    let starting = tower.update(cx, |t, cx| {
                        let before = (t.alive, t.starting.is_some());
                        t.check_alive();
                        if t.starting.is_some_and(|s| s.elapsed() > Duration::from_secs(600)) {
                            t.starting = None;
                        }
                        if (t.alive, t.starting.is_some()) != before {
                            cx.notify();
                        }
                        t.starting.is_some_and(|s| s.elapsed() > Duration::from_secs(10))
                    });
                    // `tower up` runs in the Herdr terminal `tower-up` (removed when it ends):
                    // gone, and still no daemon, means it failed.
                    if starting {
                        let up = cx.background_executor().spawn(async { crate::hc::is_live("tower-up") }).await;
                        if !up {
                            let _ = tower.update(cx, |t, cx| {
                                t.read();
                                if !t.alive && t.starting.take().is_some() {
                                    notify_error("The tower didn't start. Run `tower up` in a terminal to see why.");
                                }
                                cx.notify();
                            });
                        }
                    }
                }
            })
            .detach();
        }

        // ⌘⇧K, from anywhere: show or hide the orb.
        let (key_tx, mut keys) = mpsc::unbounded::<Key>();
        let hotkeys = register_hotkeys(key_tx);
        cx.spawn(async move |cx: &mut AsyncApp| {
            let _hotkeys = hotkeys;
            while let Some(key) = keys.next().await {
                let _ = cx.update(|cx| match key {
                    Key::Toggle => {
                        let current = orb.borrow_mut().take();
                        match current {
                            Some(h) if h.update(cx, |_, _, _| ()).is_ok() => close_orb(h, cx),
                            _ => *orb.borrow_mut() = open_orb(tower.clone(), cx),
                        }
                    }
                    Key::Talk => Tower::talk(&tower, cx),
                    Key::Hush => self::tower(&["hush"], cx),
                });
            }
        })
        .detach();
    });
    Ok(())
}

/// The orb's keys, from any app.
#[derive(Clone, Copy, Debug)]
enum Key {
    /// ⌘⇧K: show or hide the orb.
    Toggle,
    /// ⌘⇧Space: talk (or start the tower when it's off), as a click does.
    Talk,
    /// ⌘⇧Esc: stop the tower talking.
    Hush,
}

fn register_hotkeys(tx: mpsc::UnboundedSender<Key>) -> Option<global_hotkey::GlobalHotKeyManager> {
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[orb] no global keys: {e}");
            return None;
        }
    };
    let cmd_shift = Some(Modifiers::META | Modifiers::SHIFT);
    let mut keys = vec![];
    for (code, key, name) in [(Code::KeyK, Key::Toggle, "⌘⇧K"), (Code::Space, Key::Talk, "⌘⇧Space"), (Code::Escape, Key::Hush, "⌘⇧Esc")] {
        let hotkey = HotKey::new(cmd_shift, code);
        match manager.register(hotkey) {
            Ok(()) => keys.push((hotkey.id(), key)),
            Err(e) => eprintln!("[orb] {name} is taken: {e}"),
        }
    }
    GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
        if e.state != HotKeyState::Pressed {
            return;
        }
        if let Some((_, key)) = keys.iter().find(|(id, _)| *id == e.id) {
            let _ = tx.unbounded_send(*key);
        }
    }));
    Some(manager)
}

/// The pid of a running orb (not this process).
fn running_pid() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(pid_file()).ok()?.trim().parse().ok()?;
    (pid > 0 && pid as u32 != std::process::id() && pid_alive(pid) && is_orb(pid)).then_some(pid)
}

/// That pid is a `tower orb` (pids get reused).
fn is_orb(pid: i32) -> bool {
    Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(" orb"))
}

/// Starts the orb in the background unless it runs; for `tower up`.
pub fn spawn() -> Result<bool> {
    if running_pid().is_some() {
        return Ok(false);
    }
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(config::state_dir())?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(config::state_dir().join("orb.log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("orb").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // Its own session: it outlives `tower up` and the terminal that ran it.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting the orb")?;
    Ok(true)
}
