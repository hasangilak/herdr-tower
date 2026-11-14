//! One space in the full window (`docs/quiet-tower.md`, "Step 5 and 8"; W5 to
//! W8 in the design): a mission with its own tower.
//!
//! - **The head**: its name, its sessions and that it has a tower of its own;
//!   its goal and what counts as done, or that it is a standing space; and
//!   its switches. **Follow**: its results reach the user as they land.
//!   **Mute**: its decisions stay here, off the orb's count and out of the
//!   main queue. Each says what it does, in a line, while it is on.
//! - **The goal** can be changed here (the goal, done when, standing). A
//!   space with no goal is the user's own: it has no end, and its sessions
//!   get no worktrees.
//! - **The closing report**, when its goal is met, with "Close this space"
//!   (and when it closes by itself). A close the tower refuses, because a
//!   session there still works, says so and offers "Close anyway".
//! - **Resolve**, in the head of a space with a goal: the user's word that
//!   the goal is achieved. It asks first, listing what it would end (`tower
//!   space resolve --dry-run`): every session and open terminal there, the
//!   worktrees it removes and those it keeps (work not committed or pushed),
//!   whether Herdr's space goes. A session at work or waiting for the user
//!   is named, and the button is "Close anyway". Then `tower space resolve`:
//!   the goal is marked achieved and the space closed as a close closes it.
//! - **Needs you**: its own queue, the open card answered by the number keys.
//! - **Requests**, each with how far it is; **Sessions**, each with its
//!   status, its last words, its worktree, branch and port, a click away
//!   from its own view; the thread **between its sessions**.
//! - The input below goes to this space's tower when it has one that runs
//!   (`cli::ask` with the space: `tower ask --space`), else to the main
//!   tower with the space named.
//!
//! What it sets goes through `tower space …` (`window::run`): a switch shows
//! at once and goes back, with the command's error, if the tower refuses it.

use gpui::{AnyElement, App, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::input::{InputEvent, RopeExt as _, TextareaState};
use gpui_kit::component::{h_flex, v_flex};

use super::home::{CARD, LINE, SOFT, acts_row, bay_label, cstat, empty_box, mbtn, sign, tg};
use super::launch::field;
use super::session::last_words;
use super::window::{bay, eff, lrow, mark_box, nm, pg, whead};
use super::{AMBER, CYAN, Panel, glass};
use crate::orb::model::{self, Chip, Decision, GoalStep, Member, Mission, Missions};
use crate::orb::nav::Scope;

/// How many of the thread's messages a space's view shows (the latest).
const THREAD_ROWS: usize = 5;

/// A Resolve the user asked for, waiting for their word: the space, and
/// what resolving it would end (`tower space resolve --dry-run`; None while
/// it is being read).
pub(super) struct ResolveAsk {
    space: String,
    preview: Option<serde_json::Value>,
}

/// What a preview (`missions::preview`) says, as the confirmation's lines:
/// each terminal it ends, each that stays, each worktree, Herdr's space; and
/// who holds it open (at work or waiting for the user), if anyone.
pub(super) fn resolve_lines(v: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    let name_of = |t: &serde_json::Value| {
        let (n, a) = (t["name"].as_str().unwrap_or(""), t["address"].as_str().unwrap_or(""));
        if n.is_empty() || n == a { a.to_string() } else { format!("{n} ({a})") }
    };
    let mut lines = vec![];
    if v["tower"].is_object() {
        lines.push(format!("Ends its own tower, {}.", name_of(&v["tower"])));
    }
    for t in list("ends") {
        let how = match (t["agent"] == true, t["status"].as_str().unwrap_or("")) {
            (false, _) => "an open terminal".to_string(),
            (true, "working") => "a session at work".to_string(),
            (true, "blocked") => "a session waiting for you".to_string(),
            (true, s) => format!("a session, {}", status_word(s)),
        };
        lines.push(format!("Ends {}: {how}.", name_of(&t)));
    }
    if list("ends").is_empty() {
        lines.push("No session or terminal is open there.".to_string());
    }
    for t in list("stays") {
        lines.push(format!("{} stays: it is the tower’s own.", name_of(&t)));
    }
    for w in list("worktrees") {
        let what = w["branch"].as_str().filter(|b| !b.is_empty()).or(w["path"].as_str()).unwrap_or("");
        if w["removed"] == true {
            lines.push(format!("Removes the worktree {what} (its branch stays)."));
        } else {
            lines.push(format!("Keeps the worktree {what}: {}.", w["why"].as_str().unwrap_or("it has work in it").trim_end_matches('.')));
        }
    }
    match v["herdr_space"].as_str() {
        Some("closed") => lines.push("Herdr’s space closes.".to_string()),
        Some("kept") => lines.push("Herdr’s space stays: it holds the tower’s own terminal.".to_string()),
        _ => {}
    }
    let held = list("busy")
        .iter()
        .map(|t| {
            let doing = if t["status"] == "blocked" { "waits for you" } else { "is still working" };
            format!("{} {doing}", name_of(t))
        })
        .collect();
    (lines, held)
}

/// A space's goal while it is being changed.
pub(super) struct GoalEdit {
    space: String,
    /// The space as it was when the editor opened (what Save compares with).
    was: Mission,
    goal: Entity<TextareaState>,
    done_when: Entity<TextareaState>,
    standing: bool,
}

/// A session's mark in its row: a pulse at work, amber while it waits, a tick at rest.
pub(super) fn session_mark(status: &str, key: String) -> AnyElement {
    match status {
        "working" => sign(Chip::Working, 22.0, key),
        "blocked" => mark_box(Some(div().size(px(9.0)).rounded_full().bg(rgb(AMBER)).into_any_element())).into_any_element(),
        _ => sign(Chip::Done, 22.0, key),
    }
}

/// A session's status as its row says it.
pub(super) fn status_word(status: &str) -> &str {
    match status {
        "" => "idle",
        "blocked" => "waits",
        other => other,
    }
}

/// `Closing stops its 2 sessions and removes their worktrees…`, and when
/// it closes by itself (`closes`: `in 5 h`), if it does.
fn closing_line(sessions: usize, closes: Option<&str>) -> String {
    let stops = match sessions {
        0 => "Closing archives it".to_string(),
        1 => "Closing stops its session and removes its worktree".to_string(),
        n => format!("Closing stops its {n} sessions and removes their worktrees"),
    };
    let alone = match closes {
        Some(when) => format!(" Left alone, it closes by itself {when}."),
        None => String::new(),
    };
    format!("{stops} (one with work that isn’t committed or pushed is kept).{alone}")
}

/// `1 session`, `3 worktrees`.
fn counted(n: usize, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

/// What `tower space close` printed (its receipt, as JSON) in a few
/// sentences: what ended, what was removed, what was kept and why.
pub(super) fn closed_line(name: &str, out: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(out).unwrap_or_default();
    if !v.is_object() {
        // Not the receipt: what it said, as it said it.
        let said = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
        return if said.is_empty() { format!("{name} is closed.") } else { said };
    }
    let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    let closed = v["closed"].as_str().unwrap_or(name);
    let mut parts = vec![if v["resolved"] == true { format!("{closed} is resolved: its goal is achieved, and it is closed.") } else { format!("{closed} is closed.") }];
    match list("sessions_ended").len() {
        0 => {}
        n => parts.push(format!("{} ended.", counted(n, "session"))),
    }
    for t in list("sessions_left") {
        let who = t["name"].as_str().filter(|n| !n.is_empty()).or(t["address"].as_str()).unwrap_or("A session");
        parts.push(format!("{who} still runs: {}.", t["why"].as_str().unwrap_or("it couldn’t be ended").trim_end_matches('.')));
    }
    match list("worktrees_removed").len() {
        0 => {}
        n => parts.push(format!("{} removed (the branches stay).", counted(n, "worktree"))),
    }
    for w in list("kept_worktrees") {
        let what = w["branch"].as_str().filter(|b| !b.is_empty()).or(w["path"].as_str()).unwrap_or("one");
        parts.push(format!("The worktree {what} is kept: {}.", w["why"].as_str().unwrap_or("it has work in it").trim_end_matches('.')));
    }
    parts.push(if v["report"].as_str().is_some_and(|r| !r.trim().is_empty()) { "Its closing report is in Record.".into() } else { "It had no closing report.".to_string() });
    parts.join(" ")
}

/// A space's open PRs whose conflicts (or update) their sessions fix, a
/// line each (`crate::fixes::line`): "PR #841 (eyzflow) · Being fixed by
/// eyzflow: resolving its conflicts with dev."
pub(super) fn fix_lines(space: &str, prs: &std::collections::BTreeMap<String, crate::prs::Pr>) -> Vec<String> {
    let mut list: Vec<&crate::prs::Pr> = prs.values().filter(|p| p.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(space))).collect();
    list.sort_by_key(|p| (p.repo.clone(), p.number));
    list.iter().filter_map(|p| Some(format!("PR #{} ({}) · {}", p.number, crate::prscore::folder_name(&p.repo), crate::fixes::line(p)?))).collect()
}

/// The reviews of a space's open PRs, a line each: "PR #12 · Review: 2
/// critical, round 2 · fix-login-review-app-12 · Needs review" (its score's
/// word, when it has one).
pub(super) fn review_lines(space: &str, prs: &std::collections::BTreeMap<String, crate::prs::Pr>) -> Vec<String> {
    let mut list: Vec<&crate::prs::Pr> = prs.values().filter(|p| p.open() && p.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(space)) && p.review.is_some()).collect();
    list.sort_by_key(|p| (p.repo.clone(), p.number));
    list.iter()
        .filter_map(|p| {
            let r = p.review.as_ref()?;
            let line = if r.line.is_empty() { crate::reviews::chip_line(r) } else { r.line.clone() };
            let who = if r.reviewer_name.is_empty() { String::new() } else { format!(" · {}", r.reviewer_name) };
            // Whether a person must read it (`crate::prscore`), when it was scored.
            let score = p.score.as_ref().map(|s| format!(" · {}", s.word())).unwrap_or_default();
            Some(format!("PR #{} ({}) · {line}{who}{score}", p.number, p.repo.rsplit('/').next().unwrap_or(&p.repo)))
        })
        .collect()
}

