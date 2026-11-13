//! A Claude account at its usage limit, and the work moved off it.
//!
//! - **The limit.** A turn that ends on an API error fires Claude Code's
//!   StopFailure hook (`error`, and the message it showed as
//!   `last_assistant_message`): "You've hit your 5-hour limit · resets 3pm
//!   (Europe/Berlin)", "You're out of usage credits", the older "Claude AI
//!   usage limit reached|<reset>". `limit_of` reads one of those as the
//!   account's usage limit; a plain rate limit (Claude Code retries those)
//!   is none. The mod's own record of each account's usage (`cca
//!   --porcelain`'s `U` lines) says the same of a window at 100%.
//! - **Out.** The account the session ran on is out until its reset (from
//!   the message, else the mod's record, else `UNKNOWN_OUT_MS` from now),
//!   kept in `failover.json` in the state folder.
//! - **Where to.** `pick`: the accounts in the mod's order (the default
//!   first), less the one it ran on, those out, and those not signed in.
//!   None left: every account is out, and the first back is the one whose
//!   reset comes first.
//! - **The move** (`crate::daemon::failover`): in place, in the session's
//!   own terminal, so its address, and with it its agenda tasks, queued
//!   instructions, watch and cards, stay what they were: Herdr keeps the
//!   terminal (`hc keep`), its `claude` is ended, and the shell Herdr leaves
//!   runs `cd <folder> && exec cca <account> -n <name> <flags> --resume
//!   <session>`. What it was doing is sent once more (`carry_on`), and the
//!   user is told in one line (`told`).
//! - **All out.** One decision (`card`) says when the first account
//!   resets; the sessions wait in the book, and the tower moves or
//!   continues each one as soon as an account has usage again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::accounts::{Account, Usage};

/// An account out of usage whose reset isn't known is tried again after this long.
pub const UNKNOWN_OUT_MS: u64 = 60 * 60_000;
/// The moves kept for `tower failover`.
const KEEP_MOVES: usize = 50;
/// The longest instruction carried over into the note a moved session gets.
const PROMPT_CHARS: usize = 1500;

// ---- the limit ----

/// A usage limit, as the message that ended a turn says it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    /// Which: "5-hour limit", "weekly limit", "usage credits", or "usage limit".
    pub what: String,
    /// When it resets, in the message's words ("3pm (Europe/Berlin)").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resets: String,
    /// The same, as a time (Unix ms), when it reads as one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_ms: Option<u64>,
}

/// Whether a failed turn (`error`: StopFailure's error type, `text`: the
/// message Claude Code showed) ended on the account's usage limit.
pub fn limit_of(error: &str, text: &str, now_ms: u64) -> Option<Limit> {
    let t = text.replace(['\u{2019}', '\u{2018}'], "'");
    let lower = t.to_lowercase();
    // The older form: "Claude AI usage limit reached|1700000000".
    if let Some(at) = lower.find("usage limit reached") {
        let epoch = t
            .get(at..)
            .unwrap_or("")
            .split('|')
            .nth(1)
            .and_then(|s| s.trim().split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|s| s.parse::<u64>().ok());
        let resets_ms =
            epoch.map(|s| if s > 100_000_000_000 { s } else { s * 1000 }).or_else(|| resets_words(&t).and_then(|w| reset_ms(&w, now_ms)));
        return Some(Limit { what: "usage limit".into(), resets: resets_words(&t).unwrap_or_default(), resets_ms });
    }
    let what = ["you've hit your ", "you have hit your ", "you've reached your ", "you have reached your "].iter().find_map(|p| {
        let at = lower.find(p)? + p.len();
        let rest = t.get(at..)?;
        let end = rest.find(['·', '\u{2219}', '.', '\n', '(']).unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    });
    let what = match what {
        Some(w) if w.to_lowercase().contains("limit") || w.to_lowercase().contains("budget") || w.to_lowercase().contains("usage") => w,
        Some(_) => return None,
        None if lower.contains("out of usage") || lower.contains("out of extra usage") => "usage credits".to_string(),
        // "rate_limit" alone is a busy API, which Claude Code waits out itself.
        None if error == "rate_limit" && (lower.contains("usage limit") || lower.contains("limit reached")) => "usage limit".to_string(),
        None => return None,
    };
    let resets = resets_words(&t).unwrap_or_default();
    let resets_ms = if resets.is_empty() { None } else { reset_ms(&resets, now_ms) };
    Some(Limit { what, resets, resets_ms })
}

