//! Real work a session leaves running when its turn ends: a job it started
//! in the background (a training run, a long script) and watches, while its
//! own status says `done`.
//!
//! - **Where it is seen.** Claude Code runs every Bash call in a shell of its
//!   own (`zsh -c source '…/shell-snapshots/snapshot-…' … eval '<command>'`),
//!   a child of the session's `claude`. A turn's own calls end with it; a
//!   shell still there after the turn is a background task (`run_in_background`)
//!   or a Monitor. Its stdout is the task's output file (`…/tasks/<id>.output`):
//!   its name is Claude Code's task id, which the transcript ties to the
//!   call's description and command (`read_tasks`).
//! - **Job, server or watch** (`kind_of`). A Monitor, `tail -f`, `--watch`
//!   and the like only watch (`Watch`). A process holding a listening port,
//!   or a known dev-server command (`npm run dev`, `next dev`, `vite`,
//!   `uvicorn` …), is a server left running (`Server`): no job. Anything
//!   else is a finite command: a `Job`.
//! - **Progress.** The newest write to a file its processes have open for
//!   writing (its log, its task output), and whether any of them uses the
//!   CPU now (`busy`). Quiet for `QUIET_MS` and idle: possibly stuck.
//! - **Said so.** A session can also say it itself: `tower job-running
//!   start|progress|end '<what>' [--pid <pid>]` (`declared`).
//!
//! The daemon looks (`crate::daemon::running`); this module is the reading.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A job with no write and no CPU for this long may be stuck.
pub const QUIET_MS: u64 = 30 * 60_000;
/// A background shell is a job once it has run this long…
pub const MIN_JOB_MS: u64 = 2 * 60_000;
/// …or once it is seen running while its session's turn is over (done or
/// idle), and has run at least this long (a helper that ends a moment
/// after the turn is no job).
pub const TURN_OVER_MIN_MS: u64 = 15_000;

/// Whether a background shell counts as a job: a short helper a session
/// runs in the background while it works (`S=…; until …; done`, a quick
/// check) isn't, and its end is nobody's news. It counts once it has
/// outlived the session's turn (`turn_over`: seen running while the session
/// is done or idle) or run `MIN_JOB_MS`; once counted (`counted`), it stays
/// so (a Monitor's event may wake the session meanwhile). A job the session
/// declared always counts.
pub fn counts(job: &Job, now_ms: u64, turn_over: bool, counted: bool) -> bool {
    let age = now_ms.saturating_sub(job.since_ms);
    job.source == "declared" || counted || age >= MIN_JOB_MS || (turn_over && age >= TURN_OVER_MIN_MS)
}
/// A process above this CPU percent is at work, whatever it writes.
const BUSY_CPU: f32 = 5.0;

// ---- what the system says ----

/// A line of `ps -axo pid=,ppid=,etime=,pcpu=,command=`.
#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// How long it has run.
    pub secs: u64,
    pub cpu: f32,
    pub command: String,
}

/// `11:46`, `07:10:32`, `2-03:04:05` → seconds.
fn etime_secs(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest.split(':').map(|p| p.parse::<u64>().ok()).collect::<Option<_>>()?;
    let (h, m, sec) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    Some(days * 86_400 + h * 3_600 + m * 60 + sec)
}

pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let secs = etime_secs(it.next()?)?;
            let cpu = it.next()?.parse().ok()?;
            let command = it.collect::<Vec<_>>().join(" ");
            Some(Proc { pid, ppid, secs, cpu, command })
        })
        .collect()
}

/// A file a process has open: its descriptor, `r` `w` or `u`, its path.
#[derive(Debug, Clone, PartialEq)]
pub struct Open {
    pub fd: String,
    pub access: String,
    pub name: String,
}

/// `lsof -nP -Fpfan -a -p <pids>`, by pid.
pub fn parse_lsof(text: &str) -> HashMap<u32, Vec<Open>> {
    let mut out: HashMap<u32, Vec<Open>> = HashMap::new();
    let mut pid = 0u32;
    let mut cur: Option<Open> = None;
    let flush = |pid: u32, o: Option<Open>, out: &mut HashMap<u32, Vec<Open>>| {
        if let Some(o) = o.filter(|o| !o.name.is_empty()) {
            out.entry(pid).or_default().push(o);
        }
    };
    for l in text.lines() {
        let (tag, rest) = l.split_at(l.len().min(1));
        match tag {
            "p" => {
                flush(pid, cur.take(), &mut out);
                pid = rest.parse().unwrap_or(0);
            }
            "f" => {
                flush(pid, cur.take(), &mut out);
                cur = Some(Open { fd: rest.to_string(), access: String::new(), name: String::new() });
            }
            "a" => {
                if let Some(o) = cur.as_mut() {
                    o.access = rest.to_string();
                }
            }
            "n" => {
                if let Some(o) = cur.as_mut() {
                    o.name = rest.to_string();
                }
            }
            _ => {}
        }
    }
    flush(pid, cur.take(), &mut out);
    out
}

