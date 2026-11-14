//! The jobs sessions leave running (`crate::running`), looked at by the
//! daemon every `LOOK`, and those a session declares (`tower job-running`).
//!
//! - **Seen.** For every Claude Code session of this machine the radar
//!   knows: one `ps` for all, one `lsof` for the shells its `claude` keeps
//!   (and what they write), one for the listening ports, and the new lines
//!   of its transcript (read on from where the last look stopped).
//! - **Kept** in memory by session: its jobs and its servers; in the state,
//!   on each agent (`jobs`, `servers`) and on each mission's session (`job`,
//!   how it is shown). The jobs a session declared are kept in
//!   `jobs-running.json` (they outlive a restart of the tower).
//! - **Told.** A job that ends (its process gone, or `tower job-running end`)
//!   and one that goes quiet (`QUIET_MS` without a write or CPU) are told to
//!   the tower as agenda news, once each: the brain checks the work, the
//!   agenda judges the task again. Meanwhile the agenda leaves the session's
//!   tasks alone (`holds`): a turn that ended with a job running is no
//!   finished task, and no stalled one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::state::{Shared, lock};
use crate::agenda::Note;
use crate::running::{self, Declared, Job, Server, Shown, Tasks, World};

/// How often the sessions' shells are looked at.
const LOOK: Duration = Duration::from_secs(15);
/// The most of a transcript read on a first look (its tail).
const FIRST_READ: u64 = 8 * 1024 * 1024;

type Tell = Arc<dyn Fn(Note) + Send + Sync>;

#[derive(Default)]
struct Inner {
    /// Seen and declared, by session.
    jobs: BTreeMap<String, Vec<Job>>,
    servers: BTreeMap<String, Vec<Server>>,
    declared: Declared,
    /// Each transcript read so far: where to read on, and what it said.
    transcripts: HashMap<PathBuf, (u64, Tasks)>,
    /// Jobs told stuck (session/key): told once, until they write again.
    stuck_told: HashSet<String>,
}

#[derive(Clone)]
pub struct Running {
    inner: Arc<Mutex<Inner>>,
    shared: Shared,
    tell: Tell,
    path: PathBuf,
}

static RUNNING: OnceLock<Running> = OnceLock::new();

/// The daemon's, once it runs (None in a command, and in tests).
pub fn get() -> Option<&'static Running> {
    RUNNING.get()
}

/// The jobs `address` has running now.
pub fn jobs_of(address: &str) -> Vec<Job> {
    get().map(|r| lock(&r.inner).jobs.get(address).cloned().unwrap_or_default()).unwrap_or_default()
}

pub fn servers_of(address: &str) -> Vec<Server> {
    get().map(|r| lock(&r.inner).servers.get(address).cloned().unwrap_or_default()).unwrap_or_default()
}

/// How `address`'s jobs show (a mission's session, the phone).
pub fn shown_of(address: &str) -> Option<Shown> {
    running::shown(&jobs_of(address), crate::history::now_ms())
}

/// A job of the session is running and isn't quiet: the agenda leaves its
/// tasks alone (the turn's end isn't the work's).
pub fn holds(address: &str) -> bool {
    jobs_of(address).iter().any(|j| !j.stuck)
}

/// A job's key: its task, else its pid, else what it is.
fn key(j: &Job) -> String {
    j.task.clone().or_else(|| j.pid.map(|p| p.to_string())).unwrap_or_else(|| j.what.clone())
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
}

fn output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// The new complete lines of a transcript since `from` (its tail on a first look).
fn read_on(path: &PathBuf, from: u64) -> Option<(String, u64)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = if from == 0 && len > FIRST_READ { len - FIRST_READ } else if from > len { 0 } else { from };
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    // Only whole lines; the rest is read next time.
    let end = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let mut text = String::from_utf8_lossy(&buf[..end]).into_owned();
    if from == 0 && start > 0 {
        text = text.split_once('\n').map(|(_, r)| r.to_string()).unwrap_or_default();
    }
    Some((text, start + end as u64))
}

