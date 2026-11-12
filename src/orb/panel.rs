//! The tower's two windows, glass panels built with GPUI Kit (gpui-kit.com)
//! on macOS's own blur: the orb's floating panel (hover the orb to peek at
//! it, click to pin it) and the full window it opens.
//!
//! - **The floating panel** (`home`): what needs you and nothing else. The
//!   queue of decisions with one card open, the open spaces as chips, the
//!   tower's reply to what you last asked, and the input. "Full window" opens
//!   the other one; a space's chip opens it at that space.
//! - **The full window** (`window`): a sidebar of the spaces and, beside it,
//!   the queue with the missions, one space (`space`), the launcher
//!   (`launch`), the record (`record`), the screenshots (`gallery`, and
//!   `viewer` for one full size), the settings (`settings`, with `behaviors`
//!   and `phone`), or one session (`session`).
//! - **One model for both** (`crate::orb::model`, kept with the tower's
//!   state): the card picked, an answer on its way, a merge under way and a
//!   space's switches are the same in either window. Number keys answer the
//!   open card of the window that has the keyboard.
//! - **The input**: type in the pill at the bottom: Enter sends, Shift+Enter
//!   starts a new line. In a space's view it goes to that space's tower.
//!   While you talk, what you say appears in it as you say it (the voice
//!   daemon's partial transcripts), then the final text, which is sent as any
//!   voice request; after a draft (typed text, attachments) it is added to
//!   the draft, which Enter sends whole. With both windows open the one that
//!   last had the keyboard is written in.
//! - **Attachments, as in Claude Code and Codex**: paste a screenshot or a
//!   copied file, drop files on the window, or pick them with +; each becomes
//!   a token where the cursor is (`[Image #1]`, `[Video #2]`,
//!   `[File #3: notes.pdf]`). Deleting the token drops the file. The message
//!   reaches the brain as a spoken request does (`tower ask`), with the files'
//!   paths in token order.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use gpui::{
    AnyElement, App, AsyncApp, ClipboardEntry, Context, Entity, ExternalPaths, FontWeight, Hsla, Image, ImageFormat, KeyDownEvent, PathPromptOptions,
    Render, Subscription, Task, Window, div, prelude::*, px, rgb, rgba,
};
use gpui_kit::component::input::{InlineToken, InputContent, InputEvent, RopeExt as _, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, IconName, Sizable as _, Theme, ThemeMode, h_flex, v_flex};

use super::Tower;
use crate::history::{self, Entry, Job};

/// The floating panel's size, in points.
pub const W: f32 = 380.0;
pub const H: f32 = 640.0;
/// The full window's.
pub const FULL_W: f32 = 960.0;
pub const FULL_H: f32 = 640.0;
/// The full window's least size: its sidebar and queue column side by side, and room for the missions.
pub const FULL_MIN_W: f32 = 960.0;
pub const FULL_MIN_H: f32 = 520.0;
/// Their corners (the window is rounded to match, by `mac::Win::glass`).
pub const RADIUS: f32 = 28.0;

/// The glass: white on a cool blue tint over the blurred desktop.
struct Glass {
    tint: Hsla,
    text: Hsla,
    muted: Hsla,
    faint: Hsla,
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
        bubble_tower: rgba(0xffffff26).into(),
        edge: rgba(0xffffff4d).into(),
        pill: rgba(0xfffffff0).into(),
        pill_text: rgb(0x1d2330).into(),
        pill_muted: rgb(0x8a93a3).into(),
    }
}

/// The orb's own colours (its calm, listening and speaking looks), for
/// accents on the glass.
const PALE: u32 = 0xcce0ff;
const CYAN: u32 = 0x59d9ff;
const AMBER: u32 = 0xffcf70;
const ROSE: u32 = 0xff8f8f;
const NAVY: u32 = 0x16233a;
/// Accents for Claude accounts, one picked from each account's name (none of
/// them is a status colour): mint, orchid, peach, lime, lilac, coral, sand, aqua.
const ACCOUNT_HUES: [u32; 8] = [0x6eeaa8, 0xf0a6ff, 0xffb98a, 0xc8f06e, 0xc9b3ff, 0xff9fb2, 0xe8d9a0, 0x7fe8e0];

mod behaviors;
mod cli;
mod gallery;
mod home;
mod launch;
mod phone;
mod prchip;
mod projects;
mod questions;
mod record;
mod session;
mod settings;
mod space;
mod talk;
mod viewer;
mod window;

use crate::orb::shots::{self, Shot};

/// A screenshot's thumbnail: being made, made (its file), or it can't be.
#[derive(Clone)]
enum Thumb {
    Making,
    Ready(PathBuf),
    Failed,
}