/// `lsof -nP -iTCP -sTCP:LISTEN -Fpn`: the ports each pid listens on.
pub fn parse_listen(text: &str) -> HashMap<u32, Vec<u16>> {
    let mut out: HashMap<u32, Vec<u16>> = HashMap::new();
    let mut pid = 0u32;
    for l in text.lines() {
        if let Some(p) = l.strip_prefix('p') {
            pid = p.parse().unwrap_or(0);
        } else if let Some(n) = l.strip_prefix('n')
            && let Some(port) = n.rsplit(':').next().and_then(|p| p.parse::<u16>().ok())
        {
            let ports = out.entry(pid).or_default();
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    out
}

// ---- Claude Code's background tasks, from the transcript ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    /// A Bash call run in the background.
    #[default]
    Bash,
    /// A Monitor: it watches something and wakes the session on its events.
    Monitor,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Task {
    pub id: String,
    pub kind: TaskKind,
    pub description: String,
    pub command: String,
    pub at_ms: u64,
}

/// What a transcript says of its background tasks, read line by line as it grows.
#[derive(Debug, Clone, Default)]
pub struct Tasks {
    pub by_id: HashMap<String, Task>,
    /// Task ids Claude Code said ended (its `<task-notification>`), with its summary.
    pub ended: HashMap<String, String>,
    /// Calls whose result (with the task id) hasn't come yet, by tool use id.
    pending: HashMap<String, Task>,
}

fn ts_ms(v: &Value) -> u64 {
    v["timestamp"].as_str().and_then(crate::releases::ms_of_iso).unwrap_or(0)
}

/// The text of a tool result, or a message's.
fn text_of(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str().or_else(|| b.as_str())).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// `<tag>x</tag>` in `s`.
fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let from = s.find(&open)? + open.len();
    let to = s[from..].find(&format!("</{name}>"))? + from;
    Some(s[from..to].trim())
}

/// Takes in more lines of a transcript.
pub fn read_tasks(lines: &str, tasks: &mut Tasks) {
    for line in lines.lines() {
        if !(line.contains("run_in_background") || line.contains("\"Monitor\"") || line.contains("in background with ID") || line.contains("Monitor started") || line.contains("task-notification")) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let content = &v["message"]["content"];
        let blocks: Vec<&Value> = match content {
            Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        for b in blocks {
            match b["type"].as_str() {
                Some("tool_use") => {
                    let input = &b["input"];
                    let kind = match b["name"].as_str() {
                        Some("Bash") if input["run_in_background"].as_bool() == Some(true) => TaskKind::Bash,
                        Some("Monitor") => TaskKind::Monitor,
                        _ => continue,
                    };
                    let task = Task {
                        id: String::new(),
                        kind,
                        description: input["description"].as_str().unwrap_or("").trim().to_string(),
                        command: input["command"].as_str().unwrap_or("").trim().to_string(),
                        at_ms: ts_ms(&v),
                    };
                    if let Some(id) = b["id"].as_str() {
                        tasks.pending.insert(id.to_string(), task);
                    }
                }
                Some("tool_result") => {
                    let Some(task) = b["tool_use_id"].as_str().and_then(|id| tasks.pending.remove(id)) else { continue };
                    let text = text_of(&b["content"]);
                    let id = text
                        .split("in background with ID: ")
                        .nth(1)
                        .or_else(|| text.split("Monitor started (task ").nth(1))
                        .map(|r| r.chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>())
                        .filter(|id| !id.is_empty());
                    if let Some(id) = id {
                        tasks.by_id.insert(id.clone(), Task { id, ..task });
                    }
                }
                _ => {
                    let text = b["text"].as_str().map(String::from).unwrap_or_else(|| text_of(b));
                    for n in text.split("<task-notification>").skip(1) {
                        if let Some(id) = tag(n, "task-id") {
                            let summary = tag(n, "summary").or_else(|| tag(n, "status")).unwrap_or("").to_string();
                            tasks.ended.insert(id.to_string(), summary);
                        }
                    }
                }
            }
        }
    }
}

// ---- what a shell is ----

/// A shell Claude Code runs a Bash call in.
pub fn is_tool_shell(command: &str) -> bool {
    command.contains("/shell-snapshots/snapshot-") && command.contains(" -c ")
}

/// The command a tool shell runs: what follows its `eval '`, unquoted.
pub fn shell_command(command: &str) -> Option<String> {
    let from = command.find(" eval '")? + " eval '".len();
    let mut out = String::new();
    let rest = &command[from..];
    let mut i = 0;
    let b = rest.as_bytes();
    while i < b.len() {
        if b[i] == b'\'' {
            if rest[i..].starts_with("'\\''") {
                out.push('\'');
                i += 4;
                continue;
            }
            return Some(out.trim().to_string());
        }
        let ch = rest[i..].chars().next()?;
        out.push(ch);
        i += ch.len_utf8();
    }
    Some(out.trim().to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Job,
    Server,
    Watch,
}

/// A command that serves or watches rather than finishes.
fn serves(command: &str) -> Option<Kind> {
    let c = format!(" {} ", command.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" "));
    const WATCH: &[&str] = &[" tail -f ", " tail -F ", " tail -n +1 -f ", " tail -n 0 -f ", " --watch ", " -w ", " fswatch ", " entr ", " watchexec ", " cargo watch ", " nodemon "];
    if WATCH.iter().any(|w| c.contains(&w.to_lowercase())) || c.contains(" tail ") && c.contains(" -f ") {
        return Some(Kind::Watch);
    }
    const SERVE: &[&str] = &[
        " npm run dev ",
        " npm run start ",
        " npm run serve ",
        " npm start ",
        " npm run preview ",
        " yarn dev ",
        " yarn start ",
        " yarn serve ",
        " pnpm dev ",
        " pnpm run dev ",
        " pnpm start ",
        " bun dev ",
        " bun run dev ",
        " next dev ",
        " next start ",
        " vite ",
        " vite dev ",
        " astro dev ",
        " nuxt dev ",
        " remix dev ",
        " webpack serve ",
        " webpack-dev-server ",
        " rails s ",
        " rails server ",
        " http.server ",
        " flask run ",
        " uvicorn ",
        " gunicorn ",
        " php -s ",
        " hugo server ",
        " jekyll serve ",
        " serverless offline ",
        " sls offline ",
        " firebase emulators",
        " ngrok ",
        " npx serve ",
        " python -m http.server ",
        " manage.py runserver ",
    ];
    if SERVE.iter().any(|s| c.contains(s)) || compose_up(&c) {
        return Some(Kind::Server);
    }
    None
}

/// `docker compose [options] up [options]` (or `docker-compose …`) without
/// `-d`, `--detach` or `--wait`: it holds the containers in the foreground,
/// a server left running. With them it returns at once.
fn compose_up(c: &str) -> bool {
    let toks: Vec<&str> = c.split_whitespace().collect();
    (0..toks.len()).any(|i| {
        let rest = match (toks[i], toks.get(i + 1)) {
            ("docker-compose", _) => &toks[i + 1..],
            ("docker", Some(&"compose")) => &toks[i + 2..],
            _ => return false,
        };
        // Its own words, up to the next command.
        let own: Vec<&str> = rest.iter().copied().take_while(|t| !matches!(*t, "&&" | "||" | ";" | "|")).collect();
        own.contains(&"up") && !own.iter().any(|t| matches!(*t, "-d" | "--detach" | "--wait") || (t.starts_with('-') && !t.starts_with("--") && t.len() > 2 && t.contains('d') && !t.contains('=')))
    })
}

/// What a background shell is: a Monitor or a watcher watches; a process
/// holding a listening port or a dev-server command serves; the rest is a job.
pub fn kind_of(task: Option<&Task>, commands: &[&str], listening: bool) -> Kind {
    if task.is_some_and(|t| t.kind == TaskKind::Monitor) {
        return Kind::Watch;
    }
    if listening {
        return Kind::Server;
    }
    let mut seen = None;
    for c in commands.iter().copied().chain(task.map(|t| t.command.as_str())) {
        match serves(c) {
            Some(Kind::Server) => return Kind::Server,
            Some(k) => seen = seen.or(Some(k)),
            None => {}
        }
    }
    // A pipeline whose only lasting part watches (`tail -f log | grep`) is a watch.
    match seen {
        Some(Kind::Watch) if task.is_none_or(|t| serves(&t.command) == Some(Kind::Watch)) => Kind::Watch,
        _ => Kind::Job,
    }
}

// ---- what is shown ----

/// A job a session has running.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    /// What it is, in words: the call's description, else its command.
    pub what: String,
    pub command: String,
    pub since_ms: u64,
    /// Its latest write (its log, its output); None: nothing written yet.
    pub progress_ms: Option<u64>,
    /// A process of it uses the CPU now.
    pub busy: bool,
    /// Quiet and idle for `QUIET_MS`: possibly stuck.
    pub stuck: bool,
    /// Its shell (or the pid it was declared with).
    pub pid: Option<u32>,
    /// Claude Code's task id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// `background` (seen), or `declared` (`tower job-running`).
    pub source: String,
    /// The file it writes its progress to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// What the session said of its progress (`tower job-running progress`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// A Monitor of the session watches it.
    pub watched: bool,
}

/// A server a session left running: not a job.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    pub what: String,
    pub pid: u32,
    pub ports: Vec<u16>,
}

