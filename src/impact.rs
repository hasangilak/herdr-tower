//! What outside a PR depends on what it changes, and whether the change
//! breaks it (`docs/quiet-tower.md`, "Breaking changes").
//!
//! - **What it changes** (`things`): from the PR's diff, what others can
//!   use that it removes, renames or changes: public functions, types,
//!   classes and constants (a signature that changes counts), API routes,
//!   fields of what an API sends or takes (in API-like files), database
//!   columns a migration drops or renames, config keys it stops reading,
//!   events it stops sending.
//! - **Who uses it** (`scan`): every git checkout under the roots (the
//!   user's `~/Work`), searched with `git grep` for each of them: the PR's
//!   own repo outside the files the PR changes, and every other repo (the
//!   dashboard that calls a serverless API, the SDK, the mobile app). Each
//!   use is a `Dependent`: its repo, file, line and the function it is in.
//! - **Breaking** when anything outside uses what it removes or changes, or
//!   when the reviewer (or the PR's session) reports a break it found by
//!   compiling, type-checking or running the dependents' tests (`tower
//!   impact <pr> breaks "…"`). Then the PR needs review ("a breaking
//!   change"), with a warning that says what breaks and who uses it; it is
//!   never merged by the tower. Nothing found: it says so, and what was searched.
//!
//! The search is by name, so it can find a same-named thing that isn't the
//! same one: a false alarm sends the PR to the user, never the other way.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// What a PR changes that others can use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thing {
    /// `function`, `type`, `route`, `field`, `column`, `config`, `event`.
    pub kind: String,
    pub name: String,
    /// `removed`, `renamed`, `signature` (its parameters change).
    pub change: String,
    /// What it is called now, when it was renamed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
    /// The file of the PR it is changed in.
    pub file: String,
}

impl Thing {
    /// "renames formatTotal to formatAmount", "removes the field translated_text".
    pub fn said(&self) -> String {
        let what = match self.kind.as_str() {
            "function" | "type" => self.name.clone(),
            "route" => format!("the route {}", self.name),
            "field" => format!("the field {}", self.name),
            "column" => format!("the column {}", self.name),
            "config" => format!("the config key {}", self.name),
            "event" => format!("the event {}", self.name),
            k => format!("the {k} {}", self.name),
        };
        match (self.change.as_str(), &self.renamed_to) {
            ("renamed", Some(to)) => format!("renames {what} to {to}"),
            ("signature", _) => format!("changes the signature of {what}"),
            ("unread", _) => format!("stops reading {what}"),
            _ => format!("removes {what}"),
        }
    }
}

/// A use of something the PR changes, outside what the PR changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dependent {
    /// The repo's folder name.
    pub repo: String,
    pub file: String,
    pub line: u32,
    /// The name it uses (a `Thing`'s).
    pub uses: String,
    /// The function it is in, when there is one.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub within: String,
    /// The line itself, cut short.
    pub text: String,
}

impl Dependent {
    /// "shop-web src/cart/Total.tsx:12 in renderTotal".
    pub fn said(&self) -> String {
        let within = if self.within.is_empty() { String::new() } else { format!(" in {}", self.within) };
        format!("{} {}:{}{within}", self.repo, self.file, self.line)
    }
}

/// What the reviewer (or the PR's session) found by compiling,
/// type-checking or running the dependents' tests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// `breaks` or `clean`.
    pub verdict: String,
    pub text: String,
    pub by: String,
    pub at_ms: u64,
}

/// What depends on a PR's changes, as the tower found it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Impact {
    /// The PR's repo, by its folder name (its own uses are named after the others').
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repo: String,
    pub things: Vec<Thing>,
    pub dependents: Vec<Dependent>,
    /// The repos searched, by folder name.
    pub searched: Vec<String>,
    pub reports: Vec<Report>,
    /// The head commit whose diff was read.
    pub sha: String,
    pub checked_ms: u64,
}

/// How many dependents are kept, per thing and in all.
const PER_THING: usize = 8;
const ALL: usize = 40;

impl Impact {
    /// Something outside uses what it changes, or a break was reported.
    pub fn breaks(&self) -> bool {
        !self.dependents.is_empty() || self.reports.iter().any(|r| r.verdict == "breaks")
    }

    /// Each changed thing that something uses, with who uses it: "renames
    /// formatTotal to formatAmount, used by shop-web src/cart/Total.tsx:12
    /// in renderTotal and 1 more".
    pub fn breaking(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .things
            .iter()
            .filter_map(|t| {
                let mut users: Vec<&Dependent> = self.dependents.iter().filter(|d| d.uses == t.name).collect();
                // Another repo's use first, then a use inside a function (a call, not an import).
                users.sort_by_key(|d| (d.repo.eq_ignore_ascii_case(&self.repo), d.within.is_empty()));
                let first = users.first()?;
                let more = if users.len() > 1 { format!(" and {} more", users.len() - 1) } else { String::new() };
                Some(format!("{}, used by {}{more}", t.said(), first.said()))
            })
            .collect();
        out.extend(self.reports.iter().filter(|r| r.verdict == "breaks").map(|r| r.text.clone()));
        out
    }