/// "… · resets 3pm (Europe/Berlin)" → "3pm (Europe/Berlin)".
fn resets_words(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let at = lower.find("resets ")? + "resets ".len();
    let rest = text.get(at..)?;
    let end = rest.find(['·', '\u{2219}', '\n']).unwrap_or(rest.len());
    let words = rest[..end].trim().trim_end_matches('.').trim();
    (!words.is_empty()).then(|| words.to_string())
}

const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// When reset words fall, in local time: the next such moment after `now`.
/// "3pm", "3:30pm", "15:00", "Mon 9am", "Nov 18, 9am", "Nov 18 at 9:30am";
/// a zone in parentheses is the Mac's own (it isn't converted).
pub fn reset_ms(words: &str, now_ms: u64) -> Option<u64> {
    let words = words.split('(').next().unwrap_or("").to_lowercase();
    let tokens: Vec<&str> =
        words.split(|c: char| c.is_whitespace() || c == ',').filter(|w| !w.is_empty() && *w != "at" && *w != "on").collect();
    let mut clock: Option<(i32, i32)> = None;
    let mut weekday: Option<i32> = None;
    let mut month: Option<i32> = None;
    let mut mday: Option<i32> = None;
    for (i, w) in tokens.iter().enumerate() {
        if let Some(c) = clock_of(w, tokens.get(i + 1).copied()) {
            clock = Some(c);
        } else if let Some(d) = DAYS.iter().position(|d| w.starts_with(d)) {
            weekday = Some(d as i32);
        } else if let Some(m) = MONTHS.iter().position(|m| w.starts_with(m)) {
            month = Some(m as i32);
        } else if let Ok(n) = w.trim_end_matches(|c: char| c.is_alphabetic()).parse::<i32>()
            && (1..=31).contains(&n)
            && month.is_some()
        {
            mday = Some(n);
        }
    }
    let (hour, min) = clock.unwrap_or((0, 0));
    if clock.is_none() && month.is_none() && weekday.is_none() {
        return None;
    }
    let secs = (now_ms / 1000) as libc::time_t;
    // SAFETY: localtime_r fills the struct we own; mktime reads it.
    let at = |days: i32, mon: Option<i32>, day: Option<i32>, year_plus: i32| unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        (tm.tm_hour, tm.tm_min, tm.tm_sec) = (hour, min, 0);
        if let (Some(m), Some(d)) = (mon, day) {
            (tm.tm_mon, tm.tm_mday) = (m, d);
        }
        tm.tm_year += year_plus;
        tm.tm_mday += days;
        tm.tm_isdst = -1;
        let t = libc::mktime(&mut tm);
        (t > 0).then_some(t as u64 * 1000)
    };
    if let Some(m) = month {
        let day = mday.unwrap_or(1);
        let this = at(0, Some(m), Some(day), 0)?;
        return Some(if this > now_ms { this } else { at(0, Some(m), Some(day), 1)? });
    }
    if let Some(d) = weekday {
        // SAFETY: as above.
        let today = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&secs, &mut tm);
            tm.tm_wday
        };
        let mut ahead = (d - today).rem_euclid(7);
        let mut t = at(ahead, None, None, 0)?;
        if t <= now_ms {
            ahead += 7;
            t = at(ahead, None, None, 0)?;
        }
        return Some(t);
    }
    let t = at(0, None, None, 0)?;
    Some(if t > now_ms { t } else { at(1, None, None, 0)? })
}