/// The colour of a change's stage: on prod (cyan), on dev (amber), open or closed (quiet).
fn stage_color(stage: &str) -> Option<u32> {
    match stage {
        "released" => Some(CYAN),
        "merged" | "deployed" => Some(AMBER),
        _ => None,
    }
}

/// A change's stage in a word or two, for its chip.
fn stage_word(stage: &str, dev: &str, prod: &str) -> String {
    match stage {
        "open" => "open".into(),
        "merged" => format!("merged into {dev}"),
        "deployed" => format!("on {dev}"),
        "released" => format!("on {prod}"),
        _ => "closed".into(),
    }
}

/// A space's release track (`crate::releases`): per repo, what is on prod
/// and since when, what is on dev and not on prod, each change with its
/// stage, and each release to prod with what it carried.
fn release_card(t: &crate::releases::Track, now: u64) -> AnyElement {
    let g = glass();
    let mut card = v_flex().w_full().p(px(14.0)).gap(px(12.0)).rounded(px(18.0)).bg(rgba(CARD)).border_1().border_color(rgba(LINE)).child(
        h_flex()
            .gap(px(8.0))
            .items_center()
            .child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child("Release track"))
            .child(div().text_size(px(12.0)).text_color(g.faint).child(format!("from GitHub, {}", crate::releases::when(t.checked_ms, now)))),
    );
    for r in &t.repos {
        let mut repo = v_flex().gap(px(6.0)).child(
            h_flex()
                .gap(px(8.0))
                .items_center()
                .child(div().text_size(px(13.0)).font_weight(FontWeight::SEMIBOLD).child(r.repo.clone()))
                .child(div().text_size(px(12.0)).text_color(g.faint).child(format!("{} → {}", r.dev, r.prod))),
        );
        // What is where, plainly: on prod (cyan), on dev only (amber), the rest quiet; a skip of dev in rose.
        for l in r.lines(now) {
            let color = match () {
                _ if l.starts_with(&format!("On {} since", r.prod)) => rgb(CYAN),
                _ if l.starts_with(&format!("On {},", r.dev)) => rgb(AMBER),
                _ if l.contains("against the dev-then-prod rule") => rgb(0xff8f8f),
                _ => g.muted.into(),
            };
            repo = repo.child(div().text_size(px(14.0)).line_height(px(19.0)).font_weight(FontWeight::MEDIUM).text_color(color).child(l));
        }
        // Each change, newest stage first: its number, title, and stage with when.
        let mut changes = v_flex().gap(px(3.0)).pt(px(2.0));
        for c in &r.changes {
            let chip_color = stage_color(&c.stage).map(rgb).unwrap_or(g.faint.into());
            changes = changes.child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(10.0))
                    .bg(rgba(SOFT))
                    .text_size(px(13.0))
                    .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(format!("#{}", c.number)))
                    .child(div().flex_1().min_w_0().truncate().text_color(g.muted).child(c.title.clone()))
                    .child(div().flex_none().px(px(8.0)).py(px(1.0)).rounded_full().border_1().border_color(chip_color).text_size(px(12.0)).text_color(chip_color).child(stage_word(&c.stage, &r.dev, &r.prod)))
                    .child(div().flex_none().w(px(210.0)).text_size(px(12.0)).text_color(g.faint).truncate().child(c.detail(now))),
            );
        }
        repo = repo.child(changes);
        // Each release to prod: the release PR, when it landed, what it carried.
        if !r.releases.is_empty() {
            repo = repo.child(div().pt(px(4.0)).text_size(px(11.0)).font_weight(FontWeight::SEMIBOLD).text_color(g.faint).child("RELEASES"));
            for rel in &r.releases {
                let carried = if rel.carried.is_empty() { "nothing of this space yet".to_string() } else { rel.carried.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ") };
                let landed = match (rel.state.as_str(), rel.merged_ms) {
                    ("merged", Some(at)) => format!("landed on {} {}", rel.base, crate::releases::when(at, now)),
                    ("merged", None) => format!("landed on {}", rel.base),
                    _ => "under way".to_string(),
                };
                repo = repo.child(
                    h_flex()
                        .gap(px(8.0))
                        .items_center()
                        .text_size(px(13.0))
                        .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(format!("Release #{}", rel.number)))
                        .child(div().flex_none().text_color(g.faint).child(format!("{} → {}", rel.head, rel.base)))
                        .child(div().flex_none().text_color(if rel.state == "merged" { rgb(CYAN) } else { rgb(AMBER) }).child(landed))
                        .child(div().min_w_0().truncate().text_color(g.muted).child(format!("carried {carried}"))),
                );
            }
        }
        card = card.child(repo);
    }
    card.into_any_element()
}

/// The tower refused to close a space because a session there still works
/// or waits (`--force` would close it anyway).
pub(super) fn refused_close(error: &str) -> bool {
    error.contains("--force")
}

impl Panel {
    /// A field of the goal's editor has the keyboard: digits are text there.
    pub(super) fn editing_goal(&self, window: &Window, cx: &App) -> bool {
        let Some(edit) = self.full.as_ref().and_then(|f| f.edit.as_ref()) else { return false };
        [&edit.goal, &edit.done_when].iter().any(|f| gpui::Focusable::focus_handle(f.read(cx), cx).is_focused(window))
    }