/// How a session's jobs show in one place (a mission's session, the phone):
/// the one to name (the freshest that isn't stuck), and how many more.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Shown {
    pub what: String,
    pub since_ms: u64,
    pub progress_ms: Option<u64>,
    /// Every job of the session is quiet and idle.
    pub stuck: bool,
    pub more: usize,
    /// "running a job: …, since 00:21, last progress 00:33".
    pub line: String,
}

/// The job to name: the latest progress among those not stuck, else the latest.
pub fn main_job(jobs: &[Job]) -> Option<&Job> {
    let fresh = |j: &&Job| j.progress_ms.unwrap_or(j.since_ms);
    jobs.iter().filter(|j| !j.stuck).max_by_key(fresh).or_else(|| jobs.iter().max_by_key(fresh))
}

/// A command as a job's name when its call has no description: without
/// what only sets it up (`S=/tmp/x;`, `export K=v &&`, `cd dir &&`), so
/// `S=/tmp/x; cd /w/app && npm test` is named `npm test`.
pub fn name_of(command: &str) -> String {
    let mut c = command.trim();
    loop {
        let before = c;
        // `cd <dir>` then `&&` or `;`.
        if let Some(rest) = c.strip_prefix("cd ") {
            match rest.find("&&").map(|i| (i, 2)).into_iter().chain(rest.find(';').map(|i| (i, 1))).min() {
                Some((i, n)) => c = rest[i + n..].trim_start(),
                None => break,
            }
        }
        // `[export ]NAME=value` then `;` or `&&` (the value may be quoted).
        let a = c.strip_prefix("export ").unwrap_or(c);
        let name_len = a.chars().take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_').count();
        if name_len > 0 && !a.chars().next().is_some_and(|ch| ch.is_ascii_digit()) && a[name_len..].starts_with('=') {
            let v = &a[name_len + 1..];
            let mut end = 0;
            let mut quote: Option<char> = None;
            for (i, ch) in v.char_indices() {
                match (quote, ch) {
                    (None, '\'' | '"') => quote = Some(ch),
                    (Some(q), ch) if ch == q => quote = None,
                    (None, ch) if ch.is_whitespace() || ch == ';' || ch == '&' => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
                end = i + ch.len_utf8();
            }
            let after = v[end..].trim_start();
            if let Some(r) = after.strip_prefix("&&").or_else(|| after.strip_prefix(';')) {
                c = r.trim_start();
            }
        }
        if c == before || c.is_empty() {
            break;
        }
    }
    if c.is_empty() { command.trim().to_string() } else { c.to_string() }
}

pub fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>().trim_end()) } else { s }
}

