//! The record (`docs/quiet-tower.md`, the Record lane; W13 and W14 in the
//! design): everything the tower did and heard, kept for when the user wants
//! it. It is the history (`history.jsonl`) as rows, newest first, each with
//! its time, what it is, where and who (`space › who`), and its words; chips
//! narrow it to one space, and opened from inside a space it starts there.
//!
//! What a row is called (`What`): what you asked and what the tower replied;
//! what finished, started, failed or needs you; what one session wrote to
//! another (`peer`, and one that couldn't be handed over); what the tower
//! answered for you; screenshots (those a session sent together are one
//! row); the tower's notes (`note`: what its brain wrote on a turn that was
//! only for the record) and receipts; a decision it raised (`raised`) and
//! how one ended (`decision`: answered, dismissed, or over without an
//! answer); a space going quiet, ready to close, or closed with its report
//! (`space`); an instruction queued or delivered (`queue`). Anything else
//! goes by its own kind's name.
//!
//! The rows are read from the file as it is (not through `history::Entry`),
//! so a field a newer daemon adds is there.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::{h_flex, v_flex};
use serde_json::Value;

use super::home::{LINE, bay_label, empty_box, tg};
use super::session::is_test;
use super::window::whead;
use super::{AMBER, CYAN, NAVY, PALE, Panel, ROSE, clock, day_label, glass};
use crate::history;

/// The newest rows the record draws (the rest is in the file).
const SHOWN: usize = 250;
/// Screenshots one session sent within this long of the first are one row (ms).
const TOGETHER_MS: u64 = 2 * 60 * 1000;

/// What a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum What {
    Asked,
    /// What the user typed in a session's own terminal: their request there.
    Typed,
    /// A question the user answered in its session's own terminal.
    AnsweredThere,
    /// A reply the tower held, not sent: the user answered in the terminal.
    NotSent,
    Replied,
    Finished,
    Started,
    Failed,
    NeedsYou,
    /// One session to another.
    Peer,
    /// A question the tower answered itself.
    Answered,
    Shots,
    Note,
    Receipt,
    /// What the tower opened for a request (`crate::acts`), and its undo.
    Opened,
    Undone,
    /// A decision the tower put to the user.
    Raised,
    /// How a decision ended: answered, dismissed, or over without an answer.
    Decided,
    Dismissed,
    Over,
    /// A space: its last session went, its goal is met, it was closed.
    SpaceQuiet,
    SpaceReady,
    SpaceClosed,
    /// A message between sessions that couldn't be handed over.
    Undelivered,
    /// A session that only says it is ready.
    Ready,
    /// An instruction waiting for its session, and one handed over.
    Queued,
    Delivered,
    Report,
    Summary,
    Alert,
    PullRequest,
    Behavior,
    /// Anything else: called by its kind.
    Other,
}

impl What {
    /// What an entry of `kind` with `status` is.
    pub(super) fn of(kind: &str, status: &str) -> What {
        match (kind, status) {
            ("request", _) => What::Asked,
            ("answer", _) => What::Replied,
            ("receipt", _) => What::Receipt,
            ("note", _) => What::Note,
            ("peer", "undelivered") => What::Undelivered,
            ("peer", _) => What::Peer,
            ("raised", _) => What::Raised,
            ("decision", "answered") => What::Decided,
            ("decision", "dismissed") => What::Dismissed,
            ("decision", _) => What::Over,
            ("act", "undone") => What::Undone,
            ("act", _) => What::Opened,
            ("space", "quiet") => What::SpaceQuiet,
            ("space", "ready") => What::SpaceReady,
            ("space", "closed") => What::SpaceClosed,
            ("queue", "queued") => What::Queued,
            ("queue", "not-sent") => What::NotSent,
            ("queue", "delivered" | "sent") => What::Delivered,
            ("alert", "ready") => What::Ready,
            ("answered", _) => What::Answered,
            ("screenshot", _) => What::Shots,
            ("summary", _) => What::Summary,
            ("pr", _) => What::PullRequest,
            ("behavior", _) => What::Behavior,
            ("job", "working") => What::Started,
            ("job" | "watch" | "alert", "done" | "idle") => What::Finished,
            ("job" | "watch" | "alert", "blocked") => What::NeedsYou,
            ("job" | "watch" | "alert", "failed" | "error" | "exited") => What::Failed,
            ("job" | "watch", _) => What::Report,
            ("alert", _) => What::Alert,
            _ => What::Other,
        }
    }
}

