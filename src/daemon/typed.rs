//! What the tower types into a session, kept a little while, so that a
//! prompt a session's hook reports (`UserPromptSubmit`) can be told apart:
//! the tower's own (an instruction from the send queue, a message between
//! sessions, a reply to a question the session asked, the task it was
//! started with) or the user's, typed in the session's own terminal.
//!
//! The tower writes down each text before it types it (and when the typing
//! is over); a prompt then is the tower's when its words are those of a text
//! it typed into that terminal in the last ten minutes and no prompt took
//! yet, or when it comes within a few seconds of the tower's typing there
//! (words a session reports a little differently: wrapped as pasted, files
//! made into `[Image #1]`). The same words again later, or a prompt nothing
//! was typed for, are the user's. A prompt `hc send` brought from another
//! terminal (`<hc from="…">`) is that terminal's: a brain's, another
//! session's; and so is one that opens the way a message between sessions
//! does ("From t7h: …", `peers::sender_in`), whoever typed it in. A Claude
//! Code session's own notice that background work of its own finished
//! (`<task-notification>`) is that session's, not the user's.
//!
//! A message the send queue types into a session that is working (see
//! `sendq`, `busy_deliver_minutes`) comes back as a prompt only when that
//! turn ends, maybe hours later: its words are kept (`held`) until a prompt
//! takes them, for up to a day.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// What the tower typed is matched by its words this long (a send waits for
/// the user to pause typing, and the hook comes after the Enter).
const WORDS_MS: u64 = 10 * 60_000;
/// A prompt this soon after the tower finished typing into that terminal is
/// the tower's, whatever its words say.
const JUST_MS: u64 = 5_000;
/// A send still under way this long after it began is over (hc gives up long before).
const SENDING_MS: u64 = 90_000;
/// Texts kept, all terminals together.
const KEEP: usize = 200;
/// What the tower typed into a session while it worked is matched by its
/// words this long: its prompt comes when that turn ends.
const HELD_MS: u64 = 24 * 3_600_000;

/// One text the tower typed (or is typing) into a terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct Typed {
    pub id: u64,
    pub terminal: String,
    /// Its words, as `words` makes them for comparing.
    pub words: String,
    /// What typed it: `queue`, `reply`, ...
    pub by: String,
    pub at_ms: u64,
    /// When the typing was over (None: still going in).
    pub done_ms: Option<u64>,
    /// A prompt took it already: the same words again are someone else's.
    pub taken: bool,
    /// Typed in while its session worked: its prompt comes when that turn
    /// ends (its words are the tower's until then, up to `HELD_MS`).
    pub held: bool,
}

impl Typed {
    /// How long a prompt with its words is the tower's.
    fn window(&self) -> u64 {
        if self.held { HELD_MS } else { WORDS_MS }
    }
}

/// Whose a prompt is.
#[derive(Debug, Clone, PartialEq)]
pub enum Whose {
    /// The tower typed it (`by`: what of the tower).
    Tower(String),
    /// Another terminal handed it over with `hc send` (its address): a brain,
    /// another session; or it opens the way a message from another session does.
    Terminal(String),
    /// The session's own program: Claude Code telling it that background
    /// work of its own finished (`<task-notification>`).
    Session,
    /// Nothing the tower typed: the user, in the session's own terminal.
    User,
}

#[derive(Debug, Default)]
pub struct Log {
    next: u64,
    typed: Vec<Typed>,
    /// The latest prompt of each terminal that was the user's (when its hook said it came).
    user: HashMap<String, u64>,
}

impl Log {
    /// The tower is about to type `text` into `terminal`: its id, for `done`.
    pub fn note(&mut self, terminal: &str, text: &str, by: &str, now: u64) -> u64 {
        self.note_as(terminal, text, by, false, now)
    }

    /// `note`; `held`: typed in while the session works (its prompt comes
    /// when that turn ends). The turn it works on stays whoever's it was.
    pub fn note_as(&mut self, terminal: &str, text: &str, by: &str, held: bool, now: u64) -> u64 {
        self.next += 1;
        self.typed.retain(|t| now.saturating_sub(t.at_ms) < t.window() + SENDING_MS);
        if self.typed.len() >= KEEP {
            // The oldest goes; one still held for its turn's end only when nothing else can.
            let i = self.typed.iter().position(|t| !t.held || t.taken).unwrap_or(0);
            self.typed.remove(i);
        }
        self.typed.push(Typed { id: self.next, terminal: terminal.into(), words: words(text), by: by.into(), at_ms: now, done_ms: None, taken: false, held });
        // What the tower types next is no turn of the user's (typed in while it
        // works, it comes after the turn under way, which stays theirs).
        if !held {
            self.user.remove(terminal);
        }
        self.next
    }