/// "running a job: Train the 1.2B LoRA, since 00:21, last progress 00:33"
/// (and "(+1 more)", or "possibly stuck: quiet for 42 min").
pub fn line(jobs: &[Job], now_ms: u64) -> Option<String> {
    let j = main_job(jobs)?;
    let mut s = format!("running a job: {}, since {}", clip(&j.what, 70), crate::failover::when(j.since_ms, now_ms));
    if let Some(p) = j.progress_ms {
        s += &format!(", last progress {}", crate::failover::when(p, now_ms));
    }
    if !j.note.is_empty() {
        s += &format!(" ({})", clip(&j.note, 60));
    }
    if jobs.len() > 1 {
        s += &format!(" (+{} more)", jobs.len() - 1);
    }
    if jobs.iter().all(|j| j.stuck) {
        let quiet = now_ms.saturating_sub(j.progress_ms.unwrap_or(j.since_ms)) / 60_000;
        s += &format!("; possibly stuck: nothing written and no CPU for {quiet} min");
    }
    Some(s)
}

pub fn shown(jobs: &[Job], now_ms: u64) -> Option<Shown> {
    let j = main_job(jobs)?;
    Some(Shown {
        what: j.what.clone(),
        since_ms: j.since_ms,
        progress_ms: j.progress_ms,
        stuck: jobs.iter().all(|j| j.stuck),
        more: jobs.len() - 1,
        line: line(jobs, now_ms).unwrap_or_default(),
    })
}

// ---- one look at a session ----

/// What the system says now, for every session at once.
#[derive(Debug, Default)]
pub struct World {
    pub procs: Vec<Proc>,
    /// Open files of the processes looked at, by pid.
    pub files: HashMap<u32, Vec<Open>>,
    /// Listening ports, by pid.
    pub listen: HashMap<u32, Vec<u16>>,
    /// A path's last write (Unix ms), when it can be read.
    pub mtimes: HashMap<String, u64>,
}

