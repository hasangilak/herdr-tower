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
//! - **Waiters.** A job that only waits now (an `until`/`while` loop that
//!   sleeps, a leading `sleep`, `wait <pid>`; nothing but its `sleep` and
//!   short checks running: `waits_only`, `only_waiting`) is never stuck:
//!   what it waits on may take the night. Waiting on a process gone for an
//!   hour, or on a condition nothing moved for 6 h, it is worth a soft note
//!   (`overdue`); a timer ends by itself.
//! - **Said so.** A session can also say it itself: `tower job-running
//!   start|progress|end '<what>' [--pid <pid>]` (`declared`).
//!
//! The daemon looks (`crate::daemon::running`); this module is the reading.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A job with no write and no CPU for this long may be stuck.
pub const QUIET_MS: u64 = 30 * 60_000;

/// `QUIET_MS`, or `HERDER_TOWER_JOB_QUIET_SECS` (a test tower's, to see a
/// quiet job flagged without waiting half an hour).
pub fn quiet_ms() -> u64 {
    std::env::var("HERDER_TOWER_JOB_QUIET_SECS").ok().and_then(|s| s.trim().parse::<u64>().ok()).map(|s| s * 1000).unwrap_or(QUIET_MS)
}

/// Words of a finite step: a call that says one of them ends by itself.
const FINITE: &[&str] = &[
    "build", "builds", "rebuild", "test", "tests", "testing", "train", "training", "retrain", "migrate", "migration", "migrations", "import",
    "export", "seed", "install", "lint", "typecheck", "bench", "benchmark", "eval", "evaluate", "deploy", "compile", "generate", "backfill",
    "sync", "download", "upload", "convert", "score", "scan", "check", "checks", "run once", "evals", "evaluation", "evaluating", "scoring",
    "benchmarks", "baseline", "baselines", "finetune", "fine-tune", "fine-tuning", "tune", "tuning", "label", "labels", "labelling", "labeling",
    "migrating", "importing", "exporting", "generating", "building", "compiling", "index", "indexing", "reindex", "embed", "embedding",
    "embeddings", "crawl", "scrape", "transcode", "render", "predict", "predictions", "inference", "infer", "validate", "validation", "verify",
    "sweep", "pytest", "jest", "vitest", "mocha", "cypress", "playwright test", "cargo test", "go test", "dbt run", "wait", "waits",
    "waiting",
];

/// Words of serving that outweigh a finite one in a call's description:
/// "Serve the build", "Start the dev server and its tests" stays a server.
fn strong_serving(description: &str) -> bool {
    let w = words_of(description);
    let has = |x: &str| w.iter().any(|y| y == x);
    let pair = |a: &str, b: &str| w.windows(2).any(|p| p[0] == a && p[1] == b);
    ["server", "servers", "serve", "serves", "serving", "watch", "watcher", "watching"].iter().any(|x| has(x))
        || ["bring", "spin", "compose", "stack"].iter().any(|a| pair(a, "up"))
}

/// Words of a finite step in `text` ("Evaluate v1 and v2", "eval_real_both.sh").
fn names_finite(text: &str) -> bool {
    let lower = text.to_lowercase();
    let w = words_of(text);
    // `eval_real_both` reads as eval, real, both; `fine-tune` as one.
    let parts: Vec<String> = w.iter().flat_map(|x| x.split('_').map(String::from).collect::<Vec<_>>()).collect();
    FINITE.iter().any(|f| if f.contains(' ') || f.contains('-') { lower.contains(f) } else { w.iter().chain(parts.iter()).any(|y| y == f) })
}

/// A call's description that names finite work and no serving: "Evaluate v1
/// and v2 on the real labelled queries", "Run the test suite". It stays a
/// job though something under it listens (a model server its eval starts).
pub fn described_finite(description: &str) -> bool {
    names_finite(description) && !strong_serving(description)
}

/// A command that does finite work and is no server itself (`python
/// scripts/run_baselines.py`, `bash eval_real_both.sh`, `pytest -x`): a
/// server under it is its helper, not what the shell is.
pub fn finite_command(command: &str) -> bool {
    names_finite(&work_words(&name_of(command))) && serves(command) != Some(Kind::Server)
}

/// The words of a command that say what it does: its programs, the names
/// of the scripts it runs (`scripts/eval_real_both.sh` → `eval_real_both`)
/// and its plain words, not the folders it names or the files it writes
/// to (`> /tmp/…zendesk-ticket-import…/x.output` is no import).
fn work_words(command: &str) -> String {
    let toks: Vec<&str> = command.split_whitespace().collect();
    let mut out: Vec<String> = vec![];
    let mut redirect = false;
    for t in toks {
        if redirect {
            redirect = false;
            continue;
        }
        if matches!(t, ">" | ">>" | "2>" | "2>>" | "&>" | "1>" | "<") {
            redirect = true;
            continue;
        }
        if t.starts_with('>') || t.starts_with("2>") || t.starts_with("&>") || t.starts_with('<') {
            continue;
        }
        // NAME=value before a command: neither.
        if t.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')) {
            continue;
        }
        if t.contains('/') {
            let base = t.trim_matches(|c| c == '"' || c == '\'').rsplit('/').next().unwrap_or("");
            // A script or a program by its path; a folder or a URL says nothing.
            let script = base.rsplit_once('.').is_some_and(|(_, ext)| matches!(ext, "py" | "sh" | "js" | "mjs" | "cjs" | "ts" | "rb" | "pl" | "bash" | "zsh"));
            if script || (!base.is_empty() && !base.contains('.') && !t.contains("://") && !t.ends_with('/') && out.is_empty()) {
                out.push(base.to_string());
            }
            continue;
        }
        out.push(t.to_string());
    }
    out.join(" ")
}

fn words_of(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect()
}

/// A call's description that says it serves or watches and nothing finite:
/// "Start Postgres and dev servers in Docker", "Serve the docs", "Watch the
/// styles", "Bring up the stack"; not "Restart v2 training", "Start the import".
pub fn described_server(description: &str) -> bool {
    let w = words_of(description);
    let has = |x: &str| w.iter().any(|y| y == x);
    let pair = |a: &str, b: &str| w.windows(2).any(|p| p[0] == a && p[1] == b);
    let serving = ["server", "servers", "serve", "serves", "serving", "watch", "watcher", "watching", "start", "starts", "starting"].iter().any(|x| has(x))
        || pair("dev", "server")
        || ["bring", "spin", "compose", "stack"].iter().any(|a| pair(a, "up"));
    let finite = FINITE.iter().any(|f| if f.contains(' ') { description.to_lowercase().contains(f) } else { has(f) });
    serving && !finite
}
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
    /// The session said it serves (a server, localhost:<port>, listening)
    /// in its words after it started it.
    pub said_serving: bool,
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
/// How soon after starting a task the session's words about serving count for it.
const SAID_WITHIN_MS: u64 = 10 * 60_000;

/// Words that say something serves: "the dev server is up on
/// localhost:3000", "listening on 8091", "serving the docs".
fn says_serving(text: &str) -> bool {
    let l = text.to_lowercase();
    let words = ["dev server", "server is up", "server is running", "server running", "serving ", "listening on", "localhost:", "127.0.0.1:", "0.0.0.0:", "running on port", "up on port"];
    // "up on :7899", "runs at :3000".
    let port = [" on :", " at :"].iter().any(|w| l.match_indices(w).any(|(i, _)| l[i + w.len()..].chars().take_while(char::is_ascii_digit).count() >= 2));
    words.iter().any(|w| l.contains(w)) || port
}

