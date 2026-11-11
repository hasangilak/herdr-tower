//! The launcher (`docs/quiet-tower.md`, "Step 9"; W9 and W10 in the design):
//! one sentence starts a space. A goal, a saved setup, what the tower will
//! do, Launch. It asks nothing, and then the new space's own view opens.
//!
//! - **The setups** are the tower's (`tower setups --json`): each a name, an
//!   account and its repos. A tower without any, or without the command,
//!   leaves the choice out and says how one is added.
//! - **What the tower will do** is the tower's own word: the receipt of
//!   `tower launch … --dry-run --json` (the space's name, a session per
//!   repo with its account, worktree and branch, and what it assumed), read
//!   by `plan_from` once the typing pauses. Why it can't launch (no setup, a
//!   repo that is no folder) shows in its place. Before it has answered,
//!   and for a tower whose help lists no dry run, the same is worked out
//!   here (`plan`), with the name the tower's own rule gives (`slug`).
//! - **Launch** is `tower launch "<goal>" --setup "<name>" --json`; its
//!   receipt names the space, whose view opens with what was done said at
//!   its top (`launched_line`), and an error stays here.

use std::time::Duration;

use gpui::{AnyElement, AsyncApp, Context, Entity, FontWeight, SharedString, Subscription, Task, Window, div, prelude::*, px, rgba};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use serde_json::Value;

use super::home::{SOFT, acts_row, bay_label, cstat, mbtn, tg};
use super::window::whead;
use super::{Panel, cli, glass};
use crate::history;
use crate::orb::model::Missions;

/// A saved setup: the sessions a new space starts with.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Setup {
    pub name: String,
    pub account: String,
    pub repos: Vec<String>,
}

/// The setups `tower setups --json` printed: `{"setups": [...]}` or the list
/// itself, each `{name, account, repos}`. Anything else: none.
pub(super) fn setups_from(text: &str) -> Vec<Setup> {
    let v: Value = serde_json::from_str(text).unwrap_or_default();
    let list = v.get("setups").unwrap_or(&v);
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some(Setup {
                name: s["name"].as_str().filter(|n| !n.trim().is_empty())?.to_string(),
                account: s["account"].as_str().unwrap_or("").to_string(),
                repos: s["repos"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(String::from)).collect(),
            })
        })
        .collect()
}

/// A repo by its folder's name (`~/Work/eyz/eyzflow`: `eyzflow`).
pub(super) fn repo_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// The name a space gets from its goal, by the tower's own rule
/// (`launch::space_name_of`): its first two words that carry meaning, in
/// lower case with a dash (`Fix the login redirect`: `fix-login`), not the
/// words that only ask for sessions, numbered when a space of that name is
/// there already; `space` from no words at all. (The tower's own plan, which
/// also knows the repos the goal names, replaces it as soon as it comes.)
pub(super) fn slug(goal: &str, taken: &[&str]) -> String {
    let base = match crate::launch::space_name_of(goal, &[]) {
        b if b.is_empty() => "space".to_string(),
        b => b,
    };
    let taken: Vec<String> = taken.iter().map(|t| t.to_string()).collect();
    crate::launch::unique(&base, &taken)
}

/// What the tower will do with `goal` and `setup`, a line each: the name
/// first, then (with a setup) its sessions, their worktrees and the handing
/// out. Worked out here, for before the tower has said it itself.
pub(super) fn plan(goal: &str, setup: Option<&Setup>, taken: &[&str]) -> Vec<String> {
    let mut lines = vec![if goal.trim().is_empty() { "Name the space from its goal".to_string() } else { format!("{NAMES}{}", slug(goal, taken)) }];
    let Some(setup) = setup else { return lines };
    let n = setup.repos.len();
    let repos = setup.repos.iter().map(|r| repo_name(r)).collect::<Vec<_>>().join(", ");
    let on = if setup.account.is_empty() { String::new() } else { format!(" on {}", setup.account) };
    lines.push(match n {
        0 => format!("Open its sessions{on}"),
        1 => format!("Open 1 session{on}: {repos}"),
        n => format!("Open {n} sessions{on}: {repos}"),
    });
    lines.push(if n == 1 { "Give it its own worktree, branch and dev port".into() } else { "Give each its own worktree, branch and dev port".into() });
    lines.push(if n > 1 { "Introduce them to each other and hand out the goal".into() } else { "Hand out the goal".into() });
    lines
}

