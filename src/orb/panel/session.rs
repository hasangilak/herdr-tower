//! One session, opened from its space in the full window (W20 in the
//! design): what it was asked, what it last said, its screen as it is now
//! (read again every couple of seconds), and a button that shows it in Herdr.
//! A decision that waits on it is named, with the way to its card.

use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, FontWeight, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::{h_flex, v_flex};

use super::home::{LINE, acts_row, bay_label, cstat, empty_box, mbtn, tg};
use super::window::whead;
use super::{AMBER, NAVY, PALE, Panel, glass};
use crate::history::Entry;
use crate::orb::model::{Decision, Missions};
use crate::orb::nav::Scope;

/// A test session (`hc run --label tower.test=1 --name qfix-…`): never listed.
/// History keeps names only, so the name says it.
pub(super) fn is_test(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n.starts_with("qfix") || n.starts_with("tower.test")
}

/// What a session last said, from the history: the latest word on a job of
/// its that isn't just starting (a watch report, `tower job done`), or its
/// summary. One line of it.
pub(super) fn last_words(entries: &[Entry], addr: &str) -> Option<String> {
    entries
        .iter()
        .rev()
        .filter(|e| e.session.as_deref() == Some(addr) && !e.text.trim().is_empty())
        .find(|e| match e.kind.as_str() {
            "job" => e.status.as_deref() != Some("working"),
            "summary" => true,
            _ => false,
        })
        .map(|e| {
            let line = e.text.trim().lines().next().unwrap_or("").trim();
            if line.chars().count() > 240 { format!("{}…", line.chars().take(240).collect::<String>()) } else { line.to_string() }
        })
}

/// The session view's state.
#[derive(Default)]
pub(super) struct SessionUi {
    /// The session showing.
    addr: Option<String>,
    /// Its screen, as last read, and when.
    screen: String,
    read_at: Option<Instant>,
    reading: bool,
    /// Why it couldn't be shown in Herdr.
    note: Option<String>,
}

impl Panel {
    /// The space a session is in (from the live spaces, else its history).
    pub(super) fn space_of(&self, addr: &str) -> String {
        self.live
            .iter()
            .find(|l| l.sessions.iter().any(|s| s.0 == addr))
            .map(|l| l.name.clone())
            .or_else(|| self.entries.iter().rev().find(|e| e.session.as_deref() == Some(addr)).and_then(|e| e.space.clone()))
            .unwrap_or_default()
    }