pub fn read_tasks(lines: &str, tasks: &mut Tasks) {
    for line in lines.lines() {
        let assistant_text = line.contains("\"type\":\"assistant\"") && line.contains("\"text\"") && says_serving(line);
        if !(assistant_text || line.contains("\"Bash\"") || line.contains("\"Monitor\"") || line.contains("in background with ID") || line.contains("moved to the background") || line.contains("Monitor started") || line.contains("task-notification")) {
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
                    // Every Bash call: one in the foreground may yet be moved to the background.
                    let kind = match b["name"].as_str() {
                        Some("Bash") => TaskKind::Bash,
                        Some("Monitor") => TaskKind::Monitor,
                        _ => continue,
                    };
                    let task = Task {
                        id: String::new(),
                        kind,
                        description: input["description"].as_str().unwrap_or("").trim().to_string(),
                        command: input["command"].as_str().unwrap_or("").trim().to_string(),
                        at_ms: ts_ms(&v),
                        said_serving: false,
                    };
                    if let Some(id) = b["id"].as_str() {
                        tasks.pending.insert(id.to_string(), task);
                    }
                }
                Some("tool_result") => {
                    let Some(task) = b["tool_use_id"].as_str().and_then(|id| tasks.pending.remove(id)) else { continue };
                    let text = text_of(&b["content"]);
                    // Its result says it went to the background (with its id); else it was a call in the foreground.
                    let id = text
                        .split("in background with ID: ")
                        .nth(1)
                        .or_else(|| text.split("moved to the background (ID: ").nth(1))
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
                        // Ended: a status (completed, failed, stopped, killed), or a
                        // Monitor that expired. A Monitor's event isn't its end.
                        let n = n.split("</task-notification>").next().unwrap_or(n);
                        if let Some(id) = tag(n, "task-id")
                            && (tag(n, "status").is_some_and(|s| !s.trim().is_empty() && s.trim() != "running") || n.contains("Monitor expired"))
                        {
                            let summary = tag(n, "summary").or_else(|| tag(n, "status")).unwrap_or("").to_string();
                            tasks.ended.insert(id.to_string(), summary);
                        }
                    }
                    // The session's own words: a task it started a while ago that it says serves.
                    if v["type"] == "assistant" && says_serving(&text) {
                        let at = ts_ms(&v);
                        for t in tasks.by_id.values_mut() {
                            if !tasks.ended.contains_key(&t.id) && t.at_ms <= at && at.saturating_sub(t.at_ms) < SAID_WITHIN_MS {
                                t.said_serving = true;
                            }
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

/// The command a tool shell runs: what follows its `eval '`, unquoted (a
/// quote inside written `'\''` or `'"'"'`).
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
            // Claude Code's own way: `'"'"'`.
            if rest[i..].starts_with("'\"'\"'") {
                out.push('\'');
                i += 5;
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
    // Programs by their names (`./node_modules/.bin/next dev` is `next dev`).
    let c = format!(
        " {} ",
        command
            .to_lowercase()
            .split_whitespace()
            .map(|t| if t.contains('/') && !t.contains("://") && !t.starts_with('>') { t.rsplit('/').next().unwrap_or(t) } else { t })
            .collect::<Vec<_>>()
            .join(" ")
    );
    const WATCH: &[&str] = &[" --watch ", " fswatch ", " entr ", " watchexec ", " cargo watch ", " nodemon ", " --no-buffer "];
    // (`curl -N` by its case: `-n` is another flag.)
    if WATCH.iter().any(|w| c.contains(w)) || segments(&c).iter().any(|s| follows(s) || watch_flag(s)) || segments(command).iter().any(|s| curl_stream(s)) {
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
    // `<tool> dev` on a port (`storybook dev -p 6026`), and a daemon left running.
    let dev_on_port = segments(&c).iter().any(|s| {
        let t: Vec<&str> = s.split_whitespace().collect();
        t.windows(2).any(|w| w[1] == "dev" && !w[0].starts_with('-')) && t.iter().any(|x| matches!(*x, "-p" | "--port") || x.starts_with("--port="))
    });
    if dev_on_port || c.contains(" storybook dev ") || c.contains(" start-storybook ") || segments(&c).iter().any(|s| s.split_whitespace().skip_while(|t| matches!(*t, "exec" | "nohup" | "env") || t.contains('=')).nth(1) == Some("daemon")) {
        return Some(Kind::Server);
    }
    // A command that sets the port it serves on, or runs a dev-server script, serves too.
    let sets_port = c.split_whitespace().any(|t| t.strip_prefix("port=").is_some_and(|v| !v.is_empty() && v.chars().all(|ch| ch.is_ascii_digit())));
    let dev_server = ["dev-server", "devserver", "dev_server"].iter().any(|w| c.contains(w));
    if SERVE.iter().any(|s| c.contains(s)) || compose_up(&c) || docker_run(&c) || sets_port || dev_server || server_script(&c) {
        return Some(Kind::Server);
    }
    None
}

/// A command line's simple commands (split at `&&`, `||`, `;`, `|`, `(`, `)`, `$(`), each trimmed.
fn segments(c: &str) -> Vec<String> {
    c.replace("&&", ";").replace("||", ";").replace("$(", ";").replace(['|', '(', ')', '`'], ";").split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// `tail -f` / `-F` / `--follow` in one simple command (not `[ -f x ]` and a `tail -5` elsewhere).
fn follows(segment: &str) -> bool {
    let t: Vec<&str> = segment.split_whitespace().collect();
    t.first() == Some(&"tail") && t.iter().skip(1).any(|a| a.starts_with("--follow") || (a.starts_with('-') && !a.starts_with("--") && (a.contains('f') || a.contains('F'))))
}

/// A watcher's `-w` (`tsc -w`, `webpack -w`): not `grep -w`, `wc -w`, `curl -w`.
fn watch_flag(segment: &str) -> bool {
    let t: Vec<&str> = segment.split_whitespace().collect();
    let prog = t.iter().find(|x| !x.contains('=')).copied().unwrap_or("");
    let prog = if matches!(prog, "npx" | "pnpm" | "yarn" | "bunx") { t.iter().skip_while(|x| **x != prog).nth(1).copied().unwrap_or("") } else { prog };
    matches!(prog, "tsc" | "webpack" | "rollup" | "esbuild" | "sass" | "babel" | "tailwindcss" | "postcss" | "coffee" | "lessc" | "jest" | "vitest" | "elm-live" | "parcel") && t.contains(&"-w")
}

/// `curl -N` (or with `-sN`, `-skN`…): a stream it keeps reading (`/v1/events`).
fn curl_stream(segment: &str) -> bool {
    let t: Vec<&str> = segment.split_whitespace().collect();
    t.first() == Some(&"curl") && t.iter().skip(1).any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('N'))
}

/// `docker run …` in the foreground that isn't a one-off: without `--rm`
/// (a one-off removes itself when it ends) and without `-d` (that returns
/// at once): a container left running.
fn docker_run(c: &str) -> bool {
    let toks: Vec<&str> = c.split_whitespace().collect();
    (0..toks.len()).any(|i| {
        if !(toks[i] == "docker" && toks.get(i + 1) == Some(&"run")) {
            return false;
        }
        let own: Vec<&str> = toks[i + 2..].iter().copied().take_while(|t| !matches!(*t, "&&" | "||" | ";" | "|")).collect();
        !own.iter().any(|t| matches!(*t, "--rm" | "-d" | "--detach") || (t.starts_with('-') && !t.starts_with("--") && t.len() > 2 && t.contains('d') && !t.contains('=')))
    })
}

/// A program named as a server (`node server.js`, `python serve.py`,
/// `./scripts/start-server.sh`) with no finite word in the command.
fn server_script(c: &str) -> bool {
    let named = c.split_whitespace().any(|t| {
        let base = t.rsplit('/').next().unwrap_or(t);
        let stem = base.split('.').next().unwrap_or(base);
        matches!(stem, "server" | "serve" | "start-server" | "run-server" | "devserver" | "dev-server")
    });
    named && !words_of(c).iter().any(|w| FINITE.contains(&w.as_str()))
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
    // Its call names finite work and no serving: a job, whatever listens under it.
    if task.is_some_and(|t| described_finite(&t.description)) {
        return Kind::Job;
    }
    if listening {
        return Kind::Server;
    }
    // Its call says it serves or watches (and nothing finite): a server left running.
    if task.is_some_and(|t| described_server(&t.description)) {
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
    /// Quiet, but another job of the session is at work: it waits on that one (`siblings`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub waits_on: String,
    /// Said to wait (`tower job-running wait … --until`): for what, and until when at most.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub waiting: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_until_ms: Option<u64>,
    /// It only waits now (`waits_only`, `only_waiting`): on what kind of
    /// thing, and on what in words (`after_training.sh`, `data/run.log`,
    /// `sleep 3600`). Never stuck: at most a later, softer note (`overdue`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<Wait>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub waiter: String,
    /// It waits on a process that is gone: its loop should end within its next sleep.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub target_gone: bool,
}

/// What a job that only waits waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wait {
    /// A process (`until ! pgrep -f x`, `while kill -0 <pid>`, `wait <pid>`, `tail --pid`).
    Process,
    /// A condition: a file it polls (`until grep -q DONE run.log`, `until [ -f done ]`), a reply, a port.
    Condition,
    /// Time only (`sleep 3600; …`): it ends by itself.
    Timer,
}

/// A waiter's process gone for this long (its loop should have ended within
/// a sleep): a soft note. Twice the quiet time (an hour).
pub fn wait_gone_ms() -> u64 {
    2 * quiet_ms()
}

/// A waiter on a condition whose files haven't changed for this long, with
/// no other job of its session at work: a soft note. Twelve times the quiet
/// time (6 h, as long as a said wait holds).
pub fn wait_long_ms() -> u64 {
    12 * quiet_ms()
}

/// Why a waiter is worth a soft note now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overdue {
    /// What it waits on has been gone this long (ms), and it still waits.
    Gone(u64),
    /// It has waited this long (ms) on a condition that nothing moved.
    Long(u64),
}

/// A waiter of `jobs` (at `i`) worth a soft note: what it waits on gone
/// since `gone_since` for `wait_gone_ms`; or waiting on a condition with
/// nothing it watches written for `wait_long_ms` while no other job of the
/// session is at work. A timer ends by itself, and a live target is no news.
pub fn overdue(jobs: &[Job], i: usize, gone_since: Option<u64>, now_ms: u64) -> Option<Overdue> {
    let j = jobs.get(i)?;
    match j.wait? {
        Wait::Process => {
            let gone = now_ms.saturating_sub(gone_since.filter(|_| j.target_gone)?);
            (gone > wait_gone_ms()).then_some(Overdue::Gone(gone))
        }
        Wait::Condition => {
            let quiet = now_ms.saturating_sub(j.progress_ms.unwrap_or(j.since_ms));
            let other_at_work = jobs.iter().enumerate().any(|(k, o)| k != i && (o.busy || now_ms.saturating_sub(o.progress_ms.unwrap_or(o.since_ms)) <= quiet_ms()));
            (quiet > wait_long_ms() && !other_at_work).then_some(Overdue::Long(quiet))
        }
        Wait::Timer => None,
    }
}

/// A command line's simple commands, split outside quotes at `;`, `&`,
/// `|`, `(`, `)`, newlines and backquotes (`$(` opens one too): the `|` in
/// `grep -qE 'DONE|FAILED' run.log` stays in its pattern.
fn simple_commands(c: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut push = |cur: &mut String| {
        let t = cur.trim().trim_end_matches('$').trim().to_string();
        if !t.is_empty() {
            out.push(t);
        }
        cur.clear();
    };
    for ch in c.chars() {
        match (quote, ch) {
            (None, '\'' | '"') => {
                quote = Some(ch);
                cur.push(ch);
            }
            (Some(q), ch) if ch == q => {
                quote = None;
                cur.push(ch);
            }
            (None, ';' | '&' | '|' | '(' | ')' | '\n' | '`') => push(&mut cur),
            _ => cur.push(ch),
        }
    }
    push(&mut cur);
    out
}

/// A simple command without the shell words that lead it (`do`, `then`,
/// `until`, `while`, `!`, `{`): `do sleep 30` is `sleep 30`. And whether
/// one of them opened a loop.
fn bare(segment: &str) -> (bool, &str) {
    let mut s = segment.trim();
    let mut looped = false;
    loop {
        let before = s;
        for k in ["do ", "then ", "else ", "elif ", "if ", "until ", "while ", "! ", "{ "] {
            if let Some(r) = s.strip_prefix(k) {
                looped |= k == "until " || k == "while ";
                s = r.trim_start();
            }
        }
        if s == before {
            return (looped, s);
        }
    }
}

/// A command that only waits, at least at first: an `until`/`while` loop
/// that sleeps (`until ! pgrep -f x; do sleep 60; done; tail log`), one
/// that starts with a `sleep`, or a `wait <pid>` / `tail --pid` /
/// `caffeinate -w`. What it runs after the wait is work again; the look
/// tells the two apart by what runs now (`only_waiting`).
pub fn waits_only(command: &str) -> bool {
    let segs = simple_commands(command);
    let bares: Vec<(bool, &str)> = segs.iter().map(|s| bare(s)).collect();
    let sleeps = bares.iter().any(|(_, s)| s.split_whitespace().next() == Some("sleep"));
    let looped = bares.iter().any(|(l, _)| *l);
    let first_sleep = bares.first().is_some_and(|(_, s)| s.split_whitespace().next() == Some("sleep"));
    let pid_wait = bares.iter().any(|(_, s)| {
        let w: Vec<&str> = s.split_whitespace().collect();
        (w.first() == Some(&"wait") && w.get(1).is_some_and(|p| p.parse::<u32>().is_ok() || p.starts_with('$')))
            || (w.first() == Some(&"tail") && w.iter().any(|a| a.starts_with("--pid")))
            || (w.first() == Some(&"caffeinate") && w.contains(&"-w"))
            || w.first() == Some(&"pwait")
    });
    (looped && sleeps) || first_sleep || pid_wait
}

/// A process that waits for a long time by itself: `sleep`, `tail --pid`, `caffeinate -w`, `pwait`.
fn waits_long(command: &str) -> bool {
    let w = shell_words(command);
    let base = w.first().map(|f| f.rsplit('/').next().unwrap_or(f)).unwrap_or("");
    matches!(base, "sleep" | "gsleep" | "pwait") || (base == "tail" && w.iter().any(|a| a.starts_with("--pid"))) || (base == "caffeinate" && w.iter().any(|a| a == "-w"))
}

/// A check run inside a wait loop lives seconds; one that runs this long is work.
const CHECK_SECS: u64 = 60;

/// A shell's tree (the shell first) that only waits now: besides copies of
/// the shell (its subshells), only what waits by itself (`sleep`) and the
/// loop's short checks (`pgrep`, `grep -q`, `curl`, `tower thread`…).
pub fn only_waiting(tree: &[&Proc]) -> bool {
    let Some(shell) = tree.first() else { return false };
    tree.iter().skip(1).all(|p| p.command == shell.command || is_tool_shell(&p.command) || waits_long(&p.command) || p.secs < CHECK_SECS)
}

/// The files a wait loop polls: `grep [-q…] <pattern> <file>`, `[ -f <file> ]`,
/// `test -s <file>`, `ls`/`stat <file>`, as written (maybe relative).
pub fn polled_files(command: &str) -> Vec<String> {
    let mut out = vec![];
    let keep = |t: &str| !t.is_empty() && !t.starts_with('-') && !t.contains(['>', '<', '$', '*', ']']) && !t.starts_with("/dev/");
    for seg in simple_commands(command) {
        let (_, s) = bare(&seg);
        let w = shell_words(s);
        let Some(first) = w.first().map(String::as_str) else { continue };
        match first {
            "grep" | "egrep" | "fgrep" | "rg" => {
                // Its pattern: `-e X`, else the first word that isn't a flag; the rest are its files.
                let mut pattern_given = false;
                let mut args = vec![];
                let mut i = 1;
                while i < w.len() {
                    let a = w[i].as_str();
                    if a == "-e" || a == "-f" || a == "--regexp" {
                        pattern_given = true;
                        i += 2;
                        continue;
                    }
                    if matches!(a, "-m" | "-A" | "-B" | "-C" | "--max-count") {
                        i += 2;
                        continue;
                    }
                    if !a.starts_with('-') {
                        args.push(a);
                    }
                    i += 1;
                }
                out.extend(args.into_iter().skip(usize::from(!pattern_given)).filter(|t| keep(t)).map(String::from));
            }
            "[" | "[[" | "test" => {
                for p in w.windows(2) {
                    if matches!(p[0].as_str(), "-f" | "-e" | "-s" | "-r") && keep(&p[1]) {
                        out.push(p[1].clone());
                    }
                }
            }
            "ls" | "stat" => out.extend(w.iter().skip(1).filter(|t| keep(t)).cloned()),
            _ => {}
        }
    }
    out.dedup();
    out
}

/// A quiet job of a session whose other job is at work (its CPU, or a write
/// within the quiet time) isn't stuck: it waits on that one (a pipeline step
/// that sleeps until the training it follows writes its last checkpoint).
pub fn siblings(jobs: &mut [Job], now_ms: u64) {
    let q = quiet_ms();
    let active: Vec<Option<String>> =
        jobs.iter().map(|j| (j.busy || now_ms.saturating_sub(j.progress_ms.unwrap_or(j.since_ms)) <= q).then(|| j.what.clone())).collect();
    for (i, j) in jobs.iter_mut().enumerate() {
        if !j.stuck {
            continue;
        }
        if let Some(other) = active.iter().enumerate().find_map(|(k, a)| if k != i { a.clone() } else { None }) {
            j.stuck = false;
            j.waits_on = other;
        }
    }
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
    if !j.waiting.is_empty() {
        s += &format!(" (waits for {})", clip(&j.waiting, 60));
    } else if !j.waits_on.is_empty() {
        s += &format!(" (waits on {})", clip(&j.waits_on, 60));
    } else if j.target_gone {
        s += &format!(" (waits on {}, which has ended)", clip(&j.waiter, 60));
    } else if !j.waiter.is_empty() {
        s += &format!(" (waits for {})", clip(&j.waiter, 60));
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
    /// A path's last write (Unix ms), when it can be read (the files open for
    /// writing, and those `named_in` a job's commands).
    pub mtimes: HashMap<String, u64>,
    /// GPU time each process has used so far (ns, Metal: `ioreg`'s
    /// AGXDeviceUserClient), now and at the look before: a job on the GPU
    /// shows little CPU.
    pub gpu: HashMap<u32, u64>,
    pub gpu_before: HashMap<u32, u64>,
}

impl World {
    /// A process used the GPU since the look before.
    pub fn on_gpu(&self, pid: u32) -> bool {
        match (self.gpu.get(&pid), self.gpu_before.get(&pid)) {
            (Some(now), Some(before)) => now > before,
            _ => false,
        }
    }

    /// A process's working folder, when lsof said it.
    pub fn cwd_of(&self, pid: u32) -> Option<&str> {
        self.files.get(&pid)?.iter().find(|o| o.fd == "cwd").map(|o| o.name.as_str())
    }

    /// The processes a waiter waits on (`waited_in`), here, and everything
    /// that runs with them: from the topmost of their parents that isn't
    /// the system's (a detached `guard_train.py` and the training under it).
    /// Never the waiter's own (`own`), nor a `pgrep` looking.
    pub fn waited_on(&self, command: &str, own: &[u32]) -> Vec<u32> {
        let mut out: Vec<u32> = vec![];
        for w in waited_in(command) {
            let found: Vec<&Proc> = match &w {
                Waited::Pid(p) => self.procs.iter().filter(|x| x.pid == *p).collect(),
                Waited::Pattern(parts) => self
                    .procs
                    .iter()
                    .filter(|x| !own.contains(&x.pid) && !x.command.contains("pgrep") && !is_tool_shell(&x.command) && fragments_in(parts, &x.command))
                    .collect(),
            };
            for p in found {
                // Up to its topmost parent short of the system (pid 1) or a session's own shell.
                let mut root = p;
                for _ in 0..16 {
                    match self.procs.iter().find(|x| x.pid == root.ppid) {
                        Some(up) if up.pid > 1 && !is_tool_shell(&up.command) && !up.command.contains("claude") => root = up,
                        _ => break,
                    }
                }
                for q in self.subtree(root.pid) {
                    if !out.contains(&q.pid) && !own.contains(&q.pid) {
                        out.push(q.pid);
                    }
                }
            }
        }
        out
    }
}

/// How far a job reaches beyond its shell's tree: the processes it waits on
/// (`waited_on`), and the files its command and theirs name as where they
/// write (`named_files`, resolved from each one's folder).
#[derive(Debug, Default)]
pub struct Reach {
    pub waited: Vec<u32>,
    pub named: Vec<String>,
}

pub fn reach(world: &World, shell: u32, command: &str) -> Reach {
    let own: Vec<u32> = world.subtree(shell).iter().map(|p| p.pid).collect();
    let waited = world.waited_on(command, &own);
    let mut named: Vec<String> = named_files(command).iter().filter_map(|n| resolve(n, world.cwd_of(shell))).collect();
    // A wait loop's polled files (`until grep -q DONE run.log`): written, they move what it waits for.
    if waits_only(command) {
        named.extend(polled_files(command).iter().filter_map(|n| resolve(n, world.cwd_of(shell))));
    }
    for pid in own.iter().skip(1).chain(&waited) {
        if let Some(p) = world.procs.iter().find(|p| p.pid == *pid) {
            named.extend(named_files(&p.command).iter().filter_map(|n| resolve(n, world.cwd_of(*pid))));
        }
    }
    named.sort();
    named.dedup();
    Reach { waited, named }
}

/// A declared job's process (`tower job-running start --pid`) at work now:
/// its tree on the CPU or the GPU; and the last write of the files its tree
/// has open for writing or names in its commands (`--log`).
pub fn activity(world: &World, pid: u32) -> (bool, Option<u64>) {
    let tree = world.subtree(pid);
    let busy = tree.iter().any(|p| p.cpu >= BUSY_CPU || world.on_gpu(p.pid));
    let progress = tree
        .iter()
        .flat_map(|p| {
            let open = world.files.get(&p.pid).into_iter().flatten().filter(|o| written(o)).map(|o| o.name.clone());
            let named = named_files(&p.command).into_iter().filter_map(|n| resolve(&n, world.cwd_of(p.pid)));
            open.chain(named).collect::<Vec<_>>()
        })
        .filter_map(|n| world.mtimes.get(&n).copied())
        .max();
    (busy, progress)
}

/// What a waiter waits on: a process by its command line, or by its pid.
#[derive(Debug, Clone, PartialEq)]
pub enum Waited {
    /// The parts of a `pgrep -f` pattern that must show, in order (`[m]lx_lm lora -c .*v5.yaml`
    /// is `mlx_lm lora -c`, `v5.yaml`).
    Pattern(Vec<String>),
    Pid(u32),
}

/// The processes a command waits on: `pgrep [-f] '<pattern>'`, `pidof
/// <name>`, `kill -0 <pid>`, `wait <pid>`, `tail --pid=<pid>`, `lsof -p <pid>`.
pub fn waited_in(command: &str) -> Vec<Waited> {
    let toks = shell_words(command);
    let mut out = vec![];
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].as_str();
        match t {
            "pgrep" | "pidof" => {
                // Its pattern: the first word that isn't a flag.
                let mut j = i + 1;
                while j < toks.len() && toks[j].starts_with('-') {
                    j += 1;
                }
                if let Some(p) = toks.get(j) {
                    let parts: Vec<String> = p
                        .replace(['[', ']', '^', '$', '\\'], "")
                        .split(".*")
                        .flat_map(|s| s.split(".+"))
                        .map(|s| s.trim().to_string())
                        .filter(|s| s.len() >= 3)
                        .collect();
                    if !parts.is_empty() {
                        out.push(Waited::Pattern(parts));
                    }
                }
                i = j;
            }
            "kill" if toks.get(i + 1).map(String::as_str) == Some("-0") => {
                if let Some(p) = toks.get(i + 2).and_then(|p| p.parse().ok()) {
                    out.push(Waited::Pid(p));
                }
                i += 2;
            }
            "wait" => {
                if let Some(p) = toks.get(i + 1).and_then(|p| p.parse().ok()) {
                    out.push(Waited::Pid(p));
                }
            }
            _ if t.starts_with("--pid=") => {
                if let Ok(p) = t["--pid=".len()..].parse() {
                    out.push(Waited::Pid(p));
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// The parts all show in `command`, in order.
fn fragments_in(parts: &[String], command: &str) -> bool {
    let mut at = 0;
    for p in parts {
        match command[at..].find(p.as_str()) {
            Some(i) => at += i + p.len(),
            None => return false,
        }
    }
    true
}

/// A command's words, its quotes taken off (`pgrep -f '[m]lx lora'` is `pgrep`, `-f`, `[m]lx lora`).
fn shell_words(command: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, c) if c.is_whitespace() || c == ';' || c == '|' || c == '&' || c == '(' || c == ')' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (_, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The files a command names as where it writes: `--log X`, `--log-file X`,
/// `--output X`, `-o X`, `> X`, `>> X`, `tee X` (its progress can show there
/// though no process holds them open), as written (maybe relative).
pub fn named_files(command: &str) -> Vec<String> {
    let toks = shell_words(command);
    let mut out = vec![];
    for (i, t) in toks.iter().enumerate() {
        // `tee -a X`: past its flags.
        let next = if t == "tee" { toks[i + 1..].iter().find(|n| !n.starts_with('-')) } else { toks.get(i + 1) };
        let named = match t.as_str() {
            "--log" | "--log-file" | "--logfile" | "--log_file" | "--output" | "--out" | "-o" | "--save-log" | "tee" | ">" | ">>" | "2>" | "&>" => next.cloned(),
            _ => ["--log=", "--log-file=", "--logfile=", "--output=", "--out="].iter().find_map(|p| t.strip_prefix(p).map(String::from)),
        };
        if let Some(n) = named.filter(|n| !n.is_empty() && !n.starts_with('-') && n != "/dev/null") {
            out.push(n);
        }
        // `>file` written without a space.
        if let Some(n) = t.strip_prefix(">>").or_else(|| t.strip_prefix('>')).filter(|n| !n.is_empty() && !n.starts_with('&') && *n != "/dev/null") {
            out.push(n.to_string());
        }
    }
    out
}

/// A named file's path: as it is when absolute, else from `cwd`.
pub fn resolve(name: &str, cwd: Option<&str>) -> Option<String> {
    if name.starts_with('/') {
        return Some(name.to_string());
    }
    if let Some(rest) = name.strip_prefix("~/") {
        return std::env::var("HOME").ok().map(|h| format!("{h}/{rest}"));
    }
    cwd.map(|c| format!("{}/{name}", c.trim_end_matches('/')))
}

/// `ioreg -l -w0 -r -c AGXDeviceUserClient`: the GPU time each process has
/// used (ns), summed over its Metal clients. Each client is a block (`+-o
/// AGXDeviceUserClient …`) whose `AppUsage` may come before its
/// `IOUserClientCreator` ("pid 94550, python3.12").
pub fn parse_gpu(text: &str) -> HashMap<u32, u64> {
    let mut out: HashMap<u32, u64> = HashMap::new();
    for block in text.split("+-o ").skip(1) {
        let Some(pid) = block.split("\"IOUserClientCreator\" = \"pid ").nth(1).and_then(|r| r.split(',').next()).and_then(|p| p.trim().parse::<u32>().ok()) else { continue };
        let used: u64 = block
            .lines()
            .filter(|l| l.contains("\"AppUsage\""))
            .flat_map(|l| l.split("\"accumulatedGPUTime\"=").skip(1))
            .filter_map(|r| r.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().ok())
            .sum();
        *out.entry(pid).or_insert(0) += used;
    }
    out
}

/// A short name for a process: `mlx_lm lora` for `python -I -m mlx_lm lora -c …`,
/// `guard_train.py` for `python -I scripts/review/guard_train.py --config …`.
pub fn short_name(command: &str) -> String {
    let toks = shell_words(command);
    let base = |t: &str| t.rsplit('/').next().unwrap_or(t).to_string();
    let Some(first) = toks.first() else { return String::new() };
    let interp = ["python", "python3", "node", "bash", "sh", "zsh", "ruby", "uv", "deno", "bun"];
    let b = base(first);
    if !interp.iter().any(|i| b == *i || b.starts_with("python3.")) {
        return b;
    }
    let mut i = 1;
    while i < toks.len() {
        let t = toks[i].as_str();
        if t == "-m" || t == "run" {
            let module = toks.get(i + 1).cloned().unwrap_or_default();
            return match toks.get(i + 2).filter(|n| !n.starts_with('-') && !n.contains('/') && !n.contains('.')) {
                Some(sub) => format!("{module} {sub}"),
                None => module,
            };
        }
        if !t.starts_with('-') {
            return base(t);
        }
        i += 1;
    }
    b
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
///
/// `turn_over`: the session is done or idle. While it works, its foreground
/// Bash call runs in a tool shell too: then only a shell that is a background
/// task (its output is Claude Code's task file, or the transcript has its
/// `run_in_background` call) is looked at.
pub fn look(claude: u32, world: &World, tasks: &Tasks, now_ms: u64, turn_over: bool) -> (Vec<Job>, Vec<Server>) {
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
        // While the session works, a shell the transcript doesn't know as a
        // background task is its call in the foreground (Claude Code writes a
        // foreground call's output to a task file too, until it is done).
        if !turn_over && !task_id.as_ref().is_some_and(|id| tasks.by_id.contains_key(id)) {
            continue;
        }
        let task = task_id.as_ref().and_then(|id| tasks.by_id.get(id));
        let command = task.map(|t| t.command.clone()).filter(|c| !c.is_empty()).or_else(|| shell_command(&shell.command)).unwrap_or_default();
        // What is under a finite command (an eval script, a test run) is its
        // helper: a model server it starts, a server its tests bring up. Its
        // ports and commands don't make the shell a server.
        let root_finite = finite_command(&command) || task.is_some_and(|t| described_finite(&t.description));
        let helper = helpers_of(&tree, shell.pid, root_finite);
        let top: Vec<&&Proc> = tree.iter().skip(1).filter(|p| !helper.contains(&p.pid)).collect();
        let commands: Vec<&str> = std::iter::once(command.as_str()).chain(top.iter().map(|p| p.command.as_str())).collect();
        let ports: Vec<u16> = std::iter::once(&shell).chain(top.iter().copied()).flat_map(|p| world.listen.get(&p.pid).cloned().unwrap_or_default()).collect();
        let mut kind = kind_of(task, &commands, !ports.is_empty());
        // Claude Code's own word: a background task still running when the turn
        // ended is what the session waits on ("1 shell still running"). Unless
        // it is a server by more than a port (its command, its description) or
        // the session said it serves, it is work in progress.
        if kind == Kind::Server && turn_over && task.is_some_and(|t| !t.said_serving) && kind_of(task, &commands, false) != Kind::Server {
            kind = Kind::Job;
        }
        let what = task.map(|t| t.description.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| clip(&name_of(&command), 80));
        match kind {
            Kind::Watch => watches.push(task.map(|t| t.command.clone()).unwrap_or(command)),
            Kind::Server => servers.push(Server { what, pid: shell.pid, ports }),
            Kind::Job => {
                // Its reach: what it waits on (a `pgrep` loop until a detached
                // training ends) runs for it, and the files named in its
                // commands (`--log train.log`) show its progress.
                let reach = reach(world, shell.pid, &command);
                let waited: Vec<&Proc> = reach.waited.iter().filter_map(|w| world.procs.iter().find(|p| p.pid == *w)).collect();
                // It only waits now (a loop's `sleep`, a `wait <pid>`): on a
                // process its command names, a file it polls, or time.
                let wait = (waits_only(&command) && only_waiting(&tree)).then(|| {
                    let targets = waited_in(&command);
                    if !targets.is_empty() {
                        Wait::Process
                    } else if polled_files(&command).is_empty() && !simple_commands(&command).iter().any(|s| bare(s).0) {
                        Wait::Timer
                    } else {
                        Wait::Condition
                    }
                });
                let tree: Vec<&Proc> = tree.iter().copied().chain(waited.iter().copied()).collect();
                let writes: Vec<&Open> = tree.iter().flat_map(|p| world.files.get(&p.pid).map(|f| f.iter().filter(|o| written(o)).collect::<Vec<_>>()).unwrap_or_default()).collect();
                // Its log over its task output (often empty when the command
                // redirects): of the files it writes or names, the one written last.
                let logs: Vec<&String> = writes.iter().map(|o| &o.name).chain(&reach.named).filter(|n| !n.contains("/tasks/")).collect();
                let output = logs
                    .iter()
                    .max_by_key(|n| world.mtimes.get(n.as_str()).copied().unwrap_or(0))
                    .map(|n| n.to_string())
                    .or_else(|| writes.first().map(|o| o.name.clone()));
                let progress_ms = writes.iter().map(|o| &o.name).chain(&reach.named).filter_map(|n| world.mtimes.get(n).copied()).max();
                let since_ms = now_ms.saturating_sub(shell.secs * 1000);
                // Work on the CPU, or on the GPU (an MLX or Metal training shows little CPU).
                let busy = tree.iter().any(|p| p.cpu >= BUSY_CPU || world.on_gpu(p.pid));
                // What it waits on, by the process at work there (the training, not its guard).
                let waits_on = waited
                    .iter()
                    // (Of equals, the first: the topmost, `after_training.sh` over its `sleep`.)
                    .rev()
                    .max_by_key(|p| (world.on_gpu(p.pid), (p.cpu * 10.0) as u64))
                    .map(|p| clip(&short_name(&p.command), 60))
                    .unwrap_or_default();
                let quiet = now_ms.saturating_sub(progress_ms.unwrap_or(since_ms));
                // A waiter in words: its live target, else what its command
                // names (the pattern, the pid, the file), else its sleep.
                let waiter = match wait {
                    None => String::new(),
                    Some(Wait::Process) if !waits_on.is_empty() => waits_on.clone(),
                    Some(Wait::Process) => waited_in(&command)
                        .iter()
                        .map(|w| match w {
                            Waited::Pattern(parts) => parts.join(" … "),
                            Waited::Pid(p) => format!("process {p}"),
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    Some(Wait::Condition) => polled_files(&command).first().cloned().unwrap_or_else(|| "its condition".into()),
                    Some(Wait::Timer) => simple_commands(&command).iter().map(|s| bare(s).1.to_string()).find(|s| s.starts_with("sleep")).unwrap_or_default(),
                };
                jobs.push(Job {
                    what,
                    command: clip(&command, 300),
                    since_ms,
                    progress_ms,
                    busy,
                    // One that only waits is never stuck: what it waits on may
                    // take the night (an `overdue` note at most, much later).
                    // Only the work it waits on can be: a training under its
                    // guard at no CPU and no GPU, its log quiet (not a script
                    // that itself only sleeps until its turn).
                    stuck: match wait {
                        None => !busy && quiet > quiet_ms(),
                        Some(Wait::Process) if !waited.is_empty() && !only_waiting(&waited) => !busy && quiet > quiet_ms(),
                        Some(_) => false,
                    },
                    pid: Some(shell.pid),
                    task: task_id.clone(),
                    source: "background".into(),
                    output,
                    note: String::new(),
                    watched: false,
                    waits_on,
                    target_gone: wait == Some(Wait::Process) && waited.is_empty(),
                    wait,
                    waiter: clip(&waiter, 60),
                    ..Default::default()
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
    siblings(&mut jobs, now_ms);
    (jobs, servers)
}

/// The processes of a shell's tree (`tree`, parents before children, the
/// shell first) that are under a finite command: helpers of its work, not
/// what it is. `root_finite`: the shell's own command (or its call) is one.
pub fn helpers_of(tree: &[&Proc], shell: u32, root_finite: bool) -> std::collections::HashSet<u32> {
    let mut under: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for p in tree.iter().skip(1) {
        let parent_finite = if p.ppid == shell { root_finite } else { tree.iter().find(|q| q.pid == p.ppid).is_some_and(|q| finite_command(&q.command)) };
        if parent_finite || under.contains(&p.ppid) {
            under.insert(p.pid);
        }
    }
    under
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
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu3","content":"a.txt b.txt"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu4","name":"Bash","input":{"command":"cargo build","description":"Build"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu4","content":"Command did not complete within its 120s timeout and was moved to the background (ID: bmoved1). Output is being written to: /tmp/p/tasks/bmoved1.output."}]}}"#,
            r#"{"type":"user","message":{"content":"<task-notification>\n<task-id>bold1</task-id>\n<status>completed</status>\n<summary>Background command \"Old\" completed (exit code 0)</summary>\n</task-notification>"}}"#,
        ]
        .join("\n");
        let mut t = Tasks::default();
        read_tasks(&jsonl, &mut t);
        let train = &t.by_id["bevu4s93d"];
        assert_eq!((train.kind, train.description.as_str(), train.command.as_str()), (TaskKind::Bash, "Train the model", "python train.py > log 2>&1"));
        assert!(train.at_ms > 0);
        assert_eq!(t.by_id["bd49c9q53"].kind, TaskKind::Monitor);
        assert_eq!(t.by_id["bmoved1"].description, "Build", "moved to the background on its timeout: a task");
        assert_eq!(t.by_id.len(), 3, "a call in the foreground is no task");
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
        for c in ["npx dotenv -e .env.local -- env APP_NAME=management PORT=3002 node scripts/dev-server.mjs", "PORT=8080 node app.js", "node scripts/devserver.js"] {
            assert_eq!(kind_of(Some(&bash(c)), &[], false), Kind::Server, "{c}");
        }
        assert_eq!(kind_of(Some(&bash("node scripts/report.js --port-check")), &[], false), Kind::Job);
        // A container left running in the foreground: a server; a one-off (`--rm`) or detached: not.
        for c in ["docker run -p 5432:5432 postgres:16", "docker run --name db -e X=1 postgres", "node server.js", "python3 serve.py --port 9000", "./scripts/start-server.sh"] {
            assert_eq!(kind_of(Some(&bash(c)), &[], false), Kind::Server, "{c}");
        }
        for c in ["docker run --rm migrate/migrate up", "docker run -d postgres", "node scripts/build-server.js", "python serve_report.py"] {
            assert_eq!(kind_of(Some(&bash(c)), &[], false), Kind::Job, "{c}");
        }
        // By what its call says it does.
        let said = |d: &str, c: &str| Task { kind: TaskKind::Bash, description: d.into(), command: c.into(), ..Default::default() };
        for (d, c) in [("Start Postgres and dev servers in Docker", "make dev > dev.log 2>&1"), ("Serve the docs locally", "mkdocs x"), ("Watch the styles", "sass a b"), ("Bring up the local stack", "./stack.sh")] {
            assert_eq!(kind_of(Some(&said(d, c)), &[], false), Kind::Server, "{d}");
        }
        for (d, c) in [("Restart v2 training with gradient checkpointing", "python train.py"), ("Start the import of last week's orders", "node import.js"), ("Start the dev server build and run its tests", "npm run build && npm test"), ("Rerun query export in background with logging", "python export.py")] {
            assert_eq!(kind_of(Some(&said(d, c)), &[], false), Kind::Job, "{d}");
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
        let task = |id: &str, kind, d: &str, c: &str| Task { id: id.into(), kind, description: d.into(), command: c.into(), at_ms: 0, said_serving: false };
        t.by_id.insert("btrain".into(), task("btrain", TaskKind::Bash, "Train the 1.2B LoRA", "python train.py > /w/train.log 2>&1"));
        t.by_id.insert("bpost".into(), task("bpost", TaskKind::Bash, "Post-training pipeline", "bash post.sh > /w/post.log 2>&1"));
        t.by_id.insert("bmon".into(), task("bmon", TaskKind::Monitor, "val loss", "tail -n +1 -f /w/train.log | grep val"));
        t.by_id.insert("bdev".into(), task("bdev", TaskKind::Bash, "Start the dev server", "npm run dev"));
        (w, t)
    }

    /// t12r's eval (search-llm, 14 Nov): a background script that writes and
    /// runs `eval_real_both.sh`, whose `run_baselines.py --mlx` starts a local
    /// MLX model server on 8091 for itself. A job, not a server.
    fn eval_world(desc: &str, eval_cmd: &str) -> (World, Tasks) {
        let mut w = World::default();
        let cmd = "cat > scripts/eval_real_both.sh <<'EOF'\nset -euo pipefail\n$PY scripts/run_baselines.py --mlx --only mlx-ft-1.2b --sets real\nEOF\nchmod +x scripts/eval_real_both.sh && scripts/eval_real_both.sh > data/cache/eval_real.log 2>&1";
        w.procs = vec![
            Proc { pid: 1, ppid: 0, secs: 9000, cpu: 2.0, command: "claude".into() },
            shell(96519, 1, 215, cmd),
            Proc { pid: 96525, ppid: 96519, secs: 214, cpu: 0.0, command: eval_cmd.into() },
            Proc { pid: 2497, ppid: 96525, secs: 29, cpu: 0.0, command: ".venv/bin/python -I -u scripts/run_baselines.py --mlx --only mlx-ftv2-1.2b --sets real --suffix=-real".into() },
            Proc { pid: 2498, ppid: 2497, secs: 29, cpu: 1.0, command: "/w/.venv/bin/python -I /w/serve/mlx_server.py --model /w/models/hf/LFM2.5-1.2B".into() },
        ];
        w.files.insert(96519, vec![open("1", "w", "/tmp/p/tasks/babjmq4ak.output")]);
        w.files.insert(96525, vec![open("1", "w", "/w/data/cache/eval_real.log")]);
        w.listen.insert(2498, vec![8091]);
        w.mtimes.insert("/w/data/cache/eval_real.log".into(), NOW - 30_000);
        let mut t = Tasks::default();
        t.by_id.insert("babjmq4ak".into(), Task { id: "babjmq4ak".into(), kind: TaskKind::Bash, description: desc.into(), command: cmd.into(), at_ms: 0, said_serving: false });
        (w, t)
    }

    #[test]
    fn a_finite_script_that_starts_its_own_model_server_is_a_job() {
        // As it was: the eval's description names finite work.
        let (w, t) = eval_world("Evaluate v1 and v2 on the real labelled queries", "bash scripts/eval_real_both.sh");
        let (jobs, servers) = look(1, &w, &t, NOW, true);
        assert!(servers.is_empty(), "{servers:?}");
        assert_eq!((jobs.len(), jobs[0].what.as_str(), jobs[0].task.as_deref()), (1, "Evaluate v1 and v2 on the real labelled queries", Some("babjmq4ak")));
        // Without a finite description, the port is still under a finite command (rule 2).
        let (w, t) = eval_world("Run v1 and v2", "bash scripts/eval_real_both.sh");
        let (jobs, servers) = look(1, &w, &t, NOW, false);
        assert!(servers.is_empty() && jobs.len() == 1, "{servers:?} {jobs:?}");
        // A script named for nothing finite whose child listens: a server while the session works…
        let (mut w, mut t) = eval_world("Bring the models online", "bash scripts/models.sh");
        let cmd = "scripts/models.sh > data/cache/models.log 2>&1";
        w.procs[1] = shell(96519, 1, 215, cmd);
        w.procs[3].command = ".venv/bin/python -I -u scripts/launch.py".into();
        t.by_id.get_mut("babjmq4ak").unwrap().command = cmd.into();
        assert_eq!(look(1, &w, &t, NOW, false).1.len(), 1, "a port it holds itself makes it a server");
        // …and when its turn is over (Claude Code: "1 shell still running") and it never said it serves: work in progress (rule 3).
        let (jobs, servers) = look(1, &w, &t, NOW, true);
        assert!(servers.is_empty() && jobs.len() == 1, "{servers:?}");
        // Said it serves: a server.
        t.by_id.get_mut("babjmq4ak").unwrap().said_serving = true;
        assert_eq!(look(1, &w, &t, NOW, true).1.len(), 1);
    }

    #[test]
    fn a_dev_server_stays_a_server_and_a_test_runs_own_server_is_its_helper() {
        let mut w = World::default();
        w.procs = vec![
            Proc { pid: 1, ppid: 0, secs: 9000, cpu: 2.0, command: "claude".into() },
            shell(50, 1, 3000, "npm run dev"),
            Proc { pid: 51, ppid: 50, secs: 2999, cpu: 0.5, command: "node /w/node_modules/.bin/next dev".into() },
            shell(60, 1, 400, "npx playwright test --project chromium"),
            Proc { pid: 61, ppid: 60, secs: 399, cpu: 30.0, command: "node /w/node_modules/.bin/playwright test --project chromium".into() },
            Proc { pid: 62, ppid: 61, secs: 390, cpu: 5.0, command: "node /w/node_modules/.bin/next start -p 3100".into() },
        ];
        w.listen.insert(51, vec![3000]);
        w.listen.insert(62, vec![3100]);
        let mut t = Tasks::default();
        let task = |id: &str, d: &str, c: &str| Task { id: id.into(), kind: TaskKind::Bash, description: d.into(), command: c.into(), at_ms: 0, said_serving: false };
        t.by_id.insert("bdev".into(), task("bdev", "Start the dev server", "npm run dev"));
        t.by_id.insert("be2e".into(), task("be2e", "Run the e2e suite", "npx playwright test --project chromium"));
        w.files.insert(50, vec![open("1", "w", "/tmp/p/tasks/bdev.output")]);
        w.files.insert(60, vec![open("1", "w", "/tmp/p/tasks/be2e.output")]);
        for turn_over in [false, true] {
            let (jobs, servers) = look(1, &w, &t, NOW, turn_over);
            assert_eq!(servers.iter().map(|s| (s.what.as_str(), s.ports.clone())).collect::<Vec<_>>(), [("Start the dev server", vec![3000])]);
            assert_eq!(jobs.iter().map(|j| j.what.as_str()).collect::<Vec<_>>(), ["Run the e2e suite"]);
        }
    }

    #[test]
    fn finite_words_beat_a_port_unless_serving_is_named() {
        assert!(described_finite("Evaluate v1 and v2 on the real labelled queries"));
        assert!(described_finite("Fine-tune the 1.2B on v2 data") && described_finite("Label 5k queries with Claude") && described_finite("Run baselines on the test set"));
        assert!(!described_finite("Serve the eval dashboard") && !described_finite("Start the dev server and its tests") && !described_finite("Bring up the stack"));
        assert!(!described_finite("Start Postgres"));
        assert!(finite_command("bash scripts/eval_real_both.sh") && finite_command(".venv/bin/python -u scripts/run_baselines.py --mlx") && finite_command("cd /w && pytest -x"));
        assert!(!finite_command("npm run dev") && !finite_command("npm run build && npm start") && !finite_command("node server.js"));
        assert!(!finite_command("cd /w/test-app && node api.js"), "a folder's name isn't the work");
        assert!(!finite_command("./node_modules/.bin/next dev -p 3031 > /tmp/zendesk-ticket-import/x.output 2>&1"), "nor a file it writes to");
    }

    /// What the last day's background calls (all spaces, 14 Nov) showed read wrong.
    #[test]
    fn the_misreadings_the_review_found() {
        // Wait loops are finite: not watches (`[ -f x ]` and a `tail -10`; `grep -w`), not servers.
        assert_eq!(serves("until [ -f $SB/five.txt ]; do sleep 5; done; grep -E x log | tail -10"), None);
        assert_eq!(serves("while true; do L=$($ADB devices -l | grep -w device); sleep 5; done"), None);
        assert_eq!(serves("tail -n +1 -f /w/train.log | grep val"), Some(Kind::Watch));
        assert_eq!(serves("tail -F /w/x.log"), Some(Kind::Watch));
        assert_eq!(serves("npx tsc -w -p ."), Some(Kind::Watch));
        assert!(!described_server("Wait until the rebuilt server on :3000 is ready"));
        // Dev servers by their path, and a stream read for good.
        assert_eq!(serves("API_BASE_URL=http://127.0.0.1:3002 ./node_modules/.bin/next dev --webpack -p 3031 > /tmp/x.output"), Some(Kind::Server));
        assert_eq!(serves("NEXT_PUBLIC_SUPPORT_FIXTURES=1 ./node_modules/.bin/storybook dev -p 6026 --no-open --ci"), Some(Kind::Server));
        assert_eq!(serves("source /tmp/t/env.sh; cd /tmp/t; exec ~/Work/herder-tower/target/release/tower daemon > /tmp/t/daemon.log 2>&1"), Some(Kind::Server));
        assert_eq!(serves(r#"curl -skN "https://127.0.0.1:7899/v1/events?voice=1" -H "Authorization: Bearer x" > /tmp/sse.log"#), Some(Kind::Watch));
        assert_eq!(serves("curl -sn https://x/y -o out"), None);
        assert_eq!(serves("gh run watch 435 --exit-status --interval 30 > log"), None, "it ends with the run");
        // The session's words: "up on :7899".
        assert!(says_serving("The fake tower is up on :7899.") && !says_serving("Done at 14:38."));
    }

    #[test]
    fn a_monitors_event_isnt_its_end_and_words_about_serving_are_kept() {
        let line = |v: Value| v.to_string();
        let mut t = Tasks::default();
        read_tasks(
            &[
                line(serde_json::json!({"type":"assistant","timestamp":"2026-11-14T10:00:00Z","message":{"content":[{"type":"tool_use","id":"u1","name":"Monitor","input":{"description":"val loss","command":"tail -f x"}}]}})),
                line(serde_json::json!({"type":"user","timestamp":"2026-11-14T10:00:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"u1","content":"Monitor started (task bmon, expires in 30m)"}]}})),
                line(serde_json::json!({"type":"assistant","timestamp":"2026-11-14T10:00:02Z","message":{"content":[{"type":"tool_use","id":"u2","name":"Bash","input":{"description":"Run the API","command":"python api.py","run_in_background":true}}]}})),
                line(serde_json::json!({"type":"user","timestamp":"2026-11-14T10:00:03Z","message":{"content":[{"type":"tool_result","tool_use_id":"u2","content":"Command running in background with ID: bapi."}]}})),
                line(serde_json::json!({"type":"user","timestamp":"2026-11-14T10:05:00Z","message":{"content":"<task-notification>\n<task-id>bmon</task-id>\n<summary>Monitor event: \"val loss\"</summary>\n<event>2500 val 0.557</event>\n</task-notification>"}})),
                line(serde_json::json!({"type":"assistant","timestamp":"2026-11-14T10:05:10Z","message":{"content":[{"type":"text","text":"The API is up on localhost:8000."}]}})),
            ]
            .join("\n"),
            &mut t,
        );
        assert!(!t.ended.contains_key("bmon"), "an event, not its end");
        assert!(t.by_id["bapi"].said_serving);
        read_tasks(&line(serde_json::json!({"type":"user","timestamp":"2026-11-14T10:40:00Z","message":{"content":"<task-notification>\n<task-id>bmon</task-id>\n<summary>Monitor event: \"val loss\"</summary>\n<event>[Monitor expired after 30m with 2 events delivered.]</event>\n</task-notification>"}})), &mut t);
        read_tasks(&line(serde_json::json!({"type":"user","timestamp":"2026-11-14T10:41:00Z","message":{"content":"<task-notification>\n<task-id>bapi</task-id>\n<status>failed</status>\n<summary>Background command \"Run the API\" failed with exit code 1</summary>\n</task-notification>"}})), &mut t);
        assert!(t.ended.contains_key("bmon") && t.ended.contains_key("bapi"));
    }

    /// `HERDER_TOWER_JOB_CALLS=<calls.json>` (`[[project, id, description,
    /// command], …]`): how each background call reads, for a review.
    #[test]
    #[ignore]
    fn review_background_calls() {
        let Ok(path) = std::env::var("HERDER_TOWER_JOB_CALLS") else { return };
        let calls: Vec<(String, String, String, String)> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for (proj, id, d, c) in calls {
            let t = Task { id: id.clone(), kind: TaskKind::Bash, description: d.clone(), command: c.clone(), at_ms: 0, said_serving: false };
            let plain = kind_of(Some(&t), &[c.as_str()], false);
            let port = kind_of(Some(&t), &[c.as_str()], true);
            let flags = format!("{}{}{}{}", if described_server(&d) { "S" } else { "-" }, if described_finite(&d) { "F" } else { "-" }, if finite_command(&c) { "f" } else { "-" }, match serves(&c) { Some(Kind::Server) => "s", Some(Kind::Watch) => "w", _ => "-" });
            println!("REVIEW\t{plain:?}\t{port:?}\t{flags}\t{proj}\t{d}\t{}", c.replace('\n', " ⏎ ").chars().take(150).collect::<String>());
        }
    }

    /// t17h (search-llm, 15 Nov): a background `until ! pgrep -f …; do sleep
    /// 30; done` waits on a v5 LoRA training a detached `guard_train.py` runs
    /// (parent: the system). The training works on the GPU at 4% CPU and
    /// writes the log its guard's `--log` names; the waiter wrote nothing for
    /// 70 minutes. It was called stuck.
    fn gpu_world() -> (World, Tasks) {
        let mut w = World::default();
        let cmd = "until ! pgrep -f '[m]lx_lm lora -c .*lora_1.2b_v5.yaml' >/dev/null; do sleep 30; done; ls models/lora-1.2b-v5";
        w.procs = vec![
            Proc { pid: 5, ppid: 1, secs: 9000, cpu: 1.0, command: "claude".into() },
            shell(100, 5, 70 * 60, cmd),
            Proc { pid: 101, ppid: 100, secs: 10, cpu: 0.0, command: "sleep 30".into() },
            Proc { pid: 102, ppid: 100, secs: 0, cpu: 0.0, command: "pgrep -f [m]lx_lm lora -c .*lora_1.2b_v5.yaml".into() },
            Proc { pid: 200, ppid: 1, secs: 80 * 60, cpu: 0.0, command: ".venv/bin/python -I scripts/review/guard_train.py --config train/lora_1.2b_v5.yaml --log data/review/v5/train.log --min-free-gb 8".into() },
            Proc { pid: 201, ppid: 200, secs: 80 * 60, cpu: 4.0, command: "/w/.venv/bin/python -I -m mlx_lm lora -c /w/train/lora_1.2b_v5.yaml".into() },
            Proc { pid: 300, ppid: 1, secs: 80 * 60, cpu: 0.0, command: "/w/.venv/bin/python -m mlx_lm lora -c /w/train/lora_1.2b_v4.yaml".into() },
        ];
        w.files.insert(100, vec![open("1", "w", "/tmp/p/tasks/bwait.output")]);
        w.files.insert(200, vec![open("cwd", "", "/w")]);
        w.mtimes.insert("/tmp/p/tasks/bwait.output".into(), NOW - 70 * 60_000);
        w.mtimes.insert("/w/data/review/v5/train.log".into(), NOW - 60_000);
        w.gpu_before.insert(201, 1_000);
        w.gpu.insert(201, 2_000);
        let mut t = Tasks::default();
        t.by_id.insert("bwait".into(), Task { id: "bwait".into(), kind: TaskKind::Bash, description: "Wait for the v5 training to end".into(), command: cmd.into(), at_ms: 0, said_serving: false });
        (w, t)
    }

    #[test]
    fn a_job_waiting_on_a_gpu_training_is_not_stuck() {
        let (w, t) = gpu_world();
        let (jobs, servers) = look(5, &w, &t, NOW, true);
        assert!(servers.is_empty());
        let j = &jobs[0];
        assert_eq!((j.busy, j.stuck, j.waits_on.as_str()), (true, false, "mlx_lm lora"), "on the GPU: at work, though at 4% CPU");
        assert_eq!(j.progress_ms, Some(NOW - 60_000), "the guard's --log, resolved from its folder, shows the progress");
        assert_eq!(j.output.as_deref(), Some("/w/data/review/v5/train.log"));
        // The GPU paused a moment (an eval, a checkpoint): the log written a minute ago still says it moves.
        let mut w2 = gpu_world().0;
        w2.gpu.insert(201, 1_000);
        let j = &look(5, &w2, &t, NOW, true).0[0];
        assert_eq!((j.busy, j.stuck), (false, false));
        // No GPU, no CPU, its log 50 minutes old: possibly stuck after all.
        w2.mtimes.insert("/w/data/review/v5/train.log".into(), NOW - 50 * 60_000);
        assert!(look(5, &w2, &t, NOW, true).0[0].stuck);
        // A job declared with the guard's pid: idle at its own CPU, at work on the GPU below it, its --log read.
        assert_eq!(activity(&w, 200), (true, Some(NOW - 60_000)));
        assert_eq!(activity(&w2, 200), (false, Some(NOW - 50 * 60_000)), "no GPU, no CPU: its log says how long it has been quiet");
        // The training is gone (the waiter ends within its sleep): not stuck, its target gone.
        let mut w3 = gpu_world().0;
        w3.procs.retain(|p| p.pid != 200 && p.pid != 201);
        let j = &look(5, &w3, &t, NOW, true).0[0];
        assert!(!j.stuck && j.target_gone && j.waits_on.is_empty() && j.waiter == "mlx_lm lora -c … lora_1.2b_v5.yaml", "{j:?}");
        // Only what the pattern names: the v4 training (another config) isn't waited on, nor the pgrep looking.
        assert_eq!(w.waited_on(&t.by_id["bwait"].command, &[100, 101, 102]), [200, 201]);
    }

    #[test]
    fn what_a_waiter_waits_on_and_where_a_command_writes() {
        assert_eq!(waited_in("until ! pgrep -f '[m]lx_lm lora -c .*v5.yaml' >/dev/null; do sleep 30; done"), [Waited::Pattern(vec!["mlx_lm lora -c".into(), "v5.yaml".into()])]);
        assert_eq!(waited_in("while kill -0 4242 2>/dev/null; do sleep 5; done; tail --pid=77 -f x"), [Waited::Pid(4242), Waited::Pid(77)]);
        assert_eq!(waited_in("wait 991 && echo done"), [Waited::Pid(991)]);
        assert!(waited_in("python train.py --epochs 3").is_empty());
        assert_eq!(
            named_files("python guard.py --log data/train.log --out=run/x.json | tee -a eval.log > out.txt 2>/dev/null; echo hi >>notes.md"),
            ["data/train.log", "run/x.json", "eval.log", "out.txt", "notes.md"]
        );
        assert!(named_files("python train.py -c t.yaml > /dev/null").is_empty());
        assert_eq!(resolve("data/train.log", Some("/w/")), Some("/w/data/train.log".into()));
        assert_eq!(resolve("/abs/x.log", None), Some("/abs/x.log".into()));
        assert_eq!(resolve("x.log", None), None);
        assert_eq!(short_name("/w/.venv/bin/python -I -m mlx_lm lora -c /w/t.yaml"), "mlx_lm lora");
        assert_eq!(short_name(".venv/bin/python -I scripts/review/guard_train.py --config t.yaml"), "guard_train.py");
        assert_eq!(short_name("/usr/local/bin/ollama serve"), "ollama");
        // Claude Code writes a quote inside its eval as '"'"'.
        let ps = "/bin/zsh -c source /h/.claude/shell-snapshots/snapshot-zsh-1-x.sh 2>/dev/null || true && eval 'until ! pgrep -f '\"'\"'[m]lx lora'\"'\"' >/dev/null; do sleep 30; done' < /dev/null && pwd -P >| /tmp/cwd";
        assert_eq!(shell_command(ps).as_deref(), Some("until ! pgrep -f '[m]lx lora' >/dev/null; do sleep 30; done"));
    }

    #[test]
    fn gpu_time_is_read_per_client_block() {
        let text = r#"+-o AGXDeviceUserClient  <class AGXDeviceUserClient, id 0x1>
    {
      "AppUsage" = ({"API"="Metal","lastSubmittedTime"=1,"accumulatedGPUTime"=906013044375},{"API"="Metal","accumulatedGPUTime"=5})
      "IOUserClientCreator" = "pid 419, WindowServer"
    }
+-o AGXDeviceUserClient  <class AGXDeviceUserClient, id 0x2>
    {
      "AppUsage" = ()
      "IOUserClientCreator" = "pid 757, AccessibilityVis"
    }
+-o AGXDeviceUserClient  <class AGXDeviceUserClient, id 0x3>
    {
      "AppUsage" = ({"API"="Metal","lastSubmittedTime"=13145109996041,"accumulatedGPUTime"=143190186250})
      "IOUserClientCreator" = "pid 94550, python3.12"
    }
+-o AGXDeviceUserClient  <class AGXDeviceUserClient, id 0x4>
    {
      "IOUserClientCreator" = "pid 94550, python3.12"
      "AppUsage" = ({"API"="Metal","accumulatedGPUTime"=750})
    }"#;
        let g = parse_gpu(text);
        assert_eq!((g[&419], g[&757], g[&94550]), (906013044380, 0, 143190187000));
        let w = World { gpu: g.clone(), gpu_before: [(94550, 143190186250u64), (419, 906013044380)].into(), ..Default::default() };
        assert!(w.on_gpu(94550) && !w.on_gpu(419) && !w.on_gpu(757), "on the GPU: its time grew since the look before");
    }

    /// A script with nothing written yet ("No output available") is a job,
    /// quiet but not stuck until the threshold.
    #[test]
    fn a_job_with_no_output_yet_is_quiet_not_stuck() {
        let (mut w, t) = eval_world("Evaluate v1 and v2 on the real labelled queries", "bash scripts/eval_real_both.sh");
        w.mtimes.clear();
        for p in &mut w.procs {
            p.cpu = 0.0;
        }
        let (jobs, _) = look(1, &w, &t, NOW, true);
        assert!(!jobs[0].stuck && jobs[0].progress_ms.is_none(), "{:?}", jobs[0]);
        for p in &mut w.procs {
            p.secs = 31 * 60;
        }
        assert!(look(1, &w, &t, NOW, true).0[0].stuck, "quiet past the threshold");
    }

    #[test]
    fn a_session_with_training_running_has_a_job_not_the_watch_or_the_servers() {
        let (w, t) = world();
        let (jobs, servers) = look(1, &w, &t, NOW, true);
        let names: Vec<&str> = jobs.iter().map(|j| j.what.as_str()).collect();
        assert_eq!(names, ["Train the 1.2B LoRA", "Post-training pipeline"], "the Monitor watches, the MCP server isn't a tool shell, the dev server serves");
        let train = &jobs[0];
        assert_eq!((train.task.as_deref(), train.output.as_deref(), train.progress_ms, train.busy, train.stuck, train.watched), (Some("btrain"), Some("/w/train.log"), Some(NOW - 20_000), true, false, true));
        assert_eq!(train.since_ms, NOW - 700_000);
        let post = &jobs[1];
        assert!(!post.stuck && !post.busy, "quiet for 40 min and idle, but the training it follows is at work");
        assert_eq!(post.waits_on, "Train the 1.2B LoRA");
        // The training stopped too (idle, its log old): then the waiter may be stuck.
        let mut w4 = World { procs: w.procs.clone(), files: w.files.clone(), listen: w.listen.clone(), mtimes: w.mtimes.clone(), ..Default::default() };
        for p in w4.procs.iter_mut() {
            p.cpu = 0.0;
        }
        w4.mtimes.insert("/w/train.log".into(), NOW - 45 * 60_000);
        w4.mtimes.insert("/tmp/p/tasks/btrain.output".into(), NOW - 45 * 60_000);
        let (quiet_both, _) = look(1, &w4, &t, NOW, true);
        assert!(quiet_both.iter().all(|j| j.stuck && j.waits_on.is_empty()), "{quiet_both:?}");
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
        assert_eq!(look(1, &w, &t2, NOW, true).0.len(), 1);
        // No transcript: named by its command, still a job.
        let (jobs, _) = look(1, &w, &Tasks::default(), NOW, true);
        assert_eq!(jobs[0].what, "python train.py > /w/train.log 2>&1");
        // A command that redirects itself whole: its stdout is its log, not the
        // task's file; known by its command instead.
        let mut w2 = World { procs: w.procs.clone(), files: w.files.clone(), listen: w.listen.clone(), mtimes: w.mtimes.clone(), ..Default::default() };
        w2.files.insert(10, vec![open("1", "w", "/w/train.log")]);
        let (jobs, _) = look(1, &w2, &t, NOW, true);
        assert_eq!((jobs[0].what.as_str(), jobs[0].task.as_deref()), ("Train the 1.2B LoRA", Some("btrain")));
        assert!(look(999, &w, &t, NOW, true).0.is_empty());
        // While the session works: its foreground call (a tool shell the
        // transcript has no background task for, its output in a task file
        // all the same) is nothing.
        let mut w3 = World { procs: w.procs.clone(), files: w.files.clone(), listen: w.listen.clone(), mtimes: w.mtimes.clone(), ..Default::default() };
        w3.procs.push(shell(60, 1, 150, "cargo build --release && ./target/release/tower install"));
        w3.files.insert(60, vec![open("1", "w", "/tmp/p/tasks/bfore.output")]);
        let names = |turn_over: bool| look(1, &w3, &t, NOW, turn_over).0.into_iter().map(|j| j.what).collect::<Vec<_>>();
        assert_eq!(names(false), ["Train the 1.2B LoRA", "Post-training pipeline"], "the background tasks only");
        assert_eq!(names(true).len(), 3, "its turn over: a shell left is a background one");
    }

    /// A session's background loop that has waited `secs`, its `sleep` a
    /// child, and other processes of the machine; its task output untouched since.
    fn wait_world(cmd: &str, secs: u64, child: &str, others: Vec<Proc>) -> (World, Tasks) {
        let mut w = World::default();
        w.procs = vec![
            Proc { pid: 5, ppid: 1, secs: secs + 600, cpu: 1.0, command: "claude".into() },
            shell(100, 5, secs, cmd),
            Proc { pid: 101, ppid: 100, secs: 40, cpu: 0.0, command: child.into() },
        ];
        w.procs.extend(others);
        w.files.insert(100, vec![open("1", "w", "/tmp/p/tasks/bwait.output"), open("cwd", "", "/w")]);
        w.mtimes.insert("/tmp/p/tasks/bwait.output".into(), NOW - secs * 1000);
        let mut t = Tasks::default();
        t.by_id.insert("bwait".into(), Task { id: "bwait".into(), kind: TaskKind::Bash, description: "Wait for it".into(), command: cmd.into(), at_ms: 0, said_serving: false });
        (w, t)
    }

    /// The t17h case (search-llm, 2026-11-17 03:09): two background loops
    /// waited all night, one on the r1 training's guard, one on the scoring
    /// chain after it (a script that itself sleeps until the training ends).
    /// Nothing written and no CPU for 6 h 30: both were called stuck, the
    /// first as the training ended (its loop had a minute left to see it).
    #[test]
    fn a_sleep_loop_isnt_stuck_and_a_wait_loop_whose_target_is_gone_is_noted_later() {
        let night = 6 * 3600 + 30 * 60;
        let chain = "until ! pgrep -f '[a]fter_training.sh' >/dev/null; do sleep 60; done; date +%H:%M:%S; grep -E '^===|FAILED|chosen' data/review/reasons/after_training.log | tail -30";
        let script = vec![
            Proc { pid: 200, ppid: 1, secs: night, cpu: 0.0, command: "bash data/review/reasons/after_training.sh".into() },
            Proc { pid: 201, ppid: 200, secs: 20, cpu: 0.0, command: "sleep 60".into() },
        ];
        let (w, t) = wait_world(chain, night, "sleep 60", script);
        let (jobs, _) = look(5, &w, &t, NOW, true);
        let j = &jobs[0];
        assert_eq!((j.stuck, j.wait, j.target_gone), (false, Some(Wait::Process), false), "{j:?}");
        assert_eq!((j.waits_on.as_str(), j.waiter.as_str()), ("after_training.sh", "after_training.sh"));
        assert_eq!(overdue(&jobs, 0, None, NOW), None);
        assert!(line(&jobs, NOW).unwrap().contains("(waits on after_training.sh)"), "{:?}", line(&jobs, NOW));
        assert!(!shown(&jobs, NOW).unwrap().stuck);

        // The r1 loop as the training ended: its target gone, it ends within its sleep. Not stuck;
        // a soft note only if it still waits an hour later.
        let r1 = "until ! pgrep -f '[g]uard_train.py' >/dev/null; do sleep 60; done; date +%H:%M:%S; tail -3 data/review/reasons/guard.out";
        let (w, t) = wait_world(r1, night + 60, "sleep 60", vec![]);
        let (jobs, _) = look(5, &w, &t, NOW, true);
        let j = &jobs[0];
        assert_eq!((j.stuck, j.wait, j.target_gone, j.waiter.as_str()), (false, Some(Wait::Process), true, "guard_train.py"), "{j:?}");
        assert_eq!(overdue(&jobs, 0, Some(NOW - 2 * 60_000), NOW), None, "gone two minutes: its loop is about to see it");
        assert_eq!(overdue(&jobs, 0, Some(NOW - 61 * 60_000), NOW), Some(Overdue::Gone(61 * 60_000)), "gone an hour and still waiting");
        assert!(line(&jobs, NOW).unwrap().contains("(waits on guard_train.py, which has ended)"));

        // While the training ran: the guard and the training under it, on the GPU.
        let training = vec![
            Proc { pid: 300, ppid: 1, secs: night, cpu: 0.0, command: ".venv/bin/python -I scripts/review/guard_train.py --config train/r1.yaml --log data/review/reasons/train.log".into() },
            Proc { pid: 301, ppid: 300, secs: night, cpu: 3.0, command: ".venv/bin/python -I -m mlx_lm lora -c train/r1.yaml".into() },
        ];
        let (mut w, t) = wait_world(r1, night, "sleep 60", training);
        w.gpu_before.insert(301, 1);
        w.gpu.insert(301, 2);
        let j = &look(5, &w, &t, NOW, true).0[0];
        assert_eq!((j.stuck, j.busy, j.waits_on.as_str()), (false, true, "mlx_lm lora"));
        // The training itself hung (no GPU, no CPU, its log quiet): that is no wait, it may be stuck.
        w.gpu.insert(301, 1);
        w.procs[4].cpu = 0.0;
        w.files.insert(300, vec![open("cwd", "", "/w")]);
        w.mtimes.insert("/w/data/review/reasons/train.log".into(), NOW - 50 * 60_000);
        assert!(look(5, &w, &t, NOW, true).0[0].stuck, "a hung training under the guard is still told");

        // Not a waiter: the same shell running real work for hours, idle and quiet, is stuck.
        let (w, t) = wait_world("python score.py --all", night, "/w/.venv/bin/python score.py --all", vec![]);
        let mut w = w;
        w.procs[2].secs = night;
        let j = &look(5, &w, &t, NOW, true).0[0];
        assert!(j.stuck && j.wait.is_none(), "{j:?}");
    }

    #[test]
    fn a_timer_and_a_condition_wait_are_no_stuck_jobs() {
        // `sleep 7200; …`: a timer while it sleeps, never noted; the work after it is checked again.
        let timer = "sleep 7200; .venv/bin/python -I scripts/score.py --log s.log";
        let (w, t) = wait_world(timer, 6000, "sleep 7200", vec![]);
        let jobs = look(5, &w, &t, NOW, true).0;
        assert_eq!((jobs[0].stuck, jobs[0].wait, jobs[0].waiter.as_str()), (false, Some(Wait::Timer), "sleep 7200"));
        assert_eq!(overdue(&jobs, 0, None, NOW + 10 * 3600_000), None);
        let (mut w, t) = wait_world(timer, 3 * 3600, ".venv/bin/python -I scripts/score.py --log s.log", vec![]);
        w.procs[2].secs = 3 * 3600 - 7200;
        let j = &look(5, &w, &t, NOW, true).0[0];
        assert!(j.stuck && j.wait.is_none(), "the score run after the sleep, idle for an hour: {j:?}");

        // A loop polling a log: its writes are its progress; quiet 7 h with nothing else at work, a soft note.
        let cond = "until grep -qE 'SERVED DONE|FAILED|not up' data/eval/served.log; do sleep 5; done; tail -2 data/eval/served.log";
        let (mut w, t) = wait_world(cond, 7 * 3600, "sleep 5", vec![]);
        w.mtimes.insert("/w/data/eval/served.log".into(), NOW - 10 * 60_000);
        let jobs = look(5, &w, &t, NOW, true).0;
        assert_eq!((jobs[0].stuck, jobs[0].wait, jobs[0].progress_ms, jobs[0].waiter.as_str()), (false, Some(Wait::Condition), Some(NOW - 10 * 60_000), "data/eval/served.log"));
        assert_eq!(overdue(&jobs, 0, None, NOW), None);
        w.mtimes.insert("/w/data/eval/served.log".into(), NOW - 7 * 3600_000);
        let mut jobs = look(5, &w, &t, NOW, true).0;
        assert!(!jobs[0].stuck);
        assert_eq!(overdue(&jobs, 0, None, NOW), Some(Overdue::Long(7 * 3600_000)));
        // Another job of the session at work: it waits on that one.
        jobs.push(Job { what: "Serve check".into(), since_ms: NOW - 3600_000, busy: true, ..Default::default() });
        assert_eq!(overdue(&jobs, 0, None, NOW), None);
    }

    #[test]
    fn what_only_waits_and_what_a_wait_loop_polls() {
        for c in [
            "until ! pgrep -f '[g]uard_train.py' >/dev/null; do sleep 60; done; date; tail -3 guard.out",
            "(i=0; until [ \"$(ls data/answers 2>/dev/null | grep -c '^V')\" -ge 35 ] || [ $i -ge 120 ]; do sleep 5; i=$((i+1)); done; echo done)",
            "while kill -0 4242 2>/dev/null; do sleep 5; done",
            "while true; do grep -q DONE run.log && break; sleep 30; done",
            "sleep 600 && npm test",
            "wait 991 && echo done",
            "tail --pid=77 -f /dev/null",
        ] {
            assert!(waits_only(c), "{c}");
        }
        for c in ["python train.py --epochs 3", "npm test; sleep 5", "until curl -sf localhost:8090/health; do echo .; done", "echo 'until x; do sleep 1; done'"] {
            assert!(!waits_only(c), "{c}");
        }
        let p = |pid: u32, secs: u64, command: &str| Proc { pid, ppid: 100, secs, cpu: 0.0, command: command.into() };
        let sh = shell(100, 5, 9000, "until ! pgrep -f x; do sleep 60; done");
        let clone = Proc { pid: 102, ..sh.clone() };
        assert!(only_waiting(&[&sh, &p(101, 50, "sleep 60")]));
        assert!(only_waiting(&[&sh, &clone, &p(103, 0, "pgrep -f x"), &p(104, 900, "/bin/sleep 3600")]));
        assert!(only_waiting(&[&sh, &p(101, 9000, "tail --pid=77 -f /dev/null")]));
        assert!(!only_waiting(&[&sh, &p(101, 600, "/w/.venv/bin/python score.py")]), "work that runs ten minutes isn't a check");
        assert_eq!(polled_files("until grep -qE 'SERVED DONE|FAILED|not up' data/x.log 2>/dev/null; do sleep 5; done"), ["data/x.log"]);
        assert_eq!(polled_files("until [ -f out/done.flag ] || [ -s err.txt ]; do sleep 1; done"), ["out/done.flag", "err.txt"]);
        assert_eq!(polled_files("until grep -q -e DONE -m 1 a.log b.log; do sleep 2; done; ls -la models/x"), ["a.log", "b.log", "models/x"]);
        assert!(polled_files("until ! pgrep -f '[g]uard_train.py' >/dev/null; do sleep 60; done").is_empty());
    }
}