/// What was done to the queue from a window, until the state says so too.
/// Kept with the tower's state: the same for the floating panel and the full
/// window, so a card answered in one is answered in the other.
#[derive(Default)]
pub struct Acts {
    /// Decisions answered whose answer is on its way to the daemon.
    sent: HashSet<String>,
    /// Decisions (and question sets) the user took off the queue.
    dismissed: HashSet<String>,
    /// PRs merged from here, until `prs.json` says so too.
    merged: HashSet<String>,
    /// Merge cards whose "merge all" is under way: after each merge the next PR asks.
    chain: HashSet<String>,
    /// Where each PR's merge stands (by the PR's key).
    steps: HashMap<String, prchip::Step>,
}

pub struct Panel {
    tower: Entity<Tower>,
    /// Opened by hovering the orb: it shows without the keyboard, and goes
    /// when the pointer leaves (the orb sees to that) unless it is clicked.
    peek: bool,
    /// The full window's own state (`None`: this is the floating panel).
    full: Option<window::FullUi>,
    /// The open card as last drawn: the one the number keys answer.
    shown_open: Option<String>,
    /// What that card offers (`home::face`), and since when it shows just
    /// that: a card that only now took the place of another (or whose
    /// answers changed under the eye) isn't answered by a key already on
    /// its way.
    shown_face: Option<(String, Instant)>,
    /// When a number key last answered: the next card is not answered by
    /// the same press held down, or a second one right behind it.
    keyed: Option<Instant>,
    /// Redraws while a receipt shows, so it goes on time.
    receipt_tick: Option<Task<()>>,
    /// The act whose undo is running (`tower undo`), and the one whose undo
    /// was refused, with why.
    undoing: Option<String>,
    undo_note: Option<(String, String)>,
    /// Pairing the Android app (in Settings).
    phone: phone::PhoneView,
    /// What sessions ask that the tower leaves to you.
    asks: questions::Asks,
    /// The Behaviors screen.
    behaviors: behaviors::BehaviorsUi,
    /// The session showing in the full window.
    session: session::SessionUi,
    /// The input's placeholder as set (it depends on the view and the voice).
    placeholder: String,
    /// The input the voice writes in this take: the message, or a question's "Other".
    voice_field: Option<Entity<TextareaState>>,
    /// A press-to-talk take is on (from the key going down to its text being
    /// sent, kept or dropped).
    take_open: bool,
    /// Enter was pressed during a take with a draft: the message goes once
    /// the take's text is in it.
    send_after_take: bool,
    /// Screenshots in history, newest first (the state's recent list joins them when shown).
    shots: Vec<Shot>,
    thumbs: HashMap<PathBuf, Thumb>,
    text: Entity<TextareaState>,
    /// The next attachment's number in this message.
    next: usize,
    sending: bool,
    /// Above the input, briefly: (it worked, what to say).
    status: Option<(bool, String)>,
    entries: Vec<Entry>,
    jobs: Vec<Job>,
    loaded: Option<SystemTime>,
    listening_since: Option<Instant>,
    /// What the voice put in the input (your draft, then its partial or
    /// final transcript), so it can be replaced or cleared, and what you
    /// typed over it never is.
    voice: Option<String>,
    /// When the voice first wrote in it, this take (Unix ms).
    voice_since: u64,
    /// The draft the take began on (typed text, attachment tokens): what you
    /// say goes after it, and it stays.
    base: InputContent,
    /// The last take dictated into a draft that the input took in (`dictated_ms`).
    held_seen: u64,
    /// You typed over this take: its final text isn't added.
    forgot: bool,
    /// The text a take sent, shown a moment before it goes (not a draft).
    parting: Option<String>,
    /// The daemon was told the input holds a draft (see `draft`).
    holding: bool,
    /// Herdr's spaces and their sessions (`herdr-app ls`), refreshed while the panel is open.
    live: Vec<crate::spaces::Live>,
    /// They were read at least once by this window.
    live_read: bool,
    /// The accounts sessions can run on (as `tower accounts` lists them, the
    /// default first), re-read while the panel is open; `None` until read.
    accounts: Option<Vec<String>>,
    _subs: Vec<Subscription>,
    _poll: Task<()>,
}

