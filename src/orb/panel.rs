//! The orb's panel (click the orb): a glass chat with the tower, built with
//! GPUI Kit (gpui-kit.com) on macOS's own blur.
//!
//! - **Chat**: what you said or typed on the right, the tower's answers on
//!   the left, and what the sessions reported in between. Type in the pill at
//!   the bottom: Enter sends, Shift+Enter starts a new line. While you talk,
//!   it says "Listening…" with a timer.
//! - **Attachments, as in Claude Code and Codex**: paste a screenshot or a
//!   copied file, drop files on the panel, or pick them with +; each becomes
//!   a token where the cursor is (`[Image #1]`, `[Video #2]`,
//!   `[File #3: notes.pdf]`). Deleting the token drops the file. The message
//!   reaches the brain as a spoken request does (`tower ask`), with the files'
//!   paths in token order.
//! - **Jobs** (the button by the status): what the tower handed out, in
//!   progress and finished, by space or session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, AsyncApp, ClipboardEntry, Context, Entity, ExternalPaths, FontWeight, Hsla, Image, ImageFormat, KeyDownEvent,
    ObjectFit, PathPromptOptions, Render, ScrollHandle, SharedString, StyledImage as _, Subscription, Task, Window, div, img,
    prelude::*, px, rgb, rgba,
};
use gpui_kit::component::input::{InlineToken, InputEvent, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, IconName, Sizable as _, Theme, ThemeMode, h_flex, v_flex};

use super::Tower;
use crate::config;
use crate::history::{self, Entry, Job};

/// The panel's size, in points.
pub const W: f32 = 380.0;
pub const H: f32 = 640.0;
/// Its corners (the window is rounded to match, by `mac::Win::glass`).
pub const RADIUS: f32 = 28.0;

/// The glass: white on a cool blue tint over the blurred desktop.
struct Glass {
    tint: Hsla,
    text: Hsla,
    muted: Hsla,
    faint: Hsla,
    bubble_me: Hsla,
    bubble_tower: Hsla,
    edge: Hsla,
    pill: Hsla,
    pill_text: Hsla,
    pill_muted: Hsla,
}

