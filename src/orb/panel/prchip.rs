//! The pull requests that wait for the user's merge, in the queue
//! (`crate::prs`): the PRs of one repo and space are one card (`merge_card`;
//! C5 in the design), listed dev before main, each with how it stands
//! (checks, conflicts, a draft).
//!
//! Merging is the user's alone and takes two steps: the card's button asks
//! about a PR (what will happen: the base, the checks, warnings for main or
//! prod), then "Confirm" does it. Failing checks take a third, "Merge
//! anyway", for a failure the user knows is unrelated; conflicts or a draft
//! hold it back, saying why. It runs here, in the window's process
//! (`prs::merge`: GitHub asked again, pinned to the commit shown, never
//! `--admin`), then tells the daemon (`PrMerged`), which checks with GitHub
//! and tells the session and the brain.
//!
//! The card's one button begins with the PRs that can be merged cleanly.
//! For one, it asks and merges it here. For several ("Merge all 3"), the
//! one confirmation is for the whole chain: the daemon merges them in order
//! by itself (`crate::merges`), waiting for GitHub between them and having
//! a PR that fell behind or conflicts fixed by its session; each row says
//! where its PR stands, "Stop" ends it, and only what can't be fixed comes
//! back to the card, saying why. A PR whose checks fail is held back, and
//! merged only from its own row, with the extra step. Its number keys do
//! what its buttons do (`merge_acts`). Where each merge stands is kept with
//! the tower's state (`Acts`, and the daemon's `merges`), so both windows
//! show it.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px, rgb, rgba};
use gpui_kit::component::{h_flex, v_flex};

use super::home::{LINE, SOFT, acts_row, card_box, card_title, cstat, cstat_busy, mbtn, meta_row};
use super::{AMBER, CYAN, Panel, ROSE, glass};
use crate::orb::model::{self, Decision, PrRef};
use crate::prs::{Gate, Pr, gate};

/// Where a PR's merge stands.
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
    /// Merges it as confirmed (in the background), then tells the daemon.
    pub(super) fn merge_pr(&mut self, key: &str, repo: &str, number: u64, sha: &str, override_failing: bool, cx: &mut Context<Self>) {
        self.act(cx, |a| a.steps.insert(key.to_string(), Step::Merging));
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
                        this.act(cx, |a| a.steps.remove(&key));
                        this.merged_here(&key, cx);
                    }
                    Err(e) => {
                        this.act(cx, |a| a.steps.insert(key.clone(), Step::Failed(e.message().to_string())));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// The PRs of a merge decision, as one card in the queue.

/// One PR of a merge card: where its merge stands and whether it may go.
#[derive(Debug, Clone)]
pub(super) struct Line {
    pub key: String,
    pub pr: Pr,
    pub step: Step,
    pub gate: Gate,
}

impl Line {
    /// It can be merged now with one confirmation: nothing blocks it and no check fails.
    fn clean(&self) -> bool {
        self.gate.blocked.is_none() && self.gate.needs_override.is_none()
    }

    /// Where its review stands, for its row (`crate::reviews::chip_line`)
    /// and in what colour: agreed is good news, the rest is to mind.
    fn review(&self) -> Option<(String, u32)> {
        let r = self.pr.review.as_ref()?;
        let line = if r.line.is_empty() { crate::reviews::chip_line(r) } else { r.line.clone() };
        let color = match r.status.as_str() {
            "agreed" => CYAN,
            "ended" => 0xcdd6e6,
            _ => AMBER,
        };
        Some((line, color))
    }

    /// Whether it needs a human code review, for its row (`crate::prscore`):
    /// the score's line, amber when a person must read it.
    fn score(&self) -> Option<(String, u32)> {
        let s = self.pr.score.as_ref()?;
        Some((s.line.clone(), if s.needs_review { AMBER } else { CYAN }))
    }

    /// How it stands, for its row: the words and whether they are bad news.
    fn standing(&self) -> (String, u32) {
        let c = &self.pr.checks;
        match () {
            _ if self.pr.draft => ("a draft".into(), ROSE),
            _ if self.pr.mergeable == "conflicting" => ("conflicts".into(), ROSE),
            _ if c.state == "failing" => (if c.failing.len() <= 1 { "1 check fails".into() } else { format!("{} checks fail", c.failing.len()) }, ROSE),
            _ if c.state == "pending" => ("checks running".into(), AMBER),
            _ if c.state == "none" => ("no checks".into(), 0xcdd6e6),
            _ => ("checks pass".into(), CYAN),
        }
    }
}

/// What a merge card's button, or the number key on it, does.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum MergeAct {
    /// The card's own button: ask about this PR, and after it the next.
    Start(String),
    /// Ask about one PR (its row, or another try).
    Ask(String),
    /// Checks fail: asked once more.
    Override(String),
    /// Merge it (`true`: although checks fail).
    Go(String, bool),
    /// Merge these, in order, by the tower (the whole chain, confirmed once).
    GoAll(Vec<String>),
    /// Stop the tower's chain (its id): nothing more is merged.
    Stop(String),
    Cancel(String),
}