impl Panel {
    pub fn new(tower: Entity<Tower>, peek: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // GPUI Kit's own controls (the text field, its tokens) sit on the white
        // pill; its Root paints the theme's background behind every window,
        // which would hide the glass.
        Theme::change(ThemeMode::Light, Some(window), cx);
        Theme::update(cx, |t| t.tokens.background = gpui::transparent_black().into());
        // The queue is looked at: the tower looks at the PRs that wait for a merge again (one merged on GitHub goes).
        cx.background_executor().spawn(async { cli::tower(&["decisions", "--json"]) }).detach();
        let text = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 6).submit_on_enter(true).placeholder("Tell the tower…"));
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
                if this.owns_typing(window, cx) {
                    this.follow_voice(&tower, window, cx);
                }
                this.watch_sets(cx);
                this.follow_nav(cx);
                let listening = matches!(tower.read(cx).phase(), "listening" | "armed");
                if listening != this.listening_since.is_some() {
                    // (The placeholder follows in render.)
                    this.listening_since = listening.then(Instant::now);
                }
                cx.notify();
            }),
            // The window of the two that has the keyboard is the one dictation writes in.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    let me = window.window_handle();
                    this.tower.update(cx, |t, _| t.keyed = Some(me));
                }
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
                        this.tower.update(cx, |t, _| t.live = live.clone());
                        this.live = live;
                        this.live_read = true;
                        cx.notify();
                    })
                    .is_err()
                    {
                        break;
                    }
                }
                // So is the list of Claude accounts: every 30 s.
                if tick % 30 == 0 {
                    let list = cx.background_executor().spawn(async { accounts() }).await;
                    if this.update(cx, |this, cx| {
                        if this.accounts.as_ref() != Some(&list) {
                            this.accounts = Some(list);
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
                    // What the view showing reads by itself (a session's screen, the phones).
                    this.full_tick(cx);
                    // The listening timer and how long decisions have waited tick.
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        });
        text.update(cx, |s, cx| s.focus(window, cx));
        let held_seen = tower.read(cx).s("dictated_ms").as_u64().unwrap_or(0);
        let live = tower.read(cx).live.clone();
        // Opened to be typed in: the keyboard is this window's from the start.
        if !peek {
            let me = window.window_handle();
            tower.update(cx, |t, _| {
                t.pinned = Some(me);
                t.keyed = Some(me);
            });
        }
        Self {
            tower,
            peek,
            full: None,
            shown_open: None,
            shown_face: None,
            keyed: None,
            receipt_tick: None,
            undoing: None,
            undo_note: None,
            phone: Default::default(),
            asks: Default::default(),
            behaviors: Default::default(),
            session: Default::default(),
            placeholder: "Tell the tower…".into(),
            voice_field: None,
            take_open: false,
            send_after_take: false,
            shots: vec![],
            thumbs: HashMap::new(),
            text,
            next: 1,
            sending: false,
            status: None,
            entries: vec![],
            jobs: vec![],
            loaded: None,
            listening_since: None,
            voice: None,
            voice_since: 0,
            base: InputContent::new(""),
            // A take dictated before the panel opened isn't this draft's.
            held_seen,
            forgot: false,
            parting: None,
            holding: false,
            live,
            live_read: false,
            accounts: None,
            _subs: subs,
            _poll: poll,
        }
    }

    /// The full window's view (`focus`: it was opened with the keyboard).
    pub fn full(tower: Entity<Tower>, focus: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(tower, true, window, cx);
        this.peek = false;
        this.full = Some(window::FullUi::new(window, cx));
        // A goal the window was opened on is typed into the launcher.
        if let Some(goal) = this.tower.update(cx, |t, _| t.nav.goal.take()) {
            this.launch_with(&goal, window, cx);
        }
        this.follow_nav(cx);
        if focus {
            let me = window.window_handle();
            this.tower.update(cx, |t, _| t.keyed = Some(me));
        }
        this
    }

    /// What was done to the queue here or in the other window.
    fn acts<'a>(&self, cx: &'a App) -> &'a Acts {
        &self.tower.read(cx).acts
    }

    /// Changes it, for every window.
    fn act<R>(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Acts) -> R) -> R {
        self.tower.update(cx, |t, cx| {
            cx.notify();
            change(&mut t.acts)
        })
    }

    /// Dictation and the draft's hold are this window's: it last had the
    /// keyboard of the two, or it is the only one.
    fn owns_typing(&self, window: &Window, cx: &App) -> bool {
        let me = window.window_handle();
        let t = self.tower.read(cx);
        // A full window minimized into the orb hears nothing (what is said goes to the tower all the same).
        if t.full_hidden && t.full == Some(me) {
            return false;
        }
        t.keyed.is_none_or(|h| h == me || !cx.windows().contains(&h))
    }

    /// Mirrors what you say into the input as you talk: the daemon's
    /// `hearing` (partials, then the final text). Into an empty input, the
    /// daemon sends it, and it stays a moment and goes; after a draft (typed
    /// text, an image), it is added to the draft, which Enter sends whole.
    fn follow_voice(&mut self, tower: &Entity<Tower>, window: &mut Window, cx: &mut Context<Self>) {
        let (hearing, heard, held, held_ms, recording, transcribing) = {
            let t = tower.read(cx);
            let recording = t.s("mic").as_str() == Some("recording");
            let transcribing = t.s("transcribing").as_bool().unwrap_or(false);
            let heard = t.s("heard").as_str().unwrap_or("").to_string();
            // Only a request sent since the voice began writing here counts.
            let fresh = t.s("heard_ms").as_u64().unwrap_or(0) >= self.voice_since;
            let held_ms = t.s("dictated_ms").as_u64().unwrap_or(0);
            let held = if held_ms > self.held_seen { t.s("dictated").as_str().unwrap_or("").to_string() } else { String::new() };
            (t.s("hearing").as_str().unwrap_or("").to_string(), if fresh { heard } else { String::new() }, held, held_ms, recording, transcribing)
        };
        if recording {
            self.take_open = true;
        }
        if self.forgot && !held.is_empty() {
            // The end of the take you typed over.
            self.held_seen = held_ms;
            self.forgot = false;
            return;
        }
        // A take writes where it began: a question's "Other" if it had the
        // keyboard, else the message.
        let field = match (&self.voice, &self.voice_field) {
            (Some(_), Some(f)) => f.clone(),
            _ => self.asks.other_focused(window, cx).unwrap_or_else(|| self.text.clone()),
        };
        self.voice_field = Some(field.clone());
        let mut current = field.read(cx).value().to_string();
        // A new take while the last one's text still shows (it was sent):
        // that text is gone, not a draft to add to.
        if self.voice.is_none() && (!hearing.is_empty() || !held.is_empty()) && self.parting.as_deref() == Some(current.as_str()) {
            field.update(cx, |s, cx| s.set_value("", window, cx));
            self.parting = None;
            current.clear();
        }
        let base = self.base.text().to_string();
        let decision = voice_input(&hearing, &heard, &held, self.voice.as_deref(), &current, &base);
        let ended = matches!(decision, VoiceInput::Held(_) | VoiceInput::Sent(_) | VoiceInput::Restore);
        match decision {
            VoiceInput::Keep => {}
            VoiceInput::Forget => {
                self.voice = None;
                self.forgot = true;
            }
            VoiceInput::Show(text) => {
                if self.voice.is_none() {
                    self.voice_since = history::now_ms();
                    self.base = field.read(cx).content();
                    self.forgot = false;
                    self.parting = None;
                }
                self.put_voice(&field, &text, &current, window, cx);
                self.voice = Some(text);
            }
            VoiceInput::Held(text) => {
                if self.voice.is_none() {
                    self.base = field.read(cx).content();
                }
                self.held_seen = held_ms;
                self.put_voice(&field, &text, &current, window, cx);
                self.voice = None;
            }
            VoiceInput::Restore => {
                let base = std::mem::replace(&mut self.base, InputContent::new(""));
                let at = base.text().len();
                self.voice = None;
                self.set_content(&field, base, at, window, cx);
            }
            VoiceInput::Sent(text) => {
                if current != text {
                    self.put_voice(&field, &text, &current, window, cx);
                }
                self.voice = None;
                self.parting = Some(text.clone());
                cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(1200)).await;
                    let _ = this.update_in(cx, |this, window, cx| {
                        // Unless you started typing over it.
                        if this.voice.is_none() && this.text.read(cx).value().as_ref() == text.as_str() {
                            this.text.update(cx, |s, cx| s.set_value("", window, cx));
                        }
                        if this.parting.as_deref() == Some(text.as_str()) {
                            this.parting = None;
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        // Between takes there is no draft to keep apart: it's all in the input.
        if self.voice.is_none() {
            self.base = InputContent::new("");
        }
        // The take is over: its text was sent, kept or dropped (or nothing
        // more is coming). A message waiting for it goes now.
        let idle = !recording && !transcribing && hearing.is_empty() && self.voice.is_none();
        if self.take_open && (ended || idle) {
            self.take_open = false;
            if std::mem::take(&mut self.send_after_take) {
                self.send(window, cx);
            }
        }
    }

    /// Writes the voice's text in the input, with the draft's tokens (the
    /// text begins with the draft), the caret following its end.
    fn put_voice(&mut self, field: &Entity<TextareaState>, text: &str, current: &str, window: &mut Window, cx: &mut Context<Self>) {
        let at = caret_after(field.read(cx).cursor(), current.len(), text);
        let content = voice_content(&self.base, text);
        self.set_content(field, content, at, window, cx);
    }

    /// Sets the input and puts the caret at `at` (and the input scrolling
    /// with it), unless the input isn't focused: then it isn't taken.
    fn set_content(&mut self, field: &Entity<TextareaState>, content: InputContent, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let focused = gpui::Focusable::focus_handle(field.read(cx), cx).is_focused(window);
        field.update(cx, |s, cx| s.set_value(content, window, cx));
        if !focused {
            return;
        }
        // set_value puts the caret at the start. Moving it scrolls it into
        // view, which needs the new text laid out (wrapped, the pill grown):
        // so once now, and again once a frame has drawn it.
        fn place(state: &Entity<TextareaState>, at: usize, window: &mut Window, cx: &mut App) {
            state.update(cx, |s, cx| {
                if at <= s.text().len() {
                    let pos = s.text().offset_to_position(at);
                    s.set_cursor_position(pos, window, cx);
                }
            });
        }
        place(field, at, window, cx);
        let state = field.clone();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| place(&state, at, window, cx));
        });
    }

    /// Tells the daemon whether the input holds a draft of yours, so a take
    /// goes into it rather than to the brain alone.
    fn sync_hold(&mut self, window: &Window, cx: &App) {
        let value = self.text.read(cx).value();
        let own_voice = self.voice.is_some() && self.base.text().trim().is_empty();
        let parting = self.parting.as_deref() == Some(value.as_ref());
        // A question's "Other" with the keyboard: a take is an answer there, never a request.
        let answering = self.asks.other_focused(window, cx).is_some();
        let draft = self.owns_typing(window, cx) && (answering || (!value.trim().is_empty() && !own_voice && !parting));
        if draft != self.holding {
            self.holding = draft;
            crate::draft::hold(draft);
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
        self.shots = shots::load();
        if let Some(full) = self.full.as_mut() {
            full.record = record::load();
        }
        cx.notify();
    }

    /// Each file becomes a token at the cursor: `[Image #1]`, `[Video #2]`,
    /// `[File #3: name]`; in a question's "Other" when it has the keyboard.
    fn attach(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(field) = self.asks.other_focused(window, cx) {
            return self.attach_to(&field, paths, window, cx);
        }
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

    /// Files into another field (a question's "Other"), numbered after its own.
    fn attach_to(&mut self, field: &Entity<TextareaState>, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let mut n = field.read(cx).tokens().len() + 1;
        for p in paths.into_iter().filter(|p| p.is_file()) {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let label = match file_kind(&p) {
                Kind::Image => format!("[Image #{n}]"),
                Kind::Video => format!("[Video #{n}]"),
                Kind::Other => format!("[File #{n}: {name}]"),
            };
            n += 1;
            let id = p.display().to_string();
            field.update(cx, |s, cx| {
                if s.replace_with_token(InlineToken::new(id, label), window, cx).is_ok() {
                    s.insert(" ", window, cx);
                }
            });
        }
        field.update(cx, |s, cx| s.focus(window, cx));
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
        let value = self.text.read(cx).value().to_string();
        let into_message = self.voice_field.as_ref().is_none_or(|f| f == &self.text);
        match on_enter(
            self.parting.as_deref() == Some(value.as_str()),
            self.take_open,
            self.voice.is_some(),
            self.base.text().trim().is_empty() && into_message,
        ) {
            Enter::Send => {}
            Enter::Clear => {
                self.parting = None;
                self.text.update(cx, |s, cx| s.set_value("", window, cx));
                cx.notify();
                return;
            }
            Enter::Ignore => return,
            Enter::AfterTake => {
                self.send_after_take = true;
                self.status = Some((true, "Sending when you finish talking…".into()));
                cx.notify();
                return;
            }
        }
        // In New space it is the conversation that makes the space: the ticket, what to change, words for the planner.
        if self.full.is_some() && self.tower.read(cx).nav.scope == super::nav::Scope::New {
            let (text, files) = message_parts(&self.text.read(cx).content());
            let kept: Vec<String> = files.iter().map(|p| history::keep_attachment(p).display().to_string()).collect();
            let words = if kept.is_empty() { text } else { format!("{text}\n\nAttached: {}", kept.join(", ")) };
            match self.launch_say(&words, window, cx) {
                Ok(()) => {
                    self.text.update(cx, |s, cx| s.set_value("", window, cx));
                    self.next = 1;
                    self.status = None;
                }
                Err(why) if why.is_empty() => {}
                Err(why) => self.status = Some((false, why)),
            }
            cx.notify();
            return;
        }
        if !self.tower.read(cx).alive {
            self.status = Some((false, "The tower is off. Start it, then send.".into()));
            cx.notify();
            return;
        }
        let (text, files) = message_parts(&self.text.read(cx).content());
        // In a space's view it goes to that space's tower.
        let space = self.full.as_ref().and_then(|_| self.tower.read(cx).nav.scope.space().map(String::from));
        self.sending = true;
        self.status = None;
        cx.notify();
        let job = cx.background_executor().spawn(async move {
            let kept: Vec<String> = files.iter().map(|p| history::keep_attachment(p).display().to_string()).collect();
            let message = history::compose(&text, &kept);
            cli::ask(space.as_deref(), &message).map(|_| ())
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

    /// The window goes (Esc in the floating panel). The floating panel hands
    /// the keyboard back to the app that had it; the full window leaves it to
    /// a pinned floating panel if there is one.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let me = window.window_handle();
        window.remove_window();
        if self.full.is_some() {
            return super::full_closed(&self.tower, cx);
        }
        self.tower.update(cx, |t, _| {
            t.pinned = t.pinned.filter(|h| *h != me);
            t.keyed = t.keyed.filter(|h| *h != me);
            t.floating = t.floating.filter(|h| *h != me);
        });
        super::mac::give_back_focus();
    }

    /// Opened by hovering, and not clicked since.
    pub fn is_peek(&self) -> bool {
        self.peek
    }

    /// A peek that was clicked (it, or the orb): it takes the keyboard and
    /// stays until Esc or a click on the orb.
    pub fn pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.peek {
            return;
        }
        self.peek = false;
        cx.activate(true);
        if let Some(w) = super::mac::Win::of(window) {
            w.make_key();
        }
        let me = window.window_handle();
        self.tower.update(cx, |t, _| {
            t.pinned = Some(me);
            t.keyed = Some(me);
        });
        self.text.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// The full window at `to` (opened beside this one, or brought forward).
    fn show_full(&mut self, to: super::nav::Scope, window: &mut Window, cx: &mut Context<Self>) {
        let beside = super::mac::Win::of(window).map(|w| w.frame());
        super::open_full(&self.tower.clone(), Some(to), beside, None, true, true, cx);
    }

    /// A bare digit answers the open card of the view showing (1 is its
    /// first answer), before the message field takes it: while the message is
    /// empty and no other field has the keyboard. One press, one answer: a
    /// held key or a quick second press does nothing.
    fn number_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.full.is_some() && !self.tower.read(cx).nav.scope.answers() {
            return;
        }
        let m = &ev.keystroke.modifiers;
        let modified = m.control || m.alt || m.shift || m.platform || m.function;
        let empty = self.text.read(cx).value().is_empty();
        let other = self.asks.other_focused(window, cx).is_some() || self.editing_goal(window, cx);
        let Some(n) = home::answer_key(&ev.keystroke.key, modified, empty, other) else { return };
        let fresh = self.shown_face.as_ref().is_some_and(|(_, since)| !home::settled(since.elapsed().as_millis() as u64));
        if self.keyed.is_some_and(|at| ev.is_held || at.elapsed() < Duration::from_millis(home::KEY_GAP_MS)) || (fresh && self.shown_open.is_some()) {
            // Held, pressed twice, or meant for the card that was there a moment ago: no answer.
            cx.stop_propagation();
        } else if !ev.is_held && self.press_number(n, cx) {
            self.keyed = Some(Instant::now());
            cx.stop_propagation();
        }
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        // Closed with a draft in it: the voice sends again.
        if self.holding {
            crate::draft::hold(false);
        }
    }
}

impl Render for Panel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_hold(window, cx);
        let g = glass();
        // The input says where it goes: the tower, a space's own, or a change to the behaviors.
        let hint = if self.listening_since.is_some() { "Listening…".to_string() } else { self.input_hint(cx) };
        if hint != self.placeholder {
            self.placeholder = hint.clone();
            self.text.update(cx, |s, cx| s.set_placeholder(hint, window, cx));
        }
        let body: AnyElement = if self.full.is_some() {
            self.full_view(window, cx)
        } else {
            // The queue, the spaces, and above the input the tower's last reply.
            let all = self.decisions(cx);
            let missions = self.missions(&all, cx);
            let (header, list, foot) = (self.home_header(&all, &missions, cx), self.home_view(&all, &missions, cx), self.home_foot(None, cx));
            v_flex().size_full().child(header).child(list).children(foot).child(self.input(window, cx)).into_any_element()
        };
        div()
            .id("tower-panel")
            .size_full()
            .rounded(px(RADIUS))
            .bg(g.tint)
            .border_1()
            .border_color(g.edge)
            .text_color(g.text)
            // A click anywhere in a peek keeps it.
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| this.pin(window, cx)))
            // A bare digit answers the open card, before the message field takes it.
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| this.number_key(ev, window, cx)))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    // The full window goes into the orb as it is; the floating panel closes.
                    if this.full.is_some() {
                        super::minimize_full(&this.tower.clone(), window, cx);
                    } else {
                        this.close(window, cx);
                    }
                }
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.border_color(rgba(0xffffffcc)).bg(rgba(0x8fb0d84d)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| this.attach(paths.paths().to_vec(), window, cx)))
            .child(body)
    }
}