fn glass() -> Glass {
    Glass {
        tint: rgba(0x4a6280a6).into(),
        text: rgba(0xffffffff).into(),
        muted: rgba(0xffffffcc).into(),
        faint: rgba(0xffffffa3).into(),
        bubble_me: rgba(0xffffff42).into(),
        bubble_tower: rgba(0xffffff26).into(),
        edge: rgba(0xffffff4d).into(),
        pill: rgba(0xfffffff0).into(),
        pill_text: rgb(0x1d2330).into(),
        pill_muted: rgb(0x8a93a3).into(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Chat,
    Activity,
    Spaces,
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    Working,
    Done,
    All,
}

/// The orb's own colours (its calm, listening and speaking looks), for
/// accents on the glass.
const PALE: u32 = 0xcce0ff;
const CYAN: u32 = 0x59d9ff;
const PERI: u32 = 0xa39eff;
const SKY: u32 = 0x8ec5ff;
const AMBER: u32 = 0xffcf70;
const ROSE: u32 = 0xff8f8f;
const GREY: u32 = 0x9aa3b2;
const NAVY: u32 = 0x16233a;
/// Accents for Claude accounts, one picked from each account's name (none of
/// them is a status colour): mint, orchid, peach, lime, lilac, coral, sand, aqua.
const ACCOUNT_HUES: [u32; 8] = [0x6eeaa8, 0xf0a6ff, 0xffb98a, 0xc8f06e, 0xc9b3ff, 0xff9fb2, 0xe8d9a0, 0x7fe8e0];

/// What the activity view is narrowed to.
#[derive(Clone, PartialEq)]
enum Scope {
    Everything,
    Space(String),
    Session(String),
}

pub struct Panel {
    tower: Entity<Tower>,
    view: View,
    text: Entity<TextareaState>,
    /// The next attachment's number in this message.
    next: usize,
    sending: bool,
    /// Under the chat, briefly: (it worked, what to say).
    status: Option<(bool, String)>,
    entries: Vec<Entry>,
    jobs: Vec<Job>,
    loaded: Option<SystemTime>,
    chat: ScrollHandle,
    shown: usize,
    filter: Filter,
    scope: Scope,
    listening_since: Option<Instant>,
    /// The brain's current turn (when it began), to scroll its working line into view.
    turn: u64,
    /// The space opened in the Spaces view, and how far it had been read before.
    open_space: Option<String>,
    prev_read: u64,
    /// Herdr's spaces and their sessions (`herdr-app ls`), refreshed while the panel is open.
    live: Vec<crate::spaces::Live>,
    /// The account sessions run on when none is named (`cca ls --json`'s
    /// `current`), shown quietly; refreshed while the panel is open.
    default_account: Option<String>,
    _subs: Vec<Subscription>,
    _poll: Task<()>,
}

impl Panel {
    pub fn new(tower: Entity<Tower>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // GPUI Kit's own controls (the text field, its tokens) sit on the white
        // pill; its Root paints the theme's background behind every window,
        // which would hide the glass.
        Theme::change(ThemeMode::Light, Some(window), cx);
        Theme::update(cx, |t| t.tokens.background = gpui::transparent_black().into());
        let text = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 6).submit_on_enter(true).placeholder("Message the tower…"));
        let subs = vec![
            cx.subscribe_in(&text, window, |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { shift: false, .. } => this.send(window, cx),
                InputEvent::Change => {
                    if this.text.read(cx).value().is_empty() {
                        this.next = 1;
                    }
                    if this.status.as_ref().is_some_and(|(ok, _)| !ok) {
                        this.status = None;
                    }
                    cx.notify();
                }
                _ => {}
            }),
            cx.observe_in(&tower, window, |this, tower, window, cx| {
                // A new turn: bring the working line into view.
                let since = tower.read(cx).work.as_ref().map(|w| w.since_ms).unwrap_or(0);
                if since != this.turn {
                    this.turn = since;
                    if since > 0 {
                        this.chat.scroll_to_bottom();
                    }
                }
                let listening = matches!(tower.read(cx).phase(), "listening" | "armed");
                if listening != this.listening_since.is_some() {
                    this.listening_since = listening.then(Instant::now);
                    let hint = if listening { "Listening…" } else { "Message the tower…" };
                    this.text.update(cx, |s, cx| s.set_placeholder(hint, window, cx));
                }
                cx.notify();
            }),
        ];
        // History is a file several processes append to: look for news every
        // second (and tick the listening timer).
        let poll = cx.spawn(async move |this, cx: &mut AsyncApp| {
            let mut tick = 0u32;
            loop {
                // Herdr's spaces change slowly: look every 5 s.
                if tick % 5 == 0 {
                    let live = cx.background_executor().spawn(async { crate::spaces::live() }).await;
                    if this.update(cx, |this, cx| {
                        this.live = live;
                        cx.notify();
                    })
                    .is_err()
                    {
                        break;
                    }
                }
                // So is the list of Claude accounts: every 30 s.
                if tick % 30 == 0 {
                    let def = cx.background_executor().spawn(async { default_account() }).await;
                    if this.update(cx, |this, cx| {
                        if this.default_account != def {
                            this.default_account = def;
                            cx.notify();
                        }
                    })
                    .is_err()
                    {
                        break;
                    }
                }
                tick += 1;
                let alive = this.update(cx, |this, cx| {
                    this.reload(cx);
                    // The listening timer and running jobs' elapsed times tick.
                    let working = this.tower.read(cx).work.is_some();
                    if working || this.listening_since.is_some() || (this.view == View::Activity && this.jobs.iter().any(|j| j.open())) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        });
        text.update(cx, |s, cx| s.focus(window, cx));
        Self {
            tower,
            view: View::Chat,
            text,
            next: 1,
            sending: false,
            status: None,
            entries: vec![],
            jobs: vec![],
            loaded: None,
            chat: ScrollHandle::new(),
            shown: 0,
            filter: Filter::Working,
            scope: Scope::Everything,
            listening_since: None,
            turn: 0,
            open_space: None,
            prev_read: 0,
            live: vec![],
            default_account: None,
            _subs: subs,
            _poll: poll,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let modified = std::fs::metadata(history::path()).and_then(|m| m.modified()).ok();
        if modified.is_some() && modified == self.loaded {
            return;
        }
        self.loaded = modified;
        self.entries = history::load();
        self.jobs = history::jobs(&self.entries);
        let n = chat_items(&self.entries).len();
        if n != self.shown {
            self.shown = n;
            self.chat.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Each file becomes a token at the cursor: `[Image #1]`, `[Video #2]`, `[File #3: name]`.
    fn attach(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        for p in paths.into_iter().filter(|p| p.is_file()) {
            let n = self.next;
            self.next += 1;
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let label = match file_kind(&p) {
                Kind::Image => format!("[Image #{n}]"),
                Kind::Video => format!("[Video #{n}]"),
                Kind::Other => format!("[File #{n}: {name}]"),
            };
            let id = p.display().to_string();
            self.text.update(cx, |s, cx| {
                if s.replace_with_token(InlineToken::new(id, label), window, cx).is_ok() {
                    s.insert(" ", window, cx);
                }
            });
        }
        self.status = None;
        self.text.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// A screenshot copied to the clipboard (image data, no file): saved first.
    fn attach_image(&mut self, image: &Image, window: &mut Window, cx: &mut Context<Self>) {
        let ext = match image.format() {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Webp => "webp",
            ImageFormat::Gif => "gif",
            ImageFormat::Svg => "svg",
            ImageFormat::Bmp => "bmp",
            ImageFormat::Tiff => "tiff",
            _ => "png",
        };
        let dir = history::attachments_dir().join(history::now_ms().to_string());
        let path = dir.join(format!("pasted-image.{ext}"));
        if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&path, image.bytes()).is_ok() {
            self.attach(vec![path], window, cx);
        }
    }

    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                let _ = this.update_in(cx, |this, window, cx| this.attach(paths, window, cx));
            }
        })
        .detach();
    }

    fn has_message(&self, cx: &App) -> bool {
        !self.text.read(cx).value().trim().is_empty()
    }

    /// Hands the message to the brain, as `tower ask` does: the files behind
    /// its tokens, in order, copied into the tower's own folder first.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending || !self.has_message(cx) {
            return;
        }
        if !self.tower.read(cx).alive {
            self.status = Some((false, "The tower is off. Start it, then send.".into()));
            cx.notify();
            return;
        }
        let content = self.text.read(cx).content();
        let text = content.text().trim().to_string();
        let mut files: Vec<PathBuf> = vec![];
        for span in content.tokens() {
            let p = PathBuf::from(span.token().id().as_ref());
            if !files.contains(&p) {
                files.push(p);
            }
        }
        self.sending = true;
        self.status = None;
        cx.notify();
        let job = cx.background_executor().spawn(async move {
            let kept: Vec<String> = files.iter().map(|p| history::keep_attachment(p).display().to_string()).collect();
            let message = history::compose(&text, &kept);
            match Command::new(config::tower_bin()).arg("ask").arg(&message).stdin(Stdio::null()).output() {
                Ok(o) if o.status.success() => Ok(()),
                Ok(o) => {
                    let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    Err(e.strip_prefix("tower: ").unwrap_or(&e).to_string())
                }
                Err(e) => Err(e.to_string()),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(()) => {
                        this.text.update(cx, |s, cx| s.set_value("", window, cx));
                        this.next = 1;
                        this.status = None;
                    }
                    Err(e) => this.status = Some((false, if e.is_empty() { "Couldn't send.".into() } else { e })),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn close(&mut self, window: &mut Window) {
        window.remove_window();
        super::mac::give_back_focus();
    }
}

impl Render for Panel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let g = glass();
        let body = match self.view {
            View::Chat => self.chat_view(cx),
            View::Activity => self.activity_view(cx),
            View::Spaces => self.spaces_view(cx),
        };
        let input = self.input(window, cx);
        let header = self.header(cx);
        v_flex()
            .id("tower-panel")
            .size_full()
            .rounded(px(RADIUS))
            .bg(g.tint)
            .border_1()
            .border_color(g.edge)
            .text_color(g.text)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, _| {
                if ev.keystroke.key == "escape" {
                    this.close(window);
                }
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(rgba(0xffffffcc)).bg(rgba(0x8fb0d84d)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.attach(paths.paths().to_vec(), window, cx)))
            .child(header)
            .child(body)
            .child(input)
    }
}

impl Panel {
    fn header(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let g = glass();
        let t = self.tower.read(cx);
        let phase = if t.starting.is_some() { "starting".to_string() } else { t.phase().to_string() };
        // What the tower is doing right now, and how many sessions are at work.
        let (state, dot) = state_of(&phase);
        let mut line = state.to_string();
        if t.alive {
            let agents = t.s("agents").as_array().cloned().unwrap_or_default();
            let busy = agents.iter().filter(|a| a["status"] == "working").count();
            match (agents.len(), busy) {
                (0, _) => {}
                (n, 0) => line += &format!(" · {n} session{}", if n == 1 { "" } else { "s" }),
                (n, b) => line += &format!(" · {b} of {n} busy"),
            }
        }
        let spaces = t.spaces.clone();
        if let Some(f) = &spaces.focus {
            line = format!("{line} · Focus: {f}");
        }
        let running = self.jobs.iter().filter(|j| j.open()).count();
        let held: usize = crate::spaces::unread(&self.entries, &spaces).values().map(|v| v.len()).sum();
        let view = self.view;
        let tab = |id: &'static str, text: String, me: View| segment(id, text, view == me).flex_1();
        v_flex()
            .px_5()
            .pt_5()
            .pb_3()
            .gap_3()
            .child(
                v_flex()
                    .gap(px(2.0))
                    .child(div().text_lg().font_weight(FontWeight::MEDIUM).child("Tower"))
                    .child(
                        h_flex()
                            .gap(px(6.0))
                            .items_center()
                            .text_sm()
                            .text_color(g.muted)
                            .child(div().size(px(7.0)).flex_none().rounded_full().bg(dot))
                            .child(div().min_w_0().truncate().child(line)),
                    ),
            )
            .child(
                h_flex()
                    .p(px(3.0))
                    .gap(px(2.0))
                    .rounded_full()
                    .bg(rgba(0xffffff1a))
                    .border_1()
                    .border_color(g.edge)
                    .child(tab("view-chat", "Chat".into(), View::Chat).on_click(cx.listener(|this, _, _, cx| {
                        this.view = View::Chat;
                        cx.notify();
                    })))
                    .child(
                        tab("view-activity", if running > 0 { format!("Activity · {running}") } else { "Activity".into() }, View::Activity).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.view = View::Activity;
                                cx.notify();
                            }),
                        ),
                    )
                    .child(tab("view-spaces", if held > 0 { format!("Spaces · {held}") } else { "Spaces".into() }, View::Spaces).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.view = View::Spaces;
                            this.open_space = None;
                            cx.notify();
                        },
                    ))),
            )
    }

    fn chat_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let items = chat_items(&self.entries);
        let mut rows: Vec<AnyElement> = vec![];
        if items.is_empty() {
            rows.push(
                v_flex()
                    .pt(px(140.0))
                    .items_center()
                    .gap_2()
                    .text_color(g.muted)
                    .child(div().text_base().text_color(g.text).child("Say hi to the tower"))
                    .child(div().text_sm().text_center().child("Type below or tap Control and talk. Paste or drop pictures, videos and files."))
                    .into_any_element(),
            );
        }
        let mut day = String::new();
        for e in items {
            let d = day_label(e.at);
            if d != day {
                rows.push(h_flex().justify_center().py_1().child(div().text_xs().text_color(g.faint).child(d.clone())).into_any_element());
                day = d;
            }
            rows.push(match e.kind.as_str() {
                "request" | "answer" => bubble(e, &g),
                _ => event_line(e, &g),
            });
        }
        if self.sending {
            rows.push(h_flex().justify_end().child(div().text_xs().text_color(g.muted).child("Sending…")).into_any_element());
        }
        // While the brain works: what it's doing, until its answer is in.
        if let Some(w) = self.tower.read(cx).work.clone() {
            let now = history::now_ms();
            let answered = self.entries.iter().rev().take(20).any(|e| e.kind == "answer" && e.at >= w.since_ms);
            if !answered && now.saturating_sub(w.since_ms) < 30 * 60 * 1000 {
                rows.push(working_line(&w, now, &g));
            }
        }
        if let Some((ok, msg)) = self.status.clone() {
            rows.push(
                h_flex()
                    .justify_center()
                    .child(div().px_3().py_1().rounded_full().bg(if ok { rgba(0x34c75933) } else { rgba(0xff6b6b40) }).text_xs().child(msg))
                    .into_any_element(),
            );
        }
        let t = self.tower.read(cx);
        if !t.alive && t.starting.is_none() {
            rows.push(
                h_flex()
                    .justify_center()
                    .pt_2()
                    .child(
                        h_flex()
                            .id("start")
                            .gap_2()
                            .items_center()
                            .px_4()
                            .py_2()
                            .rounded_full()
                            .bg(rgba(0xffffff2e))
                            .border_1()
                            .border_color(g.edge)
                            .cursor_pointer()
                            .hover(|s| s.bg(rgba(0xffffff47)))
                            .on_click(cx.listener(|this, _, _, cx| Tower::start(&this.tower, cx)))
                            .text_sm()
                            .child(Icon::new(IconName::Play).small())
                            .child("The tower is off · Start it"),
                    )
                    .into_any_element(),
            );
        }
        div()
            .id("chat")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.chat)
            .child(v_flex().w_full().px_5().py_2().gap_3().children(rows))
            .into_any_element()
    }

    fn input(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let listening = self.listening_since.map(|t| t.elapsed().as_secs());
        let can_send = self.has_message(cx) && !self.sending;
        let round = |id: &'static str, icon: IconName| {
            div()
                .id(id)
                .size(px(32.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .text_color(rgb(0x8a93a3))
                .hover(|s| s.bg(rgb(0xeef1f5)).text_color(rgb(0x1d2330)))
                .child(Icon::new(icon).small())
        };
        let right: AnyElement = match listening {
            Some(secs) => h_flex()
                .flex_none()
                .gap_2()
                .items_center()
                .px_3()
                .py(px(6.0))
                .rounded_full()
                .bg(rgb(0xeef1f5))
                .text_sm()
                .text_color(g.pill_muted)
                .child(div().size(px(7.0)).rounded_full().bg(rgb(0x9aa3b2)))
                .child(format!("{:02}:{:02}", secs / 60, secs % 60))
                .into_any_element(),
            None => h_flex()
                .flex_none()
                .gap_1()
                .items_center()
                .child(round("attach", IconName::Plus).on_click(cx.listener(|this, _, window, cx| this.pick_files(window, cx))))
                .child(round("talk", IconName::Mic).on_click(cx.listener(|this, _, _, cx| Tower::talk(&this.tower, cx))))
                .child(
                    div()
                        .id("send")
                        .size(px(32.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(if can_send { rgb(0x1d2330) } else { rgb(0xd5dae2) })
                        .text_color(rgb(0xffffff))
                        .when(can_send, |el| el.cursor_pointer().hover(|s| s.bg(rgb(0x2f3746))))
                        .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                        .child(if self.sending {
                            Spinner::new().small().color(rgb(0xffffff).into()).into_any_element()
                        } else {
                            Icon::new(IconName::ArrowUp).small().into_any_element()
                        }),
                )
                .into_any_element(),
        };
        let me = cx.entity().downgrade();
        let field = Textarea::new(&self.text)
            .appearance(false)
            .bordered(false)
            .small()
            .w_full()
            .text_size(px(15.0))
            .text_color(g.pill_text)
            .on_paste(move |item, window, cx| {
                // A copied file attaches as a token; so does a copied image (a screenshot).
                let mut files: Vec<PathBuf> = vec![];
                let mut image = None;
                for e in item.entries() {
                    match e {
                        ClipboardEntry::ExternalPaths(p) => files.extend(p.paths().iter().cloned()),
                        ClipboardEntry::Image(i) => image = Some(i.clone()),
                        _ => {}
                    }
                }
                if !files.is_empty() {
                    let _ = me.update(cx, |this, cx| this.attach(files, window, cx));
                    return true;
                }
                if let Some(i) = image {
                    let _ = me.update(cx, |this, cx| this.attach_image(&i, window, cx));
                    return true;
                }
                false
            });
        // A short message sits in one line beside the orb and the buttons;
        // a longer one (or one with files or line breaks) gets the pill's
        // whole width, with the orb and the buttons in a row beneath it.
        let state = self.text.read(cx);
        let value = state.value();
        let wide = listening.is_none() && (value.contains('\n') || value.chars().count() > 24 || !state.tokens().is_empty());
        let pill = if wide {
            v_flex()
                .w_full()
                .pl(px(8.0))
                .pr(px(10.0))
                .pt(px(8.0))
                .pb(px(8.0))
                .gap_1()
                .rounded(px(24.0))
                .child(div().w_full().child(field))
                .child(h_flex().w_full().justify_end().child(right))
                .into_any_element()
        } else {
            h_flex()
                .w_full()
                .min_h(px(52.0))
                .pl(px(8.0))
                .pr(px(10.0))
                .py(px(8.0))
                .gap_1()
                .items_center()
                .rounded(px(26.0))
                .child(div().flex_1().min_w_0().child(field))
                .child(right)
                .into_any_element()
        };
        div()
            .px_4()
            .pb_4()
            .pt_2()
            .child(div().w_full().rounded(px(26.0)).bg(g.pill).shadow_lg().text_color(g.pill_text).child(pill))
            .into_any_element()
    }

    /// Real work only: the jobs the tower handed out (running, with how
    /// long; done, with what came of it) and what happened around them, for
    /// every session or one.
    fn activity_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let acc = Accounts::of_tower(self.tower.read(cx), self.default_account.clone());
        let scope = self.scope.clone();
        let fits = |session: &str, space: &str| match &scope {
            Scope::Everything => true,
            Scope::Space(s) => space == s,
            Scope::Session(s) => session == s,
        };
        let working: Vec<Job> = self.jobs.iter().filter(|j| j.open() && fits(&j.session, &j.space)).cloned().collect();
        let done: Vec<Job> = self.jobs.iter().filter(|j| !j.open() && fits(&j.session, &j.space)).cloned().collect();
        let events: Vec<Entry> = self
            .entries
            .iter()
            .rev()
            .filter(|e| !matches!(e.kind.as_str(), "request" | "answer"))
            .filter(|e| fits(e.session.as_deref().unwrap_or(""), e.space.as_deref().unwrap_or("")))
            .filter(|e| scope == Scope::Everything || e.session.is_some())
            .take(200)
            .cloned()
            .collect();

        let filter = self.filter;
        let filters = h_flex()
            .mx_5()
            .mb_2()
            .p(px(3.0))
            .gap(px(2.0))
            .rounded_full()
            .bg(rgba(0xffffff14))
            .border_1()
            .border_color(g.edge)
            .child(segment("f-working", format!("Working · {}", working.len()), filter == Filter::Working).flex_1().on_click(cx.listener(|this, _, _, cx| {
                this.filter = Filter::Working;
                cx.notify();
            })))
            .child(segment("f-done", format!("Done · {}", done.len()), filter == Filter::Done).flex_1().on_click(cx.listener(|this, _, _, cx| {
                this.filter = Filter::Done;
                cx.notify();
            })))
            .child(segment("f-all", "All".into(), filter == Filter::All).flex_1().on_click(cx.listener(|this, _, _, cx| {
                this.filter = Filter::All;
                cx.notify();
            })));

        // Sessions that did something, most recent first (and spaces, when there are several).
        let mut spaces: Vec<String> = vec![];
        let mut sessions: Vec<(String, String)> = vec![];
        for e in self.entries.iter().rev() {
            let (Some(addr), false) = (e.session.as_ref(), matches!(e.kind.as_str(), "request" | "answer")) else { continue };
            if let Some(sp) = e.space.as_ref().filter(|s| !s.is_empty()) {
                if !spaces.contains(sp) {
                    spaces.push(sp.clone());
                }
            }
            if !sessions.iter().any(|(a, _)| a == addr) {
                sessions.push((addr.clone(), e.name.clone().unwrap_or_else(|| addr.clone())));
            }
        }
        let mut targets: Vec<(String, Scope)> = vec![];
        if !sessions.is_empty() {
            targets.push(("All sessions".into(), Scope::Everything));
            if spaces.len() > 1 {
                targets.extend(spaces.iter().map(|s| (s.clone(), Scope::Space(s.clone()))));
            }
            targets.extend(sessions.iter().take(12).map(|(a, n)| (n.clone(), Scope::Session(a.clone()))));
        }
        let chips: Vec<AnyElement> = targets
            .into_iter()
            .enumerate()
            .map(|(i, (text, target))| {
                let selected = target == scope;
                div()
                    .id(SharedString::from(format!("scope-{i}")))
                    .px_2()
                    .py(px(3.0))
                    .rounded_full()
                    .cursor_pointer()
                    .text_xs()
                    .when(selected, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)))
                    .when(!selected, |el| el.bg(rgba(0xffffff14)).text_color(g.muted).hover(|s| s.bg(rgba(0xffffff2e))))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scope = target.clone();
                        cx.notify();
                    }))
                    .child(text)
                    .into_any_element()
            })
            .collect();

        let now = history::now_ms();
        let mut items: Vec<AnyElement> = vec![];
        match filter {
            Filter::Working => items.extend(working.iter().map(|j| job_card(j, &g, now, &acc))),
            Filter::Done => items.extend(done.iter().map(|j| job_card(j, &g, now, &acc))),
            Filter::All => {
                let mut day = String::new();
                for e in &events {
                    let d = day_label(e.at);
                    if d != day {
                        items.push(div().pt_1().text_xs().text_color(g.faint).child(d.clone()).into_any_element());
                        day = d;
                    }
                    items.push(activity_row(e, &g, &acc));
                }
            }
        }
        if items.is_empty() {
            let (title, sub) = match filter {
                Filter::Working => ("No work running", "When the tower hands a session a job, it shows here with how long it's been going."),
                Filter::Done => ("Nothing finished yet", "Finished jobs land here with what came of them."),
                Filter::All => ("No activity yet", "Jobs, session reports and alerts show up here as they happen."),
            };
            items.push(
                v_flex()
                    .pt(px(100.0))
                    .px_6()
                    .items_center()
                    .gap_2()
                    .text_color(g.muted)
                    .child(div().text_base().text_color(g.text).child(title))
                    .child(div().text_sm().text_center().child(sub))
                    .into_any_element(),
            );
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .child(filters)
            .child(h_flex().px_5().pb_3().gap_1().flex_wrap().children(chips))
            .child(div().id("activity").flex_1().min_h_0().overflow_y_scroll().child(v_flex().w_full().px_5().pb_3().gap_2().children(items)))
            .into_any_element()
    }
}