    /// The warning a PR card shows: what breaks and who uses it; or that nothing outside depends on it.
    pub fn line(&self) -> String {
        let breaking = self.breaking();
        if !breaking.is_empty() {
            let mut s = format!("Breaking change: {}", breaking.iter().take(2).cloned().collect::<Vec<_>>().join("; "));
            if breaking.len() > 2 {
                s += &format!("; and {} more", breaking.len() - 2);
            }
            return s;
        }
        let what = match self.things.len() {
            0 => "no name others can use changes".to_string(),
            1 => self.things[0].said(),
            n => format!("{} things others could use change", n),
        };
        let clean = self.reports.iter().rev().find(|r| r.verdict == "clean").map(|r| format!("; checked: {}", r.text)).unwrap_or_default();
        format!("Nothing outside depends on what it changes ({what}; searched {} repo{}{clean})", self.searched.len(), if self.searched.len() == 1 { "" } else { "s" })
    }

    /// Its part of the PR's score: a breaking change, naming what breaks and who uses it.
    pub fn score(&self) -> crate::prscore::Score {
        let breaking = self.breaking();
        if breaking.is_empty() {
            return crate::prscore::Score { line: crate::prscore::LOW_LINE.into(), ..Default::default() };
        }
        let mut what = breaking.iter().take(2).cloned().collect::<Vec<_>>().join("; ");
        if breaking.len() > 2 {
            what += &format!("; and {} more", breaking.len() - 2);
        }
        let reason = format!("a breaking change ({what})");
        crate::prscore::Score { needs_review: true, line: format!("Needs review: {reason}"), reasons: vec![reason], flags: vec!["breaking".into()], ..Default::default() }
    }

    /// A break, or that nothing breaks, reported by the reviewer or the PR's session.
    pub fn report(&mut self, verdict: &str, text: &str, by: &str, now: u64) -> anyhow::Result<()> {
        let verdict = match verdict.trim().to_lowercase().as_str() {
            "breaks" | "break" | "broken" => "breaks",
            "clean" | "none" | "ok" => "clean",
            v => anyhow::bail!("a report is breaks or clean, not {v:?}"),
        };
        let text = text.trim();
        if text.is_empty() {
            anyhow::bail!("say what: breaks \"<what breaks, and who uses it: repo, file, function>\", clean \"<what you compiled, type-checked or ran>\"");
        }
        self.reports.push(Report { verdict: verdict.into(), text: text.chars().take(600).collect(), by: by.into(), at_ms: now });
        Ok(())
    }
}

// ---- the diff ----

/// One file of a unified diff: its lines taken away and its lines added.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileDiff {
    /// Its path after the change (before it, for a file deleted).
    pub path: String,
    /// Its path before, when it was moved.
    pub was: Option<String>,
    pub removed: Vec<String>,
    pub added: Vec<String>,
}

/// The files of a unified diff (`gh pr diff`, `git diff`).
pub fn parse_diff(diff: &str) -> Vec<FileDiff> {
    let mut out: Vec<FileDiff> = vec![];
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let b = rest.rsplit_once(" b/").map(|(_, b)| b).unwrap_or(rest);
            out.push(FileDiff { path: b.to_string(), ..Default::default() });
            continue;
        }
        let Some(f) = out.last_mut() else { continue };
        if let Some(p) = line.strip_prefix("rename from ") {
            f.was = Some(p.to_string());
        } else if let Some(p) = line.strip_prefix("+++ ") {
            if let Some(p) = p.strip_prefix("b/") {
                f.path = p.to_string();
            }
        } else if line.starts_with("--- ") || line.starts_with("@@") || line.starts_with("index ") {
        } else if let Some(l) = line.strip_prefix('-') {
            f.removed.push(l.to_string());
        } else if let Some(l) = line.strip_prefix('+') {
            f.added.push(l.to_string());
        }
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The identifier at the start of `s`.
fn ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    let id: String = s.chars().take_while(|c| is_ident_char(*c)).collect();
    (!id.is_empty() && !id.chars().next().is_some_and(|c| c.is_ascii_digit())).then_some(id)
}