    /// Opens the goal's editor on a space, or closes it.
    fn toggle_goal(&mut self, m: &Mission, window: &mut Window, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_mut() else { return };
        if full.edit.take().is_some_and(|e| e.space == m.name) {
            return cx.notify();
        }
        let mut make = |text: &str, hint: &'static str| {
            // A long goal (a pasted ticket) is all there to read and change: up to 14 lines, then it scrolls.
            // Enter saves, as Save does; Shift+Enter is a new line.
            let f = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 14).submit_on_enter(true).placeholder(hint));
            let text = text.to_string();
            f.update(cx, |s, cx| {
                // What is there, with the caret after it.
                s.set_value(text, window, cx);
                let end = s.text().offset_to_position(s.text().len());
                s.set_cursor_position(end, window, cx);
            });
            cx.subscribe_in(&f, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = ev {
                    this.goal_entered(cx);
                }
            })
            .detach();
            f
        };
        let goal = make(&m.goal, "What it should get done");
        let done_when = make(&m.done_when, "What counts as done");
        goal.update(cx, |s, cx| s.focus(window, cx));
        if let Some(full) = self.full.as_mut() {
            full.notes.remove(&format!("space:{}", m.name));
            full.edit = Some(GoalEdit { space: m.name.clone(), was: m.clone(), goal, done_when, standing: m.standing });
        }
        cx.notify();
    }

    /// Enter in the goal's editor: Save, unless a save is under way.
    fn goal_entered(&mut self, cx: &mut Context<Self>) {
        let Some(was) = self.full.as_ref().and_then(|f| f.edit.as_ref()).map(|e| e.was.clone()) else { return };
        if !self.running(&format!("space:{}", was.name)) {
            self.save_goal(&was, cx);
        }
    }

    /// Saves what the editor holds: `tower space goal`, then `tower space
    /// standing` if that changed. The editor closes once both went.
    fn save_goal(&mut self, was: &Mission, cx: &mut Context<Self>) {
        let Some(edit) = self.full.as_ref().and_then(|f| f.edit.as_ref()) else { return };
        let (space, standing) = (edit.space.clone(), edit.standing);
        let goal = edit.goal.read(cx).value().trim().to_string();
        let done_when = edit.done_when.read(cx).value().trim().to_string();
        let place = format!("space:{space}");
        let mut commands: Vec<Vec<String>> = vec![];
        if goal != was.goal.trim() || done_when != was.done_when.trim() {
            // By hand: it wins over what the tower would make of a request meanwhile.
            let mut args = vec!["space".to_string(), "goal".into(), space.clone(), goal, "--by".into(), "you".into()];
            // (Given whenever it changed: an empty one clears what was there.)
            if done_when != was.done_when.trim() {
                args.extend(["--done-when".to_string(), done_when]);
            }
            commands.push(args);
        }
        if standing != was.standing {
            commands.push(vec!["space".into(), "standing".into(), space.clone(), if standing { "on" } else { "off" }.into()]);
        }
        self.run_all(place, commands, cx);
    }

    /// Runs the commands one after another for `place`; the first that fails
    /// stops them (its error shows there), and when all went the editor closes.
    fn run_all(&mut self, place: String, mut commands: Vec<Vec<String>>, cx: &mut Context<Self>) {
        if commands.is_empty() {
            if let Some(full) = self.full.as_mut() {
                full.edit = None;
            }
            return cx.notify();
        }
        let first = commands.remove(0);
        let again = place.clone();
        self.run(&place, first, cx, move |this, result, cx| {
            if result.is_ok() {
                this.run_all(again, commands, cx);
            }
        });
    }

    /// Follow or mute a space, or stop: shown at once, and taken back with
    /// the command's error if the tower refuses.
    pub(super) fn set_switch(&mut self, space: &str, follow: bool, on: bool, cx: &mut Context<Self>) {
        let place = format!("space:{space}");
        if self.running(&place) {
            return;
        }
        let verb = match (follow, on) {
            (true, true) => "follow",
            (true, false) => "unfollow",
            (false, true) => "mute",
            (false, false) => "unmute",
        };
        let name = space.to_string();
        self.tower.update(cx, |t, cx| {
            t.set_mark(&name, |m| if follow { m.follow = Some(on) } else { m.mute = Some(on) });
            cx.notify();
        });
        self.run(&place, vec!["space".into(), verb.into(), space.to_string()], cx, move |this, result, cx| {
            if result.is_err() {
                this.tower.update(cx, |t, cx| {
                    t.set_mark(&name, |m| if follow { m.follow = None } else { m.mute = None });
                    cx.notify();
                });
            }
        });
    }

    /// Review its PRs, or stop: `tower space review <space> on|off`, shown at
    /// once and taken back with the command's error if the tower refuses.
    pub(super) fn set_review(&mut self, space: &str, on: bool, cx: &mut Context<Self>) {
        let place = format!("space:{space}");
        if self.running(&place) {
            return;
        }
        let name = space.to_string();
        self.tower.update(cx, |t, cx| {
            t.set_mark(&name, |m| m.review = Some(on));
            cx.notify();
        });
        self.run(&place, vec!["space".into(), "review".into(), space.to_string(), if on { "on" } else { "off" }.into()], cx, move |this, result, cx| {
            if result.is_err() {
                this.tower.update(cx, |t, cx| {
                    t.set_mark(&name, |m| m.review = None);
                    cx.notify();
                });
            }
        });
    }

    /// "Close this space": `tower space close` (`force`: though a session
    /// there still works), then back to every space with what it did.
    fn close_space(&mut self, space: &str, force: bool, cx: &mut Context<Self>) {
        let name = space.to_string();
        let mut args: Vec<String> = vec!["space".into(), "close".into(), space.to_string()];
        if force {
            args.push("--force".into());
        }
        self.run(&format!("space:{space}"), args, cx, move |this, result, cx| {
            let Ok(out) = result else { return };
            let text = closed_line(&name, out);
            this.tower.update(cx, |t, cx| {
                t.nav.say(Scope::All, text);
                cx.notify();
            });
        });
    }

    /// Resolve was pressed: what it would end is read, and asked about.
    fn ask_resolve(&mut self, space: &str, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_mut() else { return };
        if full.resolve.as_ref().is_some_and(|r| r.space == space) {
            // Pressed again: put away.
            full.resolve = None;
            return cx.notify();
        }
        full.resolve = Some(ResolveAsk { space: space.to_string(), preview: None });
        let name = space.to_string();
        self.run(&format!("resolve:{space}"), vec!["space".into(), "resolve".into(), space.to_string(), "--dry-run".into()], cx, move |this, result, cx| {
            let Ok(out) = result else { return };
            let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_default();
            if let Some(r) = this.full.as_mut().and_then(|f| f.resolve.as_mut()).filter(|r| r.space == name) {
                r.preview = Some(v);
            }
            cx.notify();
        });
    }

    /// The user's word: `tower space resolve` (`force`: though a session
    /// there works or waits), then back to every space with what it did.
    fn resolve_space(&mut self, space: &str, force: bool, cx: &mut Context<Self>) {
        let name = space.to_string();
        let mut args: Vec<String> = vec!["space".into(), "resolve".into(), space.to_string()];
        if force {
            args.push("--force".into());
        }
        self.run(&format!("space:{space}"), args, cx, move |this, result, cx| {
            let Ok(out) = result else { return };
            if let Some(full) = this.full.as_mut() {
                full.resolve = None;
            }
            let text = closed_line(&name, out);
            this.tower.update(cx, |t, cx| {
                t.nav.say(Scope::All, text);
                cx.notify();
            });
        });
    }

    /// `HERDER_TOWER_ORB_RESOLVE=<space>` (see `crate::orb`): Resolve is
    /// pressed for `space` as a click would.
    pub(crate) fn resolve_from_start(&mut self, space: &str, cx: &mut Context<Self>) {
        self.ask_resolve(space, cx);
    }

    /// `HERDER_TOWER_ORB_RESOLVE=<space>:go`: then its card's button, as a
    /// click would (Close anyway when someone there works or waits).
    pub(crate) fn resolve_go_from_start(&mut self, space: &str, cx: &mut Context<Self>) {
        let Some(preview) = self.full.as_ref().and_then(|f| f.resolve.as_ref()).filter(|r| r.space == space).and_then(|r| r.preview.clone()) else {
            eprintln!("[orb] HERDER_TOWER_ORB_RESOLVE: what resolving {space} would end isn't read yet");
            return;
        };
        let (_, held) = resolve_lines(&preview);
        eprintln!("[orb] test: resolving {space}{}", if held.is_empty() { "" } else { " (Close anyway)" });
        self.resolve_space(space, !held.is_empty(), cx);
    }

    /// The confirmation of a Resolve for `name`, if one is asked.
    fn resolve_card(&mut self, name: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let g = glass();
        let preview = self.full.as_ref()?.resolve.as_ref().filter(|r| r.space == name)?.preview.clone();
        let place = format!("space:{name}");
        let busy = self.running(&place);
        // The tower refused (something started there since it was read): say so, and offer it anyway.
        let refused = self.full.as_ref().and_then(|f| f.notes.get(&place)).is_some_and(|(ok, text)| !ok && refused_close(text));
        let (lines, held) = preview.as_ref().map(resolve_lines).unwrap_or_default();
        let force = refused || !held.is_empty();
        let (n1, n2) = (name.to_string(), name.to_string());
        let bullet = |text: String| {
            h_flex()
                .w_full()
                .items_start()
                .gap(px(8.0))
                .child(div().flex_none().mt(px(8.0)).size(px(4.0)).rounded_full().bg(g.faint))
                .child(div().flex_1().min_w_0().text_size(px(13.0)).line_height(px(19.0)).text_color(g.text).child(text))
        };
        Some(
            v_flex()
                .w_full()
                .p(px(16.0))
                .gap(px(10.0))
                .rounded(px(18.0))
                .bg(rgba(CARD))
                .border_1()
                .border_color(rgb(AMBER))
                .child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child(format!("Resolve {name}?")))
                .child(div().text_size(px(13.0)).line_height(px(19.0)).text_color(g.muted).child(
                    "Its goal is marked achieved and the space is closed: what is open there ends, its clean worktrees are removed, its report goes into Record, and it is archived.",
                ))
                .when(preview.is_none(), |el| el.child(cstat("Looking at what it would end…", false)))
                .when(preview.is_some(), |el| el.child(v_flex().gap(px(4.0)).children(lines.into_iter().map(bullet))))
                .when(!held.is_empty(), |el| {
                    el.child(div().text_size(px(13.0)).line_height(px(19.0)).text_color(rgb(AMBER)).child(format!(
                        "{}. Closing now cuts that work off.",
                        held.join("; ")
                    )))
                })
                .children(self.said(&format!("resolve:{name}")))
                .children(self.said(&place))
                .child(
                    acts_row()
                        .child(
                            mbtn("resolve-go", if busy { "Resolving…" } else if force { "Close anyway" } else { "Resolve and close" }, true, None)
                                .when(busy || preview.is_none(), |el| el.opacity(0.5))
                                .when(!busy && preview.is_some(), |el| el.on_click(cx.listener(move |this, _, _, cx| this.resolve_space(&n1, force, cx)))),
                        )
                        .child(mbtn("resolve-cancel", "Cancel", false, None).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(full) = this.full.as_mut() {
                                full.resolve = None;
                                full.notes.remove(&format!("space:{n2}"));
                            }
                            cx.notify();
                        }))),
                )
                .into_any_element(),
        )
    }

    /// A space's head and body.
    pub(super) fn space_view(&mut self, name: &str, all: &[Decision], missions: &Missions, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let g = glass();
        let place = format!("space:{name}");
        let Some(m) = missions.open.iter().find(|m| m.name == name).cloned() else {
            // Just launched: Herdr doesn't list it yet.
            return (whead(name, vec![], None), vec![cstat("Starting: its sessions show here once they are open.", false)]);
        };
        let editing = self.full.as_ref().and_then(|f| f.edit.as_ref()).is_some_and(|e| e.space == name);
        let busy = self.running(&place);

        // The head.
        // Its sessions, and its own tower when it has one (the daemon says so).
        let mut sub = vec![div().child(match m.tower_line() { Some(line) => format!("{} · {line}", m.sub()), None => m.sub() }).into_any_element()];
        // How long its sessions have been at work: this stretch, today, since it opened.
        if let Some(a) = &m.active {
            let now = crate::history::now_ms();
            if let Some(line) = a.head_line(now) {
                let color = if a.at(now).now { rgb(CYAN).into() } else { glass().muted };
                sub.push(div().text_color(color).child(line).into_any_element());
            }
        }
        if m.standing {
            sub.push(div().child("A standing space: the tower never closes it and opens no worktrees here.").into_any_element());
        } else if m.goal.trim().is_empty() {
            sub.push(div().child("No goal: it has no end, and the tower opens no worktrees here.").into_any_element());
        } else if let Some(line) = chat_line(&m) {
            sub.push(div().child(line).into_any_element());
        }
        // The goal and what counts as done: a block of their own under the head (not while it is being changed).
        let (m1, n1, n2, n3, n4) = (m.clone(), name.to_string(), name.to_string(), name.to_string(), name.to_string());
        let (follow, mute, review) = (m.follow, m.mute, m.review);
        // A space with a goal can be resolved: its goal achieved, the space closed.
        let resolvable = !m.standing && !m.goal.trim().is_empty();
        let asking = self.full.as_ref().and_then(|f| f.resolve.as_ref()).is_some_and(|r| r.space == name);
        let toggles = h_flex()
            .gap(px(6.0))
            .child(tg("space-goal", if m.goal.trim().is_empty() && !m.standing { "Set a goal" } else { "Goal" }, editing).on_click(cx.listener(move |this, _, window, cx| this.toggle_goal(&m1, window, cx))))
            .child(tg("space-follow", "Follow", follow).on_click(cx.listener(move |this, _, _, cx| this.set_switch(&n1, true, !follow, cx))))
            .child(tg("space-mute", "Mute", mute).on_click(cx.listener(move |this, _, _, cx| this.set_switch(&n2, false, !mute, cx))))
            .child(tg("space-review", "Review PRs", review).on_click(cx.listener(move |this, _, _, cx| this.set_review(&n3, !review, cx))))
            .when(resolvable, |el| el.child(tg("space-resolve", "✓ Resolve", asking).on_click(cx.listener(move |this, _, _, cx| this.ask_resolve(&n4, cx)))))
            .into_any_element();
        let head = whead(name, sub, Some(toggles));

        let mut items: Vec<AnyElement> = vec![];
        // Resolve asks first, above everything else here.
        items.extend(self.resolve_card(name, cx));
        // Where it is at, in a few lines (`crate::briefs`), and how it read before.
        if let Some(b) = &m.brief {
            items.push(self.brief_card(name, b, cx));
        }
        let editing_goal = self.full.as_ref().and_then(|f| f.edit.as_ref()).is_some_and(|e| e.space == name);
        if !m.standing && !m.goal.trim().is_empty() && !editing_goal {
            items.push(self.goal_card(&m, cx));
        }
        // The goal, being changed.
        if let Some(edit) = self.full.as_ref().and_then(|f| f.edit.as_ref()).filter(|e| e.space == name) {
            let standing = edit.standing;
            let was = m.clone();
            let label = |text: &str| bay_label(text).px(px(0.0));
            items.push(
                v_flex()
                    .w_full()
                    .p(px(14.0))
                    .gap(px(12.0))
                    .rounded(px(18.0))
                    .bg(rgba(CARD))
                    .border_1()
                    .border_color(rgba(LINE))
                    .child(v_flex().gap(px(7.0)).child(label("Goal")).child(field(&edit.goal)))
                    .child(v_flex().gap(px(7.0)).child(label("Done when")).child(field(&edit.done_when)))
                    .child(
                        h_flex()
                            .gap(px(10.0))
                            .items_center()
                            .child(tg("goal-standing", "Standing", standing).on_click(cx.listener(|this, _, _, cx| {
                                if let Some(edit) = this.full.as_mut().and_then(|f| f.edit.as_mut()) {
                                    edit.standing = !edit.standing;
                                }
                                cx.notify();
                            })))
                            .child(div().min_w_0().text_size(px(13.0)).text_color(g.muted).child("A standing space is yours to keep: the tower never closes it.")),
                    )
                    .child(div().text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(
                        "With a goal a space is a mission: sessions the tower opens here work in their own worktrees, and it is ready to close once its requests are done.",
                    ))
                    .child(
                        acts_row()
                            .child(mbtn("goal-save", if busy { "Saving…" } else { "Save" }, true, None).when(busy, |el| el.opacity(0.5)).when(!busy, |el| {
                                el.on_click(cx.listener(move |this, _, _, cx| this.save_goal(&was, cx)))
                            }))
                            .child(mbtn("goal-cancel", "Cancel", false, None).on_click(cx.listener(|this, _, _, cx| {
                                if let Some(full) = this.full.as_mut() {
                                    full.edit = None;
                                }
                                cx.notify();
                            }))),
                    )
                    .into_any_element(),
            );
        }
        // What the last command for this space said, if it failed.
        items.extend(self.said(&place));
        if m.follow {
            items.push(eff("Following. This space’s results reach you as they land."));
        }
        if m.mute {
            items.push(eff("Muted. Its decisions stay here and leave the orb’s count and the main queue."));
        }
        // Its PRs' reviews: where each stands; or that its PRs get none.
        if !m.review {
            items.push(eff("PR review is off. A PR a session opens here gets no reviewer: you review it yourself."));
        } else {
            items.extend(review_lines(name, &self.tower.read(cx).prs).into_iter().map(eff));
        }
        // Its PRs with conflicts that their sessions are fixing (not in the queue meanwhile), or that came back.
        items.extend(fix_lines(name, &self.tower.read(cx).prs).into_iter().map(eff));
        // Where its changes stand, repo by repo: on prod, on dev, open; and its releases.
        if let Some(t) = self.tower.read(cx).state.as_ref().and_then(|s| model::release_track(s, name)) {
            items.push(release_card(&t, crate::history::now_ms()));
        }
        // Its goal is met: the closing report, and the way to close it.
        if m.ready() {
            let (n, n2) = (name.to_string(), name.to_string());
            let report = m.report.trim();
            let closes = m.closes_in(crate::history::now_ms());
            // The tower refused: a session there still works or waits.
            let refused = self.full.as_ref().and_then(|f| f.notes.get(&place)).is_some_and(|(ok, text)| !ok && refused_close(text));
            items.push(
                v_flex()
                    .w_full()
                    .p(px(14.0))
                    .gap(px(10.0))
                    .rounded(px(18.0))
                    .bg(rgba(CARD))
                    .border_1()
                    .border_color(rgba(LINE))
                    .child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child("Closing report"))
                    .child(div().text_size(px(13.0)).line_height(px(19.0)).text_color(g.muted).child(if report.is_empty() { "Its goal is met. Its tower hasn’t written the report yet.".to_string() } else { report.to_string() }))
                    .child(div().text_size(px(13.0)).line_height(px(19.0)).text_color(g.muted).child(closing_line(m.workers().len(), closes.as_deref())))
                    .child(
                        acts_row()
                            .child(mbtn("space-close", if busy { "Closing…" } else { "Close this space" }, true, None).when(busy, |el| el.opacity(0.5)).when(!busy, |el| {
                                el.on_click(cx.listener(move |this, _, _, cx| this.close_space(&n, false, cx)))
                            }))
                            .when(refused && !busy, |el| {
                                el.child(mbtn("space-close-anyway", "Close anyway", false, None).on_click(cx.listener(move |this, _, _, cx| this.close_space(&n2, true, cx))))
                            }),
                    )
                    .into_any_element(),
            );
        }

        // What needs the user here (a muted space's decisions show only here).
        let queue: Vec<Decision> = all.iter().filter(|d| d.live() && d.in_space(name)).cloned().collect();
        let cards = self.queue_items(&queue, false, &|n| format!("{n} more waiting here"), "Nothing needs you here.", cx);
        items.push(bay("Needs you", cards));
        // What the user put on hold here: listed, not counted.
        let held: Vec<Decision> = self.held_decisions(cx).into_iter().filter(|d| d.in_space(name)).collect();
        if !held.is_empty() {
            let cards = self.held_items(&held, cx);
            items.push(bay(&format!("On hold · {}", held.len()), cards));
        }

        if !m.requests.is_empty() {
            let rows = m.requests.iter().enumerate().map(|(i, r)| {
                let mark = sign(if r.finished() { Chip::Done } else { Chip::Working }, 22.0, format!("request-{i}"));
                lrow(mark, nm(&r.title).into_any_element(), Some(pg(&r.progress(), false)), vec![])
            });
            items.push(bay("Requests", vec![v_flex().gap(px(2.0)).children(rows).into_any_element()]));
        }

        let workers: Vec<Member> = m.workers().into_iter().cloned().collect();
        // Each one's Claude account, and what the account failover did with it (`accounts`).
        let (board, groups) = self.usage_now(cx);
        let now = crate::history::now_ms();
        let here = groups.into_iter().find(|g| g.space.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(name)));
        let seat = |addr: &str| here.as_ref().and_then(|g| g.seats.iter().find(|s| s.address == addr));
        // Its own tower first, with its account.
        let tower_row = here.as_ref().and_then(|g| g.seats.iter().find(|s| s.tower)).map(|t| {
            let label = if t.name.is_empty() { t.address.clone() } else { t.name.clone() };
            lrow(
                super::accounts::tower_mark(&g),
                nm(&label).truncate().into_any_element(),
                Some(pg("this space's tower", false)),
                vec![super::accounts::seat_line(t, board.as_ref(), now)],
            )
            .into_any_element()
        });
        if workers.is_empty() {
            let mut list = vec![];
            list.extend(tower_row);
            list.push(empty_box("No session is open here."));
            items.push(bay("Sessions", list));
        } else {
            let mut rows: Vec<AnyElement> = vec![];
            rows.extend(tower_row);
            for (i, s) in workers.iter().enumerate() {
                let label = if s.name.is_empty() { s.address.clone() } else { s.name.clone() };
                let mut under: Vec<AnyElement> = vec![];
                // Its brief: its "now" always, the whole on "Brief ▾" (`crate::briefs`).
                if let Some(b) = &s.brief {
                    under.push(self.session_brief(&s.address, b, cx));
                }
                // Its last words: as its hooks heard them, else from the history (its brief says them when it has one).
                if s.brief.is_none()
                    && let Some(words) = s.last_words().or_else(|| last_words(&self.entries, &s.address))
                {
                    under.push(div().text_size(px(13.0)).line_height(px(18.0)).text_color(g.muted).line_clamp(2).child(words).into_any_element());
                }
                // A browser it uses now (amber when another session shares the user's).
                if let Some(b) = &s.browser {
                    let colour = if b.shared_with.is_empty() { CYAN } else { AMBER };
                    under.push(div().text_size(px(12.5)).line_height(px(17.0)).text_color(rgb(colour)).line_clamp(2).child(b.line.clone()).into_any_element());
                }
                // A job it left running: what, since when, its last progress (amber when quiet).
                if let Some(j) = s.job.as_ref().filter(|j| !s.brief.as_ref().is_some_and(|b| b.now.starts_with(j.line.trim_end_matches('.')))) {
                    let colour = if j.stuck { AMBER } else { CYAN };
                    under.push(div().text_size(px(12.5)).line_height(px(17.0)).text_color(rgb(colour)).line_clamp(2).child(j.line.clone()).into_any_element());
                }
                let place = s.place();
                if !place.is_empty() {
                    under.push(div().font_family("Menlo").text_size(px(11.5)).line_height(px(17.0)).text_color(g.faint).child(place).into_any_element());
                }
                if let Some(seat) = seat(&s.address) {
                    under.push(super::accounts::seat_line(seat, board.as_ref(), now));
                }
                let to = Scope::Session { space: name.to_string(), addr: s.address.clone() };
                rows.push(
                    lrow(
                        session_mark(if s.runs_job() { "working" } else { &s.status }, format!("session-pulse-{i}")),
                        nm(&label).truncate().into_any_element(),
                        Some(pg(status_word(s.status_shown()), false)),
                        under,
                    )
                        .id(SharedString::from(format!("session-{}", s.address)))
                        .cursor_pointer()
                        .hover(|st| st.bg(rgba(SOFT)))
                        .on_click(cx.listener(move |this, _, _, cx| this.nav_to(to.clone(), cx)))
                        .into_any_element(),
                );
            }
            items.push(bay("Sessions", vec![v_flex().gap(px(2.0)).children(rows).into_any_element()]));
        }

        // What its sessions wrote to each other: the latest of it, each message whole a click away.
        let thread = model::thread(self.tower.read(cx).state.as_ref().unwrap_or(&serde_json::Value::Null), name);
        if !thread.is_empty() {
            let count = thread.len().max(m.thread_count);
            items.push(self.thread_view(name, &thread, count, cx));
        }
        (head, items)
    }
}