/// One row of the record.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Row {
    pub at: u64,
    pub what: What,
    /// The entry's own kind (what an `Other` row is called by).
    pub kind: String,
    /// Empty when it is no space's.
    pub space: String,
    pub who: String,
    pub text: String,
}

impl Row {
    /// What the row is called.
    pub(super) fn label(&self) -> String {
        match self.what {
            What::Asked => "You asked".into(),
            What::Typed => "You typed".into(),
            What::AnsweredThere => "You answered".into(),
            What::NotSent => "Not sent".into(),
            What::Replied => "Tower replied".into(),
            What::Finished => "Finished".into(),
            What::Started => "Started".into(),
            What::Failed => "Failed".into(),
            What::NeedsYou => "Needs you".into(),
            What::Peer => "Between sessions".into(),
            What::Answered => "Answered for you".into(),
            What::Shots => "Screenshots".into(),
            What::Note => "Note".into(),
            What::Receipt => "Receipt".into(),
            What::Opened => "Opened".into(),
            What::Undone => "Undone".into(),
            What::Raised => "Decision raised".into(),
            What::Decided => "Decision answered".into(),
            What::Dismissed => "Decision dismissed".into(),
            What::Over => "Decision over".into(),
            What::SpaceQuiet => "Space quiet".into(),
            What::SpaceReady => "Ready to close".into(),
            What::SpaceClosed => "Space closed".into(),
            What::Undelivered => "Not delivered".into(),
            What::Ready => "Ready".into(),
            What::Queued => "Queued".into(),
            What::Delivered => "Delivered".into(),
            What::Report => "Report".into(),
            What::Summary => "Summary".into(),
            What::Alert => "Alert".into(),
            What::PullRequest => "Pull request".into(),
            What::Behavior => "Behavior".into(),
            What::Other => {
                let mut c = self.kind.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_else(|| "Entry".into())
            }
        }
    }

    /// `sandbox › eyzflow`, or whichever of the two there is.
    pub(super) fn place(&self) -> String {
        match (self.space.is_empty(), self.who.is_empty()) {
            (false, false) => format!("{} › {}", self.space, self.who),
            (false, true) => self.space.clone(),
            (true, _) => self.who.clone(),
        }
    }
}

