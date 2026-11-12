//! Settings in the full window (W17 in the design): how the tower behaves,
//! in one place, a card each.
//!
//! - **How much it tells you** (`tower tell`): decisions, decisions and
//!   results, or everything.
//! - **Tower's model** (`tower model`): the model and the effort every
//!   tower brain is started on (the main one, and each space's own), what
//!   the running brain uses, and "Restart the tower's brain" when a
//!   restart is needed for the choice to apply.
//! - **Voice**: what it says out loud, `Off`, `Decisions` or `Replies and
//!   decisions` (`tower tell --voice`; off is `tower mute`), and, as a
//!   switch of its own, whether everything it says plays on this Mac
//!   (`tower mac-voice`).
//! - **Questions it answers for you**: the policy, as before.
//! - **Follow and Mute**, per open space (the switches each space has at
//!   its top).
//! - **Behaviors**: a line, and the way to its view.
//! - **Phone** (`phone`), the width of the pane: where the tower can be
//!   reached now (the network, Tailscale), pairing a phone in place, and the
//!   paired phones. `Scope::Phone` opens Settings scrolled to it.
//! - **Accounts** (the accounts mod's) and the **projects** the launcher
//!   offers (`tower setups`), with the way to their page.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px, rgba};
use gpui_kit::component::{h_flex, v_flex};

use super::home::{SOFT, acts_row, mbtn, tg};
use super::window::whead;
use super::{Panel, glass};
use crate::orb::model::{self, Missions};
use crate::orb::nav::Scope;

/// What a tell level means, in a line.
fn tell_line(level: Option<&str>) -> &'static str {
    match level {
        Some("decisions") => "Only what is yours reaches you. Everything else goes to the record.",
        Some("results") => "Also one receipt when something you asked for is finished.",
        Some("everything") => "Every event as it happens, as before.",
        _ => "Choose what reaches you. The rest goes to the record.",
    }
}

/// What a voice setting means, in a line (the design's words; `tell::Voice::says`).
fn voice_line(voice: &str) -> &'static str {
    crate::tell::Voice::parse(voice).unwrap_or_default().says()
}

/// What a questions policy means, in a line.
fn policy_line(policy: &str) -> &'static str {
    match policy {
        "keep-moving" => "It answers the borderline ones too.",
        "ask-everything" => "Nothing is answered for you: every question comes to your queue.",
        _ => "The tower answers routine questions and leaves the rest to you.",
    }
}

/// What the model's card says under its choices: what runs, and when a
/// choice applies (`says` is the tower's own sentence; it already says so
/// when a restart is needed, when the user's arguments win, or for Codex).
fn model_line(m: &model::BrainModel) -> String {
    if m.restart_needed || m.locked_by_args || m.program == "codex" {
        m.says.clone()
    } else {
        format!("{} A change applies when the tower’s brain restarts.", m.says)
    }
}

/// What the talk key is, as the state's `talk_key` says it.
fn talk_key_line(key: &str) -> &'static str {
    match key {
        "off" => "The talk key is off.",
        "needs-permission" => "The talk key needs Input Monitoring allowed for Herdr Core.",
        "left_ctrl" => "Talk key: left Control.",
        "right_ctrl" => "Talk key: right Control.",
        _ => "Talk key: Control.",
    }
}

