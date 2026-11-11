//! Introducing related sessions: when a Claude session joins a Herdr space
//! that has other Claude sessions, every Claude session in that space is told
//! who the others are (name, address, folder, what each is on), and invited
//! to talk to them and ask for changes in their layer instead of editing
//! their repos, with how (`tower queue`, `hc read`).
//!
//! This file is the part without side effects: who counts as a member,
//! when a space is due, the message, and the book of what was sent. The
//! daemon (`daemon::introduce`) scans Herdr, sends through the send queue
//! (so a working session is never interrupted) and keeps the book in
//! `~/.local/state/herder-tower/introductions.json`.
//!
//! A space is due when its set of Claude sessions has a member it didn't
//! have when last introduced (or recorded), has two members or more, and has
//! held still for `SETTLE_MS` (a dispatch that starts three sessions is one
//! introduction, not two). Sessions leaving, a lone session, or the same set
//! again send nothing; the book just follows. The first run records what
//! is open without sending anything, so turning this on doesn't message
//! every space at once.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A space's set must hold this long before it's introduced.
pub const SETTLE_MS: u64 = 30_000;

/// A Claude session in a space.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Member {
    pub address: String,
    /// Its name (`b2c-mediaroom`), or empty.
    pub name: String,
    pub folder: String,
    pub space: String,
    /// When its terminal was made: another terminal at the same address is another member.
    pub created_ms: Option<u64>,
    /// What it's on (the agenda's task, its last prompt), or empty.
    pub task: String,
    /// That task is over: it is what the user last asked it for.
    pub task_done: bool,
}

impl Member {
    /// Who it is, for the book: the address and its terminal.
    pub fn key(&self) -> String {
        match self.created_ms {
            Some(c) => format!("{}@{c}", self.address),
            None => self.address.clone(),
        }
    }

    /// What to call it, as it is called everywhere else (`crate::names`):
    /// its name within its space (`b2c-sdk` in "B2C" is `sdk`), else the
    /// project it works on (`eyz/eyz-mediaroom-serverless` is `mediaroom
    /// serverless`), else its address.
    pub fn label(&self) -> String {
        let space = Some(self.space.trim()).filter(|s| !s.is_empty());
        let named = crate::names::session_label(space, &self.name);
        if !named.is_empty() {
            return named;
        }
        crate::names::project(&serde_json::json!({ "cwd": self.folder })).unwrap_or_else(|| format!("the session at {}", self.address))
    }
}

/// The names that are the tower's own terminals, never members.
pub fn is_tower(name: &str) -> bool {
    name == crate::brain::NAME || name == crate::brain::VOICE_NAME || name == crate::brain::name()
}

/// A terminal as a member, if it is one: live, here, a Claude Code session,
/// in a space, and not the tower's. `listed` is its line in `hc ls` (the
/// space and app as they are now); `full` its `hc get` (labels, its terminal's
/// creation time), when known.
pub fn member_of(listed: &Value, full: Option<&Value>) -> Option<Member> {
    let t = full.unwrap_or(listed);
    if listed["state"].as_str().is_some_and(|s| s != "live") || listed["address"].as_str()?.contains('/') {
        return None;
    }
    let app = crate::hc::agent_app(listed).or_else(|| listed["agent"].as_str().map(String::from)).or_else(|| crate::hc::agent_app(t))?;
    if app != "claude-code" {
        return None;
    }
    let name = crate::hc::name(t).or_else(|| crate::hc::name(listed)).unwrap_or("").to_string();
    // The tower's own, and a space's own brain, are never introduced as sessions.
    if is_tower(&name) || crate::brain::brain_of(t).is_some() || crate::brain::brain_of(listed).is_some() {
        return None;
    }
    let place = listed["labels"]["where"].as_str().or_else(|| listed["where"].as_str()).or_else(|| t["labels"]["where"].as_str())?;
    let space = place.split(" / ").next().map(str::trim).filter(|s| !s.is_empty())?.to_string();
    let folder = t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str()).or_else(|| listed["cwd"].as_str()).unwrap_or("").to_string();
    // The tower's own folder (the brain) is never a member, whatever its name.
    if crate::brains::is_brain_folder(&folder) {
        return None;
    }
    Some(Member { address: listed["address"].as_str()?.to_string(), name, folder, space, created_ms: crate::hc::created_ms(t), task: String::new(), task_done: false })
}

/// Members by space, each space's in address order.
pub fn group(members: Vec<Member>) -> BTreeMap<String, Vec<Member>> {
    let mut out: BTreeMap<String, Vec<Member>> = BTreeMap::new();
    for m in members {
        out.entry(m.space.clone()).or_default().push(m);
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| a.address.cmp(&b.address));
    }
    out
}