/// "3pm", "3:30pm", "15:00", "9" followed by "am": (hour, minute).
fn clock_of(w: &str, next: Option<&str>) -> Option<(i32, i32)> {
    let (num, half) = if let Some(n) = w.strip_suffix("am") {
        (n, Some(false))
    } else if let Some(n) = w.strip_suffix("pm") {
        (n, Some(true))
    } else if matches!(next, Some("am") | Some("pm")) {
        (w, Some(next == Some("pm")))
    } else {
        (w, None)
    };
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit() || c == ':') {
        return None;
    }
    let (h, m) = match num.split_once(':') {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        // A bare number is a clock only with am/pm ("18" alone is a day).
        None if half.is_some() => (num.parse::<i32>().ok()?, 0),
        None => return None,
    };
    if !(0..60).contains(&m) {
        return None;
    }
    let h = match half {
        Some(pm) if (1..=12).contains(&h) => (h % 12) + if pm { 12 } else { 0 },
        Some(_) => return None,
        None if (0..24).contains(&h) => h,
        None => return None,
    };
    Some((h, m))
}

/// A time as the user reads it: "15:00", or "Mon 15:00" when not today.
pub fn when(ms: u64, now_ms: u64) -> String {
    let tm = |ms: u64| {
        let secs = (ms / 1000) as libc::time_t;
        // SAFETY: localtime_r fills the struct we own.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&secs, &mut tm);
            tm
        }
    };
    let (t, n) = (tm(ms), tm(now_ms));
    let clock = format!("{:02}:{:02}", t.tm_hour, t.tm_min);
    if (t.tm_year, t.tm_yday) == (n.tm_year, n.tm_yday) {
        return clock;
    }
    let day = DAYS[t.tm_wday.clamp(0, 6) as usize];
    let day = format!("{}{}", day[..1].to_uppercase(), &day[1..]);
    if ms.saturating_sub(now_ms) < 6 * 86_400_000 {
        format!("{day} {clock}")
    } else {
        format!("{day} {} {}, {clock}", t.tm_mday, MONTHS[t.tm_mon.clamp(0, 11) as usize])
    }
}

// ---- what is kept ----

/// An account the tower found out of usage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Out {
    pub since_ms: u64,
    /// When it has usage again (None: not known, see `UNKNOWN_OUT_MS`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    pub limit: Limit,
}

/// A session moved to another account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Moved {
    pub at_ms: u64,
    pub address: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// The tower it is (`tower`, `tower-<space>`), when it is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tower: Option<String>,
    pub from: String,
    pub to: String,
    pub session_id: String,
    /// `moved`, or `failed` (and why).
    pub how: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// A session that waits for an account: every one was out when it hit its limit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Waiting {
    pub address: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tower: Option<String>,
    /// The account it runs on.
    pub account: String,
    pub since_ms: u64,
    pub limit: Limit,
    /// What it was doing when it stopped: sent again when it goes on.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt: String,
}

impl Waiting {
    /// How it is called on the card: a tower by its space.
    pub fn called(&self) -> String {
        called(&self.name, self.space.as_deref(), self.tower.as_deref())
    }
}

/// "fix-login (sandbox)", "the sandbox tower", "the tower".
pub fn called(name: &str, space: Option<&str>, tower: Option<&str>) -> String {
    match (tower, space) {
        (Some(_), Some(sp)) => format!("the {sp} tower"),
        (Some(_), None) => "the tower".into(),
        (None, Some(sp)) if !sp.eq_ignore_ascii_case(name) => format!("{name} ({sp})"),
        _ => name.to_string(),
    }
}

