//! How long each space has been at work: the time at least one of its
//! sessions was working (a turn), or running a background job the tower
//! counts. Waiting on the user (a session blocked on them, none working) is
//! counted apart; idle time isn't counted. The space's own tower and its
//! PRs' reviewers aren't its sessions here.
//!
//! - **From status changes, never polled**: the `job` lines of the history
//!   (each session's status as the radar saw it change: working, blocked,
//!   done …) and its `bgjob` lines (a background job that ended, with when
//!   it began), read once at the start and then only what was added since
//!   (`Tracker::read`). The jobs running now come from the daemon's own list.
//! - **Per space** (`Active`): whether it is at work now and since when (this
//!   stretch: one unbroken run of work, across its sessions), today's active
//!   time, and the total since the space opened; the same for waiting on the
//!   user. `at_ms` is when that was so: a window adds what passed since to a
//!   space at work, so its timer runs between two states.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::history::Entry;

/// A session's open interval with no word from it for this long is closed at
/// its last word (a status the tower missed, the Mac asleep).
const STALE_MS: u64 = 6 * 60 * 60_000;

/// A space's active time, as the state, the windows and the phone carry it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Active {
    /// When these figures were so (the windows add what passed since to a space at work).
    pub at_ms: u64,
    /// A session of it works, or runs a background job, now.
    pub now: bool,
    /// When this stretch of work began (`now`): one unbroken run, across its sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// Active today (since local midnight), and since the space opened.
    pub today_ms: u64,
    pub total_ms: u64,
    /// Waiting on the user (a session blocked on them, none at work), now, today, in all.
    pub waiting_now: bool,
    pub waiting_today_ms: u64,
    pub waiting_total_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Working,
    Blocked,
    Other,
}

#[derive(Debug, Clone, Default)]
struct Session {
    space: Option<String>,
    /// Its current status and since when (None: nothing known, or not at work).
    state: Option<(State, u64)>,
    /// Its last word of any kind in the history.
    last_ms: u64,
}

/// Every session's intervals so far, kept per space, and where reading the history stopped.
#[derive(Debug, Default)]
pub struct Tracker {
    path: PathBuf,
    offset: u64,
    sessions: HashMap<String, Session>,
    /// Closed intervals per space: (from, to, waiting).
    spans: HashMap<String, Vec<(u64, u64, bool)>>,
    /// Sessions that aren't a space's own (its tower, a PR's reviewer).
    apart: std::collections::HashSet<String>,
}

/// Its tower (`tower-<space>`) or a PR's reviewer (`review <repo> <n>`): not the space's work.
pub fn apart(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n.starts_with("tower") || n.starts_with("review ") || n.starts_with("review-")
}

impl Tracker {
    pub fn at(path: PathBuf) -> Tracker {
        Tracker { path, ..Default::default() }
    }

    /// Reads what the history added since the last read (all of it, the first time).
    pub fn read(&mut self) {
        let Ok(mut f) = std::fs::File::open(&self.path) else { return };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Started over (rewritten): read again from the top.
            *self = Tracker::at(self.path.clone());
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut text = String::new();
        if f.read_to_string(&mut text).is_err() {
            return;
        }
        // Only whole lines: one being written is read next time.
        let upto = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
        for line in text[..upto].lines() {
            if let Ok(e) = serde_json::from_str::<Entry>(line) {
                self.take(&e);
            }
        }
        self.offset += upto as u64;
    }

    /// One history line.
    pub fn take(&mut self, e: &Entry) {
        let Some(addr) = e.session.clone().filter(|s| !s.is_empty()) else { return };
        if let Some(n) = e.name.as_deref().filter(|n| apart(n)) {
            let _ = n;
            self.apart.insert(addr.clone());
        }
        let s = self.sessions.entry(addr.clone()).or_default();
        s.last_ms = s.last_ms.max(e.at);
        if let Some(sp) = e.space.clone().filter(|x| !x.is_empty()) {
            s.space = Some(sp);
        }
        let space = s.space.clone();
        match e.kind.as_str() {
            "job" => {
                let to = match e.status.as_deref() {
                    Some("working") => State::Working,
                    Some("blocked") => State::Blocked,
                    _ => State::Other,
                };
                let was = s.state.take();
                s.state = (to != State::Other).then_some((to, e.at));
                // The interval its last status opened ends here (a status said again goes on).
                match was {
                    Some((st, from)) if st == to => s.state = Some((st, from)),
                    Some((st, from)) => {
                        if let Some(sp) = space {
                            self.spans.entry(sp).or_default().push((from, e.at, st == State::Blocked));
                        }
                    }
                    None => {}
                }
            }
            // A background job that ended, with when it began.
            "bgjob" => {
                if let (Some(sp), Some(from)) = (space, e.since) {
                    self.spans.entry(sp).or_default().push((from.min(e.at), e.at, false));
                }
            }
            _ => {}
        }
    }

