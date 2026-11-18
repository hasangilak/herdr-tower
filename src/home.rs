//! The space a command opens its sessions in when none is named: the space
//! of whoever runs it, never by accident the one the window happens to show.
//!
//! - **A space's own tower** (`tower-<space>`): its space. `tower start`,
//!   `tower dispatch` and `tower launch` it runs open their sessions there,
//!   whatever the window shows. A space it names (`--space`, a task's
//!   `space`, `--name`) is still taken as named, and a finding spun out
//!   (`tower finding … --off-goal`) still opens a new space of its own: it
//!   names it.
//! - **A session** in a space: that space.
//! - **The main tower**: the space its request named in front ("In
//!   support: …", as a request asked in a space with no tower of its own
//!   comes to it), when the request it answers now has one.
//! - **Anyone else** (a shell, the panel), or the main tower on a request
//!   that names no space: none, and the session goes to the space the
//!   window shows, as before.

use serde_json::Value;

/// Who runs a command, as far as its space goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A space's own tower: its terminal's name and its space.
    SpaceTower { name: String, space: String },
    /// The main tower (`tower`).
    MainTower,
    /// A session (not a tower), and the space it is in.
    Session { name: String, space: Option<String> },
    /// A shell, the panel, the phone, the daemon's own terminal: no one with a space.
    Other,
}

/// The space a command opens its sessions in when none is named, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    pub space: String,
    pub why: String,
}

/// The terminal `t` (as `hc get` has it) as a caller.
pub fn caller_of(t: &Value) -> Caller {
    let name = t["name"].as_str().unwrap_or("");
    if let Some(brain) = crate::brain::brain_of(t) {
        if crate::brains::is_space_brain(brain) {
            if !crate::brains::is_ours(brain, t) {
                return Caller::Other;
            }
            let space = crate::brains::card_of(brain)
                .map(|c| c.space)
                .or_else(|| t["labels"][crate::brains::LABEL_SPACE].as_str().map(String::from))
                .filter(|s| !s.trim().is_empty());
            return match space {
                Some(space) => Caller::SpaceTower { name: brain.to_string(), space },
                None => Caller::Other,
            };
        }
        if brain == crate::brain::NAME || brain == crate::brain::name() {
            return Caller::MainTower;
        }
        return Caller::Other;
    }
    if name == crate::brain::VOICE_NAME || crate::hc::agent_app(t).is_none() {
        return Caller::Other;
    }
    Caller::Session { name: crate::hc::spoken_name(t), space: crate::spaces::space_of(t) }
}

/// Who runs this command: the Herdr terminal it runs in (`HC_TERMINAL_ID`).
pub fn caller() -> Caller {
    match crate::peers::caller().and_then(|id| crate::hc::get(&id)) {
        Some(t) => caller_of(&t),
        None => Caller::Other,
    }
}

/// The space `caller` opens sessions in when it names none. `request`: what
/// the main tower answers now (its latest request), `spaces`: the spaces
/// there are, to read the one a request names in front.
pub fn home_of(caller: &Caller, request: Option<&str>, spaces: &[String]) -> Option<Home> {
    match caller {
        Caller::SpaceTower { name, space } => Some(Home { space: space.clone(), why: format!("its tower's own ({name} opened it): none was named") }),
        Caller::Session { name, space: Some(space) } => Some(Home { space: space.clone(), why: format!("the space of the session that opened it ({name}): none was named") }),
        Caller::MainTower => request
            .and_then(|r| crate::chat::asked_in(r, spaces))
            .map(|space| Home { space, why: "the one the request named: none was named on the command".into() }),
        Caller::Session { space: None, .. } | Caller::Other => None,
    }
}

/// The space a command run now opens sessions in when it names none (None:
/// the one the window shows). `spaces`: the spaces there are.
pub fn home(spaces: &[String]) -> Option<Home> {
    let caller = caller();
    let request = match caller {
        Caller::MainTower => crate::acts::request_for(&crate::history::load_tail(1 << 20), None, crate::history::now_ms()).map(|a| a.text),
        _ => None,
    };
    home_of(&caller, request.as_deref(), spaces)
}