/// The history's entries (as its lines read, oldest first) as rows, newest
/// first. Test sessions and entries with nothing to say are left out;
/// screenshots one session sent together are one row.
pub(super) fn rows(entries: &[Value]) -> Vec<Row> {
    let s = |e: &Value, k: &str| e[k].as_str().unwrap_or("").trim().to_string();
    let mut out: Vec<Row> = vec![];
    // The run of screenshots being gathered: its session, when it began, how many, its row.
    let mut run: Option<(String, u64, usize, usize)> = None;
    for e in entries {
        let (kind, status) = (s(e, "kind"), s(e, "status"));
        let at = e["at"].as_u64().unwrap_or(0);
        let (session, name) = (s(e, "session"), s(e, "name"));
        if kind.is_empty() || is_test(&name) {
            continue;
        }
        // Typed, or answered, in a session's own terminal.
        let there = s(e, "source").starts_with("terminal:");
        let what = match What::of(&kind, &status) {
            What::Asked if there => What::Typed,
            What::Decided if there => What::AnsweredThere,
            w => w,
        };
        // A session by what it is within its space (`crate::names`): the row's space says the rest.
        let within = Some(s(e, "space")).filter(|sp| !sp.is_empty());
        let named = if name.is_empty() { session.clone() } else { crate::names::label(within.as_deref(), &name) };
        let who = match what {
            What::Asked => "you".to_string(),
            What::Peer | What::Undelivered => {
                let from = Some(s(e, "from_name")).filter(|n| !n.is_empty()).map(|n| crate::names::label(within.as_deref(), &n)).unwrap_or_else(|| s(e, "from"));
                match (from.is_empty(), named.is_empty()) {
                    (false, false) => format!("{from} → {named}"),
                    (false, true) => from,
                    (true, _) => named.clone(),
                }
            }
            _ if named.is_empty() && matches!(what, What::Replied | What::Receipt | What::Note | What::Raised) => "tower".to_string(),
            _ => named.clone(),
        };
        let text = match what {
            What::Shots => {
                // One more of a run: its row counts it.
                if let Some(r) = run.as_mut().filter(|r| r.0 == session && at.saturating_sub(r.1) <= TOGETHER_MS) {
                    r.2 += 1;
                    let (n, i) = (r.2, r.3);
                    let caption = out[i].text.split_once(": ").map(|(_, c)| c.to_string()).unwrap_or_default();
                    out[i].text = if caption.is_empty() { format!("{n} screenshots") } else { format!("{n} screenshots: {caption}") };
                    out[i].at = at;
                    continue;
                }
                let caption = Some(s(e, "caption")).filter(|c| !c.is_empty()).unwrap_or_else(|| s(e, "text"));
                run = Some((session.clone(), at, 1, out.len()));
                if caption.is_empty() { "1 screenshot".to_string() } else { format!("1 screenshot: {caption}") }
            }
            What::Asked if s(e, "text").is_empty() && e["attachments"].as_array().is_some_and(|a| !a.is_empty()) => "(files)".to_string(),
            What::Raised => match (s(e, "why"), s(e, "text")) {
                (why, text) if !why.is_empty() && !text.is_empty() => format!("{why}: {text}"),
                (_, text) => text,
            },
            // `<what was asked> -> <what it was answered with>`, as the daemon writes it.
            What::Decided | What::Dismissed | What::Over | What::AnsweredThere => s(e, "text").replacen(" -> ", " → ", 1),
            What::Finished | What::Report | What::NeedsYou | What::Failed if kind == "watch" => crate::chat::report_text(&s(e, "text")).trim().to_string(),
            _ => s(e, "text"),
        };
        if what != What::Shots {
            run = None;
        }
        if text.is_empty() {
            continue;
        }
        out.push(Row { at, what, kind, space: s(e, "space"), who, text });
    }
    out.reverse();
    out
}

/// The rows of one space (`None`: of every space).
pub(super) fn of_space<'a>(rows: &'a [Row], space: Option<&str>) -> Vec<&'a Row> {
    rows.iter().filter(|r| space.is_none_or(|s| r.space == s)).collect()
}

/// The spaces the rows are of, the one heard from last first.
pub(super) fn spaces(rows: &[Row]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for r in rows.iter().filter(|r| !r.space.is_empty()) {
        if !out.contains(&r.space) {
            out.push(r.space.clone());
        }
    }
    out
}

/// The history as rows, read now.
pub(super) fn load() -> Vec<Row> {
    let Ok(text) = std::fs::read_to_string(history::path()) else { return vec![] };
    let entries: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    rows(&entries)
}