impl World {
    pub fn children(&self, pid: u32) -> impl Iterator<Item = &Proc> {
        self.procs.iter().filter(move |p| p.ppid == pid)
    }

    /// `pid` and everything under it.
    pub fn subtree(&self, pid: u32) -> Vec<&Proc> {
        let mut out: Vec<&Proc> = self.procs.iter().filter(|p| p.pid == pid).collect();
        let mut i = 0;
        while i < out.len() {
            let parent = out[i].pid;
            out.extend(self.children(parent));
            i += 1;
        }
        out
    }

    /// The tool shells of the session's `claude`.
    pub fn shells(&self, claude: u32) -> Vec<&Proc> {
        self.children(claude).filter(|p| is_tool_shell(&p.command)).collect()
    }
}

/// A file a process writes: a regular path open for writing.
fn written(o: &Open) -> bool {
    (o.access.contains('w') || o.access.contains('u')) && o.name.starts_with('/') && !o.name.starts_with("/dev/")
}

/// The task id of a shell writing to Claude Code's `…/tasks/<id>.output`.
pub fn task_of(files: &[Open]) -> Option<String> {
    files.iter().filter(|o| o.fd == "1" || o.fd == "2").find_map(|o| {
        let (dir, file) = o.name.rsplit_once('/')?;
        (dir.ends_with("/tasks")).then(|| file.strip_suffix(".output").map(String::from)).flatten()
    })
}

/// A session's background shells, read: its jobs and its servers (watches
/// mark the jobs they watch, and count for nothing else).
pub fn look(claude: u32, world: &World, tasks: &Tasks, now_ms: u64) -> (Vec<Job>, Vec<Server>) {
    let mut jobs = vec![];
    let mut servers = vec![];
    let mut watches: Vec<String> = vec![];
    for shell in world.shells(claude) {
        let tree = world.subtree(shell.pid);
        let none = vec![];
        let shell_files = world.files.get(&shell.pid).unwrap_or(&none);
        // Its task: by the output file on its stdout; a command that redirects
        // itself whole (`for … done > log`) moves that, so then by its command.
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let own = shell_command(&shell.command).map(|c| flat(&c));
        let task_id = task_of(shell_files).or_else(|| {
            let own = own.as_deref().filter(|c| !c.is_empty())?;
            tasks.by_id.values().filter(|t| t.kind == TaskKind::Bash && flat(&t.command) == own).max_by_key(|t| t.at_ms).map(|t| t.id.clone())
        });
        // A task Claude Code said is over, whose shell lingers a moment: done.
        if task_id.as_ref().is_some_and(|id| tasks.ended.contains_key(id)) {
            continue;
        }
        let task = task_id.as_ref().and_then(|id| tasks.by_id.get(id));
        let command = task.map(|t| t.command.clone()).filter(|c| !c.is_empty()).or_else(|| shell_command(&shell.command)).unwrap_or_default();
        let commands: Vec<&str> = std::iter::once(command.as_str()).chain(tree.iter().skip(1).map(|p| p.command.as_str())).collect();
        let ports: Vec<u16> = tree.iter().flat_map(|p| world.listen.get(&p.pid).cloned().unwrap_or_default()).collect();
        let kind = kind_of(task, &commands, !ports.is_empty());
        let what = task.map(|t| t.description.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| clip(&name_of(&command), 80));
        match kind {
            Kind::Watch => watches.push(task.map(|t| t.command.clone()).unwrap_or(command)),
            Kind::Server => servers.push(Server { what, pid: shell.pid, ports }),
            Kind::Job => {
                let writes: Vec<&Open> = tree.iter().flat_map(|p| world.files.get(&p.pid).map(|f| f.iter().filter(|o| written(o)).collect::<Vec<_>>()).unwrap_or_default()).collect();
                // Its log over its task output (often empty when the command redirects).
                let output = writes.iter().find(|o| !o.name.contains("/tasks/")).or(writes.first()).map(|o| o.name.clone());
                let progress_ms = writes.iter().filter_map(|o| world.mtimes.get(&o.name).copied()).max();
                let since_ms = now_ms.saturating_sub(shell.secs * 1000);
                let busy = tree.iter().any(|p| p.cpu >= BUSY_CPU);
                let quiet = now_ms.saturating_sub(progress_ms.unwrap_or(since_ms));
                jobs.push(Job {
                    what,
                    command: clip(&command, 300),
                    since_ms,
                    progress_ms,
                    busy,
                    stuck: !busy && quiet > QUIET_MS,
                    pid: Some(shell.pid),
                    task: task_id.clone(),
                    source: "background".into(),
                    output,
                    note: String::new(),
                    watched: false,
                });
            }
        }
    }
    // A job a watch reads (its log): watched.
    for j in &mut jobs {
        if let Some(out) = &j.output {
            j.watched = watches.iter().any(|w| w.contains(out.as_str()));
        }
    }
    (jobs, servers)
}