/// How the plan's first line begins; what follows is the name, drawn bold.
const NAMES: &str = "Name the space ";
/// The same when a space of that name is there already (`--name`).
const JOINS: &str = "Join the space ";

/// The tower's own word on a launch: the receipt `tower launch … --dry-run
/// --json` prints.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Dry {
    /// What it will do, a line each.
    pub lines: Vec<String>,
    /// What it chose by itself, in a sentence.
    pub assumed: String,
    /// Why it can't (no setup, a repo that is no folder).
    pub error: Option<String>,
}

/// A launch's receipt (the plan of a dry run) as the lines the launcher
/// shows. None when it isn't a receipt.
pub(super) fn plan_from(receipt: &Value) -> Option<Dry> {
    let space = receipt["space"].as_str().filter(|s| !s.is_empty())?;
    let sessions = receipt["sessions"].as_array().filter(|s| !s.is_empty())?;
    let a = &receipt["assumed"];
    let n = sessions.len();
    let repos = sessions.iter().map(|s| repo_name(s["repo"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ");
    let account = a["account"].as_str().or_else(|| sessions[0]["account"].as_str()).filter(|x| !x.is_empty());
    let on = account.map(|x| format!(" on {x}")).unwrap_or_default();
    let mut lines = vec![format!("{}{space}", if receipt["made"] == false { JOINS } else { NAMES })];
    lines.push(if n == 1 { format!("Open 1 session{on}: {repos}") } else { format!("Open {n} sessions{on}: {repos}") });
    let own = sessions.iter().filter(|s| s["worktree"].as_str().is_some_and(|w| !w.is_empty())).count();
    lines.push(match (own, n) {
        (0, _) => match a["worktrees_why"].as_str().map(|w| w.trim_start_matches("none: ").trim_start_matches("off ")).filter(|w| !w.is_empty()) {
            Some(why) => format!("No worktrees: {why}"),
            None => "No worktrees: each session works in its repo itself".to_string(),
        },
        (_, 1) => "Give it its own worktree, branch and dev port".to_string(),
        (own, n) if own == n => "Give each its own worktree, branch and dev port".to_string(),
        (own, n) => format!("Give {own} of {n} their own worktree, branch and dev port (a folder that is no git checkout is used as it is)"),
    });
    lines.push(if n > 1 { "Introduce them to each other and hand out the goal".into() } else { "Hand out the goal".to_string() });
    // What it chose by itself: the setup and why (not one that was named
    // to it: that is the choice above), and an account it had to pick.
    let mut assumed = match (a["setup"].as_str().filter(|x| !x.is_empty()), a["setup_why"].as_str().filter(|x| !x.is_empty())) {
        (Some(_), Some("the one named")) => String::new(),
        (Some(_), Some("the repos its first line names")) => "It opens the repos the goal's first line names.".to_string(),
        (Some(setup), Some(why)) => format!("It takes the setup “{setup}” ({why})."),
        (Some(setup), None) => format!("It takes the setup “{setup}”."),
        _ => String::new(),
    };
    if let Some(why) = a["account_why"].as_str().filter(|w| *w != "the setup's" && !w.is_empty()) {
        assumed = format!("{assumed} The account: {why}.").trim().to_string();
    }
    Some(Dry { lines, assumed, error: None })
}

/// What a launch did, from its receipt, for the top of the new space:
/// `Launched fix-login: 2 sessions on orca (eyzflow, mediaroom), each in its
/// own worktree. …`. None when it isn't a receipt.
pub(super) fn launched_line(receipt: &Value) -> Option<String> {
    let space = receipt["space"].as_str().filter(|s| !s.is_empty())?;
    let sessions = receipt["sessions"].as_array()?;
    let opened: Vec<&Value> = sessions.iter().filter(|s| s["error"].is_null()).collect();
    let account = receipt["assumed"]["account"].as_str().filter(|x| !x.is_empty());
    let on = account.map(|x| format!(" on {x}")).unwrap_or_default();
    let repos = opened.iter().map(|s| repo_name(s["repo"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ");
    let own = opened.iter().filter(|s| s["worktree"].as_str().is_some_and(|w| !w.is_empty())).count();
    let ports: Vec<String> = opened.iter().filter_map(|s| s["port"].as_u64()).map(|p| p.to_string()).collect();
    let place = match (own, opened.len()) {
        (0, _) => String::new(),
        (_, 1) => ", in its own worktree".to_string(),
        (own, n) if own == n => ", each in its own worktree".to_string(),
        (own, n) => format!(", {own} of {n} in their own worktree"),
    };
    let ports = match ports.as_slice() {
        [] => String::new(),
        [one] => format!(" (port {one})"),
        list => format!(" (ports {})", list.join(", ")),
    };
    let head = if receipt["made"] == false { format!("Joined {space}") } else { format!("Launched {space}") };
    let mut parts = vec![match opened.len() {
        0 => format!("{head}."),
        1 => format!("{head}: 1 session{on} ({repos}){place}{ports}."),
        n => format!("{head}: {n} sessions{on} ({repos}){place}{ports}."),
    }];
    for s in sessions.iter().filter(|s| !s["error"].is_null()) {
        parts.push(format!("{} didn’t open: {}.", repo_name(s["repo"].as_str().unwrap_or("A session")), s["error"].as_str().unwrap_or("").trim_end_matches('.')));
    }
    if let Some(setup) = receipt["assumed"]["setup"].as_str().filter(|x| !x.is_empty()) {
        parts.push(format!("It took the setup “{setup}”."));
    }
    parts.push("Nothing was asked. Change the goal from here.".into());
    Some(parts.join(" "))
}

/// The launcher's state.
pub(super) struct LaunchUi {
    goal: Entity<TextareaState>,
    /// The saved setups; `None` until the tower was asked.
    setups: Option<Vec<Setup>>,
    /// The setup chosen (the first, until another is).
    picked: Option<String>,
    /// Whether this tower's `launch` has `--dry-run` (`None`: not asked yet).
    dry_known: Option<bool>,
    /// What the tower said it will do, and for which goal and setup it said so.
    dry: Option<((String, Option<String>), Dry)>,
    /// Waits for the typing to pause before the tower is asked again.
    dry_wait: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl LaunchUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Panel>) -> Self {
        // A whole ticket fits: it grows to 14 lines, then scrolls.
        let goal = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 14).placeholder("open sessions in eyzflow and mediaroom: then the ticket, as long as it is"));
        let subs = vec![cx.subscribe_in(&goal, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                this.ask_plan(cx);
                cx.notify();
            }
        })];
        Self { goal, setups: None, picked: None, dry_known: None, dry: None, dry_wait: None, _subs: subs }
    }

    /// The saved setups, once the tower was asked.
    pub(super) fn setups(&self) -> Option<&[Setup]> {
        self.setups.as_deref()
    }

    /// The setup that is chosen, of those there are.
    fn setup(&self) -> Option<&Setup> {
        let list = self.setups.as_ref()?;
        self.picked.as_ref().and_then(|p| list.iter().find(|s| &s.name == p)).or(list.first())
    }
}

/// A one-line field on the white pill, as the launcher's goal.
pub(super) fn field(state: &Entity<TextareaState>) -> AnyElement {
    let g = glass();
    div()
        .w_full()
        .px(px(8.0))
        .py(px(7.0))
        .rounded(px(14.0))
        .bg(g.pill)
        .text_color(g.pill_text)
        .child(Textarea::new(state).appearance(false).bordered(false).small().w_full().text_size(px(15.0)).text_color(g.pill_text))
        .into_any_element()
}

impl Panel {
    /// Asks the tower for its setups, and once whether its launcher can say
    /// what it would do (`--dry-run` in its help).
    pub(super) fn read_setups(&mut self, cx: &mut Context<Self>) {
        self.run("setups", vec!["setups".into(), "--json".into()], cx, |this, result, _| {
            let Some(full) = this.full.as_mut() else { return };
            full.launch.setups = Some(result.as_ref().map(|out| setups_from(out)).unwrap_or_default());
        });
        if self.full.as_ref().is_some_and(|f| f.launch.dry_known.is_none()) {
            let task = cx.background_executor().spawn(async { cli::tower(&["launch", "--help"]) });
            cx.spawn(async move |this, cx: &mut AsyncApp| {
                let help = task.await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(full) = this.full.as_mut() {
                        full.launch.dry_known = Some(help.is_ok_and(|text| text.contains("--dry-run")));
                    }
                    this.ask_plan(cx);
                });
            })
            .detach();
        }
    }

    /// Puts `goal` in the launcher's field (the plan follows, as when it is typed).
    pub(super) fn launch_with(&mut self, goal: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(full) = self.full.as_ref() {
            let (field, goal) = (full.launch.goal.clone(), goal.to_string());
            field.update(cx, |s, cx| s.set_value(goal, window, cx));
        }
    }

    /// Asks the tower what it would do with the goal and the setup as they
    /// stand, once the typing has paused; only a tower that has a dry run.
    fn ask_plan(&mut self, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_mut() else { return };
        if full.launch.dry_known != Some(true) {
            return;
        }
        let goal = full.launch.goal.read(cx).value().trim().to_string();
        let key = (goal.clone(), full.launch.setup().map(|s| s.name.clone()));
        if goal.is_empty() || full.launch.dry.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        full.launch.dry_wait = Some(cx.spawn(async move |this, cx: &mut AsyncApp| {
            cx.background_executor().timer(Duration::from_millis(450)).await;
            let mut args = vec!["launch".to_string(), key.0.clone()];
            if let Some(setup) = &key.1 {
                args.extend(["--setup".to_string(), setup.clone()]);
            }
            args.extend(["--dry-run".to_string(), "--json".to_string()]);
            let said = cx.background_executor().spawn(async move { cli::tower(&args) }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(full) = this.full.as_mut() else { return };
                // Its plan, or why it can't launch; anything else is no word at all.
                let dry = match said {
                    Ok(out) => serde_json::from_str::<Value>(&out).ok().as_ref().and_then(plan_from),
                    Err(e) => Some(Dry { error: Some(e), ..Default::default() }),
                };
                full.launch.dry = dry.map(|d| (key, d));
                cx.notify();
            });
        }));
    }

    /// Launch: the space is made, and its view opens with what the tower said.
    fn launch(&mut self, missions: &Missions, cx: &mut Context<Self>) {
        let Some(full) = self.full.as_ref() else { return };
        let goal = full.launch.goal.read(cx).value().trim().to_string();
        if goal.is_empty() {
            return;
        }
        let before: Vec<String> = missions.open.iter().map(|m| m.name.clone()).chain(missions.quiet.iter().cloned()).collect();
        let taken: Vec<&str> = before.iter().map(String::as_str).collect();
        let expected = slug(&goal, &taken);
        let mut args = vec!["launch".to_string(), goal];
        if let Some(setup) = full.launch.setup() {
            args.extend(["--setup".to_string(), setup.name.clone()]);
        }
        args.push("--json".into());
        self.run("launch", args, cx, move |this, result, cx| {
            let Ok(out) = result else { return };
            // Its receipt names the space and says what was opened.
            let receipt: Value = serde_json::from_str(out).unwrap_or_default();
            let space = receipt["space"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or(expected);
            let text = launched_line(&receipt).unwrap_or_else(|| {
                let said = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
                if said.is_empty() { format!("Launched {space}. Nothing was asked. Change the goal from here.") } else { said }
            });
            if let Some(full) = this.full.as_mut() {
                full.launch.dry = None;
            }
            // What it opened is an act the tower keeps: Undo shows with what it said, as with a reply.
            let act = receipt["act"].as_str().map(String::from);
            this.tower.update(cx, |t, cx| {
                t.nav.launched(&space, before, text, history::now_ms());
                t.nav.receipt_of(act);
                cx.notify();
            });
        });
    }

    /// The launcher's head and body.
    pub(super) fn launch_view(&mut self, missions: &Missions, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let g = glass();
        let head = whead("New space", vec![div().child("Say what it should get done. The tower does the rest and asks nothing.").into_any_element()], None);
        let Some(full) = self.full.as_ref() else { return (head, vec![]) };
        let ui = &full.launch;
        let goal = ui.goal.read(cx).value().trim().to_string();
        let taken: Vec<&str> = missions.open.iter().map(|m| m.name.as_str()).chain(missions.quiet.iter().map(String::as_str)).collect();
        let setup = ui.setup().cloned();
        // The tower's own word on what it will do, if it gave it for this goal and setup.
        let key = (goal.clone(), setup.as_ref().map(|s| s.name.clone()));
        let dry = ui.dry.as_ref().filter(|(k, _)| *k == key).map(|(_, d)| d.clone()).unwrap_or_default();
        let lines = if dry.lines.is_empty() { plan(&goal, setup.as_ref(), &taken) } else { dry.lines.clone() };
        let busy = self.running("launch");
        let label = |text: &str| bay_label(text).px(px(0.0));

        // The setups, as a choice.
        let chosen = setup.as_ref().map(|s| s.name.clone());
        let setups: AnyElement = match ui.setups.as_deref() {
            None => cstat("Asking the tower…", false),
            Some([]) => cstat("None needed: the tower opens the repos your goal's first line names, on your default account.", false),
            Some(list) => h_flex()
                .flex_wrap()
                .gap(px(6.0))
                .children(list.iter().enumerate().map(|(i, s)| {
                    let name = s.name.clone();
                    tg(SharedString::from(format!("setup-{i}")), &s.name, chosen.as_deref() == Some(s.name.as_str())).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(full) = this.full.as_mut() {
                            full.launch.picked = Some(name.clone());
                        }
                        this.ask_plan(cx);
                        cx.notify();
                    }))
                }))
                .into_any_element(),
        };
        let bullets = v_flex().w_full().px(px(14.0)).py(px(12.0)).gap(px(4.0)).rounded(px(14.0)).bg(rgba(SOFT)).text_size(px(13.0)).line_height(px(18.0)).text_color(g.muted).children(
            lines.iter().map(|line| {
                let named = [NAMES, JOINS].into_iter().find_map(|lead| line.strip_prefix(lead).map(|name| (lead, name)));
                let text: AnyElement = match named {
                    Some((lead, name)) => h_flex()
                        .gap(px(4.0))
                        .child(lead.trim_end())
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(g.text).child(name.to_string()))
                        .into_any_element(),
                    None => div().min_w_0().child(line.clone()).into_any_element(),
                };
                h_flex().gap(px(9.0)).items_start().child(div().flex_none().child("•")).child(text)
            }),
        );
        let can = !goal.is_empty() && !busy;
        let button = mbtn("launch", if busy { "Launching…" } else { "Launch" }, true, None).when(!can, |el| el.opacity(0.5)).when(can, |el| {
            let m = missions.clone();
            el.on_click(cx.listener(move |this, _, _, cx| this.launch(&m, cx)))
        });
        // How long it is, and what the first line is for: a pasted ticket can be the rest.
        let words = ui.goal.read(cx).value().split_whitespace().count();
        let hint = format!(
            "{}The first line says what to open (\"open sessions in eyzflow and mediaroom\"); the rest can be the whole ticket, and goes to the sessions as it is.",
            if words > 0 { format!("{words} word{} · ", if words == 1 { "" } else { "s" }) } else { String::new() }
        );
        let body = v_flex()
            .w_full()
            .max_w(px(760.0))
            .gap(px(16.0))
            .child(
                v_flex()
                    .gap(px(7.0))
                    .child(label("Goal"))
                    .child(field(&ui.goal))
                    .child(div().px(px(4.0)).text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(hint)),
            )
            .child(v_flex().gap(px(7.0)).child(label("Setup")).child(setups).children(self.said("setups")))
            .child(
                v_flex()
                    .gap(px(7.0))
                    .child(label("What the tower will do"))
                    // Why it can't, in the plan's place; else the plan and what it assumed.
                    .child(match &dry.error {
                        Some(e) => cstat(e.clone(), true),
                        None => bullets.into_any_element(),
                    })
                    .when(dry.error.is_none() && !dry.assumed.is_empty(), |el| el.child(div().px(px(4.0)).text_size(px(12.0)).line_height(px(17.0)).text_color(g.faint).child(dry.assumed.clone()))),
            )
            .child(acts_row().child(button))
            .children(self.said("launch"))
            .into_any_element();
        (head, vec![body])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_is_named_from_its_goal() {
        assert_eq!(slug("Case exports as PDF", &[]), "case-exports");
        assert_eq!(slug("  Voice clips, for the Ingest tour!  ", &[]), "voice-clips");
        assert_eq!(slug("Rework", &[]), "rework");
        assert_eq!(slug("PR #40: fix it", &[]), "pr-40");
        // As the tower names it: small words are passed over.
        assert_eq!(slug("Fix the login redirect", &[]), "fix-login");
        assert_eq!(slug("A plan for the storefront live editor", &[]), "plan-storefront");
        // Nothing to name it by.
        assert_eq!(slug("", &[]), "space");
        assert_eq!(slug("— ¿?", &[]), "space");
        // A name that is taken (in any case) gets a number.
        assert_eq!(slug("Case exports as PDF", &["sandbox", "Case-Exports"]), "case-exports-2");
        assert_eq!(slug("Case exports as PDF", &["case-exports", "case-exports-2"]), "case-exports-3");
        assert_eq!(slug("", &["space"]), "space-2");
        // A goal that opens by asking for sessions is not named "open-session".
        assert_eq!(slug("open a session at eyzflow and mediaroom. this is the ticket: EYZFlow Support", &[]), "eyzflow-mediaroom");
    }

    #[test]
    fn the_towers_own_plan_is_read_from_its_receipt() {
        // `tower launch "Fix the login redirect" --dry-run --json`.
        let receipt = serde_json::json!({
            "space": "fix-login", "goal": "Fix the login redirect", "made": true, "dry_run": true, "agenda": null,
            "sessions": [
                { "name": "fix-login-eyzflow", "repo": "~/Work/eyz/eyzflow", "address": null, "account": "orca", "worktree": "/w/.tower-worktrees/fix-login/eyzflow", "branch": "fix-login/eyzflow", "port": null, "task": null, "error": null },
                { "name": "fix-login-mediaroom", "repo": "~/Work/eyz/mediaroom/", "address": null, "account": "orca", "worktree": "/w/.tower-worktrees/fix-login/mediaroom", "branch": "fix-login/mediaroom", "port": null, "task": null, "error": null },
            ],
            "assumed": { "setup": "eyzflow + mediaroom on orca", "setup_why": "the first of 2", "account": "orca", "account_why": "the setup's", "name": "fix-login", "name_why": "the goal's first two words, small ones left out", "worktrees": true, "worktrees_why": "one per repo, on a branch of its own" },
        });
        let dry = plan_from(&receipt).unwrap();
        assert_eq!(
            dry.lines,
            ["Name the space fix-login", "Open 2 sessions on orca: eyzflow, mediaroom", "Give each its own worktree, branch and dev port", "Introduce them to each other and hand out the goal"]
        );
        assert_eq!(dry.assumed, "It takes the setup “eyzflow + mediaroom on orca” (the first of 2).");
        // A standing space joined by name: no worktrees, and it says why; an account it picked itself is said too.
        let mut joined = receipt.clone();
        joined["made"] = false.into();
        joined["space"] = "M5 Pro".into();
        joined["sessions"].as_array_mut().unwrap().truncate(1);
        joined["sessions"][0]["worktree"] = Value::Null;
        joined["assumed"]["worktrees_why"] = "none: it is a standing space, so each session works in its repo itself".into();
        joined["assumed"]["account_why"] = "the default one: the setup names none".into();
        joined["assumed"]["setup_why"] = "the one named".into();
        let dry = plan_from(&joined).unwrap();
        assert_eq!(dry.assumed, "The account: the default one: the setup names none.", "a setup that was named to it isn't its own choice");
        assert_eq!(dry.lines, ["Join the space M5 Pro", "Open 1 session on orca: eyzflow", "No worktrees: it is a standing space, so each session works in its repo itself", "Hand out the goal"]);
        assert!(dry.assumed.ends_with("The account: the default one: the setup names none."), "{}", dry.assumed);
        // Not a receipt: no word.
        assert!(plan_from(&serde_json::json!({})).is_none() && plan_from(&serde_json::json!({ "space": "x", "sessions": [] })).is_none());
        // After the launch: what opened, where, and what didn't.
        let mut done = receipt.clone();
        done["dry_run"] = false.into();
        done["sessions"][0]["address"] = "t7".into();
        done["sessions"][0]["port"] = 3004.into();
        done["sessions"][1]["error"] = "hc run: no".into();
        assert_eq!(
            launched_line(&done).unwrap(),
            "Launched fix-login: 1 session on orca (eyzflow), in its own worktree (port 3004). mediaroom didn’t open: hc run: no. It took the setup “eyzflow + mediaroom on orca”. Nothing was asked. Change the goal from here."
        );
        done["sessions"][1]["error"] = Value::Null;
        done["sessions"][1]["port"] = 3005.into();
        assert!(launched_line(&done).unwrap().starts_with("Launched fix-login: 2 sessions on orca (eyzflow, mediaroom), each in its own worktree (ports 3004, 3005)."));
        assert!(launched_line(&serde_json::json!({})).is_none());
    }

    #[test]
    fn the_plan_says_what_a_launch_does() {
        let pair = Setup { name: "eyzflow + mediaroom on orca".into(), account: "orca".into(), repos: vec!["~/Work/eyz/eyzflow".into(), "~/Work/eyz/mediaroom/".into()] };
        assert_eq!(
            plan("Case exports as PDF", Some(&pair), &["isa"]),
            [
                "Name the space case-exports",
                "Open 2 sessions on orca: eyzflow, mediaroom",
                "Give each its own worktree, branch and dev port",
                "Introduce them to each other and hand out the goal",
            ]
        );
        let one = Setup { name: "landing".into(), account: String::new(), repos: vec!["landing-page".into()] };
        assert_eq!(
            plan("New product drawings", Some(&one), &["product-drawings"]),
            ["Name the space product-drawings-2", "Open 1 session: landing-page", "Give it its own worktree, branch and dev port", "Hand out the goal"]
        );
        // No setup: only what is known, the name; and no goal yet, not even that.
        assert_eq!(plan("Rework", None, &[]), ["Name the space rework"]);
        assert_eq!(plan(" ", None, &[]), ["Name the space from its goal"]);
        assert_eq!((repo_name("~/Work/eyz/eyzflow"), repo_name("eyzflow"), repo_name("/a/b/")), ("eyzflow", "eyzflow", "b"));
    }

    #[test]
    fn the_towers_setups_read_in_either_shape() {
        let wrapped = r#"{"setups":[{"name":"pair","account":"orca","repos":["~/a","~/b"]},{"name":"solo"},{"account":"x"},{"name":" "}]}"#;
        let list = setups_from(wrapped);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], Setup { name: "pair".into(), account: "orca".into(), repos: vec!["~/a".into(), "~/b".into()] });
        assert_eq!(list[1], Setup { name: "solo".into(), ..Default::default() });
        assert_eq!(setups_from(r#"[{"name":"pair","account":"orca","repos":[]}]"#).len(), 1);
        // A tower without the command prints nothing, or something else.
        assert!(setups_from("").is_empty() && setups_from("no setups yet").is_empty() && setups_from("{}").is_empty());
    }
}
