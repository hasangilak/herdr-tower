//! `tower orb`: one orb, floating over everything, in its own window (GPUI,
//! Zed's UI framework). It shows what the voice control tower is doing by how
//! it moves, and how many decisions wait for the user as an amber number.
//!
//!   hover        a peek at the panel: what needs you, the spaces, the input;
//!                it goes when the pointer leaves the orb and the panel
//!   click        the panel, pinned: answer what needs you, type to the tower
//!                (with pictures, videos, links); Esc or a click closes it
//!   right-click  show the tower's own terminal
//!   ⌥-click      mute the tower, or unmute it (the panel's Mute does the same)
//!   drag         put it anywhere; it stays there next time
//!   ⌘⇧K          show or hide it, from anywhere
//!   ⌘⇧Space      talk, from anywhere (as a click)
//!   ⌘⇧Esc        stop the tower talking, from anywhere
//!
//! It reads the state the daemon writes (`~/.local/state/herder-tower/ui/state.json`)
//! and runs `tower <cmd>`, like the Herdr mod in `mod/tower` it replaces.
//!
//! The panel's "Full window" opens the tower's window (`open_full`): the
//! same queue with a sidebar of the spaces, each space's own view, the
//! launcher, the record, the screenshots and the settings. Both show one
//! model (`Tower`): a card answered or picked in one is so in the other.
//!
//! `HERDER_TOWER_ORB_PEEK=off`: hovering shows one line (what it heard or
//! said) instead of the panel. To look at them with test data (`tower orb
//! --ui-dir`), without the keyboard being taken: `HERDER_TOWER_ORB_PANEL=show`
//! has the panel up from the start, `HERDER_TOWER_ORB_WINDOW=show` the full
//! window (`show:<view>` at a view, as `nav::Scope::parse` reads it); either
//! with `@<x>,<y>` opens at that spot, in screen points.

mod liquid;
mod mac;
mod model;
mod nav;
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
    AnyWindowHandle, App, AsyncApp, Bounds, Context, DispatchPhase, Entity, FontWeight, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
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
    /// The PRs sessions opened (`prs.json`, beside state.json): the chat's chips.
    prs: std::collections::BTreeMap<String, crate::prs::Pr>,
    /// The queue as the user left it (the card picked, the receipts showing):
    /// the same for every view of it.
    deck: model::Deck,
    /// What was done to the queue from a window (an answer on its way, a
    /// merge under way), until the state says so too.
    acts: panel::Acts,
    /// Follow and mute as the user just set them, until the state agrees.
    marks: std::collections::HashMap<String, model::Marks>,
    /// The spaces the user follows, and those muted (`follow` and `mute` in
    /// `spaces.json`).
    follows: Vec<String>,
    mutes: Vec<String>,
    /// Where the full window is, and the window while it is open.
    nav: nav::Nav,
    full: Option<AnyWindowHandle>,
    /// The floating panel while it is open (a peek or pinned): it closes
    /// when the user opens the full window, which takes its place.
    floating: Option<AnyWindowHandle>,
    /// The orb opens the full window rather than the floating panel: the
    /// user switched to it last ("Full window"), and not back ("Floating").
    full_mode: bool,
    /// The orb's window: the full window's "Floating" opens the panel through it.
    orb: Option<WindowHandle<Orb>>,
    /// The floating panel while it is pinned: the keyboard is its again when
    /// the full window closes.
    pinned: Option<AnyWindowHandle>,
    /// The window of the two that last had the keyboard: what is dictated is
    /// written there.
    keyed: Option<AnyWindowHandle>,
    /// Herdr's spaces as a panel last saw them, so the next one opens with them.
    live: Vec<crate::spaces::Live>,
    /// Decisions that are the user's, outside muted spaces, as last read.
    count: usize,
    alive: bool,
    /// We ran `tower up` and wait for the daemon to say it runs.
    starting: Option<Instant>,
}

