//! The full window (`docs/quiet-tower.md`, "Steps 4 and 6"; W1 to W20 in the
//! design): about 960 × 640, the same glass as the floating panel, opened and
//! closed by its "Full window" (and closed by Esc).
//!
//! - **Moving it**: a press on the header strip of the view, or on the
//!   sidebar's top ("Tower" and the status line), drags the window. Where
//!   it is dropped is kept (`window.json`, beside `orb.json`), wholly on a
//!   screen, and it opens there next time.
//! - **The sidebar**: "Tower" and the status line; "All spaces" with the
//!   count of what is the user's; each open space with its mark (an amber
//!   count, an outline when muted, a pulse at work, a tick at rest) and a
//!   small line (its sessions, "standing", "ready to close"); "New space";
//!   the quiet spaces folded into one line; each Claude account's usage
//!   (`accounts`: a click opens the Accounts view); Record, Shots, Projects
//!   and Settings at the foot.
//! - **The main pane** shows what the sidebar picked (`crate::orb::nav`):
//!   every space (here: the queue beside the missions), one space (`space`),
//!   the launcher (`launch`), the record (`record`), the screenshots, the
//!   projects (`projects`), the
//!   settings (`settings`) with the behaviors and the phone (`Scope::Phone`
//!   opens Settings at its Phone card), one session (`session`).
//! - **The input** under the queue goes to the main tower; under a space, to
//!   that space's own where this tower has one (`tower ask --space`), else
//!   to the main tower with the space named (`cli::ask`), and it says which.
//! - **What it sets** goes through the `tower` CLI (`cli`), off the UI
//!   thread; a command that fails, or isn't there yet, says so in one line
//!   where it was asked (`run`, `said`).
//!
//! It is a `Panel` like the floating one, with `full` set: the queue, the
//! cards and the number keys are the same code, on the same model.

use std::collections::{HashMap, HashSet};

use gpui::{AnyElement, App, Context, FontWeight, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::{h_flex, v_flex};

use super::home::{LINE, SOFT, bay_label, cstat, mbtn, minimize_mark, sign, tg};
use super::{CYAN, Panel, cli, glass, launch, projects, record, space};
use crate::history;
use crate::orb::model::{self, Decision, Missions};
use crate::orb::nav::{Scope, Side};

/// The sidebar's width, in points, until the user drags it (`widths`).
const SIDE_W: f32 = 236.0;
/// The least and the most the sidebar is dragged to.
const SIDE_MIN_W: f32 = 200.0;
const SIDE_MAX_W: f32 = 360.0;
/// The least the conversation's column is dragged to.
const TALK_MIN_W: f32 = 280.0;
/// The pane's padding beside its content.
const PANE_PAD: f32 = 36.0;
/// The queue's column beside the missions (of the 688 the pane has).
const QUEUE_W: f32 = 350.0;
/// The pane is laid out in two columns from this width (All spaces' queue
/// beside its missions, Settings' cards two by two), else in one.
pub(super) const TWO_COLS_W: f32 = 700.0;
/// The least the pane keeps beside the conversation's column.
const PANE_MIN_W: f32 = 420.0;

/// The conversation's column, for a window `win_w` wide: about two fifths
/// of what the sidebar leaves (300 to 460), never so wide the pane gets less
/// than `PANE_MIN_W`, and never under 280.
pub(super) fn talk_width(win_w: f32) -> f32 {
    talk_default(win_w, SIDE_W)
}

fn talk_default(win_w: f32, side: f32) -> f32 {
    let room = win_w - side;
    (room * 0.38).clamp(300.0, 460.0).min(room - PANE_MIN_W - PANE_PAD).max(TALK_MIN_W)
}

/// The sidebar's and the conversation column's widths in a window `win_w`
/// wide, from what the user dragged them to (`None`: never, or reset):
/// the sidebar 200 to 360, the column from 280 to what leaves the pane its
/// 420. A window too narrow for all three takes it from the sidebar first,
/// then the column, never under their least.
pub(super) fn widths(win_w: f32, side: Option<f32>, talk: Option<f32>) -> (f32, f32) {
    let side = side.unwrap_or(SIDE_W).clamp(SIDE_MIN_W, SIDE_MAX_W).min(win_w - TALK_MIN_W - PANE_MIN_W - PANE_PAD).max(SIDE_MIN_W);
    let talk = match talk {
        Some(t) => t.min(win_w - side - PANE_MIN_W - PANE_PAD).max(TALK_MIN_W),
        None => talk_default(win_w, side),
    };
    (side, talk)
}

/// Which border is being dragged: the sidebar's (between it and the
/// conversation, or the pane in a view without one) or the conversation's
/// (between it and the pane).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Edge {
    Side,
    Talk,
}

/// A column's border being dragged: where the press was (x, in the
/// window) and the widths then.
#[derive(Debug, Clone, Copy)]
pub(super) struct Resize {
    edge: Edge,
    x0: f32,
    side0: f32,
    talk0: f32,
    /// The view has a conversation: the sidebar's border moves it along (its right border stays put).
    with_talk: bool,
    moved: bool,
    /// Where the conversation was read (`ChatAt`): kept there as its text wraps anew.
    at: ChatAt,
}

/// Where the conversation's column is read: at its end, or at a message
/// (its top row): a new width wraps the text anew, and it stays there
/// rather than at the same distance from the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum ChatAt {
    End,
    Item(usize),
}

impl ChatAt {
    fn of(s: &gpui::ScrollHandle) -> Self {
        if f32::from(-s.offset().y) >= f32::from(s.max_offset().y) - 4.0 { ChatAt::End } else { ChatAt::Item(s.logical_scroll_top().0) }
    }

    fn keep(self, s: &gpui::ScrollHandle) {
        match self {
            ChatAt::End => s.scroll_to_bottom(),
            ChatAt::Item(ix) => s.scroll_to_top_of_item(ix),
        }
    }
}

/// The widths `r` gives with the pointer at `x` in a window `win_w` wide
/// (both dragged, as `FullUi::side_w`/`talk_w`, kept within `widths`). The
/// conversation's border moves its column's right; the sidebar's moves the
/// border between the two, the column's right staying where it is.
pub(super) fn dragged(r: &Resize, x: f32, win_w: f32) -> (f32, f32) {
    let dx = x - r.x0;
    match r.edge {
        Edge::Talk => (r.side0, widths(win_w, Some(r.side0), Some(r.talk0 + dx)).1),
        Edge::Side => {
            let (side, _) = widths(win_w, Some(r.side0 + dx), Some(r.talk0));
            if !r.with_talk {
                return (side, r.talk0);
            }
            // The column gives (or takes) what the sidebar took, its right border staying, never under its least.
            let talk = (r.talk0 - (side - r.side0)).max(TALK_MIN_W);
            let side = r.side0 + r.talk0 - talk;
            let (side, talk) = widths(win_w, Some(side), Some(talk));
            (side, talk)
        }
    }
}