    pub(super) fn show_in_herdr(&mut self, addr: &str, cx: &mut Context<Self>) {
        let addr = addr.to_string();
        let task = cx.background_executor().spawn(async move { crate::show_terminal(&addr) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.session.note = result.err().map(|e| format!("{e:#}"));
                cx.notify();
            });
        })
        .detach();
    }

    /// The session the window shows (none: another view): a new one's screen is read at once.
    pub(super) fn watch_session(&mut self, addr: Option<&str>, cx: &mut Context<Self>) {
        if self.session.addr.as_deref() == addr {
            return;
        }
        self.session = SessionUi { addr: addr.map(String::from), ..Default::default() };
        self.session_tick(cx);
    }

    /// Every couple of seconds while a session shows: its screen again.
    pub(super) fn session_tick(&mut self, cx: &mut Context<Self>) {
        let Some(addr) = self.session.addr.clone() else { return };
        if self.session.reading || self.session.read_at.is_some_and(|t| t.elapsed() < Duration::from_millis(1900)) {
            return;
        }
        self.session.reading = true;
        let task = cx.background_executor().spawn({
            let addr = addr.clone();
            async move { crate::hc::run(&["read", &addr, "--lines", "40"]).map(|o| if o.code == 0 { o.stdout } else { String::new() }).unwrap_or_default() }
        });
        cx.spawn(async move |this, cx| {
            let screen = task.await;
            let _ = this.update(cx, |this, cx| {
                // Another session was opened meanwhile: this isn't its screen.
                if this.session.addr.as_deref() != Some(addr.as_str()) {
                    return;
                }
                this.session.reading = false;
                this.session.read_at = Some(Instant::now());
                this.session.screen = screen.trim_end().to_string();
                cx.notify();
            });
        })
        .detach();
    }

    /// The session `addr` of `space`: its head and its body.
    pub(super) fn session_pane(&mut self, space: &str, addr: &str, all: &[Decision], missions: &Missions, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let g = glass();
        let scope = Scope::Session { space: space.to_string(), addr: addr.to_string() };
        let back = scope.back().map(|(label, to)| tg("session-back", &label, false).on_click(cx.listener(move |this, _, _, cx| this.nav_to(to.clone(), cx))).into_any_element());
        let mission = missions.open.iter().find(|m| m.name == space);
        let member = mission.and_then(|m| m.sessions.iter().find(|s| s.address == addr)).cloned();
        let agent = self.tower.read(cx).s("agents").as_array().and_then(|a| a.iter().find(|x| x["address"] == addr)).cloned();
        let name = member
            .as_ref()
            .map(|m| m.name.clone())
            .or_else(|| agent.as_ref().and_then(|a| a["name"].as_str().map(String::from)))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| addr.to_string());
        let title = if space.is_empty() { name.clone() } else { format!("{space} › {name}") };
        let Some(status) = member.as_ref().map(|m| m.status.clone()).or_else(|| agent.as_ref().and_then(|a| a["status"].as_str().map(String::from))) else {
            return (whead(&title, vec![], back), vec![empty_box("This session is gone.")]);
        };
        let mut sub = vec![super::space::status_word(&status).to_string()];
        sub.extend(member.as_ref().map(|m| m.place()).filter(|p| !p.is_empty()));
        sub.extend(member.as_ref().and_then(|m| m.account.clone()).map(|a| format!("on {a}")));
        let mut lines = vec![div().child(sub.join(" · ")).into_any_element()];
        // A browser it uses now: which, its site, since when (amber when it shares the user's).
        let browser = member
            .as_ref()
            .and_then(|m| m.browser.clone())
            .or_else(|| agent.as_ref().and_then(|a| serde_json::from_value::<Vec<crate::browsers::Use>>(a["browsers"].clone()).ok()).and_then(|b| b.into_iter().next()));
        if let Some(b) = browser {
            let colour = if b.shared_with.is_empty() { super::CYAN } else { super::AMBER };
            lines.push(div().text_color(rgb(colour)).child(b.line).into_any_element());
        }
        // While it works: how its turn goes, and a side question to it that doesn't stop it.
        if status == "working" {
            let progress = member
                .as_ref()
                .and_then(|m| m.progress.clone())
                .or_else(|| agent.as_ref().and_then(|a| serde_json::from_value::<crate::progress::Progress>(a["progress"].clone()).ok()));
            if let Some(p) = progress {
                lines.push(super::space::progress_line(&p, crate::history::now_ms()));
            }
            let place = format!("progress:{addr}");
            let asking = self.full.as_ref().is_some_and(|f| f.busy.contains(&place));
            let who = addr.to_string();
            lines.push(
                h_flex()
                    .pt(px(4.0))
                    .child(
                        mbtn("session-progress", if asking { "Asking it aside…" } else { "How is it going?" }, false, None)
                            .on_click(cx.listener(move |this, _, _, cx| this.ask_progress(&who, cx))),
                    )
                    .into_any_element(),
            );
        }
        let head = whead(&title, lines, back);

        let said = |text: String| div().px(px(4.0)).text_size(px(13.0)).line_height(px(19.0)).text_color(g.muted).child(text);
        let mut items: Vec<AnyElement> = vec![];
        // A decision that waits on it: named, with the way to its card.
        if let Some(d) = all.iter().find(|d| d.yours() && d.sessions.iter().any(|s| s.address == addr && s.waiting)) {
            // Its card is in its space's own queue (the only place a muted space's shows), if that space is open.
            let open = missions.open.iter().find(|m| d.in_space(&m.name)).map(|m| m.name.clone());
            let (id, to) = (d.id.clone(), open.map(Scope::Space).unwrap_or(Scope::All));
            items.push(
                h_flex()
                    .w_full()
                    .gap(px(12.0))
                    .items_center()
                    .px(px(14.0))
                    .py(px(10.0))
                    .rounded(px(14.0))
                    .border_1()
                    .border_color(rgb(AMBER))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(px(12.0)).text_color(rgb(AMBER)).child("It waits for your answer"))
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).line_clamp(2).child(d.title.clone())),
                    )
                    .child(mbtn("session-card", "Open its card", true, None).on_click(cx.listener(move |this, _, _, cx| {
                        this.pick(&id, cx);
                        this.nav_to(to.clone(), cx);
                    })))
                    .into_any_element(),
            );
        }
        // What it was asked: its latest job, else its space's current request.
        let asked = self
            .jobs
            .iter()
            .rev()
            .find(|j| j.session == addr && !j.title.trim().is_empty())
            .map(|j| j.title.clone())
            .or_else(|| mission.and_then(|m| m.current()).map(|r| r.title.clone()));
        if let Some(asked) = asked {
            items.push(v_flex().gap(px(6.0)).child(bay_label("Asked")).child(said(asked)).into_any_element());
        }
        // Its last words: as its hooks heard them (the daemon's word), else from the history.
        if let Some(words) = member.as_ref().and_then(|m| m.said.as_deref().map(str::trim).filter(|w| !w.is_empty()).map(String::from)).or_else(|| last_words(&self.entries, addr)) {
            items.push(v_flex().gap(px(6.0)).child(bay_label("Last said")).child(said(words)).into_any_element());
        }
        // Between you, the tower and it: what you typed in its terminal, what
        // the tower queued and sent it (or didn't), and what still waits for it.
        let waiting = agent.as_ref().and_then(|a| a["waiting"].as_array().cloned()).unwrap_or_default();
        let thread = super::talk::without_waiting(super::talk::session_thread(&self.entries, addr, 60), &super::talk::waiting_texts(addr, &waiting));
        let waits = super::talk::waiting_rows(&name, &waiting);
        if !thread.is_empty() || !waits.is_empty() {
            items.push(
                v_flex()
                    .gap(px(8.0))
                    .child(bay_label(&format!("Between you, the tower and {name}")))
                    .children(thread.iter().map(|s| super::talk::said_row(s, "The tower", vec![])))
                    .children(waits)
                    .into_any_element(),
            );
        }
        items.push(
            v_flex()
                .gap(px(6.0))
                .child(bay_label("Its screen now"))
                .child(
                    div()
                        .w_full()
                        .px(px(12.0))
                        .py(px(10.0))
                        .rounded(px(12.0))
                        .bg(rgb(NAVY))
                        .border_1()
                        .border_color(rgba(LINE))
                        .font_family("Menlo")
                        .text_size(px(11.5))
                        .line_height(px(17.0))
                        .text_color(rgb(PALE))
                        .child(match (self.session.screen.is_empty(), self.session.read_at.is_some()) {
                            (false, _) => self.session.screen.clone(),
                            (true, true) => "Nothing to read: it isn’t on this Mac, or Herdr isn’t open.".to_string(),
                            (true, false) => "Reading…".to_string(),
                        }),
                )
                .into_any_element(),
        );
        let a = addr.to_string();
        items.push(acts_row().child(mbtn("session-show", "Show in Herdr", true, None).on_click(cx.listener(move |this, _, _, cx| this.show_in_herdr(&a, cx)))).into_any_element());
        if let Some(n) = &self.session.note {
            items.push(cstat(n.clone(), true));
        }
        (head, items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: &str, session: &str, status: Option<&str>, text: &str, at: u64) -> Entry {
        Entry { kind: kind.into(), session: Some(session.into()), status: status.map(String::from), text: text.into(), at, ..Default::default() }
    }

    #[test]
    fn a_session_last_said_what_its_latest_report_says() {
        let entries = vec![
            e("job", "t1", Some("working"), "Plan the storefront live editor", 1),
            e("summary", "t1", None, "Reading the editor's code.", 2),
            e("job", "t1", Some("done"), "Wrote the plan.\nThree files changed.", 3),
            e("watch", "t1", None, "t1 (eyzflow) is now done. You had asked it: plan.", 4),
            e("job", "t2", Some("done"), "Answered the planning questions", 5),
            e("job", "t1", Some("working"), "Check the estimate", 6),
            e("job", "t3", Some("done"), "  ", 7),
        ];
        // What it was asked, the tower's own notes and other sessions don't count.
        assert_eq!(last_words(&entries, "t1").as_deref(), Some("Wrote the plan."));
        assert_eq!(last_words(&entries, "t2").as_deref(), Some("Answered the planning questions"));
        assert_eq!(last_words(&entries[..2], "t1").as_deref(), Some("Reading the editor's code."));
        assert_eq!((last_words(&entries, "t3"), last_words(&entries, "t9"), last_words(&entries[..1], "t1")), (None, None, None));
        let long = vec![e("job", "t1", Some("failed"), &"x".repeat(300), 1)];
        assert_eq!(last_words(&long, "t1").unwrap().chars().count(), 241);
    }

    #[test]
    fn test_sessions_are_never_listed() {
        assert!(is_test("qfix-two") && is_test(" Tower.Test=1 ") && !is_test("eyzflow") && !is_test("fix-q"));
    }
}

impl Panel {
    /// "How is it going?": `tower progress <session>`; the question and its
    /// answer land in its thread (the history), a failure says why here.
    pub(super) fn ask_progress(&mut self, addr: &str, cx: &mut Context<Self>) {
        let place = format!("progress:{addr}");
        let at = self.tower.read(cx).nav.scope.clone();
        self.run(&place, vec!["progress".into(), addr.to_string(), "--json".into()], cx, move |this, result, cx| {
            if let Err(e) = result {
                let text = format!("It couldn't be asked: {}", e.trim());
                this.tower.update(cx, |t, cx| {
                    t.nav.say(at.clone(), text);
                    cx.notify();
                });
            }
            cx.notify();
        });
    }
}