    /// The typing of `id` is over: it went in (`ok`), or it didn't (forgotten: no prompt comes of it).
    pub fn done(&mut self, id: u64, ok: bool, now: u64) {
        if ok {
            if let Some(t) = self.typed.iter_mut().find(|t| t.id == id) {
                t.done_ms = Some(now);
            }
        } else {
            self.typed.retain(|t| t.id != id);
        }
    }

    /// Whose the prompt `prompt` is, which `terminal`'s hook says came at
    /// `at_ms`. `given`: the task its terminal was started with (the last
    /// word of its command line), when it was started with one. A text of
    /// the tower's that a prompt takes is not taken again.
    pub fn whose(&mut self, terminal: &str, prompt: &str, given: Option<&str>, at_ms: u64) -> Whose {
        let whose = self.classify(terminal, prompt, given, at_ms);
        match whose {
            Whose::User => {
                self.user.insert(terminal.to_string(), at_ms);
            }
            _ => {
                self.user.remove(terminal);
            }
        }
        whose
    }

    fn classify(&mut self, terminal: &str, prompt: &str, given: Option<&str>, at_ms: u64) -> Whose {
        if let Some(from) = hc_sender(prompt) {
            return Whose::Terminal(from);
        }
        if crate::introduce::is_introduction(prompt) {
            return Whose::Tower("introduction".into());
        }
        let said = words(prompt);
        if said.is_empty() {
            // Nothing in it to be anyone's request.
            return Whose::Tower("nothing".into());
        }
        if given.is_some_and(|g| same_words(&said, &words(g))) {
            return Whose::Tower("start".into());
        }
        // Its words: the oldest text of the tower's for that terminal no prompt took yet.
        if let Some(t) = self
            .typed
            .iter_mut()
            .filter(|t| t.terminal == terminal && !t.taken && at_ms.saturating_sub(t.at_ms) < t.window() && t.at_ms <= at_ms + 2_000)
            .find(|t| same_words(&said, &t.words))
        {
            t.taken = true;
            return Whose::Tower(t.by.clone());
        }
        // Claude Code's own word that background work of its session finished.
        if said.starts_with("<task-notification>") {
            return Whose::Session;
        }
        // It opens as a message from another session does, whoever typed it in.
        if let Some(from) = crate::peers::sender_in(&said) {
            return Whose::Terminal(from);
        }
        // Other words, but the tower is typing into it this moment, or just was: the
        // tower's (said differently). Not taken: its own prompt may still come.
        if let Some(t) = self.typed.iter().find(|t| {
            t.terminal == terminal
                && !t.taken
                && match t.done_ms {
                    Some(done) => at_ms + 2_000 >= done && at_ms.saturating_sub(done) <= JUST_MS,
                    None => at_ms + 2_000 >= t.at_ms && at_ms.saturating_sub(t.at_ms) <= SENDING_MS,
                }
        }) {
            return Whose::Tower(t.by.clone());
        }
        Whose::User
    }

    /// The tower typed into `terminal` within `within` ms of `now` (its prompt taken or not).
    pub fn typed_lately(&self, terminal: &str, now: u64, within: u64) -> bool {
        self.typed.iter().any(|t| t.terminal == terminal && now.saturating_sub(t.done_ms.unwrap_or(t.at_ms)) <= within)
    }

    /// The prompt `terminal`'s hook said came at `prompt_ms` was the user's own.
    pub fn users(&self, terminal: &str, prompt_ms: u64) -> bool {
        prompt_ms > 0 && self.user.get(terminal) == Some(&prompt_ms)
    }
}

/// The terminal `hc send` says a prompt came from: `<hc from="t5">…</hc>`.
fn hc_sender(prompt: &str) -> Option<String> {
    let at = prompt.find("<hc from=\"")?;
    let rest = &prompt[at + "<hc from=\"".len()..];
    let from = &rest[..rest.find('"')?];
    Some(from.to_string())
}