impl Panel {
    /// "Where we are": a space's brief, a line per part (waits in amber,
    /// risks in rose), when it last changed, and its earlier versions on request.
    fn brief_card(&mut self, space: &str, b: &crate::briefs::SpaceBrief, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let key = format!("space:{space}");
        let open = self.full.as_ref().is_some_and(|f| f.briefs_open.contains(&key));
        let mut col = v_flex().gap(px(5.0)).w_full();
        for (label, text) in crate::briefs::space_lines(b) {
            // Its goal is in the goal card just below: here its stage.
            let (label, text) = if label == "Goal" { ("Stage", b.stage.clone()) } else { (label, text) };
            let colour = match label {
                "Waiting for you" => rgb(AMBER).into(),
                "Risks" => rgb(0xff8f8f).into(),
                _ => g.text,
            };
            col = col.child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap(px(10.0))
                    .text_size(px(13.5))
                    .line_height(px(19.0))
                    .child(div().flex_none().w(px(112.0)).font_weight(FontWeight::SEMIBOLD).text_color(g.muted).child(label))
                    .child(div().flex_1().min_w_0().text_color(colour).child(text)),
            );
        }
        let earlier = b.history.len().saturating_sub(1);
        let changed = if b.changed_ms > 0 { format!("changed {}", crate::phone::ago(b.changed_ms)) } else { String::new() };
        let k = key.clone();
        let foot = h_flex()
            .gap(px(10.0))
            .text_size(px(12.0))
            .text_color(g.faint)
            .child(changed)
            .when(earlier > 0, |el| {
                el.child(
                    div()
                        .id(SharedString::from(format!("brief-history-{space}")))
                        .cursor_pointer()
                        .text_color(rgb(CYAN))
                        .child(if open { "Hide earlier ▴".to_string() } else { format!("{earlier} earlier ▾") })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(f) = this.full.as_mut()
                                && !f.briefs_open.remove(&k)
                            {
                                f.briefs_open.insert(k.clone());
                            }
                            cx.notify();
                        })),
                )
            });
        let mut body = vec![col.into_any_element(), foot.into_any_element()];
        if open {
            for v in b.history.iter().skip(1) {
                body.push(
                    div()
                        .text_size(px(12.5))
                        .line_height(px(17.0))
                        .text_color(g.muted)
                        .child(format!("{}: {}", crate::phone::ago(v.at_ms), v.text))
                        .into_any_element(),
                );
            }
        }
        bay("Where we are", vec![v_flex().gap(px(6.0)).w_full().px(px(4.0)).children(body).into_any_element()])
    }

    /// Under a session's row: its brief's "now" (what it is doing), and the
    /// whole of it on "Brief ▾" (asked, done, next, stuck).
    fn session_brief(&mut self, address: &str, b: &crate::briefs::SessionBrief, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let open = self.full.as_ref().is_some_and(|f| f.briefs_open.contains(address));
        let lines = crate::briefs::session_lines(b);
        let a = address.to_string();
        let toggle = div()
            .id(SharedString::from(format!("brief-{address}")))
            .flex_none()
            .cursor_pointer()
            .text_size(px(12.0))
            .text_color(rgb(CYAN))
            .child(if open { "Brief ▴" } else { "Brief ▾" })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if let Some(f) = this.full.as_mut()
                    && !f.briefs_open.remove(&a)
                {
                    f.briefs_open.insert(a.clone());
                }
                cx.notify();
            }));
        let now = lines.iter().find(|(l, _)| *l == "Now").map(|(_, t)| t.clone()).unwrap_or_default();
        let mut col = v_flex().gap(px(2.0)).w_full().child(
            h_flex()
                .gap(px(8.0))
                .items_start()
                .child(div().flex_1().min_w_0().text_size(px(13.0)).line_height(px(18.0)).text_color(g.text).line_clamp(2).child(now))
                .child(toggle),
        );
        if open {
            for (label, text) in lines.into_iter().filter(|(l, _)| *l != "Now") {
                let colour = if label == "Stuck" { rgb(AMBER).into() } else { g.muted };
                col = col.child(div().text_size(px(12.5)).line_height(px(17.0)).text_color(colour).child(format!("{label}: {text}.")));
            }
        }
        col.into_any_element()
    }

    /// "Between sessions": the latest messages, newest last, each with who
    /// wrote it to whom and when. A long one shows its first lines; a click
    /// (or "Show all") shows it whole, with a way to copy it. "Earlier"
    /// shows the rest the state holds; Record has all of it.
    fn thread_view(&mut self, space: &str, thread: &[model::Message], count: usize, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let (earlier, open, copied, text_w) = match self.full.as_ref() {
            Some(f) => (f.thread_earlier.contains(space), f.thread_open.clone(), f.copied.clone(), f.text_w),
            None => (false, Default::default(), None, 600.0),
        };
        let from = if earlier { 0 } else { thread.len().saturating_sub(THREAD_ROWS) };
        let label = if count == 1 { "Between sessions · 1 message".to_string() } else { format!("Between sessions · {count} messages") };
        let link = |id: SharedString, text: String| {
            div().id(id).px(px(4.0)).text_size(px(12.0)).text_color(g.faint).cursor_pointer().hover(|s| s.text_color(rgb(CYAN))).child(text)
        };
        let sp = space.to_string();
        let head = h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(px(10.0))
            .child(bay_label(&label).flex_1().min_w(px(200.0)))
            .when(from > 0, |el| {
                el.child(link("thread-earlier".into(), format!("{from} earlier")).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = this.full.as_mut() {
                        f.thread_earlier.insert(sp.clone());
                    }
                    cx.notify();
                })))
            })
            .child(link("thread-record".into(), "All of it in Record ›".into()).on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::Record, cx))));
        let rows = thread[from..].iter().map(|msg| {
            let key = thread_key(msg);
            let long = msg_is_long(&said_text(&msg.text), text_w);
            // `HERDER_TOWER_ORB_THREAD=open`: every message whole, to look at it with test data.
            let whole = !long || open.contains(&key) || std::env::var("HERDER_TOWER_ORB_THREAD").is_ok_and(|v| v == "open");
            let (who_from, who_to) = msg.names_in(space);
            let k_open = key.clone();
            let toggle = cx.listener(move |this, _, _, cx| {
                if let Some(f) = this.full.as_mut()
                    && !f.thread_open.remove(&k_open)
                {
                    f.thread_open.insert(k_open.clone());
                }
                cx.notify();
            });
            let body = div()
                .id(SharedString::from(format!("thread-body-{key}")))
                .w_full()
                .text_size(px(13.0))
                .line_height(px(19.0))
                .text_color(if msg.delivered { g.text } else { g.muted })
                .when(!whole, |el| el.line_clamp(4).text_ellipsis())
                .when(long, |el| el.cursor_pointer())
                .child(said_text(&msg.text));
            let body = if long { body.on_click(toggle).into_any_element() } else { body.into_any_element() };
            let (k_copy, text) = (key.clone(), said_text(&msg.text));
            let k_more = key.clone();
            h_flex()
                .w_full()
                .items_start()
                .gap(px(10.0))
                .p(px(12.0))
                .rounded(px(14.0))
                .bg(rgba(SOFT))
                .border_1()
                .border_color(rgba(if whole && long { LINE } else { 0xffffff0d }))
                // Who wrote it, in that session's colour (the same in every message).
                .child(div().w(px(3.0)).flex_none().self_stretch().rounded_full().bg(rgb(sender_colour(&msg.from))))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(6.0))
                        .child(
                            h_flex()
                                .w_full()
                                .items_start()
                                .gap(px(10.0))
                                .child(
                                    h_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .flex_wrap()
                                        .items_center()
                                        .gap_x(px(6.0))
                                        .text_size(px(13.0))
                                        .line_height(px(18.0))
                                        .child(div().font_weight(FontWeight::SEMIBOLD).child(who_from))
                                        .child(div().text_color(g.faint).child("→"))
                                        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(g.muted).child(who_to)),
                                )
                                .when(!msg.delivered, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .px(px(8.0))
                                            .py(px(1.0))
                                            .rounded_full()
                                            .border_1()
                                            .border_color(rgb(AMBER))
                                            .text_size(px(11.0))
                                            .text_color(rgb(AMBER))
                                            .child("Not delivered"),
                                    )
                                })
                                .child(div().flex_none().text_size(px(12.0)).line_height(px(18.0)).text_color(g.faint).child(when_said(msg.at))),
                        )
                        .child(body)
                        .when(long, |el| {
                            el.child(
                                h_flex()
                                    .gap(px(4.0))
                                    .ml(px(-4.0))
                                    .child(link(SharedString::from(format!("thread-more-{k_more}")), if whole { "Show less".into() } else { "Show all".into() }).on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            if let Some(f) = this.full.as_mut()
                                                && !f.thread_open.remove(&k_more)
                                            {
                                                f.thread_open.insert(k_more.clone());
                                            }
                                            cx.notify();
                                        },
                                    )))
                                    .when(whole, |el| {
                                        let said = copied.as_deref() == Some(k_copy.as_str());
                                        el.child(link(SharedString::from(format!("thread-copy-{k_copy}")), if said { "Copied".into() } else { "Copy".into() }).on_click(cx.listener(
                                            move |this, _, _, cx| {
                                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                                                if let Some(f) = this.full.as_mut() {
                                                    f.copied = Some(k_copy.clone());
                                                }
                                                cx.notify();
                                            },
                                        )))
                                    }),
                            )
                        }),
                )
                .into_any_element()
        });
        v_flex().w_full().gap(px(10.0)).child(head).child(v_flex().w_full().gap(px(8.0)).children(rows)).into_any_element()
    }
}