/// What the full window keeps of its own.
pub(super) struct FullUi {
    /// The view the window last settled on, to tell when it moves.
    shown: Option<Scope>,
    /// The launcher.
    pub launch: launch::LaunchUi,
    /// The projects' page: the form while one is added or changed.
    pub projects: projects::ProjectsUi,
    /// A space's goal while it is being changed.
    pub edit: Option<space::GoalEdit>,
    /// A Resolve waiting for the user's word, with what it would end.
    pub resolve: Option<space::ResolveAsk>,
    /// What a command said, by where it shows (`tell`, `space:<name>`,
    /// `launch`, …): whether it worked, and the line.
    pub notes: HashMap<String, (bool, String)>,
    /// The commands under way, by the same places.
    pub busy: HashSet<String>,
    /// The history as the record's rows, newest first.
    pub record: Vec<record::Row>,
    /// How much it tells, as just chosen here, until the state says it too.
    pub tell: Option<String>,
    /// What it says out loud (`off`, `decisions`, `replies`), as just chosen here.
    pub voice: Option<String>,
    /// Auto-merge of low-risk PRs into dev, as just switched here, until the state says it too.
    pub auto_merge: Option<bool>,
    /// Task lists for the sessions the tower starts, as just switched here (until config.toml says it too).
    pub todo_tools: Option<bool>,
    /// The scoring preset just picked here (until config.toml says it too).
    pub scoring: Option<String>,
    /// The window is being moved by its header or the sidebar's top.
    pub drag: Option<Drag>,
    /// The widths the user dragged the sidebar and the conversation's
    /// column to (`None`: the default), kept in `window.json`; and the drag
    /// under way.
    pub side_w: Option<f32>,
    pub chat_w: Option<f32>,
    pub resize: Option<Resize>,
    /// Whether a space can have a tower of its own to talk to (this tower's
    /// `ask` takes `--space`); `None` until asked. Without one, what is
    /// typed in a space's view goes to the main tower with the space named.
    pub space_tower: Option<bool>,
    /// The brains' model and effort as just chosen here, until the state says them too.
    pub model: Option<(Option<String>, Option<String>)>,
    /// Settings' pane, scrolled to its Phone card when `Scope::Phone` is asked for.
    pub settings_scroll: gpui::ScrollHandle,
    /// New space's pane: a new line in the chat about a plan scrolls it to its end.
    pub launch_scroll: gpui::ScrollHandle,
    pub phone_anchor: gpui::ScrollAnchor,
    /// Settings was asked for at its Phone card: scrolled there once it is
    /// drawn, and again until this moment (the cards above it grow as what
    /// they show arrives).
    pub to_phone: Option<std::time::Instant>,
    /// The thread's messages shown whole (by `space::thread_key`), the
    /// spaces whose earlier messages show too, and the one just copied.
    pub thread_open: HashSet<String>,
    /// The briefs opened whole here: a session's (its address), a space's history (`space:<name>`).
    pub briefs_open: HashSet<String>,
    /// Sessions whose task list shows whole (collapsed to a line otherwise).
    pub tasks_open: HashSet<String>,
    /// The spaces whose Goal card shows its earlier versions.
    pub goals_open: HashSet<String>,
    /// How wide the pane is, in points, as last drawn (the window less the
    /// sidebar, the conversation's column and the pane's padding): views lay
    /// out to it (two columns, or one).
    pub pane_w: f32,
    /// How wide the conversation's column is (0: none in this view).
    pub talk_w: f32,
    /// The conversation column: its scroll, and what it last showed (its
    /// view, how many messages, the latest's time), to go to its end on a new one.
    pub chat_scroll: gpui::ScrollHandle,
    pub chat_seen: Option<(Option<String>, usize, u64)>,
    /// The message a click on its question or its answer went to (by its
    /// `at`), and when: it lights up a moment, so the eye finds it.
    pub chat_flash: Option<(u64, std::time::Instant)>,
    /// What each of the column's rows is, as last drawn (a message's `at`,
    /// its words, and where a click on it goes: Some(true) a question to
    /// its answer, Some(false) an answer to its question; 0 and none for a
    /// label): for clicking one by its words without the pointer
    /// (`HERDER_TOWER_ORB_CLICK`).
    pub chat_rows: Vec<(u64, String, Option<bool>)>,
    /// How wide a thread message's text is, in points, as last drawn.
    pub text_w: f32,
    pub thread_earlier: HashSet<String>,
    pub copied: Option<String>,
    /// Kept while the window is: its size and spot are saved as the user resizes it.
    pub _resizing: Option<gpui::Subscription>,
}

impl FullUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Panel>) -> Self {
        let scroll = gpui::ScrollHandle::new();
        let cols = crate::orb::saved_columns();
        Self {
            shown: None,
            launch: launch::LaunchUi::new(),
            projects: projects::ProjectsUi::default(),
            edit: None,
            resolve: None,
            notes: HashMap::new(),
            busy: HashSet::new(),
            record: vec![],
            tell: None,
            auto_merge: None,
            todo_tools: None,
            scoring: None,
            voice: None,
            drag: None,
            side_w: cols.0,
            chat_w: cols.1,
            resize: None,
            space_tower: None,
            model: None,
            settings_scroll: scroll.clone(),
            launch_scroll: gpui::ScrollHandle::new(),
            phone_anchor: gpui::ScrollAnchor::for_handle(scroll),
            to_phone: None,
            thread_open: HashSet::new(),
            briefs_open: HashSet::new(),
            // `HERDER_TOWER_ORB_TASKS_OPEN=<address>,…`: those sessions' task lists open from the start (a screenshot).
            tasks_open: std::env::var("HERDER_TOWER_ORB_TASKS_OPEN").ok().map(|v| v.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect()).unwrap_or_default(),
            goals_open: HashSet::new(),
            pane_w: 900.0,
            talk_w: 0.0,
            chat_scroll: gpui::ScrollHandle::new(),
            chat_seen: None,
            chat_flash: None,
            chat_rows: vec![],
            text_w: 600.0,
            thread_earlier: HashSet::new(),
            copied: None,
            _resizing: Some(cx.observe_window_bounds(window, |this, window, _| this.resized(window))),
        }
    }
}

/// A press on the window's header or the sidebar's top: where the pointer
/// was. It becomes a move once the pointer travels; until then it is a click.
#[derive(Debug, Clone, Copy)]
pub(super) struct Drag {
    mouse: (f64, f64),
}

/// A press that moves less than this is a click, not a drag (points).
const DRAG_SLOP: f64 = 4.0;