impl Running {
    pub fn open(shared: Shared, tell: Tell) -> Running {
        let path = crate::config::state_dir().join("jobs-running.json");
        let declared: Declared = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Running { inner: Arc::new(Mutex::new(Inner { declared, ..Default::default() })), shared, tell, path }
    }

    pub fn start(&self) {
        let _ = RUNNING.set(self.clone());
        let me = self.clone();
        let _ = thread::Builder::new().name("running".into()).spawn(move || {
            loop {
                me.look();
                thread::sleep(LOOK);
            }
        });
    }

    fn save(&self, declared: &Declared) {
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_string_pretty(declared).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// One look at every Claude Code session of this machine (but those this
    /// tower leaves alone: test sessions, or the real ones for a test tower).
    pub fn look(&self) {
        let now = crate::history::now_ms();
        let sessions: Vec<(String, String)> = self
            .shared
            .with(|s| s.agents.iter().filter(|a| a.app == "claude-code" && !a.address.contains('/')).map(|a| (a.address.clone(), a.name.clone())).collect());
        let claudes: Vec<(String, String, u32)> = sessions
            .into_iter()
            .filter_map(|(a, n)| crate::hc::get(&a).filter(|t| !super::watch::left_alone(t)).as_ref().and_then(crate::accounts::claude_pid).map(|p| (a, n, p)))
            .collect();
        let world = world_for(&claudes.iter().map(|c| c.2).collect::<Vec<_>>());
        let mut seen: BTreeMap<String, (Vec<Job>, Vec<Server>)> = BTreeMap::new();
        let mut names: HashMap<String, String> = HashMap::new();
        for (address, name, pid) in &claudes {
            names.insert(address.clone(), name.clone());
            if world.shells(*pid).is_empty() {
                continue;
            }
            let tasks = match crate::questions::transcript_path_of(*pid) {
                Some(path) => {
                    let mut i = lock(&self.inner);
                    let (from, mut tasks) = i.transcripts.remove(&path).unwrap_or_default();
                    let at = match read_on(&path, from) {
                        Some((text, at)) => {
                            running::read_tasks(&text, &mut tasks);
                            at
                        }
                        None => from,
                    };
                    i.transcripts.insert(path, (at, tasks.clone()));
                    tasks
                }
                None => Tasks::default(),
            };
            let found = running::look(*pid, &world, &tasks, now);
            if !found.0.is_empty() || !found.1.is_empty() {
                seen.insert(address.clone(), found);
            }
        }
        self.settle(seen, &names, now);
    }

    /// What was seen, with what is declared: kept, told, published.
    fn settle(&self, seen: BTreeMap<String, (Vec<Job>, Vec<Server>)>, names: &HashMap<String, String>, now: u64) {
        let mut told: Vec<(String, String)> = vec![];
        let changed = {
            let mut i = lock(&self.inner);
            // Declared jobs whose process is gone ended (told below, as every
            // job gone since the last look); the rest are aged.
            let mut gone_declared = false;
            for list in i.declared.values_mut() {
                let before = list.len();
                list.retain(|j| !j.pid.is_some_and(|p| !alive(p)));
                gone_declared |= list.len() != before;
                for j in list.iter_mut() {
                    j.stuck = now.saturating_sub(j.progress_ms.unwrap_or(j.since_ms)) > running::QUIET_MS && !j.pid.is_some_and(|p| cpu_busy(p));
                }
            }
            i.declared.retain(|_, l| !l.is_empty());
            if gone_declared {
                let d = i.declared.clone();
                self.save(&d);
            }
            let mut jobs: BTreeMap<String, Vec<Job>> = BTreeMap::new();
            let mut servers: BTreeMap<String, Vec<Server>> = BTreeMap::new();
            for (a, (j, s)) in seen {
                if !j.is_empty() {
                    jobs.insert(a.clone(), j);
                }
                if !s.is_empty() {
                    servers.insert(a, s);
                }
            }
            for (a, d) in &i.declared {
                jobs.entry(a.clone()).or_default().extend(d.iter().cloned());
            }
            // Jobs gone since the last look: ended.
            for (address, before) in &i.jobs {
                let now_keys: HashSet<String> = jobs.get(address).map(|l| l.iter().map(key).collect()).unwrap_or_default();
                for j in before.iter().filter(|j| !now_keys.contains(&key(j))) {
                    let summary = j.task.as_ref().and_then(|t| i.transcripts.values().find_map(|(_, tasks)| tasks.ended.get(t).cloned()));
                    told.push((address.clone(), ended_line(names.get(address).map(String::as_str).unwrap_or(address), j, summary.as_deref(), now)));
                }
            }
            // Newly quiet: told once (again only after it wrote meanwhile).
            for (address, list) in &jobs {
                for j in list {
                    let k = format!("{address}/{}", key(j));
                    if j.stuck && i.stuck_told.insert(k.clone()) {
                        told.push((address.clone(), stuck_line(names.get(address).map(String::as_str).unwrap_or(address), j, now)));
                    } else if !j.stuck {
                        i.stuck_told.remove(&k);
                    }
                }
            }
            let changed = i.jobs != jobs || i.servers != servers;
            i.jobs = jobs;
            i.servers = servers;
            changed
        };
        for (address, line) in told {
            eprintln!("[running] {line}");
            let name = names.get(&address).cloned().unwrap_or_else(|| address.clone());
            self.shared.record(&address, &line, "job");
            (self.tell)(Note { session: address, name, space: None, msg: format!("[job] {line}"), speech: String::new() });
        }
        if changed {
            self.publish();
            super::missions::kick();
        }
    }

    /// The agents in the state carry their jobs and servers.
    pub fn publish(&self) {
        let (jobs, servers) = {
            let i = lock(&self.inner);
            (i.jobs.clone(), i.servers.clone())
        };
        self.shared.update(|s| {
            for a in s.agents.iter_mut() {
                a.jobs = jobs.get(&a.address).cloned().unwrap_or_default();
                a.servers = servers.get(&a.address).cloned().unwrap_or_default();
            }
        });
    }

    /// `tower job-running start|progress|end`: what a session says of a job
    /// it runs (`what`; `pid`: its process, when it has one to follow).
    pub fn declare(&self, terminal: &str, action: &str, what: &str, pid: Option<u32>) -> Result<Value> {
        let terminal = terminal.trim();
        if terminal.is_empty() {
            bail!("tower job-running runs in a session's terminal (HC_TERMINAL_ID): there is none here");
        }
        let now = crate::history::now_ms();
        let what = what.trim();
        let out = {
            let mut i = lock(&self.inner);
            let list = i.declared.entry(terminal.to_string()).or_default();
            // The job it means: by what, else the latest.
            let at = list.iter().rposition(|j| !what.is_empty() && j.what.eq_ignore_ascii_case(what)).or_else(|| list.len().checked_sub(1));
            let out = match action {
                "start" => {
                    if what.is_empty() {
                        bail!("tower job-running start '<what>' [--pid <pid>]");
                    }
                    if let Some(p) = pid
                        && !alive(p)
                    {
                        bail!("no process {p}");
                    }
                    list.push(Job { what: what.to_string(), since_ms: now, progress_ms: Some(now), pid, source: "declared".into(), ..Default::default() });
                    json!({ "started": what })
                }
                "progress" => {
                    let Some(at) = at else { bail!("no job running here: tower job-running start '<what>' first") };
                    let j = &mut list[at];
                    j.progress_ms = Some(now);
                    j.stuck = false;
                    if !what.is_empty() && !j.what.eq_ignore_ascii_case(what) {
                        j.note = what.to_string();
                    }
                    json!({ "progress": j.what })
                }
                "end" | "done" | "stop" => {
                    let Some(at) = at else { bail!("no job running here") };
                    let j = list.remove(at);
                    json!({ "ended": j.what })
                }
                other => bail!("tower job-running {other}: use start, progress or end"),
            };
            i.declared.retain(|_, l| !l.is_empty());
            let d = i.declared.clone();
            self.save(&d);
            out
        };
        // Seen at once: the state, the missions, and what ended is told.
        let names: HashMap<String, String> = self.shared.with(|s| s.agents.iter().map(|a| (a.address.clone(), a.name.clone())).collect());
        let seen: BTreeMap<String, (Vec<Job>, Vec<Server>)> = {
            let i = lock(&self.inner);
            i.jobs
                .iter()
                .map(|(a, l)| (a.clone(), (l.iter().filter(|j| j.source != "declared").cloned().collect(), i.servers.get(a).cloned().unwrap_or_default())))
                .collect()
        };
        self.settle(seen, &names, now);
        Ok(out)
    }
}

/// What the system says now of these `claude`s' shells: every process,
/// and for their shells' trees the files they have open, the ports they
/// listen on, and when those files were last written.
fn world_for(claudes: &[u32]) -> World {
    let mut world = World { procs: running::parse_ps(&output("ps", &["-axww", "-o", "pid=,ppid=,etime=,pcpu=,command="])), ..Default::default() };
    let pids: Vec<String> = claudes.iter().flat_map(|p| world.shells(*p).into_iter().flat_map(|s| world.subtree(s.pid)).map(|p| p.pid.to_string()).collect::<Vec<_>>()).collect();
    if pids.is_empty() {
        return world;
    }
    world.files = running::parse_lsof(&output("lsof", &["-nP", "-Fpfan", "-a", "-p", &pids.join(",")]));
    world.listen = running::parse_listen(&output("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn"]));
    for o in world.files.values().flatten() {
        if let Ok(m) = std::fs::metadata(&o.name)
            && m.is_file()
            && let Ok(t) = m.modified()
            && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
        {
            world.mtimes.insert(o.name.clone(), d.as_millis() as u64);
        }
    }
    world
}