/// `failover.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    /// By account name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub out: BTreeMap<String, Out>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub moves: Vec<Moved>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<Waiting>,
    /// The card saying every account is out, while it is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// The user said to leave the waiting ones stopped: nothing goes on by itself.
    pub hold: bool,
    /// The card was answered or put away: not raised again until one more session waits.
    pub card_closed: bool,
}

impl Book {
    /// An account found out (again): until the later of what is known.
    pub fn mark_out(&mut self, account: &str, limit: &Limit, until_ms: Option<u64>, now_ms: u64) {
        let o = self.out.entry(account.to_string()).or_default();
        if o.until_ms.is_none_or(|u| u <= now_ms) || o.since_ms == 0 {
            o.since_ms = now_ms;
        }
        o.until_ms = match (o.until_ms.filter(|u| *u > now_ms), until_ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => b.or(a),
        };
        o.limit = limit.clone();
    }

    /// What is over: accounts back, moves past `KEEP_MOVES`.
    pub fn tidy(&mut self, now_ms: u64) -> bool {
        let before = (self.out.len(), self.moves.len());
        self.out.retain(|_, o| out_until(o, now_ms).is_some());
        if self.moves.len() > KEEP_MOVES {
            let cut = self.moves.len() - KEEP_MOVES;
            self.moves.drain(..cut);
        }
        before != (self.out.len(), self.moves.len())
    }

    pub fn moved(&mut self, m: Moved) {
        self.moves.push(m);
        self.tidy(crate::history::now_ms());
    }
}

/// Until when an account found out stays out (None: it is back).
pub fn out_until(o: &Out, now_ms: u64) -> Option<u64> {
    let until = o.until_ms.unwrap_or(o.since_ms + UNKNOWN_OUT_MS);
    (until > now_ms).then_some(until)
}

/// Until when an account has no usage: what the tower found, or what the
/// mod recorded (the later). None: it has usage.
pub fn account_out(book: &Book, usage: Option<&Usage>, name: &str, now_ms: u64) -> Option<u64> {
    let found = book.out.get(name).and_then(|o| out_until(o, now_ms));
    let recorded = usage.and_then(|u| u.spent_until(now_ms));
    found.max(recorded)
}

pub fn path() -> PathBuf {
    // Tests keep theirs in a temporary folder, never in the user's state folder.
    if cfg!(test) {
        return std::env::temp_dir().join(format!("herder-tower-test-{}", std::process::id())).join("failover.json");
    }
    crate::config::state_dir().join("failover.json")
}

pub fn load_from(path: &Path) -> Book {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_to(path: &Path, b: &Book) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(b)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Whether the tower hasn't found this account out of usage.
pub fn usable(account: &str) -> bool {
    account_out(&load_from(&path()), None, account, crate::history::now_ms()).is_none()
}

/// The first account of `list` the tower hasn't found out of usage (the
/// default first). None: every one is out, or there are none.
pub fn first_available(list: &[Account], now_ms: u64) -> Option<String> {
    let book = load_from(&path());
    list.iter().find(|a| account_out(&book, None, &a.name, now_ms).is_none()).map(|a| a.name.clone())
}

// ---- where to ----

/// Where a session at its limit goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// This account has usage left.
    To(String),
    /// Every account is out. The first back, and when (None: not known).
    AllOut { first: Option<(String, u64)> },
    /// No accounts are known (the mod isn't installed): nowhere to go.
    NoAccounts,
}

/// The account a session on `from` goes to: the first in the mod's order
/// (the default first) that isn't `from`, isn't out, and is signed in.
pub fn pick(list: &[Account], usage: &BTreeMap<String, Usage>, book: &Book, from: Option<&str>, now_ms: u64) -> Pick {
    if list.is_empty() {
        return Pick::NoAccounts;
    }
    let signed_in = |a: &Account| a.email.trim() != "not signed in";
    let out = |a: &Account| account_out(book, usage.get(&a.name), &a.name, now_ms);
    if let Some(a) = list.iter().find(|a| Some(a.name.as_str()) != from && signed_in(a) && out(a).is_none()) {
        return Pick::To(a.name.clone());
    }
    let first = list
        .iter()
        .filter(|a| signed_in(a) || Some(a.name.as_str()) == from)
        .filter_map(|a| out(a).map(|u| (a.name.clone(), u)))
        .min_by_key(|(_, u)| *u);
    Pick::AllOut { first }
}