impl Tower {
    fn new(ui_dir: &Path) -> Self {
        let mut t = Self {
            file: ui_dir.join("state.json"),
            state: None,
            work: None,
            spaces: Default::default(),
            prs: Default::default(),
            deck: Default::default(),
            acts: Default::default(),
            marks: Default::default(),
            follows: vec![],
            mutes: vec![],
            nav: Default::default(),
            full: None,
            floating: None,
            full_mode: saved_full_mode(),
            orb: None,
            pinned: None,
            keyed: None,
            live: vec![],
            count: 0,
            alive: false,
            starting: None,
        };
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
            self.prs = crate::prs::load_from(dir);
            (self.follows, self.mutes) = (self.spaces.follow.iter().cloned().collect(), self.spaces.mute.iter().cloned().collect());
        }
        self.check_alive();
        // The queue as it is now: its count for the orb, and a receipt for
        // what left it since the last look.
        if let Some(state) = &self.state {
            let now = crate::history::now_ms();
            let all = model::decisions(state, &self.prs, &|_| String::new(), now);
            model::settle_marks(&mut self.marks, state, &all, &self.follows, &self.mutes);
            self.count = model::count(&all, &model::muted_spaces(&all, state, &self.mutes, &self.marks));
            self.deck.sync(&all, now);
        } else {
            self.count = 0;
        }
    }

    /// A space's switch was set here: it counts at once (a muted space's
    /// decisions leave the orb's number).
    fn set_mark(&mut self, space: &str, change: impl FnOnce(&mut model::Marks)) {
        let mark = self.marks.entry(space.to_string()).or_default();
        change(mark);
        if *mark == model::Marks::default() {
            self.marks.remove(space);
        }
        if let Some(state) = &self.state {
            let all = model::decisions(state, &self.prs, &|_| String::new(), crate::history::now_ms());
            self.count = model::count(&all, &model::muted_spaces(&all, state, &self.mutes, &self.marks));
        }
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

    /// Muted (`tower mute`): it says nothing.
    fn muted(&self) -> bool {
        self.alive && self.s("muted").as_bool() == Some(true)
    }

    /// Everything said plays on this Mac, never on a phone (`tower mac-voice`).
    fn voice_on_mac(&self) -> bool {
        self.s("voice_on_mac").as_bool() == Some(true)
    }

    /// The orb's amber number: decisions that are the user's, outside muted
    /// spaces (`model::count`). None while the tower is off.
    fn yours(&self) -> usize {
        if self.alive { self.count } else { 0 }
    }

    /// What hovering says: the phase, the latest line either way, and how
    /// many agents wait.
    fn caption(&self) -> String {
        let mut parts = self.glance();
        let n = if self.alive { self.blocked_agents() } else { 0 };
        if n > 0 {
            parts.push(if n == 1 { "1 agent needs you".into() } else { format!("{n} agents need you") });
        }
        parts.join(" · ")
    }

    /// The phase, then the latest line either way (what it heard or said, or
    /// what went wrong), and the talk key's permission if it lacks it.
    fn glance(&self) -> Vec<String> {
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
        }
        parts
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

/// Everything said plays on this Mac, or the phones have it as routed (`tower mac-voice`).
fn toggle_mac_voice(cx: &mut App) {
    tower(&["mac-voice", "toggle"], cx);
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

/// The panel's window while it is open.
struct PanelWin {
    handle: AnyWindowHandle,
    view: Entity<panel::Panel>,
    /// Where it is on screen.
    frame: mac::Rect,
    /// Since when the pointer has been off the orb and a peek.
    away: Option<Instant>,
    /// Shown from the start to be looked at (`show_from_start`): it stays.
    held: bool,
}

/// A peek stays while the pointer is this near it (points: the gap to the
/// orb and a little more), and this long after it leaves (ms).
const PEEK_REACH: f64 = 12.0;
const PEEK_LINGER_MS: u64 = 300;

/// Hovering peeks at the panel; `HERDER_TOWER_ORB_PEEK=off` keeps the
/// one-line caption instead.
fn peek_enabled() -> bool {
    std::env::var("HERDER_TOWER_ORB_PEEK").map(|v| v != "off").unwrap_or(true)
}

/// `HERDER_TOWER_ORB_PANEL=show[@<x>,<y>]`: the panel is up from the start,
/// as a peek that stays (it takes no keyboard and no pointer is needed), to
/// look at it with test data (`tower orb --ui-dir`); with a spot, there
/// (in screen points) instead of beside the orb.
fn show_from_start() -> Option<Option<(f64, f64)>> {
    let v = std::env::var("HERDER_TOWER_ORB_PANEL").ok()?;
    match v.split_once('@') {
        None if v == "show" => Some(None),
        Some(("show", at)) => Some(at.split_once(',').and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)))),
        _ => None,
    }
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
    /// The panel, when it's open: a peek (hover) or pinned (a click).
    panel: Option<PanelWin>,
    /// The pointer is over the orb.
    inside: bool,
    hover: Option<Task<()>>,
    /// A click on the orb just closed the panel: hovering doesn't bring it
    /// back until the pointer has left the orb.
    rest: bool,
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
                    this.watch_peek(x, y, frame, cx);
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
            rest: false,
            _watch_pointer: watch_pointer,
            _subs: subs,
        }
    }

    fn hovered(&mut self, on: bool, cx: &mut Context<Self>) {
        if !on {
            self.rest = false;
        }
        if on && self.press.is_none() {
            self.hover = Some(cx.spawn(async move |this, cx: &mut AsyncApp| {
                cx.background_executor().timer(Duration::from_millis(CAPTION_DELAY_MS)).await;
                let _ = this.update(cx, |this, cx| if peek_enabled() { this.peek(cx) } else { this.open_caption(cx) });
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
        // A peek belongs where the orb was.
        if self.peeking(cx) {
            self.close_panel(false, cx);
        }
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

    /// Whether the panel is open (one closed from inside, with Esc, is forgotten).
    fn panel_open(&mut self, cx: &mut App) -> bool {
        if self.panel.as_ref().is_some_and(|p| p.handle.update(cx, |_, _, _| ()).is_err()) {
            self.panel = None;
        }
        self.panel.is_some()
    }

    /// The panel is open as a peek.
    fn peeking(&mut self, cx: &mut App) -> bool {
        self.panel_open(cx) && self.panel.as_ref().is_some_and(|p| p.view.read(cx).is_peek())
    }

    fn close_panel(&mut self, give_back: bool, cx: &mut App) {
        if let Some(p) = &self.panel {
            let h = p.handle;
            self.tower.update(cx, |t, _| t.floating = t.floating.filter(|f| *f != h));
        }
        if let Some(p) = self.panel.take()
            && p.handle.update(cx, |_, window, _| window.remove_window()).is_ok()
            && give_back
        {
            mac::give_back_focus();
        }
    }

    /// Hovering: the panel as a peek, unless it is open already.
    fn peek(&mut self, cx: &mut Context<Self>) {
        // In full mode (open or not), the full window has taken the floating panel's place.
        if self.rest || self.tower.read(cx).full_mode || full_open(&self.tower, cx) {
            return;
        }
        if self.press.is_none() && !self.panel_open(cx) {
            self.open_panel(true, cx);
        }
    }

    /// A peek goes once the pointer has left both the orb and the panel. One
    /// that was clicked is pinned (`Panel::pin`) and stays.
    fn watch_peek(&mut self, x: f64, y: f64, orb: mac::Rect, cx: &mut Context<Self>) {
        if !self.peeking(cx) {
            if let Some(p) = self.panel.as_mut() {
                p.away = None;
            }
            return;
        }
        let Some(p) = self.panel.as_mut() else { return };
        if p.held || orb.contains(x, y) || p.frame.grown(PEEK_REACH).contains(x, y) {
            p.away = None;
            return;
        }
        if p.away.get_or_insert_with(Instant::now).elapsed() >= Duration::from_millis(PEEK_LINGER_MS) {
            self.close_panel(false, cx);
        }
    }

    /// A click: closes the panel when it shows (a peek or pinned; a click
    /// inside a peek is what pins it), else opens it pinned. In full mode it
    /// closes the full window when it is open and opens it when it isn't;
    /// the floating panel stays away until the user switches back to it
    /// ("Floating" in the full window).
    fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        // What hovering was about to show is not shown now.
        self.hover = None;
        if full_open(&self.tower, cx) {
            close_full(&self.tower, cx);
            mac::give_back_focus();
            self.rest = true;
            return;
        }
        if self.tower.read(cx).full_mode {
            open_full(&self.tower, None, None, None, true, true, cx);
            return;
        }
        if self.panel_open(cx) {
            // A peek never had the keyboard; a pinned panel hands it back.
            let pinned = !self.peeking(cx);
            self.close_panel(pinned, cx);
            self.rest = true;
            return;
        }
        self.open_panel(false, cx);
    }

    /// Opens the panel beside the orb (on whichever side has room). A peek
    /// shows without taking the keyboard from the app in front.
    fn open_panel(&mut self, peek: bool, cx: &mut Context<Self>) {
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
            focus: !peek,
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
        if !peek {
            cx.activate(true);
        }
        match gpui_kit::open_window(options, cx, |window, cx| {
            if let Some(w) = mac::Win::of(window) {
                w.set_origin(x, y);
                w.glass(panel::RADIUS as f64);
                if peek { w.order_front() } else { w.make_key() }
            }
            cx.new(|cx| panel::Panel::new(tower, peek, window, cx))
        }) {
            Ok((handle, view)) => {
                self.tower.update(cx, |t, _| t.floating = Some(handle));
                self.panel = Some(PanelWin { handle, view, frame: mac::Rect { x, y, w, h }, away: None, held: false });
            }
            Err(e) => eprintln!("[orb] opening the panel: {e:#}"),
        }
    }
}

impl Render for Orb {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dark = is_dark(window.appearance());
        let yours = self.tower.read(cx).yours();
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
            // Muted: a small mark at the bottom left (the count keeps the top right).
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
            // What is the user's to decide: an amber number, and nothing when there is none.
            .when(yours > 0, |el| {
                el.child(
                    div()
                        .absolute()
                        .top(px(0.0))
                        .right(px(0.0))
                        .min_w(px(16.0))
                        .h(px(16.0))
                        .px(px(4.0))
                        .rounded(px(8.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(colors.yours)
                        .text_color(colors.on_yours)
                        .text_size(px(10.5))
                        .line_height(px(16.0))
                        .font_weight(FontWeight::BOLD)
                        .shadow_sm()
                        .child(if yours > 9 { "9+".to_string() } else { yours.to_string() }),
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

// ---------------------------------------------------------------------------
// The full window.

/// `HERDER_TOWER_ORB_WINDOW=show[:<view>][@<x>,<y>]`: the full window is up
/// from the start (it takes no keyboard), at that view and that spot, to
/// look at it with test data (`tower orb --ui-dir`). `show:new=<goal>` opens
/// the launcher with that goal typed in (`from_start_goal`).
fn window_from_start() -> Option<(nav::Scope, Option<String>, Option<(f64, f64)>)> {
    let v = std::env::var("HERDER_TOWER_ORB_WINDOW").ok()?;
    let (word, at) = match v.rsplit_once('@') {
        Some((w, at)) => (w, at.split_once(',').and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)))),
        None => (v.as_str(), None),
    };
    let view = match word.split_once(':') {
        Some(("show", view)) => view,
        None if word == "show" => "",
        _ => return None,
    };
    let (scope, filter) = nav::Scope::parse(view);
    Some((scope, filter, at))
}

/// `HERDER_TOWER_ORB_ANSWER=<decision id>:<n>[:rule]`: a moment after the
/// start, the full window answers that card with its option `n` (from 0)
/// as a click on it would, and with `:rule` takes its receipt's "Make it a
/// rule" two seconds later. For looking at what an answer leaves behind
/// without the pointer (with `HERDER_TOWER_ORB_WINDOW=show`).
fn answer_from_start() -> Option<(String, usize, bool)> {
    let v = std::env::var("HERDER_TOWER_ORB_ANSWER").ok()?;
    let mut parts = v.split(':');
    let (id, n) = (parts.next()?.trim().to_string(), parts.next()?.trim().parse().ok()?);
    (!id.is_empty()).then(|| (id, n, parts.next() == Some("rule")))
}

/// The goal `HERDER_TOWER_ORB_WINDOW=show:new=<goal>[@<x>,<y>]` names.
fn from_start_goal() -> Option<String> {
    let v = std::env::var("HERDER_TOWER_ORB_WINDOW").ok()?;
    let word = v.rsplit_once('@').map(|(w, _)| w.to_string()).unwrap_or(v);
    word.strip_prefix("show:new=").map(str::trim).filter(|g| !g.is_empty()).map(String::from)
}

/// Whether the full window is open (one closed from inside is forgotten).
fn full_open(tower: &Entity<Tower>, cx: &mut App) -> bool {
    let Some(h) = tower.read(cx).full else { return false };
    let open = cx.windows().contains(&h);
    if !open {
        tower.update(cx, |t, _| t.full = None);
    }
    open
}

/// Shows the full window at `to` (where it was, with none): opens it beside
/// `anchor` (the floating panel) or in the middle of the screen, or brings it
/// forward if it is open. `spot`: exactly there instead. `focus`: it takes
/// the keyboard, as a click on "Full window" means it to. `replace`: it
/// takes the floating panel's place, which closes (the user opened it).
fn open_full(tower: &Entity<Tower>, to: Option<nav::Scope>, anchor: Option<mac::Rect>, spot: Option<(f64, f64)>, focus: bool, replace: bool, cx: &mut App) {
    if let Some(to) = to {
        tower.update(cx, |t, cx| {
            t.nav.go(to);
            cx.notify();
        });
    }
    // The user chose it (a click on Full window, a space chip, the orb in full mode): full mode.
    if replace {
        set_full_mode(tower, true, cx);
    }
    if full_open(tower, cx) {
        // From another window's click: forward, with the keyboard.
        if let Some(h) = tower.read(cx).full
            && focus
        {
            let _ = h.update(cx, |_, window, _| {
                if let Some(w) = mac::Win::of(window) {
                    w.make_key();
                }
            });
        }
        return;
    }
    let (at_x, at_y) = anchor.map(|a| (a.x + a.w / 2.0, a.y + a.h / 2.0)).unwrap_or_else(mac::mouse);
    let Some(screen) = mac::screen_at(at_x, at_y) else {
        eprintln!("[orb] no screen to open the full window on");
        return;
    };
    // The size the user last gave it, never smaller than its least, never larger than the screen.
    let saved = saved_window();
    let (w, h) = nav::full_size(saved.map(|s| (s.2, s.3)), screen, (panel::FULL_W as f64, panel::FULL_H as f64), (panel::FULL_MIN_W as f64, panel::FULL_MIN_H as f64));
    // Where the user last put it, if that is still on a screen; else in the
    // floating panel's place when it takes it (the user opened it), or beside it.
    let (x, y) = spot.or_else(|| saved.map(|s| nav::keep_on(&mac::screens(), s.0, s.1, w, h))).unwrap_or_else(|| match anchor {
        Some(a) if replace => nav::place_over(a, screen, w, h),
        _ => nav::place(anchor, screen, w, h),
    });
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(0.0), px(0.0)), size(px(w as f32), px(h as f32))))),
        titlebar: None,
        focus,
        show: false,
        // As the panel: it takes the keyboard without bringing the app forward.
        kind: WindowKind::PopUp,
        // Moved by its header and the sidebar's top (`Panel::drag_moved` hands the move to macOS).
        is_movable: true,
        // Its edges resize it (`mac::Win::resizable`: the flag alone does nothing without a title bar).
        is_resizable: true,
        window_min_size: Some(size(px(panel::FULL_MIN_W), px(panel::FULL_MIN_H))),
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    };
    if focus {
        cx.activate(true);
    }
    let t = tower.clone();
    match gpui_kit::open_window(options, cx, |window, cx| {
        if let Some(w) = mac::Win::of(window) {
            w.resizable();
            w.set_origin(x, y);
            w.glass(panel::RADIUS as f64);
            if focus { w.make_key() } else { w.order_front() }
            // (AppKit keeps a window it orders in on the screen: a spot off it is set after.)
            if spot.is_some() {
                w.set_origin(x, y);
            }
        }
        cx.new(|cx| panel::Panel::full(t, focus, window, cx))
    }) {
        Ok((handle, view)) => {
            // Opened by the user: it takes the floating panel's place, which goes.
            let floating = tower.update(cx, |t, cx| {
                t.full = Some(handle);
                cx.notify();
                let f = if replace { t.floating.take() } else { None };
                if let Some(f) = f {
                    t.pinned = t.pinned.filter(|h| *h != f);
                    t.keyed = Some(handle);
                }
                f
            });
            // After the click that opened it is handled: that click runs inside
            // the floating panel's own window, which can't be closed from there.
            if let Some(f) = floating {
                cx.defer(move |cx| {
                    if f.update(cx, |_, window, _| window.remove_window()).is_err() {
                        eprintln!("[orb] the floating panel was already gone");
                    }
                });
            }
            // `HERDER_TOWER_ORB_ANSWER`: a card is answered as a click would, once, to look at what follows.
            static ANSWERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if let Some((id, choice, rule)) = answer_from_start().filter(|_| !focus && !ANSWERED.swap(true, std::sync::atomic::Ordering::SeqCst)) {
                cx.spawn(async move |cx: &mut AsyncApp| {
                    cx.background_executor().timer(Duration::from_millis(2500)).await;
                    let _ = view.update(cx, |p, cx| p.answer_from_start(&id, choice, rule, cx));
                })
                .detach();
            }
            // It is ordered in before its first frame, from inside the call
            // that makes it. Looked at again once it has drawn: a window
            // AppKit didn't order in then (or moved back onto the screen
            // after) is put right, so it is never left open and unseen.
            cx.spawn(async move |cx: &mut AsyncApp| {
                for wait in [80u64, 400] {
                    cx.background_executor().timer(Duration::from_millis(wait)).await;
                    let there = handle.update(cx, |_, window, _| {
                        let Some(w) = mac::Win::of(window) else { return };
                        if !w.is_visible() {
                            eprintln!("[orb] the full window wasn't on screen {wait} ms after opening: ordering it in again");
                            if focus { w.make_key() } else { w.order_front() }
                        }
                        if let Some((x, y)) = spot {
                            let f = w.frame();
                            if (f.x - x).abs() > 1.0 || (f.y - y).abs() > 1.0 {
                                w.set_origin(x, y);
                            }
                        }
                    });
                    // Closed meanwhile: nothing to look at.
                    if there.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        Err(e) => eprintln!("[orb] opening the full window: {e:#}"),
    }
}

/// Where the full window was last put (beside `orb.json`).
fn window_position_file() -> PathBuf {
    config::state_dir().join("window.json")
}

/// What `window.json` holds (nothing yet: an empty object).
fn window_file() -> Value {
    std::fs::read_to_string(window_position_file()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}))
}

fn write_window_file(v: &Value) {
    let _ = std::fs::create_dir_all(config::state_dir());
    let _ = std::fs::write(window_position_file(), v.to_string());
}

/// The full window was moved or resized: it opens there, at that size, next time.
pub(crate) fn save_window_frame(x: f64, y: f64, w: f64, h: f64) {
    let mut v = window_file();
    for (k, n) in [("x", x), ("y", y), ("w", w), ("h", h)] {
        v[k] = serde_json::json!(n);
    }
    write_window_file(&v);
}

/// Which of the two the orb opens: the full window (`true`) or the floating panel.
fn saved_full_mode() -> bool {
    window_file()["mode"].as_str() == Some("full")
}

/// The user switched to the full window (`true`) or back to the floating
/// panel: the orb opens that one from now on, across restarts.
fn set_full_mode(tower: &Entity<Tower>, full: bool, cx: &mut App) {
    let changed = tower.update(cx, |t, cx| {
        let changed = t.full_mode != full;
        t.full_mode = full;
        cx.notify();
        changed
    });
    if changed {
        let mut v = window_file();
        v["mode"] = serde_json::json!(if full { "full" } else { "floating" });
        write_window_file(&v);
    }
}

/// Where the user last put the full window and its size (none kept: the
/// default size; a file from before sizes were kept has only its spot).
fn saved_window() -> Option<(f64, f64, f64, f64)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(window_position_file()).ok()?).ok()?;
    let (x, y) = (v["x"].as_f64()?, v["y"].as_f64()?);
    let w = v["w"].as_f64().unwrap_or(panel::FULL_W as f64);
    let h = v["h"].as_f64().unwrap_or(panel::FULL_H as f64);
    Some((x, y, w, h))
}

/// "Floating" in the full window: back to the floating panel. The full
/// window goes (`window`: it, from its own click), the orb opens the panel
/// from now on, and the panel opens now, pinned, once the click is handled.
fn to_floating(tower: &Entity<Tower>, window: &mut Window, cx: &mut App) {
    set_full_mode(tower, false, cx);
    let orb = tower.update(cx, |t, cx| {
        t.full = None;
        cx.notify();
        t.orb
    });
    window.remove_window();
    cx.defer(move |cx| match orb {
        Some(o) => {
            let _ = o.update(cx, |orb, _, cx| {
                if !orb.panel_open(cx) {
                    orb.open_panel(false, cx);
                }
            });
        }
        None => mac::give_back_focus(),
    });
}

/// Closes the full window from another window (the floating panel's button).
fn close_full(tower: &Entity<Tower>, cx: &mut App) {
    if let Some(h) = tower.update(cx, |t, cx| {
        cx.notify();
        t.full.take()
    }) {
        let _ = h.update(cx, |_, window, _| window.remove_window());
    }
}

/// "Full window": opens it, or closes it when it is open.
fn toggle_full(tower: &Entity<Tower>, anchor: Option<mac::Rect>, cx: &mut App) {
    if full_open(tower, cx) { close_full(tower, cx) } else { open_full(tower, None, anchor, None, true, true, cx) }
}

/// The full window closed itself (Esc): a pinned floating panel has the
/// keyboard again, else the app that had it before.
fn full_closed(tower: &Entity<Tower>, cx: &mut App) {
    let pinned = tower.update(cx, |t, cx| {
        t.full = None;
        cx.notify();
        t.pinned
    });
    let kept = pinned.is_some_and(|h| {
        h.update(cx, |_, window, _| {
            if let Some(w) = mac::Win::of(window) {
                w.make_key();
            }
        })
        .is_ok()
    });
    if !kept {
        mac::give_back_focus();
    }
}

/// Herdr's colours: its dark theme ("graphite"), read from a running Herdr.
/// The light one ("paper") isn't readable from outside Herdr: these are close.
struct Palette {
    /// The count of what needs the user (the panel's amber), and its digits.
    yours: Hsla,
    on_yours: Hsla,
    popover: Hsla,
    foreground: Hsla,
    border: Hsla,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            yours: rgb(0xffcf70).into(),
            on_yours: rgb(0x16233a).into(),
            popover: rgb(0x25272b).into(),
            foreground: rgb(0xeeeeef).into(),
            border: rgba(0xffffff18).into(),
        }
    } else {
        Palette {
            yours: rgb(0xffcf70).into(),
            on_yours: rgb(0x16233a).into(),
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
        Ok(h) => {
            if let Some(spot) = show_from_start() {
                let _ = h.update(cx, |orb, _, cx| {
                    orb.open_panel(true, cx);
                    if let Some(p) = orb.panel.as_mut() {
                        p.held = true;
                        // (AppKit keeps a window it orders in on the screen: a spot off it is set after.)
                        if let Some((x, y)) = spot {
                            (p.frame.x, p.frame.y) = (x, y);
                            let _ = p.handle.update(cx, |_, window, _| {
                                if let Some(w) = mac::Win::of(window) {
                                    w.set_origin(x, y);
                                }
                            });
                        }
                    }
                });
            }
            Some(h)
        }
        Err(e) => {
            eprintln!("[orb] opening the window: {e:#}");
            None
        }
    }
}