impl Panel {
    /// Every space: how it's doing, its agenda and what's waiting; focus one
    /// to hear only its news. Tap one for the whole picture.
    fn spaces_view(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(name) = self.open_space.clone() {
            return self.space_detail(&name, cx);
        }
        let g = glass();
        let acc = Accounts::of_tower(self.tower.read(cx), self.default_account.clone());
        let sp = self.tower.read(cx).spaces.clone();
        let unread = crate::spaces::unread(&self.entries, &sp);
        let mut names = crate::spaces::known(&self.live, &self.entries, &sp);
        let last = |n: &String| self.entries.iter().rev().find(|e| e.space.as_ref() == Some(n)).map(|e| e.at).unwrap_or(0);
        names.sort_by_key(|n| (sp.focus.as_ref() != Some(n), std::cmp::Reverse(unread.get(n).map(|v| v.len()).unwrap_or(0)), std::cmp::Reverse(last(n))));

        let hint = match &sp.focus {
            Some(f) => format!("Focused on {f}: other spaces' news waits here instead of being spoken."),
            None => "No focus: every space's news is spoken. Focus one to hold the rest.".to_string(),
        };
        let mut items: Vec<AnyElement> = vec![
            h_flex()
                .gap_2()
                .items_center()
                .pb_1()
                .child(div().flex_1().min_w_0().text_xs().text_color(g.muted).child(hint))
                .when(sp.focus.is_some(), |el| {
                    el.child(
                        div()
                            .id("clear-focus")
                            .flex_none()
                            .px_2()
                            .py(px(3.0))
                            .rounded_full()
                            .cursor_pointer()
                            .text_xs()
                            .bg(rgba(0xffffff1f))
                            .hover(|s| s.bg(rgba(0xffffff38)))
                            .on_click(cx.listener(|this, _, _, cx| this.set_focus(None, cx)))
                            .child("Clear focus"),
                    )
                })
                .into_any_element(),
        ];
        for (i, name) in names.iter().enumerate() {
            let sessions = self.live.iter().find(|l| &l.name == name).map(|l| l.sessions.clone()).unwrap_or_default();
            let held = unread.get(name).cloned().unwrap_or_default();
            let urgent = sessions.iter().any(|s| s.2 == "blocked") || held.iter().any(|e| e.status.as_deref() == Some("blocked"));
            let busy = sessions.iter().filter(|s| s.2 == "working").count();
            let (dot, mood) = if urgent {
                (AMBER, "needs you".to_string())
            } else if busy > 0 {
                (CYAN, format!("{busy} working"))
            } else {
                (GREY, "quiet".to_string())
            };
            let running = self.jobs.iter().filter(|j| &j.space == name && j.open()).count();
            let mut meta = vec![mood, format!("{} session{}", sessions.len(), if sessions.len() == 1 { "" } else { "s" })];
            if running > 0 {
                meta.push(format!("{running} job{} running", if running == 1 { "" } else { "s" }));
            }
            let at = last(name);
            if at > 0 {
                meta.push(ago(at));
            }
            let focused = sp.focus.as_ref() == Some(name);
            let (n1, n2) = (name.clone(), name.clone());
            items.push(
                v_flex()
                    .id(SharedString::from(format!("space-{i}")))
                    .p_4()
                    .gap(px(6.0))
                    .rounded(px(18.0))
                    .cursor_pointer()
                    .bg(if focused { rgba(0xffffff33) } else { g.bubble_tower.into() })
                    .border_1()
                    .border_color(if focused { rgb(PALE).into() } else { g.edge })
                    .hover(|s| s.bg(rgba(0xffffff2e)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open(&n1, cx)))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(8.0)).flex_none().rounded_full().bg(rgb(dot)))
                            .child(div().flex_1().min_w_0().truncate().text_base().font_weight(FontWeight::MEDIUM).child(name.clone()))
                            .children(acc.space_badge(&sessions, &g))
                            .when(!held.is_empty(), |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .px_2()
                                        .rounded_full()
                                        .bg(rgb(if urgent { AMBER } else { CYAN }))
                                        .text_color(rgb(NAVY))
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(format!("{} new", held.len())),
                                )
                            })
                            .child(
                                div()
                                    .id(SharedString::from(format!("focus-{i}")))
                                    .flex_none()
                                    .px_2()
                                    .py(px(3.0))
                                    .rounded_full()
                                    .text_xs()
                                    .cursor_pointer()
                                    .when(focused, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)).font_weight(FontWeight::MEDIUM))
                                    .when(!focused, |el| el.border_1().border_color(g.edge).hover(|s| s.bg(rgba(0xffffff2e))))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        let f = if this.tower.read(cx).spaces.focus.as_ref() == Some(&n2) { None } else { Some(n2.clone()) };
                                        this.set_focus(f, cx);
                                    }))
                                    .child(if focused { "Focused" } else { "Focus" }),
                            ),
                    )
                    .child(match sp.agendas.get(name) {
                        Some(a) => div().text_sm().line_clamp(2).child(a.clone()),
                        None => div().text_sm().text_color(g.faint).child("No agenda yet"),
                    })
                    .child(div().text_xs().text_color(g.muted).truncate().child(meta.join(" · ")))
                    .into_any_element(),
            );
        }
        if names.is_empty() {
            items.push(
                v_flex()
                    .pt(px(100.0))
                    .px_6()
                    .items_center()
                    .gap_2()
                    .text_color(g.muted)
                    .child(div().text_base().text_color(g.text).child("No spaces yet"))
                    .child(div().text_sm().text_center().child("Spaces you make in Herdr show up here, each with its sessions and news."))
                    .into_any_element(),
            );
        }
        div()
            .id("spaces")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(v_flex().w_full().px_5().pb_3().gap_2().children(items))
            .into_any_element()
    }

    /// One space: its agenda, sessions, what came in while it wasn't in
    /// focus (marked read on opening), its jobs and its story so far.
    fn space_detail(&mut self, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let acc = Accounts::of_tower(self.tower.read(cx), self.default_account.clone());
        let sp = self.tower.read(cx).spaces.clone();
        let focused = sp.focus.as_deref() == Some(name);
        let sessions = self.live.iter().find(|l| l.name == name).map(|l| l.sessions.clone()).unwrap_or_default();
        let now = history::now_ms();
        let section = |t: &str| div().pt_2().text_xs().font_weight(FontWeight::MEDIUM).text_color(g.faint).child(t.to_uppercase());
        let mut items: Vec<AnyElement> = vec![];
        let n = name.to_string();
        items.push(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .id("back")
                        .px_2()
                        .py(px(3.0))
                        .rounded_full()
                        .cursor_pointer()
                        .text_sm()
                        .text_color(g.muted)
                        .hover(|s| s.bg(rgba(0xffffff1f)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_space = None;
                            cx.notify();
                        }))
                        .child("‹ All spaces"),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("detail-focus")
                        .px_3()
                        .py(px(4.0))
                        .rounded_full()
                        .cursor_pointer()
                        .text_sm()
                        .when(focused, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)).font_weight(FontWeight::MEDIUM))
                        .when(!focused, |el| el.border_1().border_color(g.edge).hover(|s| s.bg(rgba(0xffffff2e))))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let f = if this.tower.read(cx).spaces.focus.as_ref() == Some(&n) { None } else { Some(n.clone()) };
                            this.set_focus(f, cx);
                        }))
                        .child(if focused { "Focused" } else { "Focus this space" }),
                )
                .into_any_element(),
        );
        items.push(
            h_flex()
                .pt_1()
                .gap_2()
                .items_center()
                .child(div().min_w_0().truncate().text_xl().font_weight(FontWeight::MEDIUM).child(name.to_string()))
                .children(acc.space_badge(&sessions, &g))
                .into_any_element(),
        );
        items.push(match sp.agendas.get(name) {
            Some(a) => div().text_sm().child(a.clone()).into_any_element(),
            None => div().text_sm().text_color(g.faint).child("No agenda yet. Tell the tower what this space is for.").into_any_element(),
        });
        if !sessions.is_empty() {
            items.push(section("Sessions").into_any_element());
            items.push(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(sessions.iter().map(|(addr, sname, st)| {
                        let dot = match st.as_str() {
                            "working" => CYAN,
                            "blocked" => AMBER,
                            "done" | "idle" => PALE,
                            _ => GREY,
                        };
                        h_flex()
                            .gap(px(6.0))
                            .items_center()
                            .px_2()
                            .py(px(3.0))
                            .rounded_full()
                            .bg(rgba(0xffffff1a))
                            .text_xs()
                            .child(div().size(px(6.0)).rounded_full().bg(rgb(dot)))
                            .child(if sname.is_empty() { addr.clone() } else { sname.clone() })
                            .when(!st.is_empty(), |el| el.child(div().text_color(g.muted).child(st.clone())))
                            .children(acc.of(addr).map(|a| acc.badge(a, &g)))
                    }))
                    .into_any_element(),
            );
        }
        // What waited while another space was in focus.
        let fresh: Vec<&Entry> =
            self.entries.iter().rev().filter(|e| e.held && e.space.as_deref() == Some(name) && e.at > self.prev_read).take(30).collect();
        if !fresh.is_empty() {
            items.push(section(&format!("New while you were elsewhere · {}", fresh.len())).into_any_element());
            items.extend(fresh.iter().map(|e| activity_row(e, &g, &acc)));
        }
        let jobs: Vec<&Job> = self.jobs.iter().filter(|j| j.space == name).collect();
        let (running, done): (Vec<&Job>, Vec<&Job>) = jobs.into_iter().partition(|j| j.open());
        if !running.is_empty() {
            items.push(section("Running").into_any_element());
            items.extend(running.iter().map(|j| job_card(j, &g, now, &acc)));
        }
        if !done.is_empty() {
            items.push(section("Done").into_any_element());
            items.extend(done.iter().take(10).map(|j| job_card(j, &g, now, &acc)));
        }
        let story: Vec<&Entry> = self
            .entries
            .iter()
            .rev()
            .filter(|e| e.space.as_deref() == Some(name) && matches!(e.kind.as_str(), "summary" | "watch" | "alert" | "note") && !(e.held && e.at > self.prev_read))
            .take(40)
            .collect();
        if !story.is_empty() {
            items.push(section("Story so far").into_any_element());
            items.extend(story.iter().map(|e| activity_row(e, &g, &acc)));
        }
        div()
            .id("space-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(v_flex().w_full().px_5().pb_3().gap_2().children(items))
            .into_any_element()
    }

    /// Opens a space: what was held there counts as delivered from now.
    fn open(&mut self, name: &str, cx: &mut Context<Self>) {
        let n = name.to_string();
        self.prev_read = self.tower.read(cx).spaces.read_ms.get(&n).copied().unwrap_or(0);
        self.open_space = Some(n.clone());
        self.tower.update(cx, |t, _| {
            t.spaces.read_ms.insert(n, history::now_ms());
            t.save_spaces();
        });
        cx.notify();
    }

    fn set_focus(&mut self, focus: Option<String>, cx: &mut Context<Self>) {
        self.tower.update(cx, |t, cx| {
            t.spaces.focus = focus;
            t.save_spaces();
            cx.notify();
        });
        cx.notify();
    }
}