/// A definition on this line that others can use: (kind, name, the whole
/// signature). Public only: `export` (JS, TS), `pub` (Rust), a capital
/// (Go), a module-level name not starting with `_` (Python), not `private`
/// (Kotlin, Java, Swift).
pub fn definition(line: &str) -> Option<(&'static str, String)> {
    let raw = line;
    let t = line.trim_start();
    let indent = raw.len() - t.len();
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.is_empty() || t.starts_with("//") || t.starts_with('#') || t.starts_with('*') {
        return None;
    }
    let private = words.iter().any(|w| matches!(*w, "private" | "fileprivate" | "internal"));
    // Rust.
    if words[0] == "pub" || words[0].starts_with("pub(") {
        let mut it = words.iter().skip(1).skip_while(|w| matches!(**w, "async" | "const" | "unsafe" | "extern" | "\"C\"") && **w != "const");
        let kw = it.next()?;
        let kind = match *kw {
            "fn" => "function",
            "struct" | "enum" | "trait" | "type" | "union" => "type",
            "const" | "static" => "function",
            _ => return None,
        };
        let name = ident(it.next()?)?;
        return Some((kind, name));
    }
    // JS / TS.
    if words[0] == "export" {
        let mut rest: Vec<&str> = words[1..].to_vec();
        while rest.first().is_some_and(|w| matches!(*w, "default" | "async" | "declare" | "abstract")) {
            rest.remove(0);
        }
        let kw = rest.first()?;
        let (kind, at) = match *kw {
            "function" | "function*" => ("function", 1),
            "class" => ("type", 1),
            "interface" | "type" | "enum" => ("type", 1),
            "const" | "let" | "var" => ("function", 1),
            _ => return None,
        };
        let name = ident(rest.get(at)?.trim_start_matches('*'))?;
        return Some((kind, name));
    }
    if let Some(rest) = t.strip_prefix("module.exports.").or_else(|| t.strip_prefix("exports.")) {
        return ident(rest).map(|n| ("function", n));
    }
    // Python: module level only (a method is its class's).
    if indent == 0 && (words[0] == "def" || words[0] == "class" || (words[0] == "async" && words.get(1) == Some(&"def"))) {
        let at = if words[0] == "async" { 2 } else { 1 };
        let name = ident(words.get(at)?)?;
        if name.starts_with('_') {
            return None;
        }
        return Some((if words[at - 1] == "class" { "type" } else { "function" }, name));
    }
    // Go: an exported name begins with a capital.
    if words[0] == "func" || words[0] == "type" {
        let after = t[words[0].len()..].trim_start();
        let after = if words[0] == "func" && after.starts_with('(') { after.split_once(')').map(|(_, r)| r.trim_start()).unwrap_or("") } else { after };
        let name = ident(after)?;
        if name.chars().next().is_some_and(|c| c.is_uppercase()) {
            return Some((if words[0] == "type" { "type" } else { "function" }, name));
        }
        return None;
    }
    // Kotlin, Java, Swift: what isn't private.
    if !private {
        for (i, w) in words.iter().enumerate() {
            let kind = match *w {
                "fun" | "func" => "function",
                "class" | "interface" | "object" | "struct" | "protocol" | "record" if i + 1 < words.len() => "type",
                _ => continue,
            };
            if words[..i].iter().all(|m| matches!(*m, "public" | "open" | "data" | "sealed" | "abstract" | "final" | "static" | "suspend" | "override" | "inline" | "enum" | "@JvmStatic")) {
                return ident(words[i + 1]).map(|n| (kind, n));
            }
        }
    }
    None
}

/// Its parameters, as written: what is between the first `(` and its `)`.
fn params(line: &str) -> String {
    let Some(start) = line.find('(') else { return String::new() };
    let mut depth = 0;
    for (i, c) in line[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return line[start + 1..start + i].split_whitespace().collect::<Vec<_>>().join(" ");
                }
            }
            _ => {}
        }
    }
    line[start + 1..].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A file whose fields others read: what an API sends or takes.
fn api_file(path: &str) -> bool {
    let p = path.to_lowercase();
    if [".proto", ".graphql", ".gql", ".avsc"].iter().any(|e| p.ends_with(e)) || p.contains("openapi") || p.contains("swagger") {
        return true;
    }
    let words: Vec<String> = p.split(|c: char| !c.is_alphanumeric()).map(String::from).collect();
    words.iter().any(|w| {
        matches!(
            w.as_str(),
            "api" | "apis" | "route" | "routes" | "router" | "handler" | "handlers" | "controller" | "controllers" | "function" | "functions" | "lambda" | "lambdas"
                | "serverless" | "server" | "graphql" | "schema" | "schemas" | "dto" | "dtos" | "types" | "model" | "models" | "resolver" | "resolvers" | "endpoint"
                | "endpoints" | "response" | "responses" | "request" | "requests" | "contract" | "contracts" | "events" | "payload" | "payloads" | "sdk" | "client"
                | "serializers" | "serializer"
        )
    })
}

/// Keys too common to search for by name.
const COMMON: &[&str] = &[
    "id", "name", "type", "data", "value", "values", "status", "error", "errors", "message", "title", "text", "url", "key", "items", "item", "default", "case", "return",
    "body", "headers", "method", "path", "query", "params", "result", "results", "count", "total", "page", "limit", "offset", "code", "label", "description", "created",
    "updated", "createdAt", "updatedAt", "user", "email", "date", "time", "list", "content", "options", "config", "props", "state", "style", "className", "children",
    "async", "await", "const", "import", "export", "from", "this", "self", "true", "false", "null", "none", "undefined",
];

/// An object's key at the start of this line (`translated_text: …`, `"translated_text": …`, `translated_text?: string;`).
fn key_of(line: &str) -> Option<String> {
    let t = line.trim_start();
    let t = t.strip_prefix("readonly ").unwrap_or(t);
    let (quoted, t) = match t.chars().next()? {
        '"' | '\'' => (true, &t[1..]),
        _ => (false, t),
    };
    let k = ident(t)?;
    let after = t[k.len()..].trim_start();
    let after = if quoted { after.strip_prefix(['"', '\'']).unwrap_or("").trim_start() } else { after };
    let after = after.strip_prefix('?').unwrap_or(after);
    (after.starts_with(':') && !after.starts_with("::")).then_some(k)
}

/// A key worth searching for: not common, not short.
fn distinctive(k: &str) -> bool {
    k.len() >= 5 && !COMMON.iter().any(|c| c.eq_ignore_ascii_case(k))
}

/// String literals on a line.
fn literals(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !matches!(c, '"' | '\'' | '`') {
            continue;
        }
        let start = i + 1;
        let mut end = None;
        for (j, d) in chars.by_ref() {
            if d == c {
                end = Some(j);
                break;
            }
        }
        if let Some(e) = end {
            out.push(line[start..e].to_string());
        }
    }
    out
}

