//! The daemon as the phone API reaches it (`lan::tower::Tower`): each call
//! is what the daemon's socket answers for the Mac's windows and the CLI
//! (`Daemon::decide`, `tell`, `model`, the missions' look), or the function
//! a `tower space …`, `tower setups` or `tower launch` command runs. A
//! phone and the Mac can't drift apart: there is one of each.

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::Daemon;
use super::decisions::Refused;
use super::lan::tower::Tower;
use crate::launch::Plan;
use crate::protocol::Response;
use crate::spaces;

/// A socket answer as a result: its `state`, or its error.
fn answered(r: Response) -> Result<Value> {
    match r.ok {
        true => Ok(r.state.unwrap_or_default()),
        false => Err(anyhow!(r.error.unwrap_or_else(|| "the tower refused it".into()))),
    }
}

impl Tower for Daemon {
    fn decisions(&self, space: Option<&str>, all: bool) -> Value {
        // A PR merged on GitHub leaves the phone's queue as soon as it looks.
        if let Some(p) = crate::prs::daemon() {
            p.look_now();
        }
        // As it stands now: a space may have been muted a moment ago.
        self.decisions.refresh();
        self.decisions.listed(space, all)
    }

    fn decision(&self, id: &str) -> Option<Value> {
        self.decisions.view(id)
    }

    fn answer(&self, id: &str, body: &Value, submit: bool) -> Result<Value, Refused> {
        self.decide(id, body, submit)
    }

    fn dismiss(&self, id: &str) -> Result<Value, Refused> {
        self.decisions.dismiss_with(id, &self.asks)
    }

    fn missions(&self) -> Value {
        self.missions.now()
    }

    fn spaces(&self) -> Vec<String> {
        spaces::known(&spaces::live(), &crate::history::load(), &spaces::load())
    }

    fn remembered(&self) -> Vec<String> {
        let s = spaces::load();
        let mut names: Vec<String> = s.missions.keys().cloned().collect();
        names.extend(s.agendas.keys().filter(|n| !s.missions.contains_key(*n)).cloned());
        names
    }

    fn goal(&self, space: &str, goal: Option<&str>, done_when: Option<&str>, standing: Option<bool>) -> Result<Value> {
        let name = spaces::pick(space)?;
        let now = crate::history::now_ms();
        // One writer at a time: the missions' own look keeps the same file.
        Ok(spaces::update(|s| {
            let v = crate::missions::goal_in(s, &name, goal, done_when, now);
            match standing {
                Some(on) => crate::missions::standing_in(s, &name, on, now),
                None => v,
            }
        }))
    }

    fn follow(&self, space: &str, on: bool) -> Result<()> {
        let name = spaces::pick(space)?;
        spaces::update(|s| spaces::set_follow(s, &name, on));
        Ok(())
    }

    fn mute(&self, space: &str, on: bool) -> Result<()> {
        let name = spaces::pick(space)?;
        spaces::update(|s| spaces::set_mute(s, &name, on));
        // The queue and the orb's count follow at once.
        self.decisions.refresh();
        Ok(())
    }

    fn holding(&self, space: &str) -> Result<Vec<Value>> {
        crate::missions::holding(space)
    }

    fn closing(&self, space: &str) -> Result<Value> {
        crate::missions::preview(space)
    }

    fn resolve(&self, space: &str, force: bool) -> Result<Value> {
        let v = crate::missions::resolve_named(space, force, false)?;
        if let Some(name) = v["closed"].as_str() {
            self.decisions.space_over(name, true);
        }
        self.missions.now();
        Ok(v)
    }

    fn close(&self, space: &str, force: bool) -> Result<Value> {
        // The voice itself closes it: nobody else is asked to look.
        let v = crate::missions::close_named(space, force, false)?;
        if let Some(name) = v["closed"].as_str() {
            self.decisions.space_over(name, true);
        }
        // The missions in the state say so at once.
        self.missions.now();
        Ok(v)
    }

    fn setups(&self) -> Result<Value> {
        crate::setups::value()
    }

    fn plan(&self, goal: &str, setup: Option<&str>, name: Option<&str>) -> Result<Plan> {
        crate::launch::planned(goal, setup, name)
    }

    fn launch(&self, plan: &Plan, by: &str, each: &mut dyn FnMut(&Value)) -> Result<Value> {
        let r = crate::launch::open(plan, Some(by), &mut |so_far| {
            // Its space shows as its sessions open.
            super::missions::kick();
            each(so_far)
        });
        super::missions::kick();
        r
    }

    fn tell(&self, level: Option<&str>, voice: Option<&str>) -> Result<Value> {
        answered(Daemon::tell(self, level, voice))
    }

    fn model(&self, model: Option<&str>, effort: Option<&str>, restart: bool) -> Result<Value> {
        answered(Daemon::model(self, model, effort, restart))
    }

    fn acts(&self) -> Value {
        self.acts.list()
    }

    fn act(&self, id: &str) -> Option<Value> {
        self.acts.view(id)
    }

    fn undo(&self, id: &str, force: bool) -> Result<Value, crate::acts::Refused> {
        Daemon::undo(self, Some(id), force, None)
    }

    fn rule(&self, id: &str, answer: Option<&str>) -> Result<(String, Option<String>), Refused> {
        self.decisions.rule_of(id, answer)
    }

    fn ask(&self, text: &str, phone: Option<String>, source: &str, space: Option<&str>) -> Result<()> {
        super::timing::start("asked");
        super::stt::deliver_to(&self.shared, &self.mouth, text, super::ack_delay(&self.cfg), phone, source, space, false)
    }
}