/// `tower job-running look <session>`: one look at a session now, as the
/// daemon takes it (reads only: ps, lsof, its transcript).
pub fn look_at(address: &str) -> Result<Value> {
    let t = crate::hc::get(address).with_context(|| format!("no terminal {address}"))?;
    let pid = crate::accounts::claude_pid(&t).with_context(|| format!("{address} runs no Claude Code session on this machine"))?;
    let world = world_for(&[pid]);
    let mut tasks = Tasks::default();
    if let Some(path) = crate::questions::transcript_path_of(pid)
        && let Some((text, _)) = read_on(&path, 0)
    {
        running::read_tasks(&text, &mut tasks);
    }
    let now = crate::history::now_ms();
    let (jobs, servers) = running::look(pid, &world, &tasks, now);
    let shells: Vec<Value> = world
        .shells(pid)
        .iter()
        .map(|s| json!({ "pid": s.pid, "task": running::task_of(world.files.get(&s.pid).map(Vec::as_slice).unwrap_or(&[])), "command": running::clip(&running::shell_command(&s.command).unwrap_or_default(), 120) }))
        .collect();
    Ok(json!({ "session": address, "claude": pid, "line": running::line(&jobs, now), "jobs": jobs, "servers": servers, "shells": shells }))
}

fn cpu_busy(pid: u32) -> bool {
    output("ps", &["-o", "pcpu=", "-p", &pid.to_string()]).trim().parse::<f32>().is_ok_and(|c| c >= 5.0)
}