/// What a space last had, and when it was introduced.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Known {
    /// Member keys (`address@created_ms`).
    pub members: BTreeSet<String>,
    /// When the set was last introduced (0: only recorded).
    pub sent_ms: u64,
    /// Who got it then.
    pub sent_to: Vec<String>,
}

/// What was introduced (or recorded) per space: kept across restarts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    /// The first look was recorded (nothing is sent for what was already open then).
    pub seeded: bool,
    pub spaces: BTreeMap<String, Known>,
    /// Each member's last real task (by key): its latest prompt may be a
    /// note, an introduction among them, which is no task.
    pub tasks: BTreeMap<String, String>,
    /// Who has had an introduction (by key: a terminal), and to which space.
    /// Each gets one: a session that drops out of view for a while (Herdr
    /// reads it a moment without its agent, the tower restarts) and comes
    /// back is not introduced again, nor are those already there when
    /// another joins: only the newcomer is. Forgotten an hour after its
    /// terminal is no longer seen.
    pub introduced: BTreeMap<String, Intro>,
}

/// A session's introduction: to which space, when, and since when its
/// terminal has not been seen (None: it is there).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Intro {
    pub space: String,
    pub at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gone_ms: Option<u64>,
}

/// How long a terminal that is no longer seen keeps its introduction.
pub const INTRODUCED_KEEP_MS: u64 = 60 * 60_000;

impl Book {
    pub fn load_from(path: &Path) -> Book {
        let mut book = Self::read(path);
        book.migrate();
        book
    }

    fn read(path: &Path) -> Book {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                let aside = path.with_extension(format!("bad-{}.json", crate::history::now_ms()));
                let _ = std::fs::copy(path, &aside);
                eprintln!("[introduce] {} unreadable ({e}); kept as {}", path.display(), aside.display());
                // Not seeded again: what is open now isn't news, but a later join is.
                Book { seeded: false, ..Default::default() }
            }),
            Err(_) => Book::default(),
        }
    }

    /// Written whole through a temporary file and a rename.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// A book from before `introduced`: those it sent an introduction to have had one.
    fn migrate(&mut self) {
        if !self.introduced.is_empty() {
            return;
        }
        for (space, k) in self.spaces.iter().filter(|(_, k)| k.sent_ms > 0) {
            for key in &k.members {
                let address = key.split('@').next().unwrap_or(key);
                if k.sent_to.iter().any(|a| a == address) {
                    self.introduced.insert(key.clone(), Intro { space: space.clone(), at_ms: k.sent_ms, gone_ms: None });
                }
            }
        }
    }

    /// It was introduced to `members` now: every one of them got it.
    #[cfg(test)]
    pub fn sent(&mut self, space: &str, members: &[Member], now: u64) {
        self.introduced_to(space, members, members, now);
    }

    /// `space` has `members` now, and `reached` of them got the introduction.
    pub fn introduced_to(&mut self, space: &str, members: &[Member], reached: &[Member], now: u64) {
        self.spaces.insert(
            space.to_string(),
            Known { members: members.iter().map(Member::key).collect(), sent_ms: now, sent_to: reached.iter().map(|m| m.address.clone()).collect() },
        );
        for m in reached {
            self.introduced.insert(m.key(), Intro { space: space.to_string(), at_ms: now, gone_ms: None });
        }
    }

    /// `space` has `members` now, and nobody needed an introduction (they all had one).
    pub fn known_now(&mut self, space: &str, members: &[Member]) {
        self.spaces.entry(space.to_string()).or_default().members = members.iter().map(Member::key).collect();
    }

    /// Those of `members` that have had no introduction to `space`.
    pub fn recipients<'a>(&self, space: &str, members: &'a [Member]) -> Vec<&'a Member> {
        members.iter().filter(|m| self.introduced.get(&m.key()).is_none_or(|i| i.space != space)).collect()
    }

    /// The terminals seen now (`present`, by key): an introduction is kept
    /// while its terminal is seen, and for an hour after. True when the book changed.
    pub fn keep_introduced(&mut self, present: &BTreeSet<String>, now: u64) -> bool {
        let mut changed = false;
        self.introduced.retain(|key, i| {
            if present.contains(key) {
                changed |= i.gone_ms.take().is_some();
                return true;
            }
            match i.gone_ms {
                None => {
                    i.gone_ms = Some(now);
                    changed = true;
                    true
                }
                Some(gone) if now.saturating_sub(gone) > INTRODUCED_KEEP_MS => {
                    changed = true;
                    false
                }
                Some(_) => true,
            }
        });
        changed
    }
}

