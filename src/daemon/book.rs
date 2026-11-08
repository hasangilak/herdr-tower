//! What the radar must not forget across a restart, in one file
//! (`watches.json` in the state folder):
//!
//! - `watches`: the sessions the brain asked to hear about (`tower watch`),
//!   with what it asked and the status the session had when last seen.
//! - `seen`: the last status of every agent session, so a restarted voice can
//!   tell which ones stopped working while it was down.
//! - `outbox`: reports decided but not yet taken by the brain (it may be
//!   restarting too). A watch moves into the outbox the moment it fires, in
//!   one write, so a finish is reported once: never lost, never twice.
//!
//! `reconcile` is the startup check: given the book, the open jobs in the
//! history and what Herdr says now, which reports are owed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Watch {
    /// What the brain asked the session to do.
    pub note: String,
    pub since_ms: u64,
    /// Its status when watched or last seen (`working`, ...; "" unknown).
    pub last: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// A report owed to the brain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pending {
    pub address: String,
    pub name: String,
    pub space: Option<String>,
    /// What it is now: `done`, `blocked`, `exited`, ...
    pub status: String,
    /// The `[watch] ...` line for the brain.
    pub msg: String,
    /// The short line to say out loud when the brain can't take it.
    pub speech: String,
    /// `watch` (asked for) or `job` (an open job, no watch).
    pub kind: String,
    pub at_ms: u64,
    /// Already said out loud once (the brain was away).
    pub spoken: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub watches: BTreeMap<String, Watch>,
    pub seen: BTreeMap<String, String>,
    pub outbox: Vec<Pending>,
}

/// Reports older than this are dropped rather than delivered.
pub const STALE_MS: u64 = 24 * 3600 * 1000;

impl Book {
    pub fn path() -> PathBuf {
        crate::config::state_dir().join("watches.json")
    }

    /// The book in `path`; also reads the old format (address -> what was asked).
    pub fn load_from(path: &Path) -> Book {
        let Ok(text) = std::fs::read_to_string(path) else { return Book::default() };
        if let Ok(b) = serde_json::from_str::<Book>(&text)
            && (text.contains("\"watches\"") || text.contains("\"outbox\"") || text.contains("\"seen\""))
        {
            return b;
        }
        let old: BTreeMap<String, String> = serde_json::from_str(&text).unwrap_or_default();
        Book { watches: old.into_iter().map(|(a, note)| (a, Watch { note, ..Default::default() })).collect(), ..Default::default() }
    }