/// A route on a line that defines one: its path.
fn route_of(line: &str) -> Option<String> {
    let l = line.to_lowercase();
    let hints = ["get(", "post(", "put(", "patch(", "delete(", "route", "router", "app.", "path", "@get", "@post", "endpoint", "handle(", "fetch(", "axios", "url"];
    if !hints.iter().any(|h| l.contains(h)) {
        return None;
    }
    // `path: v1/messages` in a serverless.yml.
    if let Some(v) = line.trim_start().strip_prefix("path:") {
        let v = v.trim().trim_matches(['"', '\'']);
        if v.len() >= 3 && v.chars().any(|c| c.is_alphabetic()) {
            return Some(if v.starts_with('/') { v.to_string() } else { format!("/{v}") });
        }
    }
    literals(line).into_iter().find(|s| s.starts_with('/') && s.len() >= 4 && s.chars().any(|c| c.is_alphabetic()) && !s.contains(' ') && !s.starts_with("//"))
}

/// A config key a line reads (`process.env.TRANSLATE_KEY`, `os.environ["X"]`, `env::var("X")`).
fn config_of(line: &str) -> Option<String> {
    for p in ["process.env.", "import.meta.env.", "process.env[\"", "process.env['", "os.environ[\"", "os.environ['", "os.environ.get(\"", "os.getenv(\"", "env::var(\"", "getenv(\"", "System.getenv(\""] {
        if let Some(i) = line.find(p) {
            let k = ident(&line[i + p.len()..])?;
            if k.len() >= 3 && k.chars().all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_') {
                return Some(k);
            }
        }
    }
    None
}

/// An event a line sends or listens to: its name.
fn event_of(line: &str) -> Option<String> {
    for p in ["emit(", "publish(", "dispatch(", "trigger(", "subscribe(", ".on(", "sendEvent(", "track("] {
        if let Some(i) = line.find(p) {
            let rest = &line[i + p.len()..];
            let lit = literals(rest).into_iter().next()?;
            if lit.len() >= 4 && !lit.contains(' ') && lit.chars().any(|c| c.is_alphabetic()) {
                return Some(lit);
            }
        }
    }
    None
}

/// A column a migration drops or renames: (name, renamed to).
fn column_of(line: &str) -> Option<(String, Option<String>)> {
    let l = line.to_lowercase();
    let words: Vec<String> = l.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(String::from).collect();
    let at = |w: &str| words.iter().position(|x| x == w);
    if let Some(i) = at("rename").filter(|i| words.get(i + 1).is_some_and(|w| w == "column")) {
        let from = words.get(i + 2)?.clone();
        let to = words.iter().skip(i + 3).skip_while(|w| *w != "to").nth(1).cloned();
        return Some((from, to));
    }
    if let Some(i) = at("drop").filter(|i| words.get(i + 1).is_some_and(|w| w == "column")) {
        let mut j = i + 2;
        if words.get(j).is_some_and(|w| w == "if") {
            j += 2;
        }
        return words.get(j).cloned().map(|c| (c, None));
    }
    // Knex, Rails, Alembic: their names are literals.
    let lits = literals(line);
    for (call, rename) in [("renamecolumn", true), ("rename_column", true), ("dropcolumn", false), ("removecolumn", false), ("remove_column", false), ("drop_column", false)] {
        if l.replace(['.', ':'], "").contains(call) {
            let syms: Vec<String> = if lits.is_empty() { words.iter().skip_while(|w| !w.contains(call.trim_start_matches("rename_").trim_start_matches("remove_"))).skip(1).cloned().collect() } else { lits.clone() };
            return match (rename, syms.len()) {
                (true, n) if n >= 3 => Some((syms[n - 2].clone(), Some(syms[n - 1].clone()))),
                (true, 2) => Some((syms[0].clone(), Some(syms[1].clone()))),
                (false, n) if n >= 1 => Some((syms[n - 1].clone(), None)),
                _ => None,
            };
        }
    }
    None
}