impl Panel {
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
        // The floating panel's has its margins; the full window's pane has its own.
        div()
            .when(self.full.is_none(), |el| el.px_4().pb_4().pt_2())
            .child(div().w_full().rounded(px(26.0)).bg(g.pill).shadow_lg().text_color(g.pill_text).child(pill))
            .into_any_element()
    }
}

/// What the brain is doing, in a word and its detail, and for how long (ms).
fn working_words(w: &crate::activity::Work, now: u64) -> (String, String, u64) {
    let elapsed = now.saturating_sub(w.since_ms);
    // A finished step stays a moment (a quick read is seen too), then it's thinking again.
    let thinking = (w.word == "Thinking" && w.detail.is_empty()) || (w.done_ms > 0 && now.saturating_sub(w.done_ms) > 3000);
    // Plain thinking reads the same for a while: let its words turn over.
    if thinking {
        (["Thinking", "Considering", "Working it out"][((elapsed / 3000) % 3) as usize].to_string(), String::new(), elapsed)
    } else {
        (w.word.clone(), w.detail.clone(), elapsed)
    }
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

/// The accounts sessions can run on, the default first (as the accounts mod
/// lists them, and `tower accounts` has them); none without the mod.
fn accounts() -> Vec<String> {
    crate::accounts::list().map(|l| l.into_iter().map(|a| a.name).collect()).unwrap_or_default()
}

/// The message's text and the files behind its tokens, in order.
fn message_parts(content: &InputContent) -> (String, Vec<PathBuf>) {
    let mut files: Vec<PathBuf> = vec![];
    for span in content.tokens() {
        let p = PathBuf::from(span.token().id().as_ref());
        if !files.contains(&p) {
            files.push(p);
        }
    }
    (content.text().trim().to_string(), files)
}

/// What Enter does.
#[derive(Debug, PartialEq)]
enum Enter {
    Send,
    /// The input shows what the voice just sent: it went already.
    Clear,
    /// A take into an empty input: the voice sends it itself.
    Ignore,
    /// A take after a draft: both go together once its text is in.
    AfterTake,
}

/// `parting`: the input holds what the voice just sent; `take_open`: a
/// press-to-talk take is on (or its text is still coming); `showing`: its
/// words are in the input; `voice_sends`: it began on an empty message.
fn on_enter(parting: bool, take_open: bool, showing: bool, voice_sends: bool) -> Enter {
    if parting {
        return Enter::Clear;
    }
    if !(take_open || showing) {
        return Enter::Send;
    }
    if showing && voice_sends { Enter::Ignore } else { Enter::AfterTake }
}

/// What the input should do about the voice.
#[derive(Debug, PartialEq)]
enum VoiceInput {
    Keep,
    /// You typed over it: leave the input alone from now on.
    Forget,
    /// Show this (your draft, then what you are saying so far).
    Show(String),
    /// The take went into your draft as this: keep it, for Enter to send.
    Held(String),
    /// The take came to nothing: put your draft back as it was.
    Restore,
    /// The take is done and sent as this: show it a moment, then clear.
    Sent(String),
}

/// `hearing`: the take so far (empty when none runs); `heard`: the request
/// sent since the voice began writing (empty if none); `held`: a take the
/// daemon kept for the draft, not yet taken in (empty if none); `shown`: what
/// the voice put in the input; `current`: what the input holds; `base`: the
/// draft the take began on.
fn voice_input(hearing: &str, heard: &str, held: &str, shown: Option<&str>, current: &str, base: &str) -> VoiceInput {
    if !hearing.is_empty() {
        return match shown {
            // A take begins: after whatever the input holds.
            None => VoiceInput::Show(joined(current, hearing)),
            Some(s) if current != s => VoiceInput::Forget,
            Some(_) => {
                let want = joined(base, hearing);
                if current == want { VoiceInput::Keep } else { VoiceInput::Show(want) }
            }
        };
    }
    match shown {
        // A short take, over before a partial came.
        None if !held.is_empty() => VoiceInput::Held(joined(current, held)),
        None => VoiceInput::Keep,
        Some(s) if current != s => VoiceInput::Forget,
        Some(_) if !held.is_empty() => VoiceInput::Held(joined(base, held)),
        // Sent: the daemon cleared `hearing` as it set `heard`.
        Some(_) if !heard.is_empty() && base.trim().is_empty() => VoiceInput::Sent(heard.to_string()),
        // Dropped (nothing heard, a hush, cancelled), or sent without the
        // draft: the draft is still yours.
        Some(_) => VoiceInput::Restore,
    }
}

/// What you said after the draft: `draft said`.
fn joined(draft: &str, said: &str) -> String {
    let draft = draft.trim_end();
    if draft.is_empty() { said.to_string() } else { format!("{draft} {said}") }
}

/// `text` (which begins with the draft) with the draft's attachment tokens.
fn voice_content(base: &InputContent, text: &str) -> InputContent {
    base.tokens().iter().fold(InputContent::new(text.to_string()), |c, span| {
        let kept = c.clone();
        c.with_token(span.range(), span.token().clone()).unwrap_or(kept)
    })
}

/// Where the caret goes when the voice rewrites the input: at the end of the
/// new text if it was at the end of the old (following the dictation), else
/// where you put it.
fn caret_after(cursor: usize, old_len: usize, new: &str) -> usize {
    if cursor >= old_len {
        return new.len();
    }
    let mut at = cursor.min(new.len());
    while !new.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod voice_tests {
    use super::*;

    #[test]
    fn the_caret_follows_the_dictation() {
        // An empty input, then each partial: the caret stays at the end.
        assert_eq!(caret_after(0, 0, "run the"), 7);
        assert_eq!(caret_after(7, 7, "run the tests"), 13);
        // You clicked into the middle: it stays there.
        assert_eq!(caret_after(3, 7, "run the tests"), 3);
        // Never inside a character, never past the end.
        assert_eq!(caret_after(2, 9, "né"), 1);
        assert_eq!(caret_after(5, 9, "ab"), 2);
    }

    #[test]
    fn what_you_say_shows_as_you_say_it() {
        use VoiceInput::*;
        // Partials replace each other in an empty input...
        assert_eq!(voice_input("run the", "", "", None, "", ""), Show("run the".into()));
        assert_eq!(voice_input("run the tests", "", "", Some("run the"), "run the", ""), Show("run the tests".into()));
        assert_eq!(voice_input("run the tests", "", "", Some("run the tests"), "run the tests", ""), Keep);
        // ...the final text arrives the same way, and once sent stays a moment.
        let t = "run the tests in eyzflow";
        assert_eq!(voice_input("", t, "", Some(t), t, ""), Sent(t.into()));
        // A take that came to nothing goes.
        assert_eq!(voice_input("", "", "", Some("um"), "um", ""), Restore);
        // The last partial differs from what was sent: the sent text wins.
        assert_eq!(voice_input("", "run the tests", "", Some("run the test"), "run the test", ""), Sent("run the tests".into()));
        // What you type over it is never overwritten.
        assert_eq!(voice_input("run the tests", "", "", Some("run the"), "run the, and", ""), Forget);
        assert_eq!(voice_input("", "run the tests", "", Some("run the tests"), "run the tests!", ""), Forget);
        // Nothing to do without a take.
        assert_eq!(voice_input("", "old", "", None, "", ""), Keep);
    }

    #[test]
    fn enter_never_sends_a_voice_message_twice_or_half() {
        use Enter::*;
        // Nothing to do with the voice: it sends.
        assert_eq!(on_enter(false, false, false, false), Send);
        // The voice's message still shows after it went: Enter only clears.
        assert_eq!(on_enter(true, false, false, true), Clear);
        // Talking into an empty input: the voice sends it, not Enter.
        assert_eq!(on_enter(false, true, true, true), Ignore);
        // Talking after a draft (or before the words show): it waits for them.
        assert_eq!(on_enter(false, true, true, false), AfterTake);
        assert_eq!(on_enter(false, true, false, true), AfterTake);
    }

    #[test]
    fn what_you_say_goes_after_your_draft() {
        use VoiceInput::*;
        // An image dropped in, then Control: the words follow the token...
        let img = "[Image #1] ";
        assert_eq!(voice_input("what is", "", "", None, img, ""), Show("[Image #1] what is".into()));
        let shown = "[Image #1] what is";
        assert_eq!(voice_input("what is this", "", "", Some(shown), shown, img), Show("[Image #1] what is this".into()));
        // ...the daemon keeps the take for the draft, which stays for Enter.
        let shown = "[Image #1] what is this";
        assert_eq!(voice_input("", "", "what is this?", Some(shown), shown, img), Held("[Image #1] what is this?".into()));
        // A take too short for a partial lands the same way.
        assert_eq!(voice_input("", "", "look", None, "my draft", ""), Held("my draft look".into()));
        // Nothing said: the draft is put back, the image with it.
        assert_eq!(voice_input("", "", "", Some("[Image #1] um"), "[Image #1] um", img), Restore);
        // Sent without the draft (an older daemon): the draft isn't cleared.
        assert_eq!(voice_input("", "what is this", "", Some(shown), shown, img), Restore);
    }

    #[test]
    fn the_draft_keeps_its_attachments() {
        let base = InputContent::new("[Image #1] ").with_token(0..10, InlineToken::new("/tmp/a.png", "[Image #1]")).unwrap();
        let c = voice_content(&base, "[Image #1] what is this");
        assert_eq!(c.text().as_ref(), "[Image #1] what is this");
        assert_eq!(c.tokens().len(), 1);
        assert_eq!(c.tokens()[0].range(), 0..10);
        // And Enter sends them together, as the tower prompt reads them.
        let (text, files) = message_parts(&c);
        assert_eq!(files, vec![PathBuf::from("/tmp/a.png")]);
        assert_eq!(history::compose(&text, &["/tmp/a.png".into()]), "[Image #1] what is this [attached: /tmp/a.png]");
        // No draft: plain text.
        assert!(voice_content(&InputContent::new(""), "hello").tokens().is_empty());
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;

    #[test]
    fn an_account_keeps_its_colour() {
        assert_eq!(account_hue("side"), account_hue("Side"));
        assert!(ACCOUNT_HUES.contains(&account_hue("anything-new")));
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