/// A message's own key in the view (what is open, what was copied).
fn thread_key(m: &model::Message) -> String {
    format!("{}-{}-{}", m.at, m.from, m.to)
}

/// What a message says, without the "From t7r (Android). " it opens with:
/// the row names who wrote it already. Only an opening the tower reads as
/// a session's signature (`peers::sender_in`) goes: "From t7h: …", "From
/// t2. …", "From t7r (Android). …", "From Shop › web (t7h): …".
fn said_text(text: &str) -> String {
    let t = text.trim();
    if crate::peers::sender_in(t).is_none() {
        return t.to_string();
    }
    let head: String = t.lines().next().unwrap_or("").chars().take(200).collect();
    // Where the signature ends: its first ": " or ". ", or past the brackets that come before it.
    let Some(stop) = [": ", ". "].iter().filter_map(|p| head.find(p)).min() else { return t.to_string() };
    let rest = match head[..stop].find('(') {
        Some(open) => match head[open..].find(')') {
            Some(close) => t[open + close + 1..].trim_start_matches([':', '.']),
            None => return t.to_string(),
        },
        None => &t[stop + 2..],
    };
    let rest = rest.trim();
    if rest.is_empty() { t.to_string() } else { rest.to_string() }
}

/// Longer than the four lines a message shows, at `width` points across:
/// its lines as they wrap, at a little more than the 13-point text's
/// average letter, so a message that might be cut is always offered whole.
fn msg_is_long(text: &str, width: f32) -> bool {
    let per_line = ((width / 7.0) as usize).max(30);
    let lines: usize = text.trim().lines().map(|l| l.chars().count().div_ceil(per_line).max(1)).sum();
    lines > 4
}