/// What the PR changes that others can use, from its diff.
pub fn things(files: &[FileDiff]) -> Vec<Thing> {
    let mut out: Vec<Thing> = vec![];
    let mut push = |t: Thing| {
        if !out.iter().any(|o| o.kind == t.kind && o.name == t.name) {
            out.push(t);
        }
    };
    // Definitions added anywhere: a name still defined isn't gone.
    let added_defs: Vec<(String, String, &'static str, &str)> =
        files.iter().flat_map(|f| f.added.iter().filter_map(move |l| definition(l).map(|(k, n)| (n, params(l), k, f.path.as_str())))).collect();
    let all_added: String = files.iter().flat_map(|f| f.added.iter()).cloned().collect::<Vec<_>>().join("\n");
    for f in files {
        let removed_defs: Vec<(&'static str, String, String)> = f.removed.iter().filter_map(|l| definition(l).map(|(k, n)| (k, n, params(l)))).collect();
        for (kind, name, before) in &removed_defs {
            match added_defs.iter().find(|(n, ..)| n == name) {
                Some((_, after, ..)) if after != before && !(before.is_empty() && after.is_empty()) => {
                    push(Thing { kind: (*kind).into(), name: name.clone(), change: "signature".into(), renamed_to: None, file: f.path.clone() });
                }
                Some(_) => {}
                None => {
                    // Renamed: one new definition of its kind in the same file that wasn't there.
                    let fresh: Vec<&String> = added_defs.iter().filter(|(n, _, k, p)| k == kind && *p == f.path && !removed_defs.iter().any(|(_, r, _)| r == n)).map(|(n, ..)| n).collect();
                    let (change, to) = if fresh.len() == 1 { ("renamed", Some(fresh[0].clone())) } else { ("removed", None) };
                    push(Thing { kind: (*kind).into(), name: name.clone(), change: change.into(), renamed_to: to, file: f.path.clone() });
                }
            }
        }
        // Routes, config keys and events no longer in what it adds.
        for l in &f.removed {
            if let Some(r) = route_of(l).filter(|r| !all_added.contains(r.as_str())) {
                push(Thing { kind: "route".into(), name: r, change: "removed".into(), renamed_to: None, file: f.path.clone() });
            }
            if let Some(k) = config_of(l).filter(|k| !all_added.contains(k.as_str())) {
                push(Thing { kind: "config".into(), name: k, change: "unread".into(), renamed_to: None, file: f.path.clone() });
            }
            if let Some(e) = event_of(l).filter(|e| !all_added.contains(&format!("\"{e}\"")) && !all_added.contains(&format!("'{e}'"))) {
                push(Thing { kind: "event".into(), name: e, change: "removed".into(), renamed_to: None, file: f.path.clone() });
            }
        }
        // Fields of what an API sends or takes.
        if api_file(&f.path) {
            let added_keys: Vec<String> = f.added.iter().filter_map(|l| key_of(l)).collect();
            let removed_keys: Vec<String> = f.removed.iter().filter_map(|l| key_of(l)).collect();
            let fresh: Vec<&String> = added_keys.iter().filter(|k| !removed_keys.contains(k)).collect();
            for k in removed_keys.iter().filter(|k| !added_keys.contains(k) && distinctive(k)) {
                let (change, to) = if fresh.len() == 1 { ("renamed", Some(fresh[0].clone())) } else { ("removed", None) };
                push(Thing { kind: "field".into(), name: k.clone(), change: change.into(), renamed_to: to, file: f.path.clone() });
            }
        }
        // Columns a migration drops or renames.
        if crate::prscore::file_kind(&f.path) == Some("migration") {
            for l in &f.added {
                if let Some((c, to)) = column_of(l).filter(|(c, _)| c.len() >= 3) {
                    let change = if to.is_some() { "renamed" } else { "removed" };
                    push(Thing { kind: "column".into(), name: c, change: change.into(), renamed_to: to, file: f.path.clone() });
                }
            }
        }
    }
    out
}

// ---- who uses it ----

/// A git checkout to search: its folder's name and where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Repo {
    pub name: String,
    pub path: PathBuf,
}

/// The git checkouts under `roots` (their own `.git` folder: a worktree of
/// another is that one's), three folders deep at most, without what is
/// hidden, built or installed.
pub fn repos_under(roots: &[PathBuf]) -> Vec<Repo> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<Repo>) {
        if dir.join(".git").is_dir() {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(Repo { name, path: dir.to_path_buf() });
            return;
        }
        if depth == 0 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut subs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        subs.sort();
        for p in subs {
            let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if n.starts_with('.') || matches!(n.as_str(), "node_modules" | "target" | "build" | "dist" | "vendor" | "Pods") {
                continue;
            }
            walk(&p, depth - 1, out);
        }
    }
    let mut out = vec![];
    for r in roots {
        walk(r, 3, &mut out);
    }
    out
}

/// Whether `text` uses `name` as a whole word.
fn uses_word(text: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(i) = text[from..].find(name) {
        let at = from + i;
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        if !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char) {
            return true;
        }
        from = at + name.len();
    }
    false
}

/// The function a line of a file is in: the nearest definition above it.
fn within(path: &Path, line: u32) -> String {
    let Ok(text) = std::fs::read_to_string(path) else { return String::new() };
    let lines: Vec<&str> = text.lines().collect();
    let at = (line as usize).saturating_sub(1).min(lines.len());
    for l in lines[at.saturating_sub(300)..=at.min(lines.len().saturating_sub(1))].iter().rev() {
        if let Some((_, n)) = definition(l).or_else(|| any_function(l).map(|n| ("function", n))) {
            return n;
        }
    }
    String::new()
}

/// A function's name on its first line, public or not (for "in <function>").
fn any_function(line: &str) -> Option<String> {
    let t = line.trim_start();
    for kw in ["function ", "async function ", "def ", "async def ", "fn ", "func ", "fun "] {
        if let Some(r) = t.strip_prefix(kw) {
            return ident(r.trim_start_matches('*'));
        }
    }
    // `const renderTotal = (…) =>`, `renderTotal(…) {` (a method).
    let r = t.strip_prefix("const ").or_else(|| t.strip_prefix("let ")).unwrap_or(t);
    let n = ident(r)?;
    let after = r[n.len()..].trim_start();
    // `= (…) =>`, `= async (…) =>`, `= x =>`, `= function`: not `= await fetch(…).then((r) => …)`.
    let value = after.strip_prefix('=').map(str::trim_start).filter(|_| !after.starts_with("=="));
    let arrow = value.is_some_and(|v| {
        let v = v.strip_prefix("async").map(str::trim_start).unwrap_or(v);
        v.starts_with("function") || (v.starts_with('(') && v.contains("=>")) || ident(v).is_some_and(|x| v[x.len()..].trim_start().starts_with("=>"))
    });
    let method = after.starts_with('(') && t.trim_end().ends_with('{') && !matches!(n.as_str(), "if" | "for" | "while" | "switch" | "catch" | "return");
    (arrow || method).then_some(n)
}