    /// A space's figures at `now`. `opened_ms`: when it opened; `day_ms`:
    /// today's local midnight; `working`: whether each of its sessions works
    /// now as the radar sees it (one it says doesn't has its open interval
    /// closed at its last word); `jobs`: background jobs running now, by
    /// session, with when each began.
    pub fn of(&self, space: &str, opened_ms: u64, day_ms: u64, now: u64, working: &dyn Fn(&str) -> Option<bool>, jobs: &[(String, u64)]) -> Active {
        let mut active: Vec<(u64, u64)> = vec![];
        let mut waiting: Vec<(u64, u64)> = vec![];
        let mine = |sp: &Option<String>| sp.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(space));
        for (sp, list) in &self.spans {
            if !sp.eq_ignore_ascii_case(space) {
                continue;
            }
            for &(from, to, w) in list {
                if w { waiting.push((from, to)) } else { active.push((from, to)) }
            }
        }
        for (addr, s) in &self.sessions {
            if !mine(&s.space) || self.apart.contains(addr) {
                continue;
            }
            let Some((st, from)) = s.state else { continue };
            // Still at it as far as the radar says; one it says isn't (a status the tower missed),
            // or long quiet, ends at its last word.
            let end = match working(addr) {
                Some(false) if st == State::Working => s.last_ms.max(from),
                None if now.saturating_sub(s.last_ms) > STALE_MS => s.last_ms.max(from),
                _ => now,
            };
            if st == State::Blocked { waiting.push((from, end)) } else { active.push((from, end)) }
        }
        for (addr, from) in jobs {
            if self.sessions.get(addr).is_some_and(|s| mine(&s.space)) && !self.apart.contains(addr) {
                active.push((*from, now));
            }
        }
        let active = union(active, opened_ms, now);
        let waiting = minus(&union(waiting, opened_ms, now), &active);
        let current = active.iter().find(|(a, b)| *a <= now && *b >= now.saturating_sub(1_000)).map(|(a, _)| *a);
        Active {
            at_ms: now,
            now: current.is_some(),
            since_ms: current,
            today_ms: measure(&active, day_ms, now),
            total_ms: measure(&active, opened_ms, now),
            waiting_now: current.is_none() && waiting.iter().any(|(a, b)| *a <= now && *b >= now.saturating_sub(1_000)),
            waiting_today_ms: measure(&waiting, day_ms, now),
            waiting_total_ms: measure(&waiting, opened_ms, now),
        }
    }
}