impl Panel {
    /// A press where the window can be taken by: it may become a move.
    fn drag_start(&mut self, _window: &Window) {
        if let Some(full) = self.full.as_mut() {
            full.drag = Some(Drag { mouse: crate::orb::mac::mouse() });
        }
    }

    /// The pointer moved with the button down: past a few points, macOS
    /// moves the window from here (as any window's title bar does), so a
    /// click on a button in the header stays a click. Where it is put is
    /// kept by `resized` (a move changes its bounds too).
    fn drag_moved(&mut self, window: &Window) {
        let Some(d) = self.full.as_ref().and_then(|f| f.drag) else { return };
        let m = crate::orb::mac::mouse();
        if (m.0 - d.mouse.0).hypot(m.1 - d.mouse.1) < DRAG_SLOP {
            return;
        }
        if let Some(full) = self.full.as_mut() {
            full.drag = None;
        }
        window.start_window_move();
    }

    /// Let go without moving: it was a click.
    fn drag_end(&mut self, _window: &Window) {
        if let Some(full) = self.full.as_mut() {
            full.drag = None;
        }
    }

    /// A press on a column's border: a drag of it starts (a double click
    /// puts that border back where it was at first, and keeps that).
    fn resize_start(&mut self, edge: Edge, e: &gpui::MouseDownEvent, window: &Window, cx: &mut Context<Self>) {
        let win_w = f32::from(window.viewport_size().width);
        let Some(f) = self.full.as_mut() else { return };
        let at = ChatAt::of(&f.chat_scroll);
        if e.click_count >= 2 {
            f.resize = None;
            at.keep(&f.chat_scroll);
            match edge {
                Edge::Side => f.side_w = None,
                Edge::Talk => f.chat_w = None,
            }
            crate::orb::save_columns(f.side_w, f.chat_w);
            cx.notify();
            return;
        }
        let (side0, talk0) = widths(win_w, f.side_w, f.chat_w);
        let with_talk = f.talk_w > 0.0;
        f.resize = Some(Resize { edge, x0: f32::from(e.position.x), side0, talk0, with_talk, moved: false, at });
        cx.notify();
    }

    /// The pointer moved while a border is held: the columns follow it.
    /// A move with the button up (let go outside the window) ends it.
    fn resize_moved(&mut self, e: &gpui::MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(r) = self.full.as_ref().and_then(|f| f.resize) else { return };
        if e.pressed_button != Some(gpui::MouseButton::Left) {
            self.resize_end(cx);
            return;
        }
        let (side, talk) = dragged(&r, f32::from(e.position.x), f32::from(window.viewport_size().width));
        if let Some(f) = self.full.as_mut() {
            f.side_w = Some(side);
            if r.with_talk {
                f.chat_w = Some(talk);
            }
            if let Some(r) = f.resize.as_mut() {
                r.moved = true;
            }
            r.at.keep(&f.chat_scroll);
        }
        cx.notify();
    }

    /// Let go of a border: the widths it left are kept for next time.
    fn resize_end(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.full.as_mut() else { return };
        let Some(r) = f.resize.take() else { return };
        if r.moved {
            crate::orb::save_columns(f.side_w, f.chat_w);
        }
        cx.notify();
    }

    /// A column's border, `x` from the window's left: an 8-point strip the
    /// pointer drags it by (a resize cursor; a line shows on hover and while dragged).
    fn border_handle(&mut self, edge: Edge, x: f32, cx: &mut Context<Self>) -> AnyElement {
        let held = self.full.as_ref().and_then(|f| f.resize).is_some_and(|r| r.edge == edge);
        let id = match edge {
            Edge::Side => "side-border",
            Edge::Talk => "talk-border",
        };
        div()
            .id(id)
            .group(id)
            .absolute()
            .top_0()
            .h_full()
            .left(px(x - 4.0))
            .w(px(8.0))
            .flex()
            .justify_center()
            .cursor(gpui::CursorStyle::ResizeColumn)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, e: &gpui::MouseDownEvent, window, cx| {
                    // Not a move of the window, nor a click on what is under it.
                    cx.stop_propagation();
                    this.resize_start(edge, e, window, cx);
                }),
            )
            .child(div().w(px(2.0)).h_full().when(held, |el| el.bg(rgba(0xffffff40))).when(!held, |el| el.group_hover(id, |s| s.bg(rgba(0xffffff26)))))
            .into_any_element()
    }

    /// The user moved or resized it: where it is and its size are kept for next time.
    pub(super) fn resized(&mut self, window: &Window) {
        if let Some(win) = crate::orb::mac::Win::of(window) {
            let f = win.frame();
            crate::orb::save_window_frame(f.x, f.y, f.w, f.h);
        }
    }

    /// Where the window can be taken by: `el`, with a press on it starting a move.
    fn grip(&mut self, id: &'static str, el: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id(id)
            .w_full()
            // The strip above it too, as a title bar's.
            .pt(px(20.0))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _: &gpui::MouseDownEvent, window, _| this.drag_start(window)))
            .child(el)
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// The pieces every view of the pane is made of.

/// A view's head: its title, the lines under it, and what sits at its right
/// (a switch, a way back, the button to a new space).
pub(super) fn whead(title: &str, sub: Vec<AnyElement>, right: Option<AnyElement>) -> AnyElement {
    h_flex()
        .w_full()
        .flex_wrap()
        .gap_x(px(16.0))
        .gap_y(px(10.0))
        .items_start()
        .child(
            v_flex()
                .flex_1()
                .min_w(px(240.0))
                .gap(px(2.0))
                .child(div().text_size(px(20.0)).line_height(px(24.0)).font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
                .child(v_flex().gap(px(1.0)).text_size(px(13.0)).line_height(px(18.0)).text_color(glass().muted).children(sub)),
        )
        .children(right.map(|r| div().flex_none().child(r)))
        .into_any_element()
}

/// `Goal …`, `Done when …`: a word in white, then what it is.
pub(super) fn named(word: &str, text: &str) -> AnyElement {
    h_flex()
        .items_start()
        .gap(px(5.0))
        .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).text_color(glass().text).child(word.to_string()))
        .child(div().min_w_0().child(text.to_string()))
        .into_any_element()
}

/// A line that says what a switch does, or what just happened.
pub(super) fn eff(text: impl Into<SharedString>) -> AnyElement {
    div()
        .w_full()
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(10.0))
        .bg(rgba(SOFT))
        .text_size(px(13.0))
        .line_height(px(18.0))
        .text_color(glass().muted)
        .child(text.into())
        .into_any_element()
}

/// A section: its heading and what is under it.
pub(super) fn bay(label: &str, items: Vec<AnyElement>) -> AnyElement {
    v_flex().w_full().gap(px(10.0)).child(bay_label(label)).children(items).into_any_element()
}