/// Files that are no one's functionality: docs, lockfiles, what is built.
fn ignored(path: &str) -> bool {
    let p = path.to_lowercase();
    [".md", ".txt", ".lock", ".min.js", ".map", ".snap", ".svg", ".csv"].iter().any(|e| p.ends_with(e))
        || ["dist/", "build/", "vendor/", "node_modules/", "__snapshots__/", "changelog"].iter().any(|d| p.contains(d))
        || p.ends_with("package-lock.json")
        || p.ends_with("pnpm-lock.yaml")
}

/// `git grep` in one checkout: (file, line, text) for each line with one of `names`.
fn grep(repo: &Path, names: &[&str], words: bool) -> Vec<(String, u32, String)> {
    if names.is_empty() {
        return vec![];
    }
    let mut args: Vec<String> = vec!["-C".into(), repo.display().to_string(), "grep".into(), "-n".into(), "-I".into(), "-F".into()];
    if words {
        args.push("-w".into());
    }
    for n in names {
        args.extend(["-e".into(), n.to_string()]);
    }
    let out = Command::new("git").args(&args).stdin(Stdio::null()).stderr(Stdio::null()).output();
    let Ok(out) = out else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (file, rest) = l.split_once(':')?;
            let (n, text) = rest.split_once(':')?;
            Some((file.to_string(), n.parse().ok()?, text.to_string()))
        })
        .filter(|(f, ..)| !ignored(f))
        .collect()
}

/// Whether this checkout is the PR's repo (`owner/name`, or its name).
fn is_prs_repo(r: &Repo, pr_repo: &str) -> bool {
    let name = crate::prscore::folder_name(pr_repo);
    if r.name.eq_ignore_ascii_case(name) {
        return true;
    }
    let url = Command::new("git").args(["-C", &r.path.display().to_string(), "remote", "get-url", "origin"]).stderr(Stdio::null()).output();
    url.ok().is_some_and(|o| {
        let u = String::from_utf8_lossy(&o.stdout).trim().trim_end_matches(".git").to_lowercase();
        pr_repo.contains('/') && u.ends_with(&pr_repo.to_lowercase())
    })
}

/// Who uses what the diff changes: every checkout under `roots`, the PR's
/// own (`pr_repo`) outside the files the PR changes.
pub fn scan(files: &[FileDiff], pr_repo: &str, roots: &[PathBuf]) -> Impact {
    let things = things(files);
    let repos = repos_under(roots);
    let changed: Vec<&str> = files.iter().flat_map(|f| std::iter::once(f.path.as_str()).chain(f.was.as_deref())).collect();
    let mut impact = Impact {
        repo: crate::prscore::folder_name(pr_repo).to_string(),
        things: things.clone(),
        searched: repos.iter().map(|r| r.name.clone()).collect(),
        ..Default::default()
    };
    if things.is_empty() {
        return impact;
    }
    // Routes by their fixed part (up to a `:id` or `{id}`); the rest by the whole word.
    let route_key = |t: &Thing| t.name.split([':', '{']).next().unwrap_or(&t.name).trim_end_matches('/').to_string();
    let words: Vec<String> = things.iter().filter(|t| t.kind != "route").map(|t| t.name.clone()).collect();
    let routes: Vec<String> = things.iter().filter(|t| t.kind == "route").map(route_key).filter(|r| r.len() >= 4).collect();
    for r in &repos {
        let own = is_prs_repo(r, pr_repo);
        let mut hits = grep(&r.path, &words.iter().map(String::as_str).collect::<Vec<_>>(), true);
        hits.extend(grep(&r.path, &routes.iter().map(String::as_str).collect::<Vec<_>>(), false));
        for (file, line, text) in hits {
            // The PR's own files are its to change; a definition of the same name isn't a use.
            if own && changed.iter().any(|c| *c == file) {
                continue;
            }
            for t in &things {
                let used = if t.kind == "route" { text.contains(&route_key(t)) } else { uses_word(&text, &t.name) };
                if !used || definition(&text).is_some_and(|(_, n)| n == t.name) {
                    continue;
                }
                if impact.dependents.len() >= ALL || impact.dependents.iter().filter(|d| d.uses == t.name).count() >= PER_THING {
                    continue;
                }
                if impact.dependents.iter().any(|d| d.repo == r.name && d.file == file && d.line == line && d.uses == t.name) {
                    continue;
                }
                impact.dependents.push(Dependent {
                    repo: r.name.clone(),
                    file: file.clone(),
                    line,
                    uses: t.name.clone(),
                    within: within(&r.path.join(&file), line),
                    text: text.trim().chars().take(160).collect(),
                });
            }
        }
    }
    impact
}

/// The roots searched for dependents (`[prs] impact_roots`, `~` made whole).
pub fn roots() -> Vec<PathBuf> {
    crate::config::Config::live().prs.impact_roots.iter().map(|r| PathBuf::from(crate::dispatch::expand(r.trim()))).filter(|p| p.is_dir()).collect()
}

