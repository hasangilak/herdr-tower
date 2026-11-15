//! Risk and confidence for the tower's recommendations, so it picks up the
//! low-risk questions itself and asks the user less.
//!
//! - **Risk** (low, medium, high), with a why: how far an answer reaches
//!   (blast radius), whether it can be undone, whether it leaves the
//!   session's project folder or touches shared or remote state. The brain
//!   says it (`tower recommend --risk`); the tower reads its own **floor**
//!   from the question and the command (`floor`): the hard escalate list is
//!   high, whatever the brain says, and the brain can only raise a floor.
//! - **Confidence** (0 to 1), with a why: how sure the tower is that its
//!   first pick is what the user would choose. The brain says it
//!   (`--confidence`); the user's own answers move it (`Learned`): each card
//!   of the same kind (the same space, the same kind of question) answered
//!   the way the tower recommended raises it, each one overridden lowers it.
//! - **The decision** (`decide`): high risk always goes to the user; low risk
//!   is answered for them at `low` confidence or more; medium at `medium` or
//!   more (`[scoring]` in config.toml, Settings). The hard escalate list is
//!   applied before any of this (`questions::must_escalate`), as strict as
//!   ever.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How far an answer reaches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    #[default]
    Low,
    Medium,
    High,
}

impl Risk {
    pub fn parse(s: &str) -> Option<Risk> {
        match s.trim().to_lowercase().as_str() {
            "low" | "l" => Some(Risk::Low),
            "medium" | "med" | "m" => Some(Risk::Medium),
            "high" | "h" => Some(Risk::High),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

/// The confidence the tower must have to answer for the user, by risk
/// (`[scoring]` in config.toml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    /// Low risk is answered for the user from this confidence on.
    pub low: f64,
    /// Medium risk, from this one.
    pub medium: f64,
    /// The user's answers move the confidence (`Learned`).
    pub learn: bool,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds { low: 0.6, medium: 0.85, learn: true }
    }
}

impl Thresholds {
    /// The named presets Settings offers: fewer prompts, balanced (the default), careful.
    pub fn preset(name: &str) -> Option<Thresholds> {
        match name {
            "fewer" => Some(Thresholds { low: 0.5, medium: 0.75, learn: true }),
            "balanced" => Some(Thresholds::default()),
            "careful" => Some(Thresholds { low: 0.75, medium: 0.95, learn: true }),
            _ => None,
        }
    }

    /// Which preset these are, if one.
    pub fn preset_name(&self) -> Option<&'static str> {
        ["fewer", "balanced", "careful"].into_iter().find(|n| Thresholds::preset(n).is_some_and(|p| (p.low - self.low).abs() < 1e-9 && (p.medium - self.medium).abs() < 1e-9))
    }
}

/// The scores a recommendation carries, and what they decided: on the
/// question set, its card, and the "answered for you" entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scores {
    /// The risk that counts: the brain's, or the tower's floor when that is higher.
    pub risk: Risk,
    pub risk_why: String,
    /// The tower's own floor, from the question and the command.
    pub floor: Risk,
    pub floor_why: String,
    /// The confidence that counts: the brain's, moved by the user's answers.
    pub confidence: f64,
    pub confidence_why: String,
    /// The brain's own, as it said it (None: it gave none: decided as before scores).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brain_confidence: Option<f64>,
    /// `auto` or `escalate`, and why, by the scores.
    pub decided: String,
    pub decided_why: String,
    /// The kind of question the user's answers are kept by (`space|kind`).
    pub kind: String,
    /// One line for a card: "risk low · confidence 0.82".
    pub line: String,
}