/// `default and orca`, `a, b and c`.
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A card of the settings: its title and what is in it.
fn card(title: &str, items: Vec<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .min_w_0()
        .p(px(14.0))
        .gap(px(9.0))
        .rounded(px(16.0))
        .bg(rgba(SOFT))
        .child(div().text_size(px(14.0)).font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
        .children(items)
        .into_any_element()
}

/// A paragraph of a card.
fn para(text: impl Into<SharedString>) -> AnyElement {
    div().text_size(px(13.0)).line_height(px(18.0)).text_color(glass().muted).child(text.into()).into_any_element()
}

impl Panel {
    /// `tower tell <level>`: chosen at once, and back if the tower refuses.
    fn set_tell(&mut self, level: &'static str, cx: &mut Context<Self>) {
        if self.running("tell") {
            return;
        }
        if let Some(full) = self.full.as_mut() {
            full.tell = Some(level.to_string());
        }
        self.run("tell", vec!["tell".into(), level.into()], cx, |this, result, _| {
            if result.is_err()
                && let Some(full) = this.full.as_mut()
            {
                full.tell = None;
            }
        });
    }

    /// `tower tell --voice <off|decisions|replies>`: chosen at once, and back
    /// if the tower refuses.
    fn set_voice(&mut self, voice: &'static str, cx: &mut Context<Self>) {
        if self.running("voice") {
            return;
        }
        if let Some(full) = self.full.as_mut() {
            full.voice = Some(voice.to_string());
        }
        self.run("voice", vec!["tell".into(), "--voice".into(), voice.into()], cx, |this, result, _| {
            if result.is_err()
                && let Some(full) = this.full.as_mut()
            {
                full.voice = None;
            }
        });
    }

    /// `tower model <model>` or `tower model --effort <level>`: chosen at
    /// once, and back if the tower refuses. It is kept for the next start
    /// of a brain: nothing is typed into the one that runs.
    fn set_model(&mut self, model: Option<String>, effort: Option<String>, cx: &mut Context<Self>) {
        if self.running("model") {
            return;
        }
        if let Some(full) = self.full.as_mut() {
            let chosen = full.model.get_or_insert_with(Default::default);
            if model.is_some() {
                chosen.0 = model.clone();
            }
            if effort.is_some() {
                chosen.1 = effort.clone();
            }
        }
        let mut args = vec!["model".to_string()];
        args.extend(model);
        if let Some(e) = effort {
            args.extend(["--effort".to_string(), e]);
        }
        self.run("model", args, cx, |this, result, _| {
            if result.is_err()
                && let Some(full) = this.full.as_mut()
            {
                full.model = None;
            }
        });
    }

    /// "Restart the tower's brain": `tower model --restart`, so every brain
    /// that runs starts again on the setting.
    fn restart_brains(&mut self, cx: &mut Context<Self>) {
        self.run("model", vec!["model".into(), "--restart".into()], cx, |_, _, _| {});
    }

    /// The questions policy: through the daemon, as the phone sets it.
    fn set_policy(&mut self, policy: &'static str, cx: &mut Context<Self>) {
        self.asks.policy = Some(policy.to_string());
        if let Some(full) = self.full.as_mut() {
            full.notes.remove("policy");
        }
        let task = cx.background_executor().spawn(async move { crate::answers::set_policy(policy) });
        cx.spawn(async move |this, cx| {
            let failed = task.await.err();
            let _ = this.update(cx, |this, cx| {
                if let Some(e) = failed {
                    this.asks.policy = None;
                    if let Some(full) = this.full.as_mut() {
                        full.notes.insert("policy".into(), (false, format!("{e:#}")));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// The settings' head and body.
    pub(super) fn settings_view(&mut self, missions: &Missions, cx: &mut Context<Self>) -> (AnyElement, Vec<AnyElement>) {
        let g = glass();
        let head = whead("Settings", vec![div().child("How the tower behaves, in one place.").into_any_element()], None);
        let (alive, said_voice, on_mac, talk_key, said_level, said_policy, behaviors, brains) = {
            let t = self.tower.read(cx);
            (
                t.alive,
                model::tell_voice(t.state.as_ref().unwrap_or(&serde_json::Value::Null)),
                t.voice_on_mac(),
                talk_key_line(t.s("talk_key").as_str().unwrap_or("")),
                t.state.as_ref().and_then(|s| model::tell_level(s)).map(String::from),
                t.s("question_policy").as_str().unwrap_or("normal").to_string(),
                t.s("behaviors").clone(),
                t.state.as_ref().filter(|_| t.alive).and_then(model::brain_model),
            )
        };
        // What was just chosen here, until the state says the same.
        if let Some(full) = self.full.as_mut()
            && full.tell.is_some()
            && full.tell == said_level
        {
            full.tell = None;
        }
        if self.asks.policy.as_deref() == Some(said_policy.as_str()) {
            self.asks.policy = None;
        }
        if let Some(full) = self.full.as_mut()
            && full.voice.as_deref() == Some(said_voice)
        {
            full.voice = None;
        }
        let voice_now = self.full.as_ref().and_then(|f| f.voice.clone()).unwrap_or_else(|| said_voice.to_string());
        let level = self.full.as_ref().and_then(|f| f.tell.clone()).or(said_level);
        let policy = self.asks.policy.clone().unwrap_or(said_policy);
        let seg = || h_flex().flex_wrap().gap(px(6.0));

        // How much it tells you.
        let tell = card(
            "How much it tells you",
            vec![
                seg()
                    .children([("decisions", "Decisions"), ("results", "Decisions and results"), ("everything", "Everything")].map(|(l, text)| {
                        tg(SharedString::from(format!("tell-{l}")), text, level.as_deref() == Some(l)).on_click(cx.listener(move |this, _, _, cx| this.set_tell(l, cx)))
                    }))
                    .into_any_element(),
                para(tell_line(level.as_deref())),
            ]
            .into_iter()
            .chain(self.said("tell"))
            .collect(),
        );

        // The model the tower's brains run on.
        let brain = match brains {
            None => card("Tower’s model", vec![para(if alive { "This tower doesn’t say what its brain runs on." } else { "The tower is off: its brain’s model shows here once it runs." })]),
            Some(m) => {
                // What was just chosen here, until the state says the same.
                if let Some(full) = self.full.as_mut()
                    && let Some((model, effort)) = &full.model
                    && model.as_deref().is_none_or(|x| x == m.model)
                    && effort.as_deref().is_none_or(|x| x == m.effort)
                    && !full.busy.contains("model")
                {
                    full.model = None;
                }
                let chosen = self.full.as_ref().and_then(|f| f.model.clone()).unwrap_or_default();
                let (model_now, effort_now) = (chosen.0.unwrap_or_else(|| m.model.clone()), chosen.1.unwrap_or_else(|| m.effort.clone()));
                let busy = self.running("model");
                let mut items = vec![
                    seg()
                        .children(m.models.iter().map(|c| {
                            let id = c.id.clone();
                            tg(SharedString::from(format!("model-{}", c.id)), &c.label, model_now == c.id).on_click(cx.listener(move |this, _, _, cx| this.set_model(Some(id.clone()), None, cx)))
                        }))
                        .into_any_element(),
                    seg()
                        .children(m.efforts.iter().map(|e| {
                            let level = e.clone();
                            tg(SharedString::from(format!("effort-{e}")), e, effort_now == *e).on_click(cx.listener(move |this, _, _, cx| this.set_model(None, Some(level.clone()), cx)))
                        }))
                        .into_any_element(),
                    para(model_line(&m)),
                ];
                if m.restart_needed || busy {
                    items.push(
                        acts_row()
                            .child(mbtn("model-restart", if busy { "Working…" } else { "Restart the tower’s brain" }, true, None).when(busy, |el| el.opacity(0.5)).when(!busy, |el| el.on_click(cx.listener(|this, _, _, cx| this.restart_brains(cx)))))
                            .child(div().min_w_0().text_size(px(12.0)).text_color(g.faint).child("It starts a new conversation."))
                            .into_any_element(),
                    );
                }
                items.extend(self.said("model"));
                card("Tower’s model", items)
            }
        };

        // Voice: what it says out loud, and (a switch of its own) where.
        let voice = card(
            "Voice",
            if alive {
                vec![
                    seg()
                        .children([("off", "Off"), ("decisions", "Decisions"), ("replies", "Replies and decisions")].map(|(v, text)| {
                            tg(SharedString::from(format!("voice-{v}")), text, voice_now == v).on_click(cx.listener(move |this, _, _, cx| this.set_voice(v, cx)))
                        }))
                        .into_any_element(),
                    para(format!("{} {talk_key}", voice_line(&voice_now))),
                    seg().child(tg("voice-mac", "Always on this Mac", on_mac).on_click(cx.listener(|_, _, _, cx| crate::orb::toggle_mac_voice(cx)))).into_any_element(),
                    para(if on_mac {
                        "Everything it says plays here, even while a phone is the voice."
                    } else {
                        "A phone that is the voice plays it, and hears its own replies; this Mac otherwise."
                    }),
                ]
                .into_iter()
                .chain(self.said("voice"))
                .collect()
            } else {
                vec![para("The tower is off: nothing is said.")]
            },
        );

        // Questions it answers for you.
        let questions = card(
            "Questions it answers for you",
            vec![
                seg()
                    .children([("normal", "Normal"), ("keep-moving", "Keep moving"), ("ask-everything", "Ask me everything")].map(|(p, text)| {
                        tg(SharedString::from(format!("policy-{p}")), text, policy == p).on_click(cx.listener(move |this, _, _, cx| this.set_policy(p, cx)))
                    }))
                    .into_any_element(),
                para(format!("{} Deploys, deletes, production data, secrets and money always come to you.", policy_line(&policy))),
            ]
            .into_iter()
            .chain(self.said("policy"))
            .collect(),
        );

        // Follow and Mute, per open space.
        let mut marks: Vec<AnyElement> = vec![];
        for (i, m) in missions.open.iter().enumerate() {
            let (n1, n2, follow, mute) = (m.name.clone(), m.name.clone(), m.follow, m.mute);
            marks.push(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(div().flex_1().min_w_0().truncate().text_size(px(13.0)).child(m.name.clone()))
                    .child(tg(SharedString::from(format!("set-follow-{i}")), "Follow", follow).on_click(cx.listener(move |this, _, _, cx| this.set_switch(&n1, true, !follow, cx))))
                    .child(tg(SharedString::from(format!("set-mute-{i}")), "Mute", mute).on_click(cx.listener(move |this, _, _, cx| this.set_switch(&n2, false, !mute, cx))))
                    .into_any_element(),
            );
            marks.extend(self.said(&format!("space:{}", m.name)));
        }
        if marks.is_empty() {
            marks.push(para("No space is open."));
        }
        marks.push(para("Follow: its results reach you as they land. Mute: its decisions stay in its own view."));
        let follow = card("Follow and Mute", marks);

        // Behaviors, and the phone.
        let rules = match (behaviors["enabled"].as_u64(), behaviors["count"].as_u64()) {
            (Some(on), Some(n)) => format!("The rules that say how the tower works: {on} of {n} are on. Read, switch or rewrite any of them."),
            _ => "The rules that say how the tower works. Read, switch or rewrite any of them.".to_string(),
        };
        let behaviors = card(
            "Behaviors",
            vec![
                para(rules),
                acts_row().child(mbtn("open-behaviors", "Open behaviors", false, None).on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::Behaviors, cx)))).into_any_element(),
            ],
        );
        // The phone: as wide as the pane, and where `Scope::Phone` scrolls to.
        let anchor = self.full.as_ref().map(|f| f.phone_anchor.clone());
        let phone = div().id("phone-card").w_full().anchor_scroll(anchor).child(self.phone_card(cx)).into_any_element();

        // Accounts, and the setups the launcher offers.
        let accounts = card(
            "Accounts",
            vec![para(match self.accounts.as_deref() {
                None => "Reading…".to_string(),
                Some([]) => "No accounts mod here: every session runs on this Mac’s own login.".to_string(),
                Some([one]) => format!("{one}, read from the accounts mod."),
                Some(list) => format!("{}, read from the accounts mod. A session uses {} unless a setup or your request names another.", listed(list), list[0]),
            })],
        );
        // The projects New space offers: their names here, the rest on their page.
        let list = self.full.as_ref().and_then(|f| f.launch.setups()).map(|s| s.to_vec());
        let mut saved: Vec<AnyElement> = vec![para(match list.as_deref() {
            None => "Asking the tower…".to_string(),
            Some([]) => "None yet. A project is its repos and a line on what each is for: New space reads them to draft a space from a ticket.".to_string(),
            Some(list) => {
                let names: Vec<String> = list.iter().map(|s| s.name.clone()).collect();
                format!("{}. New space offers them, and reads what each repo is for to draft a space from a ticket.", listed(&names))
            }
        })];
        saved.push(acts_row().child(mbtn("open-projects", "Open projects", false, None).on_click(cx.listener(|this, _, _, cx| this.nav_to(Scope::Projects, cx)))).into_any_element());
        saved.extend(self.said("setups"));
        let setups = card("Projects", saved);

        // Two by two when the pane has room, else one under another.
        let cols = if self.full.as_ref().map(|f| f.pane_w).unwrap_or(900.0) >= super::window::TWO_COLS_W { 2 } else { 1 };
        let grid = |cards: Vec<AnyElement>| div().w_full().grid().grid_cols(cols).gap(px(12.0)).children(cards).into_any_element();
        (head, vec![v_flex().w_full().gap(px(12.0)).child(grid(vec![tell, brain, voice, questions, follow, behaviors])).child(phone).child(grid(vec![accounts, setups])).into_any_element()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_choice_says_what_it_does() {
        assert!(tell_line(Some("decisions")).starts_with("Only what is yours"));
        assert!(tell_line(Some("results")).contains("receipt"));
        assert!(tell_line(Some("everything")).contains("Every event"));
        // A tower that doesn't say its level: none is marked, and the line says what choosing does.
        assert!(tell_line(None).starts_with("Choose"));
        assert_eq!((talk_key_line("ctrl"), talk_key_line("off"), talk_key_line("right_ctrl")), ("Talk key: Control.", "The talk key is off.", "Talk key: right Control."));
        assert_ne!(policy_line("normal"), policy_line("keep-moving"));
        assert_ne!(policy_line("normal"), policy_line("ask-everything"));
        assert_eq!(policy_line("anything else"), policy_line("normal"));
    }

    #[test]
    fn the_models_card_says_what_runs_and_when_a_choice_applies() {
        let said = |cfg: &crate::config::Config, running: &[crate::brains::Running]| model_line(&model::brain_model(&serde_json::json!({ "brain_model": crate::brains::report(cfg, running) })).unwrap());
        let runs = |argv: &[&str]| crate::brains::Running { brain: "tower".into(), space: None, address: "t7".into(), argv: argv.iter().map(|s| s.to_string()).collect() };
        let cfg = crate::config::Config::default();
        // Started on the setting: what it runs on, and that a change waits for a restart.
        assert_eq!(
            said(&cfg, &[runs(&["claude", "-n", "tower", "--model", "claude-opus-5-5", "--effort", "high"])]),
            "The tower's brain runs on Opus 5.5 at high effort. A change applies when the tower’s brain restarts."
        );
        // Started on something else: the tower's sentence already says what applies when.
        assert_eq!(
            said(&cfg, &[runs(&["claude", "-n", "tower", "--model", "sonnet", "--effort", "medium"])]),
            "The tower's brain runs on sonnet at medium effort. Opus 5.5 at high effort applies when it restarts."
        );
        // The user's own arguments win, and a Codex brain has its own settings: said as they are, with nothing promised.
        let mut own = crate::config::Config::default();
        own.claude_args = vec!["--model".into(), "sonnet".into()];
        assert!(said(&own, &[]).ends_with("that wins over the choice here."));
        let mut codex = crate::config::Config::default();
        codex.brain = "codex".into();
        assert!(said(&codex, &[]).starts_with("The tower's brain is Codex") && !said(&codex, &[]).contains("A change applies"));
    }

    #[test]
    fn accounts_are_listed_as_a_sentence_has_them() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(listed(&names(&["default", "orca"])), "default and orca");
        assert_eq!(listed(&names(&["a", "b", "c"])), "a, b and c");
        assert_eq!((listed(&names(&["solo"])), listed(&[])), ("solo".to_string(), String::new()));
    }
}