    /// Written whole, through a temporary file: a crash leaves the old book or the new one.
    pub fn save_to(&self, path: &Path) {
        let tmp = path.with_extension("json.tmp");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// The watch on `address` fires: it leaves the watches and its report
    /// joins the outbox (save once after this). None when it had already fired.
    pub fn fire(&mut self, address: &str, report: impl FnOnce(&Watch) -> Pending) -> Option<Pending> {
        let w = self.watches.remove(address)?;
        let p = report(&w);
        self.outbox.push(p.clone());
        Some(p)
    }

    /// A report owed without a watch (an open job). Once per address and status.
    pub fn owe(&mut self, p: Pending) -> bool {
        if self.outbox.iter().any(|o| o.address == p.address && o.status == p.status) {
            return false;
        }
        self.outbox.push(p);
        true
    }

    /// Hands each report to `send` (true: taken), oldest first, dropping stale ones.
    /// Returns whether anything changed.
    pub fn deliver(&mut self, now_ms: u64, mut send: impl FnMut(&mut Pending) -> bool) -> bool {
        let before = self.outbox.clone();
        self.outbox.retain(|p| now_ms.saturating_sub(p.at_ms) < STALE_MS);
        let mut keep = vec![];
        for mut p in std::mem::take(&mut self.outbox) {
            if !send(&mut p) {
                keep.push(p);
            }
        }
        self.outbox = keep;
        self.outbox != before
    }
}

/// A session as Herdr has it now.
#[derive(Debug, Clone, PartialEq)]
pub struct Now {
    pub live: bool,
    pub status: String,
}

/// An open job in the history (no watch needed).
#[derive(Debug, Clone, PartialEq)]
pub struct OpenJob {
    pub session: String,
    /// The job's own status (`working` or `blocked`).
    pub status: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Owed {
    /// A watched session stopped working, is blocked or exited.
    Watch { address: String, status: String, was: String },
    /// A session with an open job (and no watch) did, while nobody looked.
    Job { address: String, status: String, was: String, title: String },
}

fn stopped(status: &str) -> bool {
    matches!(status, "done" | "idle" | "blocked" | "error")
}

/// What is owed after a restart. `now` has every session on the machines
/// that answered; `reachable(address)` says whether its machine did (a
/// session on a machine that didn't answer is unknown, not gone).
pub fn reconcile(book: &Book, jobs: &[OpenJob], now: &BTreeMap<String, Now>, reachable: impl Fn(&str) -> bool) -> Vec<Owed> {
    let mut out = vec![];
    for (address, w) in &book.watches {
        if !reachable(address) {
            continue;
        }
        let was = book.seen.get(address).cloned().unwrap_or_else(|| w.last.clone());
        match now.get(address) {
            None => out.push(Owed::Watch { address: address.clone(), status: "gone".into(), was }),
            Some(n) if !n.live => out.push(Owed::Watch { address: address.clone(), status: "exited".into(), was }),
            Some(n) if stopped(&n.status) => out.push(Owed::Watch { address: address.clone(), status: n.status.clone(), was }),
            // Still working, or not reporting yet (a first-run question: the trust check has it).
            Some(_) => {}
        }
    }
    let mut done: Vec<&str> = vec![];
    for j in jobs {
        let a = j.session.as_str();
        if book.watches.contains_key(a) || done.contains(&a) || !reachable(a) {
            continue;
        }
        done.push(a);
        // Where it stood when last seen; the job's own word when never seen.
        let was = book.seen.get(a).cloned().unwrap_or_else(|| j.status.clone());
        let status = match now.get(a) {
            None => "gone".to_string(),
            Some(n) if !n.live => "exited".into(),
            Some(n) => n.status.clone(),
        };
        let owed = match status.as_str() {
            // Unless the radar saw it end while it was up.
            "gone" | "exited" => !matches!(was.as_str(), "gone" | "exited"),
            // Finished since it was last seen working.
            "done" | "idle" | "error" => was == "working",
            // Newly waiting on someone (a job already marked blocked was told).
            "blocked" => j.status != "blocked" && was != "blocked",
            _ => false,
        };
        if owed {
            out.push(Owed::Job { address: a.into(), status, was, title: j.title.clone() });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now(list: &[(&str, bool, &str)]) -> BTreeMap<String, Now> {
        list.iter().map(|(a, live, s)| (a.to_string(), Now { live: *live, status: s.to_string() })).collect()
    }

    fn watch(note: &str, last: &str) -> Watch {
        Watch { note: note.into(), since_ms: 1, last: last.into(), account: None }
    }

    fn report(address: &str, status: &str) -> Pending {
        Pending {
            address: address.into(),
            status: status.into(),
            msg: format!("[watch] {address} is now {status}."),
            at_ms: 100,
            ..Default::default()
        }
    }

    #[test]
    fn reconcile_reports_what_changed_while_down() {
        let mut b = Book::default();
        b.watches.insert("t1".into(), watch("run the tests", "working"));
        b.watches.insert("t2".into(), watch("deploy", "working"));
        b.watches.insert("t3".into(), watch("fix it", "working"));
        b.watches.insert("t4".into(), watch("ask", "working"));
        b.watches.insert("t5".into(), watch("long job", "working"));
        b.watches.insert("vps/t1".into(), watch("remote", "working"));
        let n = now(&[("t1", true, "done"), ("t2", true, "blocked"), ("t3", false, ""), ("t5", true, "working")]);
        let owed = reconcile(&b, &[], &n, |a| !a.starts_with("vps/"));
        assert_eq!(
            owed,
            vec![
                Owed::Watch { address: "t1".into(), status: "done".into(), was: "working".into() },
                Owed::Watch { address: "t2".into(), status: "blocked".into(), was: "working".into() },
                Owed::Watch { address: "t3".into(), status: "exited".into(), was: "working".into() },
                Owed::Watch { address: "t4".into(), status: "gone".into(), was: "working".into() },
            ],
            "t5 still works; vps didn't answer, so its session is unknown, not gone"
        );
    }

    #[test]
    fn reconcile_catches_open_jobs_without_a_watch() {
        let mut b = Book::default();
        b.seen.insert("t1".into(), "working".into());
        b.seen.insert("t2".into(), "done".into());
        b.watches.insert("t6".into(), watch("watched", "working"));
        let job = |s: &str, st: &str| OpenJob { session: s.into(), status: st.into(), title: format!("job on {s}") };
        b.seen.insert("t8".into(), "gone".into());
        let jobs = [
            job("t1", "working"),
            job("t2", "working"),
            job("t3", "working"),
            job("t4", "blocked"),
            job("t5", "working"),
            job("t6", "working"),
            job("t7", "working"),
            job("t8", "working"),
            job("t1", "working"),
        ];
        let n = now(&[
            ("t1", true, "done"),
            ("t2", true, "done"),
            ("t4", true, "blocked"),
            ("t5", true, "working"),
            ("t6", true, "working"),
            ("t7", true, "blocked"),
        ]);
        let owed = reconcile(&b, &jobs, &n, |_| true);
        assert_eq!(
            owed,
            vec![
                Owed::Job { address: "t1".into(), status: "done".into(), was: "working".into(), title: "job on t1".into() },
                Owed::Job { address: "t3".into(), status: "gone".into(), was: "working".into(), title: "job on t3".into() },
                Owed::Job { address: "t7".into(), status: "blocked".into(), was: "working".into(), title: "job on t7".into() },
            ],
            "t2 was already done when last seen; t4's job already says blocked; t5 works; t6 has a watch; t8 ended while the tower looked; t1 once"
        );
    }

    #[test]
    fn old_format_loads() {
        let dir = std::env::temp_dir().join(format!("tower-book-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("watches.json");
        std::fs::write(&path, r#"{ "t22": "release it" }"#).unwrap();
        let b = Book::load_from(&path);
        assert_eq!(b.watches["t22"].note, "release it");
        assert_eq!(b.watches["t22"].last, "");
        std::fs::write(&path, r#"{ "watches": {}, "seen": {}, "outbox": [] }"#).unwrap();
        assert_eq!(Book::load_from(&path), Book::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tower goes down while a watched session works, the session
    /// finishes, the tower comes back (twice), and the brain is away at first.
    #[test]
    fn simulated_restart_reports_once() {
        let dir = std::env::temp_dir().join(format!("tower-book-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("watches.json");

        // Before: the brain watches t22 while it works.
        let mut b = Book::default();
        b.watches.insert("t22".into(), watch("release the dashboard", "working"));
        b.seen.insert("t22".into(), "working".into());
        b.save_to(&path);
        // ... the tower is down; t22 finishes.
        let herdr = now(&[("t22", true, "done")]);

        // First start: the finish is owed; the brain isn't up yet.
        let mut b = Book::load_from(&path);
        let owed = reconcile(&b, &[], &herdr, |_| true);
        assert_eq!(owed, vec![Owed::Watch { address: "t22".into(), status: "done".into(), was: "working".into() }]);
        assert!(
            b.fire("t22", |w| {
                assert_eq!(w.note, "release the dashboard");
                report("t22", "done")
            })
            .is_some()
        );
        assert!(b.fire("t22", |_| report("t22", "done")).is_none(), "a watch fires once");
        b.save_to(&path);
        let mut tries = 0;
        b.deliver(200, |p| {
            tries += 1;
            p.spoken = true;
            false
        });
        assert_eq!((tries, b.outbox.len(), b.outbox[0].spoken), (1, 1, true), "kept for later, said out loud once");
        b.save_to(&path);

        // Restarted again before the brain took it: nothing new is owed, the report is still waiting.
        let mut b = Book::load_from(&path);
        assert!(reconcile(&b, &[], &herdr, |_| true).is_empty(), "no second report for the same finish");
        assert_eq!(b.outbox.len(), 1);
        let mut got = vec![];
        assert!(b.deliver(300, |p| {
            got.push(p.msg.clone());
            true
        }));
        assert_eq!(got, vec!["[watch] t22 is now done."]);
        b.save_to(&path);

        // And once more: all quiet.
        let mut b = Book::load_from(&path);
        assert!(reconcile(&b, &[], &herdr, |_| true).is_empty());
        assert!(!b.deliver(400, |_| panic!("nothing left to send")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_reports_are_owed_once_and_go_stale() {
        let mut b = Book::default();
        assert!(b.owe(report("t1", "done")));
        assert!(!b.owe(report("t1", "done")));
        assert!(b.owe(report("t1", "blocked")));
        assert!(b.deliver(100 + STALE_MS, |_| false), "a day-old report is dropped");
        assert_eq!(b.outbox.len(), 0);
    }
}
