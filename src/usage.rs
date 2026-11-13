//! Each Claude account's usage and who runs on it: the state's `accounts`
//! (the Mac's windows and the phone show it), and `tower accounts --usage`.
//!
//! - **The board** (`board`): every account the claude-accounts mod lists,
//!   the default first, with its two usage windows as the mod last recorded
//!   them (`cca --porcelain`'s `U` lines: percent used, when it resets; a
//!   window past its reset reads as 0 %, as `cca ls` reads it), and what the
//!   tower found (`crate::failover`): out of usage until when, and why. With
//!   it, the recent moves off an account, the sessions moving now and those
//!   waiting for an account, and the account a new session starts on.
//! - **The seats** (`seats`): who runs where, from the state the daemon
//!   writes: the main tower, then each open space with its own tower and
//!   its sessions, then the sessions outside any space; each with its
//!   account, and the move that put it there when the failover moved it.
//!
//! The daemon looks again every `LOOK` (and at once when the failover
//! changes something), and writes the board into the state only when it
//! changed: a window's percent moves as the agents use it, so the windows
//! follow within seconds.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::accounts::{Account, Usage, Window};
use crate::failover::{self, Book, Moved};

/// How often the daemon reads the accounts' usage again.
pub const LOOK: Duration = Duration::from_secs(15);
/// The moves the board carries: the latest, from the last week.
const MOVES_KEPT: usize = 20;
const MOVES_FOR_MS: u64 = 7 * 86_400_000;

/// One account, as the board shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Line {
    pub name: String,
    pub email: String,
    /// The mod's default (the first it lists).
    pub default: bool,
    /// The 5-hour and 7-day windows (None: the mod recorded none).
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Until when it has no usage (the tower found it out, or a window is
    /// at 100 %), and why ("5-hour limit"). None: it has usage.
    pub out_until_ms: Option<u64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub out_why: String,
}

impl Line {
    /// The window nearest its limit, for a line that has room for one: the
    /// 5-hour one unless the 7-day one is fuller, or the only one Claude
    /// Code reports (it sends a window only while that window applies).
    pub fn fullest(&self) -> Option<(&'static str, &Window)> {
        match (&self.five_hour, &self.seven_day) {
            (Some(f), Some(s)) if s.used > f.used => Some(("7d", s)),
            (Some(f), _) => Some(("5h", f)),
            (None, Some(s)) => Some(("7d", s)),
            (None, None) => None,
        }
    }
}

/// A session waiting for an account: every one was out when it hit its limit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Waits {
    pub address: String,
    pub account: String,
    pub since_ms: u64,
    /// "5-hour limit".
    pub why: String,
}

/// The state's `accounts`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Board {
    pub list: Vec<Line>,
    /// The account a session started without one named starts on (the
    /// default, unless the tower found it out).
    pub starts_on: Option<String>,
    /// The latest moves off an account, oldest first.
    pub moves: Vec<Moved>,
    /// Sessions moving to another account now (their addresses).
    pub moving: Vec<String>,
    pub waiting: Vec<Waits>,
}

/// A window as the user reads it now: past its reset, it is 0 % again.
fn live(w: &Option<Window>, now_ms: u64) -> Option<Window> {
    let w = w.as_ref()?;
    Some(match w.resets_ms {
        Some(r) if r <= now_ms => Window { used: 0.0, resets_ms: None },
        _ => w.clone(),
    })
}

/// Why an account is out: what the tower found, else the window at 100 %.
fn out_why(book: &Book, usage: Option<&Usage>, name: &str, now_ms: u64) -> String {
    if let Some(o) = book.out.get(name).filter(|o| failover::out_until(o, now_ms).is_some()) {
        return o.limit.what.clone();
    }
    let full = |w: &Option<Window>| w.as_ref().is_some_and(|w| w.used >= 100.0 && w.resets_ms.is_none_or(|r| r > now_ms));
    match usage {
        Some(u) if full(&u.seven_day) => "weekly limit".into(),
        Some(u) if full(&u.five_hour) => "5-hour limit".into(),
        _ => String::new(),
    }
}