/// The kind of a question, for learning: what is asked, not its words.
/// A permission: its tool and program ("bash:npm", "edit"); anything else:
/// its kind and its question's first words ("claude-ask:which database should").
pub fn kind_of(kind: &str, target: &str, question: &str) -> String {
    if !target.trim().is_empty() {
        let (tool, rest) = target.split_once(':').unwrap_or((target, ""));
        let tool = tool.trim().to_lowercase();
        if tool == "bash" {
            let cmd = crate::running::name_of(rest.trim());
            let program = cmd.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("").to_lowercase();
            // `npm test` and `npm install` are different kinds; `git status` and `git push` too.
            let sub = cmd.split_whitespace().nth(1).filter(|w| !w.starts_with('-')).map(|w| w.to_lowercase()).filter(|_| matches!(program.as_str(), "npm" | "pnpm" | "yarn" | "bun" | "cargo" | "git" | "gh" | "docker" | "make" | "uv" | "pip" | "python" | "python3" | "node"));
            return match sub {
                Some(s) => format!("bash:{program} {}", s.chars().take(24).collect::<String>()),
                None => format!("bash:{program}"),
            };
        }
        return match tool.as_str() {
            "update" | "edit" | "write" | "multiedit" => "edit".into(),
            t => t.to_string(),
        };
    }
    let words: Vec<String> = question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(4)
        .map(String::from)
        .collect();
    format!("{kind}:{}", words.join(" "))
}

/// The tower's own risk floor for a question: the hard escalate list is
/// high (`escalate`: its reason); then what reaches beyond the session's
/// project is medium (`medium_signal`); the rest is low.
pub fn floor(escalate: Option<&str>, text: &str, command: Option<&str>, cwd: Option<&str>) -> (Risk, String) {
    if let Some(why) = escalate {
        return (Risk::High, why.to_string());
    }
    if let Some(why) = medium_signal(text, command, cwd) {
        return (Risk::Medium, why);
    }
    (Risk::Low, "stays in its project and can be undone".into())
}

/// What makes a question medium risk: it reaches beyond the project folder,
/// touches shared, remote or machine-wide state, or is hard to undo.
fn medium_signal(text: &str, command: Option<&str>, cwd: Option<&str>) -> Option<String> {
    let c = command.unwrap_or("").to_lowercase();
    let t = format!(" {} ", text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" "));
    let cmd = format!(" {} ", c.split_whitespace().collect::<Vec<_>>().join(" "));
    let has = |s: &str, words: &[&str]| words.iter().find(|w| s.contains(*w)).map(|w| w.trim().to_string());
    // Remote: the network, another machine, a hosted service.
    if let Some(w) = has(&cmd, &[" curl ", " wget ", " ssh ", " scp ", " rsync ", " gh api ", " gh pr create", " gh issue ", " vercel ", " aws ", " gcloud ", " az ", " kubectl ", " terraform ", " fly ", " heroku ", " stripe ", " supabase "]) {
        return Some(format!("it reaches a remote service ({w})"));
    }
    // Machine-wide or shared state: installs, global config, services, databases.
    if let Some(w) = has(&cmd, &[" brew ", " npm install -g", " npm i -g", " pip install", " pipx ", " cargo install", " launchctl ", " defaults write", " crontab ", " git config --global", " docker ", " psql ", " mysql ", " redis-cli ", " mongosh ", " sqlite3 "]) {
        return Some(format!("it changes shared or machine-wide state ({w})"));
    }
    // A file outside the project folder.
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        let outside = c.split_whitespace().chain(t.split_whitespace()).any(|w| {
            let w = w.trim_matches(|ch: char| ch == '"' || ch == '\'' || ch == ',' || ch == ')' || ch == '(');
            (w.starts_with('/') || w.starts_with("~/")) && !w.starts_with(cwd) && !w.starts_with("/tmp") && !w.starts_with("/private/tmp") && !w.starts_with("/dev/") && w.len() > 2
        });
        if outside {
            return Some("it reaches outside its project folder".into());
        }
    }
    // Hard to undo: overwrites and resets that a commit doesn't keep.
    if let Some(w) = has(&cmd, &[" rm ", " mv ", " reset --hard", " checkout -- ", " restore ", " clean -", " truncate ", " dd ", "> /"]) {
        return Some(format!("it is hard to undo ({w})"));
    }
    // In words: a question about shared or remote things.
    if let Some(w) = has(&t, &[" database ", " schema ", " api key", " shared ", " other team", " customers ", " users' ", " billing ", " dns ", " email them", " send an email", " slack "]) {
        return Some(format!("it touches shared state ({w})"));
    }
    None
}