/// A text's words for comparing: without what delivered it (`<hc from=…>`,
/// `<pasted_content …>` and their ends), without control characters, every
/// run of whitespace one space, in lower case.
pub fn words(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let wrapper = ["<hc from=", "</hc>", "<pasted_content", "</pasted_content"].iter().any(|w| tail.starts_with(w));
        match (wrapper, tail.find('>')) {
            (true, Some(end)) => {
                out.push(' ');
                rest = &tail[end + 1..];
            }
            _ => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    crate::hc::clean(&out).split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The same words: equal, or (for a text long enough to tell) the start of
/// the shorter is in the longer one (a prompt can come with more around it,
/// or cut).
fn same_words(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.chars().count() <= b.chars().count() { (a, b) } else { (b, a) };
    if short.chars().count() < 16 {
        return false;
    }
    let head: String = short.chars().take(120).collect();
    long.contains(&head)
}

fn log() -> std::sync::MutexGuard<'static, Log> {
    static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
    LOG.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// The daemon's: the tower is about to type `text` into `terminal`.
pub fn note(terminal: &str, text: &str, by: &str) -> u64 {
    log().note(terminal, text, by, crate::history::now_ms())
}

/// `note`; `held`: into a session that works (its prompt comes when that turn ends).
pub fn note_as(terminal: &str, text: &str, by: &str, held: bool) -> u64 {
    log().note_as(terminal, text, by, held, crate::history::now_ms())
}

/// The daemon's: the typing of `id` is over.
pub fn done(id: u64, ok: bool) {
    log().done(id, ok, crate::history::now_ms());
}

/// The daemon's: whose a prompt is (see `Log::whose`).
pub fn whose(terminal: &str, prompt: &str, given: Option<&str>, at_ms: u64) -> Whose {
    log().whose(terminal, prompt, given, at_ms)
}

/// The daemon's: the tower typed into `terminal` lately.
pub fn typed_lately(terminal: &str, within: u64) -> bool {
    log().typed_lately(terminal, crate::history::now_ms(), within)
}

/// The daemon's: the session's latest prompt (as its hooks said) was the
/// user's own, typed in its terminal: its turn is the user's conversation there.
pub fn users_turn(terminal: &str) -> bool {
    let Some(s) = super::sessions::registry().get(terminal) else { return false };
    log().users(terminal, s.prompt_ms)
}

/// The task a terminal was started with: the last word of its command line,
/// when that runs an agent with one (`claude -n web "<task>"`, `cca orca -n web "<task>"`).
pub fn given_task(t: &serde_json::Value) -> Option<String> {
    let argv: Vec<&str> = t["argv"].as_array()?.iter().filter_map(serde_json::Value::as_str).collect();
    // The agent program, also behind the login shell `tower start` runs it in
    // (`/bin/zsh -lic '"$@"; exec "$0" -li' /bin/zsh …/cca default -n web "<task>"`).
    let program = argv.iter().position(|a| ["claude", "cca", "codex"].contains(&a.rsplit('/').next().unwrap_or("")))?;
    if argv.len() < program + 2 {
        return None;
    }
    let last = *argv.last()?;
    // A flag's value or a bare flag is no task.
    let before = argv.get(argv.len().wrapping_sub(2)).copied().unwrap_or("");
    if last.starts_with('-') || (before.starts_with('-') && !before.starts_with("--") && before.len() == 2) {
        return None;
    }
    Some(last.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;

    #[test]
    fn the_first_task_tower_start_gives_a_session_is_the_towers_start() {
        let mut l = Log::default();
        let task = "This is a throwaway check of the tower's session introductions. Reply with the single word ok.";
        let t = serde_json::json!({ "argv": ["/bin/zsh", "-lic", "\"$@\"; exec \"$0\" -li", "/bin/zsh", "/Users/u/.local/bin/cca", "default", "-n", "intro-check", task] });
        // Its prompt comes a while later (after a folder trust prompt): still the start.
        assert_eq!(l.whose("t8f", task, given_task(&t).as_deref(), T0 + 200_000), Whose::Tower("start".into()));
        assert!(!l.users("t8f", T0 + 200_000));
        // What the user types after it is theirs.
        assert_eq!(l.whose("t8f", "now also check the docs", given_task(&t).as_deref(), T0 + 300_000), Whose::User);
    }

    #[test]
    fn a_prompt_the_tower_typed_a_moment_ago_is_the_towers() {
        let mut l = Log::default();
        let id = l.note("t5", "Fix the login form, then run the tests.", "queue", T0);
        l.done(id, true, T0 + 400);
        // The hook brings it back wrapped as pasted, a moment later.
        let seen = "<pasted_content id=\"2248\">\nFix the login form,  then run the tests.\n</pasted_content id=\"2248\">";
        assert_eq!(l.whose("t5", seen, None, T0 + 900), Whose::Tower("queue".into()));
        assert!(!l.users("t5", T0 + 900));
        // Taken: the same words again, a while later, are the user's.
        assert_eq!(l.whose("t5", "Fix the login form, then run the tests.", None, T0 + 120_000), Whose::User);
        assert!(l.users("t5", T0 + 120_000));
    }

    #[test]
    fn the_same_words_the_tower_sent_long_ago_are_the_users() {
        let mut l = Log::default();
        let id = l.note("t5", "run the tests", "queue", T0);
        l.done(id, true, T0 + 300);
        // Its own prompt never came (no hook then); long after, the user types the same.
        assert_eq!(l.whose("t5", "run the tests", None, T0 + WORDS_MS + 60_000), Whose::User);
        // Another terminal's text is never this one's.
        let mut l = Log::default();
        let id = l.note("t6", "run the tests", "queue", T0 + 1);
        l.done(id, true, T0 + 2);
        assert_eq!(l.whose("t5", "run the tests", None, T0 + 30_000), Whose::User);
        assert_eq!(l.whose("t6", "run the tests", None, T0 + 30_000), Whose::Tower("queue".into()));
    }

    #[test]
    fn a_prompt_nothing_was_typed_for_is_the_users() {
        let mut l = Log::default();
        assert_eq!(l.whose("t5", "also update the README", None, T0), Whose::User);
        // The tower typed into it long ago: its words don't match, and it isn't now.
        let id = l.note("t5", "deploy the docs site", "queue", T0);
        l.done(id, true, T0 + 500);
        assert_eq!(l.whose("t5", "no, use the other branch", None, T0 + 60_000), Whose::User);
    }

    #[test]
    fn a_prompt_said_differently_while_the_tower_types_is_the_towers() {
        let mut l = Log::default();
        // Going in this moment (hc waits for a pause): the hook's words differ (a file became an image).
        let id = l.note("t5", "Look at this: [attached: /a/b/shot.png]", "queue", T0);
        assert_eq!(l.whose("t5", "Look at this: [Image #1]", None, T0 + 1_500), Whose::Tower("queue".into()));
        l.done(id, true, T0 + 1_600);
        assert_eq!(l.whose("t5", "Look at this: [Image #1]", None, T0 + 4_000), Whose::Tower("queue".into()));
        // Not taken by a guess: once the typing is a while over, other words are the user's.
        assert_eq!(l.whose("t5", "and now the footer", None, T0 + 1_600 + JUST_MS + 1_000), Whose::User);
        // A send that didn't go in leaves nothing behind.
        let id = l.note("t7", "something", "queue", T0);
        l.done(id, false, T0 + 100);
        assert_eq!(l.whose("t7", "something else entirely", None, T0 + 200), Whose::User);
    }

    #[test]
    fn what_another_terminal_or_the_start_gave_is_not_the_users() {
        let mut l = Log::default();
        assert_eq!(l.whose("t5", "<hc from=\"7136/t4m\">Reply with exactly: pong.</hc>", None, T0), Whose::Terminal("7136/t4m".into()));
        let given = "Set up the billing page\n\nYou work in /w/.tower-worktrees/shop/app on shop/app.";
        assert_eq!(l.whose("t5", given, Some(given), T0), Whose::Tower("start".into()));
        let intro = "[tower] You are Claude in app (t5) in the Herdr space \"Shop\". 1 other Claude session is open";
        assert_eq!(l.whose("t5", intro, None, T0), Whose::Tower("introduction".into()));
        assert!(!l.users("t5", T0));
    }

    #[test]
    fn what_the_tower_types_next_ends_the_users_turn() {
        let mut l = Log::default();
        assert_eq!(l.whose("t5", "rename the module", None, T0), Whose::User);
        assert!(l.users("t5", T0) && !l.users("t5", T0 + 1) && !l.users("t6", T0));
        l.note("t5", "From Shop › api (t6): the schema changed", "queue", T0 + 5_000);
        assert!(!l.users("t5", T0));
        assert!(l.typed_lately("t5", T0 + 6_000, 20_000) && !l.typed_lately("t5", T0 + 60_000, 20_000) && !l.typed_lately("t6", T0 + 6_000, 20_000));
    }

    /// What the send queue typed in while the session worked comes back as
    /// a prompt when that turn ends, hours later maybe: still the tower's.
    /// The turn under way stays the user's meanwhile.
    #[test]
    fn what_was_typed_in_while_it_worked_is_the_towers_when_its_turn_ends() {
        let mut l = Log::default();
        assert_eq!(l.whose("t5", "refactor the billing module", None, T0), Whose::User);
        let id = l.note_as("t5", "From Shop › api (t6): the schema changed", "queue", true, T0 + 60_000);
        l.done(id, true, T0 + 61_000);
        assert!(l.users("t5", T0), "the turn it works on is still the user's");
        let three_hours = T0 + 3 * 3_600_000;
        assert_eq!(l.whose("t5", "From Shop › api (t6): the schema changed", None, three_hours), Whose::Tower("queue".into()));
        // Once: the same words later are someone else's (they open as a message from t6: that session's).
        assert_eq!(l.whose("t5", "From Shop › api (t6): the schema changed", None, three_hours + 1_000), Whose::Terminal("t6".into()));
        // Not held: after ten minutes its words are no longer the tower's.
        let id = l.note("t8", "run the migrations check please", "queue", T0);
        l.done(id, true, T0 + 100);
        assert_eq!(l.whose("t8", "run the migrations check please", None, T0 + WORDS_MS + 1), Whose::User);
    }

    /// A message from another session is no prompt of the user's, however it
    /// came in (another terminal's paste, not the queue); nor is Claude Code's
    /// notice that background work of the session's own finished.
    #[test]
    fn a_message_or_a_notice_is_not_the_users_prompt() {
        let mut l = Log::default();
        let from = "<pasted_content id=\"2248\">\nFrom t7p (m5pro herder tower). Sorry for the silence: your messages reached me late.\n</pasted_content id=\"2248\">";
        assert_eq!(l.whose("t7r", from, None, T0), Whose::Terminal("t7p".into()));
        assert_eq!(l.whose("t7r", "From Sandbox › eyzflow (t7h): the plan is on the page", None, T0), Whose::Terminal("t7h".into()));
        assert_eq!(l.whose("t7p", "<task-notification> <summary>Goal check-in: background work still running</summary> </task-notification>", None, T0), Whose::Session);
        assert!(!l.users("t7p", T0) && !l.users("t7r", T0));
        // The user's own words, even about a session.
        assert_eq!(l.whose("t7r", "From now on, run the tests before every commit", None, T0), Whose::User);
        assert_eq!(l.whose("t7r", "from t7p's branch, cherry-pick the fix", None, T0), Whose::User);
    }

    #[test]
    fn a_terminals_first_task_is_its_command_lines_last_word() {
        let t = serde_json::json!({ "argv": ["cca", "orca", "-n", "web", "fix the login"] });
        assert_eq!(given_task(&t).as_deref(), Some("fix the login"));
        let t = serde_json::json!({ "argv": ["/usr/local/bin/claude", "-n", "web"] });
        assert_eq!(given_task(&t), None, "a name is no task");
        assert_eq!(given_task(&serde_json::json!({ "argv": ["/bin/zsh", "-l"] })), None);
        // Behind the login shell `tower start` runs the agent in, as Herdr lists it.
        let started = serde_json::json!({ "argv": ["/bin/zsh", "-lic", "\"$@\"; exec \"$0\" -li", "/bin/zsh", "/Users/u/.local/bin/cca", "default", "-n", "intro-check", "This is a throwaway check. Reply with ok."] });
        assert_eq!(given_task(&started).as_deref(), Some("This is a throwaway check. Reply with ok."));
        let named_only = serde_json::json!({ "argv": ["/bin/zsh", "-lic", "\"$@\"; exec \"$0\" -li", "/bin/zsh", "/Users/u/.local/bin/cca", "default", "-n", "web"] });
        assert_eq!(given_task(&named_only), None, "a name is no task, behind the shell too");
        assert_eq!(given_task(&serde_json::json!({ "argv": ["/bin/zsh", "-lic", "\"$@\"; exec \"$0\" -li", "/bin/zsh", "/usr/local/bin/claude"] })), None);
        assert_eq!(given_task(&serde_json::json!({ "argv": ["codex", "--yolo"] })), None);
        assert_eq!(given_task(&serde_json::json!({})), None);
    }
}