/// "7:50 PM · 12m ago" today, else the day and the time.
pub(super) fn when_said(at: u64) -> String {
    if at == 0 {
        return String::new();
    }
    let day = super::day_label(at);
    if day == "Today" { format!("{} · {}", super::clock(at), super::ago(at)) } else { format!("{day}, {}", super::clock(at)) }
}

/// A session's colour in the thread, the same for it in every message.
fn sender_colour(address: &str) -> u32 {
    const COLOURS: [u32; 5] = [CYAN, AMBER, 0xb9a7ff, 0x8fe3b0, 0xff9fb2];
    let n = address.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    COLOURS[n as usize % COLOURS.len()]
}

/// How the goal now came to be, and the versions before it (newest first):
/// the history's last version is the goal now when it says the same.
pub(super) fn goal_versions<'a>(goal: &str, done_when: &str, history: &'a [GoalStep]) -> (Option<&'a GoalStep>, Vec<&'a GoalStep>) {
    match history.split_last() {
        Some((last, before)) if last.goal.trim() == goal.trim() && last.done_when.trim() == done_when.trim() => (Some(last), before.iter().rev().collect()),
        _ => (None, history.iter().rev().collect()),
    }
}

/// `HERDER_TOWER_ORB_GOALS=open` (looking at it with test data): every Goal
/// card shows its earlier versions.
fn goals_open_from_start() -> bool {
    std::env::var("HERDER_TOWER_ORB_GOALS").is_ok_and(|v| v.trim() == "open")
}