/// `gh pr diff`: a PR's diff, as GitHub has it now.
pub fn fetch_diff(bin: &Path, repo: &str, number: u64) -> anyhow::Result<String> {
    let out = Command::new(bin).args(["pr", "diff", &number.to_string(), "--repo", repo]).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        anyhow::bail!("gh pr diff: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What depends on a real PR's changes at `sha` (its diff from GitHub).
pub fn of_pr(bin: &Path, repo: &str, number: u64, sha: &str, roots: &[PathBuf]) -> anyhow::Result<Impact> {
    let diff = fetch_diff(bin, repo, number)?;
    let mut i = scan(&parse_diff(&diff), repo, roots);
    i.sha = sha.to_string();
    i.checked_ms = crate::history::now_ms();
    Ok(i)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A git checkout with these files, added to its index (`git grep` reads it).
    fn repo(root: &Path, name: &str, files: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (p, text) in files {
            let path = dir.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&dir).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success(), "git {args:?}");
        git(&["init", "-q"]);
        git(&["add", "-A"]);
    }

    /// The scratch repos of the validation: an API (the PR's repo), a dashboard that uses it, and one that doesn't.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tower-impact-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        repo(
            &root,
            "shop-api",
            &[
                ("src/money.ts", "export function formatTotal(cents: number): string {\n  return (cents / 100).toFixed(2);\n}\n"),
                ("src/report.ts", "import { formatTotal } from './money';\n\nexport function dailyReport(rows: Row[]) {\n  return rows.map((r) => formatTotal(r.cents));\n}\n"),
                ("src/api/messages.ts", "export async function getMessages() {\n  return rows.map((t) => ({\n    id: t.id,\n    translated_text: t.text,\n  }));\n}\n"),
                ("src/util.ts", "function trimSpaces(s: string) {\n  return s.trim();\n}\nexport const clean = (s: string) => trimSpaces(s);\n"),
                ("README.md", "Call formatTotal to format a total.\n"),
            ],
        );
        repo(
            &root,
            "shop-web",
            &[
                ("src/cart/Total.tsx", "import { formatTotal } from 'shop-api/money';\n\nexport function renderTotal(cents: number) {\n  return <span>{formatTotal(cents)}</span>;\n}\n"),
                ("src/inbox/api.ts", "export async function loadMessages() {\n  const rows = await fetch('/v1/messages').then((r) => r.json());\n  return rows.map((r) => r.translated_text);\n}\n"),
            ],
        );
        repo(&root, "unrelated", &[("main.py", "def trimSpaces(s):\n    return s\n")]);
        root
    }

    /// A rename of an exported function, and of a field the API sends.
    pub(crate) const BREAKING: &str = "diff --git a/src/money.ts b/src/money.ts