/// The board, from the accounts the mod lists with their usage, the
/// failover's book and the sessions it is moving now.
pub fn board(list: &[Account], usage: &BTreeMap<String, Usage>, book: &Book, moving: &[String], now_ms: u64) -> Board {
    let lines = list
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let u = usage.get(&a.name);
            let out = failover::account_out(book, u, &a.name, now_ms);
            Line {
                name: a.name.clone(),
                email: a.email.clone(),
                default: i == 0,
                five_hour: u.and_then(|u| live(&u.five_hour, now_ms)),
                seven_day: u.and_then(|u| live(&u.seven_day, now_ms)),
                out_until_ms: out,
                out_why: if out.is_some() { out_why(book, u, &a.name, now_ms) } else { String::new() },
            }
        })
        .collect();
    // As `accounts::default_of` picks it: the first the tower hasn't found out.
    let starts_on = list.iter().find(|a| failover::account_out(book, None, &a.name, now_ms).is_none()).or(list.first()).map(|a| a.name.clone());
    let mut moves: Vec<Moved> = book.moves.iter().filter(|m| m.at_ms + MOVES_FOR_MS > now_ms).cloned().collect();
    if moves.len() > MOVES_KEPT {
        moves.drain(..moves.len() - MOVES_KEPT);
    }
    let mut moving = moving.to_vec();
    moving.sort();
    let waiting = book.waiting.iter().map(|w| Waits { address: w.address.clone(), account: w.account.clone(), since_ms: w.since_ms, why: w.limit.what.clone() }).collect();
    Board { list: lines, starts_on, moves, moving, waiting }
}

/// The board as the state carries it (None, from a daemon that doesn't
/// write one: the windows then show the accounts without their usage).
pub fn of_state(state: &Value) -> Option<Board> {
    let v = state.get("accounts").filter(|v| v.is_object())?;
    serde_json::from_value(v.clone()).ok()
}

// ---- who runs where ----

/// A session or a tower, with the account it runs on.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Seat {
    pub address: String,
    pub name: String,
    /// A tower (the main one, or a space's own), not a session of the work.
    pub tower: bool,
    pub status: String,
    /// None: not known (Codex, another machine, not looked at yet).
    pub account: Option<String>,
    /// The failover's latest move of it, while it still runs where that put it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<Moved>,
    pub moving: bool,
    pub waiting: bool,
}

/// The seats of one place: the main tower, a space, or outside every space.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Group {
    /// The space (None: the main tower, or the sessions outside any space).
    pub space: Option<String>,
    /// The main tower's own group.
    pub main: bool,
    pub seats: Vec<Seat>,
}

impl Group {
    pub fn title(&self) -> String {
        match (&self.space, self.main) {
            (Some(s), _) => s.clone(),
            (None, true) => "Main tower".into(),
            (None, false) => "Outside spaces".into(),
        }
    }
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// A tower's own terminal (`tower`, a space's `tower-…`): not a session of the work.
fn is_tower(name: &str) -> bool {
    name == "tower" || name.starts_with("tower-")
}

/// Every seat the state knows, grouped: the main tower first, then the open
/// spaces in the state's order (each with its own tower first), then the
/// sessions outside any space. `board`: the moves, the moving, the waiting.
pub fn seats(state: &Value, board: Option<&Board>) -> Vec<Group> {
    let empty = Board::default();
    let board = board.unwrap_or(&empty);
    let seat = |address: String, name: String, tower: bool, status: String, account: Option<String>| {
        // The latest move of this terminal, while it still runs on the account that move put it on.
        let moved = board.moves.iter().rev().find(|m| m.address == address).filter(|m| m.how != "moved" || account.as_ref().is_none_or(|a| *a == m.to)).cloned();
        Seat {
            moving: board.moving.contains(&address),
            waiting: board.waiting.iter().any(|w| w.address == address),
            address,
            name,
            tower,
            status,
            account,
            moved,
        }
    };
    let brains: Vec<&Value> = state["brains"].as_array().into_iter().flatten().collect();
    let brain_seat = |b: &Value| {
        let name = text(&b["name"]).unwrap_or_else(|| "tower".into());
        seat(text(&b["address"]).unwrap_or_default(), name, true, text(&b["status"]).unwrap_or_default(), text(&b["account"]))
    };
    let mut groups: Vec<Group> = vec![];
    let mut placed: Vec<String> = vec![];
    if let Some(main) = brains.iter().find(|b| b["space"].is_null()) {
        let s = brain_seat(main);
        placed.push(s.address.clone());
        groups.push(Group { space: None, main: true, seats: vec![s] });
    }
    for m in state["missions"]["spaces"].as_array().into_iter().flatten() {
        let Some(space) = text(&m["name"]) else { continue };
        let mut g = Group { space: Some(space.clone()), main: false, seats: vec![] };
        // Its own tower: as the brains list it (with its account), else as the mission has it.
        let own = brains.iter().find(|b| b["space"].as_str().is_some_and(|s| s.trim().eq_ignore_ascii_case(&space)));
        match own {
            Some(b) => g.seats.push(brain_seat(b)),
            None if m["tower"]["status"].as_str().is_some_and(|s| !s.is_empty() && s != "off") => {
                let t = &m["tower"];
                g.seats.push(seat(text(&t["address"]).unwrap_or_default(), text(&t["name"]).unwrap_or_default(), true, text(&t["status"]).unwrap_or_default(), text(&t["account"])));
            }
            None => {}
        }
        for s in m["sessions"].as_array().into_iter().flatten() {
            let name = text(&s["name"]).unwrap_or_default();
            let Some(address) = text(&s["address"]) else { continue };
            if is_tower(&name) || placed.contains(&address) || g.seats.iter().any(|x| x.address == address) {
                continue;
            }
            g.seats.push(seat(address, name, false, text(&s["status"]).unwrap_or_default(), text(&s["account"])));
        }
        placed.extend(g.seats.iter().map(|s| s.address.clone()));
        groups.push(g);
    }
    let brain_at: Vec<String> = brains.iter().filter_map(|b| text(&b["address"])).collect();
    let outside: Vec<Seat> = state["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let address = text(&a["address"])?;
            (!placed.contains(&address) && !brain_at.contains(&address) && !address.contains('/'))
                .then(|| seat(address, text(&a["name"]).unwrap_or_default(), false, text(&a["status"]).unwrap_or_default(), text(&a["account"])))
        })
        .collect();
    if !outside.is_empty() {
        groups.push(Group { space: None, main: false, seats: outside });
    }
    groups
}