/// The user's answers on cards, by kind (`space|kind`): agreed with the
/// tower's first pick, or overrode it (`learning.json` in the state dir).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Learned {
    pub kinds: BTreeMap<String, Kind>,
}

/// One kind's record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kind {
    pub agreed: u32,
    pub overridden: u32,
    /// The latest, newest last (at most `KEEP`).
    pub last: Vec<Seen>,
}

/// One answer the user gave on a card of that kind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Seen {
    pub at_ms: u64,
    pub agreed: bool,
    /// The question, cut short.
    pub question: String,
    /// What the tower recommended, and what the user chose.
    pub recommended: String,
    pub chose: String,
}

/// Answers kept per kind.
const KEEP: usize = 20;
/// How many answers the brain's own confidence weighs (a prior of this many).
const PRIOR: f64 = 4.0;

impl Learned {
    /// The user answered a card of `kind` (`agreed`: the tower's first pick).
    pub fn learn(&mut self, kind: &str, seen: Seen) {
        let k = self.kinds.entry(kind.to_string()).or_default();
        if seen.agreed {
            k.agreed += 1;
        } else {
            k.overridden += 1;
        }
        k.last.push(seen);
        if k.last.len() > KEEP {
            let n = k.last.len() - KEEP;
            k.last.drain(..n);
        }
    }

    /// The confidence that counts for `kind`: the brain's (`said`), moved by
    /// the user's answers to that kind (a weighted mean: the brain's counts as
    /// `PRIOR` answers), and why.
    pub fn confidence(&self, kind: &str, said: f64, learn: bool) -> (f64, String) {
        let said = said.clamp(0.0, 1.0);
        let k = self.kinds.get(kind).filter(|_| learn);
        match k {
            Some(k) if k.agreed + k.overridden > 0 => {
                let (a, o) = (k.agreed as f64, k.overridden as f64);
                let c = (said * PRIOR + a) / (PRIOR + a + o);
                (c, format!("the brain said {said:.2}; you chose its pick {} of {} times for this kind", k.agreed, k.agreed + k.overridden))
            }
            _ => (said, format!("the brain said {said:.2}; no answers of yours for this kind yet")),
        }
    }
}

/// Whether to answer for the user, from the risk and the confidence that
/// count, and why. High risk never is.
pub fn decide(risk: Risk, confidence: f64, t: &Thresholds) -> (bool, String) {
    match risk {
        Risk::High => (false, "high risk: always yours".into()),
        Risk::Low if confidence >= t.low => (true, format!("low risk, confidence {confidence:.2} (answered from {:.2})", t.low)),
        Risk::Low => (false, format!("low risk, but confidence {confidence:.2} is under {:.2}", t.low)),
        Risk::Medium if confidence >= t.medium => (true, format!("medium risk, confidence {confidence:.2} (answered from {:.2})", t.medium)),
        Risk::Medium => (false, format!("medium risk, confidence {confidence:.2} is under {:.2}", t.medium)),
    }
}

