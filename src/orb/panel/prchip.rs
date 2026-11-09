//! A PR a session opened, as a chip in the chat (`crate::prs`): what it is
//! (repo, number, title, into which branch), how it stands (checks,
//! conflicts, open / merged / closed), whether its session waits for the
//! user's merge, and a Merge button.
//!
//! Merging is the user's alone and takes two clicks: Merge shows what will
//! happen (the base, the checks, the repo's merge method, warnings for main
//! or prod), then Merge into <base> does it. Failing checks take a third,
//! "Merge anyway", for a failure the user knows is unrelated; conflicts or a
//! draft disable it, saying why. It runs here, in the panel's process
//! (`prs::merge`: GitHub asked again, pinned to the commit shown, never
//! `--admin`), then tells the daemon (`PrMerged`), which checks with GitHub
//! and tells the session and the brain.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};

use super::{AMBER, Panel, ROSE, glass};
use crate::prs::{Pr, gate};

const GREEN: u32 = 0x6eeaa8;

/// Where a chip's merge stands, in this panel.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) enum Step {
    #[default]
    Idle,
    /// Merge was clicked: what will happen, and a button to do it.
    Confirm,
    /// Checks fail: the user is asked once more.
    Override,
    Merging,
    /// It didn't go: why.
    Failed(String),
}