/// How many sessions and towers run on `account`.
pub fn on(groups: &[Group], account: &str) -> (usize, usize) {
    let all = groups.iter().flat_map(|g| &g.seats).filter(|s| s.account.as_deref() == Some(account));
    all.fold((0, 0), |(s, t), x| if x.tower { (s, t + 1) } else { (s + 1, t) })
}

// ---- words ----

/// "42%", rounded as `cca ls` rounds it.
pub fn pct(used: f64) -> String {
    format!("{:.0}%", used.clamp(0.0, 999.0))
}

/// "5h 42% · resets 15:00": one window in a line.
pub fn window_words(label: &str, w: Option<&Window>, now_ms: u64) -> String {
    match w {
        None => format!("{label} –"),
        Some(w) => match w.resets_ms {
            Some(r) => format!("{label} {} · resets {}", pct(w.used), failover::when(r, now_ms)),
            None => format!("{label} {}", pct(w.used)),
        },
    }
}

/// What a moved seat says: "moved from default 15:02 (5-hour limit)", or
/// why it couldn't.
pub fn moved_words(m: &Moved, now_ms: u64) -> String {
    let at = failover::when(m.at_ms, now_ms);
    if m.how == "moved" {
        format!("moved from {} at {at}", m.from)
    } else {
        let why = if m.why.is_empty() { String::new() } else { format!(": {}", m.why) };
        format!("couldn't move to {} at {at}{why}", m.to)
    }
}