/// Jobs a session declared (`tower job-running`), by session: kept by the daemon.
pub type Declared = BTreeMap<String, Vec<Job>>;

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_794_620_000_000;

    fn shell(pid: u32, ppid: u32, secs: u64, cmd: &str) -> Proc {
        Proc {
            pid,
            ppid,
            secs,
            cpu: 0.0,
            command: format!("/bin/zsh -c source '/h/.claude/shell-snapshots/snapshot-zsh-1-x.sh' 2>/dev/null || true && eval '{}' < /dev/null && pwd -P >| /tmp/cwd", cmd.replace('\'', "'\\''")),
        }
    }

    fn open(fd: &str, a: &str, name: &str) -> Open {
        Open { fd: fd.into(), access: a.into(), name: name.into() }
    }

    #[test]
    fn a_short_helper_is_no_job_until_it_outlives_the_turn_or_runs_two_minutes() {
        let at = |secs: u64| Job { since_ms: NOW - secs * 1000, source: "background".into(), ..Default::default() };
        // While the session works: a helper of a few seconds, or of a minute, isn't a job.
        assert!(!counts(&at(5), NOW, false, false));
        assert!(!counts(&at(90), NOW, false, false));
        // Two minutes in: a job, turn or no turn.
        assert!(counts(&at(120), NOW, false, false));
        // Its session's turn is over and it still runs: a job (but not one that only just started).
        assert!(counts(&at(20), NOW, true, false));
        assert!(!counts(&at(5), NOW, true, false));
        // Counted once: it stays a job when the session works again.
        assert!(counts(&at(30), NOW, false, true));
        // Declared: always.
        assert!(counts(&Job { source: "declared".into(), ..at(1) }, NOW, false, false));
    }

    #[test]
    fn reads_ps_lsof_and_listening_ports() {
        let ps = parse_ps(" 66615 66612       11:46  98.5 .venv/bin/python -u scripts/train_lora.py --model 1.2b\n 82737 82686    07:10:32   0.0 uv run blender-mcp\n  9 1 2-03:04:05 0.1 x\n");
        assert_eq!(ps[0], Proc { pid: 66615, ppid: 66612, secs: 706, cpu: 98.5, command: ".venv/bin/python -u scripts/train_lora.py --model 1.2b".into() });
        assert_eq!((ps[1].secs, ps[2].secs), (25_832, 183_845));
        let files = parse_lsof("p66612\nf0\nar\nn/dev/null\nf1\naw\nn/tmp/p/tasks/bevu4s93d.output\np66615\nf1\naw\nn/w/data/train.log\n");
        assert_eq!(task_of(&files[&66612]), Some("bevu4s93d".into()));
        assert_eq!(files[&66615], [open("1", "w", "/w/data/train.log")]);
        let listen = parse_listen("p4100\nf22\nn*:3000\nf23\nn[::1]:3000\np4200\nn127.0.0.1:5173\n");
        assert_eq!((listen[&4100].clone(), listen[&4200].clone()), (vec![3000], vec![5173]));
    }

    #[test]
    fn a_tool_shells_command_is_its_eval() {
        let s = shell(1, 2, 3, "python -c 'print(1)' > out.log");
        assert!(is_tool_shell(&s.command));
        assert_eq!(shell_command(&s.command).as_deref(), Some("python -c 'print(1)' > out.log"));
        assert!(!is_tool_shell("uv run blender-mcp"));
    }

    #[test]
    fn the_transcript_ties_task_ids_to_what_they_are_and_says_which_ended() {
        let jsonl = [
            r#"{"type":"assistant","timestamp":"2026-11-14T00:21:27.106Z","message":{"content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"python train.py > log 2>&1","description":"Train the model","run_in_background":true}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"Command running in background with ID: bevu4s93d. Output is being written to: /tmp/p/tasks/bevu4s93d.output."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu2","name":"Monitor","input":{"command":"tail -f log | grep val","description":"val loss"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu2","content":[{"type":"text","text":"Monitor started (task bd49c9q53, expires in 30m)"}]}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu3","name":"Bash","input":{"command":"ls","description":"List"}}]}}"#,
            r#"{"type":"user","message":{"content":"<task-notification>\n<task-id>bold1</task-id>\n<status>completed</status>\n<summary>Background command \"Old\" completed (exit code 0)</summary>\n</task-notification>"}}"#,
        ]
        .join("\n");
        let mut t = Tasks::default();
        read_tasks(&jsonl, &mut t);
        let train = &t.by_id["bevu4s93d"];
        assert_eq!((train.kind, train.description.as_str(), train.command.as_str()), (TaskKind::Bash, "Train the model", "python train.py > log 2>&1"));
        assert!(train.at_ms > 0);
        assert_eq!(t.by_id["bd49c9q53"].kind, TaskKind::Monitor);
        assert_eq!(t.by_id.len(), 2, "a call in the foreground is no task");
        assert_eq!(t.ended["bold1"], "Background command \"Old\" completed (exit code 0)");
    }

    #[test]
    fn jobs_servers_and_watches_are_told_apart() {
        let bash = |c: &str| Task { kind: TaskKind::Bash, command: c.into(), ..Default::default() };
        let mon = Task { kind: TaskKind::Monitor, command: "until [ -f x ]; do sleep 1; done".into(), ..Default::default() };
        assert_eq!(kind_of(Some(&bash("python scripts/train_lora.py > log 2>&1")), &[], false), Kind::Job);
        assert_eq!(kind_of(Some(&bash("bash scripts/post_train.sh")), &["sleep 30"], false), Kind::Job);
        assert_eq!(kind_of(Some(&mon), &[], false), Kind::Watch, "a Monitor watches");
        for c in ["npm run dev", "pnpm dev --port 3001", "npx next dev", "vite", "uvicorn app:app --reload", "python -m http.server 8000", "cd web && yarn start"] {
            assert_eq!(kind_of(Some(&bash(c)), &[], false), Kind::Server, "{c}");
        }
        assert_eq!(kind_of(Some(&bash("node scripts/odd.js")), &[], true), Kind::Server, "it holds a listening port");
        assert_eq!(kind_of(Some(&bash("tail -f data/train.log | grep val")), &[], false), Kind::Watch);
        assert_eq!(kind_of(None, &["cargo build --release"], false), Kind::Job);
        assert_eq!(kind_of(None, &["cargo test -- --watch"], false), Kind::Watch);
        // docker compose in the foreground holds its containers: a server; detached it returns.
        for c in ["docker compose -f docker-compose.dev.yml up --build > /tmp/x.log 2>&1", "docker-compose up", "cd app && docker compose --profile dev up web"] {
            assert_eq!(kind_of(Some(&bash(c)), &[], false), Kind::Server, "{c}");
        }
        for c in ["docker compose -f x.yml up -d", "docker compose up --detach && npm test", "docker-compose up --wait", "docker compose up -dV", "docker compose build", "docker compose -f x.yml run --rm migrate"] {
            assert_eq!(kind_of(Some(&bash(c)), &[], false), Kind::Job, "{c}");
        }
    }

    #[test]
    fn a_job_without_a_description_is_named_by_what_it_runs() {
        assert_eq!(name_of("S=/private/tmp/claude-501/x/scratchpad; until [ -f \"$S/done\" ]; do sleep 5; done"), "until [ -f \"$S/done\" ]; do sleep 5; done");
        assert_eq!(name_of("cd /Users/me/Work/app && S=/tmp/x; npm test -- --runInBand"), "npm test -- --runInBand");
        assert_eq!(name_of("export K='a b' && cd \"/w/my app\"; make build"), "make build");
        assert_eq!(name_of("A=1 B=2 make"), "A=1 B=2 make", "variables for the command itself stay");
        assert_eq!(name_of("cd /w/app"), "cd /w/app", "nothing after it: as it was");
        assert_eq!(name_of("python train.py > log 2>&1"), "python train.py > log 2>&1");
    }

    /// The t12r case: training (writes its log), a post-training script
    /// that waits (sleeps), a Monitor that tails the log, an MCP server.
    fn world() -> (World, Tasks) {
        let mut w = World::default();
        w.procs = vec![
            shell(10, 1, 700, "python train.py > /w/train.log 2>&1"),
            Proc { pid: 11, ppid: 10, secs: 700, cpu: 97.0, command: "python train.py".into() },
            shell(20, 1, 700, "bash post.sh > /w/post.log 2>&1"),
            Proc { pid: 21, ppid: 20, secs: 700, cpu: 0.0, command: "bash post.sh".into() },
            Proc { pid: 22, ppid: 21, secs: 10, cpu: 0.0, command: "sleep 30".into() },
            shell(30, 1, 690, "tail -n +1 -f /w/train.log | grep val"),
            Proc { pid: 31, ppid: 30, secs: 690, cpu: 0.0, command: "tail -n +1 -f /w/train.log".into() },
            Proc { pid: 40, ppid: 1, secs: 9000, cpu: 0.0, command: "uv run blender-mcp".into() },
            shell(50, 1, 300, "npm run dev"),
            Proc { pid: 51, ppid: 50, secs: 300, cpu: 1.0, command: "node next dev".into() },
        ];
        w.files.insert(10, vec![open("1", "w", "/tmp/p/tasks/btrain.output")]);
        w.files.insert(11, vec![open("1", "w", "/w/train.log"), open("3", "r", "/w/data.jsonl")]);
        w.files.insert(20, vec![open("1", "w", "/tmp/p/tasks/bpost.output")]);
        w.files.insert(21, vec![open("1", "w", "/w/post.log")]);
        w.files.insert(30, vec![open("1", "w", "/tmp/p/tasks/bmon.output")]);
        w.files.insert(50, vec![open("1", "w", "/tmp/p/tasks/bdev.output")]);
        w.listen.insert(51, vec![3000]);
        w.mtimes.insert("/w/train.log".into(), NOW - 20_000);
        w.mtimes.insert("/tmp/p/tasks/btrain.output".into(), NOW - 700_000);
        w.mtimes.insert("/w/post.log".into(), NOW - 40 * 60_000);
        w.mtimes.insert("/tmp/p/tasks/bpost.output".into(), NOW - 40 * 60_000);
        let mut t = Tasks::default();
        let task = |id: &str, kind, d: &str, c: &str| Task { id: id.into(), kind, description: d.into(), command: c.into(), at_ms: 0 };
        t.by_id.insert("btrain".into(), task("btrain", TaskKind::Bash, "Train the 1.2B LoRA", "python train.py > /w/train.log 2>&1"));
        t.by_id.insert("bpost".into(), task("bpost", TaskKind::Bash, "Post-training pipeline", "bash post.sh > /w/post.log 2>&1"));
        t.by_id.insert("bmon".into(), task("bmon", TaskKind::Monitor, "val loss", "tail -n +1 -f /w/train.log | grep val"));
        t.by_id.insert("bdev".into(), task("bdev", TaskKind::Bash, "Start the dev server", "npm run dev"));
        (w, t)
    }

    #[test]
    fn a_session_with_training_running_has_a_job_not_the_watch_or_the_servers() {
        let (w, t) = world();
        let (jobs, servers) = look(1, &w, &t, NOW);
        let names: Vec<&str> = jobs.iter().map(|j| j.what.as_str()).collect();
        assert_eq!(names, ["Train the 1.2B LoRA", "Post-training pipeline"], "the Monitor watches, the MCP server isn't a tool shell, the dev server serves");
        let train = &jobs[0];
        assert_eq!((train.task.as_deref(), train.output.as_deref(), train.progress_ms, train.busy, train.stuck, train.watched), (Some("btrain"), Some("/w/train.log"), Some(NOW - 20_000), true, false, true));
        assert_eq!(train.since_ms, NOW - 700_000);
        let post = &jobs[1];
        assert!(post.stuck && !post.busy, "quiet for 40 min and idle");
        assert_eq!(servers, [Server { what: "Start the dev server".into(), pid: 50, ports: vec![3000] }]);
        // Shown: the training (fresh), one more; not stuck while the training writes.
        let s = shown(&jobs, NOW).unwrap();
        assert_eq!((s.what.as_str(), s.more, s.stuck), ("Train the 1.2B LoRA", 1, false));
        assert!(s.line.starts_with("running a job: Train the 1.2B LoRA, since ") && s.line.contains(", last progress ") && s.line.ends_with("(+1 more)"), "{}", s.line);
        // Every job quiet and idle: possibly stuck.
        let mut quiet = jobs.clone();
        for j in &mut quiet {
            j.stuck = true;
        }
        assert!(line(&quiet, NOW).unwrap().contains("possibly stuck"));
        // A task Claude Code said ended, whose shell lingers: gone.
        let mut t2 = t.clone();
        t2.ended.insert("btrain".into(), "completed".into());
        assert_eq!(look(1, &w, &t2, NOW).0.len(), 1);
        // No transcript: named by its command, still a job.
        let (jobs, _) = look(1, &w, &Tasks::default(), NOW);
        assert_eq!(jobs[0].what, "python train.py > /w/train.log 2>&1");
        // A command that redirects itself whole: its stdout is its log, not the
        // task's file; known by its command instead.
        let mut w2 = World { procs: w.procs.clone(), files: w.files.clone(), listen: w.listen.clone(), mtimes: w.mtimes.clone() };
        w2.files.insert(10, vec![open("1", "w", "/w/train.log")]);
        let (jobs, _) = look(1, &w2, &t, NOW);
        assert_eq!((jobs[0].what.as_str(), jobs[0].task.as_deref()), ("Train the 1.2B LoRA", Some("btrain")));
        assert!(look(999, &w, &t, NOW).0.is_empty());
    }
}