/// A space's set as it was when first seen changed, and since when (in memory: a restart waits again).
#[derive(Debug, Clone, PartialEq)]
pub struct Settling {
    pub members: BTreeSet<String>,
    pub since_ms: u64,
}

/// One look at the spaces: what's due now. The book follows what it sees
/// (sessions leaving, a lone one, the first look); `changed` says whether it
/// should be saved. A failed look must not be passed in (it would read as everyone leaving).
pub fn decide(book: &mut Book, settling: &mut BTreeMap<String, Settling>, groups: &BTreeMap<String, Vec<Member>>, now: u64, settle_ms: u64) -> (Vec<String>, bool) {
    let mut changed = false;
    if !book.seeded {
        for (space, ms) in groups {
            book.spaces.insert(space.clone(), Known { members: ms.iter().map(Member::key).collect(), ..Default::default() });
        }
        book.seeded = true;
        settling.clear();
        return (vec![], true);
    }
    let mut due = vec![];
    for (space, ms) in groups {
        let set: BTreeSet<String> = ms.iter().map(Member::key).collect();
        let known = book.spaces.get(space).map(|k| k.members.clone()).unwrap_or_default();
        if set == known {
            settling.remove(space);
            continue;
        }
        let joined = set.iter().any(|k| !known.contains(k));
        if !joined || set.len() < 2 {
            // Only leaving, or alone: nothing to say; the book follows.
            let k = book.spaces.entry(space.clone()).or_default();
            k.members = set;
            settling.remove(space);
            changed = true;
            continue;
        }
        match settling.get(space) {
            Some(s) if s.members == set && now.saturating_sub(s.since_ms) >= settle_ms => {
                settling.remove(space);
                due.push(space.clone());
            }
            Some(s) if s.members == set => {}
            _ => {
                settling.insert(space.clone(), Settling { members: set, since_ms: now });
            }
        }
    }
    // A space with no Claude session left: forgotten, so two that come later are news.
    let gone: Vec<String> = book.spaces.keys().filter(|s| !groups.contains_key(*s)).cloned().collect();
    for s in gone {
        book.spaces.remove(&s);
        settling.remove(&s);
        changed = true;
    }
    (due, changed)
}

fn tilde(folder: &str) -> String {
    let home = crate::config::home().to_string_lossy().into_owned();
    match folder.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => folder.to_string(),
    }
}

fn clip(s: &str, n: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n { flat } else { format!("{}…", flat.chars().take(n).collect::<String>().trim_end()) }
}

/// An introduction (ours), whatever wraps it: never anyone's task.
pub fn is_introduction(text: &str) -> bool {
    text.contains("[tower] You are ") && text.contains("in the Herdr space") && text.contains("Claude session")
}