fn minutes(ms: u64) -> String {
    let m = ms / 60_000;
    if m >= 120 { format!("{} h {} min", m / 60, m % 60) } else { format!("{m} min") }
}

/// "eyz search llm's job ended: Train the 1.2B LoRA (ran 52 min; completed (exit code 0))."
fn ended_line(name: &str, j: &Job, summary: Option<&str>, now: u64) -> String {
    let how = summary.filter(|s| !s.is_empty()).map(|s| format!("; {s}")).unwrap_or_default();
    format!("{name}'s job ended: {} (ran {}{how}). Check what it left and whether its task is done.", running::clip(&j.what, 80), minutes(now.saturating_sub(j.since_ms)))
}

/// "eyz search llm's job may be stuck: …: nothing written and no CPU for 34 min."
fn stuck_line(name: &str, j: &Job, now: u64) -> String {
    let quiet = now.saturating_sub(j.progress_ms.unwrap_or(j.since_ms));
    format!("{name}'s job may be stuck: {}: nothing written and no CPU for {}.", running::clip(&j.what, 80), minutes(quiet))
}

pub(super) fn dispatch(running: Option<&Running>, terminal: &str, action: &str, what: &str, pid: Option<u32>) -> Result<Value> {
    let r = running.context("the tower's daemon has no job watch yet")?;
    r.declare(terminal, action, what, pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::state::State;

    fn running() -> (Running, Arc<Mutex<Vec<Note>>>) {
        let shared = Shared::new(State::new("ptt", "test", "test"));
        let told = Arc::new(Mutex::new(vec![]));
        let t = told.clone();
        let dir = std::env::temp_dir().join(format!("tower-running-{}-{}", std::process::id(), crate::history::now_ms()));
        let _ = std::fs::create_dir_all(&dir);
        let r = Running {
            inner: Arc::default(),
            shared,
            tell: Arc::new(move |n: Note| lock(&t).push(n)),
            path: dir.join("jobs-running.json"),
        };
        (r, told)
    }

    fn job(what: &str, task: &str, since: u64, progress: u64, stuck: bool) -> Job {
        Job { what: what.into(), since_ms: since, progress_ms: Some(progress), stuck, task: Some(task.into()), source: "background".into(), ..Default::default() }
    }

    #[test]
    fn a_job_that_ends_or_goes_quiet_is_told_once() {
        let (r, told) = running();
        let names: HashMap<String, String> = [("t12r".to_string(), "eyz search llm".to_string())].into();
        let now = 1_794_620_000_000;
        let mut seen = BTreeMap::new();
        seen.insert("t12r".to_string(), (vec![job("Train the 1.2B LoRA", "btrain", now - 600_000, now - 10_000, false)], vec![]));
        r.settle(seen.clone(), &names, now);
        assert!(lock(&told).is_empty());
        assert_eq!(lock(&r.inner).jobs["t12r"].len(), 1);
        // Quiet and idle: told once, not twice.
        let mut quiet = BTreeMap::new();
        quiet.insert("t12r".to_string(), (vec![job("Train the 1.2B LoRA", "btrain", now - 600_000, now - 40 * 60_000, true)], vec![]));
        r.settle(quiet.clone(), &names, now);
        r.settle(quiet, &names, now + 15_000);
        let t = lock(&told).clone();
        assert_eq!(t.len(), 1, "{t:?}");
        assert!(t[0].msg.starts_with("[job] eyz search llm's job may be stuck: Train the 1.2B LoRA: nothing written and no CPU for 40 min"), "{}", t[0].msg);
        // Its process gone: it ended, told so; nothing held any more.
        r.settle(BTreeMap::new(), &names, now + 30_000);
        let t = lock(&told).clone();
        assert_eq!(t.len(), 2);
        assert!(t[1].msg.starts_with("[job] eyz search llm's job ended: Train the 1.2B LoRA (ran 10 min)") && t[1].session == "t12r", "{}", t[1].msg);
        assert!(lock(&r.inner).jobs.is_empty());
    }

    #[test]
    fn a_session_declares_its_job_and_ends_it() {
        let (r, told) = running();
        assert!(r.declare("", "start", "x", None).is_err(), "no terminal");
        assert!(r.declare("t5", "start", "", None).is_err());
        r.declare("t5", "start", "Rebuild the search index", None).unwrap();
        let j = lock(&r.inner).jobs["t5"][0].clone();
        assert_eq!((j.what.as_str(), j.source.as_str(), j.stuck), ("Rebuild the search index", "declared", false));
        r.declare("t5", "progress", "40% of the titles", None).unwrap();
        assert_eq!(lock(&r.inner).jobs["t5"][0].note, "40% of the titles");
        assert!(std::fs::read_to_string(&r.path).unwrap().contains("Rebuild the search index"), "kept across restarts");
        r.declare("t5", "end", "", None).unwrap();
        assert!(lock(&r.inner).jobs.get("t5").is_none());
        let t = lock(&told).clone();
        assert!(t.len() == 1 && t[0].msg.contains("job ended: Rebuild the search index"), "{t:?}");
        assert!(r.declare("t5", "progress", "", None).is_err(), "nothing running");
        assert!(r.declare("t5", "start", "Wait on it", Some(999_999_999)).is_err(), "no such process");
    }
}