fn close_orb(h: WindowHandle<Orb>, cx: &mut App) {
    let _ = h.update(cx, |orb, window, cx| {
        orb.close_caption(cx);
        if orb.peeking(cx) {
            orb.close_panel(false, cx);
        }
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
        let handle = *orb.borrow();
        tower.update(cx, |t, _| t.orb = handle);
        if let Some((scope, filter, spot)) = window_from_start() {
            tower.update(cx, |t, _| {
                t.nav.go(scope);
                t.nav.rec_filter = filter.or(t.nav.rec_filter.take());
                t.nav.goal = from_start_goal();
            });
            let beside = orb.borrow().as_ref().and_then(|h| h.update(cx, |orb, _, _| orb.panel.as_ref().map(|p| p.frame)).ok().flatten());
            // `HERDER_TOWER_ORB_WINDOW_REPLACES`: it takes the floating panel's place
            // as a click on "Full window" does, from inside the panel's window
            // (where that click runs), without taking the keyboard.
            let floating = tower.read(cx).floating;
            match floating.filter(|_| std::env::var_os("HERDER_TOWER_ORB_WINDOW_REPLACES").is_some()) {
                Some(f) => {
                    let t = tower.clone();
                    let _ = f.update(cx, move |_, _, cx| open_full(&t, None, beside, spot, false, true, cx));
                }
                None => open_full(&tower, None, beside, spot, false, false, cx),
            }
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_file_keeps_the_frame_and_the_mode_apart() {
        // The tests' state folder is a scratch one (HERDER_TOWER_STATE); this file is this test's alone.
        let _ = std::fs::remove_file(window_position_file());
        assert!(!saved_full_mode(), "no file: the floating panel, as before");
        save_window_frame(10.0, 20.0, 1200.0, 760.0);
        let mut v = window_file();
        v["mode"] = serde_json::json!("full");
        write_window_file(&v);
        assert!(saved_full_mode());
        // Moving or resizing the window keeps the mode; the mode kept the frame.
        save_window_frame(30.0, 40.0, 1100.0, 700.0);
        assert!(saved_full_mode());
        assert_eq!(saved_window(), Some((30.0, 40.0, 1100.0, 700.0)));
        // A file from before the mode was kept: the floating panel.
        write_window_file(&serde_json::json!({ "x": 1.0, "y": 2.0 }));
        assert!(!saved_full_mode());
        assert_eq!(saved_window(), Some((1.0, 2.0, panel::FULL_W as f64, panel::FULL_H as f64)));
        let _ = std::fs::remove_file(window_position_file());
    }
}