/// The space a space's own tower launches into when it names none (None:
/// a new space, named from the goal, as for anyone else).
pub fn launch_home(caller: &Caller) -> Option<String> {
    match caller {
        Caller::SpaceTower { space, .. } => Some(space.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spaces() -> Vec<String> {
        ["search-llm", "support", "M5 Pro", "b2c support"].map(String::from).to_vec()
    }

    #[test]
    fn a_spaces_tower_opens_in_its_own_space() {
        let tower = Caller::SpaceTower { name: "tower-support".into(), space: "support".into() };
        let h = home_of(&tower, None, &spaces()).expect("its own space");
        assert_eq!(h.space, "support");
        assert!(h.why.contains("tower-support"), "{}", h.why);
        // What the main tower was asked last doesn't count for a space's tower.
        assert_eq!(home_of(&tower, Some("In search-llm: open a session"), &spaces()).map(|h| h.space).as_deref(), Some("support"));
        // Its launch joins its space.
        assert_eq!(launch_home(&tower).as_deref(), Some("support"));
    }

    #[test]
    fn the_main_tower_keeps_the_window_unless_its_request_names_a_space() {
        // As today: none, so the window's space.
        assert_eq!(home_of(&Caller::MainTower, None, &spaces()), None);
        assert_eq!(home_of(&Caller::MainTower, Some("open a session for the login bug"), &spaces()), None);
        // A request asked in a space with no tower of its own comes with the space in front.
        assert_eq!(home_of(&Caller::MainTower, Some("In b2c support: open a session for the refunds"), &spaces()).map(|h| h.space).as_deref(), Some("b2c support"));
        // Its launch makes a space, as before.
        assert_eq!(launch_home(&Caller::MainTower), None);
        assert_eq!(home_of(&Caller::Other, Some("In support: x"), &spaces()), None);
        assert_eq!(launch_home(&Caller::Other), None);
    }

    #[test]
    fn a_session_opens_in_its_own_space() {
        let s = Caller::Session { name: "fable".into(), space: Some("search-llm".into()) };
        assert_eq!(home_of(&s, None, &spaces()).map(|h| h.space).as_deref(), Some("search-llm"));
        assert_eq!(home_of(&Caller::Session { name: "x".into(), space: None }, None, &spaces()), None);
        assert_eq!(launch_home(&s), None, "a session's launch makes a space, as before");
    }

    #[test]
    fn who_runs_it_by_its_terminal() {
        // The main tower, by its name.
        assert_eq!(caller_of(&json!({ "name": crate::brain::NAME, "agent": "claude" })), Caller::MainTower);
        // A space's tower started from elsewhere (not this tower's folders) is no one's.
        assert_eq!(caller_of(&json!({ "name": "tower-support", "cwd": "/tmp/elsewhere", "labels": { "tower.space": "support" } })), Caller::Other);
        // The daemon's own terminal and a plain shell have no space.
        assert_eq!(caller_of(&json!({ "name": crate::brain::VOICE_NAME })), Caller::Other);
        assert_eq!(caller_of(&json!({ "name": "zsh", "labels": { "where": "support / zsh" } })), Caller::Other);
        // A space's tower this tower runs: its space from its label (no folder card for this one).
        let dir = crate::config::brains_dir().join("zz-home-test-space");
        let t = json!({ "name": "tower-zz-home-test-space", "cwd": dir.to_string_lossy(), "labels": { "tower.space": "Support" } });
        assert_eq!(caller_of(&t), Caller::SpaceTower { name: "tower-zz-home-test-space".into(), space: "Support".into() });
        // A session in a space's tab (named by its label only: never a tower).
        let s = json!({ "labels": { "tower.name": "tower-fix", "where": "support / tower-fix" }, "status": { "app": "claude" } });
        assert_eq!(caller_of(&s), Caller::Session { name: crate::hc::spoken_name(&s), space: Some("support".into()) });
    }
}