/// The mark column of a row: 22 points, whatever is in it.
pub(super) fn mark_box(child: Option<AnyElement>) -> gpui::Div {
    div().size(px(22.0)).flex_none().flex().items_center().justify_center().children(child)
}

/// A row of a list (a mission, a request, a session): its mark, its name
/// with what stands at the right of it, and the lines under.
pub(super) fn lrow(mark: AnyElement, name: AnyElement, right: Option<AnyElement>, under: Vec<AnyElement>) -> gpui::Div {
    h_flex()
        .w_full()
        .gap(px(10.0))
        .items_start()
        .p(px(8.0))
        .rounded(px(12.0))
        .child(mark)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(px(1.0))
                .child(h_flex().gap(px(10.0)).items_center().min_h(px(22.0)).child(div().flex_1().min_w_0().child(name)).children(right.map(|r| div().flex_none().child(r))))
                .children(under),
        )
}

/// A row's name.
pub(super) fn nm(text: &str) -> gpui::Div {
    div().text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).child(text.to_string())
}

/// What stands at a row's right: how far it is.
pub(super) fn pg(text: &str, ok: bool) -> AnyElement {
    div().text_size(px(12.0)).text_color(if ok { rgb(CYAN).into() } else { glass().faint }).child(text.to_string()).into_any_element()
}

// ---------------------------------------------------------------------------
// Moving about, and what it sets.

impl Panel {
    /// To another view of the full window.
    pub(super) fn nav_to(&mut self, to: Scope, cx: &mut Context<Self>) {
        self.tower.update(cx, |t, cx| {
            t.nav.go(to);
            cx.notify();
        });
        cx.notify();
    }

    /// Where the input's words go, as its placeholder says.
    pub(super) fn input_hint(&self, cx: &Context<Self>) -> String {
        if self.full.is_none() {
            return "Tell the tower…".into();
        }
        match &self.tower.read(cx).nav.scope {
            Scope::New => self.new_hint(cx),
            // The conversation column's head says which tower it is; its input is narrow.
            Scope::Space(_) if self.full.as_ref().is_some_and(|f| f.talk_w < 360.0) => "Write here…".into(),
            Scope::Space(_) => "Write to the tower…".into(),
            Scope::Behaviors => "Tell the tower what to change…".into(),
            _ => "Tell the tower…".into(),
        }
    }

    /// A space can have a tower of its own to talk to (see `FullUi::space_tower`).
    pub(super) fn space_tower(&self) -> bool {
        self.full.as_ref().is_some_and(|f| f.space_tower == Some(true))
    }

    /// The space called `name` has a tower of its own that runs: what is
    /// typed in its view goes there (else to the main tower, the space named).
    pub(super) fn space_tower_of(&self, name: &str, cx: &App) -> bool {
        let t = self.tower.read(cx);
        self.space_tower() && t.alive && t.state.as_ref().is_some_and(|s| model::has_own_tower(s, name))
    }