/// The head of the Goal card: "updated by your request at 3:55 PM" (none
/// when how it was set isn't known; `first`: it is the goal the space began
/// with).
pub(super) fn goal_said(now: Option<&GoalStep>, first: bool) -> Option<String> {
    let s = now.filter(|s| s.at_ms > 0)?;
    Some(format!("{} at {}", s.words(first), super::talk::when(s.at_ms)))
}

/// What a goal does to a space opened by a chat (`Mission::chat`), under
/// its name: none for any other space.
pub(super) fn chat_line(m: &Mission) -> Option<&'static str> {
    if !m.chat || m.standing || m.goal.trim().is_empty() {
        return None;
    }
    Some(match m.goal_set_by_user() {
        false => "Goal written from your requests. Sessions keep working in the checkout, and it won't close on this goal until you set one.",
        true => "Sessions here keep working in the checkout.",
    })
}

impl Panel {
    /// A space's Goal card: who last set the goal and when ("updated by
    /// your request at 3:55 PM"), the goal, what counts as done as a list,
    /// the request that changed it; and its earlier versions, folded.
    fn goal_card(&self, m: &Mission, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let (now, earlier) = goal_versions(&m.goal, &m.done_when, &m.goal_history);
        let open = self.full.as_ref().is_some_and(|f| f.goals_open.contains(&m.name)) || goals_open_from_start();
        let name = m.name.clone();
        let quote = |text: &str| format!("“{}”", crate::launch::clip_words(text.trim(), 220));
        let version = |s: &GoalStep| {
            v_flex()
                .gap(px(3.0))
                .pl(px(12.0))
                .border_l_2()
                .border_color(rgba(LINE))
                .child(div().text_size(px(12.0)).text_color(g.faint).child(format!("{} · {}", super::talk::when(s.at_ms), s.words(m.goal_history.first().is_some_and(|f| std::ptr::eq(f, s))))))
                .child(div().text_size(px(13.5)).line_height(px(19.0)).text_color(g.muted).child(s.goal.clone()))
                .when(!s.done_when.trim().is_empty(), |el| el.child(div().text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(format!("Done when: {}", s.done_when.trim()))))
                .when(s.by == "request" && !s.request.trim().is_empty(), |el| {
                    el.child(div().text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(format!("Your request: {}", quote(&s.request))))
                })
        };
        let done = done_items(m.done_when.trim());
        let n = earlier.len();
        v_flex()
            .w_full()
            .px(px(16.0))
            .py(px(14.0))
            .gap(px(10.0))
            .rounded(px(18.0))
            .bg(rgba(CARD))
            .border_1()
            .border_color(rgba(LINE))
            .child(
                h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.0))
                    .child(bay_label("Goal").px(px(0.0)))
                    .children(goal_said(now, earlier.is_empty()).map(|said| div().text_size(px(12.0)).text_color(g.faint).child(said))),
            )
            .child(div().text_size(px(16.0)).line_height(px(23.0)).font_weight(FontWeight::SEMIBOLD).text_color(g.text).child(m.goal.trim().to_string()))
            .children(now.filter(|s| s.by == "request" && !s.request.trim().is_empty()).map(|s| {
                let why = if s.why.trim().is_empty() { String::new() } else { format!(" ({})", s.why.trim().trim_end_matches('.')) };
                div().text_size(px(12.5)).line_height(px(18.0)).text_color(g.muted).child(format!("Your request: {}{why}", quote(&s.request)))
            }))
            .when(!done.is_empty(), |el| {
                el.child(div().pt(px(4.0)).child(bay_label("Done when").px(px(0.0)))).child(v_flex().gap(px(8.0)).children(done.into_iter().map(|d| {
                    h_flex()
                        .items_start()
                        .gap(px(10.0))
                        .text_size(px(13.5))
                        .line_height(px(20.0))
                        .child(div().flex_none().text_color(g.faint).child("•"))
                        .child(div().flex_1().min_w_0().text_color(g.text).child(d))
                })))
            })
            .when(n > 0, |el| {
                el.child(
                    h_flex().pt(px(2.0)).child(
                        tg("goal-earlier", &format!("{} earlier version{}", n, if n == 1 { "" } else { "s" }), open).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(full) = this.full.as_mut()
                                && !full.goals_open.remove(&name)
                            {
                                full.goals_open.insert(name.clone());
                            }
                            cx.notify();
                        })),
                    ),
                )
                .when(open, |el| el.child(v_flex().gap(px(10.0)).children(earlier.into_iter().map(version))))
            })
            .into_any_element()
    }
}