/// The brain at work, as Claude Code shows it: a turning mark, what it's
/// doing (from its own hooks), and for how long.
fn working_line(w: &crate::activity::Work, now: u64, g: &Glass) -> AnyElement {
    const MARKS: [&str; 10] = ["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
    let elapsed = now.saturating_sub(w.since_ms);
    // A finished step stays a moment (a quick read is seen too), then it's thinking again.
    let thinking = (w.word == "Thinking" && w.detail.is_empty()) || (w.done_ms > 0 && now.saturating_sub(w.done_ms) > 3000);
    // Plain thinking reads the same for a while: let its words turn over.
    let (word, detail) = if thinking {
        (["Thinking", "Considering", "Working it out"][((elapsed / 3000) % 3) as usize].to_string(), String::new())
    } else {
        (w.word.clone(), w.detail.clone())
    };
    let mark = div().w(px(14.0)).flex_none().text_color(rgb(CYAN)).child(MARKS[0]).with_animation(
        "working-mark",
        Animation::new(Duration::from_millis(1200)).repeat(),
        |_, d| div().w(px(14.0)).flex_none().text_color(rgb(CYAN)).child(MARKS[((d * MARKS.len() as f32) as usize).min(MARKS.len() - 1)]),
    );
    h_flex()
        .w_full()
        .child(
            h_flex()
                .max_w(px(W * 0.86))
                .gap_2()
                .items_center()
                .px_4()
                .py(px(10.0))
                .rounded(px(18.0))
                .bg(g.bubble_tower)
                .border_1()
                .border_color(g.edge)
                .text_sm()
                .child(mark)
                .child(div().flex_none().font_weight(FontWeight::MEDIUM).text_color(rgb(PALE)).child(if detail.is_empty() { format!("{word}…") } else { word }))
                .when(!detail.is_empty(), |el| el.child(div().min_w_0().truncate().child(format!("{detail}…"))))
                .child(div().flex_none().text_color(g.faint).child(format!("· {}", span(elapsed)))),
        )
        .into_any_element()
}

/// A session's report without the instructions the tower gave the brain with it.
fn report(text: &str) -> String {
    let mut t = text.to_string();
    for cut in [" Read its screen and tell me what happened, briefly.", " Tell me briefly.", " Ask me before you answer it."] {
        t = t.replace(cut, "");
    }
    t
}

/// One choice in a segmented control on the glass.
fn segment(id: &'static str, text: String, selected: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py(px(5.0))
        .flex()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .text_sm()
        .when(selected, |el| el.bg(rgb(PALE)).text_color(rgb(NAVY)).font_weight(FontWeight::MEDIUM))
        .when(!selected, |el| el.text_color(rgba(0xffffffd9)).hover(|s| s.bg(rgba(0xffffff1f))))
        .child(text)
}

/// What the chat shows: requests and answers, and what came in between.
fn chat_items(entries: &[Entry]) -> Vec<&Entry> {
    // News held from an unfocused space waits in Spaces, not here.
    let keep = |e: &&Entry| !e.held && match e.kind.as_str() {
        "request" | "answer" | "watch" | "alert" | "summary" => true,
        "job" => matches!(e.status.as_deref(), Some("done" | "failed" | "error" | "exited" | "blocked")) && !e.text.is_empty(),
        _ => false,
    };
    let all: Vec<&Entry> = entries.iter().filter(keep).collect();
    let skip = all.len().saturating_sub(150);
    all.into_iter().skip(skip).collect()
}

fn bubble(e: &Entry, g: &Glass) -> AnyElement {
    let mine = e.kind == "request";
    let images: Vec<&String> = e.attachments.iter().filter(|a| matches!(file_kind(Path::new(a)), Kind::Image)).collect();
    let others: Vec<&String> = e.attachments.iter().filter(|a| !matches!(file_kind(Path::new(a)), Kind::Image)).collect();
    let body = v_flex()
        .max_w(px(W * 0.74))
        .px_4()
        .py_3()
        .gap_2()
        .rounded(px(20.0))
        .bg(if mine { g.bubble_me } else { g.bubble_tower })
        .border_1()
        .border_color(g.edge)
        .when(!images.is_empty(), |el| {
            el.child(h_flex().gap_2().flex_wrap().children(images.into_iter().map(|p| {
                div()
                    .size(px(72.0))
                    .rounded(px(12.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(g.edge)
                    .child(img(PathBuf::from(p)).size_full().object_fit(ObjectFit::Cover))
            })))
        })
        .when(!others.is_empty(), |el| {
            el.child(h_flex().gap_1().flex_wrap().children(others.into_iter().map(|p| {
                let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                h_flex()
                    .gap_1()
                    .px_2()
                    .py(px(2.0))
                    .rounded_md()
                    .bg(rgba(0xffffff26))
                    .text_xs()
                    .child(Icon::new(if matches!(file_kind(Path::new(p)), Kind::Video) { IconName::Play } else { IconName::File }).xsmall())
                    .child(name)
            })))
        })
        .when(!e.text.is_empty(), |el| el.child(div().text_sm().line_height(px(21.0)).child(e.text.clone())))
        .child(div().text_xs().text_color(g.faint).child(clock(e.at)));
    h_flex().w_full().when(mine, |el| el.justify_end()).child(body).into_any_element()
}

fn event_line(e: &Entry, g: &Glass) -> AnyElement {
    let who = e.name.clone().or_else(|| e.session.clone()).unwrap_or_default();
    let (icon, what) = match (e.kind.as_str(), e.status.as_deref()) {
        ("job", Some("done")) => (IconName::CircleCheck, format!("{who} finished: {}", e.text)),
        ("job", Some("blocked")) => (IconName::TriangleAlert, format!("{who} needs you: {}", e.text)),
        ("job", _) => (IconName::CircleX, format!("{who}: {}", e.text)),
        ("alert", _) => (IconName::Bell, e.text.clone()),
        ("summary", _) => (IconName::FileText, format!("{who}: {}", e.text)),
        _ => (IconName::SquareTerminal, report(&e.text)),
    };
    h_flex()
        .w_full()
        .justify_center()
        .child(
            h_flex()
                .max_w(px(W * 0.86))
                .gap_2()
                .items_start()
                .px_3()
                .py(px(6.0))
                .rounded(px(14.0))
                .bg(rgba(0xffffff1f))
                .text_xs()
                .text_color(g.muted)
                .child(div().pt(px(1.0)).child(Icon::new(icon).xsmall()))
                .child(div().min_w_0().line_clamp(3).child(what))
                .child(div().flex_none().text_color(g.faint).child(clock(e.at))),
        )
        .into_any_element()
}

/// A job: what was asked, of whom, and where it stands.
fn job_card(j: &Job, g: &Glass, now: u64, acc: &Accounts) -> AnyElement {
    let who = if j.name.is_empty() { j.session.clone() } else { j.name.clone() };
    let (mark, color): (AnyElement, Hsla) = match j.status.as_str() {
        "working" => (div().size(px(8.0)).rounded_full().bg(rgb(CYAN)).into_any_element(), rgb(CYAN).into()),
        "blocked" => (Icon::new(IconName::TriangleAlert).small().text_color(rgb(AMBER)).into_any_element(), rgb(AMBER).into()),
        "done" | "idle" => (Icon::new(IconName::CircleCheck).small().text_color(rgb(PALE)).into_any_element(), rgb(PALE).into()),
        _ => (Icon::new(IconName::CircleX).small().text_color(rgb(ROSE)).into_any_element(), rgb(ROSE).into()),
    };
    let when = match j.status.as_str() {
        "working" => format!("running {}", span(now.saturating_sub(j.started))),
        "blocked" => format!("waiting for you · {}", span(now.saturating_sub(j.started))),
        s => format!("{} {} · took {}", status_word(s), ago(j.updated), span(j.updated.saturating_sub(j.started))),
    };
    let mut meta = vec![who.clone()];
    if !j.space.is_empty() {
        meta.push(j.space.clone());
    }
    let title = if j.title.is_empty() { format!("Work in {who}") } else { j.title.clone() };
    v_flex()
        .p_4()
        .gap(px(6.0))
        .rounded(px(18.0))
        .bg(g.bubble_tower)
        .border_1()
        .border_color(g.edge)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(h_flex().w(px(16.0)).flex_none().justify_center().child(mark))
                .child(div().flex_1().min_w_0().text_sm().font_weight(FontWeight::MEDIUM).line_clamp(3).child(title)),
        )
        .when(!j.summary.is_empty(), |el| el.child(div().pl(px(24.0)).text_sm().line_clamp(6).child(j.summary.clone())))
        .child(
            h_flex()
                .pl(px(24.0))
                .gap_2()
                .text_xs()
                .child(div().flex_none().text_color(color).child(when))
                .child(div().min_w_0().truncate().text_color(g.muted).child(meta.join(" · ")))
                .children(acc.of(&j.session).map(|a| acc.badge(a, g))),
        )
        .into_any_element()
}

/// A line of activity: a session reporting back, an alert, a summary, a job starting or ending.
fn activity_row(e: &Entry, g: &Glass, acc: &Accounts) -> AnyElement {
    let who = e.name.clone().or_else(|| e.session.clone()).unwrap_or_default();
    let (icon, color, text) = match (e.kind.as_str(), e.status.as_deref()) {
        ("job", Some("working")) => (IconName::ArrowUp, CYAN, format!("Asked {who}: {}", e.text)),
        ("job", Some("done")) => (IconName::CircleCheck, PALE, if e.text.is_empty() { format!("{who} finished") } else { format!("{who} finished: {}", e.text) }),
        ("job", Some("blocked")) => (IconName::TriangleAlert, AMBER, format!("{who} needs you{}", if e.text.is_empty() { String::new() } else { format!(": {}", e.text) })),
        ("job", Some(s)) => (IconName::CircleX, ROSE, format!("{who} {}{}", status_word(s), if e.text.is_empty() { String::new() } else { format!(": {}", e.text) })),
        ("watch", _) => (IconName::SquareTerminal, SKY, report(&e.text)),
        ("alert", _) => (IconName::Bell, AMBER, e.text.clone()),
        ("summary", _) => (IconName::FileText, PERI, format!("{who}: {}", e.text)),
        _ => (IconName::Info, GREY, e.text.clone()),
    };
    h_flex()
        .gap_3()
        .items_start()
        .py(px(5.0))
        .child(div().w(px(52.0)).flex_none().pt(px(1.0)).text_xs().text_color(g.faint).child(clock(e.at)))
        .child(div().pt(px(2.0)).child(Icon::new(icon).xsmall().text_color(rgb(color))))
        .child(div().flex_1().min_w_0().text_sm().line_clamp(4).child(text))
        .children(e.session.as_deref().and_then(|s| acc.of(s)).map(|a| div().pt(px(1.0)).flex_none().child(acc.badge(a, g))))
        .into_any_element()
}

/// Which Claude account each session runs on (`default`, `orca`), from the
/// radar's `account` field; Codex and unknown sessions have none. Tolerates
/// the field missing (older daemons): then nothing is shown.
#[derive(Default)]
struct Accounts {
    by_session: HashMap<String, String>,
    /// The claude-accounts mod's default, drawn quietly.
    default: Option<String>,
}

impl Accounts {
    fn of_tower(t: &Tower, default: Option<String>) -> Self {
        Self::from_agents(t.s("agents"), default)
    }

    fn from_agents(agents: &serde_json::Value, default: Option<String>) -> Self {
        let by_session = agents
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| {
                    let addr = a["address"].as_str()?;
                    let acc = a["account"].as_str().map(str::trim).filter(|s| !s.is_empty())?;
                    Some((addr.to_string(), acc.to_string()))
                })
                .collect();
        Accounts { by_session, default }
    }

    fn of(&self, addr: &str) -> Option<&str> {
        self.by_session.get(addr).map(String::as_str)
    }

    fn badge(&self, account: &str, g: &Glass) -> AnyElement {
        badge(account, self.default.as_deref() == Some(account), g)
    }

    /// The accounts a space's Claude sessions run on, each once.
    fn space_accounts(&self, sessions: &[(String, String, String)]) -> Vec<&str> {
        let mut seen: Vec<&str> = sessions.iter().filter_map(|(addr, _, _)| self.of(addr)).collect();
        seen.sort();
        seen.dedup();
        seen
    }

    /// A space's account: the one all its Claude sessions share, or mixed.
    fn space(&self, sessions: &[(String, String, String)]) -> SpaceAccount {
        match self.space_accounts(sessions).as_slice() {
            [] => SpaceAccount::None,
            [one] => SpaceAccount::One(one.to_string()),
            _ => SpaceAccount::Mixed,
        }
    }

    fn space_badge(&self, sessions: &[(String, String, String)], g: &Glass) -> Option<AnyElement> {
        match self.space(sessions) {
            SpaceAccount::None => None,
            SpaceAccount::One(a) => Some(self.badge(&a, g)),
            SpaceAccount::Mixed => Some(
                h_flex()
                    .flex_none()
                    .gap(px(4.0))
                    .items_center()
                    .px(px(7.0))
                    .rounded_full()
                    .border_1()
                    .border_color(g.edge)
                    .text_xs()
                    .text_color(g.muted)
                    .children(self.space_accounts(sessions).into_iter().take(3).map(|a| {
                        let c: Hsla = if self.default.as_deref() == Some(a) { g.muted } else { rgb(account_hue(a)).into() };
                        div().size(px(5.0)).rounded_full().bg(c)
                    }))
                    .child("mixed")
                    .into_any_element(),
            ),
        }
    }
}

#[derive(Debug, PartialEq)]
enum SpaceAccount {
    None,
    One(String),
    Mixed,
}

/// A small account label with the account's own name: the default account
/// quiet (outlined), any other filled with a colour picked from its name, so
/// an account keeps its colour and a new one gets one without a code change.
fn badge(account: &str, quiet: bool, g: &Glass) -> AnyElement {
    let el = div().flex_none().px(px(7.0)).rounded_full().text_xs().max_w(px(110.0)).truncate();
    if quiet {
        el.border_1().border_color(g.edge).text_color(g.muted).child(account.to_string())
    } else {
        el.bg(rgb(account_hue(account))).text_color(rgb(NAVY)).font_weight(FontWeight::MEDIUM).child(account.to_string())
    }
    .into_any_element()
}

/// The same colour for the same name, every run (FNV-1a over the name).
fn account_hue(name: &str) -> u32 {
    let h = name.to_lowercase().bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    ACCOUNT_HUES[h as usize % ACCOUNT_HUES.len()]
}

/// The claude-accounts mod's default account (`cca ls --json`'s `current`),
/// or None without the mod.
fn default_account() -> Option<String> {
    let out = Command::new("cca").args(["ls", "--json"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v["current"].as_str().filter(|s| !s.is_empty()).map(String::from)
}

#[cfg(test)]
mod account_tests {
    use super::*;
    use serde_json::json;

    fn sessions(addrs: &[&str]) -> Vec<(String, String, String)> {
        addrs.iter().map(|a| (a.to_string(), String::new(), String::new())).collect()
    }

    #[test]
    fn a_space_shows_its_one_account_or_mixed() {
        let agents = json!([
            { "address": "t1", "account": "work" },
            { "address": "t2", "account": "work" },
            { "address": "t3", "account": "side" },
            { "address": "t4", "account": null },
            { "address": "t5" }
        ]);
        let acc = Accounts::from_agents(&agents, Some("work".into()));
        assert_eq!(acc.space(&sessions(&["t1", "t2", "t4", "t5"])), SpaceAccount::One("work".into()));
        assert_eq!(acc.space(&sessions(&["t1", "t3"])), SpaceAccount::Mixed);
        assert_eq!(acc.space(&sessions(&["t4", "t5", "t9"])), SpaceAccount::None);
        assert_eq!(acc.of("t3"), Some("side"));
        assert_eq!(acc.of("t4"), None);
        // No field at all (an older daemon): nothing to show.
        assert_eq!(Accounts::from_agents(&json!(null), None).of("t1"), None);
    }

    #[test]
    fn an_account_keeps_its_colour() {
        assert_eq!(account_hue("side"), account_hue("Side"));
        assert!(ACCOUNT_HUES.contains(&account_hue("anything-new")));
    }
}

/// What the tower is doing, in a word, and its colour (the orb's).
fn state_of(phase: &str) -> (&'static str, Hsla) {
    let (word, color) = match phase {
        "listening" | "armed" => ("Listening", CYAN),
        "speaking" | "announcing" => ("Speaking", PERI),
        "transcribing" => ("Understanding", SKY),
        "thinking" => ("Thinking", SKY),
        "starting" => ("Starting", SKY),
        "blocked" => ("Needs you", ROSE),
        "standby" => ("Standby", PALE),
        "off" => ("Off", GREY),
        _ => ("Ready", PALE),
    };
    (word, rgb(color).into())
}

fn status_word(status: &str) -> &str {
    match status {
        "done" | "idle" => "finished",
        "failed" => "failed",
        "error" => "hit an error",
        "exited" => "exited",
        other => other,
    }
}

/// "45s", "12m", "1h 05m", "2d 3h".
fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

enum Kind {
    Image,
    Video,
    Other,
}

fn file_kind(p: &Path) -> Kind {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "bmp" | "tiff" | "svg" => Kind::Image,
        "mov" | "mp4" | "m4v" | "webm" | "mkv" | "avi" => Kind::Video,
        _ => Kind::Other,
    }
}

/// Local time of a Unix-milliseconds instant.
fn local(ms: u64) -> libc::tm {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r fills the struct we own.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    }
}

/// "9:31 AM".
fn clock(ms: u64) -> String {
    let t = local(ms);
    let h = t.tm_hour % 12;
    format!("{}:{:02} {}", if h == 0 { 12 } else { h }, t.tm_min, if t.tm_hour < 12 { "AM" } else { "PM" })
}

fn day_label(ms: u64) -> String {
    let t = local(ms);
    let today = local(history::now_ms());
    let yesterday = local(history::now_ms().saturating_sub(86_400_000));
    let same = |a: &libc::tm, b: &libc::tm| a.tm_year == b.tm_year && a.tm_yday == b.tm_yday;
    if same(&t, &today) {
        "Today".into()
    } else if same(&t, &yesterday) {
        "Yesterday".into()
    } else {
        const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        format!("{} {} {}", DAYS[t.tm_wday as usize % 7], t.tm_mday, MONTHS[t.tm_mon as usize % 12])
    }
}

fn ago(ms: u64) -> String {
    let s = history::now_ms().saturating_sub(ms) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}