--- a/src/money.ts
+++ b/src/money.ts
@@ -1,3 +1,3 @@
-export function formatTotal(cents: number): string {
+export function formatAmount(cents: number): string {
   return (cents / 100).toFixed(2);
 }
diff --git a/src/api/messages.ts b/src/api/messages.ts
--- a/src/api/messages.ts
+++ b/src/api/messages.ts
@@ -2,5 +2,5 @@
   return rows.map((t) => ({
     id: t.id,
-    translated_text: t.text,
+    translation: t.text,
   }));
";

    /// A rename inside one file: a helper nobody else can import.
    pub(crate) const INTERNAL: &str = "diff --git a/src/util.ts b/src/util.ts
--- a/src/util.ts
+++ b/src/util.ts
@@ -1,4 +1,4 @@
-function trimSpaces(s: string) {
+function squeeze(s: string) {
   return s.trim();
 }
-export const clean = (s: string) => trimSpaces(s);
+export const clean = (s: string) => squeeze(s);
";

    #[test]
    fn a_renamed_function_and_an_api_field_another_repo_reads_are_breaking_changes() {
        let root = scratch("breaking");
        let i = scan(&parse_diff(BREAKING), "acme/shop-api", &[root.clone()]);
        assert_eq!(
            i.things.iter().map(|t| (t.kind.as_str(), t.name.as_str(), t.change.as_str(), t.renamed_to.as_deref())).collect::<Vec<_>>(),
            [("function", "formatTotal", "renamed", Some("formatAmount")), ("field", "translated_text", "renamed", Some("translation"))]
        );
        assert_eq!(i.searched, ["shop-api", "shop-web", "unrelated"]);
        let deps: Vec<(String, String, String)> = i.dependents.iter().map(|d| (d.uses.clone(), d.said(), d.text.clone())).collect();
        // The other repo's callers, with the function each is in; its own repo's caller outside the PR's files; not the README.
        assert!(deps.contains(&("formatTotal".into(), "shop-web src/cart/Total.tsx:4 in renderTotal".into(), "return <span>{formatTotal(cents)}</span>;".into())), "{deps:#?}");
        assert!(deps.iter().any(|d| d.1 == "shop-api src/report.ts:4 in dailyReport"), "{deps:#?}");
        assert!(deps.contains(&("translated_text".into(), "shop-web src/inbox/api.ts:3 in loadMessages".into(), "return rows.map((r) => r.translated_text);".into())), "{deps:#?}");
        assert!(!deps.iter().any(|d| d.1.contains("README") || d.1.starts_with("unrelated")), "{deps:#?}");
        assert!(i.breaks());
        let line = i.line();
        // Another repo's call is named first, before the PR's own repo and before an import.
        assert!(line.starts_with("Breaking change: renames formatTotal to formatAmount, used by shop-web src/cart/Total.tsx:4 in renderTotal and 3 more"), "{line}");
        assert!(line.contains("renames the field translated_text to translation, used by shop-web src/inbox/api.ts:3 in loadMessages"), "{line}");
        let s = i.score();
        assert!(s.needs_review && s.flags == ["breaking"] && s.line.starts_with("Needs review: a breaking change (renames formatTotal to formatAmount, used by"), "{s:?}");
    }

    #[test]
    fn a_purely_internal_change_breaks_nothing_and_says_so() {
        let root = scratch("internal");
        let i = scan(&parse_diff(INTERNAL), "acme/shop-api", &[root]);
        assert!(i.things.is_empty(), "a helper nobody else can import: {:?}", i.things);
        assert!(i.dependents.is_empty() && !i.breaks());
        assert_eq!(i.line(), "Nothing outside depends on what it changes (no name others can use changes; searched 3 repos)");
        assert!(!i.score().needs_review);
        // An exported name nobody uses: named, and nothing found.
        let lonely = "diff --git a/src/util.ts b/src/util.ts\n--- a/src/util.ts\n+++ b/src/util.ts\n@@ -4 +4 @@\n-export const clean = (s: string) => trimSpaces(s);\n";
        let root = scratch("lonely");
        let i = scan(&parse_diff(lonely), "acme/shop-api", &[root]);
        assert_eq!(i.things.len(), 1);
        assert_eq!(i.line(), "Nothing outside depends on what it changes (removes clean; searched 3 repos)");
        // A reviewer's word: a break it found by running the dependents' tests counts; a clean run is said.
        let mut i = i;
        i.report("clean", "tsc --noEmit in shop-web: no errors", "t9", 1).unwrap();
        assert!(!i.breaks() && i.line().ends_with("checked: tsc --noEmit in shop-web: no errors)"), "{}", i.line());
        i.report("breaks", "the mobile app's sync job reads clean from the SDK (shop-mobile src/sync.kt in syncAll)", "t9", 2).unwrap();
        assert!(i.breaks() && i.line() == "Breaking change: the mobile app's sync job reads clean from the SDK (shop-mobile src/sync.kt in syncAll)", "{}", i.line());
        assert!(i.report("maybe", "x", "t9", 3).is_err() && i.report("breaks", " ", "t9", 3).is_err());
    }

    #[test]
    fn what_a_diff_changes_that_others_use() {
        let d = |kind: &str, line: &str| definition(line).filter(|(k, _)| *k == kind).map(|(_, n)| n);
        assert_eq!(d("function", "export async function getMessages() {"), Some("getMessages".into()));
        assert_eq!(d("type", "export interface Message {"), Some("Message".into()));
        assert_eq!(d("function", "pub fn merge_title(refs: &[PrRef]) -> String {"), Some("merge_title".into()));
        assert_eq!(d("type", "pub(crate) struct Gate {"), Some("Gate".into()));
        assert_eq!(d("function", "def load_rows(path):"), Some("load_rows".into()));
        assert_eq!(definition("    def method(self):"), None, "a method is its class's");
        assert_eq!(definition("def _private():"), None);
        assert_eq!(d("function", "func (s *Server) HandleOrders(w http.ResponseWriter) {"), Some("HandleOrders".into()));
        assert_eq!(definition("func handleOrders() {"), None, "unexported in Go");
        assert_eq!(d("function", "suspend fun syncAll(api: Api) {"), Some("syncAll".into()));
        assert_eq!(definition("private fun helper() {"), None);
        assert_eq!(definition("function local() {"), None, "not exported in JS");
        // A signature that changes; a route, a config key and an event it no longer has; a column a migration drops.
        let diff = "diff --git a/src/api/orders.ts b/src/api/orders.ts\n--- a/src/api/orders.ts\n+++ b/src/api/orders.ts\n@@\n-export function createOrder(cart: Cart) {\n+export function createOrder(cart: Cart, user: User) {\n-router.get('/v1/orders/:id', show);\n-const key = process.env.ORDERS_API_KEY;\n-bus.emit('order.created', order);\ndiff --git a/db/migrations/0043_drop.sql b/db/migrations/0043_drop.sql\n--- /dev/null\n+++ b/db/migrations/0043_drop.sql\n@@\n+ALTER TABLE orders DROP COLUMN IF EXISTS legacy_total;\n+ALTER TABLE orders RENAME COLUMN shipping_fee TO shipping_cents;\n";
        let t = things(&parse_diff(diff));
        let got: Vec<(&str, &str, &str)> = t.iter().map(|t| (t.kind.as_str(), t.name.as_str(), t.change.as_str())).collect();
        assert_eq!(
            got,
            [
                ("function", "createOrder", "signature"),
                ("route", "/v1/orders/:id", "removed"),
                ("config", "ORDERS_API_KEY", "unread"),
                ("event", "order.created", "removed"),
                ("column", "legacy_total", "removed"),
                ("column", "shipping_fee", "renamed"),
            ]
        );
        assert_eq!(t[5].renamed_to.as_deref(), Some("shipping_cents"));
        assert_eq!(t[0].said(), "changes the signature of createOrder");
        // A field of a file that isn't an API's, or a common key: not searched for.
        let ui = "diff --git a/src/components/Card.tsx b/src/components/Card.tsx\n--- a/src/components/Card.tsx\n+++ b/src/components/Card.tsx\n@@\n-  translated_text: t,\n";
        assert!(things(&parse_diff(ui)).is_empty());
        let common = "diff --git a/src/api/x.ts b/src/api/x.ts\n--- a/src/api/x.ts\n+++ b/src/api/x.ts\n@@\n-  status: s,\n";
        assert!(things(&parse_diff(common)).is_empty());
    }
}