impl Panel {
    /// The record's head and body.
    pub(super) fn record_view(&mut self, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        // The time, kind and place beside the text need about this much of the pane.
        let wide = self.full.as_ref().map(|f| f.pane_w).unwrap_or(900.0) >= super::window::TWO_COLS_W;
        let g = glass();
        let filter = self.tower.read(cx).nav.rec_filter.clone();
        let Some(full) = self.full.as_ref() else { return (whead("Record", vec![], None), vec![]) };
        let all = &full.record;
        let mine = of_space(all, filter.as_deref());
        let head = whead(
            "Record",
            vec![div().child(format!("Everything the tower did and heard, kept for when you want it. {} of {} entries.", mine.len(), all.len())).into_any_element()],
            None,
        );
        // The spaces, as a choice; the one the record was opened on is there even with nothing yet.
        let mut names = spaces(all);
        if let Some(f) = filter.as_ref().filter(|f| !names.contains(f)) {
            names.insert(0, f.clone());
        }
        let pick = |id: SharedString, text: &str, to: Option<String>, on: bool| {
            tg(id, text, on).on_click(cx.listener(move |this, _, _, cx| {
                this.tower.update(cx, |t, cx| {
                    t.nav.rec_filter = to.clone();
                    cx.notify();
                });
            }))
        };
        let chips = h_flex()
            .flex_wrap()
            .gap(px(6.0))
            .child(pick("record-all".into(), "All spaces", None, filter.is_none()))
            .children(names.iter().take(14).enumerate().map(|(i, n)| pick(SharedString::from(format!("record-space-{i}")), n, Some(n.clone()), filter.as_deref() == Some(n.as_str()))))
            .into_any_element();
        if mine.is_empty() {
            let none = if filter.is_some() { "Nothing is recorded for this space yet." } else { "Nothing is recorded yet." };
            return (head, vec![chips, empty_box(none)]);
        }
        let mut list = v_flex().w_full();
        let mut day = String::new();
        for r in mine.iter().take(SHOWN) {
            let d = day_label(r.at);
            if d != day {
                list = list.child(bay_label(&d).pt(px(if day.is_empty() { 0.0 } else { 14.0 })).pb(px(6.0)));
                day = d;
            }
            let pill = div().flex_none().px(px(8.0)).py(px(1.0)).rounded_full().border_1().text_size(px(11.0)).line_height(px(15.0));
            let pill = match r.what {
                What::Asked | What::Typed => pill.bg(rgb(PALE)).border_color(rgb(PALE)).text_color(rgb(NAVY)),
                What::Answered | What::Decided | What::AnsweredThere | What::SpaceReady => pill.border_color(rgb(CYAN)).text_color(rgb(CYAN)),
                What::Raised | What::NeedsYou => pill.border_color(rgb(AMBER)).text_color(rgb(AMBER)),
                What::Failed | What::Undelivered => pill.border_color(rgb(ROSE)).text_color(rgb(ROSE)),
                _ => pill.border_color(rgba(LINE)).text_color(g.muted),
            };
            // Its columns side by side when the pane has room for the text beside them; else the text under them.
            let row = if wide {
                h_flex()
                    .w_full()
                    .gap(px(12.0))
                    .items_start()
                    .child(div().w(px(58.0)).flex_none().text_color(g.faint).child(clock(r.at)))
                    .child(div().w(px(118.0)).flex_none().flex().child(pill.child(r.label())))
                    .child(div().w(px(170.0)).flex_none().text_color(g.muted).child(r.place()))
                    .child(div().flex_1().min_w_0().line_clamp(4).child(r.text.clone()))
                    .into_any_element()
            } else {
                v_flex()
                    .w_full()
                    .gap(px(5.0))
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .items_center()
                            .gap(px(10.0))
                            .child(div().flex_none().text_color(g.faint).child(clock(r.at)))
                            .child(div().flex_none().flex().child(pill.child(r.label())))
                            .child(div().min_w_0().text_color(g.muted).child(r.place())),
                    )
                    .child(div().w_full().line_clamp(4).child(r.text.clone()))
                    .into_any_element()
            };
            list = list.child(div().w_full().px(px(4.0)).py(px(9.0)).border_t_1().border_color(rgba(LINE)).text_size(px(13.0)).line_height(px(18.0)).child(row));
        }
        let mut items = vec![chips, list.into_any_element()];
        if mine.len() > SHOWN {
            items.push(div().px(px(4.0)).text_size(px(12.0)).text_color(g.faint).font_weight(FontWeight::NORMAL).child(format!("The {SHOWN} latest are shown. The rest is in history.jsonl.")).into_any_element());
        }
        (head, items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn history() -> Vec<Value> {
        vec![
            json!({ "at": 10, "kind": "request", "text": "Open a space named sandbox with three sessions.", "source": "voice" }),
            json!({ "at": 20, "kind": "answer", "text": "Opened sandbox." }),
            json!({ "at": 30, "kind": "job", "status": "working", "session": "t1", "name": "eyzflow", "space": "sandbox", "text": "Plan the live editor" }),
            json!({ "at": 40, "kind": "peer", "session": "t1", "name": "eyzflow", "from": "t2", "from_name": "mediaroom", "space": "sandbox", "status": "delivered", "text": "Answers on stale writes.", "record": true }),
            json!({ "at": 50, "kind": "answered", "session": "t2", "name": "mediaroom", "space": "isa", "text": "Start with the small fix." }),
            json!({ "at": 60, "kind": "screenshot", "session": "t3", "name": "clips", "space": "isa", "caption": "The Ingest walk on dev", "path": "/a/1.png" }),
            json!({ "at": 61, "kind": "screenshot", "session": "t3", "name": "clips", "space": "isa", "caption": "The same, scrolled", "path": "/a/2.png" }),
            json!({ "at": 62, "kind": "screenshot", "session": "t3", "name": "clips", "space": "isa", "path": "/a/3.png" }),
            json!({ "at": 70, "kind": "job", "status": "done", "session": "t1", "name": "eyzflow", "space": "sandbox", "text": "Put all five additions into the plan.", "record": true }),
            json!({ "at": 80, "kind": "note", "session": "t1", "name": "eyzflow", "space": "sandbox", "text": "The plan is finished and checked.", "record": true }),
            json!({ "at": 90, "kind": "raised", "session": "t4", "name": "release", "space": "isa", "why": "Merge", "text": "Merge PR #42 into main?" }),
            json!({ "at": 100, "kind": "receipt", "text": "The login fix is merged into dev.", "agenda": "a3", "space": "isa" }),
            json!({ "at": 110, "kind": "job", "status": "done", "session": "t9", "name": "qfix-one", "text": "a test session" }),
            json!({ "at": 120, "kind": "job", "status": "done", "session": "t1", "name": "eyzflow", "space": "sandbox", "text": " " }),
            json!({ "at": 130, "kind": "queue", "session": "t1", "space": "sandbox", "text": "delivered to t1" }),
            json!({ "at": 140, "kind": "request", "text": "", "attachments": ["/a/b.png"] }),
        ]
    }

    #[test]
    fn the_history_reads_as_rows_newest_first() {
        let rows = rows(&history());
        let got: Vec<(u64, String, String, &str)> = rows.iter().map(|r| (r.at, r.label(), r.place(), r.text.as_str())).collect();
        let want: Vec<(u64, &str, &str, &str)> = vec![
            (140, "You asked", "you", "(files)"),
            (130, "Queue", "sandbox › t1", "delivered to t1"),
            (100, "Receipt", "isa › tower", "The login fix is merged into dev."),
            (90, "Decision raised", "isa › release", "Merge: Merge PR #42 into main?"),
            (80, "Note", "sandbox › eyzflow", "The plan is finished and checked."),
            (70, "Finished", "sandbox › eyzflow", "Put all five additions into the plan."),
            (62, "Screenshots", "isa › clips", "3 screenshots: The Ingest walk on dev"),
            (50, "Answered for you", "isa › mediaroom", "Start with the small fix."),
            (40, "Between sessions", "sandbox › mediaroom → eyzflow", "Answers on stale writes."),
            (30, "Started", "sandbox › eyzflow", "Plan the live editor"),
            (20, "Tower replied", "tower", "Opened sandbox."),
            (10, "You asked", "you", "Open a space named sandbox with three sessions."),
        ];
        assert_eq!(got.iter().map(|(a, l, p, t)| (*a, l.as_str(), p.as_str(), *t)).collect::<Vec<_>>(), want);
    }

    #[test]
    fn what_a_quiet_tower_writes_down_reads_as_what_it_is() {
        // As the daemon writes them (`docs/quiet-tower.md`): each with `record: true` unless it was told.
        let entries = vec![
            json!({ "at": 10, "kind": "raised", "id": "d0123456789ab", "session": "t4", "name": "release", "space": "isa", "why": "merge", "text": "Merge PR #42 into main: the checks pass", "options": [{ "label": "Merge it" }] }),
            json!({ "at": 20, "kind": "alert", "status": "blocked", "session": "t4", "name": "release", "space": "isa", "text": "release needs you: Merge PR #42 into main: the checks pass. I'd say: Merge it" }),
            json!({ "at": 30, "kind": "decision", "id": "d0123456789ab", "status": "answered", "session": "t4", "name": "release", "space": "isa", "why": "merge", "text": "Merge PR #42 into main: the checks pass -> Merge it", "record": true }),
            json!({ "at": 40, "kind": "decision", "status": "dismissed", "space": "isa", "text": "Which microscope drawing?", "record": true }),
            json!({ "at": 50, "kind": "decision", "status": "stopped", "session": "t9", "name": "clips", "space": "isa", "text": "Run the migration?", "record": true }),
            json!({ "at": 60, "kind": "peer", "status": "undelivered", "session": "t1", "name": "eyzflow", "from": "t2", "from_name": "mediaroom", "space": "sandbox", "text": "Are you there?", "record": true }),
            json!({ "at": 70, "kind": "note", "text": "Nothing to do: the queue only delivered a message.", "record": true }),
            json!({ "at": 80, "kind": "receipt", "text": "The docs are updated.", "agenda": "a4", "space": "sandbox", "record": true }),
            json!({ "at": 90, "kind": "alert", "status": "ready", "session": "t1", "name": "eyzflow", "space": "sandbox", "text": "eyzflow is ready", "record": true }),
            json!({ "at": 100, "kind": "space", "status": "ready", "space": "isa", "text": "isa is ready to close: every request there is finished.", "record": true }),
            json!({ "at": 110, "kind": "space", "status": "closed", "space": "isa", "text": "isa is closed. The clips are live on dev.", "record": true }),
            json!({ "at": 120, "kind": "space", "status": "quiet", "space": "sandbox", "text": "sandbox is quiet: its last session is gone.", "record": true }),
            json!({ "at": 130, "kind": "queue", "status": "queued", "session": "t1", "name": "eyzflow", "space": "sandbox", "text": "queued for eyzflow: run the tests" }),
            json!({ "at": 140, "kind": "queue", "status": "delivered", "session": "t1", "name": "eyzflow", "space": "sandbox", "text": "delivered to eyzflow: run the tests" }),
        ];
        let rows = rows(&entries);
        let got: Vec<(String, String, &str)> = rows.iter().rev().map(|r| (r.label(), r.place(), r.text.as_str())).collect();
        let want = [
            ("Decision raised", "isa › release", "merge: Merge PR #42 into main: the checks pass"),
            ("Needs you", "isa › release", "release needs you: Merge PR #42 into main: the checks pass. I'd say: Merge it"),
            ("Decision answered", "isa › release", "Merge PR #42 into main: the checks pass → Merge it"),
            ("Decision dismissed", "isa", "Which microscope drawing?"),
            ("Decision over", "isa › clips", "Run the migration?"),
            ("Not delivered", "sandbox › mediaroom → eyzflow", "Are you there?"),
            ("Note", "tower", "Nothing to do: the queue only delivered a message."),
            ("Receipt", "sandbox › tower", "The docs are updated."),
            ("Ready", "sandbox › eyzflow", "eyzflow is ready"),
            ("Ready to close", "isa", "isa is ready to close: every request there is finished."),
            ("Space closed", "isa", "isa is closed. The clips are live on dev."),
            ("Space quiet", "sandbox", "sandbox is quiet: its last session is gone."),
            ("Queued", "sandbox › eyzflow", "queued for eyzflow: run the tests"),
            ("Delivered", "sandbox › eyzflow", "delivered to eyzflow: run the tests"),
        ];
        assert_eq!(got.iter().map(|(l, p, t)| (l.as_str(), p.as_str(), *t)).collect::<Vec<_>>(), want);
    }

    #[test]
    fn the_record_narrows_to_a_space() {
        let rows = rows(&history());
        assert_eq!(spaces(&rows), ["sandbox", "isa"], "the one heard from last first");
        let sandbox = of_space(&rows, Some("sandbox"));
        assert_eq!(sandbox.iter().map(|r| r.at).collect::<Vec<_>>(), [130, 80, 70, 40, 30]);
        assert_eq!(of_space(&rows, Some("isa")).len(), 4);
        assert_eq!(of_space(&rows, None).len(), rows.len());
        assert!(of_space(&rows, Some("anna")).is_empty());
    }

    #[test]
    fn a_row_is_called_by_what_it_is() {
        for (kind, status, want) in [
            ("job", "done", What::Finished),
            ("job", "working", What::Started),
            ("job", "blocked", What::NeedsYou),
            ("job", "failed", What::Failed),
            ("job", "exited", What::Failed),
            ("watch", "done", What::Finished),
            ("watch", "", What::Report),
            ("alert", "blocked", What::NeedsYou),
            ("alert", "", What::Alert),
            ("alert", "done", What::Finished),
            ("peer", "undelivered", What::Undelivered),
            ("peer", "delivered", What::Peer),
            ("decision", "answered", What::Decided),
            ("decision", "stopped", What::Over),
            ("space", "closed", What::SpaceClosed),
            ("queue", "sent", What::Delivered),
            ("queue", "news", What::Other),
            ("alert", "ready", What::Ready),
            ("answered", "", What::Answered),
            ("raised", "", What::Raised),
            ("receipt", "", What::Receipt),
            ("note", "", What::Note),
            ("introduce", "", What::Other),
        ] {
            assert_eq!(What::of(kind, status), want, "{kind} {status}");
        }
        let other = Row { at: 0, what: What::Other, kind: "introduce".into(), space: String::new(), who: String::new(), text: "x".into() };
        assert_eq!((other.label(), other.place()), ("Introduce".to_string(), String::new()));
        let peer = Row { what: What::Peer, kind: "peer".into(), space: "isa".into(), who: String::new(), ..other.clone() };
        assert_eq!(peer.place(), "isa");
    }

    #[test]
    fn screenshots_sent_together_are_one_row() {
        let shot = |at: u64, session: &str, caption: &str| json!({ "at": at, "kind": "screenshot", "session": session, "name": session, "caption": caption });
        let entries = vec![
            shot(0, "t1", "First"),
            shot(60_000, "t1", "Second"),
            shot(TOGETHER_MS + 1, "t1", "Later"),
            shot(TOGETHER_MS + 2, "t2", ""),
            json!({ "at": TOGETHER_MS + 3, "kind": "note", "text": "between" }),
            shot(TOGETHER_MS + 4, "t2", ""),
        ];
        let rows = rows(&entries);
        let texts: Vec<&str> = rows.iter().rev().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["2 screenshots: First", "1 screenshot: Later", "1 screenshot", "between", "1 screenshot"]);
    }