/// What counts as done, a condition each: split where the goal joined them
/// ("; ", or its lines), without list marks.
fn done_items(done_when: &str) -> Vec<String> {
    done_when
        .split(['\n', ';'])
        .map(|s| s.trim().trim_start_matches(['-', '*', '•']).trim())
        .map(|s| s.trim_end_matches('…').trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_goal_card_says_how_the_goal_came_to_be_and_folds_the_versions_before() {
        let step = |goal: &str, by: &str, at_ms: u64| GoalStep { goal: goal.into(), by: by.into(), at_ms, ..Default::default() };
        let h = vec![step("Export cases as PDF", "launch", 1), step("Export cases as PDF and CSV", "request", 2)];
        let (now, earlier) = goal_versions("Export cases as PDF and CSV", "", &h);
        assert_eq!(now.map(|s| s.by.as_str()), Some("request"));
        assert_eq!(earlier.iter().map(|s| s.goal.as_str()).collect::<Vec<_>>(), ["Export cases as PDF"]);
        assert!(goal_said(now, false).unwrap().starts_with("updated by your request at "));
        // A goal the history doesn't end with (set by an older tower): every version is an earlier one.
        let (now, earlier) = goal_versions("Something else", "", &h);
        assert!(now.is_none() && earlier.len() == 2 && goal_said(now, false).is_none());
        assert_eq!(step("x", "you", 1).by_words(), "set by you");
    }

    #[test]
    fn a_message_is_long_when_it_wraps_past_four_lines_at_the_window_width() {
        let text = "word ".repeat(74); // 370 letters
        assert!(!msg_is_long(&text, 800.0), "about 3 lines in a wide window");
        assert!(msg_is_long(&text, 500.0), "about 6 lines in a narrow one");
        assert!(msg_is_long("a\nb\nc\nd\ne", 800.0), "five short lines");
        assert!(!msg_is_long("a\nb\nc\nd", 800.0));
    }

    #[test]
    fn a_message_says_what_it_says_without_who_wrote_it() {
        assert_eq!(said_text("From t7r (Android). Checked against the real tower."), "Checked against the real tower.");
        assert_eq!(said_text("From t7p (Mac tower): installed."), "installed.");
        assert_eq!(said_text("From t2. The build is green"), "The build is green");
        // Not an opening of that shape: as it is.
        assert_eq!(said_text("From now on, run the tests first."), "From now on, run the tests first.");
        assert_eq!(said_text("From t7r (Android)."), "From t7r (Android).");
        assert_eq!(said_text("  The api is on 3001 "), "The api is on 3001");
        assert_eq!(said_text("From Shop › web (t7h): the build is green"), "the build is green");
        assert_eq!(said_text("From t7h: fixed (see PR 12). Next is the editor."), "fixed (see PR 12). Next is the editor.");
        assert_eq!(said_text("From the orb session: hello"), "From the orb session: hello", "not a session's signature");
    }

    #[test]
    fn done_when_is_a_list_a_condition_each() {
        assert_eq!(
            done_items("Incoming support messages have a Translate Message option; the user's message language is detected automatically;  "),
            ["Incoming support messages have a Translate Message option", "The user's message language is detected automatically"]
        );
        assert_eq!(done_items("- one\n- two"), ["One", "Two"]);
        assert!(done_items("").is_empty());
    }

    #[test]
    fn a_session_row_says_how_it_stands() {
        assert_eq!((status_word("working"), status_word("blocked"), status_word("done"), status_word("")), ("working", "waits", "done", "idle"));
        assert!(closing_line(2, None).starts_with("Closing stops its 2 sessions and removes their worktrees"));
        assert!(closing_line(1, None).starts_with("Closing stops its session and removes its worktree"));
        assert!(closing_line(0, None).starts_with("Closing archives it"));
        // It closes by itself only when the tower says when.
        assert!(closing_line(2, Some("in 5 h")).ends_with("Left alone, it closes by itself in 5 h."));
        assert!(!closing_line(2, None).contains("by itself"));
    }

    #[test]
    fn a_close_says_what_it_did() {
        // The receipt `tower space close` prints when piped.
        let out = r#"{
          "closed": "fix-login",
          "sessions_ended": [{ "address": "t4", "name": "fix-login-eyzflow" }, { "address": "t5", "name": "fix-login-mediaroom" }],
          "sessions_left": [],
          "herdr_space": "closed",
          "worktrees_removed": ["/w/.tower-worktrees/fix-login/eyzflow"],
          "kept_worktrees": [{ "path": "/w/.tower-worktrees/fix-login/mediaroom", "branch": "fix-login/mediaroom", "repo": "/w/mediaroom", "why": "it has uncommitted changes" }],
          "requests_closed": ["a7"],
          "report": "The redirect is fixed and merged into dev."
        }"#;
        assert_eq!(
            closed_line("fix-login", out),
            "fix-login is closed. 2 sessions ended. 1 worktree removed (the branches stay). The worktree fix-login/mediaroom is kept: it has uncommitted changes. Its closing report is in Record."
        );
        let left = r#"{ "closed": "isa", "sessions_ended": [], "sessions_left": [{ "address": "t9", "name": "clips", "why": "hc kill: no such terminal." }], "worktrees_removed": [], "kept_worktrees": [], "report": "" }"#;
        assert_eq!(closed_line("isa", left), "isa is closed. clips still runs: hc kill: no such terminal. It had no closing report.");
        // Words, not a receipt (a terminal's output): as they are.
        assert_eq!(closed_line("isa", "Closed \"isa\".\nEnded: clips (t9).\n"), "Closed \"isa\". Ended: clips (t9).");
        assert_eq!(closed_line("isa", ""), "isa is closed.");
        let resolved = r#"{"closed":"isa","resolved":true,"sessions_ended":[{"address":"t9","name":"clips"},{"address":"t10","name":"shell"}],"sessions_left":[],"worktrees_removed":[],"kept_worktrees":[],"report":"Clips are live."}"#;
        assert_eq!(closed_line("isa", resolved), "isa is resolved: its goal is achieved, and it is closed. 2 sessions ended. Its closing report is in Record.");
    }

    #[test]
    fn resolve_asks_with_everything_it_would_end() {
        let v = serde_json::json!({
            "space": "isa",
            "ends": [
                { "address": "t9", "name": "clips", "status": "working", "agent": true, "busy": true },
                { "address": "t10", "name": "shell", "status": "", "agent": false, "busy": false },
                { "address": "t11", "name": "docs", "status": "blocked", "agent": true, "busy": true },
                { "address": "t12", "name": "t12", "status": "done", "agent": true, "busy": false }
            ],
            "tower": { "address": "t13", "name": "tower-isa" },
            "stays": [{ "address": "t1", "name": "tower" }],
            "busy": [{ "address": "t9", "name": "clips", "status": "working" }, { "address": "t11", "name": "docs", "status": "blocked" }],
            "worktrees": [
                { "path": "/w/.tower-worktrees/isa/app", "branch": "isa/app", "removed": true },
                { "path": "/w/.tower-worktrees/isa/api", "branch": "isa/api", "removed": false, "why": "2 files not committed." }
            ],
            "herdr_space": "kept"
        });
        let (lines, held) = resolve_lines(&v);
        assert_eq!(
            lines,
            [
                "Ends its own tower, tower-isa (t13).",
                "Ends clips (t9): a session at work.",
                "Ends shell (t10): an open terminal.",
                "Ends docs (t11): a session waiting for you.",
                "Ends t12: a session, done.",
                "tower (t1) stays: it is the tower’s own.",
                "Removes the worktree isa/app (its branch stays).",
                "Keeps the worktree isa/api: 2 files not committed.",
                "Herdr’s space stays: it holds the tower’s own terminal.",
            ]
        );
        assert_eq!(held, ["clips (t9) is still working", "docs (t11) waits for you"]);
        let (lines, held) = resolve_lines(&serde_json::json!({ "ends": [], "herdr_space": "closed" }));
        assert_eq!((lines, held), (vec!["No session or terminal is open there.".to_string(), "Herdr’s space closes.".to_string()], vec![]));
        // A refusal names the way round it.
        assert!(refused_close("tower space: \"isa\" isn't closed: clips (t9) is working. Wait for them, or close it anyway with --force (their work in progress is cut off)."));
        assert!(!refused_close("tower space: no space called \"isa\""));
    }
}