/// What a session is on, from its latest prompt: the text without what
/// delivered it (`<hc from="t5">…</hc>`, a `<pasted_content …>` wrapper);
/// None for an introduction or nothing.
pub fn task_from_prompt(prompt: &str) -> Option<String> {
    if is_introduction(prompt) {
        return None;
    }
    let mut out = String::new();
    let mut rest = prompt;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let wrapper = ["<hc from=", "</hc>", "<pasted_content", "</pasted_content>"].iter().any(|w| tail.starts_with(w));
        match (wrapper, tail.find('>')) {
            (true, Some(end)) => rest = &tail[end + 1..],
            _ => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    let t = out.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

/// What the user asked a session for, from a text it was given: as
/// `task_from_prompt`, and never a message from another session ("From
/// t7h: …", "From Shop › web (t7h): …") nor the session's own notice that
/// background work finished (`<task-notification>`).
pub fn users_task(text: &str) -> Option<String> {
    task_from_prompt(text).filter(|t| crate::peers::sender_in(t).is_none() && !signed_by_a_session(t) && !t.starts_with("<task-notification>") && !bare_reply(t))
}

/// A message that opens by saying which session wrote it, the way sessions
/// introduce themselves to each other ("Hi, this is sandbox-eyzflow (t7h),
/// …", "I'm the api session (t2): …"): an older tower kept some as tasks.
fn signed_by_a_session(text: &str) -> bool {
    let head: String = text.chars().take(120).collect::<String>().to_lowercase();
    ["this is ", "i am ", "i'm ", "i’m "].iter().filter_map(|w| head.find(w).map(|i| &head[i + w.len()..])).any(|rest| {
        let rest: String = rest.chars().take(70).collect();
        rest.find('(').and_then(|o| rest[o + 1..].find(')').map(|c| &rest[o + 1..o + 1 + c])).is_some_and(|inside| {
            let mut ch = inside.chars();
            inside.len() <= 8 && ch.next() == Some('t') && ch.next().is_some_and(|c| c.is_ascii_digit()) && inside.chars().all(|c| c.is_ascii_alphanumeric())
        })
    })
}

/// An answer to a question, not a request: "yeah", "ok", "2", "go on".
fn bare_reply(text: &str) -> bool {
    const REPLIES: [&str; 22] = [
        "y", "yes", "yeah", "yep", "yup", "sure", "ok", "okay", "k", "n", "no", "nope", "go", "go on", "go ahead", "do it", "continue", "proceed",
        "thanks", "thank you", "done", "cool",
    ];
    let t: String = text.trim().trim_end_matches(['.', '!', '?']).to_lowercase();
    REPLIES.contains(&t.as_str()) || (!t.is_empty() && t.len() <= 2 && t.chars().all(|c| c.is_ascii_digit()))
}

impl Book {
    /// Each member's task: `fresh` (what it is on now) when there is one,
    /// else the last real task it had. Forgets those no space has any more,
    /// and a kept one that is no request of the user's (an older tower kept
    /// messages between sessions as tasks). True when the book changed.
    pub fn tasks_for(&mut self, groups: &mut BTreeMap<String, Vec<Member>>, fresh: impl Fn(&Member) -> Option<String>) -> bool {
        let before_kept = self.tasks.len();
        self.tasks.retain(|_, t| users_task(t).is_some());
        let mut changed = self.tasks.len() != before_kept;
        let mut keys = BTreeSet::new();
        for m in groups.values_mut().flatten() {
            let key = m.key();
            match fresh(m) {
                Some(t) => {
                    if self.tasks.get(&key) != Some(&t) {
                        self.tasks.insert(key.clone(), t.clone());
                        changed = true;
                    }
                    m.task = t;
                }
                None => {
                    // The last of the user's requests it had: over by now, as far as the tower knows.
                    m.task = self.tasks.get(&key).cloned().unwrap_or_default();
                    m.task_done = true;
                }
            }
            keys.insert(key);
        }
        let before = self.tasks.len();
        self.tasks.retain(|k, _| keys.contains(k));
        changed || self.tasks.len() != before
    }
}

/// The message for `to`: the other Claude sessions in its space, what each
/// is on, and how to reach them.
pub fn compose(space: &str, to: &Member, members: &[Member]) -> String {
    let others: Vec<&Member> = members.iter().filter(|m| m.address != to.address).collect();
    let mut out = format!(
        "[tower] You are {} ({}) in the Herdr space \"{space}\". {} other Claude session{} open in this space:\n",
        to.label(),
        to.address,
        others.len(),
        if others.len() == 1 { " is" } else { "s are" }
    );
    for m in &others {
        let task = if m.task.trim().is_empty() {
            "no task reported yet".to_string()
        } else if m.task_done {
            format!("last asked for: {}", clip(&m.task, 160))
        } else {
            format!("working on: {}", clip(&m.task, 160))
        };
        out += &format!("- {} ({}), in {}: {task}\n", m.label(), m.address, tilde(&m.folder));
    }
    out += "Talk to them when you need their help, and ask for changes in their layer instead of editing their repos. \
        To hand one a message: tower queue <address> \"<complete message>\" (the tower adds your name to it, so don't write who it is from; \
        it waits until that session is free). To look at one: hc read <address> --lines 60. \
        Nothing to answer here: carry on with your work.";
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(address: &str, space: &str) -> Member {
        Member {
            address: address.into(),
            name: format!("s-{address}"),
            folder: format!("/w/{address}"),
            space: space.into(),
            created_ms: Some(100),
            task: String::new(),
            task_done: false,
        }
    }

    fn groups(ms: &[Member]) -> BTreeMap<String, Vec<Member>> {
        group(ms.to_vec())
    }

    /// One look, settled: a first look, then the same again past the settle time.
    fn look(book: &mut Book, s: &mut BTreeMap<String, Settling>, ms: &[Member], now: u64) -> Vec<String> {
        let (mut due, _) = decide(book, s, &groups(ms), now, SETTLE_MS);
        let (more, _) = decide(book, s, &groups(ms), now + SETTLE_MS, SETTLE_MS);
        due.extend(more);
        due
    }

    fn seeded() -> Book {
        Book { seeded: true, ..Default::default() }
    }

    #[test]
    fn a_new_session_joining_one_other_introduces_both() {
        let (mut book, mut s) = (seeded(), BTreeMap::new());
        assert!(look(&mut book, &mut s, &[m("t1", "B2C")], 0).is_empty(), "a lone session gets nothing");
        assert_eq!(look(&mut book, &mut s, &[m("t1", "B2C"), m("t2", "B2C")], 100_000), vec!["B2C".to_string()]);
    }

    #[test]
    fn several_others_and_a_dispatch_is_one_introduction() {
        let (mut book, mut s) = (seeded(), BTreeMap::new());
        let three = [m("t1", "B2C"), m("t2", "B2C"), m("t3", "B2C")];
        // t2 and t3 arrive within the settle time: one introduction, to the three.
        assert!(decide(&mut book, &mut s, &groups(&three[..2]), 0, SETTLE_MS).0.is_empty());
        assert!(decide(&mut book, &mut s, &groups(&three), 5_000, SETTLE_MS).0.is_empty());
        assert!(decide(&mut book, &mut s, &groups(&three), 5_000 + SETTLE_MS - 1, SETTLE_MS).0.is_empty());
        assert_eq!(decide(&mut book, &mut s, &groups(&three), 5_000 + SETTLE_MS, SETTLE_MS).0, vec!["B2C".to_string()]);
        book.sent("B2C", &three, 5_000 + SETTLE_MS);
        // A fourth joins later: introduced again, to all four.
        let four = [three.to_vec(), vec![m("t4", "B2C")]].concat();
        assert_eq!(look(&mut book, &mut s, &four, 200_000), vec!["B2C".to_string()]);
    }

    #[test]
    fn the_same_set_again_sends_nothing_twice() {
        let (mut book, mut s) = (seeded(), BTreeMap::new());
        let two = [m("t1", "B2C"), m("t2", "B2C")];
        assert_eq!(look(&mut book, &mut s, &two, 0).len(), 1);
        book.sent("B2C", &two, 1);
        for i in 0..5 {
            assert!(look(&mut book, &mut s, &two, 100_000 * (i + 1)).is_empty());
        }
        // One leaves, then nothing; it was already known: leaving says nothing.
        assert!(look(&mut book, &mut s, &two[..1], 900_000).is_empty());
        // Another session at a reused address is a new member.
        let reused = [m("t1", "B2C"), Member { created_ms: Some(999), ..m("t2", "B2C") }];
        assert_eq!(look(&mut book, &mut s, &reused, 1_000_000).len(), 1);
    }

    #[test]
    fn spaces_are_separate_and_the_first_look_sends_nothing() {
        let mut book = Book::default();
        let mut s = BTreeMap::new();
        let open = [m("t1", "B2C"), m("t2", "B2C"), m("t3", "B2B")];
        let (due, changed) = decide(&mut book, &mut s, &groups(&open), 0, SETTLE_MS);
        assert!(due.is_empty() && changed && book.seeded, "already open when the tower first looked: not news");
        assert!(look(&mut book, &mut s, &open, 1).is_empty());
        // One joins B2B, where t3 is alone: B2B only.
        let joined = [open.to_vec(), vec![m("t4", "B2B")]].concat();
        assert_eq!(look(&mut book, &mut s, &joined, 100_000), vec!["B2B".to_string()]);
        book.sent("B2B", &groups(&joined)["B2B"], 100_000);
        // A session in another space alone: nothing.
        let elsewhere = [joined.clone(), vec![m("t9", "Support")]].concat();
        assert!(look(&mut book, &mut s, &elsewhere, 200_000).is_empty());
    }

    #[test]
    fn the_book_survives_a_restart() {
        let (mut book, mut s) = (seeded(), BTreeMap::new());
        let two = [m("t1", "B2C"), m("t2", "B2C")];
        assert_eq!(look(&mut book, &mut s, &two, 0).len(), 1);
        book.sent("B2C", &two, 7);
        let path = std::env::temp_dir().join(format!("tower-introduce-{}.json", std::process::id()));
        book.save_to(&path).unwrap();
        let mut back = Book::load_from(&path);
        assert_eq!(back, book);
        // After the restart, the same set: nothing; a new one: introduced.
        let mut fresh = BTreeMap::new();
        assert!(look(&mut back, &mut fresh, &two, 500_000).is_empty());
        let three = [two.to_vec(), vec![m("t3", "B2C")]].concat();
        assert_eq!(look(&mut back, &mut fresh, &three, 600_000).len(), 1);
        std::fs::write(&path, "garbage").unwrap();
        assert!(!Book::load_from(&path).seeded, "an unreadable book starts over by recording, not by messaging everyone");
        let _ = std::fs::remove_file(&path);
    }

    /// The recipients' addresses when `ms` is due in B2C.
    fn to(book: &Book, ms: &[Member]) -> Vec<String> {
        book.recipients("B2C", ms).into_iter().map(|m| m.address.clone()).collect()
    }

    #[test]
    fn a_session_that_drops_out_of_view_and_comes_back_is_not_introduced_again() {
        let (mut book, mut s) = (seeded(), BTreeMap::new());
        let two = [m("t1", "B2C"), m("t2", "B2C")];
        assert_eq!(look(&mut book, &mut s, &two, 0).len(), 1);
        book.sent("B2C", &two, 1);
        // The tower restarts, or Herdr reads t2 a moment without its agent: t2 is not seen.
        assert!(look(&mut book, &mut s, &two[..1], 100_000).is_empty());
        assert!(book.keep_introduced(&two[..1].iter().map(Member::key).collect(), 100_000));
        // Back: a join as the space's set goes, but both have had theirs.
        assert_eq!(look(&mut book, &mut s, &two, 200_000), vec!["B2C".to_string()]);
        assert!(to(&book, &two).is_empty(), "nobody is introduced again");
        book.known_now("B2C", &two);
        assert!(book.keep_introduced(&two.iter().map(Member::key).collect(), 200_000), "t2 is there again");
        assert!(look(&mut book, &mut s, &two, 300_000).is_empty(), "and then the set is as it was");
    }

    #[test]
    fn a_newcomer_alone_is_introduced_and_hears_of_everyone() {
        let (mut book, mut s) = (seeded(), BTreeMap::new());
        let two = [m("t1", "B2C"), m("t2", "B2C")];
        assert_eq!(look(&mut book, &mut s, &two, 0).len(), 1);
        book.sent("B2C", &two, 1);
        let three = [two.to_vec(), vec![m("t3", "B2C")]].concat();
        assert_eq!(look(&mut book, &mut s, &three, 100_000), vec!["B2C".to_string()]);
        assert_eq!(to(&book, &three), ["t3"], "only the newcomer");
        let text = compose("B2C", &three[2], &three);
        assert!(text.contains("(t1)") && text.contains("(t2)") && text.starts_with("[tower] You are s t3 (t3)"), "{text}");
        let reached: Vec<Member> = book.recipients("B2C", &three).into_iter().cloned().collect();
        book.introduced_to("B2C", &three, &reached, 100_000);
        assert!(to(&book, &three).is_empty());
        assert_eq!(book.spaces["B2C"].sent_to, ["t3"]);
        // A new terminal at an old address is a new session: it gets one.
        let reused = [three[0].clone(), three[1].clone(), Member { created_ms: Some(999), ..m("t3", "B2C") }];
        assert_eq!(to(&book, &reused), ["t3"]);
        // The same terminal moved to another space gets that space's.
        assert_eq!(book.recipients("Support", &[m("t1", "Support")]).len(), 1);
    }

    #[test]
    fn who_was_introduced_survives_a_restart_and_an_older_book_counts_those_it_sent_to() {
        let mut book = seeded();
        let two = [m("t1", "B2C"), m("t2", "B2C")];
        book.sent("B2C", &two, 7);
        let path = std::env::temp_dir().join(format!("tower-introduced-{}.json", std::process::id()));
        book.save_to(&path).unwrap();
        let back = Book::load_from(&path);
        assert_eq!(back, book);
        assert!(to(&back, &two).is_empty());
        // A book from before `introduced`: those it sent to have had one; one it only recorded has not.
        let old = serde_json::json!({
            "seeded": true,
            "spaces": {
                "B2C": { "members": ["t1@100", "t2@100", "t3@100"], "sent_ms": 5, "sent_to": ["t1", "t2"] },
                "Support": { "members": ["t8@100", "t9@100"], "sent_ms": 0, "sent_to": [] }
            }
        });
        std::fs::write(&path, old.to_string()).unwrap();
        let migrated = Book::load_from(&path);
        let three = [two.to_vec(), vec![m("t3", "B2C")]].concat();
        assert_eq!(to(&migrated, &three), ["t3"]);
        assert_eq!(migrated.recipients("Support", &[m("t8", "Support"), m("t9", "Support")]).len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_introduction_is_forgotten_an_hour_after_its_terminal_went() {
        let mut book = seeded();
        let two = [m("t1", "B2C"), m("t2", "B2C")];
        book.sent("B2C", &two, 0);
        let only_t1: BTreeSet<String> = [two[0].key()].into();
        assert!(book.keep_introduced(&only_t1, 1_000));
        assert!(!book.keep_introduced(&only_t1, 1_000 + INTRODUCED_KEEP_MS), "an hour: kept, and nothing to save");
        assert!(book.introduced.contains_key(&two[1].key()));
        assert!(book.keep_introduced(&only_t1, 2_000 + INTRODUCED_KEEP_MS));
        assert!(!book.introduced.contains_key(&two[1].key()) && book.introduced.contains_key(&two[0].key()));
    }

    #[test]
    fn working_on_is_what_the_user_asked_never_a_message_between_sessions() {
        assert_eq!(users_task("Fix the login bug in the api").as_deref(), Some("Fix the login bug in the api"));
        assert_eq!(users_task("<pasted_content id=\"1\">merge it and rebuild and restart the tower</pasted_content>").as_deref(), Some("merge it and rebuild and restart the tower"));
        assert_eq!(users_task("From M5 Pro › m5pro herder tower (t7p): Heads-up, for your merge"), None);
        assert_eq!(users_task("From t7h: both corrections are on the page"), None);
        assert_eq!(users_task("<hc from=\"t7p\">From t7p (Mac tower). Installed now</hc>"), None);
        assert_eq!(users_task("<task-notification> <summary>done</summary>"), None);
        assert_eq!(users_task(&compose("B2C", &m("t1", "B2C"), &[m("t1", "B2C"), m("t2", "B2C")])), None);
        for reply in ["yeah", "Yes.", "ok", "2", "go ahead", "Do it!"] {
            assert_eq!(users_task(reply), None, "{reply} answers a question");
        }
        assert_eq!(users_task("fix login").as_deref(), Some("fix login"));
        assert_eq!(users_task("Hi, this is sandbox-eyzflow (t7h), the dashboard session. With sandbox-project"), None);
        assert_eq!(users_task("I'm the api session (t2): the endpoint is on 3001"), None);
        assert_eq!(users_task("This is urgent (top priority): fix the login").as_deref(), Some("This is urgent (top priority): fix the login"));
        assert_eq!(users_task("this is the billing page (t1 in the plan): port it").as_deref(), Some("this is the billing page (t1 in the plan): port it"));
        // A message an older tower kept as a session's task is dropped; the user's stays.
        let mut book = seeded();
        let (a, b) = (m("t1", "B2C"), m("t2", "B2C"));
        book.tasks.insert(a.key(), "From M5 Pro › m5pro herder tower (t7p): Retry with 0.43".into());
        book.tasks.insert(b.key(), "commit everything and push. then give me the release".into());
        let mut g = groups(&[a.clone(), b.clone()]);
        assert!(book.tasks_for(&mut g, |_| None));
        assert_eq!(g["B2C"][0].task, "", "no task reported yet, rather than another session's message");
        assert_eq!(g["B2C"][1].task, "commit everything and push. then give me the release");
        assert!(!book.tasks.contains_key(&a.key()));
        assert!(compose("B2C", &b, &g["B2C"]).contains("(t1), in /w/t1: no task reported yet"));
        // The kept one is what the user last asked; one it is on now is what it is working on.
        assert!(compose("B2C", &a, &g["B2C"]).contains("(t2), in /w/t2: last asked for: commit everything and push."));
        let mut g = groups(&[a.clone(), b.clone()]);
        book.tasks_for(&mut g, |m| (m.address == "t2").then(|| "port the billing page".to_string()));
        assert!(compose("B2C", &a, &g["B2C"]).contains("(t2), in /w/t2: working on: port the billing page"));
    }

    #[test]
    fn only_live_local_claude_sessions_in_a_space_are_members() {
        let listed = |address: &str, agent: &str, place: Option<&str>, name: Option<&str>| {
            let mut v = json!({ "address": address, "agent": agent, "cwd": "/w/app", "state": "live", "status": "idle" });
            if let Some(p) = place {
                v["where"] = json!(p);
            }
            if let Some(n) = name {
                v["name"] = json!(n);
            }
            v
        };
        let a = member_of(&listed("t5", "claude-code", Some("B2C support / mediaroom"), Some("b2c-mediaroom")), None).unwrap();
        assert_eq!((a.space.as_str(), a.name.as_str(), a.folder.as_str()), ("B2C support", "b2c-mediaroom", "/w/app"));
        assert!(member_of(&listed("t6", "codex", Some("B2C support / x"), None), None).is_none(), "Codex isn't introduced");
        assert!(member_of(&listed("t7", "zsh", Some("B2C support / x"), None), None).is_none());
        assert!(member_of(&listed("t8", "claude-code", None, None), None).is_none(), "in no space");
        assert!(member_of(&listed("t9", "claude-code", Some("B2C / x"), Some("tower")), None).is_none(), "the brain");
        assert!(member_of(&listed("ta", "tower", Some("B2C / x"), Some("tower-voice")), None).is_none());
        assert!(member_of(&listed("vps/t2", "claude-code", Some("B2C / x"), None), None).is_none(), "another machine");
        let mut ended = listed("tb", "claude-code", Some("B2C / x"), None);
        ended["state"] = json!("finished");
        assert!(member_of(&ended, None).is_none());
        // `hc get`'s record gives its terminal's creation time and its labelled name.
        let full = json!({ "address": "tc", "created_ms": 1234, "labels": { "where": "B2C / y", "tower.name": "b2c-sdk" },
            "status": { "state": "idle", "app": "claude-code" }, "foreground": { "cwd": "/w/sdk" } });
        let c = member_of(&listed("tc", "claude-code", Some("B2C / y"), None), Some(&full)).unwrap();
        assert_eq!((c.name.as_str(), c.folder.as_str(), c.created_ms, c.key()), ("b2c-sdk", "/w/sdk", Some(1234), "tc@1234".to_string()));
    }

    #[test]
    fn an_introduction_is_nobodys_task() {
        let intro = compose("B2C", &m("t1", "B2C"), &[m("t1", "B2C"), m("t2", "B2C")]);
        // As it came back from the hooks live: wrapped by the paste and by hc.
        let seen = format!("<pasted_content id=\"24c4\"> <hc from=\"t65\">{intro}</hc> </pasted_content>");
        assert!(is_introduction(&seen));
        assert_eq!(task_from_prompt(&seen), None);
        assert_eq!(task_from_prompt("<hc from=\"t65\">Fix the login bug in the api</hc>").as_deref(), Some("Fix the login bug in the api"));
        assert_eq!(task_from_prompt("Compare a<b and c>d").as_deref(), Some("Compare a<b and c>d"));
        assert_eq!(task_from_prompt("   "), None);
        // The task it had stays when its latest prompt was an introduction, across a restart too.
        let mut book = seeded();
        let mut g = groups(&[m("t1", "B2C"), m("t2", "B2C")]);
        let prompts: BTreeMap<&str, &str> = [("t1", "Fix the login bug"), ("t2", "Write the docs")].into_iter().collect();
        assert!(book.tasks_for(&mut g, |m| task_from_prompt(prompts[m.address.as_str()])));
        let path = std::env::temp_dir().join(format!("tower-introduce-tasks-{}.json", std::process::id()));
        book.save_to(&path).unwrap();
        let mut back = Book::load_from(&path);
        let _ = std::fs::remove_file(&path);
        let mut g = groups(&[m("t1", "B2C"), m("t2", "B2C")]);
        back.tasks_for(&mut g, |_| task_from_prompt(&seen));
        assert_eq!(g["B2C"][0].task, "Fix the login bug");
        assert_eq!(g["B2C"][1].task, "Write the docs");
        assert!(!compose("B2C", &g["B2C"][0], &g["B2C"]).contains("You are s-t2"), "no introduction inside an introduction");
        // A member that's gone takes its task with it.
        let mut g = groups(&[m("t1", "B2C")]);
        assert!(back.tasks_for(&mut g, |_| None));
        assert_eq!(back.tasks.len(), 1);
    }

    #[test]
    fn the_message_lists_the_others_and_says_how_to_reach_them() {
        let mut a = m("t1", "B2C");
        a.name = "b2c-mediaroom".into();
        let mut b = m("t2", "B2C");
        b.name = "b2c-sdk".into();
        b.task = "Add the chat widget's new event to the SDK".into();
        let c = Member { name: String::new(), folder: "/w/eyzflow".into(), ..m("t3", "B2C") };
        let text = compose("B2C", &a, &[a.clone(), b.clone(), c.clone()]);
        // Each as it is called everywhere else: its name within the space, or the project it works on.
        assert!(text.starts_with("[tower] You are mediaroom (t1) in the Herdr space \"B2C\". 2 other Claude sessions are open"), "{text}");
        assert!(text.contains("- sdk (t2), in /w/t2: working on: Add the chat widget's new event to the SDK"), "{text}");
        assert!(text.contains("- eyzflow (t3), in /w/eyzflow: no task reported yet"), "{text}");
        assert!(!text.contains("Claude in "), "{text}");
        let unnamed = Member { name: String::new(), folder: "/Users/me/Work/eyz/eyz-mediaroom-serverless".into(), ..m("t4", "B2C") };
        assert_eq!((unnamed.label(), Member { name: String::new(), folder: String::new(), ..m("t5", "B2C") }.label()), ("mediaroom serverless".to_string(), "the session at t5".to_string()));
        assert!(!text.contains("(t1),"), "not itself");
        assert!(text.contains("tower queue <address> \"<complete message>\"") && text.contains("hc read <address> --lines 60"));
        assert!(text.contains("the tower adds your name to it"), "{text}");
        assert!(text.contains("ask for changes in their layer instead of editing their repos"));
        assert!(compose("B2C", &a, &[a.clone(), b]).contains("1 other Claude session is open"));
    }
}