/// An account's state in words: "out until 15:00 (5-hour limit)", "at its
/// limit", or "" while it has usage.
pub fn out_words(l: &Line, now_ms: u64) -> String {
    match l.out_until_ms {
        Some(u) => {
            let why = if l.out_why.is_empty() { String::new() } else { format!(" ({})", l.out_why) };
            format!("out until {}{why}", failover::when(u, now_ms))
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failover::{Limit, Out, Waiting};
    use serde_json::json;

    const NOW: u64 = 1_794_600_000_000;

    fn accounts() -> Vec<Account> {
        vec![Account { name: "default".into(), email: "me@example.com".into() }, Account { name: "orca".into(), email: "ai@example.com".into() }]
    }

    fn win(used: f64, resets_ms: Option<u64>) -> Option<Window> {
        Some(Window { used, resets_ms })
    }

    #[test]
    fn the_board_has_each_account_with_its_usage_and_what_the_tower_found() {
        let mut usage = BTreeMap::new();
        usage.insert("default".to_string(), Usage { five_hour: win(42.4, Some(NOW + 3_600_000)), seven_day: win(12.0, Some(NOW - 1)) });
        let b = board(&accounts(), &usage, &Book::default(), &[], NOW);
        assert_eq!(b.list.len(), 2);
        let d = &b.list[0];
        assert!(d.default && !b.list[1].default, "the first listed is the default");
        assert_eq!(d.five_hour, win(42.4, Some(NOW + 3_600_000)));
        assert_eq!(d.seven_day, win(0.0, None), "a window past its reset reads as 0 %, as `cca ls` has it");
        assert_eq!((d.out_until_ms, d.out_why.as_str()), (None, ""));
        assert_eq!((b.list[1].five_hour.clone(), b.list[1].seven_day.clone()), (None, None), "nothing recorded: no windows");
        assert_eq!(b.starts_on.as_deref(), Some("default"));
        assert_eq!(d.fullest().map(|(l, w)| (l, w.used)), Some(("5h", 42.4)), "the 7-day one reset to 0");
        let weekly = Line { seven_day: win(99.0, Some(NOW + 1)), ..Default::default() };
        assert_eq!(weekly.fullest().map(|(l, w)| (l, w.used)), Some(("7d", 99.0)), "only the 7-day window reported (as Claude Code sends it now)");
        let both = Line { five_hour: win(20.0, None), seven_day: win(70.0, None), ..Default::default() };
        assert_eq!(both.fullest().map(|(l, _)| l), Some("7d"));
        assert_eq!(Line::default().fullest(), None);

        // The tower found the default out: it is out until then, and new sessions start on orca.
        let mut book = Book::default();
        book.out.insert("default".into(), Out { since_ms: NOW - 60_000, until_ms: Some(NOW + 7_200_000), limit: Limit { what: "5-hour limit".into(), ..Default::default() } });
        book.waiting.push(Waiting { address: "t9".into(), name: "w".into(), account: "orca".into(), since_ms: NOW - 5, limit: Limit { what: "weekly limit".into(), ..Default::default() }, ..Default::default() });
        let b = board(&accounts(), &usage, &book, &["t3".into()], NOW);
        assert_eq!((b.list[0].out_until_ms, b.list[0].out_why.as_str()), (Some(NOW + 7_200_000), "5-hour limit"));
        assert_eq!(out_words(&b.list[0], NOW).split(" (").nth(1), Some("5-hour limit)"));
        assert_eq!(b.starts_on.as_deref(), Some("orca"));
        assert_eq!(b.moving, ["t3"]);
        assert_eq!(b.waiting, [Waits { address: "t9".into(), account: "orca".into(), since_ms: NOW - 5, why: "weekly limit".into() }]);

        // A window at 100 % that hasn't reset: out until its reset, by that window.
        let mut spent = BTreeMap::new();
        spent.insert("orca".to_string(), Usage { five_hour: win(100.0, Some(NOW + 60_000)), seven_day: win(80.0, None) });
        let b = board(&accounts(), &spent, &Book::default(), &[], NOW);
        assert_eq!((b.list[1].out_until_ms, b.list[1].out_why.as_str()), (Some(NOW + 60_000), "5-hour limit"));
        assert_eq!(b.starts_on.as_deref(), Some("default"));
        assert_eq!(board(&[], &BTreeMap::new(), &Book::default(), &[], NOW), Board::default(), "no mod: an empty board");
    }

    #[test]
    fn the_board_keeps_the_last_weeks_moves() {
        let mut book = Book::default();
        for i in 0..30 {
            book.moves.push(Moved { at_ms: NOW - 1000 + i, address: format!("t{i}"), how: "moved".into(), ..Default::default() });
        }
        book.moves.insert(0, Moved { at_ms: NOW - 8 * 86_400_000, address: "old".into(), ..Default::default() });
        let b = board(&accounts(), &BTreeMap::new(), &book, &[], NOW);
        assert_eq!(b.moves.len(), MOVES_KEPT);
        assert_eq!(b.moves.last().unwrap().address, "t29", "the latest last");
        assert!(!b.moves.iter().any(|m| m.address == "old"));
    }

    #[test]
    fn a_board_goes_through_the_state_and_back() {
        let mut usage = BTreeMap::new();
        usage.insert("orca".to_string(), Usage { five_hour: win(7.0, Some(NOW + 1)), seven_day: None });
        let b = board(&accounts(), &usage, &Book::default(), &[], NOW);
        let state = json!({ "accounts": serde_json::to_value(&b).unwrap() });
        assert_eq!(of_state(&state), Some(b));
        assert_eq!(of_state(&json!({ "accounts": null })), None, "a daemon that writes none");
        assert_eq!(of_state(&json!({})), None);
    }

    fn state() -> Value {
        json!({
            "brains": [
                { "name": "tower", "space": null, "address": "t1", "status": "idle", "account": "default" },
                { "name": "tower-shop", "space": "Shop", "address": "t2", "status": "working", "account": "orca" }
            ],
            "missions": { "spaces": [
                { "name": "shop", "tower": { "name": "tower-shop", "status": "working", "address": "t2" },
                  "sessions": [
                      { "address": "t3", "name": "web", "status": "working", "account": "orca" },
                      { "address": "t4", "name": "api", "status": "idle", "account": "default" },
                      { "address": "t2", "name": "tower-shop", "status": "working", "account": "orca" }
                  ] },
                { "name": "docs", "tower": { "name": "tower-docs", "status": "off", "address": null },
                  "sessions": [{ "address": "t5", "name": "writer", "status": "done", "account": null }] }
            ], "quiet": [] },
            "agents": [
                { "address": "t3", "name": "web", "status": "working", "account": "orca" },
                { "address": "t6", "name": "loose", "status": "idle", "account": "default" },
                { "address": "vps/t1", "name": "far", "status": "idle", "account": null }
            ]
        })
    }

    #[test]
    fn every_tower_and_session_has_its_seat_with_its_account() {
        let g = seats(&state(), None);
        let shape: Vec<(String, Vec<(&str, bool, Option<&str>)>)> =
            g.iter().map(|g| (g.title(), g.seats.iter().map(|s| (s.address.as_str(), s.tower, s.account.as_deref())).collect())).collect();
        assert_eq!(
            shape,
            [
                ("Main tower".to_string(), vec![("t1", true, Some("default"))]),
                ("shop".to_string(), vec![("t2", true, Some("orca")), ("t3", false, Some("orca")), ("t4", false, Some("default"))]),
                ("docs".to_string(), vec![("t5", false, None)]),
                ("Outside spaces".to_string(), vec![("t6", false, Some("default"))]),
            ],
            "the main tower, each space with its own tower first (a tower that is off has no seat), then the rest of this machine"
        );
        assert_eq!(on(&g, "orca"), (1, 1));
        assert_eq!(on(&g, "default"), (2, 1));
        // A tower the brains don't list (an older daemon): as its mission has it.
        let mut s = state();
        s["brains"] = json!([]);
        let g = seats(&s, None);
        assert_eq!(g[0].seats[0].address, "t2");
        assert_eq!(g[0].seats[0].account, None);
    }

    #[test]
    fn a_moved_seat_carries_its_move_while_it_runs_where_the_move_put_it() {
        let mut usage = BTreeMap::new();
        usage.insert("orca".to_string(), Usage::default());
        let mut book = Book::default();
        let mv = |address: &str, from: &str, to: &str, how: &str| Moved {
            at_ms: NOW - 60_000,
            address: address.into(),
            name: address.into(),
            from: from.into(),
            to: to.into(),
            how: how.into(),
            ..Default::default()
        };
        book.moves.push(mv("t3", "default", "orca", "moved"));
        book.moves.push(mv("t4", "orca", "default", "moved"));
        book.moves.push(mv("t4", "default", "orca", "moved"));
        book.moves.push(mv("t2", "default", "orca", "moved"));
        book.moves.push(mv("t5", "default", "orca", "failed"));
        let b = board(&accounts(), &usage, &book, &["t4".into()], NOW);
        let g = seats(&state(), Some(&b));
        let by = |a: &str| g.iter().flat_map(|g| &g.seats).find(|s| s.address == a).unwrap().clone();
        assert_eq!(by("t3").moved.map(|m| (m.from, m.to)), Some(("default".into(), "orca".into())), "it runs on orca, where the move put it");
        assert_eq!(by("t2").moved.map(|m| m.from), Some("default".into()), "a tower's move shows too");
        assert_eq!(by("t4").moved, None, "it moved to orca last, but runs on default: that move is over");
        assert!(by("t4").moving);
        assert_eq!(by("t5").moved.map(|m| m.how), Some("failed".into()), "a move that failed is said too");
        assert_eq!(by("t1").moved, None);
        let m = by("t3").moved.unwrap();
        assert!(moved_words(&m, NOW).starts_with("moved from default at "), "{}", moved_words(&m, NOW));
    }

    #[test]
    fn words_for_the_windows() {
        assert_eq!(pct(42.4), "42%");
        assert_eq!(pct(99.6), "100%");
        assert_eq!(window_words("5h", None, NOW), "5h –");
        assert_eq!(window_words("7d", Some(&Window { used: 3.0, resets_ms: None }), NOW), "7d 3%");
        assert!(window_words("5h", Some(&Window { used: 50.0, resets_ms: Some(NOW + 60_000) }), NOW).starts_with("5h 50% · resets "));
    }
}