/// "risk low · confidence 0.82".
pub fn line(risk: Risk, confidence: Option<f64>) -> String {
    match confidence {
        Some(c) => format!("risk {} · confidence {c:.2}", risk.word()),
        None => format!("risk {}", risk.word()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_floor_is_high_for_the_escalate_list_and_reads_what_reaches_beyond_the_project() {
        assert_eq!(floor(Some("it pushes or merges to a shared branch"), "", Some("git push origin main"), None).0, Risk::High);
        assert_eq!(floor(None, "Run the tests?", Some("npm test"), Some("/w/app")), (Risk::Low, "stays in its project and can be undone".into()));
        assert_eq!(floor(None, "", Some("curl -s https://api.example.com/x"), None).0, Risk::Medium);
        assert_eq!(floor(None, "", Some("brew install jq"), None).0, Risk::Medium);
        assert_eq!(floor(None, "", Some("rm build/out.js"), None).0, Risk::Medium);
        assert_eq!(floor(None, "Edit /Users/me/.zshrc?", Some("cat >> /Users/me/.zshrc"), Some("/w/app")).1, "it reaches outside its project folder");
        assert_eq!(floor(None, "Write /w/app/src/x.ts?", None, Some("/w/app")).0, Risk::Low, "inside the project");
        assert_eq!(floor(None, "Which database should the app use?", None, None).0, Risk::Medium);
        assert_eq!(floor(None, "Read /tmp/out.log?", Some("cat /tmp/out.log"), Some("/w/app")).0, Risk::Low, "scratch is no reach");
    }

    #[test]
    fn high_risk_is_always_the_users_and_low_and_medium_go_by_threshold() {
        let t = Thresholds::default();
        assert!(!decide(Risk::High, 1.0, &t).0);
        assert!(decide(Risk::Low, 0.6, &t).0 && !decide(Risk::Low, 0.59, &t).0);
        assert!(decide(Risk::Medium, 0.85, &t).0 && !decide(Risk::Medium, 0.8, &t).0);
        let careful = Thresholds::preset("careful").unwrap();
        assert!(!decide(Risk::Low, 0.7, &careful).0);
        assert_eq!((Thresholds::default().preset_name(), careful.preset_name()), (Some("balanced"), Some("careful")));
    }

    #[test]
    fn the_users_answers_move_the_confidence_for_that_kind_only() {
        let mut l = Learned::default();
        let seen = |agreed| Seen { at_ms: 1, agreed, question: "Run npm test?".into(), recommended: "Yes".into(), chose: if agreed { "Yes".into() } else { "No".into() } };
        assert_eq!(l.confidence("app|bash:npm test", 0.5, true).0, 0.5);
        for _ in 0..4 {
            l.learn("app|bash:npm test", seen(true));
        }
        let (up, why) = l.confidence("app|bash:npm test", 0.5, true);
        assert!((up - 0.75).abs() < 1e-9, "{up}");
        assert!(why.contains("4 of 4"), "{why}");
        // Overridden twice: it drops.
        l.learn("app|bash:npm test", seen(false));
        l.learn("app|bash:npm test", seen(false));
        assert!(l.confidence("app|bash:npm test", 0.5, true).0 < up);
        // Another kind, or learning off: untouched.
        assert_eq!(l.confidence("app|bash:git push", 0.5, true).0, 0.5);
        assert_eq!(l.confidence("app|bash:npm test", 0.5, false).0, 0.5);
        // Only the latest are kept.
        for _ in 0..30 {
            l.learn("x", seen(true));
        }
        assert_eq!((l.kinds["x"].last.len(), l.kinds["x"].agreed), (20, 30));
    }

    #[test]
    fn a_kind_is_the_tool_and_program_or_the_questions_first_words() {
        assert_eq!(kind_of("claude-permission", "Bash: npm test -- --watch=false", ""), "bash:npm test");
        assert_eq!(kind_of("claude-permission", "Bash: cd /w/app && cargo build --release", ""), "bash:cargo build");
        assert_eq!(kind_of("claude-permission", "Bash: ls -la", ""), "bash:ls");
        assert_eq!(kind_of("claude-permission", "Update: /w/app/src/x.ts", ""), "edit");
        assert_eq!(kind_of("claude-ask", "", "Which database should the app use?"), "claude-ask:which database should the");
        assert_eq!(Risk::parse("Medium"), Some(Risk::Medium));
        assert_eq!(line(Risk::Low, Some(0.8234)), "risk low · confidence 0.82");
    }
}