/// A "Merge all N" the daemon runs for a card (its `merges` in the state).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Run {
    pub id: String,
    pub over: bool,
    /// Each PR's key and where it stands (`crate::merges::Step::line`).
    pub links: Vec<(String, String)>,
    /// It stopped at a PR that came back to the user: why.
    pub back: Option<String>,
}

impl Run {
    /// The card's chain as the daemon's state has it: the one running, else
    /// the last one that came back to the user.
    pub fn of(state: &serde_json::Value, card: &str) -> Option<Run> {
        let all: Vec<&serde_json::Value> = state["merges"].as_array().into_iter().flatten().filter(|c| c["card"] == card).collect();
        let pick = all.iter().rev().find(|c| c["over"] != true).or_else(|| all.iter().rev().find(|c| c["links"].as_array().is_some_and(|l| l.iter().any(|x| x["step"]["step"] == "back"))))?;
        let links: Vec<(String, String)> = pick["links"].as_array().into_iter().flatten().map(|l| (l["key"].as_str().unwrap_or("").to_string(), l["line"].as_str().unwrap_or("").to_string())).collect();
        let back = pick["links"].as_array().into_iter().flatten().find(|l| l["step"]["step"] == "back").map(|l| format!("#{} {}", l["number"], l["line"].as_str().unwrap_or("")));
        Some(Run { id: pick["id"].as_str().unwrap_or("").to_string(), over: pick["over"] == true, links, back })
    }

    fn line(&self, key: &str) -> Option<&str> {
        self.links.iter().find(|(k, _)| k == key).map(|(_, l)| l.as_str())
    }
}

/// A merge card's buttons as it stands, key 1 first: (label, the main one,
/// what it does). At rest it is one button, named for the PRs it will merge
/// ("Merge both, dev first"): those that can go cleanly.
/// `chained`: the card's own button began it (several PRs: confirmed as a
/// whole chain); `run`: the tower's chain for this card, while it merges.
pub(super) fn merge_acts(lines: &[Line], chained: bool, run: Option<&Run>) -> Vec<(String, bool, MergeAct)> {
    let cancel = |k: &str| ("Cancel".to_string(), false, MergeAct::Cancel(k.to_string()));
    if let Some(r) = run.filter(|r| !r.over) {
        return vec![("Stop merging".into(), false, MergeAct::Stop(r.id.clone()))];
    }
    let clean: Vec<&Line> = lines.iter().filter(|l| l.clean()).collect();
    if let Some(l) = lines.iter().find(|l| l.step != Step::Idle) {
        let k = l.key.clone();
        if chained && l.step == Step::Confirm && clean.len() > 1 && l.clean() {
            let label = if clean.len() == 2 { "Confirm: merge both in order".to_string() } else { format!("Confirm: merge all {} in order", clean.len()) };
            return vec![(label, true, MergeAct::GoAll(clean.iter().map(|l| l.key.clone()).collect())), cancel(&l.key)];
        }
        return match (&l.step, &l.gate.needs_override) {
            (Step::Merging, _) => vec![],
            (Step::Failed(_), _) => vec![("Try again".into(), true, MergeAct::Ask(k)), cancel(&l.key)],
            (Step::Override, _) => vec![("Merge anyway: the failure is unrelated".into(), true, MergeAct::Go(k, true)), cancel(&l.key)],
            (_, Some(_)) => vec![("Checks fail: continue…".into(), true, MergeAct::Override(k)), cancel(&l.key)],
            (_, None) => vec![(format!("Confirm: merge #{}", l.pr.number), true, MergeAct::Go(k, false)), cancel(&l.key)],
        };
    }
    if let Some(l) = clean.first() {
        let refs: Vec<PrRef> = clean.iter().map(|l| PrRef { repo: l.pr.repo.clone(), number: l.pr.number, base: l.pr.base.clone(), ..Default::default() }).collect();
        return vec![(model::merge_label(&refs), true, MergeAct::Start(l.key.clone()))];
    }
    // Nothing merges cleanly: the first that can at all, with its extra step.
    match lines.iter().find(|l| l.gate.blocked.is_none()) {
        Some(l) => vec![(format!("Review #{}: its checks fail", l.pr.number), false, MergeAct::Ask(l.key.clone()))],
        None => vec![],
    }
}