    /// Asks this tower, once, whether its `ask` reaches a space's own tower.
    fn learn_space_tower(&mut self, cx: &mut Context<Self>) {
        if self.full.as_ref().is_none_or(|f| f.space_tower.is_some()) {
            return;
        }
        let task = cx.background_executor().spawn(async { cli::tower(&["ask", "--help"]) });
        cx.spawn(async move |this, cx| {
            let help = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(full) = this.full.as_mut() {
                    full.space_tower = Some(help.is_ok_and(|text| text.contains("--space")));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The window follows where it was sent (from here, or by a chip of the
    /// floating panel): a view that is left lets go of what it held, one
    /// that opens reads what it shows. A space that is gone falls back to
    /// every space.
    pub(super) fn follow_nav(&mut self, cx: &mut Context<Self>) {
        if self.full.is_none() {
            return;
        }
        let all = self.decisions(cx);
        let missions = self.missions(&all, cx);
        let names: Vec<&str> = missions.open.iter().map(|m| m.name.as_str()).collect();
        let now = history::now_ms();
        // (Not before Herdr's spaces were read: a window that has just opened knows none.)
        let known = !names.is_empty() || self.live_read;
        let (scope, to_phone) = self.tower.update(cx, |t, _| {
            if known {
                t.nav.settle(&names, now);
            }
            // The phone is a card of Settings: asked for, Settings opens there.
            let to_phone = t.nav.scope == Scope::Phone;
            if to_phone {
                t.nav.go(Scope::Settings);
            }
            (t.nav.scope.clone(), to_phone)
        });
        let Some(full) = self.full.as_mut() else { return };
        if to_phone {
            full.to_phone = Some(std::time::Instant::now() + std::time::Duration::from_millis(1500));
        }
        if full.shown.as_ref() == Some(&scope) {
            if to_phone {
                cx.notify();
            }
            return;
        }
        let from = full.shown.replace(scope.clone());
        // What a command said belongs to the view it was asked in.
        full.notes.clear();
        full.edit = None;
        if from == Some(Scope::Settings) {
            self.phone_leave();
        }
        self.watch_session(if let Scope::Session { addr, .. } = &scope { Some(addr.as_str()) } else { None }, cx);
        match scope {
            Scope::Behaviors => self.open_behaviors(cx),
            Scope::Phone | Scope::Settings => {
                self.phone_tick(cx);
                self.read_setups(cx);
                // `HERDER_TOWER_ORB_PAIR=open` (looking at it with test data): the pairing box opens, once.
                if to_phone && super::phone::pair_from_start() {
                    self.phone_pair(cx);
                }
            }
            Scope::New | Scope::Projects => self.read_setups(cx),
            Scope::Space(_) => self.learn_space_tower(cx),
            _ => {}
        }
        cx.notify();
    }

    /// Each second: what the view showing reads by itself.
    pub(super) fn full_tick(&mut self, cx: &mut Context<Self>) {
        if self.full.is_none() {
            return;
        }
        self.follow_nav(cx);
        // What changed in a new space in draft is kept, whatever view shows.
        self.drafts_tick(cx);
        match self.tower.read(cx).nav.scope {
            Scope::Phone | Scope::Settings => self.phone_tick(cx),
            Scope::Behaviors => self.behaviors_tick(cx),
            Scope::Session { .. } => self.session_tick(cx),
            _ => {}
        }
    }

    /// Runs `tower <args>` off the UI thread. `place` is where it was asked:
    /// one command at a time runs for a place, and if it fails its error
    /// shows there (`said`). `then` gets what it printed, or that error.
    pub(super) fn run(
        &mut self,
        place: &str,
        args: Vec<String>,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &Result<String, String>, &mut Context<Self>) + 'static,
    ) {
        self.run_or(place, args, cx, then, |_| {});
    }

    /// The same; `gone` gets what it printed instead if the window closed
    /// before it was done (for what must not be lost with it).
    pub(super) fn run_or(
        &mut self,
        place: &str,
        args: Vec<String>,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &Result<String, String>, &mut Context<Self>) + 'static,
        gone: impl FnOnce(&Result<String, String>) + 'static,
    ) {
        let Some(full) = self.full.as_mut() else { return };
        if !full.busy.insert(place.to_string()) {
            return;
        }
        full.notes.remove(place);
        let place = place.to_string();
        let task = cx.background_executor().spawn(async move { cli::tower(&args) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let here = this.update(cx, |this, cx| {
                if let Some(full) = this.full.as_mut() {
                    full.busy.remove(&place);
                    if let Err(e) = &result {
                        full.notes.insert(place.clone(), (false, e.clone()));
                    }
                }
                then(this, &result, cx);
                cx.notify();
            });
            if here.is_err() {
                gone(&result);
            }
        })
        .detach();
        cx.notify();
    }

    /// A command for `place` is under way.
    pub(super) fn running(&self, place: &str) -> bool {
        self.full.as_ref().is_some_and(|f| f.busy.contains(place))
    }

    /// What the last command for `place` said, if it has something to say.
    pub(super) fn said(&self, place: &str) -> Option<AnyElement> {
        let (ok, text) = self.full.as_ref()?.notes.get(place)?;
        Some(cstat(text.clone(), !ok))
    }

}

// ---------------------------------------------------------------------------
// Drawing it.

/// A scope as a word, for the element that scrolls it (each view keeps its
/// own place).
fn scope_key(scope: &Scope) -> String {
    match scope {
        Scope::All => "all".into(),
        Scope::Space(name) => format!("space-{name}"),
        Scope::New => "new".into(),
        Scope::Record => "record".into(),
        Scope::Shots => "shots".into(),
        Scope::Projects => "projects".into(),
        Scope::Settings => "settings".into(),
        Scope::Behaviors => "behaviors".into(),
        Scope::Phone => "phone".into(),
        Scope::Session { addr, .. } => format!("session-{addr}"),
        Scope::Accounts => "accounts".into(),
    }
}

impl Panel {
    /// The whole window: the sidebar, and the pane for the view showing.
    pub(super) fn full_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let all = self.decisions(cx);
        let missions = self.missions(&all, cx);
        let (scope, notice, act) = {
            let t = self.tower.read(cx);
            // What a launch from here opened, as the tower holds it now: undone from where it is said.
            let act = t.nav.notice_act().and_then(|id| t.state.as_ref()?["acts"].as_array()?.iter().find(|a| a["id"].as_str() == Some(id)).and_then(super::home::ActShown::of_view));
            (t.nav.scope.clone(), t.nav.notice().map(String::from), act)
        };
        // The widths, from the window's as it is now: the conversation's
        // column (All spaces, a space) and what is left for the pane, which
        // every view lays itself out to (`FullUi::pane_w`).
        // The sidebar and the column are as wide as the user dragged them (`widths`).
        let win_w = f32::from(window.viewport_size().width);
        let (side_w, chat_w) = self.full.as_ref().map(|f| widths(win_w, f.side_w, f.chat_w)).unwrap_or((SIDE_W, talk_width(win_w)));
        let talk_w = matches!(scope, Scope::All | Scope::Space(_) | Scope::New).then_some(chat_w);
        let pane_w = win_w - side_w - talk_w.unwrap_or(0.0) - PANE_PAD;
        if let Some(f) = self.full.as_mut() {
            f.pane_w = pane_w;
            f.talk_w = talk_w.unwrap_or(0.0);
            // What a thread message's text gets across: the pane, less the row's padding and its colour bar.
            f.text_w = pane_w - 24.0 - 13.0 - 2.0;
        }
        let undo = act.map(|a| self.receipt_undo(&a, cx)).unwrap_or_default();
        // The view: its head, its body, whose reply shows above the input
        // (`None`: no reply here), and whether there is an input.
        let (head, items, reply, input): (AnyElement, Vec<AnyElement>, Option<Option<String>>, bool) = match &scope {
            Scope::All => {
                let (h, b) = self.all_view(&all, &missions, cx);
                (h, b, Some(None), true)
            }
            Scope::Space(name) => {
                let (h, b) = self.space_view(name, &all, &missions, cx);
                (h, b, Some(Some(name.clone())), true)
            }
            Scope::New => {
                let (h, b) = self.launch_view(&missions, window, cx);
                (h, b, None, false)
            }
            Scope::Record => {
                let (h, b) = self.record_view(cx);
                (h, b, None, false)
            }
            Scope::Shots => {
                let (h, b) = self.shots_pane(cx);
                (h, b, None, false)
            }
            Scope::Projects => {
                let (h, b) = self.projects_view(window, cx);
                (h, b, None, false)
            }
            Scope::Settings | Scope::Phone => {
                let (h, b) = self.settings_view(&missions, cx);
                (h, b, None, false)
            }
            Scope::Behaviors => {
                let (h, b) = self.behaviors_pane(window, cx);
                let typing = !self.behaviors.editing();
                (h, b, typing.then_some(None), typing)
            }
            Scope::Session { space, addr } => {
                let (h, b) = self.session_pane(space, addr, &all, &missions, cx);
                (h, b, None, false)
            }
            Scope::Accounts => {
                let (h, b) = self.accounts_view(cx);
                (h, b, None, false)
            }
        };
        // A view with a conversation (All spaces, a space, New space's with
        // the tower's brain about the space it drafts): it has a column of
        // its own beside the sidebar, the whole of it, with the input under it.
        let talk: Option<Option<String>> = match &scope {
            Scope::All => Some(None),
            Scope::Space(name) => Some(Some(name.clone())),
            _ => None,
        };
        let chat = match (&scope, talk_w) {
            (Scope::New, Some(w)) => Some(self.new_chat_column(w, window, cx)),
            _ => talk.as_ref().zip(talk_w).map(|(space, w)| self.chat_column(space.as_deref(), w, window, cx)),
        };
        let input = input && chat.is_none();
        let foot = match reply.filter(|_| chat.is_none()) {
            Some(space) => self.foot_items(space.as_deref(), cx),
            None => vec![],
        };
        let side = self.sidebar(&scope, side_w, &all, &missions, cx);
        // The columns' borders, dragged to resize them (on top of what is beside them).
        let mut borders = vec![self.border_handle(Edge::Side, side_w, cx)];
        if let Some(t) = talk_w.filter(|_| chat.is_some()) {
            borders.push(self.border_handle(Edge::Talk, side_w + t, cx));
        }
        let resizing = self.full.as_ref().is_some_and(|f| f.resize.is_some());
        // The window is moved by its header strip (and the sidebar's top).
        let head = self.grip("head-grip", head, cx);
        let me = cx.entity().downgrade();
        // Asked for at the Phone card: scrolled there on the next frame (the card is laid out by then).
        let settings = matches!(scope, Scope::Settings | Scope::Phone);
        let scroll = self.full.as_mut().filter(|_| settings).map(|f| {
            match f.to_phone {
                Some(until) if std::time::Instant::now() < until => {
                    f.phone_anchor.scroll_to(window, cx);
                    window.request_animation_frame();
                }
                Some(_) => f.to_phone = None,
                None => {}
            }
            f.settings_scroll.clone()
        });
        let scroll = scroll.or_else(|| self.full.as_ref().filter(|_| matches!(scope, Scope::New)).map(|f| f.launch_scroll.clone()));
        let pane = div()
            .id(SharedString::from(format!("pane-{}", scope_key(&scope))))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .when_some(scroll, |el, s| el.track_scroll(&s))
            .child(v_flex().w_full().gap(px(16.0)).pb(px(4.0)).children(notice.map(eff)).when(!undo.is_empty(), |el| el.child(v_flex().gap(px(8.0)).children(undo))).children(items));
        h_flex()
            .size_full()
            .items_start()
            .child(side)
            .children(chat)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .px(px(18.0))
                    .pb(px(16.0))
                    .gap(px(16.0))
                    .child(head)
                    .child(pane)
                    .when(!foot.is_empty(), |el| el.child(v_flex().gap(px(8.0)).children(foot)))
                    .when(input, |el| el.child(self.input(window, cx))),
            )
            // A press on a grip becomes a move once the pointer travels (`drag_moved`).
            .child(
                gpui::canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        // A border held: the resize cursor wherever the pointer goes.
                        if resizing {
                            window.set_window_cursor_style(gpui::CursorStyle::ResizeColumn);
                        }
                        let m = me.clone();
                        window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, window, cx| {
                            if phase == gpui::DispatchPhase::Bubble {
                                let _ = m.update(cx, |this, cx| {
                                    this.drag_moved(window);
                                    this.resize_moved(e, window, cx);
                                });
                            }
                        });
                        let m = me.clone();
                        window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, window, cx| {
                            if phase == gpui::DispatchPhase::Bubble && e.button == gpui::MouseButton::Left {
                                let _ = m.update(cx, |this, cx| {
                                    this.drag_end(window);
                                    this.resize_end(cx);
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .children(borders)
            .into_any_element()
    }

    /// The sidebar: where the window can go, and how each space stands.
    fn sidebar(&mut self, scope: &Scope, width: f32, all: &[Decision], missions: &Missions, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let here = scope.side();
        let status = self.status_el(all, missions, cx);
        let yours = model::yours(&model::main_queue(all, &missions.muted()));
        let item = |id: SharedString, mark: AnyElement, name: &str, small: String, on: bool| {
            h_flex()
                .id(id)
                .w_full()
                .gap(px(9.0))
                .items_center()
                .p(px(7.0))
                .rounded(px(12.0))
                .border_1()
                .cursor_pointer()
                .when(on, |el| el.bg(rgba(0xffffff2b)).border_color(rgba(LINE)))
                .when(!on, |el| el.border_color(gpui::transparent_black()).hover(|s| s.bg(rgba(SOFT))))
                .child(mark)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_size(px(14.0)).line_height(px(18.0)).font_weight(FontWeight::SEMIBOLD).truncate().child(name.to_string()))
                        .when(!small.is_empty(), |el| el.child(div().text_size(px(12.0)).line_height(px(16.0)).text_color(g.faint).truncate().child(small))),
                )
        };
        // Every space: the count of what is the user's, or a ring when nothing is.
        let all_mark = match yours {
            0 => mark_box(Some(div().size(px(9.0)).rounded_full().border(px(1.5)).border_color(g.faint).into_any_element())).into_any_element(),
            n => sign(model::Chip::Yours(n), 22.0, "side-all"),
        };
        let mut list = v_flex().w_full().gap(px(2.0)).child(
            item("side-all".into(), all_mark, "All spaces", "the main tower".into(), here == Side::All)
                .on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::All, cx))),
        );
        if !missions.open.is_empty() {
            list = list.child(bay_label("Open spaces").px(px(8.0)).pt(px(10.0)).pb(px(3.0)));
        }
        let now = crate::history::now_ms();
        for (i, m) in missions.open.iter().enumerate() {
            let name = m.name.clone();
            // Where its changes stand: "prod 14:20 · 1 on dev".
            let released = self.tower.read(cx).state.as_ref().and_then(|s| model::release_track(s, &m.name)).map(|t| t.summary(now)).filter(|s| !s.is_empty());
            let line = match released {
                Some(r) if m.side_line().is_empty() => r,
                Some(r) => format!("{} · {r}", m.side_line()),
                None => m.side_line(),
            };
            // How long it has been at work: "at work 12m" first (the row is cut at its end), else "1h 12m today" after.
            let at_work = m.active.as_ref().is_some_and(|a| a.at(now).now);
            let line = match m.active.as_ref().and_then(|a| a.side_word(now)) {
                Some(w) if line.is_empty() => w,
                Some(w) if at_work => format!("{w} · {line}"),
                Some(w) => format!("{line} · {w}"),
                None => line,
            };
            list = list.child(
                item(SharedString::from(format!("side-space-{i}")), sign(m.chip(), 22.0, format!("side-pulse-{i}")), &m.name, line, here == Side::Space(m.name.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| this.nav_to(Scope::Space(name.clone()), cx))),
            );
        }
        let plus = mark_box(Some(
            div()
                .size(px(22.0))
                .rounded_full()
                .border(px(1.5))
                .border_color(rgba(LINE))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(15.0))
                .line_height(px(18.0))
                .text_color(g.muted)
                .child("+")
                .into_any_element(),
        ))
        .into_any_element();
        // How many new spaces wait in draft, once there are some.
        let new_sub = match self.full.as_ref().map(|f| f.launch.drafts_kept()).unwrap_or(0) {
            0 => "from one sentence".to_string(),
            n => format!("{n} in draft"),
        };
        list = list.child(item("side-new".into(), plus, "New space", new_sub, here == Side::New).on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::New, cx))));
        // The quiet spaces take one line together, and what closed today with them.
        let (quiet, closed) = (missions.quiet.len(), missions.closed_today.len());
        if quiet + closed > 0 {
            let open = self.tower.read(cx).nav.quiet_open;
            let mut line = match quiet {
                0 => String::new(),
                1 => "1 quiet space".to_string(),
                n => format!("{n} quiet spaces"),
            };
            if closed > 0 {
                line = if line.is_empty() { format!("{closed} closed today") } else { format!("{line} · {closed} closed today") };
            }
            list = list.child(
                h_flex()
                    .id("side-fold")
                    .w_full()
                    .mt(px(6.0))
                    .pl(px(40.0))
                    .pr(px(8.0))
                    .py(px(8.0))
                    .gap(px(10.0))
                    .rounded(px(12.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(SOFT)))
                    .text_size(px(13.0))
                    .text_color(g.faint)
                    .items_start()
                    .child(div().flex_1().min_w_0().child(line))
                    .child(div().flex_none().child(if open { "Hide" } else { "Show" }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.tower.update(cx, |t, cx| {
                            t.nav.quiet_open = !t.nav.quiet_open;
                            cx.notify();
                        });
                    })),
            );
            if open {
                let mut names = v_flex().pl(px(40.0)).pr(px(8.0)).pb(px(6.0)).gap(px(3.0)).text_size(px(13.0)).line_height(px(17.0)).text_color(g.faint);
                if closed > 0 {
                    names = names.child(div().child(format!("Closed today: {}. The closing report is in Record.", missions.closed_today.join(", "))));
                }
                // Each quiet space, with what it was for where the tower knows.
                names = names.children(missions.quiet.iter().map(|n| {
                    let goal = missions.resting.iter().find(|m| &m.name == n).map(|m| m.goal.trim()).filter(|g| !g.is_empty());
                    div().truncate().child(match goal {
                        Some(goal) => format!("{n} · {goal}"),
                        None => n.clone(),
                    })
                }));
                if quiet > 0 {
                    names = names.child(div().pt(px(3.0)).child("No session runs in any of them. One left quiet for a day is archived."));
                }
                list = list.child(names);
            }
        }
        let hint = self.phone_hint(cx);
        // Each Claude account's usage, live; a click shows who runs on which.
        let accounts = self.accounts_strip(here == Side::Accounts, cx);
        let foot = |id: &'static str, text: &'static str, side: Side, to: Scope| {
            let on = here == side;
            div()
                .id(id)
                // Four of them across the sidebar: Record, Shots, Projects, Settings.
                .px(px(5.0))
                .py(px(5.0))
                .rounded(px(8.0))
                .cursor_pointer()
                .text_size(px(12.0))
                .when(on, |el| el.bg(rgba(0xffffff2b)).text_color(g.text))
                .when(!on, |el| el.text_color(g.faint).hover(|s| s.text_color(g.text)))
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| this.nav_to(to.clone(), cx)))
        };
        v_flex()
            .w(px(width))
            .h_full()
            .flex_none()
            .px(px(10.0))
            .pb(px(14.0))
            .gap(px(12.0))
            .bg(rgba(SOFT))
            .border_r_1()
            .border_color(rgba(LINE))
            // "Tower" and how things stand: the window is moved by it too.
            .child(
                v_flex()
                    .id("side-grip")
                    .pt(px(18.0))
                    .px(px(8.0))
                    .gap(px(3.0))
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _: &gpui::MouseDownEvent, window, _| this.drag_start(window)))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().flex_1().text_size(px(19.0)).line_height(px(23.0)).font_weight(FontWeight::SEMIBOLD).child("Tower"))
                            // Back to the floating panel: it opens, this window closes, the orb opens the panel from now on.
                            .child(tg("to-floating", "Floating", false).on_click(cx.listener(|this, _, window, cx| crate::orb::to_floating(&this.tower.clone(), window, cx))))
                            // Into the orb, kept as it is (as Esc, or a click on the orb, does); the orb brings it back.
                            .child(minimize_mark("minimize-full").on_click(cx.listener(|this, _, window, cx| crate::orb::minimize_full(&this.tower.clone(), window, cx)))),
                    )
                    .child(div().text_size(px(12.5)).text_color(g.muted).child(status))
                    .children(hint),
            )
            .child(div().id("side-list").flex_1().min_h_0().overflow_y_scroll().child(list))
            .children(accounts)
            .child(
                h_flex()
                    .px(px(2.0))
                    .gap(px(1.0))
                    .child(foot("side-record", "Record", Side::Record, Scope::Record))
                    .child(foot("side-shots", "Shots", Side::Shots, Scope::Shots))
                    .child(foot("side-projects", "Projects", Side::Projects, Scope::Projects))
                    .child(foot("side-settings", "Settings", Side::Settings, Scope::Settings)),
            )
            .into_any_element()
    }

    /// Every space (W1 to W4): the queue, most waiting work first, beside
    /// the open spaces as missions.
    fn all_view(&mut self, all: &[Decision], missions: &Missions, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let g = glass();
        let mut sub = vec![format!("{} open", missions.open.len())];
        if !missions.quiet.is_empty() {
            sub.push(format!("{} quiet", missions.quiet.len()));
        }
        sub.push("the main tower".into());
        let head = whead(
            "All spaces",
            vec![div().child(sub.join(" · ")).into_any_element()],
            Some(mbtn("new-space", "+ New space", true, None).on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::New, cx))).into_any_element()),
        );
        let queue = model::main_queue(all, &missions.muted());
        let mut cards = match self.off_items(cx) {
            Some(off) => off,
            None => self.queue_items(&queue, true, &|_| "After that".to_string(), "Your queue is empty.", cx),
        };
        if self.shown_open.is_some() {
            cards.push(div().px(px(4.0)).text_size(px(12.0)).text_color(g.faint).line_height(px(16.0)).child("Click the window, then press a number to answer.").into_any_element());
        }
        let threads = self.tower.read(cx).s("threads").clone();
        let mut rows: Vec<AnyElement> = vec![];
        for (i, m) in missions.open.iter().enumerate() {
            let (progress, ok) = m.progress();
            let about = m.about();
            let name = m.name.clone();
            let title = h_flex()
                .gap(px(7.0))
                .items_center()
                .child(nm(&m.name).min_w_0().truncate())
                .when(m.standing, |el| {
                    el.child(div().flex_none().px(px(7.0)).rounded_full().border_1().border_color(rgba(LINE)).text_size(px(11.0)).text_color(g.faint).child("standing"))
                })
                .into_any_element();
            let mut under = vec![];
            if !about.is_empty() {
                under.push(div().text_size(px(13.0)).line_height(px(18.0)).text_color(g.muted).line_clamp(2).child(about).into_any_element());
            }
            under.push(
                div()
                    .text_size(px(12.0))
                    .line_height(px(16.0))
                    .text_color(g.faint)
                    .child(m.meta(threads[m.name.as_str()].as_array().map(|a| a.len()).unwrap_or(0)))
                    .into_any_element(),
            );
            rows.push(
                lrow(sign(m.chip(), 22.0, format!("mission-pulse-{i}")), title, (!progress.is_empty()).then(|| pg(&progress, ok)), under)
                    .id(("mission", i))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgba(SOFT)))
                    .on_click(cx.listener(move |this, _, _, cx| this.nav_to(Scope::Space(name.clone()), cx)))
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px(px(4.0))
                    .text_size(px(13.0))
                    .line_height(px(18.0))
                    .text_color(g.muted)
                    .child("No space has a session running. A new space starts from one sentence.")
                    .into_any_element(),
            );
        }
        let pane_w = self.full.as_ref().map(|f| f.pane_w).unwrap_or(900.0);
        // On hold (muted spaces' aside, as their queue is): under the queue, not counted.
        let muted = missions.muted();
        let held: Vec<Decision> = self.held_decisions(cx).into_iter().filter(|d| !d.muted && !muted.iter().any(|m| d.in_space(m))).collect();
        let held = self.held_bay(&held, cx);
        let queue_col = v_flex().gap(px(10.0)).child(bay_label("Your queue · most waiting work first")).children(cards).children(held);
        let missions_col = v_flex().min_w_0().gap(px(8.0)).child(bay_label("Missions")).child(v_flex().gap(px(2.0)).children(rows));
        // Side by side when both have room; else the missions under the queue, each the pane's width.
        let cols = if pane_w >= QUEUE_W + 22.0 + 300.0 {
            h_flex().w_full().gap(px(22.0)).items_start().child(queue_col.w(px(QUEUE_W)).flex_none()).child(missions_col.flex_1()).into_any_element()
        } else {
            v_flex().w_full().gap(px(22.0)).child(queue_col.w_full()).child(missions_col.w_full()).into_any_element()
        };
        (head, vec![cols])
    }

    /// The screenshots (W15): the gallery, newest first; a click shows one
    /// full size, over the whole screen.
    fn shots_pane(&mut self, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let list: Vec<_> = self.all_shots(cx).into_iter().filter(|s| !super::session::is_test(s.who())).collect();
        let today = list.iter().filter(|s| super::day_label(s.at) == "Today").count();
        let count = match (list.len(), today) {
            (0, _) => String::new(),
            (n, 0) => format!(" {n} in all."),
            (_, t) => format!(" {t} today."),
        };
        let head = whead("Shots", vec![div().child(format!("What sessions sent to show their work, newest first.{count}")).into_any_element()], None);
        let body = if list.is_empty() { super::home::empty_box("No screenshots yet.") } else { self.gallery(&list, cx) };
        (head, vec![body])
    }

    /// The behaviors, from Settings (W18): the rules, each with its switch.
    fn behaviors_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let (on, n) = self.behaviors.counts();
        let line = if n == 0 { "How the tower works, as rules you can read and change.".to_string() } else { format!("{on} of {n} on. How the tower works, as rules you can read and change. Open one, or tell the tower below.") };
        let back = tg("back-settings", "Back to Settings", false).on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::Settings, cx))).into_any_element();
        (whead("Behaviors", vec![div().child(line).into_any_element()], Some(back)), self.behaviors_view(window, cx))
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dragged_columns_keep_every_column_its_least() {
        // Never dragged: the sidebar's 236 and the column's default.
        assert_eq!(widths(1400.0, None, None), (236.0, talk_width(1400.0)));
        // Dragged: as wide as asked, within its bounds.
        assert_eq!(widths(1400.0, Some(280.0), Some(600.0)), (280.0, 600.0));
        assert_eq!(widths(1400.0, Some(100.0), Some(100.0)), (SIDE_MIN_W, TALK_MIN_W), "never under their least");
        assert_eq!(widths(1400.0, Some(900.0), None).0, SIDE_MAX_W);
        // Too wide for the window: the pane keeps its 420.
        let (s, t) = widths(1400.0, Some(236.0), Some(1200.0));
        assert_eq!(1400.0 - s - t - PANE_PAD, PANE_MIN_W);
        // The window made narrower after: the column gives way, the pane still keeps its own.
        for w in [960.0_f32, 1000.0, 1150.0, 1400.0, 1728.0, 2400.0] {
            for (side, talk) in [(None, None), (Some(360.0), Some(2000.0)), (Some(200.0), Some(280.0))] {
                let (s, t) = widths(w, side, talk);
                assert!((SIDE_MIN_W..=SIDE_MAX_W).contains(&s) && t >= TALK_MIN_W, "{w} {side:?} {talk:?}: {s} {t}");
                assert!(w - s - t - PANE_PAD >= PANE_MIN_W - 0.01 || (s, t) == (SIDE_MIN_W, TALK_MIN_W), "{w}: the pane keeps room ({s} {t})");
            }
        }
    }

    #[test]
    fn a_border_follows_the_pointer_and_stops_at_the_least() {
        let talk = Resize { edge: Edge::Talk, x0: 600.0, side0: 236.0, talk0: 364.0, with_talk: true, moved: false, at: ChatAt::End };
        assert_eq!(dragged(&talk, 800.0, 1500.0), (236.0, 564.0), "wider by what it moved");
        assert_eq!(dragged(&talk, 500.0, 1500.0), (236.0, 280.0), "narrower, never under 280");
        assert_eq!(dragged(&talk, 1500.0, 1500.0), (236.0, 1500.0 - 236.0 - PANE_MIN_W - PANE_PAD), "the pane keeps its 420");
        // The sidebar's border: the column gives what the sidebar takes, its right border staying.
        let side = Resize { edge: Edge::Side, x0: 236.0, side0: 236.0, talk0: 364.0, with_talk: true, moved: false, at: ChatAt::End };
        assert_eq!(dragged(&side, 200.0, 1500.0), (200.0, 400.0));
        assert_eq!(dragged(&side, 300.0, 1500.0), (300.0, 300.0));
        assert_eq!(dragged(&side, 340.0, 1500.0), (320.0, 280.0), "stops where the column has its least");
        assert_eq!(dragged(&side, 100.0, 1500.0), (200.0, 400.0), "and at the sidebar's");
        // A view without a conversation: the sidebar alone.
        let alone = Resize { with_talk: false, ..side };
        assert_eq!(dragged(&alone, 340.0, 1500.0), (340.0, 364.0));
    }

    #[test]
    fn each_view_scrolls_by_itself() {
        let session = |addr: &str| Scope::Session { space: "isa".into(), addr: addr.into() };
        assert_eq!(scope_key(&Scope::Space("isa".into())), "space-isa");
        assert_ne!(scope_key(&Scope::Space("isa".into())), scope_key(&Scope::Space("anna".into())));
        assert_ne!(scope_key(&session("t1")), scope_key(&session("t2")));
        let all = [Scope::All, Scope::New, Scope::Record, Scope::Shots, Scope::Projects, Scope::Settings, Scope::Behaviors, Scope::Phone, Scope::Accounts];
        let keys: HashSet<String> = all.iter().map(scope_key).collect();
        assert_eq!(keys.len(), all.len());
    }
}