// ---- the move ----

/// The command line that resumes a session's conversation on `account`:
/// the tower's own folders first when they aren't the usual ones (a tower
/// run beside the real one: its hooks reach it), then `cca <account> -n
/// <name> <flags> --resume <session>`.
pub fn resume_argv(cca: &str, account: &str, name: &str, flags: &[String], session_id: &str, env: &[(String, String)]) -> Vec<String> {
    let mut a: Vec<String> = vec![];
    if !env.is_empty() {
        a.push("env".into());
        a.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    }
    a.extend([cca.to_string(), account.to_string()]);
    if !name.trim().is_empty() {
        a.extend(["-n".into(), name.to_string()]);
    }
    a.extend(flags.iter().cloned());
    a.extend(["--resume".into(), session_id.to_string()]);
    a
}

/// What is typed into the shell Herdr left: into its folder, and the
/// resume in the shell's place (so the terminal ends with it, as before).
pub fn resume_line(cwd: &str, argv: &[String]) -> String {
    let cmd = argv.iter().map(|a| crate::reopen::quote(a)).collect::<Vec<_>>().join(" ");
    if cwd.is_empty() { format!("exec {cmd}") } else { format!("cd {} && exec {cmd}", crate::reopen::quote(cwd)) }
}

/// The start and the end of a long instruction.
fn clip(text: &str, n: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= n {
        return text.to_string();
    }
    let head: String = text.chars().take(n * 2 / 3).collect();
    let tail: String = text.chars().skip(count - n / 3).collect();
    format!("{head} … {tail}")
}

/// What a moved session is told once it runs again: why, and to go on
/// with what it was doing (`prompt`: its last instruction, when known).
pub fn carry_on(from: &str, to: &str, limit: &Limit, prompt: &str) -> String {
    let mut s = format!(
        "The Claude account {from} hit its {}, so the tower moved you to the {to} account: this is the same conversation, resumed where it stopped. Carry on with what you were doing; nothing of it is done unless you did it.",
        limit.what
    );
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        s.push_str(&format!(" Your last instruction was:\n\n{}", clip(prompt, PROMPT_CHARS)));
    }
    s
}

/// Who, as a sentence opens with it: "The sandbox tower", but a session's
/// name as it is spelled ("fix-login").
pub fn opening(who: &str) -> String {
    match who.strip_prefix("the ") {
        Some(rest) => format!("The {rest}"),
        None => who.to_string(),
    }
}

/// The one line the user is told of a move.
pub fn told(who: &str, from: &str, to: &str, limit: &Limit, until_ms: Option<u64>, now_ms: u64) -> String {
    let back = match until_ms {
        Some(u) => format!(" (back at {})", when(u, now_ms)),
        None if !limit.resets.is_empty() => format!(" (resets {})", limit.resets),
        None => String::new(),
    };
    let who = opening(who);
    format!("{who} moved from the {from} account to {to}: {from} hit its {}{back}.", limit.what)
}

/// What a session that waited on its own account is told once that one is back.
pub fn carry_back(account: &str, limit: &Limit, prompt: &str) -> String {
    let mut s = format!(
        "The Claude account {account} has usage again after its {}. Carry on with what you were doing when it stopped.",
        limit.what
    );
    let prompt = prompt.trim();
    if !prompt.is_empty() {
        s.push_str(&format!(" Your last instruction was:\n\n{}", clip(prompt, PROMPT_CHARS)));
    }
    s
}