/// What the other PRs of the card do while `this` one is asked about:
/// "#1277 waits for its checks."
fn others_line(this: &Line, lines: &[Line]) -> Option<String> {
    let parts: Vec<String> = lines
        .iter()
        .filter(|l| l.key != this.key)
        .map(|l| {
            let n = l.pr.number;
            match () {
                _ if l.gate.blocked.is_some() => format!("#{n} can't be merged yet."),
                _ if l.gate.needs_override.is_some() => format!("#{n} is held back: its checks fail."),
                _ if l.pr.checks.state == "pending" => format!("#{n} waits for its checks."),
                _ => format!("#{n} is next."),
            }
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

impl Panel {
    /// The PRs of a merge decision as `prs.json` has them now (one the tower
    /// doesn't know yet, as the decision describes it).
    fn merge_lines(&self, d: &Decision, cx: &gpui::App) -> Vec<Line> {
        let t = self.tower.read(cx);
        d.prs
            .iter()
            .map(|r| {
                let key = r.key();
                // `prs.json` knows it best (its commit, its checks by name); else the decision's own word.
                let (pr, gate) = match t.prs.get(&key) {
                    Some(pr) => (pr.clone(), gate(pr)),
                    None => {
                        let pr = described(r);
                        let gate = gate_of(r).unwrap_or_else(|| gate(&pr));
                        (pr, gate)
                    }
                };
                Line { step: t.acts.steps.get(&key).cloned().unwrap_or_default(), gate, pr, key }
            })
            .filter(|l| l.pr.open())
            .collect()
    }

    /// The card's buttons as it stands (the number keys follow them).
    pub(super) fn merge_acts(&self, d: &Decision, cx: &gpui::App) -> Vec<(String, bool, MergeAct)> {
        let (chained, run) = (self.acts(cx).chain.contains(&d.id), self.run_of(d, cx));
        merge_acts(&self.merge_lines(d, cx), chained, run.as_ref())
    }

    pub(super) fn merge_act(&mut self, d: &Decision, act: MergeAct, cx: &mut Context<Self>) {
        let card = d.id.clone();
        match act {
            MergeAct::Start(k) => self.act(cx, |a| {
                a.chain.insert(card);
                a.steps.insert(k, Step::Confirm);
            }),
            MergeAct::Ask(k) => self.act(cx, |a| drop(a.steps.insert(k, Step::Confirm))),
            MergeAct::Override(k) => self.act(cx, |a| drop(a.steps.insert(k, Step::Override))),
            MergeAct::Cancel(k) => self.act(cx, |a| {
                a.steps.remove(&k);
                a.chain.remove(&card);
            }),
            MergeAct::GoAll(keys) => self.merge_chain(&card, keys, cx),
            MergeAct::Stop(id) => {
                let req = crate::protocol::Request::MergeChain { action: "stop".into(), id, card: card.clone(), prs: vec![], by: String::new() };
                cx.background_executor().spawn(async move { drop(crate::protocol::send(&req)) }).detach();
            }
            MergeAct::Go(k, anyway) => match self.tower.read(cx).prs.get(&k).cloned() {
                Some(pr) => self.merge_pr(&k, &pr.repo, pr.number, &pr.head_sha, anyway, cx),
                None => self.act(cx, |a| drop(a.steps.insert(k, Step::Failed("the tower hasn't looked at this pull request yet".into())))),
            },
        }
        cx.notify();
    }

    /// The tower's chain for this card, as the daemon's state has it.
    pub(super) fn run_of(&self, d: &Decision, cx: &gpui::App) -> Option<Run> {
        self.tower.read(cx).state.as_ref().and_then(|s| Run::of(s, &d.id))
    }

    /// "Merge all N", confirmed: the daemon merges them in order from now on.
    fn merge_chain(&mut self, card: &str, keys: Vec<String>, cx: &mut Context<Self>) {
        let first = keys.first().cloned().unwrap_or_default();
        self.act(cx, |a| {
            for k in &keys {
                a.steps.remove(k);
            }
            a.steps.insert(first.clone(), Step::Merging);
        });
        let req = crate::protocol::Request::MergeChain { action: "start".into(), id: String::new(), card: card.to_string(), prs: keys.clone(), by: "mac".into() };
        let task = cx.background_executor().spawn(async move { crate::protocol::send(&req) });
        let card = card.to_string();
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                let failed = match r {
                    Ok(r) if r.ok => None,
                    Ok(r) => Some(r.error.unwrap_or_else(|| "the tower said no".into())),
                    Err(e) => Some(format!("the tower isn't running ({e:#})")),
                };
                this.act(cx, |a| {
                    a.chain.remove(&card);
                    match failed {
                        Some(e) => drop(a.steps.insert(first.clone(), Step::Failed(e))),
                        None => drop(a.steps.remove(&first)),
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// A PR was merged from here: its receipt, and if the card's own button
    /// began it, the next PR of the card asks.
    fn merged_here(&mut self, key: &str, cx: &mut Context<Self>) {
        let all = self.decisions(cx);
        let card = all.iter().find(|d| d.prs.iter().any(|p| p.key() == key)).cloned();
        self.act(cx, |a| a.merged.insert(key.to_string()));
        let said = match self.tower.read(cx).prs.get(key) {
            Some(pr) => format!("Merged #{} into {}.", pr.number, pr.base),
            None => "Merged.".to_string(),
        };
        self.tower.update(cx, |t, cx| {
            t.deck.note(key, said, crate::history::now_ms());
            cx.notify();
        });
        let Some(d) = card.filter(|d| self.acts(cx).chain.contains(&d.id)) else { return };
        let next = self.merge_lines(&d, cx).into_iter().find(|l| l.key != key && l.clean());
        self.act(cx, |a| match next {
            Some(l) => drop(a.steps.insert(l.key, Step::Confirm)),
            None => drop(a.chain.remove(&d.id)),
        });
    }

    /// A merge decision as the queue's open card: its PRs in order, and the
    /// two steps of merging one.
    pub(super) fn merge_card(&mut self, d: &Decision, tag: &str, now_ms: u64, cx: &mut Context<Self>) -> AnyElement {
        let g = glass();
        let lines = self.merge_lines(d, cx);
        let run = self.run_of(d, cx);
        let acts = merge_acts(&lines, self.acts(cx).chain.contains(&d.id), run.as_ref());
        let auto_on = self.tower.read(cx).s("auto_merge").as_bool().unwrap_or(false);
        let running = run.as_ref().filter(|r| !r.over);
        let active = lines.iter().find(|l| l.step != Step::Idle).filter(|_| running.is_none());
        let mut rows = v_flex().gap(px(6.0));
        for l in &lines {
            let (word, color) = l.standing();
            let (d2, key) = (d.clone(), l.key.clone());
            let can_ask = l.gate.blocked.is_none() && active.is_none();
            rows = rows.child(
                v_flex()
                    .id(SharedString::from(format!("merge-row-{}", l.key)))
                    .px(px(10.0))
                    .py(px(7.0))
                    .gap(px(1.0))
                    .rounded(px(10.0))
                    .bg(rgba(SOFT))
                    .text_size(px(13.0))
                    .child(
                        h_flex()
                            .gap(px(10.0))
                            .items_center()
                            .child(div().flex_none().font_weight(FontWeight::SEMIBOLD).child(format!("#{}", l.pr.number)))
                            .child(div().flex_1().min_w_0().truncate().text_color(g.muted).child(format!("into {}", l.pr.base)))
                            .child(div().flex_none().text_color(rgb(color)).child(word)),
                    )
                    .when(!l.pr.title.is_empty(), |el| el.child(div().text_size(px(12.0)).text_color(g.faint).truncate().child(l.pr.title.clone())))
                    .children(l.review().map(|(line, color)| div().text_size(px(12.0)).text_color(rgb(color)).truncate().child(line)))
                    .children(l.score().map(|(line, color)| div().text_size(px(12.0)).line_height(px(16.0)).text_color(rgb(color)).line_clamp(2).child(line)))
                    .children(crate::prscore::auto_line(&l.pr, auto_on).map(|line| div().text_size(px(12.0)).text_color(g.faint).truncate().child(line)))
                    // Where the tower's chain has it.
                    .children(running.and_then(|r| r.line(&l.key)).map(|line| {
                        let color = if line.starts_with("back to you") { ROSE } else if line == "merged" { CYAN } else { AMBER };
                        div().text_size(px(12.0)).text_color(rgb(color)).child(line.to_string())
                    }))
                    .when(can_ask, |el| {
                        el.cursor_pointer().hover(|s| s.bg(rgba(LINE))).on_click(cx.listener(move |this, _, _, cx| this.merge_act(&d2, MergeAct::Ask(key.clone()), cx)))
                    }),
            );
        }
        // The second step: what merging this one does, and what the others do meanwhile.
        let mut said: Vec<AnyElement> = vec![];
        if let Some(r) = running {
            let now = r.links.iter().find(|(k, l)| l != "merged" && !l.starts_with("passed over") && lines.iter().any(|x| &x.key == k)).or(r.links.last());
            let words = now.map(|(k, l)| format!("Merging in order: #{}: {l}.", k.rsplit('#').next().unwrap_or(""))).unwrap_or_else(|| "Merging in order…".into());
            said.push(cstat_busy(words));
        } else if let Some(why) = run.as_ref().and_then(|r| r.back.clone()).filter(|_| active.is_none()) {
            said.push(cstat(format!("Merging in order stopped: {why}."), true));
        }
        if let Some(l) = active {
            match &l.step {
                Step::Merging => said.push(cstat_busy(format!("Merging #{}…", l.pr.number))),
                Step::Failed(e) => said.push(cstat(format!("Not merged: {e}"), true)),
                _ => {
                    let mut line = format!("Merging #{} puts it into {} ({}).", l.pr.number, l.pr.base, l.pr.checks_line());
                    if let Some(rest) = others_line(l, &lines) {
                        line = format!("{line} {rest}");
                    }
                    said.push(cstat(line, false));
                    for w in l.gate.warnings.iter().chain(&l.gate.needs_override) {
                        said.push(div().text_size(px(12.0)).line_height(px(16.0)).text_color(rgb(AMBER)).child(w.clone()).into_any_element());
                    }
                }
            }
        } else if acts.is_empty() {
            // None of them can be merged: say why.
            said.extend(lines.iter().filter_map(|l| l.gate.blocked.clone()).take(1).map(|why| cstat(why, false)));
        }
        let mut buttons = acts_row();
        for (i, (label, primary, act)) in acts.iter().enumerate() {
            let (d2, act) = (d.clone(), act.clone());
            buttons = buttons.child(mbtn(format!("merge-{}-{i}", d.id), label, *primary, Some(i)).on_click(cx.listener(move |this, _, _, cx| this.merge_act(&d2, act.clone(), cx))));
        }
        card_box(true)
            .child(meta_row(Some(tag), &d.space, &d.badge(), format!("{} · {}", d.stake(), d.waited(now_ms))))
            .child(card_title(&d.title))
            .child(rows)
            .children(said)
            .when(!acts.is_empty(), |el| el.child(buttons))
            .into_any_element()
    }
}

/// A PR known only from its decision: enough to show its row.
fn described(r: &PrRef) -> Pr {
    Pr {
        repo: r.repo.clone(),
        number: r.number,
        base: r.base.clone(),
        title: r.title.clone(),
        url: r.url.clone(),
        session: r.session.clone(),
        state: "open".into(),
        mergeable: "mergeable".into(),
        checks: crate::prs::Checks {
            state: match r.checks.as_str() {
                "fail" => "failing",
                "pending" => "pending",
                "none" => "none",
                _ => "passing",
            }
            .into(),
            ..Default::default()
        },
        review: r.review.clone(),
        score: r.score.clone(),
        ..Default::default()
    }
}

/// Whether it can be merged, as its decision says (a daemon that doesn't say: None).
fn gate_of(r: &PrRef) -> Option<Gate> {
    let m = r.merge.as_ref()?;
    let blocked = m.blocked.clone().or_else(|| (!m.can).then(|| "It can't be merged yet.".to_string()));
    Some(Gate { blocked, needs_override: m.needs_override.clone(), warnings: m.warnings.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(number: u64, base: &str, checks: &str, step: Step) -> Line {
        let pr = Pr {
            repo: "acme/shop".into(),
            number,
            base: base.into(),
            state: "open".into(),
            mergeable: "mergeable".into(),
            checks: crate::prs::Checks { state: checks.into(), failing: if checks == "failing" { vec!["lint".into()] } else { vec![] }, ..Default::default() },
            ..Default::default()
        };
        Line { key: pr.key(), gate: gate(&pr), pr, step }
    }

    fn labels(acts: &[(String, bool, MergeAct)]) -> Vec<&str> {
        acts.iter().map(|a| a.0.as_str()).collect()
    }

    #[test]
    fn a_merge_card_asks_once_more_before_it_merges() {
        let both = "Merge both, dev first";
        // At rest: the one button, named for what it merges; it begins with the first clean PR.
        let idle = [line(1273, "dev", "passing", Step::Idle), line(1277, "main", "passing", Step::Idle)];
        assert_eq!(merge_acts(&idle, false, None), [(both.to_string(), true, MergeAct::Start("acme/shop#1273".into()))]);
        // Asked: the second step names the PR; Cancel is key 2.
        let asked = [line(1273, "dev", "passing", Step::Confirm), line(1277, "main", "passing", Step::Idle)];
        let acts = merge_acts(&asked, false, None);
        assert_eq!(labels(&acts), ["Confirm: merge #1273", "Cancel"]);
        assert_eq!(acts[0].2, MergeAct::Go("acme/shop#1273".into(), false));
        assert_eq!(acts[1].2, MergeAct::Cancel("acme/shop#1273".into()));
        assert_eq!(others_line(&asked[0], &asked).as_deref(), Some("#1277 is next."));
        // While it merges there is nothing to press.
        assert!(merge_acts(&[line(1273, "dev", "passing", Step::Merging)], false, None).is_empty());
        // It didn't go: another try, or leave it.
        let failed = [line(1273, "dev", "passing", Step::Failed("a review is required".into()))];
        assert_eq!(labels(&merge_acts(&failed, false, None)), ["Try again", "Cancel"]);
    }

    /// "Merge all 3" is confirmed once, for the whole chain, which the tower
    /// then merges in order; while it does, the card can only stop it, and
    /// each row says where its PR stands.
    #[test]
    fn merge_all_is_confirmed_once_and_the_tower_merges_the_chain() {
        let k = |n: u64| format!("acme/shop#{n}");
        let idle = [line(1374, "dev", "none", Step::Idle), line(1375, "dev", "none", Step::Idle), line(1376, "dev", "passing", Step::Idle)];
        assert_eq!(merge_acts(&idle, false, None), [("Merge all 3".to_string(), true, MergeAct::Start(k(1374)))]);
        let asked = [line(1374, "dev", "none", Step::Confirm), line(1375, "dev", "none", Step::Idle), line(1376, "dev", "passing", Step::Idle)];
        let acts = merge_acts(&asked, true, None);
        assert_eq!(labels(&acts), ["Confirm: merge all 3 in order", "Cancel"]);
        assert_eq!(acts[0].2, MergeAct::GoAll(vec![k(1374), k(1375), k(1376)]));
        // One that fails its checks isn't in the chain (it stays for its own row).
        let mixed = [line(1374, "dev", "none", Step::Confirm), line(1375, "dev", "failing", Step::Idle), line(1376, "dev", "passing", Step::Idle)];
        assert_eq!(merge_acts(&mixed, true, None)[0], ("Confirm: merge both in order".to_string(), true, MergeAct::GoAll(vec![k(1374), k(1376)])));
        // A single PR keeps its own confirmation, merged here.
        assert_eq!(labels(&merge_acts(&[line(1374, "dev", "none", Step::Confirm)], true, None)), ["Confirm: merge #1374", "Cancel"]);

        // The daemon's chain, as the state carries it.
        let state = serde_json::json!({ "merges": [
            { "id": "m1", "card": "d9", "over": false, "links": [
                { "key": k(1374), "number": 1374, "step": { "step": "merged" }, "line": "merged" },
                { "key": k(1375), "number": 1375, "step": { "step": "waiting", "why": "GitHub is working out whether it merges cleanly" }, "line": "GitHub is working out whether it merges cleanly" },
                { "key": k(1376), "number": 1376, "step": { "step": "queued" }, "line": "next" } ] },
            { "id": "m0", "card": "d8", "over": true, "links": [
                { "key": k(9), "number": 9, "step": { "step": "back", "why": "its checks fail" }, "line": "back to you: 1 check failing: test after its session's fix" } ] } ] });
        let run = Run::of(&state, "d9").unwrap();
        assert_eq!((run.id.as_str(), run.over, run.line(&k(1375))), ("m1", false, Some("GitHub is working out whether it merges cleanly")));
        assert_eq!(merge_acts(&idle[1..], false, Some(&run)), [("Stop merging".to_string(), false, MergeAct::Stop("m1".into()))]);
        // One that came back: said on its card, whose button is the usual one again.
        let back = Run::of(&state, "d8").unwrap();
        assert_eq!(back.back.as_deref(), Some("#9 back to you: 1 check failing: test after its session's fix"));
        assert_eq!(labels(&merge_acts(&[line(9, "dev", "passing", Step::Idle)], false, Some(&back))), ["Merge #9"]);
        assert!(Run::of(&state, "d7").is_none());
    }

    #[test]
    fn a_failing_check_holds_its_pull_request_back() {
        // The button skips the failing one (and is named for the other alone); it says so while the other is asked about.
        let mixed = [line(1273, "dev", "failing", Step::Idle), line(1277, "main", "passing", Step::Idle)];
        assert_eq!(merge_acts(&mixed, false, None), [("Merge #1277".to_string(), true, MergeAct::Start("acme/shop#1277".into()))]);
        let asked = [line(1273, "dev", "passing", Step::Confirm), line(1277, "main", "failing", Step::Idle), line(1280, "main", "pending", Step::Idle)];
        assert_eq!(others_line(&asked[0], &asked).as_deref(), Some("#1277 is held back: its checks fail. #1280 waits for its checks."));
        // From its own row it takes two more steps, the second explicit.
        let own = [line(1277, "main", "failing", Step::Confirm)];
        let acts = merge_acts(&own, false, None);
        assert_eq!((labels(&acts), &acts[0].2), (vec!["Checks fail: continue…", "Cancel"], &MergeAct::Override("acme/shop#1277".into())));
        let sure = [line(1277, "main", "failing", Step::Override)];
        assert_eq!(merge_acts(&sure, false, None)[0].2, MergeAct::Go("acme/shop#1277".into(), true));
        // Only failing ones left: no one-button merge, a way to look at the first.
        let only = [line(1277, "main", "failing", Step::Idle)];
        assert_eq!(merge_acts(&only, false, None), [("Review #1277: its checks fail".to_string(), false, MergeAct::Ask("acme/shop#1277".into()))]);
        assert_eq!(only[0].standing(), ("1 check fails".to_string(), ROSE));
        // A draft or a conflict can't be merged at all.
        let mut draft = line(9, "dev", "passing", Step::Idle);
        draft.pr.draft = true;
        draft.gate = gate(&draft.pr);
        assert!(merge_acts(&[draft.clone()], false, None).is_empty());
        assert_eq!(draft.standing().0, "a draft");
        assert_eq!(others_line(&only[0], &[only[0].clone(), draft]).as_deref(), Some("#9 can't be merged yet."));
    }

    #[test]
    fn a_merge_cards_row_says_where_its_review_stands_and_merging_stays_the_users() {
        let brief = |status: &str, rounds: u32, critical: u32| {
            let mut b = crate::reviews::Brief { status: status.into(), rounds, critical, ..Default::default() };
            b.line = crate::reviews::chip_line(&b);
            b
        };
        let with = |b: Option<crate::reviews::Brief>| {
            let mut l = line(12, "dev", "passing", Step::Idle);
            l.pr.review = b;
            l.gate = gate(&l.pr);
            l
        };
        assert_eq!(with(None).review(), None, "no review: no line");
        assert_eq!(with(Some(brief("reviewing", 0, 0))).review(), Some(("Reviewing…".into(), AMBER)));
        assert_eq!(with(Some(brief("changes", 2, 2))).review(), Some(("Review: 2 critical, round 2".into(), AMBER)));
        assert_eq!(with(Some(brief("agreed", 2, 0))).review(), Some(("Review: agreed".into(), CYAN)));
        // Not agreed: said plainly when it is asked about, and still the user's to merge.
        let open = [with(Some(brief("changes", 2, 2)))];
        assert!(open[0].gate.warnings.iter().any(|w| w == "The review isn't agreed: 2 critical issues open, round 2."));
        assert_eq!(merge_acts(&open, false, None), [("Merge #12".to_string(), true, MergeAct::Start("acme/shop#12".into()))]);
        let agreed = with(Some(brief("agreed", 2, 0)));
        assert!(agreed.gate.warnings.iter().all(|w| !w.contains("review")));
        // One the window knows only from its decision keeps its review.
        let r = PrRef { repo: "acme/shop".into(), number: 7, review: Some(brief("stuck", 3, 1)), ..Default::default() };
        assert_eq!(described(&r).review.unwrap().line, "Review: stuck after 3 rounds, yours to decide");
    }

    #[test]
    fn a_merge_cards_row_says_whether_the_pr_needs_a_human_review() {
        let ev = |files: &[&str]| crate::prscore::Evidence { title: "x".into(), files: files.iter().map(|f| f.to_string()).collect(), ..Default::default() };
        let with = |files: &[&str]| {
            let mut l = line(12, "dev", "passing", Step::Idle);
            l.pr.score = Some(crate::prscore::score(&ev(files)));
            l.gate = gate(&l.pr);
            l
        };
        let m = with(&["db/migrations/0042_orders.sql"]);
        assert_eq!(m.score(), Some(("Needs review: a database migration (db/migrations/0042_orders.sql)".into(), AMBER)));
        assert!(m.gate.warnings.contains(&"Needs review: a database migration (db/migrations/0042_orders.sql).".to_string()), "said before the merge too");
        let ui = with(&["src/Cart.tsx"]);
        assert_eq!(ui.score(), Some((crate::prscore::LOW_LINE.into(), CYAN)));
        assert!(ui.gate.warnings.iter().all(|w| !w.contains("review")));
        assert_eq!(line(12, "dev", "passing", Step::Idle).score(), None, "no score: no line");
        // One the window knows only from its decision keeps its score.
        let r = PrRef { repo: "acme/shop".into(), number: 7, score: m.pr.score.clone(), ..Default::default() };
        assert!(described(&r).score.unwrap().needs_review);
    }

    #[test]
    fn a_pull_request_the_tower_has_not_seen_shows_as_its_decision_says() {
        let r = PrRef { repo: "acme/shop".into(), number: 7, base: "dev".into(), title: "Fix the cart".into(), checks: "fail".into(), ..Default::default() };
        let pr = described(&r);
        assert_eq!((pr.key().as_str(), pr.open(), pr.checks.state.as_str(), pr.title.as_str()), ("acme/shop#7", true, "failing", "Fix the cart"));
        assert!(gate(&pr).needs_override.is_some());
        assert!(gate_of(&r).is_none(), "a daemon that doesn't say: worked out from its checks");
        // No checks ran: said so, not "checks pass".
        assert_eq!(described(&PrRef { checks: "none".into(), ..r.clone() }).checks.state, "none");
        // The daemon's own word on whether it merges (`prs[].merge`) is taken as it is.
        let held = PrRef {
            merge: Some(crate::orb::model::MergeGate { can: false, blocked: Some("It's a draft: mark it ready for review first.".into()), needs_override: None, warnings: vec!["It merges into main: that ships to production.".into()] }),
            ..r.clone()
        };
        let g = gate_of(&held).unwrap();
        assert_eq!((g.blocked.as_deref(), g.warnings.len()), (Some("It's a draft: mark it ready for review first."), 1));
        let anyway = PrRef { merge: Some(crate::orb::model::MergeGate { can: true, needs_override: Some("1 check failing: lint.".into()), ..Default::default() }), ..r };
        let g = gate_of(&anyway).unwrap();
        assert!(g.blocked.is_none() && g.needs_override.is_some());
    }
}