impl Panel {
    /// The chip for the PR `key` (its chat entry: `title` from when it was found).
    pub(super) fn pr_chip(&mut self, key: &str, title: &str, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let pr = self.tower.read(cx).prs.get(key).cloned().unwrap_or_else(|| Pr { title: title.into(), ..Default::default() });
        let step = self.pr_steps.get(key).cloned().unwrap_or_default();
        let gate = gate(&pr);
        let (state_word, state_color) = match (pr.state.as_str(), pr.draft) {
            ("merged", _) => ("Merged", rgb(0xb79cff)),
            ("closed", _) => ("Closed", rgb(ROSE)),
            ("open", true) => ("Draft", rgb(0x9aa3b2)),
            ("open", false) => ("Open", rgb(GREEN)),
            _ => ("…", rgb(0x9aa3b2)),
        };
        let url = pr.url.clone();
        let head = h_flex()
            .gap_2()
            .items_center()
            .text_xs()
            .text_color(g.muted)
            .child(Icon::new(IconName::Github).xsmall())
            .child(
                div()
                    .id(SharedString::from(format!("pr-link-{key}")))
                    .min_w_0()
                    .truncate()
                    .cursor_pointer()
                    .hover(|s| s.text_color(g.text))
                    .child(format!("{} #{}", pr.repo, pr.number))
                    .on_click(move |_, _, cx| {
                        if !url.is_empty() {
                            cx.open_url(&url);
                        }
                    }),
            )
            .child(div().flex_1())
            .child(div().px_2().py(px(1.0)).rounded_full().border_1().border_color(state_color).text_color(state_color).child(state_word));
        let title_line = div().text_sm().font_weight(FontWeight::MEDIUM).line_height(px(19.0)).line_clamp(3).child(if pr.title.is_empty() {
            title.to_string()
        } else {
            pr.title.clone()
        });
        // Into which branch, its checks, whether it merges cleanly.
        let (check_icon, check_color, check_text) = match pr.checks.state.as_str() {
            "passing" => (IconName::CircleCheck, rgb(GREEN), "Checks passing".to_string()),
            "failing" => (IconName::CircleX, rgb(ROSE), format!("Failing: {}", pr.checks.failing.join(", "))),
            "pending" => (IconName::LoaderCircle, rgb(AMBER), format!("Checks running ({} of {})", pr.checks.total - pr.checks.pending.len(), pr.checks.total)),
            _ => (IconName::CircleAlert, rgb(0x9aa3b2), "No checks".to_string()),
        };
        let facts = v_flex()
            .gap(px(3.0))
            .text_xs()
            .child(
                h_flex()
                    .gap_1()
                    .text_color(g.muted)
                    .child(div().text_color(if crate::prs::is_prod(&pr.base) { rgb(AMBER) } else { g.text.into() }).child(format!("into {}", pr.base)))
                    .when(!pr.head.is_empty(), |el| el.child(format!("← {}", pr.head))),
            )
            .when(pr.open(), |el| {
                el.child(h_flex().gap_1().items_center().text_color(check_color).child(Icon::new(check_icon).xsmall()).child(div().min_w_0().line_clamp(2).child(check_text)))
                    .child(match pr.mergeable.as_str() {
                        "conflicting" => h_flex().gap_1().items_center().text_color(rgb(ROSE)).child(Icon::new(IconName::TriangleAlert).xsmall()).child("Conflicts"),
                        "mergeable" => h_flex().gap_1().items_center().text_color(g.muted).child(Icon::new(IconName::Check).xsmall()).child("No conflicts"),
                        _ => h_flex().gap_1().items_center().text_color(g.faint).child("Mergeable: GitHub is checking"),
                    })
            });
        // Who made it, and whether it waits for the user.
        let who = h_flex()
            .gap_2()
            .items_center()
            .text_xs()
            .text_color(g.faint)
            .child(div().min_w_0().truncate().child(pr.name.clone()))
            .when(pr.open() && pr.waiting, |el| {
                el.child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .px_2()
                        .py(px(1.0))
                        .rounded_full()
                        .bg(rgba(0xffcf7033))
                        .text_color(rgb(AMBER))
                        .child(div().size(px(6.0)).rounded_full().bg(rgb(AMBER)))
                        .child("Waiting for you"),
                )
            })
            .when_some(pr.merged_by.as_deref().filter(|_| pr.state == "merged"), |el, by| {
                el.child(div().text_color(g.muted).child(match by {
                    "mac" => "you merged it here".to_string(),
                    b if b.starts_with("phone:") => "you merged it from your phone".to_string(),
                    _ => "merged on GitHub".to_string(),
                }))
            });
        let actions = pr.open().then(|| self.pr_actions(key, &pr, &gate, &step, cx));
        v_flex()
            .w(px(310.0))
            .p_3()
            .gap_2()
            .rounded(px(18.0))
            .bg(g.bubble_tower)
            .border_1()
            .border_color(if pr.open() && pr.waiting { rgba(0xffcf7080) } else { g.edge.into() })
            .child(head)
            .child(title_line)
            .child(facts)
            .child(who)
            .children(actions)
            .into_any_element()
    }

    /// The Merge button, and what it asks before it merges.
    fn pr_actions(&mut self, key: &str, pr: &Pr, gate: &crate::prs::Gate, step: &Step, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let button = |id: String, label: String, primary: bool, enabled: bool| {
            div()
                .id(SharedString::from(id))
                .px_3()
                .py(px(5.0))
                .rounded_full()
                .text_sm()
                .when(primary && enabled, |el| el.bg(rgb(0xcce0ff)).text_color(rgb(0x16233a)).cursor_pointer().hover(|s| s.bg(rgb(0xe2edff))))
                .when(!primary && enabled, |el| el.border_1().border_color(g.edge).cursor_pointer().hover(|s| s.bg(rgba(0xffffff1f))))
                .when(!enabled, |el| el.bg(rgba(0xffffff14)).text_color(g.faint))
                .child(label)
        };
        let set = |k: String, to: Step| {
            cx.listener(move |this: &mut Panel, _, _, cx| {
                this.pr_steps.insert(k.clone(), to.clone());
                cx.notify();
            })
        };
        let k = key.to_string();
        match step {
            Step::Merging => h_flex().gap_2().items_center().text_sm().text_color(g.muted).child(Spinner::new().small()).child("Merging…").into_any_element(),
            Step::Idle | Step::Failed(_) => {
                let why = gate.blocked.clone();
                let mut col = v_flex().gap_1();
                if let Step::Failed(e) = step {
                    col = col.child(div().text_xs().text_color(rgb(ROSE)).child(format!("Not merged: {e}")));
                }
                let b = button(format!("pr-merge-{k}"), "Merge".into(), true, why.is_none());
                let b = if why.is_none() { b.on_click(set(k.clone(), Step::Confirm)) } else { b };
                col.child(h_flex().gap_2().items_center().child(b).when_some(why, |el, w| el.child(div().text_xs().text_color(g.muted).child(w)))).into_any_element()
            }
            Step::Confirm | Step::Override => {
                let overriding = *step == Step::Override;
                let mut lines: Vec<String> = vec![format!("Merge {} #{} into {} ({}).", pr.repo, pr.number, pr.base, pr.checks_line())];
                lines.extend(gate.warnings.iter().cloned());
                if let Some(o) = &gate.needs_override {
                    lines.push(o.clone());
                }
                // Failing checks: one more, explicit step.
                let next_label = match (&gate.needs_override, overriding) {
                    (Some(_), false) => "Checks fail: continue…".to_string(),
                    (Some(_), true) => "Merge anyway: the failure is unrelated".to_string(),
                    (None, _) => format!("Merge into {}", pr.base),
                };
                let go = button(format!("pr-go-{k}"), next_label, true, true);
                let go = if gate.needs_override.is_some() && !overriding {
                    go.on_click(set(k.clone(), Step::Override))
                } else {
                    let (key, repo, number, sha) = (k.clone(), pr.repo.clone(), pr.number, pr.head_sha.clone());
                    go.on_click(cx.listener(move |this, _, _, cx| this.merge_pr(&key, &repo, number, &sha, overriding, cx)))
                };
                v_flex()
                    .gap_2()
                    .p_2()
                    .rounded(px(12.0))
                    .bg(rgba(0x0000002e))
                    .children(lines.into_iter().enumerate().map(|(i, l)| {
                        div().text_xs().text_color(if i == 0 { g.text.into() } else { rgb(AMBER) }).line_height(px(16.0)).child(l)
                    }))
                    .child(h_flex().gap_2().flex_wrap().child(go).child(button(format!("pr-cancel-{k}"), "Cancel".into(), false, true).on_click(set(k, Step::Idle))))
                    .into_any_element()
            }
        }
    }

    /// Merges it as confirmed (in the background), then tells the daemon.
    fn merge_pr(&mut self, key: &str, repo: &str, number: u64, sha: &str, override_failing: bool, cx: &mut Context<Self>) {
        self.pr_steps.insert(key.to_string(), Step::Merging);
        cx.notify();
        let (key, repo, sha) = (key.to_string(), repo.to_string(), sha.to_string());
        let task = cx.background_executor().spawn({
            let (key, repo) = (key.clone(), repo.clone());
            async move {
                let r = crate::prs::merge(&crate::prs::gh_bin(), &repo, number, &sha, override_failing);
                if r.is_ok() {
                    // The daemon asks GitHub too, then tells the session and the brain.
                    let _ = crate::protocol::send(&crate::protocol::Request::PrMerged { pr: key, by: "mac".into() });
                }
                r
            }
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(_) => {
                        this.pr_steps.remove(&key);
                    }
                    Err(e) => {
                        this.pr_steps.insert(key.clone(), Step::Failed(e.message().to_string()));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