/// The line the user is told when one that waited goes on where it is.
pub fn told_back(who: &str, account: &str) -> String {
    let who = opening(who);
    format!("{who} goes on: the {account} account has usage again.")
}

/// The card while every account is out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Card {
    pub title: String,
    pub detail: String,
    /// (label, why, code, the tower's pick).
    pub options: Vec<(String, String, String, bool)>,
}

/// The code an option of the card carries (`decisions::Opt::text`).
pub const WAIT: &str = "accounts:wait";
pub const STOP: &str = "accounts:stop";

/// The card for the sessions that wait (None: none wait). `first`: the
/// first account back, and when; `out`: every account and until when.
pub fn card(waiting: &[Waiting], first: Option<&(String, u64)>, out: &[(String, Option<u64>)], now_ms: u64) -> Option<Card> {
    if waiting.is_empty() {
        return None;
    }
    let names: Vec<String> = waiting.iter().map(Waiting::called).collect();
    let who =
        if names.len() == 1 { format!("{} waits", names[0]) } else { format!("{} sessions wait ({})", names.len(), names.join(", ")) };
    let first_line = match first {
        Some((a, u)) => format!("The first back is {a}, at {}.", when(*u, now_ms)),
        None => "When the first one resets isn't known.".to_string(),
    };
    let title = format!("Every Claude account is out of usage: {who}. {first_line}");
    let detail = out
        .iter()
        .map(|(a, u)| match u {
            Some(u) => format!("{a}: out until {}", when(*u, now_ms)),
            None => format!("{a}: out"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let at = first.map(|(a, u)| format!("{a} at {}", when(*u, now_ms))).unwrap_or_else(|| "the first reset".into());
    let options = vec![
        (
            format!("Go on at {}", first.map(|(_, u)| when(*u, now_ms)).unwrap_or_else(|| "the reset".into())),
            format!("the tower moves or continues each one as soon as an account has usage again ({at})"),
            WAIT.to_string(),
            true,
        ),
        ("Leave them stopped".to_string(), "nothing goes on by itself: continue them yourself".to_string(), STOP.to_string(), false),
    ];
    Some(Card { title, detail, options })
}

/// What the card's answer asks: wait (true) or leave them stopped (false).
pub fn of_code(code: &str) -> Option<bool> {
    match code {
        WAIT => Some(true),
        STOP => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(name: &str) -> Account {
        Account { name: name.into(), email: format!("{name}@example.com") }
    }

    /// Unix ms of a local time today (hour, minute).
    fn today_at(now_ms: u64, h: i32, m: i32) -> u64 {
        let secs = (now_ms / 1000) as libc::time_t;
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&secs, &mut tm);
            (tm.tm_hour, tm.tm_min, tm.tm_sec) = (h, m, 0);
            tm.tm_isdst = -1;
            libc::mktime(&mut tm) as u64 * 1000
        }
    }

    #[test]
    fn a_usage_limit_is_read_off_the_message_a_plain_rate_limit_is_not() {
        let now = today_at(crate::history::now_ms(), 10, 0);
        let l = limit_of("rate_limit", "You've hit your 5-hour limit · resets 3pm (Europe/Berlin)", now).unwrap();
        assert_eq!((l.what.as_str(), l.resets.as_str()), ("5-hour limit", "3pm (Europe/Berlin)"));
        assert_eq!(l.resets_ms, Some(today_at(now, 15, 0)));
        let l = limit_of("", "You’ve hit your weekly limit ∙ resets Mon 9am", now).unwrap();
        assert_eq!(l.what, "weekly limit");
        assert!(l.resets_ms.is_some_and(|r| r > now && r - now <= 7 * 86_400_000));
        assert_eq!(
            limit_of("billing_error", "You're out of usage credits. Run /usage-credits to continue", now).unwrap().what,
            "usage credits"
        );
        let old = limit_of("", "Claude AI usage limit reached|1794600000", now).unwrap();
        assert_eq!(old.resets_ms, Some(1_794_600_000_000));
        // A busy API, a refusal, an answer that talks about limits: none.
        assert_eq!(limit_of("rate_limit", "API Error: Request rejected (429) · retrying", now), None);
        assert_eq!(limit_of("server_error", "API Error: 500", now), None);
        assert_eq!(limit_of("", "I raised the upload size limit to 10 MB.", now), None);
        assert_eq!(limit_of("", "You've hit your stride, nice", now), None);
    }

    #[test]
    fn reset_words_are_the_next_such_time() {
        let now = today_at(crate::history::now_ms(), 16, 0);
        assert_eq!(reset_ms("3pm", now), Some(today_at(now, 15, 0) + 86_400_000), "3pm has passed today: tomorrow's");
        assert_eq!(reset_ms("5:30pm (Europe/Berlin)", now), Some(today_at(now, 17, 30)));
        assert_eq!(reset_ms("21:15", now), Some(today_at(now, 21, 15)));
        assert!(reset_ms("Nov 18, 9am", now).is_some_and(|t| t > now));
        assert!(reset_ms("Nov 18 at 9:30am", now).is_some_and(|t| t > now));
        assert_eq!(reset_ms("soon", now), None);
        assert_eq!(clock_of("13pm", None), None);
        assert_eq!(clock_of("9", Some("am")), Some((9, 0)));
        assert_eq!(clock_of("18", None), None, "a bare number is no clock");
    }

    #[test]
    fn a_session_goes_to_the_first_account_with_usage_the_default_first() {
        let now = 1_000_000_000_000;
        let list = vec![acc("default"), acc("work"), acc("side")];
        let mut book = Book::default();
        let none = BTreeMap::new();
        assert_eq!(pick(&list, &none, &book, Some("default"), now), Pick::To("work".into()));
        assert_eq!(pick(&list, &none, &book, Some("work"), now), Pick::To("default".into()), "the default first");
        book.mark_out("default", &Limit { what: "5-hour limit".into(), ..Default::default() }, Some(now + 3_600_000), now);
        assert_eq!(pick(&list, &none, &book, Some("work"), now), Pick::To("side".into()), "the default is out");
        // Not signed in: never a place to go.
        let unsigned = vec![acc("default"), Account { name: "new".into(), email: "not signed in".into() }];
        assert_eq!(pick(&unsigned, &none, &book, Some("default"), now), Pick::AllOut { first: Some(("default".into(), now + 3_600_000)) });
        // What the mod recorded counts too.
        let mut usage = BTreeMap::new();
        usage.insert(
            "work".to_string(),
            Usage { five_hour: Some(crate::accounts::Window { used: 100.0, resets_ms: Some(now + 600_000) }), seven_day: None },
        );
        usage.insert(
            "side".to_string(),
            Usage { five_hour: Some(crate::accounts::Window { used: 100.0, resets_ms: Some(now - 1) }), seven_day: None },
        );
        assert_eq!(pick(&list, &usage, &book, Some("default"), now), Pick::To("side".into()), "side's window reset already");
        usage.get_mut("side").unwrap().seven_day = Some(crate::accounts::Window { used: 100.0, resets_ms: Some(now + 7_200_000) });
        assert_eq!(
            pick(&list, &usage, &book, Some("default"), now),
            Pick::AllOut { first: Some(("work".into(), now + 600_000)) },
            "work is back first"
        );
        assert_eq!(pick(&[], &usage, &book, None, now), Pick::NoAccounts);
        // An account out with no reset known is tried again after a while.
        let mut b = Book::default();
        b.mark_out("work", &Limit::default(), None, now);
        assert_eq!(account_out(&b, None, "work", now + 1), Some(now + UNKNOWN_OUT_MS));
        assert_eq!(account_out(&b, None, "work", now + UNKNOWN_OUT_MS), None);
        assert!(b.tidy(now + UNKNOWN_OUT_MS) && b.out.is_empty());
    }

    #[test]
    fn the_resume_runs_in_the_shells_place_on_the_other_account() {
        let flags = vec!["--model".to_string(), "claude-opus-5-5".to_string()];
        let argv = resume_argv("/u/cca", "orca", "fix-login", &flags, "s-1", &[]);
        assert_eq!(argv, ["/u/cca", "orca", "-n", "fix-login", "--model", "claude-opus-5-5", "--resume", "s-1"]);
        assert_eq!(resume_line("/w/my app", &argv), "cd '/w/my app' && exec /u/cca orca -n fix-login --model claude-opus-5-5 --resume s-1");
        let env = vec![("HERDER_TOWER_STATE".to_string(), "/tmp/t".to_string())];
        assert_eq!(
            resume_argv("cca", "default", "", &[], "s-2", &env),
            ["env", "HERDER_TOWER_STATE=/tmp/t", "cca", "default", "--resume", "s-2"]
        );
    }

    #[test]
    fn the_user_is_told_in_one_line_and_the_session_what_to_go_on_with() {
        let now = today_at(crate::history::now_ms(), 10, 0);
        let limit = Limit { what: "5-hour limit".into(), resets: "3pm".into(), resets_ms: Some(today_at(now, 15, 0)) };
        assert_eq!(
            told(&called("fix-login", Some("sandbox"), None), "default", "orca", &limit, limit.resets_ms, now),
            "fix-login (sandbox) moved from the default account to orca: default hit its 5-hour limit (back at 15:00)."
        );
        assert_eq!(
            told(&called("tower-sandbox", Some("sandbox"), Some("tower-sandbox")), "default", "orca", &limit, None, now),
            "The sandbox tower moved from the default account to orca: default hit its 5-hour limit (resets 3pm)."
        );
        let note = carry_on("default", "orca", &limit, "fix the login bug");
        assert!(
            note.contains("moved you to the orca account") && note.ends_with("Your last instruction was:\n\nfix the login bug"),
            "{note}"
        );
        assert!(!carry_on("default", "orca", &limit, " ").contains("last instruction"));
    }

    #[test]
    fn every_account_out_is_one_card_saying_when_the_first_is_back() {
        let now = today_at(crate::history::now_ms(), 10, 0);
        assert_eq!(card(&[], None, &[], now), None);
        let w = |name: &str, tower: Option<&str>| Waiting {
            address: "t1".into(),
            name: name.into(),
            space: Some("sandbox".into()),
            tower: tower.map(String::from),
            account: "default".into(),
            ..Default::default()
        };
        let first = ("orca".to_string(), today_at(now, 15, 0));
        let c = card(
            &[w("fix-login", None), w("tower-sandbox", Some("tower-sandbox"))],
            Some(&first),
            &[("default".into(), Some(today_at(now, 16, 0))), ("orca".into(), Some(first.1))],
            now,
        )
        .unwrap();
        assert_eq!(
            c.title,
            "Every Claude account is out of usage: 2 sessions wait (fix-login (sandbox), the sandbox tower). The first back is orca, at 15:00."
        );
        assert_eq!(c.detail, "default: out until 16:00\norca: out until 15:00");
        assert_eq!(
            c.options.iter().map(|o| (o.0.as_str(), o.2.as_str(), o.3)).collect::<Vec<_>>(),
            [("Go on at 15:00", WAIT, true), ("Leave them stopped", STOP, false)]
        );
        assert_eq!((of_code(WAIT), of_code(STOP), of_code("reopen:*")), (Some(true), Some(false), None));
        let unknown = card(&[w("fix-login", None)], None, &[("default".into(), None)], now).unwrap();
        assert!(unknown.title.ends_with("fix-login (sandbox) waits. When the first one resets isn't known."), "{}", unknown.title);
    }
}