    /// What the user did in a session's own terminal reads as theirs, where they did it.
    #[test]
    fn what_the_user_did_in_a_terminal_reads_as_theirs() {
        let entries = vec![
            json!({ "at": 10, "kind": "request", "source": "terminal:t5", "session": "t5", "name": "Shop api", "space": "Shop", "text": "add rate limiting to the login endpoint", "agenda": "a7", "record": true }),
            json!({ "at": 20, "kind": "decision", "id": "d0123456789ab", "status": "answered", "source": "terminal:t5", "session": "t5", "name": "Shop api", "space": "Shop", "text": "Allow Bash: npm test? -> Answered in Shop › api: No", "record": true }),
            json!({ "at": 30, "kind": "queue", "status": "not-sent", "session": "t5", "name": "Shop api", "space": "Shop", "text": "Not sent: you answered it in the terminal (the reply was \"Yes, open it.\")", "record": true }),
        ];
        let rows = rows(&entries);
        let got: Vec<(String, String, &str)> = rows.iter().rev().map(|r| (r.label(), r.place(), r.text.as_str())).collect();
        let want = [
            ("You typed", "Shop › api", "add rate limiting to the login endpoint"),
            ("You answered", "Shop › api", "Allow Bash: npm test? → Answered in Shop › api: No"),
            ("Not sent", "Shop › api", "Not sent: you answered it in the terminal (the reply was \"Yes, open it.\")"),
        ];
        assert_eq!(got.iter().map(|(l, p, t)| (l.as_str(), p.as_str(), *t)).collect::<Vec<_>>(), want);
        assert_eq!(of_space(&rows, Some("Shop")).len(), 3, "in that space's record");
    }
}