/// Intervals merged where they touch or overlap, clipped to [from, to].
fn union(mut v: Vec<(u64, u64)>, from: u64, to: u64) -> Vec<(u64, u64)> {
    v.retain(|(a, b)| b > a && *b > from && *a < to);
    v.sort();
    let mut out: Vec<(u64, u64)> = vec![];
    for (a, b) in v {
        let (a, b) = (a.max(from), b.min(to));
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// `v` without what `cut` covers (both merged).
fn minus(v: &[(u64, u64)], cut: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = vec![];
    for &(mut a, b) in v {
        for &(c, d) in cut {
            if d <= a || c >= b {
                continue;
            }
            if c > a {
                out.push((a, c));
            }
            a = a.max(d);
            if a >= b {
                break;
            }
        }
        if a < b {
            out.push((a, b));
        }
    }
    out
}

/// How much of `v` falls in [from, to].
fn measure(v: &[(u64, u64)], from: u64, to: u64) -> u64 {
    v.iter().map(|&(a, b)| b.min(to).saturating_sub(a.max(from))).sum()
}

impl Active {
    /// As at `now`: a space at work goes on counting since its figures were made.
    pub fn at(&self, now: u64) -> Active {
        let mut a = self.clone();
        let d = now.saturating_sub(self.at_ms);
        if a.now {
            (a.today_ms, a.total_ms) = (a.today_ms + d, a.total_ms + d);
        } else if a.waiting_now {
            (a.waiting_today_ms, a.waiting_total_ms) = (a.waiting_today_ms + d, a.waiting_total_ms + d);
        }
        a.at_ms = now;
        a
    }

    /// The sidebar's few words: "at work 12m" (this stretch), else "1h 12m today"; none when nothing today.
    pub fn side_word(&self, now: u64) -> Option<String> {
        let a = self.at(now);
        match a.since_ms.filter(|_| a.now) {
            Some(since) => Some(format!("at work {}", short(now.saturating_sub(since)))),
            None if a.today_ms >= 60_000 => Some(format!("{} today", short(a.today_ms))),
            None => None,
        }
    }

    /// The space's head: "Active now · 1h 12m this stretch · 3h 05m today ·
    /// 14h 20m since it opened · waited on you 25m today". None: never at work.
    pub fn head_line(&self, now: u64) -> Option<String> {
        let a = self.at(now);
        if a.total_ms < 60_000 && !a.now {
            return None;
        }
        let mut parts = vec![match a.since_ms.filter(|_| a.now) {
            Some(since) => format!("Active now · {} this stretch", short(now.saturating_sub(since))),
            None if a.waiting_now => "Waiting on you now".to_string(),
            None => "Not at work now".to_string(),
        }];
        parts.push(format!("{} today", short(a.today_ms)));
        parts.push(format!("{} since it opened", short(a.total_ms)));
        if a.waiting_today_ms >= 60_000 {
            parts.push(format!("waited on you {} today", short(a.waiting_today_ms)));
        }
        Some(parts.join(" · "))
    }
}

/// "1h 12m", "42m", "0m".
fn short(ms: u64) -> String {
    let m = ms / 60_000;
    if m < 60 { format!("{m}m") } else { format!("{}h {:02}m", m / 60, m % 60) }
}


#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn job(at: u64, session: &str, space: &str, status: &str) -> Entry {
        Entry { at, kind: "job".into(), session: Some(session.into()), name: Some(format!("{session} web")), space: Some(space.into()), status: Some(status.into()), ..Default::default() }
    }

    fn of(t: &Tracker, now: u64, working: &dyn Fn(&str) -> Option<bool>, jobs: &[(String, u64)]) -> Active {
        t.of("Shop", 0, 100 * MIN, now, working, jobs)
    }

    /// A session works, goes idle, works again: the timer runs, stops, and adds up.
    #[test]
    fn a_space_is_active_while_a_session_works_and_adds_up_its_stretches() {
        let mut t = Tracker::default();
        t.take(&job(110 * MIN, "t1", "Shop", "working"));
        let live = |_: &str| Some(true);
        let a = of(&t, 130 * MIN, &live, &[]);
        assert_eq!((a.now, a.since_ms, a.today_ms, a.total_ms), (true, Some(110 * MIN), 20 * MIN, 20 * MIN), "20 minutes into its first stretch");
        t.take(&job(140 * MIN, "t1", "Shop", "done"));
        let a = of(&t, 150 * MIN, &|_| Some(false), &[]);
        assert_eq!((a.now, a.since_ms, a.today_ms), (false, None, 30 * MIN), "idle: paused at 30 minutes");
        t.take(&job(160 * MIN, "t1", "Shop", "working"));
        let a = of(&t, 165 * MIN, &live, &[]);
        assert_eq!((a.now, a.since_ms, a.today_ms), (true, Some(160 * MIN), 35 * MIN), "again: a new stretch, the day's time adds up");
        // Yesterday's work counts in the total, not in today's.
        t.take(&job(10 * MIN, "t3", "Shop", "working"));
        t.take(&job(20 * MIN, "t3", "Shop", "done"));
        assert_eq!(of(&t, 165 * MIN, &live, &[]).total_ms, 45 * MIN);
        assert_eq!((short(45 * MIN), short(72 * MIN), short(30_000)), ("45m".to_string(), "1h 12m".to_string(), "0m".to_string()));
    }

    /// Two sessions at once are one stretch, not double; waiting on the user
    /// counts apart, and only while nobody works; its tower and reviewers
    /// don't count; a background job does.
    #[test]
    fn sessions_overlap_once_waiting_is_apart_and_towers_and_reviewers_dont_count() {
        let mut t = Tracker::default();
        t.take(&job(110 * MIN, "t1", "Shop", "working"));
        t.take(&job(120 * MIN, "t2", "Shop", "working"));
        t.take(&job(130 * MIN, "t1", "Shop", "done"));
        t.take(&job(140 * MIN, "t2", "Shop", "blocked"));
        t.take(&job(150 * MIN, "t2", "Shop", "done"));
        // A reviewer and the space's tower working all along: not the space's work.
        let mut r = job(100 * MIN, "t9", "Shop", "working");
        r.name = Some("review web 12".into());
        t.take(&r);
        let mut tw = job(100 * MIN, "t8", "Shop", "working");
        tw.name = Some("tower-shop".into());
        t.take(&tw);
        let a = of(&t, 160 * MIN, &|_| None, &[]);
        assert_eq!((a.today_ms, a.waiting_today_ms, a.now, a.waiting_now), (30 * MIN, 10 * MIN, false, false));
        // t2 waits on the user now, while t1 works: active, not waiting.
        t.take(&job(170 * MIN, "t2", "Shop", "blocked"));
        t.take(&job(175 * MIN, "t1", "Shop", "working"));
        let a = of(&t, 180 * MIN, &|_| None, &[]);
        assert_eq!((a.now, a.waiting_now, a.waiting_today_ms, a.today_ms), (true, false, 15 * MIN, 35 * MIN));
        // A background job running now keeps an idle session's space at work.
        t.take(&job(185 * MIN, "t1", "Shop", "done"));
        t.take(&job(185 * MIN, "t2", "Shop", "done"));
        let a = of(&t, 200 * MIN, &|_| Some(false), &[("t1".into(), 190 * MIN)]);
        assert_eq!((a.now, a.since_ms, a.today_ms), (true, Some(190 * MIN), 50 * MIN));
        // And one that ended, read back from its line.
        t.take(&Entry { at: 210 * MIN, kind: "bgjob".into(), session: Some("t1".into()), since: Some(190 * MIN), ..Default::default() });
        assert_eq!(of(&t, 220 * MIN, &|_| Some(false), &[]).today_ms, 60 * MIN);
    }

    /// A status the tower missed: the radar says the session isn't working,
    /// so its open interval ends at its last word, not now.
    #[test]
    fn a_missed_end_is_closed_at_the_sessions_last_word() {
        let mut t = Tracker::default();
        t.take(&job(110 * MIN, "t1", "Shop", "working"));
        t.take(&Entry { at: 125 * MIN, kind: "reply".into(), session: Some("t1".into()), ..Default::default() });
        let a = of(&t, 300 * MIN, &|_| Some(false), &[]);
        assert_eq!((a.now, a.today_ms), (false, 15 * MIN));
        // Another space's sessions are its own.
        assert_eq!(t.of("Docs", 0, 100 * MIN, 300 * MIN, &|_| Some(true), &[]).total_ms, 0);
    }

    /// What the sidebar and the space's head say, and the timer running between two states.
    #[test]
    fn the_timer_runs_on_between_two_states() {
        let a = Active { at_ms: 200 * MIN, now: true, since_ms: Some(128 * MIN), today_ms: 185 * MIN, total_ms: 860 * MIN, waiting_today_ms: 25 * MIN, ..Default::default() };
        assert_eq!(a.side_word(200 * MIN).as_deref(), Some("at work 1h 12m"));
        assert_eq!(a.head_line(200 * MIN).as_deref(), Some("Active now · 1h 12m this stretch · 3h 05m today · 14h 20m since it opened · waited on you 25m today"));
        // Five minutes later, with no new state: it went on.
        assert_eq!(a.head_line(205 * MIN).as_deref(), Some("Active now · 1h 17m this stretch · 3h 10m today · 14h 25m since it opened · waited on you 25m today"));
        let idle = Active { now: false, since_ms: None, ..a.clone() };
        assert_eq!(idle.side_word(205 * MIN).as_deref(), Some("3h 05m today"), "paused: it doesn't run on");
        assert_eq!(idle.head_line(205 * MIN).as_deref(), Some("Not at work now · 3h 05m today · 14h 20m since it opened · waited on you 25m today"));
        let waiting = Active { waiting_now: true, ..idle.clone() };
        assert!(waiting.head_line(205 * MIN).unwrap().starts_with("Waiting on you now · 3h 05m today"));
        assert_eq!(Active::default().side_word(5 * MIN), None);
        assert_eq!(Active::default().head_line(5 * MIN), None, "never at work: nothing said");
    }

    #[test]
    fn the_history_is_read_from_where_it_stopped() {
        let dir = std::env::temp_dir().join(format!("tower-active-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("history.jsonl");
        let line = |e: &Entry| format!("{}\n", serde_json::to_string(e).unwrap());
        std::fs::write(&path, line(&job(110 * MIN, "t1", "Shop", "working"))).unwrap();
        let mut t = Tracker::at(path.clone());
        t.read();
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(line(&job(140 * MIN, "t1", "Shop", "done")).as_bytes()).unwrap();
        // Half a line being written: left for the next read.
        f.write_all(b"{\"at\": 1").unwrap();
        t.read();
        assert_eq!(of(&t, 150 * MIN, &|_| Some(false), &[]).today_ms, 30 * MIN);
        f.write_all(format!("{}, \"kind\": \"job\", \"session\": \"t1\", \"space\": \"Shop\", \"status\": \"working\"}}\n", 50 * MIN).as_bytes()).unwrap();
        t.read();
        assert!(t.sessions["t1"].state.is_some(), "the rest of that line, read whole");
    }
}
